//! Intent turn support (design doc 27 §5.4.5, §7.1).
//!
//! A turn triggered by an inbound Intent `request` puts the agent in the
//! **responder** role: `intent_send` / `ask_user_question` are hidden from
//! the tool set (R1), human-awaiting pause states are refused (R3), and the
//! turn's final output is written back to the Gateway as a `response` by
//! the *system* — never by a model tool call (§7.1).
//!
//! This module owns the pieces that are not loop mechanics: the context
//! struct, the user-visible rendering of an envelope, and the Gateway
//! write-back call.

use std::time::Duration;

use acowork_core::defaults::GATEWAY_HTTP_PORT;

use super::inbound::IntentEnvelope;
use serde_json::Value;

/// Bound on the best-effort Gateway writes. All three calls below are
/// contractually fire-and-forget — a lost row degrades a surface, it never
/// loses the conversation — so an unbounded hang is strictly worse than a
/// logged failure: the Chat Router's write-back runs on a per-chat drain task,
/// and a stalled Gateway would otherwise park that chat's queue (and its
/// anchor) forever. Generous for a localhost hop, finite for a hung one.
const GATEWAY_WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Same bound for an attachment pull, scaled up for bytes over the loopback
/// relay rather than a JSON row.
const GATEWAY_PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// The Chat surface a turn owes its visible behaviour to (ADR-099 §2.2
/// generalised; ADR-100 D12).
///
/// Every obligation a turn has toward a Chat session — where an interactive
/// card is written, who may answer it, whether the turn may block on a human
/// at all, which Chat row the terminal echo belongs to — reads these facts
/// and nothing else. A turn carries exactly one, produced at the transport
/// boundary from the envelope fields the Gateway already stamped.
///
/// This struct exists because its absence was a root cause, not a symptom:
/// the same facts used to be re-derived ad hoc per consumer (`chat_writeback`
/// standing in for "is there a chat", `echo_chat_id` re-reading the session
/// binding, `has_human` copied into a separate field), so any transport that
/// dropped one of them silently turned a consumer's "no chat here" branch
/// into real behaviour — a card that never appears, a pause that aborts
/// instead of waiting. A consumer may not derive these facts from anything
/// else; a new transport or dispatch scenario produces a `TurnSurface` (or
/// states explicitly why it has no chat obligations).
#[derive(Debug, Clone, Default)]
pub struct TurnSurface {
    /// The Chat session this turn is accountable to. Empty = none.
    pub chat_session_id: String,
    /// The Chat row that triggered the turn — an interactive card's
    /// `in_reply_to` and the coordinate an answer resolves against (§5.5.4).
    /// Empty = the turn has no row to hang a card on.
    pub trigger_message_id: String,
    /// Who may answer this turn's cards: the sender of the triggering row.
    /// `None` = the transport carried no sender, so a card would be
    /// unanswerable — it is not written rather than left as a dead button.
    pub answerable_by: Option<String>,
    /// Whether the accountable chat has a human participant, as derived by
    /// the Gateway from the session it bound to (never from the sender).
    /// `None` = unknown → fail open (interactive tools stay available, no
    /// card is written). `Some(false)` = agent-to-agent: a turn must not
    /// block on a human (the §2.2 availability gate, now transport-agnostic).
    pub has_human: Option<bool>,
}

impl TurnSurface {
    /// The single producer: turn-to-chat accountability is decided here,
    /// from the envelope, for every transport (Chat write endpoint, pinned
    /// turn, Intent delivery). A turn bound to a Chat with a known sender
    /// owes its cards and its terminal echo to that Chat; the kind of the
    /// turn does not change that — it changes only whether the turn must
    /// *reply* to another agent, which is [`IntentTurnContext`]'s business.
    pub fn from_envelope(env: &IntentEnvelope) -> Self {
        Self::new(
            env.source_chat_session_id.clone(),
            env.message_id.clone(),
            &env.from,
            env.has_human,
        )
    }

    /// The field-level form of [`Self::from_envelope`], for a caller that has
    /// already destructured the envelope (the deterministic Intent route).
    /// Same rule, one place: a Chat-bound turn with a known sender owes that
    /// Chat its cards and its terminal echo.
    pub fn new(
        chat_session_id: String,
        trigger_message_id: String,
        from: &str,
        has_human: Option<bool>,
    ) -> Self {
        Self {
            chat_session_id,
            trigger_message_id,
            answerable_by: (!from.is_empty()).then(|| from.to_string()),
            has_human,
        }
    }

