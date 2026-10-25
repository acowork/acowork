//! One Chat Router turn: read history, call the model, act, log
//! (ADR-099 §5, design 29 §5–§7).
//!
//! The turn owns no conversation state. Everything it knows arrives from two
//! places — the chat history on the Gateway and the candidate list the caller
//! built — so two turns with the same inputs produce the same request. The
//! only carried-over value is [`ChatSlot::anchor_seq`], which keeps the cached
//! prefix stable.
//!
//! Flow:
//!
//! 1. router config off disk, candidate rows off disk — no lock held;
//! 2. one short `SessionManager` lock: prompt, candidates, router model;
//! 3. history read over node-auth HTTP (bounded by a timeout — it is the only
//!    network hop before the model);
//! 4. Block B projection + Block C tail, model call(s) — `dispatch` plus
//!    read-only information tools (`memory_recall`, GET-only `http_request`);
//!    information rounds stay inside one turn so a full
//!    user → assistant → tool_call → tool_result → assistant chain is never
//!    cut across turns;
//! 5. act: reply (the write-back channel), dispatch to a session, or degrade
//!    to the pre-ADR-099 deterministic route;
//! 6. append one line to the decision log.
//!
//! Every step runs on the chat's drain task, never on the MQTT inbound loop
//! (see `chat_router::mod` — "Where a turn runs").

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use acowork_core::defaults::GATEWAY_HTTP_PORT;
use acowork_core::providers::traits::{
    CacheControl, ChatMessage, ChatRequest, ChatResponse, Provider, ToolCall,
};
use acowork_core::tools::traits::{Tool, ToolContext};
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;

use super::context::{
    build_block_b, parse_history, prefix_hash, relative_age, truncate_body, CandidateLine, ChatRow,
    TurnContext, WindowBudget,
};
use super::decision::{append_decision, DecisionRecord};
use super::prompt::{build_block_a, surface_label, PersonaSource};
use super::tool::{
    memory_recall_tool_name, parse_dispatch, router_tools, AliasTable, DispatchArgs, DispatchError,
    DISPATCH_TOOL, HTTP_REQUEST_TOOL,
};
use super::ChatSlot;
use crate::agent::agent_core::AgentCore;
use crate::agent::inbound::IntentEnvelope;
use crate::agent::intent_turn::{post_intent_response, IntentTurnContext};
use crate::agent::session::chat_candidates::{
    self, CandidateSurface, SessionCandidate,
};
use crate::agent::session::intent_routing::SessionSeed;
use crate::agent::session::{SessionManager, SessionMessage};
use crate::agent_config::ChatRouterConfig;

/// Router output ceiling. A routed turn's own text is a one-to-three-sentence
/// acknowledgement (design 29 §3.2), but the ceiling must also leave room for
/// an information round — a thinking block plus tool-call arguments plus the
/// final sentence — so one `user → assistant → tool_call → tool_result →
/// assistant` chain stays complete instead of being cut mid-call by a
/// length-truncated response.
const MAX_OUTPUT_TOKENS: u32 = 2048;

/// Per-result ceiling for an information tool fed back to the model. Generous
/// (a whole page of search hits or recalled memories), yet bounded: one
/// runaway HTTP body must not swamp the router request. The truncation
/// marker keeps the round structurally complete.
const INFO_RESULT_MAX_BYTES: usize = 8 * 1024;

/// Everything a turn needs from the Runtime process, snapshotted once so the
/// `SessionManager` lock is not held across the HTTP/LLM calls.
#[derive(Debug, Clone)]
pub struct RouterDeps {
    /// This agent's instance id — the `sender` the Gateway attributes rows to.
    pub instance_id: String,
    pub gateway_host: String,
    pub node_token: String,
    pub work_dir: String,
}

impl RouterDeps {
    pub fn from_core(core: &AgentCore, node_token: &str) -> Self {
        let cfg = &core.config;
        let token = if node_token.trim().is_empty() {
            cfg.mqtt_password.clone().unwrap_or_default()
        } else {
            node_token.to_string()
        };
        Self {
            instance_id: cfg.agent_instance_id.clone(),
            gateway_host: cfg
                .gateway_host
                .clone()
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            node_token: token,
            work_dir: cfg.work_dir.clone(),
        }
    }
}

/// What the caller must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The router owned the turn and delivered it (answered or dispatched).
    Handled,
    /// Not a Chat turn (or the router is switched off): keep the legacy path.
    PassThrough,
    /// The router failed to decide: write nothing, use the legacy path. The
    /// caller still delivers the user's message, so the turn is never lost.
    /// `chat_excerpt` is a bounded tail of the conversation the router had
    /// already read: a degraded `fresh` route has no LLM-composed brief, so
    /// the caller attaches the excerpt to the new session's first message
    /// (mechanical context, no model involved — strictly better than raw
    /// text alone). `None` when the router never got history this turn.
    Fallback {
        reason: String,
        chat_excerpt: Option<String>,
    },
}

/// Entry point: decide whether this envelope is a router turn and run it.
///
/// Called from the chat's drain task. The config read is a small file and
/// happens outside the `SessionManager` lock; only the `RouterDeps` snapshot
/// is taken under it.
pub async fn handle_intent(
    session_manager: &Arc<AsyncMutex<SessionManager>>,
    manager: &super::ChatSessionManager,
    envelope: &IntentEnvelope,
    node_token: &str,
) -> Outcome {
    let Some(text) = chat_turn_text(envelope) else {
        return Outcome::PassThrough;
    };

    // ── Short lock: snapshot what this agent runs on ─────────────────
    let deps = {
        let guard = session_manager.lock().await;
        RouterDeps::from_core(&guard.core(), node_token)
    };
    let cfg = router_config(&deps.work_dir);
    if !cfg.is_enabled() {
        tracing::debug!(
            chat_id = %envelope.source_chat_session_id,
            "chat router disabled by agent_config — using the deterministic route"
        );
        return Outcome::PassThrough;
    }

    // Per-chat serialization: two messages of one chat must not both read the
    // same anchor and then race on the write-back.
    let mut slot = manager.begin_turn(&envelope.source_chat_session_id).await;
    run_turn(session_manager, &mut slot, &cfg, &deps, envelope, &text).await
}

/// Is this envelope a turn the Chat Router takes? Pure shape test — no disk,
/// no lock. The inbound loop uses it to decide whether to hand the message to
/// the per-chat queue instead of routing it inline.
pub fn is_chat_turn(envelope: &IntentEnvelope) -> bool {
    chat_turn_text(envelope).is_some()
}

