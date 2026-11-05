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
//! 4. Block B projection + Block C tail, model call(s) — the two acts
//!    (`exec_session_create`, `exec_session_propose_continue`) plus read-only
//!    information tools (`memory_recall`, GET-only `http_request`,
//!    `exec_session_read_round`); information rounds stay inside
//!    one turn so a full user → assistant → tool_call → tool_result → assistant
//!    chain is never cut across turns;
//! 5. act: reply (the write-back channel), start a NEW execution session, ask
//!    the human to confirm a continuation, or degrade to the pre-ADR-099
//!    deterministic route — never silently continue a session (ADR-106 D6);
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
    memory_recall_tool_name, parse_exec_session_create, parse_exec_session_propose_continue,
    parse_exec_session_read_round, router_tools, Act, ActError, AliasTable,
    EXEC_SESSION_CREATE_TOOL, EXEC_SESSION_PROPOSE_CONTINUE_TOOL, EXEC_SESSION_READ_ROUND_TOOL,
    HTTP_REQUEST_TOOL, Target,
};
use super::ChatSlot;
use crate::agent::agent_core::AgentCore;
use crate::agent::inbound::IntentEnvelope;
use crate::agent::session::chat_candidates::{
    self, CandidateSurface, SessionCandidate,
};
use crate::agent::session::intent_routing::SessionSeed;
use crate::agent::session::{SessionManager, SessionMessage};
use crate::agent_config::ChatRouterConfig;

/// Per-result ceiling for an information tool fed back to the model. Generous
/// (a whole page of search hits or recalled memories), yet bounded: one
/// runaway HTTP body must not swamp the router request. The truncation
/// marker keeps the round structurally complete.
const INFO_RESULT_MAX_BYTES: usize = 8 * 1024;

/// Everything a turn needs from the Runtime process, snapshotted once so the
/// `SessionManager` lock is not held across the HTTP/LLM calls.
pub struct RouterDeps {
    /// This agent's instance id — the `sender` the Gateway attributes rows to.
    pub instance_id: String,
    pub gateway_host: String,
    pub node_token: String,
    pub work_dir: String,
    /// Token ledger for this router's own LLM spend (design 29 follow-up).
    /// Every successful model call of a turn is recorded here — the router
    /// is the only LLM consumer in the Runtime outside a session, and an
    /// un-counted one skews the agent totals and the model-share panel.
    pub chat_usage: Arc<dyn crate::usecases::ChatUsageService>,
}

impl std::fmt::Debug for RouterDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouterDeps")
            .field("instance_id", &self.instance_id)
            .field("gateway_host", &self.gateway_host)
            .field("node_token", &"[redacted]")
            .field("work_dir", &self.work_dir)
            .field("chat_usage", &"<ChatUsageService>")
            .finish()
    }
}