    /// Whether this turn may write an interactive card, and where: the
    /// `(chat_session_id, trigger_message_id, answerable_by)` triple §5.5.4
    /// requires. `None` when any coordinate is missing.
    pub fn card_target(&self) -> Option<(&str, &str, &str)> {
        if self.chat_session_id.is_empty() || self.trigger_message_id.is_empty() {
            return None;
        }
        self.answerable_by
            .as_deref()
            .map(|a| (self.chat_session_id.as_str(), self.trigger_message_id.as_str(), a))
    }

    /// The Chat this turn's terminal row is echoed to, if any.
    pub fn echo_chat(&self) -> Option<&str> {
        (!self.chat_session_id.is_empty()).then_some(self.chat_session_id.as_str())
    }
}

/// How a turn ended, in the Chat-facing vocabulary: the status of the
/// terminal row (ADR-100 D9) and of the §7.1 write-back.
///
/// One field on the loop carries it and both terminal writers read it. Before
/// it existed the loop held a `turn_aborted` bool for the echo while the
/// write-back context held a parallel `outcome` string mutated from inside
/// the loop — two mirrors of one fact, and the transport that only had the
/// bool echoed a turn paused *waiting for a human* as `aborted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Aborted,
    /// Ended waiting on a human answer (§5.5.4): the open card is the answer
    /// entry and the answer opens the next turn.
    Waiting,
    /// The loop returned `Err` — the terminal row is `failed`.
    Failed,
}

impl TurnOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnOutcome::Completed => "completed",
            TurnOutcome::Aborted => "aborted",
            TurnOutcome::Waiting => "waiting",
            TurnOutcome::Failed => "failed",
        }
    }
}

/// The request a responder turn must answer. Built when a `request`
/// IntentMessage starts a turn; consumed at turn end for the write-back.
///
/// This is the §7.1 **response channel** to the initiator agent — after
/// ADR-099 it means exactly that and nothing more. It used to double as the
/// "is there a Chat" signal for card writing and the human-awaiting gate;
/// those obligations moved to [`TurnSurface`], which every transport carries.
#[derive(Debug, Clone)]
pub struct IntentTurnContext {
    /// The request's stable message id — the write-back coordinate.
    pub request_id: String,
    /// Sender instance (the response is addressed back to it).
    pub from: String,
    pub action: String,
}

impl IntentTurnContext {
    pub fn new(request_id: &str, from: &str, action: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            from: from.to_string(),
            action: action.to_string(),
        }
    }
}

/// Design 27 §5.4.5 / plan D-C/C1: an inbound Intent kind decides **two
/// orthogonal things**, and conflating them is what made 1:1 user-agent
/// chat unusable (a human message judged as a `request` stripped
/// `ask_user_question` from the turn). The decision is a pure lookup in
/// the [`acowork_core::intent::TurnOrigin`] policy table — no inference
/// here, the producer stamped it.
///
/// Returns `(responder_role, needs_write_back)`:
///
/// - `"request"` (and empty = legacy request): the turn must answer an
///   agent-to-agent request — responder role (R1 tool trim, R3 no
///   human-awaiting pause) **and** a system write-back at turn end. The
///   write-back goes through `POST /api/intents/{request_id}/response`
///   even when no Chat session is bound (mailbox-only degradation), so a
///   request always owes a write-back.
/// - `"user_message"`: a human chat message routed through the Intent
///   transport (§5.4.4) — an ordinary initiator turn with the full tool
///   set, but the turn owes its Chat session a terminal row. Needs a
///   `message_id` to answer against; without one there is no coordinate.
/// - `"pm_task"`: a PM task dispatch — also human input (same role and
///   write-back as `user_message`), but routed on the task key, not by
///   the Chat Router.
/// - `"response"`: the answer arriving back at the initiator — an
///   ordinary turn with **no** write-back (the response row is already
///   durable; the Gateway appended it when the answer was posted).
pub fn turn_kind_semantics(kind: &str, message_id: &str) -> (bool, bool) {
    let origin = acowork_core::intent::TurnOrigin::parse(kind);
    (origin.policy().responder, origin.write_back(message_id))
}