/// Whether the router is switched on for this agent.
///
/// A synchronous read of `agent_config.json` (a few KB), deliberately cheap
/// enough to stay on the inbound loop: with the router off, a Chat message
/// must take exactly the pre-ADR-099 path — inline, deterministic — instead of
/// paying a queue hop only to be handed back. The switch is the kill gate for
/// the whole feature (ADR-099 D8), so this is also the code that decides
/// whether degraded operator setups keep working.
pub fn router_enabled(work_dir: &std::path::Path) -> bool {
    router_config(&work_dir.to_string_lossy()).is_enabled()
}

/// This agent's router config, or the default (enabled) when `agent_config.json`
/// is missing or unreadable — the same fallback the Setup tab reads.
fn router_config(work_dir: &str) -> ChatRouterConfig {
    crate::agent_config::load_agent_config(std::path::Path::new(work_dir))
        .ok()
        .flatten()
        .map(|c| c.chat_router_config())
        .unwrap_or_default()
}

/// A Chat Router turn is a human `user_message` delivered from a Chat session
/// (design 27 §5.4.4). Everything else — cross-agent requests, responses,
/// cron triggers — keeps the legacy route.
///
/// Returns the message text to route. `Some("")` is legitimate: an
/// attachment-only message still has to be routed.
fn chat_turn_text(envelope: &IntentEnvelope) -> Option<String> {
    if envelope.kind != "user_message" || envelope.source_chat_session_id.is_empty() {
        return None;
    }
    let text = envelope
        .params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if text.trim().is_empty() && envelope.chat_attachment_ids.is_empty() {
        return None;
    }
    Some(text)
}