impl RouterDeps {
    pub fn from_core(core: &Arc<AgentCore>, node_token: &str) -> Self {
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
            chat_usage: crate::usecases::chat_usage_impl::RuntimeChatUsageService::new(
                core.clone(),
            ),
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

/// ADR-106 D6 levels 1–2: the execution session the **human** addressed this
/// turn, written by the Gateway when the message replies to a round's
/// reference row or names a session in the second-level `@`.
///
/// `None` means unaddressed — the only case in which a model may decide
/// anything. An addressed turn is delivered before the Chat Router is consulted,
/// because a guess that happens to agree with an address is still a guess, and a
/// guess that disagrees silently overrides the person who typed it.
///
/// An empty or blank value reads as absent: "addressed at nothing" and "not
/// addressed" must not diverge later, and a half-written key is the failure mode
/// this reader exists to survive.
pub fn addressed_session(envelope: &IntentEnvelope) -> Option<String> {
    envelope
        .params
        .get(acowork_core::protocol::TARGET_SESSION_PARAM)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// ADR-106 D6 level 3's precondition: the human named **this** agent.
///
/// The rule it gates — "one routable session in this chat, so continue it with
/// no model in the loop" — is deterministic and cheap, which is exactly why it
/// must not fire on a message nobody addressed. In a group the agent hears
/// everything; reading idle chatter as an `@` would open execution work on
/// sentences that were never aimed at anyone.
pub fn explicit_mention(envelope: &IntentEnvelope) -> bool {
    envelope
        .params
        .get(acowork_core::protocol::EXPLICIT_MENTION_PARAM)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// ADR-106 D6's cheap escape, read off a Chat turn: the human refused
/// continuation, so no session may be carried forward.
///
/// Absent and `false` mean the same thing here, which is why the Gateway writes
/// the key only when it is true. Skipping continuation is not "route
/// nowhere": the turn still reaches the ladder's next level, where a new session
/// may be opened with a fresh brief.
pub fn force_new(envelope: &IntentEnvelope) -> bool {
    envelope
        .params
        .get(acowork_core::protocol::FORCE_NEW_PARAM)
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
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

/// A Chat Router turn is a human `user_message` delivered from a Chat
/// session (design 27 §5.4.4). Everything else — PM task dispatches
/// (keyed on the task conversation), cross-agent requests, responses,
/// cron triggers — keeps the deterministic route. The eligibility comes
/// from the origin policy table, not a kind-string comparison.
///
/// Returns the message text to route. `Some("")` is legitimate: an
/// attachment-only message still has to be routed.
fn chat_turn_text(envelope: &IntentEnvelope) -> Option<String> {
    if !acowork_core::intent::TurnOrigin::parse(&envelope.kind)
        .policy()
        .router_eligible
        || envelope.source_chat_session_id.is_empty()
    {
        return None;
    }
    let text = envelope
        .params
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    // A turn with nothing typed still has to be routed when it carries
    // something, and the question has one owner ([`super::context::
    // turn_has_content`]) — the same one Block C answers, so the gate cannot
    // wave through a turn the prompt then renders as an empty message.
    // `chat_attachment_ids` keeps one extra vote on top: an older or
    // half-deployed Gateway sends ids without the `chat_attachments` metadata
    // param (see [`turn_attachments`]), and dropping a turn for want of a name
    // is a lost message.
    let uploads = turn_attachments(envelope);
    let refs = turn_refs(envelope);
    if !super::context::turn_has_content(&text, &uploads, &refs)
        && envelope.chat_attachment_ids.is_empty()
    {
        return None;
    }
    Some(text)
}

/// This turn's own attachment metadata — what the human actually attached.
///
/// Deliberately tolerant: the Gateway only started sending `chat_attachments`
/// alongside `chat_attachment_ids`, so an older (or half-deployed) Gateway
/// yields an empty list. A turn routed without the file list is a worse
/// decision; a turn that fails to run because of it is a lost message.
///
/// The single reader of the param: the Block C renderer, the decision log and
/// the degraded route's brief all go through here, so "does this turn have
/// files?" has exactly one answer.
pub(crate) fn turn_attachments(
    envelope: &IntentEnvelope,
) -> Vec<acowork_core::protocol::ChatAttachmentMeta> {
    envelope
        .params
        .get(acowork_core::protocol::CHAT_ATTACHMENTS_PARAM)
        .and_then(|v| {
            serde_json::from_value::<Vec<acowork_core::protocol::ChatAttachmentMeta>>(v.clone())
                .ok()
        })
        .unwrap_or_default()
        .into_iter()
        // Per-field serde defaults mean a wrong-shaped element parses into an
        // entry describing nothing. Drop those: Block C must never claim "there
        // is a file" with no name to name it by, and the decision log's count
        // must mean files, not array slots.
        .filter(|a: &acowork_core::protocol::ChatAttachmentMeta| {
            !a.filename.is_empty() || !a.mime.is_empty()
        })
        .collect()
}

/// Workspace references this turn carries — the ADR-046 `attached_items` arm
/// the composer stamped into `params_json`.
///
/// This is the second of the two lists a Chat turn can arrive with, and the
/// router used to read only the first. A "add to chat" reference rides on
/// `attached_items` and never on `chat_attachments` (the Gateway cannot read a
/// node path to describe it), so a message whose whole content was a reference
/// was routed as an empty one and answered with "you attached nothing" while
/// the UI showed the file. Same blindness, one missing reader.
///
/// Projected through [`AttachmentLabel`], the identical type the Chat row
/// stores and the desktop renders, so the model, the transcript and the chip
/// cannot drift apart.
pub(crate) fn turn_refs(envelope: &IntentEnvelope) -> Vec<acowork_core::protocol::AttachmentLabel> {
    crate::agent::intent_turn::attached_items_from_params(&envelope.params)
        .unwrap_or_default()
        .iter()
        .filter_map(acowork_core::protocol::AttachmentLabel::from_item)
        .collect()
}

/// One `@session` reference the Gateway pinned onto this turn (ADR-106
/// revision 2026-11-04 · reference redrawing).
///
/// The quadruple is the pin the Gateway wrote before the row landed: the
/// round is decided, not re-decided here. Distinct from [`turn_refs`] —
/// those are *workspace* files the composer pointed at; these are quoted
/// rounds of an execution session, material for the model to read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub(crate) struct PinnedChatRef {
    #[allow(dead_code)] // the owning agent — the Gateway already filtered per target
    pub agent_instance_id: String,
    pub session_id: String,
    pub round_id: String,
    #[serde(default)]
    pub terminal: bool,
}

/// The single reader of `params.chat_refs` — the suppression check, the
/// Block C section, the decision path's reference parts and the delivery
/// chain all go through here, so "does this turn quote material?" has
/// exactly one answer.
///
/// Tolerant like [`turn_attachments`]: a missing key, a non-array, or an
/// element that does not describe a full coordinate all read as no
/// material — a producer bug may cost one block, never the human's message.
/// A pinned quadruple without a non-blank `round_id`/`session_id` describes
/// nothing and is dropped: the renderer would have to guess what to read.
pub(crate) fn pinned_chat_refs(envelope: &IntentEnvelope) -> Vec<PinnedChatRef> {
    pinned_chat_refs_from_params(&envelope.params)
}

/// [`pinned_chat_refs`] on raw params — the delivery chain reads the
/// envelope apart (gateway_loop destructures it), and both halves must
/// answer from one reader.
pub(crate) fn pinned_chat_refs_from_params(params: &Value) -> Vec<PinnedChatRef> {
    params
        .get(acowork_core::protocol::CHAT_REFS_PARAM)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| serde_json::from_value::<PinnedChatRef>(v.clone()).ok())
                .filter(|r| {
                    !r.session_id.trim().is_empty() && !r.round_id.trim().is_empty()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Total budget for one turn's `[referenced rounds]` material (Block C and
/// the delivered `referenced_round` parts alike). Per-ref bodies already
/// carry the per-field clamps of [`read_round_result`]; this bounds the
/// *segment*, because five references at full size
/// would out-shout the human's own message. Past the budget the remaining
/// refs collapse into one explicit dropped line — the reader learns the
/// material was cut, it never learns it silently vanished.
pub(crate) const REFS_TOTAL_MAX_BYTES: usize = 8 * 1024;

/// The collapse line [`render_pinned_rounds`] writes for refs past the
/// budget. Public wording: it reaches the model and the execution-side
/// transcript verbatim.
const REFS_DROPPED_LINE: &str = "dropped: reference budget exhausted";

/// Render the pinned rounds of [`PinnedChatRef`]s into one body per ref —
/// the same lines [`read_round_result`] paints for `exec_session_read_round`
/// (question / answer / card, same per-field clamps), prefixed with the
/// source coordinates the parts must name, plus the pin's non-terminal
/// mark. Disk-only through `fold_or_rebuild` + `project`, so a session that
/// is not resident is still readable; every failure shape is an explicit
/// hole, never a swallowed ref.
///
/// This is the ONE producer of the material bytes: Block C renders these
/// strings and the delivery ships them as `referenced_round` parts, so what
/// the router was shown and what the execution session receives cannot
/// drift.
pub(crate) fn render_pinned_rounds(
    conversations_dir: &std::path::Path,
    refs: &[PinnedChatRef],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(refs.len());
    let mut used = 0usize;
    for r in refs {
        let entry = render_one_pinned_round(conversations_dir, r);
        if used + entry.len() > REFS_TOTAL_MAX_BYTES {
            out.push(REFS_DROPPED_LINE.to_string());
            break;
        }
        used += entry.len();
        out.push(entry);
    }
    out
}

fn render_one_pinned_round(conversations_dir: &std::path::Path, r: &PinnedChatRef) -> String {
    let jsonl = conversations_dir.join(format!("{}.jsonl", r.session_id));
    let rounds_path = crate::rounds::rounds_path(conversations_dir, &r.session_id);
    let index = match crate::rounds::fold_or_rebuild(&rounds_path, &jsonl) {
        Ok(m) => m,
        Err(e) => {
            return format!(
                "session {} · round {}: read failed: {e}\n",
                r.session_id, r.round_id
            )
        }
    };
    let Some(rec) = index.get(&r.round_id) else {
        return format!(
            "session {} · round {}: content unavailable (no such round on this host)\n",
            r.session_id, r.round_id
        );
    };
    match crate::rounds::project(&jsonl, rec) {
        Ok(Some(p)) => {
            let mut out = format!(
                "session {} · round {} · {}{}\n",
                r.session_id,
                p.round_id,
                p.status,
                if r.terminal { "" } else { " (was still running)" }
            );
            if let Some(u) = &p.user {
                out.push_str(&format!(
                    "question: {}\n",
                    truncate_body(&u.body, READ_ROUND_USER_MAX_BYTES)
                ));
            }
            match &p.answer {
                Some(a) => out.push_str(&format!(
                    "answer: {}\n",
                    truncate_body(&a.body, READ_ROUND_ANSWER_MAX_BYTES)
                )),
                None => out.push_str("answer: (none yet)\n"),
            }
            if p.card.is_some() {
                out.push_str("card: parked on a question for the human\n");
            }
            out
        }
        Ok(None) => format!(
            "session {} · round {}: content unavailable (transcript pruned or rows moved)\n",
            r.session_id, r.round_id
        ),
        Err(e) => format!(
            "session {} · round {}: read failed: {e}\n",
            r.session_id, r.round_id
        ),
    }
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
    let budget = WindowBudget::new(cfg.history_max_tokens(), cfg.history_rows());
    let now_secs = chrono::Utc::now().timestamp();
    let now = chrono::Utc::now().to_rfc3339();

    // ── Candidate rows: off disk, before the lock ────────────────────
    // The meta scan is the slowest step of this turn's setup and needs nothing
    // from `SessionManager` but the work dir, which `RouterDeps` already
    // snapshotted. Under the global lock it would make `POST /sessions` and
    // every other session operation queue behind a filesystem walk.
    let candidate_rows = chat_candidates::scan_rows(&deps.work_dir);
    // Same read-as-you-can discipline: the intent bindings are another disk
    // scan (ADR-106 D11 — they live on the session rows now), so they are
    // collected outside the lock and handed to `resolve_candidates` as data.
    let candidate_bindings = chat_candidates::scan_bindings(&deps.work_dir);

    // ── Short lock: prompt + candidates + router model ──────────────────
    let prepared = {
        let guard = session_manager.lock().await;
        let core = guard.core();
        // A group session's own `chat.md` (forwarded by the Gateway in the
        // delivery params) wins over the package's `prompts/chat.md`; either
        // one replaces the whole built-in prompt (ADR-099, extended).
        let session_prompt = envelope
            .params
            .get("chat_prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let package_persona = core.chat_persona();
        let block_a = build_block_a(
            &PersonaSource {
                display_name: core.manifest.display_name.as_deref(),
                name: &core.manifest.name,
                role: core.manifest.role.as_deref(),
                description: &core.manifest.description,
            },
            session_prompt.or(package_persona.as_deref()),
            &chat_kind,
            guard.identity_context(),
        );
        let candidates = guard.resolve_candidates(
            &candidate_rows,
            &candidate_bindings,
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
            status: candidate.status.label().to_string(),
            age: relative_age(now_secs, candidate.last_active_secs),
            origin: candidate.origin.label().to_string(),
            title: candidate.title.clone(),
        });
    }
    let draft = draft_label(&envelope.session_config_json);
    let task = task_label(&envelope.conversation_id);
    let attachments = turn_attachments(envelope);
    let refs = turn_refs(envelope);
    // ADR-106 revision 2026-11-04: the pinned `@session` material, read from
    // disk once per turn. The same rendered bodies enter Block C here and
    // become `referenced_round` parts if the turn is dispatched — one
    // producer, so the router and the execution session cannot drift.
    let pinned = pinned_chat_refs(envelope);
    let referenced_rounds = if pinned.is_empty() {
        Vec::new()
    } else {
        render_pinned_rounds(
            std::path::Path::new(&deps.work_dir).join("conversations").as_path(),
            &pinned,
        )
    };
    let turn_view = TurnContext {
        surface: surface_label(&chat_kind),
        speaker: &envelope.from,
        now: &now,
        draft: Some(draft.as_str()).filter(|d| !d.is_empty()),
        candidates: &lines,
        task: task.as_deref(),
        history_available,
        user_message: text,
        attachments: &attachments,
        refs: &refs,
        referenced_rounds: &referenced_rounds,
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
        // `cfg.max_output_tokens()`, not a constant: the ceiling must cover a
        // `exec_session_create` brief *and* an information round (a thinking block plus
        // tool-call arguments plus the final sentence), which the old
        // hardcoded 2,048 did not. See `ChatRouterConfig::max_output_tokens`.
        max_tokens: Some(cfg.max_output_tokens()),
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
        history_tokens: block_b.token_len,
        attachments: attachments.len(),
        refs: refs.len(),
        attachment_only: super::context::is_attachment_only(text, &attachments, &refs),
        started,
        info_rounds: 0,
    };

    let calls = ModelCalls {
        deps,
        chat_id: &chat_id,
        chat_kind: &chat_kind,
        provider: &prepared.provider,
        provider_label: &prepared.provider_label,
        model: &prepared.model,
        timeout_ms: cfg.timeout_ms(),
        // The window this turn projected — the budget's occupancy.
        history_tokens: Some(block_b.token_len as u64),
    };

    let mut response = match calls.call(request.clone()).await {
        Ok(r) => r,
        Err(reason) => {
            trace.write("fallback", None, "error", Some(reason.clone()), None);
            return degrade(deps, envelope, &chat_id, reason, &rows).await;
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
        // An act - even after earlier lookups - outranks executing more info
        // calls: the act is the turn's decision, the lookups were only context
        // for it.
        if act_call(&response).is_some() {
            break;
        }
        let info_calls: Vec<ToolCall> = response
            .tool_calls
            .as_ref()
            .map(|calls| {
                calls
                    .iter()
                    .filter(|call| !is_act(&call.function.name))
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
            // `exec_session_read_round` is not a `Tool` off the catalog: it is
            // the router's own bounded read over the round projection store,
            // resolved against this turn's alias table — an arm of the router,
            // not a shared builtin.
            let result = if call.function.name == EXEC_SESSION_READ_ROUND_TOOL {
                exec_read_round(call, deps, &aliases)
            } else {
                execute_info_tool(
                    &prepared.info_tools,
                    call,
                    deps,
                    text,
                    &chat_id,
                    cfg.timeout_ms(),
                )
                .await
            };
            request.messages.push(ChatMessage::tool(call.id.clone(), result));
        }
        response = match calls.call(request.clone()).await {
            Ok(r) => r,
            Err(reason) => {
                trace.write("fallback", None, "error", Some(reason.clone()), None);
                return degrade(deps, envelope, &chat_id, reason, &rows).await;
            }
        };
    }

    // ── Act ────────────────────────────────────────────────────────────
    let mut act: Option<(Act, String)> = None;
    if let Some(call) = act_call(&response) {
        match parse_act(&call, &aliases) {
            Ok(parsed) => act = Some((parsed, call_label(&call))),
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
                match calls.call(request.clone()).await {
                    Ok(retry) => {
                        response = retry;
                        if let Some(second) = act_call(&response) {
                            match parse_act(&second, &aliases) {
                                Ok(parsed) => act = Some((parsed, call_label(&second))),
                                Err(again) => {
                                    let reason =
                                        format!("invalid act twice: {}", again.message());
                                    trace.write("fallback", None, "error", Some(reason.clone()), None);
                                    return degrade(deps, envelope, &chat_id, reason, &rows).await;
                                }
                            }
                        }
                    }
                    Err(reason) => {
                        trace.write("fallback", None, "error", Some(reason.clone()), None);
                        return degrade(deps, envelope, &chat_id, reason, &rows).await;
                    }
                }
            }
        }
    }

    let usage = response.usage.clone();
    match act {
        None => {
            // A truncated response is not a decision. `finish_reason == "length"`
            // means the provider cut the model off at `chat_router
            // .max_output_tokens`, so whatever `content` holds is the tail of an
            // unfinished thought. Publishing it as the router's answer is exactly
            // how an `exec_session_create` turn became "an agent message with no execution
            // session": the brief ran long, the tool-call JSON was cut mid-way,
            // `act_call()` found no act, and the partial prose was written to the
            // chat as if the model had chosen to reply. The provider already
            // normalises Anthropic's `max_tokens` to `length` (see
            // `providers/anthropic.rs::normalize_stop_reason`); the router is the
            // caller that has to look at it.
            if is_truncated(&response) {
                let reason = format!(
                    "output truncated at {} tokens — an `{EXEC_SESSION_CREATE_TOOL}` brief that \
                     does not fit is cut mid-call",
                    cfg.max_output_tokens()
                );
                trace.write("fallback", None, "error", Some(reason.clone()), None);
                return degrade(deps, envelope, &chat_id, reason, &rows).await;
            }
            let body = response.content.trim().to_string();
            // Some Anthropic-compatible endpoints hand the tool call back inside
            // the message body (`<tool_call><invoke name=…>`). The router has no
            // textual tool-call parser — and should not grow one — but it must
            // not publish that envelope to the human as if the model had written
            // prose. Observed in production: the whole turn surfaced as a chat
            // answer that *looked* like a tool call and routed nothing.
            if body.is_empty() || looks_like_tool_call_envelope(&body) {
                let reason = if body.is_empty() {
                    "empty reply".to_string()
                } else {
                    "tool call written as message text (no structured act)".to_string()
                };
                trace.write("fallback", None, "error", Some(reason.clone()), None);
                return degrade(deps, envelope, &chat_id, reason, &rows).await;
            }
            // ADR-106 D12: the router's own answer is a chat-native row, so it
            // is written here and not carried back through an intent response —
            // this is the only place `agent_message` is still produced, and the
            // Gateway closes the tracked request off that row's `in_reply_to`.
            // The idempotency key is the request id, so a redelivered turn
            // leaves no second answer beside the first.
            let sent = crate::agent::intent_turn::append_chat_row(
                &deps.instance_id,
                &deps.gateway_host,
                &deps.node_token,
                &chat_id,
                acowork_core::intent::chat_kinds::AGENT_MESSAGE,
                &envelope.from,
                &body,
                crate::agent::intent_turn::router_answer_meta(&envelope.message_id),
            )
            .await;
            match sent {
                Ok(()) => {
                    trace.write("reply", None, "ok", None, usage.as_ref().map(|u| (u.prompt_tokens, u.completion_tokens, u.cache_read_tokens, u.cache_write_tokens)));
                    Outcome::Handled
                }
                Err(e) => {
                    trace.write("fallback", None, "error", Some(e.clone()), None);
                    degrade(deps, envelope, &chat_id, format!("reply write-back failed: {e}"), &rows).await
                }
            }
        }
        Some((Act::ProposeContinue(proposal), label)) => {
            // The act IS the question. Nothing is delivered: continuing a
            // session is addressing, and addressing belongs to the human
            // (ADR-106 D6). The card names the candidate, the router's reason
            // for proposing it, and the "new task" escape beside it.
            //
            // The label is this turn's own candidate title, looked up by the id
            // the alias resolved to — a card naming a session the human cannot
            // see in [candidates] would be a decision made from private state.
            let title = prepared
                .candidates
                .iter()
                .find(|c| c.session_id == proposal.session_id)
                .map(|c| c.title.clone())
                .unwrap_or_else(|| proposal.alias.clone());
            let option = serde_json::json!({
                "session_id": proposal.session_id,
                "label": title,
                "why": proposal.why,
            });
            let asked = crate::agent::intent_turn::emit_session_choice(
                &deps.instance_id,
                &deps.gateway_host,
                &deps.node_token,
                &chat_id,
                &envelope.from,
                &envelope.message_id,
                "confirm_continue",
                vec![option],
            )
            .await;
            match asked {
                Ok(()) => {
                    trace.write(&label, Some(&proposal.session_id), "ok", None, usage.as_ref().map(|u| (u.prompt_tokens, u.completion_tokens, u.cache_read_tokens, u.cache_write_tokens)));
                    Outcome::Handled
                }
                Err(e) => {
                    // A question that could not be written must not be answered
                    // by delivering after all — that is the act this arm refuses
                    // to perform. The deterministic route is honest instead: it
                    // continues a session only when there is exactly one to
                    // continue.
                    let reason = format!("proposal card write failed: {e}");
                    trace.write(&label, None, "error", Some(reason.clone()), None);
                    degrade(deps, envelope, &chat_id, reason, &rows).await
                }
            }
        }
        Some((Act::ExecSessionCreate(args), label)) => {
            // An `exec_session_create` without `session_config` still honours the user's
            // draft configuration — that is the whole point of not forcing a
            // new session on a config change.
            let seed = args
                .seed
                .clone()
                .or_else(|| SessionSeed::from_json(&envelope.session_config_json));
            // `opened_new` is read back from the dispatch rather than assumed:
            // `Target::New` creates, and the background must not be dropped if
            // that ever stops being true — the new session would open on the
            // human's words alone, with nothing to resolve them against.
            let background = Some(args.background.clone());
            let delivered = {
                let mut guard = session_manager.lock().await;
                match guard
                    .chat_dispatch(&chat_id, Target::New, seed, Some(envelope.from.as_str()))
                    .await
                {
                    Ok((session_id, opened_new)) => {
                        let (content, context_refs) =
                            dispatch_parts(opened_new, background.as_deref(), text, &referenced_rounds);
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
                                // The queue row is the only place the human
                                // reads what is waiting on a busy session, so
                                // it falls back to the background when the turn
                                // carried no words of its own (an
                                // attachment-only message).
                                preview: crate::agent::session_state::pending_preview(
                                    &crate::agent::session_state::preview_source(
                                        &content,
                                        &context_refs,
                                    ),
                                ),
                                from_chat: true,
                            },
                        );
                        let sent = guard.send_to_session(
                            &session_id,
                            SessionMessage::Intent {
                                content,
                                message_id: envelope.message_id.clone(),
                                surface: crate::agent::intent_turn::TurnSurface::new(
                                    chat_id.clone(),
                                    &envelope.message_id,
                                    &envelope.from,
                                    envelope.has_human,
                                ),
                                from: envelope.from.clone(),
                                action: envelope.action.clone(),
                                repo: envelope.repo.clone(),
                                kind: envelope.kind.clone(),
                                chat_attachment_ids: envelope.chat_attachment_ids.clone(),
                                // Same arm the execution surface's own write
                                // fills on its `ChatMessage`: a workspace reference means one
                                // thing whichever transport carried the turn.
                                attached_items: crate::agent::intent_turn::attached_items_from_params(
                                    &envelope.params,
                                )
                                .unwrap_or_default(),
                                in_reply_to: envelope.in_reply_to.clone(),
                                // The router only takes Chat-originated
                                // turns (policy.router_eligible); a PM
                                // dispatch's brief never arrives here. The task
                                // id is still parsed off the conversation key so
                                // the two transports state the same fact the same
                                // way — empty for a chat conversation.
                                assignment_brief: String::new(),
                                task_id: acowork_core::intent::task_id_from_conversation(
                                    &envelope.conversation_id,
                                )
                                .unwrap_or_default(),
                                // ADR-106 D3: the routing's own verdict, carried
                                // to the round's reference row because the loop
                                // cannot see it — it replaces the retired
                                // `turn_routed` reaction row.
                                opened_new: Some(opened_new),
                                // ADR-107: the router's framing, delivered as a
                                // reference part of the human's turn.
                                context_refs,
                            },
                        );
                        (Ok((session_id, opened_new)), sent)
                    }
                    Err(e) => (Err(e.to_string()), Ok(())),
                }
            };
            match delivered {
                (Ok((session_id, _opened_new)), Ok(())) => {
                    // ADR-106 D3: no reaction row is written here. "Your message
                    // went to a new task / continues task X" is now stated by the
                    // round's reference row (`opened_new` on it), which the
                    // runtime writes when the round actually opens — before that
                    // the message is only queued, and a reaction claiming a round
                    // that has not started would be a promise, not a fact.
                    trace.write(&label, Some(&session_id), "ok", None, usage.as_ref().map(|u| (u.prompt_tokens, u.completion_tokens, u.cache_read_tokens, u.cache_write_tokens)));
                    Outcome::Handled
                }
                (Ok(_), Err(e)) => {
                    let reason = format!("exec_session_create delivery failed: {e}");
                    trace.write(&label, None, "error", Some(reason.clone()), None);
                    degrade(deps, envelope, &chat_id, reason, &rows).await
                }
                (Err(e), _) => {
                    let reason = format!("exec_session_create session creation failed: {e}");
                    trace.write(&label, None, "error", Some(reason.clone()), None);
                    degrade(deps, envelope, &chat_id, reason, &rows).await
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

/// The one path from a turn to the provider: make the call, then book it.
///
/// Accounting belongs to the call, not to the caller — "every successful model
/// call is billable spend" is a property of the call itself, and a turn makes
/// 1–3 of them (first answer, information rounds, one dispatch retry). Keeping
/// the booking at each call site is how a later site silently under-bills the
/// chat, so `run_turn` builds one `ModelCalls` and every provider round goes
/// through [`ModelCalls::call`].
///
/// A Provider that reported no usage at all folds as "no measurement" —
/// skipped, never fabricated (ADR-027). The ledger is the chat's own row; the
/// agent totals are fed even when the ledger store is unavailable.
struct ModelCalls<'a> {
    deps: &'a RouterDeps,
    chat_id: &'a str,
    chat_kind: &'a str,
    provider: &'a Arc<dyn Provider>,
    provider_label: &'a str,
    model: &'a str,
    timeout_ms: u64,
    /// Tokens the projected history window (Block B) cost on this turn.
    ///
    /// This is the budget's occupancy: `chat_router.history_max_tokens`
    /// bounds the HISTORY window alone, so the provider-reported prompt
    /// count (which also carries the system prompt and the tool schemas)
    /// is not comparable with it. Booked onto the ledger row as a
    /// snapshot — the window is refilled per turn, so only the latest
    /// occupancy is meaningful.
    history_tokens: Option<u64>,
}

impl ModelCalls<'_> {
    async fn call(&self, request: ChatRequest) -> Result<ChatResponse, String> {
        let response = call_provider(self.provider, request, self.timeout_ms).await?;
        if let Some(usage) = response.usage.as_ref() {
            self.deps.chat_usage.record_chat_llm_usage(
                self.chat_id,
                self.chat_kind,
                usage,
                self.provider_label,
                self.model,
                self.history_tokens,
            );
        }
        Ok(response)
    }
}

/// Transport only — timeouts, error shaping, think-block sanitizing. Billing
/// lives in [`ModelCalls::call`], which is the only caller production code has.
async fn call_provider(
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
            "Error: `{name}` is not available on a chat turn — valid tools: {EXEC_SESSION_CREATE_TOOL}, {EXEC_SESSION_PROPOSE_CONTINUE_TOOL}, {EXEC_SESSION_READ_ROUND_TOOL}, {available} (GET only for {HTTP_REQUEST_TOOL})"
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

/// Per-round ceiling on the answer body inside an `exec_session_read_round`
/// result. The whole result is additionally bounded by
/// [`INFO_RESULT_MAX_BYTES`]; this one is per round, so one long latest
/// report cannot crowd the other asked rounds out.
const READ_ROUND_ANSWER_MAX_BYTES: usize = 2 * 1024;
/// The same rule for the human's question: a summary line, not a transcript.
const READ_ROUND_USER_MAX_BYTES: usize = 512;

/// Run one `exec_session_read_round` call: the bounded round projection read
/// (ADR-106 D4) straight against this process's conversations directory —
/// the same files and the same fold the loopback HTTP endpoint serves, minus
/// the hop, and disk-only like it, so a session that is not resident is
/// still readable.
///
/// The alias discipline is the proposal's: the read can only name a session
/// this turn's permission-filtered candidate set admitted. Everything failing
/// here is a *tool result*, never a turn failure — a torn transcript is an
/// explicit hole the model reports as one, not a reason to degrade the turn.
fn exec_read_round(call: &ToolCall, deps: &RouterDeps, aliases: &AliasTable) -> String {
    let raw: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    let parsed = match parse_exec_session_read_round(&raw, aliases) {
        Ok(p) => p,
        Err(err) => return format!("Error: {}", err.message()),
    };
    let dir = std::path::Path::new(&deps.work_dir).join("conversations");
    let text = read_round_result(&dir, &parsed.session_id, &parsed.alias, parsed.rounds);
    truncate_info_result(&text)
}

/// The read itself, split from the call envelope so tests can point it at a
/// temp directory without a live `RouterDeps`.
fn read_round_result(
    conversations_dir: &std::path::Path,
    session_id: &str,
    alias: &str,
    rounds: u32,
) -> String {
    use super::context::truncate_body;
    let jsonl = conversations_dir.join(format!("{session_id}.jsonl"));
    let rounds_path = crate::rounds::rounds_path(conversations_dir, session_id);
    let index = match crate::rounds::fold_or_rebuild(&rounds_path, &jsonl) {
        Ok(m) => m,
        Err(e) => return format!("Error: could not read the round index: {e}"),
    };
    if index.is_empty() {
        return format!(
            "Session `{alias}` has no recorded round yet — the delivery may still \
             be queued, or its first turn is still running."
        );
    }
    // Newest N by commit time: `ts` is ISO-8601, so the lexical order is the
    // time order, and `round_id` breaks ties deterministically.
    let mut recs: Vec<&crate::rounds::RoundRecord> = index.values().collect();
    recs.sort_by(|a, b| (&a.ts, &a.round_id).cmp(&(&b.ts, &b.round_id)));
    let mut picked: Vec<_> = recs.into_iter().rev().take(rounds as usize).collect();
    // Oldest of the picked first: the model reads the tail of the story in
    // the order it happened.
    picked.reverse();
    let mut out = String::new();
    for rec in picked {
        match crate::rounds::project(&jsonl, rec) {
            Ok(Some(p)) => {
                out.push_str(&format!(
                    "round {} · {}{}\n",
                    p.round_id,
                    p.status,
                    if p.terminal { "" } else { " (still running)" }
                ));
                if let Some(u) = &p.user {
                    out.push_str(&format!(
                        "question: {}\n",
                        truncate_body(&u.body, READ_ROUND_USER_MAX_BYTES)
                    ));
                }
                match &p.answer {
                    Some(a) => out.push_str(&format!(
                        "answer: {}\n",
                        truncate_body(&a.body, READ_ROUND_ANSWER_MAX_BYTES)
                    )),
                    None => out.push_str("answer: (none yet)\n"),
                }
                if p.card.is_some() {
                    out.push_str("card: parked on a question for the human\n");
                }
            }
            // An explicit projection hole: say it is one — the model then
            // reports "no readable result yet", which is the honest answer.
            Ok(None) => out.push_str(&format!(
                "round {}: content unavailable (transcript pruned or rows moved)\n",
                rec.round_id
            )),
            Err(e) => out.push_str(&format!("round {}: read failed: {e}\n", rec.round_id)),
        }
    }
    out
}

/// The turn's act call, if the model made one.
///
/// When a model emits both acts in one response the **proposal wins**: it is the
/// arm that delivers nothing, so a self-contradictory answer costs the human one
/// click instead of moving their message into a session nobody addressed.
fn act_call(response: &ChatResponse) -> Option<ToolCall> {
    let calls = response.tool_calls.as_ref()?;
    calls
        .iter()
        .find(|call| call.function.name == EXEC_SESSION_PROPOSE_CONTINUE_TOOL)
        .or_else(|| {
            calls
                .iter()
                .find(|call| call.function.name == EXEC_SESSION_CREATE_TOOL)
        })
        .cloned()
}

/// Either act name — the information-round filter must not run an act as a
/// lookup, and a refusal text must name what the model may actually call.
fn is_act(name: &str) -> bool {
    name == EXEC_SESSION_CREATE_TOOL || name == EXEC_SESSION_PROPOSE_CONTINUE_TOOL
}

/// True when the provider stopped because it ran out of output budget rather
/// than because the model finished. Accepts both vocabularies: the adapters
/// normalise Anthropic's `max_tokens` to `length`, but a provider adapter added
/// later may pass the vendor string through unchanged.
fn is_truncated(response: &ChatResponse) -> bool {
    response
        .finish_reason
        .as_deref()
        .is_some_and(|reason| reason == "length" || reason == "max_tokens")
}

/// True when the model put a tool call in the message body instead of emitting
/// a structured one.
///
/// The router deliberately has no textual tool-call parser: an act carries a
/// delivery decision, and guessing one out of prose is not a decision the model
/// actually made. Recognising the envelope is still required, because otherwise
/// the raw markup is published to the human as the agent's answer — a chat row
/// that looks like a tool call and routes nothing.
///
/// Matching is case-insensitive and marker-based, not a parse. Some endpoints
/// splice a zero-width character into the opening tag (`<\u{200b}tool_call>`),
/// so the markers are the fragments either side of that seam.
fn looks_like_tool_call_envelope(body: &str) -> bool {
    const MARKERS: &[&str] = &["<invoke", "tool_call", "function_call"];
    let lowered = body.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lowered.contains(marker))
}

fn parse_act(call: &ToolCall, aliases: &AliasTable) -> Result<Act, ActError> {
    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    if call.function.name == EXEC_SESSION_PROPOSE_CONTINUE_TOOL {
        parse_exec_session_propose_continue(&args, aliases).map(Act::ProposeContinue)
    } else {
        parse_exec_session_create(&args).map(Act::ExecSessionCreate)
    }
}

/// `exec_session_create` / `exec_session_propose_continue:S3` — the decision
/// label for the log.
fn call_label(call: &ToolCall) -> String {
    let raw: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
    match raw.get("target").and_then(Value::as_str).map(str::trim) {
        Some(target) if !target.is_empty() => format!("{}:{}", call.function.name, target),
        _ => call.function.name.to_string(),
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
    let rest = conversation_id.strip_prefix(acowork_core::intent::TASK_CONVERSATION_PREFIX)?;
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
/// text alone.
///
/// ADR-106: no receipt row is written for the degradation. The fallback route
/// runs a round, and that round's reference row is the record the human needs;
/// a `system_notice` announcing "the router could not decide" is a progress
/// fact about a component, not a channel fact, and it used to key off the
/// retired `rf:` prefix.
async fn degrade(
    deps: &RouterDeps,
    envelope: &IntentEnvelope,
    chat_id: &str,
    reason: String,
    rows: &[ChatRow],
) -> Outcome {
    tracing::warn!(
        chat_id = %chat_id,
        message_id = %envelope.message_id,
        reason = %reason,
        "chat router degraded to the deterministic route"
    );
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
    /// Tokens the projected history cost — the number the configured budget
    /// (`chat_router.history_max_tokens`) is enforced against.
    history_tokens: usize,
    /// Attachments on the turn being routed — the count, not the metadata:
    /// the log holds routing metadata only (ADR-096). Zero was the invisible
    /// state: a turn carrying a file routed exactly like the same text with
    /// nothing attached, and no line in the log could show which had happened.
    attachments: usize,
    /// Workspace references on the turn — see [`DecisionRecord::refs`].
    refs: usize,
    /// The turn had attachments and no text (see
    /// [`context::is_attachment_only`]), i.e. Block C rendered the placeholder.
    attachment_only: bool,
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
        usage: Option<(u64, u64, u64, u64)>,
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
            history_tokens: self.history_tokens,
            attachments: self.attachments,
            refs: self.refs,
            attachment_only: self.attachment_only,
            info_rounds: (self.info_rounds > 0).then_some(self.info_rounds),
            prompt_tokens: usage.map(|u| u.0),
            completion_tokens: usage.map(|u| u.1),
            cache_read_tokens: usage.map(|u| u.2),
            cache_write_tokens: usage.map(|u| u.3),
        };
        tracing::info!("{}", record.summary());
        if let Err(e) = append_decision(std::path::Path::new(&self.deps.work_dir), &record) {
            tracing::debug!(error = %e, "chat router: decision log not written");
        }
    }
}

/// What the execution session actually receives for a dispatched turn.
///
/// Two things, and they are never the same string (ADR-107):
///
/// - **the message** — the human's own words, verbatim. This is what the
///   session persists as its `user` row, and therefore what the auto-title is
///   written from (ADR-046). It must never be re-authored here;
/// - **the background** — the router's framing, delivered as a reference part
///   that follows the words in the model-facing message and lands beside them
///   as a `context_ref` row on disk.
///
/// The background is attached only when this turn physically opened the
/// session. A **continued** session already has its own history — a compaction
/// included, since a compacted session keeps its summary and its tail and
/// continues in place (ADR-032/ADR-061) — so it receives the user's words
/// alone; anything it genuinely lacks it pulls with `chat_history_read`.
///
/// `opened_new` is the routing-time authority for "this turn landed in a
/// physically new session", which is why the choice is made here rather than
/// from `Target`: `Target::Existing` against a session that is gone from disk
/// also creates one, and that one still needs the background.
///
/// History: the words originally vanished entirely — the brief was the whole
/// message. Prepending them (the brief, a blank line, `[user message]`, then
/// the words) fixed that, but made every dispatched session's first `user` row
/// — and so every auto-title — open with the router's fixed sentence, which is
/// what ADR-107 removes by splitting the two into parts.
fn dispatch_parts(
    opened_new: bool,
    background: Option<&str>,
    user_text: &str,
    referenced_rounds: &[String],
) -> (String, Vec<acowork_core::MessageRef>) {
    let content = user_text.to_string();
    let background = match opened_new {
        true => background.map(str::trim).filter(|b| !b.is_empty()),
        false => None,
    };
    // A background that only restates the words adds nothing the session does
    // not already have as its message: the router doing that was common while
    // the schema still asked for a self-contained brief.
    let background = background.filter(|b| b.trim() != content.trim());
    let mut refs = background
        .map(|b| vec![acowork_core::MessageRef::chat_background(b)])
        .unwrap_or_default();
    // ADR-106 revision 2026-11-04: the quoted rounds are the human's own
    // material, not the router's framing — they travel whether or not the
    // session is new, and the same rendered bodies Block C showed (one
    // producer, same clamps).
    for body in referenced_rounds {
        let part = acowork_core::MessageRef::referenced_round(body.trim().to_string());
        if !part.is_empty() {
            refs.push(part);
        }
    }
    (content, refs)
}

// The `SessionStatus` → `CandidateStatus` vocabulary table lives in
// `chat_candidates` (ADR-099): `SessionManager::resolve_candidates` needs the
// same mapping, and the session layer must not reach up into the router layer
// for it.

#[cfg(test)]
mod tests {
    use super::super::tool::{ExecSessionCreateArgs, ProposeContinueArgs};
    use super::*;
    use crate::agent::session::chat_candidates::{CandidateOrigin, CandidateStatus};
    use acowork_core::providers::mock::{MockProvider, MockResponse};

    #[tokio::test]
    async fn call_provider_strips_inline_think_tags() {
        // Chat-surface leak (2026-10-24): a compat endpoint inlined its think
        // block into `content` on the non-streaming path; the router posted it
        // verbatim to the chat. call_provider must sanitize every response.
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
        let response = call_provider(&provider, request, 5_000).await.unwrap();
        assert_eq!(response.content, "现在是 19:37。");
    }

    /// Counts what the turn books, so "one successful model call = one ledger
    /// record" is asserted instead of re-derived by reading the call sites.
    #[derive(Default)]
    struct CountingChatUsage {
        records: std::sync::Mutex<Vec<String>>,
    }

    impl crate::usecases::chat_usage::ChatUsageService for CountingChatUsage {
        fn record_chat_llm_usage(
            &self,
            chat_session_id: &str,
            chat_kind: &str,
            _usage: &acowork_core::providers::traits::UsageInfo,
            provider_label: &str,
            model_id: &str,
            _history_tokens: Option<u64>,
        ) {
            self.records.lock().unwrap().push(format!(
                "{chat_session_id}|{chat_kind}|{provider_label}|{model_id}"
            ));
        }

        fn forget_chat(&self, _chat_session_id: &str) -> bool {
            false
        }

        fn get_chat(&self, _chat_session_id: &str) -> Option<acowork_sqlite::ChatUsageRow> {
            None
        }

        fn ledger_totals(&self) -> acowork_core::session_meta::SessionTotals {
            Default::default()
        }
    }

    #[tokio::test]
    async fn every_successful_model_call_is_booked_once_per_call() {
        // Design 29: a turn is 1–3 model calls and every one of them is
        // billable. Booking lives in `ModelCalls::call`, so the count must
        // track the calls — not the turn, and not the final response only.
        let ledger = Arc::new(CountingChatUsage::default());
        let mut d = deps();
        d.chat_usage = ledger.clone();
        let ok: Arc<dyn Provider> = Arc::new(MockProvider::with_usage("ok", 120, 8));
        let request = || ChatRequest {
            model: "mock-model".into(),
            messages: vec![],
            temperature: None,
            max_tokens: None,
            tools: None,
            reasoning_effort: None,
            thinking_mode: None,
        };
        fn calls<'a>(provider: &'a Arc<dyn Provider>, d: &'a RouterDeps) -> ModelCalls<'a> {
            ModelCalls {
                deps: d,
                chat_id: "cs-1",
                chat_kind: "group",
                provider,
                provider_label: "mock|mock-model",
                model: "mock-model",
                timeout_ms: 5_000,
                history_tokens: None,
            }
        }

        calls(&ok, &d).call(request()).await.unwrap();
        calls(&ok, &d).call(request()).await.unwrap();
        assert_eq!(
            ledger.records.lock().unwrap().as_slice(),
            [
                "cs-1|group|mock|mock-model|mock-model".to_string(),
                "cs-1|group|mock|mock-model|mock-model".to_string()
            ]
        );

        // A failed call is not spend: it must not add a record.
        let failing: Arc<dyn Provider> = Arc::new(MockProvider::new(vec![MockResponse::Error {
            message: "boom".into(),
        }]));
        assert!(calls(&failing, &d).call(request()).await.is_err());
        assert_eq!(ledger.records.lock().unwrap().len(), 2);
    }

    /// Params carrying (or not carrying) an explicit session address.
    fn addressed_params(session: Option<&str>) -> Value {
        let mut params = serde_json::json!({ "message": "go on" });
        if let Some(session) = session {
            params.as_object_mut().unwrap().insert(
                acowork_core::protocol::TARGET_SESSION_PARAM.to_string(),
                serde_json::json!(session),
            );
        }
        params
    }

    /// ADR-106 D6 level 1: the address is read off the params the Gateway wrote.
    /// An absent, blank or non-string value all read as "the human addressed
    /// nothing", so the turn reaches the router instead of being delivered at an
    /// empty address — the two failures must not look alike later.
    #[test]
    fn an_addressed_turn_reads_the_session_and_a_blank_one_does_not() {
        assert_eq!(
            addressed_session(&envelope("user_message", "cs-1", addressed_params(Some("s-9"))))
                .as_deref(),
            Some("s-9")
        );
        // Surrounding whitespace is not part of an id, but the intent is real.
        assert_eq!(
            addressed_session(&envelope(
                "user_message",
                "cs-1",
                addressed_params(Some("  s-9  "))
            ))
            .as_deref(),
            Some("s-9")
        );

        for session in [None, Some(""), Some("   ")] {
            assert_eq!(
                addressed_session(&envelope("user_message", "cs-1", addressed_params(session)))
                    .as_deref(),
                None,
                "{session:?} must not read as addressed"
            );
        }
        // A wrong-typed value is a half-written key, not an address.
        for junk in [serde_json::json!(null), serde_json::json!(7)] {
            let mut params = serde_json::json!({ "message": "go on" });
            params
                .as_object_mut()
                .unwrap()
                .insert(acowork_core::protocol::TARGET_SESSION_PARAM.to_string(), junk);
            assert_eq!(
                addressed_session(&envelope("user_message", "cs-1", params)).as_deref(),
                None
            );
        }
    }

    /// The two flags gate a deterministic rule and a refusal, so an absent key,
    /// a `false` and a wrong-typed value must all read as "no special handling" —
    /// and one of them being true must not imply the other is.
    #[test]
    fn the_addressing_flags_are_independent_and_default_to_false() {
        let bare = envelope("user_message", "cs-1", addressed_params(None));
        assert!(!explicit_mention(&bare));
        assert!(!force_new(&bare));

        let mut params = addressed_params(None);
        params.as_object_mut().unwrap().insert(
            acowork_core::protocol::EXPLICIT_MENTION_PARAM.to_string(),
            serde_json::json!(true),
        );
        let mentioned = envelope("user_message", "cs-1", params);
        assert!(explicit_mention(&mentioned));
        assert!(!force_new(&mentioned), "an @ is not a request for new work");

        let mut params = addressed_params(None);
        params
            .as_object_mut()
            .unwrap()
            .insert(acowork_core::protocol::FORCE_NEW_PARAM.to_string(), serde_json::json!("yes"));
        assert!(!force_new(&envelope("user_message", "cs-1", params)));
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

    /// End to end across the wire, in one test: the params object the Gateway
    /// writes, read by the router, rendered into Block C.
    ///
    /// The two halves are tested separately (`intent::chat_client` pins the
    /// projection, the context tests pin the rendering), which still leaves one
    /// thing unguarded — the KEY. A Gateway writing `chatAttachments` and a
    /// Runtime reading `chat_attachments` would pass both suites and lose every
    /// attachment in production. So the literal below is the Gateway's params
    /// object as `intent_api` writes it (same key, same nesting, same field
    /// names from the user service), and the assertion is the line the model
    /// would see.
    #[test]
    fn the_gateways_delivery_params_reach_the_router_prompt() {
        // Same keys the Gateway's `chat_delivery_params` writes. The
        // attachment key is inserted through the shared constant rather than
        // spelled out here: `json!` needs a literal key, and a second literal
        // is exactly the drift this test is supposed to catch (the Gateway
        // half of the contract is pinned by
        // `http::intent_api::chat_params_tests`).
        let mut params = serde_json::json!({
            "message": "look at this",
            "chat_attachment_ids": ["att-1"],
            "chat_kind": "user_agent",
            "chat_prompt": "",
        });
        params[acowork_core::protocol::CHAT_ATTACHMENTS_PARAM] = serde_json::json!([
            {"filename": "report.pdf", "mime": "application/pdf", "size": 12288}
        ]);
        let turn = envelope("user_message", "cs-1", params);

        assert_eq!(chat_turn_text(&turn).as_deref(), Some("look at this"));
        let attachments = turn_attachments(&turn);
        assert_eq!(attachments.len(), 1);

        let rendered = super::super::context::TurnContext {
            surface: surface_label(&chat_kind_of(&turn)),
            speaker: &turn.from,
            now: "2026-10-21T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "look at this",
            attachments: &attachments,
            refs: &[],
            referenced_rounds: &[],
        }
        .render();
        assert!(
            rendered.contains("look at this
[attachments] report.pdf (application/pdf, 12KB)"),
            "block C must carry the attachment line the Gateway delivered: {rendered}"
        );
    }

    /// Regression (2026-11-06): a workspace reference added with "add to chat"
    /// rides on `attached_items`, never on `chat_attachments` — the Gateway
    /// cannot read a node-local path to describe it. The router read only the
    /// latter, so a turn whose whole content was a reference was routed as an
    /// empty one and answered in the chat with "you sent no content or
    /// attachment" while the UI showed the file. The reference was invisible at
    /// the one layer that decides whether to involve an Agent that could read it.
    #[test]
    fn a_workspace_reference_is_a_turn_the_router_can_see() {
        let turn = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({
                "message": "",
                "attached_items": [{
                    "type": "attached_file",
                    "absPath": "E:/projects/tranxon/core/src/chat_prompt.rs",
                    "name": "chat_prompt.rs",
                }],
            }),
        );
        // No words, no uploaded files, one reference: still a router turn.
        assert_eq!(
            chat_turn_text(&turn).as_deref(),
            Some(""),
            "a reference-only message must reach the router at all"
        );

        let refs = turn_refs(&turn);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "chat_prompt.rs");
        assert_eq!(refs[0].kind, "attached_file");

        let rendered = super::super::context::TurnContext {
            surface: "dm",
            speaker: "user-1",
            now: "2026-11-06T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "",
            attachments: &[],
            refs: &refs,
            referenced_rounds: &[],
        }
        .render();
        assert!(
            rendered.contains("[attached refs] chat_prompt.rs"),
            "Block C must name the file the human pointed at: {rendered}"
        );
        assert!(
            rendered.contains(super::super::context::ATTACHMENT_ONLY_PLACEHOLDER),
            "a reference-only turn is a normal turn, not a blank one: {rendered}"
        );
        // The path is the part that must NOT cross: it is node-local layout the
        // chat cannot read, and the prompt is written into a durable transcript.
        assert!(
            !rendered.contains("E:/projects") && !rendered.contains("tranxon"),
            "a node path leaked into the router prompt: {rendered}"
        );
    }

    /// The blank-name rule has ONE owner (`AttachmentLabel::from_item`), so the
    /// turn list the router counts is the same set the Gateway wrote into the
    /// row. Two rules would mean a turn whose Block C names an item its own
    /// history row dropped — the "reads identically" invariant, broken by the
    /// projection rather than by the reader.
    #[test]
    fn an_unnamable_reference_is_dropped_from_the_turn_like_it_is_from_the_row() {
        let turn = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({
                "message": "",
                "attached_items": [
                    { "type": "attached_file", "absPath": "", "name": "" },
                    { "type": "file_upload", "documentId": "ab12-1", "filename": "", "format": "pdf", "sizeBytes": 1 },
                    { "type": "attached_file", "absPath": "E:/p/a.rs", "name": "a.rs" },
                ],
            }),
        );
        let refs = turn_refs(&turn);
        assert_eq!(
            refs.len(),
            1,
            "only the named item survives, as it does in the row label"
        );
        assert_eq!(refs[0].filename, "a.rs");
    }

    /// A selection reference keeps its line range — it is the one part of the
    /// reference a chat may show without being able to open anything, and it
    /// changes what the router should infer about the ask.
    #[test]
    fn a_selection_reference_is_named_with_its_range() {
        let turn = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({
                "message": "这段还在用吗",
                "attached_items": [{
                    "type": "attached_selection",
                    "absPath": "/srv/app/src/lib.rs",
                    "name": "src/lib.rs",
                    "startLine": 12,
                    "endLine": 25,
                }],
            }),
        );
        let refs = turn_refs(&turn);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].filename, "src/lib.rs");
        assert_eq!(refs[0].start_line, Some(12));
        assert_eq!(refs[0].end_line, Some(25));
    }

    /// Uploads and references are two arms of one turn and both belong in the
    /// prompt — the line the human sees at the head of a round is the line the
    /// router reasons over.
    #[test]
    fn uploads_and_references_render_together() {
        let turn = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({
                "message": "对比一下",
                acowork_core::protocol::CHAT_ATTACHMENTS_PARAM: [
                    {"filename": "spec.pdf", "mime": "application/pdf", "size": 12345}
                ],
                "attached_items": [{
                    "type": "attached_file",
                    "absPath": "/srv/app/docs/old-spec.pdf",
                    "name": "old-spec.pdf",
                }],
            }),
        );
        let uploads = turn_attachments(&turn);
        let refs = turn_refs(&turn);
        assert_eq!(uploads.len(), 1);
        assert_eq!(refs.len(), 1);
        let rendered = super::super::context::TurnContext {
            surface: "dm",
            speaker: "user-1",
            now: "2026-11-06T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "对比一下",
            attachments: &uploads,
            refs: &refs,
            referenced_rounds: &[],
        }
        .render();
        assert!(rendered.contains("[attachments] spec.pdf (application/pdf, 12KB)"));
        assert!(rendered.contains("[attached refs] old-spec.pdf"));
    }

    /// The Gateway only started writing `chat_attachments` in this change, so
    /// a half-deployed Gateway meets a new Runtime. The reader gives up the
    /// files before it gives up the turn: every malformed shape reads as "no
    /// attachments", never as an error and never as a skipped message.
    #[test]
    fn a_malformed_attachment_payload_costs_the_files_not_the_turn() {
        let turn = |params: Value| envelope("user_message", "cs-1", params);

        // Absent (older Gateway) and explicit null.
        assert!(turn_attachments(&turn(serde_json::json!({"message": "x"}))).is_empty());
        assert!(
            turn_attachments(&turn(
                serde_json::json!({"message": "x", "chat_attachments": null})
            ))
            .is_empty()
        );
        // Wrong JSON type, wrong container, wrong element shape.
        assert!(
            turn_attachments(&turn(serde_json::json!({"chat_attachments": "att-1"}))).is_empty()
        );
        assert!(
            turn_attachments(&turn(
                serde_json::json!({"chat_attachments": {"filename": "a.pdf"}})
            ))
            .is_empty()
        );
        // Wrong element shape: per-field defaults would otherwise parse this
        // into a nameless entry — nothing worth telling the model.
        assert!(
            turn_attachments(&turn(
                serde_json::json!({"chat_attachments": [{"name": "a.pdf"}]})
            ))
            .is_empty()
        );
        // A partially-described file is still a file.
        assert_eq!(
            turn_attachments(&turn(
                serde_json::json!({"chat_attachments": [{"filename": "a.pdf"}, {"name": "b.pdf"}]})
            ))
            .len(),
            1
        );
        // Present but empty is the ordinary text-only turn.
        assert!(turn_attachments(&turn(serde_json::json!({"chat_attachments": []}))).is_empty());
        // A list of empty objects is "no files" here even though the raw array
        // is non-empty. Everything that asks "did this turn carry files?" —
        // Block C, the decision log, the degraded route's brief — goes through
        // this reader, so there is exactly one answer.
        assert!(
            turn_attachments(&turn(serde_json::json!({"chat_attachments": [{}, {"size": 5}]})))
                .is_empty()
        );

        // Well-formed: order preserved, nothing dropped.
        let got = turn_attachments(&turn(serde_json::json!({"chat_attachments": [
            {"filename": "report.pdf", "mime": "application/pdf", "size": 12288},
            {"filename": "shot.png", "mime": "image/png", "size": 348160},
        ]})));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].filename, "report.pdf");
        assert_eq!(got[0].size, 12_288);
        assert_eq!(got[1].mime, "image/png");
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
    fn act_arguments_are_parsed_from_the_tool_call() {
        let aliases = AliasTable::from([("S1".to_string(), "sess-1".to_string())]);
        let proposal = ToolCall {
            id: "call-1".into(),
            call_type: "function".into(),
            function: acowork_core::providers::traits::FunctionCall {
                name: super::super::tool::EXEC_SESSION_PROPOSE_CONTINUE_TOOL.into(),
                arguments: r#"{"target":"S1","why":"same login bug"}"# .into(),
            },
        };
        assert_eq!(
            parse_act(&proposal, &aliases).unwrap(),
            Act::ProposeContinue(ProposeContinueArgs {
                alias: "S1".into(),
                session_id: "sess-1".into(),
                why: "same login bug".into(),
            })
        );
        assert_eq!(
            call_label(&proposal),
            "exec_session_propose_continue:S1"
        );

        let task = ToolCall {
            function: acowork_core::providers::traits::FunctionCall {
                name: super::super::tool::EXEC_SESSION_CREATE_TOOL.into(),
                arguments: r#"{"background":"what was decided"}"# .into(),
            },
            ..proposal
        };
        assert_eq!(
            parse_act(&task, &aliases).unwrap(),
            Act::ExecSessionCreate(ExecSessionCreateArgs {
                background: "what was decided".into(),
                seed: None,
            })
        );
        // `exec_session_create` addresses nothing, so its label carries no
        // target.
        assert_eq!(call_label(&task), "exec_session_create");
    }

    #[test]
    fn a_malformed_arguments_string_is_a_validation_error_not_a_crash() {
        let call = ToolCall {
            id: "c".into(),
            call_type: "function".into(),
            function: acowork_core::providers::traits::FunctionCall {
                name: super::super::tool::EXEC_SESSION_CREATE_TOOL.into(),
                arguments: "{not json".into(),
            },
        };
        assert_eq!(parse_act(&call, &AliasTable::new()), Err(ActError::MissingBackground));
        assert_eq!(call_label(&call), "exec_session_create");
    }

    /// Fixtures for the read tool: a transcript whose rows carry their round
    /// stamp plus its side table, exactly as the runtime writer leaves them.
    fn round_entry(role: &str, content: &str, round_id: &str) -> crate::conversation::ConversationEntry {
        crate::conversation::ConversationEntry {
            id: format!("{role}-{content}"),
            ts: "2026-11-03T00:00:00.000Z".to_string(),
            role: role.to_string(),
            content: content.to_string(),
            metadata: Some(serde_json::json!({ crate::rounds::META_KEY_ROUND_ID: round_id })),
            kind: None,
        }
    }

    fn write_fixture_transcript(dir: &std::path::Path, sid: &str, entries: &[crate::conversation::ConversationEntry]) {
        let lines: Vec<String> = entries.iter().map(|e| serde_json::to_string(e).unwrap()).collect();
        std::fs::write(dir.join(format!("{sid}.jsonl")), lines.join("\n") + "\n").unwrap();
    }

    fn fixture_record(round_id: &str, user: u64, answer: Option<u64>, ts: &str) -> crate::rounds::RoundRecord {
        crate::rounds::RoundRecord {
            round_id: round_id.to_string(),
            user: Some(user),
            answer,
            card: None,
            terminal: true,
            status: Some("completed".to_string()),
            ts: ts.to_string(),
        }
    }

    /// The happy read: the newest asked rounds, oldest first, each with its
    /// question, answer and terminal state — the facts the router is allowed
    /// to report.
    #[test]
    fn read_round_result_renders_the_newest_rounds_in_order() {
        let dir = tempfile::TempDir::new().unwrap();
        let conv = dir.path();
        write_fixture_transcript(
            conv,
            "sess-1",
            &[
                round_entry("user", "first ask", "r:1"),
                round_entry("assistant", "first answer", "r:1"),
                round_entry("user", "second ask", "r:2"),
                round_entry("assistant", "second answer", "r:2"),
            ],
        );
        let rounds = crate::rounds::rounds_path(conv, "sess-1");
        crate::rounds::append_record(&rounds, &fixture_record("r:1", 0, Some(1), "2026-11-03T00:00:01.000Z")).unwrap();
        crate::rounds::append_record(&rounds, &fixture_record("r:2", 2, Some(3), "2026-11-03T00:00:02.000Z")).unwrap();

        let all = read_round_result(conv, "sess-1", "S1", 3);
        assert!(all.contains("second ask"), "{all}");
        assert!(all.contains("second answer"), "{all}");
        assert!(all.contains("first ask"), "{all}");
        assert!(all.contains("completed"), "{all}");
        // Chronological: the older round is rendered first.
        assert!(
            all.find("first ask").unwrap() < all.find("second ask").unwrap(),
            "{all}"
        );

        // `rounds` bounds the read to the newest N.
        let one = read_round_result(conv, "sess-1", "S1", 1);
        assert!(one.contains("second ask"), "{one}");
        assert!(!one.contains("first ask"), "{one}");
    }

    /// A session with no round on disk is an explicit "not yet" — the queued
    /// dispatch reads as queued, never as a failure and never as a guess.
    #[test]
    fn read_round_result_states_an_explicit_hole_when_nothing_is_recorded() {
        let dir = tempfile::TempDir::new().unwrap();
        let text = read_round_result(dir.path(), "sess-9", "S3", 1);
        assert!(text.contains("S3"), "{text}");
        assert!(text.contains("no recorded round yet"), "{text}");
    }

    /// A side-table coordinate the transcript can no longer vouch for renders
    /// as the hole it is — the read must never serve whatever sits at the
    /// position (ADR-106 D4/D5).
    #[test]
    fn read_round_result_reports_unprojectable_rounds_as_holes() {
        let dir = tempfile::TempDir::new().unwrap();
        let conv = dir.path();
        write_fixture_transcript(conv, "sess-2", &[round_entry("user", "only row", "r:1")]);
        let rounds = crate::rounds::rounds_path(conv, "sess-2");
        crate::rounds::append_record(&rounds, &fixture_record("r:1", 40, None, "2026-11-03T00:00:01.000Z")).unwrap();

        let text = read_round_result(conv, "sess-2", "S1", 1);
        assert!(text.contains("content unavailable"), "{text}");
        assert!(!text.contains("only row"), "a drifted coordinate must not be served: {text}");
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
        };
        let line = CandidateLine {
            alias: "S1".into(),
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
            attachments: &[],
            refs: &[],
            referenced_rounds: &[],
        }
        .render();
        assert!(rendered.contains("S1"));
        assert!(!rendered.contains("sess-secret"));
    }

    #[test]
    fn a_continued_session_receives_the_users_own_words_and_no_background() {
        // Regression (2026-10-24): the router appended a context brief when
        // dispatching to an existing session, and it replaced the user's
        // message — the execution session's persisted `user` row and its
        // auto-title then held the router's paraphrase, and the brief restated
        // history that session already has.
        let (content, refs) = dispatch_parts(
            false,
            Some("上下文（同一 session 内刚完成的工作）：1. …"),
            "把本地修改的代码提交一下。",
            &[],
        );
        assert_eq!(content, "把本地修改的代码提交一下。");
        assert!(
            refs.is_empty(),
            "a session with its own history needs no re-briefing"
        );
    }

    #[test]
    fn a_fresh_session_gets_the_words_as_the_message_and_the_background_as_a_ref() {
        // ADR-107. The message is the human's, byte for byte — this is what the
        // session persists and what its auto-title is written from. The
        // background travels as a reference part, which the model reads AFTER
        // the words and the transcript shows beside them.
        let (content, refs) = dispatch_parts(
            true,
            Some("Commit the batch described below: …"),
            "raw",
            &[],
        );
        assert_eq!(content, "raw", "the human's words are the message");
        assert_eq!(
            refs,
            vec![acowork_core::MessageRef::chat_background(
                "Commit the batch described below: …"
            )],
            "the router's framing is a reference part, never the message"
        );

        // Without a background (e.g. an attachment-only turn) the words still
        // land, alone.
        for absent in [None, Some("   ")] {
            let (content, refs) = dispatch_parts(true, absent, "raw", &[]);
            assert_eq!(content, "raw");
            assert!(refs.is_empty());
        }

        // A router that echoed the message verbatim must not ship a reference
        // that says exactly what the message already says.
        let (content, refs) = dispatch_parts(true, Some("  提交一下 "), "  提交一下 
", &[]);
        assert_eq!(content, "  提交一下 
", "the message is never rewritten");
        assert!(
            refs.is_empty(),
            "a background identical to the message adds nothing"
        );
    }

    /// ADR-106 revision 2026-11-04: the single reader of `chat_refs` — the
    /// quadruple the Gateway pinned before the row landed. Tolerant like
    /// `turn_attachments`: a degenerate wire costs the material, never the
    /// turn.
    #[test]
    fn pinned_chat_refs_reads_the_quadruples_and_survives_degenerate_wires() {
        let turn = envelope(
            "user_message",
            "cs-1",
            serde_json::json!({
                "message": "汇总一下",
                acowork_core::protocol::CHAT_REFS_PARAM: [{
                    "agent_instance_id": "i-1",
                    "session_id": "sess-a",
                    "round_id": "r:m-1",
                    "terminal": true,
                }],
            }),
        );
        let got = pinned_chat_refs(&turn);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].session_id, "sess-a");
        assert_eq!(got[0].round_id, "r:m-1");
        assert!(got[0].terminal, "the pin's terminal fact must survive the wire");

        let at = |v: Value| {
            pinned_chat_refs(&envelope(
                "user_message",
                "cs-1",
                serde_json::json!({
                    "message": "x",
                    acowork_core::protocol::CHAT_REFS_PARAM: v,
                }),
            ))
        };
        // Wrong shape, wrong type, an unpinned quadruple, a blank id: all read
        // as "no material" — never an error, never a dropped message.
        assert!(at(serde_json::json!([])).is_empty());
        assert!(at(serde_json::json!("nope")).is_empty());
        assert!(at(serde_json::json!([{"session_id": "s"}])).is_empty());
        assert!(at(serde_json::json!([{
            "agent_instance_id": "i", "session_id": " ", "round_id": "r", "terminal": true
        }]))
        .is_empty());
        // `terminal` absent defaults false; the material still travels.
        let part = at(serde_json::json!([
            {"agent_instance_id": "i", "session_id": "s", "round_id": "r"}
        ]));
        assert_eq!(part.len(), 1);
        assert!(!part[0].terminal);
    }

    /// Block C's material and the delivered parts come from this one renderer:
    /// the `read_round_result` line shape with the source coordinates, the
    /// pin's non-terminal mark, explicit holes, and the segment budget that
    /// collapses the tail instead of silently dropping it.
    #[test]
    fn render_pinned_rounds_paints_lines_holes_and_the_budget_collapse() {
        let dir = tempfile::TempDir::new().unwrap();
        let conv = dir.path();
        write_fixture_transcript(
            conv,
            "sess-a",
            &[
                round_entry("user", "ask A", "r:1"),
                round_entry("assistant", &format!("answer A {}", "x".repeat(4000)), "r:1"),
            ],
        );
        crate::rounds::append_record(
            &crate::rounds::rounds_path(conv, "sess-a"),
            &fixture_record("r:1", 0, Some(1), "2026-11-04T00:00:01.000Z"),
        )
        .unwrap();
        write_fixture_transcript(conv, "sess-b", &[round_entry("user", "ask B", "r:2")]);
        let mut running = fixture_record("r:2", 0, None, "2026-11-04T00:00:02.000Z");
        running.terminal = false;
        running.status = Some("working".to_string());
        crate::rounds::append_record(&crate::rounds::rounds_path(conv, "sess-b"), &running)
            .unwrap();

        let pin = |session_id: &str, round_id: &str, terminal: bool| PinnedChatRef {
            agent_instance_id: "i-1".into(),
            session_id: session_id.into(),
            round_id: round_id.into(),
            terminal,
        };
        let bodies = render_pinned_rounds(
            conv,
            &[
                pin("sess-a", "r:1", true),
                pin("sess-b", "r:2", false),
                pin("sess-gone", "r:9", true),
            ],
        );
        assert_eq!(bodies.len(), 3, "every ref renders, holes included");
        // Coordinates first — the parts must name where the material came from.
        assert!(
            bodies[0].starts_with("session sess-a · round r:1 · completed\n"),
            "{bodies:?}"
        );
        assert!(bodies[0].contains("question: ask A"), "{bodies:?}");
        // The 4 KB answer arrives clamped like `exec_session_read_round` clamps.
        assert!(bodies[0].contains(super::super::context::TRUNCATION_SUFFIX));
        assert!(bodies[0].len() < 3 * 1024, "per-field clamps bound one entry");
        // A pin taken while the round still ran says so — the material is
        // progress as of send time, and the reader must know.
        assert!(bodies[1].contains("(was still running)"), "{bodies:?}");
        assert!(bodies[1].contains("answer: (none yet)"), "{bodies:?}");
        // Unreadable is an explicit hole, never a swallowed ref.
        assert!(bodies[2].contains("no such round on this host"), "{bodies:?}");

        // The segment budget: refs past the cap collapse into one explicit
        // dropped line, in ref order — the reader learns the material was cut.
        let many: Vec<PinnedChatRef> = (0..6).map(|_| pin("sess-a", "r:1", true)).collect();
        let capped = render_pinned_rounds(conv, &many);
        assert!(capped.len() < many.len(), "the budget must trip: {capped:?}");
        assert_eq!(
            capped.last().expect("a collapsed tail").as_str(),
            "dropped: reference budget exhausted",
        );
        assert!(
            capped[..capped.len() - 1]
                .iter()
                .all(|b| b.contains("ask A")),
            "refs within the budget keep their bodies: {capped:?}",
        );
    }

    /// ADR-106 revision 2026-11-04: the quoted material is the human's, not
    /// the router's framing — it travels on both dispatch arms, as
    /// `referenced_round` parts (ADR-107), never folded into the message.
    #[test]
    fn material_travels_whether_or_not_the_session_is_new() {
        let material = vec![
            "session sess-a · round r:1 · completed\nquestion: q\n".to_string(),
            "   ".to_string(),
        ];
        let (content, refs) = dispatch_parts(true, Some("bg"), "words", &material);
        assert_eq!(content, "words", "the human's words stay the message");
        assert_eq!(refs.len(), 2, "the blank body never ships: no header without a block");
        assert_eq!(refs[0], acowork_core::MessageRef::chat_background("bg"));
        assert_eq!(
            refs[1],
            acowork_core::MessageRef::referenced_round(
                "session sess-a · round r:1 · completed\nquestion: q"
            ),
        );

        // A continued session gets no re-briefing — but the material is not a
        // briefing: it travels anyway.
        let (_, refs) = dispatch_parts(false, Some("bg"), "words", &material[..1]);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].kind, acowork_core::REF_KIND_REFERENCED_ROUND);
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
            chat_usage: Arc::new(crate::usecases::chat_usage_impl::NoopChatUsageService),
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