/// Extract the PM dispatch's wake-up playbook from an envelope's params.
///
/// `assignment_brief` is a HIGH-TRUST field: it lands in the receiving
/// session's system block (Block A), above everything the model reads.
/// Only the Gateway's task-dispatch pipeline — a *system* producer — may
/// fill it, so the value is honored for [`TurnOrigin::PmTask`] and empty
/// for every other origin. Without this gate any agent holding
/// `intent:send` could smuggle persistent system-level instructions into
/// a peer's trust slot through the arbitrary JSON of an `intent_send`
/// params object (design 27 §5.4.5 trust boundary; the same rule the
/// Gateway enforces on its side by stripping the key from agent-supplied
/// params).
pub fn extract_assignment_brief(kind: &str, params: &Value) -> String {
    brief_for_system_block(
        kind,
        params
            .get("assignment_brief")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    )
    .to_string()
}

/// The origin gate itself: the brief may reach the system block only on a
/// [`TurnOrigin::PmTask`] turn. Applied at extraction (inbound envelope)
/// and again at consumption (the stored Intent message), so neither layer
/// can hand the system block to an untrusted producer.
pub fn brief_for_system_block<'a>(kind: &str, brief: &'a str) -> &'a str {
    if acowork_core::intent::TurnOrigin::parse(kind) == acowork_core::intent::TurnOrigin::PmTask {
        brief
    } else {
        ""
    }
}

/// Render an Intent envelope as the user-role message the receiving model
/// sees. Shared by the mid-turn injection path (`loop_inbound`) and the
/// new-turn path (`session_task`) so both renderings stay identical.
#[allow(clippy::too_many_arguments)]
pub fn render_intent_message(
    from: &str,
    action: &str,
    params: &Value,
    message_id: &str,
    conversation_id: &str,
    source_chat_session_id: &str,
    repo: &str,
    kind: &str,
    in_reply_to: &str,
    status: &str,
) -> String {
    // Design 27 §3.2: the receiver sees the full envelope — reply
    // coordinates (conversation / source sessions) and the authorized
    // repo are part of the message, not ambient.
    //
    // §5.4.4: a human turn (`user_message`, or a `pm_task` dispatch —
    // both are people speaking) must reach the model as the person, not
    // an envelope. No reply coordinates either: the write-back is
    // system-driven from the envelope, not from anything the model reads
    // here. A `pm_task`'s assignment playbook is NOT part of this
    // message — the turn handler injects it into the system block.
    if !acowork_core::intent::TurnOrigin::parse(kind)
        .policy()
        .render_envelope
    {
        return match params.get("message").and_then(|v| v.as_str()) {
            Some(text) => text.to_string(),
            None => params.to_string(),
        };
    }
    let mut rendered = if kind == "response" || kind == "intent_response" {
        format!(
            "[intent_response:{from}:{action}] {params}
(in_reply_to: {in_reply_to}, status: {})",
            if status.is_empty() { "completed" } else { status }
        )
    } else {
        format!("[intent:{from}:{action}] {params}")
    };
    if !message_id.is_empty() {
        rendered.push_str(&format!("\n(message_id: {message_id})"));
    }
    if !conversation_id.is_empty() {
        rendered.push_str(&format!("\n(conversation_id: {conversation_id})"));
    }
    if !source_chat_session_id.is_empty() {
        rendered.push_str(&format!(
            "\n(source_chat_session_id: {source_chat_session_id})"
        ));
    }
    if !repo.is_empty() {
        rendered.push_str(&format!("\n(repo: {repo})"));
    }
    rendered
}