/// Chat surface from the envelope. Absent (older Gateway) reads as a group —
/// the privacy-safe default (design 29 §3.2).
fn chat_kind_of(envelope: &IntentEnvelope) -> String {
    envelope
        .params
        .get("chat_kind")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

fn candidate_surface(chat_kind: &str) -> CandidateSurface {
    if super::prompt::is_group_surface(chat_kind) {
        CandidateSurface::Group
    } else {
        CandidateSurface::Direct
    }
}

/// Everything the model call needs, gathered under one lock.
struct Prepared {
    block_a: String,
    candidates: Vec<SessionCandidate>,
    provider: Arc<dyn Provider>,
    model: String,
    provider_label: String,
    /// The read-only information tools enabled for this agent, resolved off
    /// the dispatch list (`all_tools` is already the enabled-builtin subset,
    /// so the agent-level toggle governs the router too).
    info_tools: Vec<Arc<dyn Tool>>,
}

async fn run_turn(
    session_manager: &Arc<AsyncMutex<SessionManager>>,
    slot: &mut ChatSlot,
    cfg: &ChatRouterConfig,
    deps: &RouterDeps,
    envelope: &IntentEnvelope,
    text: &str,
) -> Outcome {
    let started = Instant::now();
    let chat_id = envelope.source_chat_session_id.clone();
    let chat_kind = chat_kind_of(envelope);
    let budget = WindowBudget::from_history_window(cfg.history_window());
    let now_secs = chrono::Utc::now().timestamp();
    let now = chrono::Utc::now().to_rfc3339();

    // ── Candidate rows: off disk, before the lock ────────────────────
    // The meta scan is the slowest step of this turn's setup and needs nothing
    // from `SessionManager` but the work dir, which `RouterDeps` already
    // snapshotted. Under the global lock it would make `POST /sessions` and
    // every other session operation queue behind a filesystem walk.
    let candidate_rows = chat_candidates::scan_rows(&deps.work_dir);

    // ── Short lock: prompt + candidates + router model ──────────────────
    let prepared = {
        let guard = session_manager.lock().await;
        let core = guard.core();
        let block_a = build_block_a(
            &PersonaSource {
                display_name: core.manifest.display_name.as_deref(),
                name: &core.manifest.name,
                role: core.manifest.role.as_deref(),
                description: &core.manifest.description,
            },
            core.chat_persona().as_deref(),
            &chat_kind,
            guard.identity_context(),
        );
        let candidates = guard.resolve_candidates(
            &candidate_rows,
            &chat_id,
            Some(envelope.from.as_str()).filter(|s| !s.is_empty()),
            candidate_surface(&chat_kind),
            cfg.max_candidates(),
        );
        let (provider, model) = router_llm(&core, cfg);
        // The router only ever runs the two read-only lookups, resolved by
        // name off the enabled dispatch list. A disabled or missing tool is
        // neither exposed nor executed.
        //
        // ADR-101: the memory lookup is the official memory service's MCP
        // wrapper (when the Gateway put it in this agent's catalog), not a
        // builtin. Resolving by name keeps the router free of memory
        // business logic: no catalog entry, no memory tool on a chat turn.
        let memory_tool = memory_recall_tool_name();
        let info_tools: Vec<Arc<dyn Tool>> = core
            .all_tools
            .iter()
            .filter(|t| t.name() == memory_tool || t.name() == HTTP_REQUEST_TOOL)
            .cloned()
            .collect();
        Prepared {
            block_a,
            candidates,
            provider_label: provider.name().to_string(),
            provider,
            model,
            info_tools,
        }
    };

    // ── Block B: the append-only history projection ─────────────────────
    let (rows, history_available) =
        match read_history(deps, &chat_id, budget.page_size(), cfg.timeout_ms()).await {
            Ok((rows, budget_truncated)) => {
                if budget_truncated {
                    // The Gateway spent its page byte budget from the newest row
                    // backwards, so this window starts later than `count`
                    // suggests. Not an error — the turn still has everything
                    // recent — but it must not be read as a complete window.
                    tracing::debug!(
                        chat_id = %chat_id,
                        rows = rows.len(),
                        "chat router: history page truncated by the Gateway byte budget"
                    );
                }
                (rows, true)
            }
            Err(e) => {
                tracing::warn!(
                    chat_id = %chat_id,
                    error = %e,
                    "chat router: history read failed — the turn runs without it (design 29 §9)"
                );
                (Vec::new(), false)
            }
        };
    let mut excluded: HashSet<String> = HashSet::new();
    for id in [envelope.message_id.as_str(), envelope.in_reply_to.as_str()] {
        if !id.is_empty() {
            excluded.insert(id.to_string());
        }
    }
    let block_b = build_block_b(
        &rows,
        slot.anchor_seq,
        &excluded,
        &deps.instance_id,
        &budget,
    );
    // The window this turn used is the window the next turn starts from. Set
    // before the model call: a failed turn must not make the window grow
    // without bound on every retry.
    slot.anchor_seq = block_b.anchor_seq;

    // ── Block C: this turn's own view ───────────────────────────────────
    let mut aliases: AliasTable = AliasTable::new();
    let mut lines: Vec<CandidateLine> = Vec::with_capacity(prepared.candidates.len());
    for (index, candidate) in prepared.candidates.iter().enumerate() {
        let alias = format!("S{}", index + 1);
        aliases.insert(alias.clone(), candidate.session_id.clone());
        lines.push(CandidateLine {
            alias,
            current: candidate.current,
            status: candidate.status.label().to_string(),
            age: relative_age(now_secs, candidate.last_active_secs),
            origin: candidate.origin.label().to_string(),
            title: candidate.title.clone(),
        });
    }
    let draft = draft_label(&envelope.session_config_json);
    let task = task_label(&envelope.conversation_id);
    let turn_view = TurnContext {
        surface: surface_label(&chat_kind),
        speaker: &envelope.from,
        now: &now,
        draft: Some(draft.as_str()).filter(|d| !d.is_empty()),
        candidates: &lines,
        task: task.as_deref(),
        history_available,
        user_message: text,
    }
    .render();

    // ── Stateless call(s) ──────────────────────────────────────────────
    let memory_tool = prepared
        .info_tools
        .iter()
        .find(|t| t.name() == memory_recall_tool_name());
    let http_enabled = prepared
        .info_tools
        .iter()
        .any(|t| t.name() == HTTP_REQUEST_TOOL);
    let tools = router_tools(
        memory_tool.map(|t| t.name()).as_deref(),
        http_enabled,
    );
    let tools_json = serde_json::to_string(&tools).unwrap_or_default();
    let mut system = ChatMessage::system(prepared.block_a.clone());
    system.cache_control = Some(CacheControl::Ephemeral);
    let mut messages = Vec::with_capacity(block_b.messages.len() + 2);
    messages.push(system);
    messages.extend(block_b.messages.iter().cloned());
    messages.push(ChatMessage::user(turn_view));
    let mut request = ChatRequest {
        model: prepared.model.clone(),
        messages,
        temperature: Some(0.0),
        max_tokens: Some(MAX_OUTPUT_TOKENS),
        tools: Some(tools),
        reasoning_effort: None,
        thinking_mode: None,
    };

    let mut trace = TurnTrace {
        deps,
        chat_id: &chat_id,
        message_id: &envelope.message_id,
        chat_kind: &chat_kind,
        model: &prepared.model,
        provider: &prepared.provider_label,
        candidates: lines.iter().map(|c| c.alias.clone()).collect(),
        prefix_hash: prefix_hash(&prepared.block_a, &tools_json, &block_b.messages),
        anchor_seq: block_b.anchor_seq as usize,
        history_len: block_b.messages.len(),
        started,
        info_rounds: 0,
    };

    let mut response = match call_model(&prepared.provider, request.clone(), cfg.timeout_ms()).await {
        Ok(r) => r,
        Err(reason) => {
            trace.write("fallback", None, "error", Some(reason.clone()), None);
            return degrade(deps, envelope, &chat_id, reason, &rows, true).await;
        }
    };

    // ── Information rounds ────────────────────────────────────────────
    // How many lookups a turn may run is a prompt-level soft limit
    // ("usually zero, rarely one or two"), not a hard counter here: the
    // tools are read-only, every round pays the per-call model timeout and
    // the per-tool timeout, so a runaway chain degrades on time, never on
    // an arbitrary cap - and a cap that cuts a lookup off mid-answer is
    // exactly the "result runs away" failure it would claim to prevent.
    // The assistant → tool_call → tool_result → assistant chain lives in
    // this single request only; the write-back persists the final text, so
    // no tool traffic ever enters Block B.
    loop {
        // A dispatch - even after earlier lookups - outranks executing
        // more info calls: routing is the turn's decision, the lookups
        // were only context for it.
        if dispatch_call(&response).is_some() {
            break;
        }
        let info_calls: Vec<ToolCall> = response
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .filter(|call| call.function.name != DISPATCH_TOOL)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        if info_calls.is_empty() {
            break;
        }
        trace.info_rounds += 1;
        request.messages.push(ChatMessage::assistant_with_tools(
            response.content.clone(),
            info_calls.clone(),
        ));
        for call in &info_calls {
            let result = execute_info_tool(
                &prepared.info_tools,
                call,
                deps,
                text,
                &chat_id,
                cfg.timeout_ms(),
            )
            .await;
            request.messages.push(ChatMessage::tool(call.id.clone(), result));
        }
        response = match call_model(&prepared.provider, request.clone(), cfg.timeout_ms()).await {
            Ok(r) => r,
            Err(reason) => {
                trace.write("fallback", None, "error", Some(reason.clone()), None);
                return degrade(deps, envelope, &chat_id, reason, &rows, true).await;
            }
        };
    }

    // ── Act ────────────────────────────────────────────────────────────
    let mut dispatch: Option<(DispatchArgs, String)> = None;
    if let Some(call) = dispatch_call(&response) {
        match parse_arguments(&call, &aliases) {
            Ok(args) => dispatch = Some((args, call_label(&call))),
            Err(err) => {
                // Exactly one retry with the validation error fed back as a
                // tool result (ADR-099 §5). A second failure degrades — a
                // router that cannot fill in one required field twice is not
                // going to improve on a third try.
                request.messages.push(ChatMessage::assistant_with_tools(
                    response.content.clone(),
                    vec![call.clone()],
                ));
                request.messages.push(ChatMessage::tool(
                    call.id.clone(),
                    format!("Error: {}", err.message()),
                ));
                match call_model(&prepared.provider, request.clone(), cfg.timeout_ms()).await {
                    Ok(retry) => {
                        response = retry;
                        if let Some(second) = dispatch_call(&response) {
                            match parse_arguments(&second, &aliases) {
                                Ok(args) => dispatch = Some((args, call_label(&second))),
                                Err(again) => {
                                    let reason = format!(
                                        "invalid dispatch twice: {}",
                                        again.message()
                                    );
                                    trace.write("fallback", None, "error", Some(reason.clone()), None);
                                    return degrade(deps, envelope, &chat_id, reason, &rows, true).await;
                                }
                            }
                        }
                    }
                    Err(reason) => {
                        trace.write("fallback", None, "error", Some(reason.clone()), None);
                        return degrade(deps, envelope, &chat_id, reason, &rows, true).await;
                    }
                }
            }
        }
    }

    let usage = response.usage.clone();
    match dispatch {
        None => {
            let body = response.content.trim().to_string();
            if body.is_empty() {
                trace.write("fallback", None, "error", Some("empty reply".into()), None);
                return degrade(deps, envelope, &chat_id, "empty reply".to_string(), &rows, true).await;
            }
            let sent = post_intent_response(
                &deps.instance_id,
                &deps.gateway_host,
                &deps.node_token,
                // No session pointer: the row is the router's own answer, and
                // an empty id is what tells the surface there is nothing to
                // open (design 29 §7.2).
                "",
                &IntentTurnContext::new(
                    &envelope.message_id,
                    &envelope.conversation_id,
                    &envelope.source_chat_session_id,
                    &envelope.from,
                    &envelope.action,
                ),
                &body,
            )
            .await;
            match sent {
                Ok(()) => {
                    trace.write("reply", None, "ok", None, usage.as_ref().map(|u| (u.prompt_tokens, u.cache_read_tokens, u.cache_write_tokens)));
                    Outcome::Handled
                }
                Err(e) => {
                    trace.write("fallback", None, "error", Some(e.clone()), None);
                    degrade(deps, envelope, &chat_id, format!("reply write-back failed: {e}"), &rows, false).await
                }
            }
        }
        Some((args, label)) => {
            // A `dispatch` without `session_config` still honours the user's
            // draft configuration when it starts a new session — that is the
            // whole point of not forcing a new session on a config change.
            let seed = args
                .seed
                .clone()
                .or_else(|| SessionSeed::from_json(&envelope.session_config_json));
            // The router's `message` is a brief, and whether this turn needs
            // one is NOT decided by the model's `target` argument: an
            // existing session may have vanished from disk, in which case
            // `chat_dispatch` opens a fresh one and reports `opened_new`.
            // So the brief waits until the target is actually resolved.
            let brief = args.message.clone();
            let target = args.target.clone();
            let delivered = {
                let mut guard = session_manager.lock().await;
                match guard.chat_dispatch(&chat_id, target, seed).await {
                    Ok((session_id, opened_new)) => {
                        let content = dispatch_content(opened_new, brief.as_deref(), text);
                        // §5.5.5 pending queue: record BEFORE delivering. The
                        // only place that drops this row is the SessionTask
                        // dequeue, which can run only once the send below has
                        // happened — so recording first guarantees the row is
                        // present when removal fires. Recording after the send
                        // raced: an idle session woke and started the turn at
                        // once, its empty removal ran first, and the row pushed
                        // right after (the session now `is_active`) sat in the
                        // queue forever. A busy session is active now (row
                        // recorded, cleared at dequeue); an idle one is not
                        // (record is a no-op — it starts immediately).
                        guard.record_pending_intent(
                            &session_id,
                            crate::agent::session_state::PendingIntent {
                                message_id: envelope.message_id.clone(),
                                from: envelope.from.clone(),
                                preview: crate::agent::session_state::pending_preview(
                                    &content,
                                ),
                                from_chat: true,
                            },
                        );
                        let sent = guard.send_to_session(
                            &session_id,
                            SessionMessage::Intent {
                                content,
                                message_id: envelope.message_id.clone(),
                                conversation_id: envelope.conversation_id.clone(),
                                source_chat_session_id: chat_id.clone(),
                                from: envelope.from.clone(),
                                action: envelope.action.clone(),
                                repo: envelope.repo.clone(),
                                kind: envelope.kind.clone(),
                                has_human: envelope.has_human,
                                chat_attachment_ids: envelope.chat_attachment_ids.clone(),
                                in_reply_to: envelope.in_reply_to.clone(),
                            },
                        );
                        (Ok((session_id, opened_new)), sent)
                    }
                    Err(e) => (Err(e.to_string()), Ok(())),
                }
            };
            match delivered {
                (Ok((session_id, opened_new)), Ok(())) => {
                    // ADR-100 D11: the routing reaction. The dispatch/start
                    // *progress* milestones stay runtime-only (D10) — what is
                    // written here is the one conversational fact the Chat
                    // turn owes immediately: "your message went to a new task
                    // / continues task X". It lands the moment the target is
                    // resolved, minutes or hours before the answer, and it is
                    // durable because "which task handled this message" is a
                    // fact of the conversation, not of the progress bar.
                    // Best-effort: a lost row costs the reaction, never the
                    // delivery (the turn runs regardless).
                    if let Err(e) = crate::agent::intent_turn::append_chat_row(
                        &deps.instance_id,
                        &deps.gateway_host,
                        &deps.node_token,
                        &chat_id,
                        "system_notice",
                        &envelope.from,
                        "",
                        crate::agent::intent_turn::turn_routed_meta(
                            &envelope.message_id,
                            &session_id,
                            opened_new,
                        ),
                    )
                    .await
                    {
                        tracing::warn!(
                            chat_id = %chat_id,
                            message_id = %envelope.message_id,
                            error = %e,
                            "routing reaction row failed — non-fatal"
                        );
                    }
                    trace.write(&label, Some(&session_id), "ok", None, usage.as_ref().map(|u| (u.prompt_tokens, u.cache_read_tokens, u.cache_write_tokens)));
                    Outcome::Handled
                }
                (Ok(_), Err(e)) => {
                    let reason = format!("dispatch delivery failed: {e}");
                    trace.write(&label, None, "error", Some(reason.clone()), None);
                    degrade(deps, envelope, &chat_id, reason, &rows, true).await
                }
                (Err(e), _) => {
                    let reason = format!("dispatch target resolution failed: {e}");
                    trace.write(&label, None, "error", Some(reason.clone()), None);
                    degrade(deps, envelope, &chat_id, reason, &rows, true).await
                }
            }
        }
    }
}

/// Read the chat history page this turn projects (design 29 §4.4). The
/// Gateway caps the page; `WindowBudget::page_size` never asks for more.
///
/// Bounded by the turn's own model-call timeout: a stalled Gateway must cost
/// this chat one degraded turn, not a parked drain task. The turn runs without
/// history when the read fails, so a timeout here is a documented, survivable
/// outcome — a hang would not be.
///
/// `Ok((rows, truncated))` — `truncated` is the Gateway's page-byte-budget
/// verdict (older rows of this page dropped), never a failure.
async fn read_history(
    deps: &RouterDeps,
    chat_id: &str,
    count: usize,
    timeout_ms: u64,
) -> Result<(Vec<ChatRow>, bool), String> {
    let url = format!(
        "http://{}:{}/api/intents/chat-history",
        deps.gateway_host, GATEWAY_HTTP_PORT
    );
    let mut req = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(timeout_ms))
        .query(&[
            ("sender", deps.instance_id.as_str()),
            ("session_id", chat_id),
            ("offset", "0"),
            ("count", &count.to_string()),
        ]);
    if !deps.node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, &deps.node_token);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let detail = body
            .get("message")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("chat history rejected");
        return Err(format!("HTTP {status}: {detail}"));
    }
    let rows = parse_history(&body)?;
    let truncated = body
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((rows, truncated))
}