    /// A truncated response is not a decision. The production failure this
    /// pins: an `exec_session_create` brief ran past `max_output_tokens`, the provider cut
    /// the tool-call JSON mid-way, `act_call()` found no act, and the partial
    /// prose was published to the chat as the router's answer — the user saw an
    /// agent message and no execution session was ever created.
    #[test]
    fn an_output_budget_exhaustion_is_detected_in_both_vocabularies() {
        let with = |reason: Option<&str>| ChatResponse {
            finish_reason: reason.map(str::to_string),
            ..Default::default()
        };
        // Anthropic reports `max_tokens`; the adapter normalises it to `length`.
        // A future adapter may pass the vendor string through unchanged, so both
        // spellings must read as truncated.
        assert!(is_truncated(&with(Some("length"))));
        assert!(is_truncated(&with(Some("max_tokens"))));
        assert!(!is_truncated(&with(Some("stop"))));
        assert!(!is_truncated(&with(Some("tool_calls"))));
        // A missing finish_reason is not evidence of truncation.
        assert!(!is_truncated(&with(None)));
    }

    /// The router has no textual tool-call parser by design — an act carries a
    /// delivery decision, and none can be inferred from prose. Recognising the
    /// envelope is still required, or the raw markup is published to the human
    /// as the agent's answer.
    #[test]
    fn a_tool_call_written_as_text_is_recognised_and_withheld() {
        assert!(looks_like_tool_call_envelope(
            "<tool_call>\n<invoke name=\"exec_session_create\"><message>do it"
        ));
        // Some endpoints splice a zero-width space into the opening tag; the
        // `tool_call` marker survives on the other side of that seam.
        assert!(looks_like_tool_call_envelope("<\u{200b}tool_call>"));
        assert!(looks_like_tool_call_envelope("<INVOKE name=\"x\">"));
        assert!(!looks_like_tool_call_envelope("Sure - I will start that."));
        assert!(!looks_like_tool_call_envelope(
            "I can look that up if you want."
        ));
    }
}