/// POST a Runtime-owned chat row through the Gateway relay
/// (`POST /api/intents/chat-append`, plan 3.0): `interaction_request`
/// cards (§5.5.4) and the terminal `agent_message` echoes a chat-bound
/// execution session writes back for locally-composed turns (ADR-100 D9).
/// Same source as the §5.5.1 write-back — the Runtime never speaks to
/// the user service directly. Idempotent by `meta.message_id`: a
/// redelivered turn produces no second row of the same kind.
///
/// Best-effort by contract: a lost pointer or card degrades the surface,
/// it does not lose the conversation; callers log and continue.
#[allow(clippy::too_many_arguments)] // positional row shape; a struct here is boilerplate
pub async fn append_chat_row(
    instance_id: &str,
    gateway_host: &str,
    node_token: &str,
    chat_session_id: &str,
    kind: &str,
    to: &str,
    body: &str,
    meta: serde_json::Value,
) -> Result<(), String> {
    let url = format!("http://{gateway_host}:{GATEWAY_HTTP_PORT}/api/intents/chat-append");
    let payload = serde_json::json!({
        "from": instance_id,
        "chat_session_id": chat_session_id,
        "kind": kind,
        "to": to,
        "body": body,
        "meta": meta,
    });
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(GATEWAY_WRITE_TIMEOUT)
        .json(&payload);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req.send().await.map_err(|e| format!("chat-append failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("chat-append rejected (HTTP {status}): {body}"))
    }
}

/// The `meta` of a continuation echo — the standalone `agent_message` a
/// chat-bound execution session writes back for a turn the user composed
/// there directly (ADR-100 D9).
///
/// A turn composed straight against the execution surface carries no request
/// context (unlike a routed Chat turn), so it has no `in_reply_to` to thread
/// under and no response entry to answer. The row is therefore standalone: an
/// `exec:`-prefixed id (never collides with a request/response row, and the
/// store's idempotency key so a redelivered turn cannot double the echo),
/// `sender_session_id` so the surface can drill into the session that produced
/// it, and `status` so an empty body still reads as a finished run rather than
/// an unanswered one.
///
/// `in_reply_to` is the exception, and ADR-100 D12 is what created it: a
/// **pinned** turn — composed in the execution view but delivered through the
/// Chat entry point — already has its `user_message` row in the chat, keyed by
/// `trigger_message_id`. Threading the echo under that row is what makes the
/// chat read as one round (the question the human asked, the answer that came
/// back) instead of an orphan result with no head. `None` keeps the
/// standalone shape the echo has always had.
///
/// `status` is the turn's terminal outcome (ADR-100 revision: intermediate
/// milestones such as dispatch/start are runtime-only and never persisted, so
/// the echo is the only row a locally-composed turn can leave). A stopped
/// local turn writes `aborted` with an empty body — the chat can then explain
/// why its previous message has no answer.
pub fn exec_echo_meta(
    local_message_id: &str,
    agent_session_id: &str,
    status: &str,
    trigger_message_id: Option<&str>,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "message_id": format!("exec:{local_message_id}"),
        "sender_session_id": agent_session_id,
        "status": if status.is_empty() { "completed" } else { status },
    });
    if let Some(trigger) = trigger_message_id.filter(|s| !s.is_empty()) {
        meta["in_reply_to"] = serde_json::Value::String(trigger.to_string());
    }
    meta
}

/// The `meta` of a routing reaction row (ADR-100 D11).
///
/// A routed Chat turn owes the chat an immediate in-conversation answer to
/// "where did my message go": the Runtime appends one `system_notice` with
/// `notice_type = turn_routed` at the moment the routing target is resolved,
/// carrying the routing-time verdict as durable data:
///
/// - `message_id = "tr:{request_id}"` — idempotent per trigger row, so a
///   redelivered envelope cannot double the reaction;
/// - `in_reply_to` — the trigger `user_message`, what threads the reaction
///   under it and what the surface uses to name the task (fallback: the
///   trigger body, see design 27 §5.5.5);
/// - `agent_session_id` — the execution session that will run the turn, the
///   drill target and the title source;
/// - `turn_mode` — `"new"` when the routing **created** a physical session,
///   `"continue"` for every reuse (the same logical task
///   carries on). It is a one-time routing-time fact, never re-derived.
///
/// The body stays empty: the row is rendered from i18n by the surface, in
/// the executing agent's attribution (avatar + name), never as a centered
/// system line and never in the agent's model voice.
pub fn turn_routed_meta(
    request_id: &str,
    agent_session_id: &str,
    opened_new: bool,
) -> serde_json::Value {
    serde_json::json!({
        "message_id": format!("tr:{request_id}"),
        "in_reply_to": request_id,
        "agent_session_id": agent_session_id,
        "turn_mode": if opened_new { "new" } else { "continue" },
        "notice_type": "turn_routed",
    })
}

/// The `meta` of the router-fallback receipt — the one channel fact a
/// degraded Chat turn owes the human (design 29 §9): the router could not
/// decide, so the message was forwarded automatically. Unlike the retired
/// progress milestones (ADR-100 revision), this notice is still written: the
/// fallback is a visible difference in behaviour, not a progress detail. The
/// row carries no body; the surfaces paint the localised pill from
/// `notice_type` (their `routerFallback` i18n key).
pub fn router_fallback_meta(request_id: &str) -> serde_json::Value {
    serde_json::json!({
        "message_id": format!("rf:{request_id}"),
        "in_reply_to": request_id,
        "notice_type": "router_fallback",
    })
}