/// Model resolution (ADR-099 D)-3): explicit `chat_router.model` first, then
/// the agent's compact-model chain, then the agent default.
fn router_llm(core: &AgentCore, cfg: &ChatRouterConfig) -> (Arc<dyn Provider>, String) {
    if let Some(model) = cfg
        .model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        let provider = cfg
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty());
        return core.summary_llm_snapshot(provider, Some(model));
    }
    let (provider, model, _tier) = core.resolve_summary_llm(None, None);
    (provider, model)
}

async fn call_model(
    provider: &Arc<dyn Provider>,
    request: ChatRequest,
    timeout_ms: u64,
) -> Result<ChatResponse, String> {
    match tokio::time::timeout(Duration::from_millis(timeout_ms), provider.chat(request)).await {
        Ok(Ok(mut response)) => {
            // Compat endpoints (MiniMax/DeepSeek style) can inline think blocks
            // into `content` on the non-streaming path, where ThinkTagParser
            // never runs. The main loop defends at persistence time
            // (strip_think_block); the router has no such path — its content is
            // posted to the chat verbatim. Strip at the single choke point so
            // the reply body AND the history pushes below stay clean.
            response.content = crate::agent::loop_session::strip_think_block(&response.content);
            Ok(response)
        }
        Ok(Err(e)) => Err(format!("provider error: {e}")),
        Err(_) => Err(format!("router call timed out after {timeout_ms}ms")),
    }
}