/// POST the responder's final output back through the Gateway write entry
/// (`POST /api/intents/{request_id}/response`). The Gateway verifies the
/// caller is the recorded target, appends the `intent_response` chat row,
/// and pushes the response to the initiator (§7.1).
///
/// Best-effort by contract: a failed write-back is logged by the caller;
/// the responder's turn is already over and must not re-run.
pub async fn post_intent_response(
    instance_id: &str,
    gateway_host: &str,
    node_token: &str,
    session_id: &str,
    ctx: &IntentTurnContext,
    outcome: TurnOutcome,
    body: &str,
) -> Result<(), String> {
    let url = format!(
        "http://{gateway_host}:{GATEWAY_HTTP_PORT}/api/intents/{}/response",
        ctx.request_id
    );
    let payload = serde_json::json!({
        "sender": instance_id,
        "sender_session_id": session_id,
        "target": ctx.from,
        "action": ctx.action,
        "params": { "message": body },
        "kind": "response",
        "in_reply_to": ctx.request_id,
        "status": outcome.as_str(),
    });
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(GATEWAY_WRITE_TIMEOUT)
        .json(&payload);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway write-back failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("gateway rejected write-back (HTTP {status}): {body}"))
    }
}

/// Parse the ADR-046 attachment union an Intent envelope carries (design 27
/// §4.2.1, ADR-100 D12).
///
/// Both transports that take a human Chat turn read the same `attached_items`
/// key the Gateway stamped into `params_json`: the pinned path feeds
/// `SessionMessage::ChatMessage`'s own arm, the routed path the `Intent` one.
/// One parse for both, so a reference never means two things on the wire.
///
/// An entry the Runtime cannot read is dropped, not fatal — the turn still
/// runs with what could be parsed (same rule the Chat-attachment pull applies).
pub fn attached_items_from_params(
    params: &Value,
) -> Option<Vec<acowork_core::protocol::AttachedItem>> {
    params
        .get("attached_items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|d| {
                    serde_json::from_value::<acowork_core::protocol::AttachedItem>(d.clone()).ok()
                })
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<acowork_core::protocol::AttachedItem>| !v.is_empty())
}

/// Pull one Chat attachment pinned to the durable `user_message` row this
/// Intent answers (design 27 §4.2.1). The bytes travel over node-auth HTTP
/// through the Gateway (`GET /api/intents/chat-attachments/…`), never over
/// MQTT: the Intent envelope carries ids only. The Gateway re-reads the
/// blob from the user service, which verifies the row is addressed to
/// `target` and pins `attachment_id` — so an id the agent was never sent
/// resolves to 404, not to somebody else's file.
///
/// Returns `(filename, mime, bytes)`. Filename comes from the streamed
/// response's `Content-Disposition` (`filename*=UTF-8''` preferred, plain
/// `filename="…"` fallback, attachment id as last resort).
pub async fn pull_chat_attachment(
    gateway_host: &str,
    node_token: &str,
    target_instance: &str,
    chat_session_id: &str,
    attachment_id: &str,
    message_id: &str,
) -> Result<(String, String, Vec<u8>), String> {
    let url = format!(
        "http://{gateway_host}:{GATEWAY_HTTP_PORT}/api/intents/chat-attachments/{target_instance}/{chat_session_id}/{attachment_id}/{message_id}"
    );
    let mut req = reqwest::Client::new()
        .get(&url)
        .timeout(GATEWAY_PULL_TIMEOUT);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req.send().await.map_err(|e| format!("chat attachment pull failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("chat attachment pull rejected (HTTP {status})"));
    }
    let headers = resp.headers().clone();
    let mime = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let filename = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_filename_from_content_disposition)
        .unwrap_or_else(|| attachment_id.to_string());
    let bytes = resp.bytes().await.map_err(|e| format!("chat attachment read failed: {e}"))?;
    Ok((filename, mime, bytes.to_vec()))
}

/// Extract the real filename from a `Content-Disposition` value, preferring
/// the percent-encoded `filename*=UTF-8''…` form (what the user service's
/// `content_disposition` emits for non-ASCII names).
fn parse_filename_from_content_disposition(cd: &str) -> Option<String> {
    if let Some(rest) = cd.to_ascii_lowercase().find("filename*=utf-8''").map(|i| &cd[i + 17..]) {
        let enc = rest.split(';').next().unwrap_or(rest).trim();
        return percent_decode(enc);
    }
    let (_, after) = cd.split_once("filename=\"")?;
    let name = after.split('"').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Minimal percent-decoding (UTF-8) for the `filename*` form — the same
/// charset rules the user service's encoder keeps unescaped.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Truncate a response body to the byte ceiling with an explicit marker
/// (design 27 §5.5.1 length bound; the initiator asks for more with a new
/// request rather than the wire carrying an unbounded turn).
pub fn truncate_response_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    // Cut on a char boundary.
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &body[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An envelope of the real wire shape (every field but from/action/params
    /// is `#[serde(default)]`), so a producer-side field rename cannot quietly
    /// become a test-side one.
    fn env(from: &str, chat: &str, has_human: Option<bool>) -> IntentEnvelope {
        serde_json::from_value(serde_json::json!({
            "from": from,
            "action": "chat",
            "params": { "message": "hi" },
            "message_id": "m-1",
            "source_chat_session_id": chat,
            "has_human": has_human,
        }))
        .unwrap()
    }

    #[test]
    fn pinned_turn_surface_keeps_its_card_target() {
        // The root cause this pins: a turn pinned to an execution session was
        // converted onto the ChatMessage transport carrying content + row id
        // only, and the card writer's guard read `chat_writeback` — a field
        // that path never set. `ask_user_question` then wrote no Chat card
        // while the execution dialog worked fine. The surface is produced
        // from the envelope by BOTH transports, so the coordinates survive
        // the conversion.
        let s = TurnSurface::from_envelope(&env("user-1", "chat-9", Some(true)));
        assert_eq!(s.card_target(), Some(("chat-9", "m-1", "user-1")));
        assert_eq!(s.echo_chat(), Some("chat-9"));
        assert_eq!(s.has_human, Some(true));
    }

    #[test]
    fn request_turn_owes_its_card_to_the_pair_session_it_was_bound_to() {
        // Same producer for the Intent transport: the pair session the
        // Gateway bound (and stamped) is where the card lands.
        let s = TurnSurface::from_envelope(&env("inst-a", "chat-pair", Some(false)));
        assert_eq!(s.card_target(), Some(("chat-pair", "m-1", "inst-a")));
        // A human-free chat is the one fact that must block *blocking*
        // behaviour (§6.5 R3) while the card itself stays writable — the gate
        // reads `has_human`, not the presence of a write-back.
        assert_eq!(s.has_human, Some(false));
    }

    #[test]
    fn unbound_or_senderless_turn_writes_no_card() {
        // Mailbox-only degradation: no chat bound → nothing to answer.
        let mut e = env("inst-a", "", Some(true));
        e.source_chat_session_id = String::new();
        assert_eq!(TurnSurface::from_envelope(&e).card_target(), None);
        // A chat with no known sender would be a dead button → not written.
        let mut e = env("", "chat-9", Some(true));
        e.from = String::new();
        assert_eq!(TurnSurface::from_envelope(&e).card_target(), None);
    }

    #[test]
    fn a_bound_chat_alone_does_not_make_a_card() {
        // A turn composed straight against the execution surface (ADR-100 D9):
        // the session's own chat binding gets the terminal echo, but there is
        // no triggering row and no answer coordinate, so no card — the
        // D12 distinction, expressed by the surface rather than by re-reading
        // the binding at every consumer.
        let mut s = TurnSurface::default();
        assert_eq!(s.card_target(), None);
        s.chat_session_id = "chat-9".into();
        assert_eq!(s.card_target(), None);
        assert_eq!(s.echo_chat(), Some("chat-9"));
    }

    #[test]
    fn waiting_is_not_aborted() {
        // Symptom 3 of the same root cause: the pinned arm had only a
        // `turn_aborted` bool, so a turn that ended *waiting for a human* was
        // echoed to the Chat as `aborted`. One outcome field, read by both
        // terminal writers, and the two states are distinct strings.
        assert_eq!(TurnOutcome::Waiting.as_str(), "waiting");
        assert_eq!(TurnOutcome::Aborted.as_str(), "aborted");
        assert_ne!(TurnOutcome::Waiting.as_str(), TurnOutcome::Aborted.as_str());
    }

    #[test]
    fn render_carries_envelope_coordinates() {
        let s = render_intent_message(
            "inst-a",
            "query",
            &serde_json::json!({"message": "hi"}),
            "m1",
            "c1",
            "cs1",
            "repo1",
            "request",
            "",
            "",
        );
        assert!(s.contains("[intent:inst-a:query]"));
        assert!(s.contains("(message_id: m1)"));
        assert!(s.contains("(conversation_id: c1)"));
        assert!(s.contains("(source_chat_session_id: cs1)"));
        assert!(s.contains("(repo: repo1)"));
    }

    #[test]
    fn render_response_variant() {
        let s = render_intent_message(
            "inst-b",
            "query",
            &serde_json::json!("answer"),
            "m2",
            "c1",
            "",
            "",
            "response",
            "m1",
            "completed",
        );
        assert!(s.contains("[intent_response:inst-b:query]"));
        assert!(s.contains("in_reply_to: m1"));
        assert!(!s.contains("source_chat_session_id"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "héllo wörld";
        let t = truncate_response_body(s, 4);
        assert!(t.ends_with("[truncated]"));
        assert!(s.starts_with(&t[..t.len() - "\n[truncated]".len()]));
        assert_eq!(truncate_response_body(s, 100), s);
    }

    #[test]
    fn user_message_renders_as_plain_human_input() {
        // §5.4.4: a human 1:1 message routed through the Intent transport
        // must reach the model as the person speaking — no envelope tag,
        // no coordinates.
        let s = render_intent_message(
            "user-1",
            "chat",
            &serde_json::json!({"message": "帮我看看日志"}),
            "m1",
            "",
            "cs1",
            "",
            "user_message",
            "",
            "",
        );
        assert_eq!(s, "帮我看看日志");
    }

    #[test]
    fn kind_semantics_split_role_from_writeback() {
        // D-C/C1: responder role and write-back are orthogonal.
        // A `request` carries both; a human `user_message` owes a
        // write-back but must not take the responder role.
        assert_eq!(turn_kind_semantics("request", "m1"), (true, true));
        assert_eq!(turn_kind_semantics("", "m1"), (true, true));
        assert_eq!(turn_kind_semantics("user_message", "m1"), (false, true));
        // No message_id → no write-back coordinate for a human message.
        assert_eq!(turn_kind_semantics("user_message", ""), (false, false));
        // A response arriving at the initiator: ordinary turn, no debt.
        assert_eq!(turn_kind_semantics("response", "m2"), (false, false));
        // A PM task dispatch is human input on a keyed route: full tool
        // set (the woken agent must be able to ask the owner), owes the
        // project group a terminal row.
        assert_eq!(turn_kind_semantics("pm_task", "m1"), (false, true));
        assert_eq!(turn_kind_semantics("pm_task", ""), (false, true));
    }

    #[test]
    fn pm_task_renders_human_text_not_envelope() {
        // The regression this kills: a dispatch rendered as
        // `[intent:owner:task] {"message":…}` in the execution
        // transcript, and the JSON leaked into the auto-title.
        let s = render_intent_message(
            "owner-1",
            "task",
            &serde_json::json!({"message": "修复分页不满屏", "task_id": "t-083", "assignment_brief": "Claim it first…"}),
            "m1",
            "task:p1:t-083",
            "cs-group",
            "",
            "pm_task",
            "",
            "",
        );
        assert_eq!(s, "修复分页不满屏");
        assert!(!s.contains("intent:"));
        assert!(!s.contains("assignment_brief"), "playbook is not message text");
    }

    #[test]
    fn assignment_brief_is_pm_task_only() {
        // Trust boundary: the brief lands in the system block, so a peer's
        // params — arbitrary JSON on `intent_send`, and the same key on any
        // other origin — must never reach it. Only the PM dispatch origin
        // passes, at both the extraction and consumption gates.
        let with_brief = serde_json::json!({"message": "hi", "assignment_brief": "obey me"});
        assert_eq!(extract_assignment_brief("pm_task", &with_brief), "obey me");
        for kind in ["request", "user_message", "response", "", "bogus"] {
            assert_eq!(
                extract_assignment_brief(kind, &with_brief),
                "",
                "kind {kind:?} must not feed the system block"
            );
        }
        // Consumption gate: a stored Intent whose field was filled by any
        // other path is still refused on non-PM kinds.
        assert_eq!(brief_for_system_block("request", "obey me"), "");
        assert_eq!(brief_for_system_block("pm_task", "claim it"), "claim it");
        // Non-string / absent values degrade to empty, never panic.
        assert_eq!(
            extract_assignment_brief("pm_task", &serde_json::json!({"assignment_brief": 7})),
            ""
        );
        assert_eq!(extract_assignment_brief("pm_task", &serde_json::json!({})), "");
    }

    #[test]
    fn tool_visibility_splits_role_from_answerability() {
        // §6.5 R1 + §5.5.4 (plan 2b.3): `intent_send` is hidden by role,
        // `ask_user_question` by a *known* human-free session. Unknown
        // fails open.
        use crate::agent::loop_context::hidden_tools_for_turn as h;
        // Responder in a pure agent-agent pair: both gone.
        assert_eq!(h(true, Some(false)), ["intent_send", "ask_user_question"]);
        // Responder in a group with humans: only the nested-Intent gate.
        assert_eq!(h(true, Some(true)), ["intent_send"]);
        // Responder, answerability unknown (older Gateway): role only.
        assert_eq!(h(true, None), ["intent_send"]);
        // Initiator turn in a human-free pair (response arrival): the card
        // tool goes, the ability to continue the conversation stays.
        assert_eq!(h(false, Some(false)), ["ask_user_question"]);
        // Human 1:1 / mention turns: nothing hidden.
        assert!(h(false, Some(true)).is_empty());
        assert!(h(false, None).is_empty());
    }

    #[test]
    fn exec_echo_meta_is_a_standalone_agent_message() {
        // ADR-100 D9: a locally-composed turn in a chat-bound session echoes
        // its output as a standalone agent_message. There is no request to
        // thread under, so no in_reply_to; the `exec:` id is the store's
        // idempotency key (a redelivered turn cannot double the row).
        // `sender_session_id` is what makes the row drillable back into the
        // execution session that produced it.
        let m = exec_echo_meta("local-7", "exec-3", "completed", None);
        assert_eq!(m["message_id"], "exec:local-7");
        assert_eq!(m["sender_session_id"], "exec-3");
        assert_eq!(m["status"], "completed");
        assert!(m.get("in_reply_to").is_none());

        // A stopped local turn still owes the chat a terminal row — it is the
        // only way the surface can tell an interrupted run from a silent one.
        let aborted = exec_echo_meta("local-8", "exec-3", "aborted", None);
        assert_eq!(aborted["message_id"], "exec:local-8");
        assert_eq!(aborted["status"], "aborted");

        // Empty status degrades to the ordinary completed echo.
        assert_eq!(
            exec_echo_meta("local-9", "exec-3", "", None)["status"],
            "completed"
        );
    }

    #[test]
    fn exec_echo_meta_threads_a_pinned_turn_under_its_chat_row() {
        // ADR-100 D12: a pinned turn was written to the chat as a real
        // `user_message` before it reached the agent, so the echo answers that
        // row. This is what pairs the two surfaces into one round — without it
        // the chat would hold an orphan result with no question above it, the
        // exact shape the pin exists to remove.
        let m = exec_echo_meta("row-1", "exec-3", "completed", Some("row-1"));
        assert_eq!(m["in_reply_to"], "row-1");
        // The id keeps the `exec:` prefix: idempotency must not depend on the
        // trigger, or a redelivered turn doubles the answer row.
        assert_eq!(m["message_id"], "exec:row-1");

        // An empty trigger is not a thread — it degrades to the standalone
        // shape rather than pointing the answer at a nonexistent row.
        let blank = exec_echo_meta("row-2", "exec-3", "completed", Some(""));
        assert!(blank.get("in_reply_to").is_none());
    }

    #[test]
    fn turn_routed_meta_threads_the_reaction_to_its_trigger() {
        // ADR-100 D11: the routing reaction row is keyed `tr:{request}` — one
        // per trigger message, so a redelivered envelope cannot double it —
        // and threaded under that trigger, which is what puts the agent
        // avatar + bubble immediately after the user's own message.
        let m = turn_routed_meta("msg-42", "20261023_131156_a3bb7d", true);
        assert_eq!(m["message_id"], "tr:msg-42");
        assert_eq!(m["in_reply_to"], "msg-42");
        assert_eq!(m["agent_session_id"], "20261023_131156_a3bb7d");
        assert_eq!(m["turn_mode"], "new");
        assert_eq!(m["notice_type"], "turn_routed");

        // Every reuse reads as "continue" — the same logical
        // task carries on; only physical creation is "new".
        assert_eq!(turn_routed_meta("msg-43", "s-1", false)["turn_mode"], "continue");
    }
}