/// Run one router information-tool call and render its result for the model.
///
/// Everything failing here is a *tool result*, never a turn failure: the
/// model reads the error text and decides whether to answer with what it
/// has, look something else up, or route. `http_request` is forced to a
/// bounded GET regardless of what arguments the model invented (ADR-099 D3:
/// the router reads, it never changes anything) - the narrowed request-side
/// schema is the polite form of the same rule.
async fn execute_info_tool(
    tools: &[Arc<dyn Tool>],
    call: &ToolCall,
    deps: &RouterDeps,
    user_text: &str,
    chat_id: &str,
    timeout_ms: u64,
) -> String {
    let name = call.function.name.as_str();
    let Some(tool) = tools.iter().find(|t| t.name() == name) else {
        let available = tools
            .iter()
            .map(|t| t.name())
            .collect::<Vec<_>>()
            .join(", ");
        return format!(
            "Error: `{name}` is not available on a chat turn — valid tools: {DISPATCH_TOOL}, {available} (GET only for {HTTP_REQUEST_TOOL})"
        );
    };
    let raw: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    let params = if name == HTTP_REQUEST_TOOL {
        let url = raw
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return "Error: `url` must be an http(s) URL".to_string();
        }
        // String headers only; anything else the model invented is dropped.
        let headers: serde_json::Map<String, Value> = raw
            .get("headers")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(k, v)| {
                        v.as_str().map(|s| (k.clone(), Value::String(s.to_string())))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // Inner request timeout strictly under the outer turn timeout so
        // the model - not the deadline - sees the failure.
        let secs = (timeout_ms / 1000).saturating_sub(1).max(1);
        serde_json::json!({
            "method": "GET",
            "url": url,
            "headers": Value::Object(headers),
            "timeout_secs": secs,
        })
    } else {
        raw
    };
    let ctx = ToolContext {
        work_dir: Some(deps.work_dir.clone()),
        // The router turn belongs to the chat, not to any execution
        // session: no session-node exclusion, and the user's message is
        // the semantic query source for memory recall. The chat id is
        // the one coordinate the router DOES own — session-aware tools
        // (chat_history_read) resolve their binding from it.
        session_id: None,
        current_user_message: Some(user_text.to_string()),
        chat_session_id: (!chat_id.is_empty()).then(|| chat_id.to_string()),
    };
    let outcome = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        tool.execute_with_context(params, &ctx),
    )
    .await;
    let text = match outcome {
        Ok(Ok(result)) if result.ok => result.content,
        Ok(Ok(result)) => format!(
            "Error: {}",
            result.error.unwrap_or_else(|| "tool failed".to_string())
        ),
        Ok(Err(e)) => format!("Error: {e}"),
        Err(_) => format!("Error: `{name}` timed out after {timeout_ms}ms"),
    };
    truncate_info_result(&text)
}

/// Bound one tool result without splitting a character; the marker keeps
/// the message pair structurally valid however large the payload was.
fn truncate_info_result(text: &str) -> String {
    if text.len() <= INFO_RESULT_MAX_BYTES {
        return text.to_string();
    }
    let mut end = INFO_RESULT_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

fn dispatch_call(response: &ChatResponse) -> Option<ToolCall> {
    response
        .tool_calls
        .as_ref()?
        .iter()
        .find(|call| call.function.name == super::tool::DISPATCH_TOOL)
        .cloned()
}

fn parse_arguments(call: &ToolCall, aliases: &AliasTable) -> Result<DispatchArgs, DispatchError> {
    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    parse_dispatch(&args, aliases)
}

/// `dispatch:S1` / `dispatch:new` — the decision label for the log.
fn call_label(call: &ToolCall) -> String {
    let raw: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    match raw.get("target").and_then(Value::as_str).map(str::trim) {
        Some(target) if target.eq_ignore_ascii_case("new") => "dispatch:new".to_string(),
        Some(target) => format!("dispatch:{}", target.to_ascii_uppercase()),
        None => "dispatch:?".to_string(),
    }
}

/// Rendered draft configuration for Block C (`model=…, workspace=…`).
/// Empty when the user has not touched anything.
fn draft_label(raw: &str) -> String {
    let Some(seed) = SessionSeed::from_json(raw) else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(v) = &seed.model {
        parts.push(format!("model={v}"));
    }
    if let Some(v) = &seed.provider {
        parts.push(format!("provider={v}"));
    }
    if let Some(v) = &seed.workspace {
        parts.push(format!("workspace={v}"));
    }
    if let Some(v) = &seed.reasoning_effort {
        parts.push(format!("effort={v}"));
    }
    parts.join(", ")
}

/// `task:{project}:{task}` → `project=… task=…` (design 29 §5.3). `None` for
/// every other conversation shape — a `task:` id is the only one the router
/// can describe without guessing.
fn task_label(conversation_id: &str) -> Option<String> {
    let rest = conversation_id.strip_prefix("task:")?;
    let mut parts = rest.splitn(2, ':');
    let project = parts.next().unwrap_or("").trim();
    let task = parts.next().unwrap_or("").trim();
    let mut out = String::new();
    if !project.is_empty() {
        out.push_str(&format!("project={project}"));
    }
    if !task.is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("task={task}"));
    }
    (!out.is_empty()).then_some(out)
}

/// A degraded turn: hand the message back to the deterministic route. The
/// user's message is still delivered — degradation costs latency, never the
/// message.
///
/// `rows` are the chat rows this turn had already read (empty when history
/// itself failed): they become a bounded excerpt the caller attaches to the
/// degraded route's first message, so a fresh session is not left with raw
/// text alone. `notify` posts the one conversational fact the degraded turn
/// owes the human — the router could not decide and the message was
/// forwarded automatically — best-effort like the routing reaction row.
async fn degrade(
    deps: &RouterDeps,
    envelope: &IntentEnvelope,
    chat_id: &str,
    reason: String,
    rows: &[ChatRow],
    notify: bool,
) -> Outcome {
    tracing::warn!(
        chat_id = %chat_id,
        message_id = %envelope.message_id,
        reason = %reason,
        "chat router degraded to the deterministic route"
    );
    if notify
        && let Err(e) = crate::agent::intent_turn::append_chat_row(
            &deps.instance_id,
            &deps.gateway_host,
            &deps.node_token,
            chat_id,
            "system_notice",
            &envelope.from,
            "",
            crate::agent::intent_turn::router_fallback_meta(&envelope.message_id),
        )
        .await
    {
        tracing::warn!(
            chat_id = %chat_id,
            error = %e,
            "router fallback receipt row failed — non-fatal"
        );
    }
    Outcome::Fallback {
        reason,
        chat_excerpt: chat_tail_excerpt(rows, &deps.instance_id),
    }
}

/// How many recent chat rows a degraded turn may hand to the fresh session.
const FALLBACK_EXCERPT_ROWS: usize = 6;
/// Byte ceiling of the same excerpt (per row and total) — the excerpt must
/// stay a footnote to the user's message, never a second context window.
const FALLBACK_EXCERPT_ROW_BYTES: usize = 512;
const FALLBACK_EXCERPT_TOTAL_BYTES: usize = 2048;

/// Mechanical tail of the conversation for the degraded `fresh` route: the
/// last few rows, oldest first, each sender-tagged and byte-bounded. No LLM
/// is involved — this is what the router already saw, not what it concluded.
/// `None` when nothing but the trigger message itself would remain.
fn chat_tail_excerpt(rows: &[ChatRow], agent_instance: &str) -> Option<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    for row in rows.iter().rev() {
        if lines.len() >= FALLBACK_EXCERPT_ROWS || bytes >= FALLBACK_EXCERPT_TOTAL_BYTES {
            break;
        }
        // The trigger message rides along verbatim on the legacy route;
        // notices and cards are routing chrome, not conversation.
        if !matches!(row.kind.as_str(), "user_message" | "agent_message" | "text" | "") {
            continue;
        }
        let body = truncate_body(row.body.trim(), FALLBACK_EXCERPT_ROW_BYTES);
        if body.is_empty() {
            continue;
        }
        let role = if !agent_instance.is_empty() && row.from == agent_instance {
            "agent"
        } else {
            "user"
        };
        let line = format!("[{role}] {body}");
        bytes += line.len();
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    lines.reverse();
    Some(lines.join("\n"))
}

/// Decision-log context for one turn. Holds borrows only, so writing the
/// record never needs to re-derive anything.
struct TurnTrace<'a> {
    deps: &'a RouterDeps,
    chat_id: &'a str,
    message_id: &'a str,
    chat_kind: &'a str,
    model: &'a str,
    provider: &'a str,
    candidates: Vec<String>,
    prefix_hash: String,
    anchor_seq: usize,
    history_len: usize,
    started: Instant,
    /// Information-tool rounds this turn ran before deciding (audit
    /// surface for "the router looked things up instead of routing").
    info_rounds: u64,
}

impl TurnTrace<'_> {
    fn write(
        &self,
        decision: &str,
        target_session_id: Option<&str>,
        outcome: &str,
        fallback_reason: Option<String>,
        usage: Option<(u64, u64, u64)>,
    ) {
        let record = DecisionRecord {
            ts: chrono::Utc::now().to_rfc3339(),
            chat_id: self.chat_id.to_string(),
            message_id: self.message_id.to_string(),
            chat_kind: self.chat_kind.to_string(),
            model: self.model.to_string(),
            provider: self.provider.to_string(),
            candidates: self.candidates.clone(),
            decision: decision.to_string(),
            target_session_id: target_session_id.map(str::to_string),
            latency_ms: self.started.elapsed().as_millis() as u64,
            outcome: outcome.to_string(),
            fallback_reason,
            prefix_hash: self.prefix_hash.clone(),
            anchor_seq: self.anchor_seq,
            history_len: self.history_len,
            info_rounds: (self.info_rounds > 0).then_some(self.info_rounds),
            prompt_tokens: usage.map(|u| u.0),
            cache_read_tokens: usage.map(|u| u.1),
            cache_write_tokens: usage.map(|u| u.2),
        };
        tracing::info!("{}", record.summary());
        if let Err(e) = append_decision(std::path::Path::new(&self.deps.work_dir), &record) {
            tracing::debug!(error = %e, "chat router: decision log not written");
        }
    }
}

/// What the execution session actually receives for a dispatched turn.
///
/// The router's brief exists for one thing only: a session that starts empty
/// has no history to resolve "commit the local changes" against, so the
/// brief has to carry the conversation. A **continued** session already has
/// its own history — a compaction included, since a compacted session keeps
/// its summary and its tail and continues in place (ADR-032/ADR-061) — so it
/// receives the user's own words verbatim. Overwriting them with a router
/// paraphrase would put the paraphrase in the persisted `user` row and in
/// the auto-title (ADR-046 keeps those the user's actual message) and
/// re-inject context the session already holds; anything it genuinely lacks it
/// pulls with `chat_history_read`.
///
/// `opened_new` is the routing-time authority for "this turn landed in a
/// physically new session", which is why the choice is made here rather than
/// from `Target`: `Target::Existing` against a session that is gone from disk
/// also creates one, and that one still needs the brief.
fn dispatch_content(opened_new: bool, brief: Option<&str>, user_text: &str) -> String {
    match (opened_new, brief) {
        (true, Some(brief)) => brief.to_string(),
        _ => user_text.to_string(),
    }
}

// The `SessionStatus` → `CandidateStatus` vocabulary table lives in
// `chat_candidates` (ADR-099): `SessionManager::resolve_candidates` needs the
// same mapping, and the session layer must not reach up into the router layer
// for it.

#[cfg(test)]
mod tests {
    use super::super::tool::Target;
    use super::*;
    use crate::agent::session::chat_candidates::{CandidateOrigin, CandidateStatus};
    use acowork_core::providers::mock::{MockProvider, MockResponse};

    #[tokio::test]
    async fn call_model_strips_inline_think_tags() {
        // Chat-surface leak (2026-10-24): a compat endpoint inlined its think
        // block into `content` on the non-streaming path; the router posted it
        // verbatim to the chat. call_model must sanitize every response.
        let open = "<\u{74}hink>";
        let close = "</\u{74}hink>";
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![MockResponse::Text {
            content: format!("{open}User asks the time. Simple answer.{close}现在是 19:37。"),
        }]));
        let request = ChatRequest {
            model: "mock-model".into(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            tools: None,
            reasoning_effort: None,
            thinking_mode: None,
        };
        let response = call_model(&provider, request, 5_000).await.unwrap();
        assert_eq!(response.content, "现在是 19:37。");
    }

    fn envelope(kind: &str, chat_id: &str, params: Value) -> IntentEnvelope {
        IntentEnvelope {
            from: "user-1".into(),
            action: "chat".into(),
            params,
            message_id: "m-1".into(),
            conversation_id: String::new(),
            source_chat_session_id: chat_id.into(),
            repo: String::new(),
            session_config_json: String::new(),
            kind: kind.into(),
            in_reply_to: String::new(),
            status: String::new(),
            has_human: Some(true),
            chat_attachment_ids: Vec::new(),
            for_execution_session: String::new(),
        }
    }

    #[test]
    fn only_human_chat_messages_are_router_turns() {
        let ok = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({"message": "hi"}),
        );
        assert_eq!(chat_turn_text(&ok).as_deref(), Some("hi"));

        // Cross-agent request, response, or no chat binding → legacy path.
        assert!(chat_turn_text(&envelope("request", "cs-1", serde_json::json!({"message": "x"}))).is_none());
        assert!(chat_turn_text(&envelope("response", "cs-1", serde_json::json!({"message": "x"}))).is_none());
        assert!(chat_turn_text(&envelope("user_message", "", serde_json::json!({"message": "x"}))).is_none());

        // Empty text with no attachment has no addressable turn.
        assert!(chat_turn_text(&envelope("user_message", "cs-1", serde_json::json!({"message": "  "}))).is_none());

        // An attachment-only message is still a turn.
        let mut with_file = envelope("user_message", "cs-1", serde_json::json!({"message": ""}));
        with_file.chat_attachment_ids = vec!["att-1".into()];
        assert_eq!(chat_turn_text(&with_file).as_deref(), Some(""));
    }

    #[test]
    fn absent_chat_kind_reads_as_a_group() {
        let missing = envelope("user_message", "cs-1", serde_json::json!({}));
        assert_eq!(chat_kind_of(&missing), "");
        assert_eq!(candidate_surface(""), CandidateSurface::Group);

        let dm = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({"chat_kind": "user_agent"}),
        );
        assert_eq!(candidate_surface(&chat_kind_of(&dm)), CandidateSurface::Direct);
    }

    #[test]
    fn draft_label_lists_only_the_fields_the_user_set() {
        assert_eq!(draft_label(""), "");
        assert_eq!(draft_label("not json"), "");
        assert_eq!(
            draft_label(r#"{"model":"m1","workspace":"ws-1"}"#),
            "model=m1, workspace=ws-1"
        );
        // A provider without a model cannot pin anything and is dropped by
        // the seed parser, so the label must not claim it either.
        assert_eq!(draft_label(r#"{"provider":"p1"}"#), "");
    }

    #[test]
    fn task_label_only_describes_task_conversations() {
        assert_eq!(task_label("").as_deref(), None);
        assert_eq!(
            task_label("task:proj-1:T-12").as_deref(),
            Some("project=proj-1 task=T-12")
        );
        assert_eq!(task_label("task:proj-1").as_deref(), Some("project=proj-1"));
        assert_eq!(task_label("conv-9").as_deref(), None);
    }

    #[test]
    fn dispatch_arguments_are_parsed_from_the_tool_call() {
        let aliases = AliasTable::from([("S1".to_string(), "sess-1".to_string())]);
        let call = ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: acowork_core::providers::traits::FunctionCall {
                name: super::super::tool::DISPATCH_TOOL.into(),
                arguments: r#"{"target":"S1"}"#.into(),
            },
        };
        let parsed = parse_arguments(&call, &aliases).unwrap();
        assert_eq!(parsed.target, Target::Existing("sess-1".into()));
        assert_eq!(call_label(&call), "dispatch:S1");

        let new = ToolCall {
            function: acowork_core::providers::traits::FunctionCall {
                name: super::super::tool::DISPATCH_TOOL.into(),
                arguments: r#"{"target":"New"}"#.into(),
            },
            ..call
        };
        assert_eq!(call_label(&new), "dispatch:new");
    }

    #[test]
    fn a_malformed_arguments_string_is_a_validation_error_not_a_crash() {
        let call = ToolCall {
            id: "c".into(),
            call_type: "function".into(),
            function: acowork_core::providers::traits::FunctionCall {
                name: super::super::tool::DISPATCH_TOOL.into(),
                arguments: "{not json".into(),
            },
        };
        assert_eq!(
            parse_arguments(&call, &AliasTable::new()),
            Err(DispatchError::MissingTarget)
        );
        assert_eq!(call_label(&call), "dispatch:?");
    }

    #[test]
    fn candidate_lines_carry_no_session_ids() {
        // The alias is the only handle the model may see (design 29 §5.2).
        let candidate = SessionCandidate {
            session_id: "sess-secret".into(),
            title: "Fix login".into(),
            status: CandidateStatus::Idle,
            origin: CandidateOrigin::Chat,
            last_active_secs: 1_700_000_000,
            current: false,
        };
        let line = CandidateLine {
            alias: "S1".into(),
            current: candidate.current,
            status: candidate.status.label().into(),
            age: relative_age(1_700_000_120, candidate.last_active_secs),
            origin: candidate.origin.label().into(),
            title: candidate.title.clone(),
        };
        let rendered = TurnContext {
            surface: "dm",
            speaker: "u1",
            now: "2026-10-21T14:03:00+08:00",
            draft: None,
            candidates: std::slice::from_ref(&line),
            task: None,
            history_available: true,
            user_message: "continue",
        }
        .render();
        assert!(rendered.contains("S1"));
        assert!(!rendered.contains("sess-secret"));
    }

    #[test]
    fn a_continued_session_receives_the_users_own_words() {
        // Regression (2026-10-24): the router appended a context brief when
        // dispatching to an existing session, and it replaced the user's
        // message — the execution session's persisted `user` row and its
        // auto-title then held the router's paraphrase, and the brief restated
        // history that session already has.
        let brief = "把本地修改的代码提交一下。\n\n上下文（同一 session 内刚完成的工作）：1. …";
        assert_eq!(
            dispatch_content(false, Some(brief), "把本地修改的代码提交一下。"),
            "把本地修改的代码提交一下。"
        );
    }

    #[test]
    fn a_fresh_session_keeps_the_routers_brief() {
        // A session that starts empty has nothing else to work from. This
        // covers `dispatch(new)` and a `target=S3` whose session has vanished
        // from disk — `chat_dispatch` reports `opened_new` for both.
        assert_eq!(
            dispatch_content(true, Some("Commit the batch described below: …"), "raw"),
            "Commit the batch described below: …"
        );
        // Without a brief (e.g. an attachment-only turn) the text still lands.
        assert_eq!(dispatch_content(true, None, "raw"), "raw");
    }

    fn info_call(name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: acowork_core::providers::traits::FunctionCall {
                name: name.into(),
                arguments: arguments.into(),
            },
        }
    }

    fn deps() -> RouterDeps {
        RouterDeps {
            instance_id: "i-1".into(),
            gateway_host: "127.0.0.1".into(),
            node_token: String::new(),
            work_dir: ".".into(),
        }
    }

    /// A tool that echoes the params it was called with, so the test can see
    /// exactly what the runner handed to the builtin layer.
    struct EchoTool {
        name: String,
    }

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
            acowork_core::tools::traits::ToolSpec {
                name: self.name.to_string(),
                description: "echo".to_string(),
                input_schema: serde_json::json!({ "type": "object" }),
            }
        }

        async fn execute(
            &self,
            params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
            Ok(acowork_core::tools::traits::ToolResult {
                ok: true,
                content: params.to_string(),
                error: None,
                token_usage: None,
            })
        }
    }

    #[tokio::test]
    async fn http_info_calls_are_forced_to_a_bounded_get() {
        // Even if the model invents mutating arguments, only a GET with the
        // url (+ string headers) reaches the tool (ADR-099 D3).
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool {
            name: HTTP_REQUEST_TOOL.to_string(),
        })];
        let call = info_call(
            HTTP_REQUEST_TOOL,
            r#"{"url":"https://example.com/time","method":"DELETE","body":{"evil":1},"headers":{"a":"b","c":42},"timeout_secs":9999}"#,
        );
        let out = execute_info_tool(&tools, &call, &deps(), "now?", "cs-test", 8000).await;
        let echoed: Value = serde_json::from_str(&out)
            .unwrap_or_else(|e| panic!("echoed json params: {e}\nout: {out}"));
        assert_eq!(echoed["method"], "GET");
        assert_eq!(echoed["url"], "https://example.com/time");
        assert_eq!(echoed["headers"], serde_json::json!({ "a": "b" }));
        assert!(echoed.get("body").is_none());
        assert!(
            echoed["timeout_secs"].as_u64().unwrap() <= 8,
            "inner timeout must stay under the turn budget"
        );

        // Non-http schemes are rejected before any request runs.
        let bad = info_call(HTTP_REQUEST_TOOL, r#"{"url":"file:///etc/passwd"}"#);
        let out = execute_info_tool(&tools, &bad, &deps(), "x", "cs-test", 1000).await;
        assert!(out.starts_with("Error:"), "got: {out}");
        assert!(out.contains("http(s)"));
    }

    #[tokio::test]
    async fn unavailable_or_malformed_info_calls_come_back_as_tool_errors() {
        // A hallucinated or disabled tool name is feedback, not a turn
        // failure - the model gets one more chance to answer or route.
        let out = execute_info_tool(&[], &info_call("web_fetch", "{}"), &deps(), "x", "cs-test", 1000).await;
        assert!(out.starts_with("Error:"));
        assert!(out.contains("not available"));

        // Malformed argument JSON degrades to Null params, not a panic.
        let memory = memory_recall_tool_name();
        let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(EchoTool {
            name: memory.clone(),
        })];
        let out = execute_info_tool(
            &tools,
            &info_call(&memory, "{oops"),
            &deps(),
            "x",
            "cs-test",
            1000,
        )
        .await;
        assert!(!out.starts_with("Error:"), "got: {out}");
    }

    #[test]
    fn oversized_info_results_are_cut_on_char_boundaries() {
        let big = "汉".repeat(3000); // 9000 bytes > INFO_RESULT_MAX_BYTES
        let cut = truncate_info_result(&big);
        assert!(cut.len() <= INFO_RESULT_MAX_BYTES + "…[truncated]".len());
        assert!(cut.ends_with("…[truncated]"));
        // Valid UTF-8 by construction.
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
        // Small results pass through untouched.
        assert_eq!(truncate_info_result("fine"), "fine");
    }

    #[test]
    fn chat_tail_excerpt_keeps_recent_conversation_and_drops_chrome() {
        let row = |kind: &str, from: &str, body: &str, seq: u64| ChatRow {
            seq,
            kind: kind.to_string(),
            from: from.to_string(),
            body: body.to_string(),
            ..Default::default()
        };
        let rows = vec![
            row("user_message", "u-1", "first", 1),
            row("system_notice", "agent-1", "routing chrome", 2),
            row("agent_message", "agent-1", "answer one", 3),
            row("user_message", "u-1", "and the deploy script?", 4),
        ];
        let excerpt = chat_tail_excerpt(&rows, "agent-1").expect("conversation rows exist");
        // Oldest first, sender-tagged, notices excluded.
        assert_eq!(
            excerpt,
            "[user] first\n[agent] answer one\n[user] and the deploy script?"
        );

        // Nothing but the trigger row itself (or empty bodies) -> no excerpt.
        assert!(chat_tail_excerpt(&[row("user_message", "u-1", "  ", 1)], "agent-1").is_none());
        assert!(chat_tail_excerpt(&[], "agent-1").is_none());

        // Byte cap: a huge newest row is clipped, the older rows survive.
        let big = row("user_message", "u-1", &"x".repeat(4000), 5);
        let excerpt = chat_tail_excerpt(&[rows[0].clone(), big.clone()], "agent-1").unwrap();
        assert!(excerpt.starts_with("[user] first\n[user] xxx"));
        assert!(excerpt.len() < 4000);
        assert!(!excerpt.ends_with('y')); // clipped, not copied whole
    }
}


