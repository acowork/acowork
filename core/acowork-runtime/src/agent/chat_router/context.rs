//! Block B / Block C of a Chat Router turn (design 29 §4–§5).
//!
//! **Block B** is the append-only history projection: chat rows in, fixed
//! `ChatMessage`s out. The projection is deterministic — the same row always
//! produces the same bytes, on any process, in any turn — because the cached
//! prefix is only stable if the projection is. That rules out relative times,
//! ids in the text, and `HashMap` iteration order.
//!
//! **Block C** is the turn's own view (surface, speaker, clock, draft config,
//! candidate list) merged with the current user message into the final `user`
//! message. Everything that changes between turns lives here, after the
//! cached prefix.
//!
//! Window anchoring: the manager keeps `anchor_seq` (the seq of the newest row
//! *excluded* from the front of the window). The window is sized in **tokens**
//! — how much context the router spends on history — not in rows: one-word
//! acknowledgements and 2 KB incident reports are not the same amount of
//! context. While the window stays under `max_tokens + slack_tokens` the
//! anchor never moves, so consecutive turns append one message each and reuse
//! the whole prefix; once the budget trips, the window is rebuilt down to
//! `retain_tokens` in one step.

use std::collections::HashSet;

use acowork_core::protocol::{AttachmentLabel, ChatAttachmentMeta};
use acowork_core::providers::traits::{CacheControl, ChatMessage};
use serde::Deserialize;
use serde_json::Value;

use super::prompt::truncate_utf8;

/// Rows that fit one Gateway page minus the current message and the anchor
/// row — the hard ceiling on any window, enforced by the transport rather
/// than by policy.
pub const PAGE_ROWS: usize = GATEWAY_MAX_PAGE - 2;
/// Rows the Gateway serves per `GET /api/intents/chat-history` call.
pub const GATEWAY_MAX_PAGE: usize = 50;
/// Per-row body cap (`max_message_bytes`).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 2048;
/// Attachment metadata cap per row.
pub const ATTACHMENT_META_MAX_BYTES: usize = 512;
/// Fixed truncation marker.
pub const TRUNCATION_SUFFIX: &str = "…[truncated]";

/// Stands in for an empty body when the turn carried files instead of words.
/// Fixed bytes on purpose: the decision log and the tests must be able to name
/// the exact shape a degenerate turn was routed with.
pub const ATTACHMENT_ONLY_PLACEHOLDER: &str = "(no text — the message is its attachments)";

// ── Wire rows ─────────────────────────────────────────────────────────

/// One `user_message` / `agent_message` / `interaction_request` /
/// `system_notice` row as returned by `GET /api/intents/chat-history`.
///
/// Only the fields the projection needs are deserialized; unknown fields are
/// ignored so the Gateway can extend the row shape without breaking the
/// router.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatRow {
    /// Session-local monotonic sequence. The window anchor is expressed in
    /// this space.
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub ts: i64,
    /// Participant id of the sender (user UUID or agent instance UUID).
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub meta: RowMeta,
    #[serde(default)]
    pub attachments: Vec<ChatAttachmentMeta>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RowMeta {
    #[serde(default)]
    pub message_id: String,
    #[serde(default)]
    pub in_reply_to: String,
    #[serde(default)]
    pub agent_session_id: String,
    #[serde(default)]
    pub notice_type: String,
    #[serde(default)]
    pub interaction: Option<RowInteraction>,
    /// Workspace references and execution-side uploads the human handed to
    /// this turn (ADR-100 D12). The current turn arrives the same way, through
    /// the intent's `attached_items` param, so a turn and its own history row
    /// describe their attachments with one shape and one renderer.
    #[serde(default)]
    pub exec_attachments: Vec<AttachmentLabel>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RowInteraction {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

impl ChatRow {
    /// Ids this row answers to: its own `message_id` plus, for a group
    /// `user_message`, the shared row id the mention fan-out replies to.
    /// Used to drop the current message from Block B.
    pub fn identity_ids(&self) -> impl Iterator<Item = &str> + '_ {
        [self.meta.message_id.as_str(), self.meta.in_reply_to.as_str()].into_iter()
    }
}

/// Parse the `messages` array of a chat-history response. Malformed rows are
/// dropped with a warning — a router turn must not die on one bad line
/// (ADR-092 bounded-read spirit).
pub fn parse_history(body: &Value) -> Result<Vec<ChatRow>, String> {
    let Some(arr) = body.get("messages").and_then(Value::as_array) else {
        return Err("chat history response has no `messages` array".to_string());
    };
    let mut rows = Vec::with_capacity(arr.len());
    for value in arr {
        match serde_json::from_value::<ChatRow>(value.clone()) {
            Ok(row) => rows.push(row),
            Err(e) => tracing::warn!(error = %e, "chat router: skipping malformed history row"),
        }
    }
    rows.sort_by_key(|r| r.seq);
    Ok(rows)
}

// ── Window budget ─────────────────────────────────────────────────────

/// Window caps, constructed from `agent_config.json` `chat_router`
/// (already clamped by `ChatRouterConfig`).
///
/// `max_tokens` is the primary criterion; `max_rows` is a transport ceiling
/// (one Gateway page), not a sizing policy.
#[derive(Debug, Clone, Copy)]
pub struct WindowBudget {
    /// Context the projected history may cost, in estimated tokens.
    pub max_tokens: usize,
    /// Row ceiling for one window.
    pub max_rows: usize,
}

impl Default for WindowBudget {
    fn default() -> Self {
        Self {
            // The single source of the default budget lives on the config
            // type — the panel, the wire doc and this fallback must never
            // disagree about what "default" means.
            max_tokens: crate::agent_config::ChatRouterConfig::DEFAULT_HISTORY_MAX_TOKENS,
            max_rows: PAGE_ROWS,
        }
    }
}

impl WindowBudget {
    /// Build from the configured token budget plus an optional row ceiling
    /// (`chat_router.history_window`, a deprecated sizing knob).
    ///
    /// The row ceiling can never exceed [`PAGE_ROWS`]: the whole window has
    /// to fit in one Gateway page, otherwise the window silently falls off
    /// the page's oldest end each turn — the prefix would shrink from the
    /// front and every turn would miss the cache, exactly what the anchor
    /// exists to prevent.
    pub fn new(max_tokens: usize, rows_ceiling: Option<usize>) -> Self {
        Self {
            max_tokens: max_tokens.max(1),
            max_rows: rows_ceiling
                .map(|rows| rows.clamp(1, PAGE_ROWS))
                .unwrap_or(PAGE_ROWS),
        }
    }

    /// Tokens the window may grow past `max_tokens` before the batch rebuild
    /// trips. Same shape as the retired row slack (1/8 of the cap): enough
    /// headroom that a rebuild is amortised over several turns.
    pub fn slack_tokens(&self) -> usize {
        (self.max_tokens / 8).max(64)
    }

    /// Tokens kept after a rebuild — half the budget, so the next rebuild
    /// lands after the window has grown by roughly half a budget again.
    pub fn retain_tokens(&self) -> usize {
        (self.max_tokens / 2).max(1)
    }

    /// Budget plus slack: the point where the window is rebuilt.
    pub fn threshold_tokens(&self) -> usize {
        self.max_tokens.saturating_add(self.slack_tokens())
    }

    /// Rows requested per turn: the row ceiling plus one turn of growth and
    /// the current message (which Block B excludes but the page still holds).
    pub fn page_size(&self) -> usize {
        (self.max_rows + 2).min(GATEWAY_MAX_PAGE)
    }

    /// Rows kept after a row-ceiling rebuild — half the ceiling, the same
    /// ratio the token budget uses, so the row pass amortises rebuilds the
    /// same way instead of sliding one row per turn.
    pub fn retain_rows(&self) -> usize {
        (self.max_rows / 2).max(1)
    }
}

// ── Block B ───────────────────────────────────────────────────────────

/// The projected history plus the anchor it was projected with.
#[derive(Debug, Clone)]
pub struct BlockB {
    /// Seq boundary for the *next* turn: every message here has a seq
    /// greater than this.
    pub anchor_seq: u64,
    pub messages: Vec<ChatMessage>,
    /// True when the window was rebuilt (anchor moved) this turn.
    pub rebuilt: bool,
    /// Estimated context this block costs, via the unified token counter —
    /// this is the number the budget is enforced against, and the one the
    /// decision log records.
    pub token_len: usize,
}

/// Build Block B.
///
/// - `anchor_seq`: the seq of the newest row excluded from the front
///   (`0` = no anchor yet, the whole fetched page is eligible).
/// - `exclude_ids`: coordinates of the current message (`message_id`, and for
///   a group mention also `in_reply_to`) — those rows are the Block C
///   message and must not appear twice.
/// - `agent_instance`: used only to role-map legacy `text` rows.
///
/// Trimming is two passes (design 29 §4.4): the row ceiling (transport, one
/// Gateway page) and then the token budget (policy). Both move the anchor to
/// the seq of the last dropped row, so the boundary survives a restart as
/// long as the caller persists it. Both drop in one batch — never a row per
/// turn — because a window that slides one row at a time re-hashes the whole
/// cached prefix every turn.
pub fn build_block_b(
    rows: &[ChatRow],
    anchor_seq: u64,
    exclude_ids: &HashSet<String>,
    agent_instance: &str,
    budget: &WindowBudget,
) -> BlockB {
    let mut window: Vec<&ChatRow> = rows
        .iter()
        .filter(|r| r.seq > anchor_seq)
        .filter(|r| !exclude_ids.iter().any(|id| r.identity_ids().any(|rid| rid == id)))
        .collect();

    let mut anchor = anchor_seq;
    let mut rebuilt = false;

    // Row ceiling. This is a transport bound (one Gateway page), not the sizing
    // policy — but it must still drop in a batch. A window that trims one row
    // per turn re-hashes the whole cached prefix every turn, so once the page
    // is full the row pass rebuilds down to `retain_rows` and the next turns
    // append again.
    if window.len() > budget.max_rows {
        let keep = budget.retain_rows();
        let drop_count = window.len() - keep;
        anchor = window[drop_count - 1].seq.max(anchor);
        window.drain(..drop_count);
        rebuilt = true;
    }

    let mut projected: Vec<(u64, ChatMessage, usize)> = window
        .iter()
        .filter_map(|row| {
            project_row(row, agent_instance).map(|m| {
                let tokens = crate::token::count_text(&m.content);
                (row.seq, m, tokens)
            })
        })
        .collect();

    // Token budget (policy). Past `max_tokens + slack`, keep the newest rows
    // that fit `retain_tokens` — at least one, so a single oversized row can
    // never empty the window.
    let mut tokens: usize = projected.iter().map(|(_, _, t)| *t).sum();
    if tokens > budget.threshold_tokens() {
        let retain = budget.retain_tokens();
        let mut keep = 0usize;
        let mut kept_tokens = 0usize;
        for (_, _, row_tokens) in projected.iter().rev() {
            if keep > 0 && kept_tokens + row_tokens > retain {
                break;
            }
            kept_tokens += row_tokens;
            keep += 1;
        }
        let drop_count = projected.len().saturating_sub(keep);
        if drop_count > 0 {
            anchor = anchor.max(projected[drop_count - 1].0);
            tokens = kept_tokens;
            projected.drain(..drop_count);
            rebuilt = true;
        }
    }

    let mut messages: Vec<ChatMessage> = projected.into_iter().map(|(_, m, _)| m).collect();
    // Anthropic breakpoint at the end of the append-only prefix (design 29
    // §6.1). Providers with automatic prefix caching ignore the field.
    if let Some(last) = messages.last_mut() {
        last.cache_control = Some(CacheControl::Ephemeral);
    }

    BlockB {
        anchor_seq: anchor,
        messages,
        rebuilt,
        token_len: tokens,
    }
}

/// Project one chat row. `None` for rows that carry nothing a router can use
/// (unknown kinds, empty cards).
pub fn project_row(row: &ChatRow, agent_instance: &str) -> Option<ChatMessage> {
    match row.kind.as_str() {
        "user_message" => Some(ChatMessage::user(prefix_body(
            &format!("[user:{}]", row.from),
            &row.body,
            &row.attachments,
            &row.meta.exec_attachments,
        ))),
        "agent_message" => Some(ChatMessage::assistant(prefix_body(
            &format!("[agent:{}]", row.from),
            &row.body,
            &row.attachments,
            &row.meta.exec_attachments,
        ))),
        "interaction_request" => {
            let mut text = String::from("[card] ");
            let card = row.meta.interaction.as_ref();
            let title = card
                .map(|c| c.question.trim())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| row.body.trim());
            text.push_str(title);
            if let Some(card) = card {
                for option in &card.options {
                    let option = option.trim();
                    if !option.is_empty() {
                        text.push_str("\n- ");
                        text.push_str(option);
                    }
                }
            }
            Some(ChatMessage::user(truncate_body(&text, DEFAULT_MAX_MESSAGE_BYTES)))
        }
        "system_notice" => Some(ChatMessage::user(notice_text(row))),
        // Legacy 1:1 rows predating the four-kind vocabulary carry
        // `kind = "text"`. Role comes from who wrote it.
        "text" | "" => {
            let role_is_agent = !agent_instance.is_empty() && row.from == agent_instance;
            let tag = if role_is_agent { "[agent" } else { "[user" };
            let content = prefix_body(
                &format!("{tag}:{}]", row.from),
                &row.body,
                &row.attachments,
                &row.meta.exec_attachments,
            );
            Some(if role_is_agent {
                ChatMessage::assistant(content)
            } else {
                ChatMessage::user(content)
            })
        }
        _ => None,
    }
}

/// Fixed notice texts (design 29 §4.3). The `turn_dispatched` / `turn_started`
/// arms are LEGACY-ONLY: the ADR-100 revision stopped writing those milestones
/// (intermediate state is runtime-only), so only transcripts produced before
/// the revision can carry them. They stay here so a router reading an old
/// history still sees a sane line instead of a raw notice type.
fn notice_text(row: &ChatRow) -> String {
    match row.meta.notice_type.as_str() {
        "turn_dispatched" => {
            let sid = &row.meta.agent_session_id;
            let short: String = sid.chars().take(8).collect();
            if short.is_empty() {
                "[system] Dispatched to execution".to_string()
            } else {
                format!("[system] Dispatched to session {short}")
            }
        }
        "turn_started" => {
            let sid = &row.meta.agent_session_id;
            let short: String = sid.chars().take(8).collect();
            if short.is_empty() {
                "[system] A round started".to_string()
            } else {
                format!("[system] A round started in session {short}")
            }
        }
        "delivery_failed" => "[system] Delivery failed".to_string(),
        "budget_exhausted" => "[system] Conversation budget exhausted".to_string(),
        "truncated" => "[system] Response truncated".to_string(),
        "unavailable" => "[system] Agent unavailable".to_string(),
        "router_fallback" => "[system] Routed automatically".to_string(),
        "" => "[system] Notice".to_string(),
        other => format!("[system] Notice: {other}"),
    }
}

/// `<tag> <body>` with the attachment lines appended, all bounded.
fn prefix_body(
    tag: &str,
    body: &str,
    uploads: &[ChatAttachmentMeta],
    refs: &[AttachmentLabel],
) -> String {
    let mut out = String::with_capacity(tag.len() + body.len() + 8);
    out.push_str(tag);
    out.push(' ');
    out.push_str(&truncate_body(body, DEFAULT_MAX_MESSAGE_BYTES));
    out.push_str(&attachment_lines(uploads, refs));
    out
}

/// The one answer to "does this turn carry anything at all?".
///
/// Words, an uploaded file, or a named reference. Two consumers ask it — the
/// routing gate (a turn with no content is not worth a router call) and Block C
/// (a turn with content and no words is a normal turn, not a blank one) — and
/// they must not answer separately, or the gate can wave through a turn the
/// prompt then renders as an empty message.
pub(crate) fn turn_has_content(
    user_message: &str,
    uploads: &[ChatAttachmentMeta],
    refs: &[AttachmentLabel],
) -> bool {
    !user_message.trim().is_empty() || !uploads.is_empty() || !refs.is_empty()
}

/// `true` when the turn carries files or references and no words.
///
/// One predicate for two surfaces — the Block C body and the decision log —
/// so the prompt and the log can never disagree about what a turn was. A
/// whitespace-only body counts as no words: the human attached and did not type.
pub(crate) fn is_attachment_only(
    user_message: &str,
    uploads: &[ChatAttachmentMeta],
    refs: &[AttachmentLabel],
) -> bool {
    user_message.trim().is_empty() && turn_has_content(user_message, uploads, refs)
}

/// Entries joined by `, ` and capped at [`ATTACHMENT_META_MAX_BYTES`]. An
/// entry that does not fit is dropped whole, so a line never ends mid-entry.
///
/// One loop for both attachment lists: the cap is a prompt-budget rule, not a
/// property of either arm, and two copies of it start disagreeing the moment
/// one of them is retuned.
fn join_capped(entries: impl Iterator<Item = String>) -> Option<String> {
    let mut out = String::new();
    for entry in entries {
        let entry = if out.is_empty() {
            entry
        } else {
            format!(", {entry}")
        };
        if out.len() + entry.len() > ATTACHMENT_META_MAX_BYTES {
            break;
        }
        out.push_str(&entry);
    }
    (!out.is_empty()).then_some(out)
}

/// `report.pdf (application/pdf, 12KB), screenshot.png (image/png, 340KB)` —
/// the files this turn's human uploaded into the conversation.
fn attachments_line(attachments: &[ChatAttachmentMeta]) -> Option<String> {
    join_capped(attachments.iter().map(|att| {
        format!(
            "{} ({}, {})",
            sanitize_inline(&att.filename),
            sanitize_inline(&att.mime),
            human_size(att.size)
        )
    }))
}

/// `chat_prompt.rs, src/lib.rs (L12-L25)` — what the turn names without
/// holding. A reference is not a copy (ADR-046): the Agent that receives it
/// reads the path with its own tools, and the router only ever learns that the
/// human pointed at something. The names are still decisive for routing —
/// "look at this" plus a filename is a work order, and the incident this
/// function exists for was a router that read it as small talk.
///
/// The `L{start}-L{end}` spelling is the same one every chip uses
/// (`attachmentGlyph.selectionRange`), so the prompt and the transcript
/// describe one selection with one string.
fn refs_line(refs: &[AttachmentLabel]) -> Option<String> {
    join_capped(refs.iter().map(|reference| {
        let range = match (reference.start_line, reference.end_line) {
            (Some(start), Some(end)) if start != end => format!(" (L{start}-L{end})"),
            (Some(start), _) => format!(" (L{start})"),
            _ => String::new(),
        };
        format!("{}{}", sanitize_inline(&reference.filename), range)
    }))
}

/// The two attachment lines of a row or a turn, already prefixed. Empty when
/// the turn carries neither.
///
/// Block B and Block C both go through here. That is the point: a message the
/// router decided on must read identically once it is history, or every later
/// turn reasons about a transcript that never looked like the live one.
fn attachment_lines(uploads: &[ChatAttachmentMeta], refs: &[AttachmentLabel]) -> String {
    let mut out = String::new();
    if let Some(meta) = attachments_line(uploads) {
        out.push_str("\n[attachments] ");
        out.push_str(&meta);
    }
    if let Some(meta) = refs_line(refs) {
        out.push_str("\n[attached refs] ");
        out.push_str(&meta);
    }
    out
}

/// `900B` / `12KB` / `3.4MB` — deterministic, no locale.
pub fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{}KB", bytes / 1024)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Replace newlines/tabs inside a one-line field so a filename cannot forge
/// a second logical line.
fn sanitize_inline(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Truncate a body to `max` bytes on a UTF-8 boundary, appending
/// [`TRUNCATION_SUFFIX`] when anything was cut. Returns the input unchanged
/// (no allocation) when it fits.
pub fn truncate_body(body: &str, max: usize) -> String {
    if body.len() <= max {
        return body.to_string();
    }
    let keep = max.saturating_sub(TRUNCATION_SUFFIX.len());
    format!("{}{TRUNCATION_SUFFIX}", truncate_utf8(body, keep))
}

// ── Block C ───────────────────────────────────────────────────────────

/// One row of the `[candidates]` block. No session id, owner, model — the
/// alias is the only handle the model ever sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateLine {
    pub alias: String,
    /// The session currently bound to this chat.
    pub current: bool,
    /// `working` / `idle` / `errored` / `closed` (ADR-099 §5.2 vocabulary —
    /// coarser than `SessionStatus` on purpose).
    pub status: String,
    /// Relative age ("2m", "3h", "2d") — only Block C may carry time.
    pub age: String,
    /// `chat` or `task`.
    pub origin: String,
    pub title: String,
}

impl CandidateLine {
    fn render(&self) -> String {
        format!(
            "{:<3}  {:<7}  {:<8}  {:<4}  {:<5}  {}",
            self.alias,
            if self.current { "current" } else { "" },
            self.status,
            self.age,
            self.origin,
            self.title
        )
    }
}

/// Relative age for the candidate list. Deliberately coarse — a minute-level
/// value would churn the tail of every turn for nothing.
pub fn relative_age(now_secs: i64, then_secs: i64) -> String {
    let diff = now_secs.saturating_sub(then_secs).max(0);
    if diff < 60 {
        "0m".to_string()
    } else if diff < 3600 {
        format!("{}m", diff / 60)
    } else if diff < 86_400 {
        format!("{}h", diff / 3600)
    } else {
        format!("{}d", diff / 86_400)
    }
}

/// Everything Block C needs for one turn.
pub struct TurnContext<'a> {
    /// `dm` / `group`.
    pub surface: &'a str,
    /// Current speaker (participant id); the only human allowed to be acted
    /// for on this turn.
    pub speaker: &'a str,
    /// RFC3339, produced by the caller once.
    pub now: &'a str,
    /// Rendered draft config (`model=…, workspace=…`), `None` = untouched.
    pub draft: Option<&'a str>,
    pub candidates: &'a [CandidateLine],
    /// `project=… task=…` when the turn is a PM follow-up.
    pub task: Option<&'a str>,
    /// False when the history read failed — the model is told to be careful.
    pub history_available: bool,
    /// The user's message, verbatim.
    pub user_message: &'a str,
    /// Files this turn carries, as the Gateway read them back from the stored
    /// row. Rendered with the *same* line function Block B uses, so the turn
    /// being routed looks exactly like the same turn once it becomes history.
    pub attachments: &'a [ChatAttachmentMeta],
    /// Workspace references this turn names (the `attached_items` arm the
    /// composer sent). Rendered through the same function Block B uses, so the
    /// turn being routed and the same turn once it is history cannot disagree.
    pub refs: &'a [AttachmentLabel],
}

impl TurnContext<'_> {
    /// Render the final `user` message: `[runtime context]` … blank line …
    /// `[user message]`, then — when the turn carried files — the same
    /// `[attachments] …` line Block B paints for that message once it is
    /// history.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.user_message.len() + 512);
        out.push_str("[runtime context]\n");
        out.push_str(&format!("surface: {}\n", self.surface));
        out.push_str(&format!("speaker: {}\n", self.speaker));
        out.push_str(&format!("now: {}\n", self.now));
        out.push_str(&format!(
            "draft: {}\n",
            self.draft.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("(none)")
        ));
        if !self.history_available {
            out.push_str("history: unavailable (the read failed; do not assume what was said before)\n");
        }
        out.push_str("[candidates]\n");
        if self.candidates.is_empty() {
            out.push_str("(none)\n");
        } else {
            for line in self.candidates {
                out.push_str(&line.render());
                out.push('\n');
            }
        }
        if let Some(task) = self.task.map(str::trim).filter(|s| !s.is_empty()) {
            out.push_str("[task]\n");
            out.push_str(task);
            out.push('\n');
        }
        out.push_str("\n[user message]\n");
        // An attachment-only turn is a normal turn, not an empty one: the human
        // sent something, they just did not type. Without this the model reads a
        // blank message and treats it as small talk instead of work.
        if is_attachment_only(self.user_message, self.attachments, self.refs) {
            out.push_str(ATTACHMENT_ONLY_PLACEHOLDER);
        } else {
            out.push_str(self.user_message);
        }
        out.push_str(&attachment_lines(self.attachments, self.refs));
        out
    }
}

// ── Prefix hash ───────────────────────────────────────────────────────

/// Hash over `Block A + tools schema + Block B` (design 29 §6.4).
///
/// Eight hex characters (first 8 bytes of SHA-256) is enough to detect a
/// changed prefix in the decision log without storing the prompt itself.
pub fn prefix_hash(block_a: &str, tools_json: &str, history: &[ChatMessage]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;

    let mut hasher = Sha256::new();
    hasher.update(block_a.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(tools_json.as_bytes());
    hasher.update(b"\x1f");
    for msg in history {
        hasher.update(b"\x1e");
        hasher.update(msg.content.as_bytes());
    }
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(seq: u64, kind: &str, from: &str, body: &str) -> ChatRow {
        ChatRow {
            seq,
            ts: 1_700_000_000 + seq as i64,
            from: from.to_string(),
            kind: kind.to_string(),
            body: body.to_string(),
            meta: RowMeta {
                message_id: format!("m{seq}"),
                ..Default::default()
            },
            attachments: Vec::new(),
        }
    }

    fn ids(list: &[&str]) -> HashSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// Tokens one projected row costs when its body is `body_len` chars —
    /// the same number `build_block_b` charges against the budget, so the
    /// assertions below can state their premises instead of guessing.
    fn row_tokens(body_len: usize) -> usize {
        crate::token::count_text(&format!("[user:u1] {}", "x".repeat(body_len)))
    }

    #[test]
    fn projection_is_deterministic_and_tagged() {
        let rows = vec![
            row(1, "user_message", "u1", "hello"),
            row(2, "agent_message", "a1", "hi there"),
        ];
        let a = build_block_b(&rows, 0, &ids(&[]), "a1", &WindowBudget::default());
        let b = build_block_b(&rows, 0, &ids(&[]), "a1", &WindowBudget::default());
        let bytes = |block: &BlockB| -> Vec<(String, String)> {
            block
                .messages
                .iter()
                .map(|m| (format!("{:?}", m.role), m.content.clone()))
                .collect()
        };
        assert_eq!(bytes(&a), bytes(&b));
        assert!(a.messages[0].content.starts_with("[user:u1] hello"));
        assert!(a.messages[1].content.starts_with("[agent:a1] hi there"));
    }

    #[test]
    fn the_current_message_is_excluded_by_message_id() {
        let mut current = row(3, "user_message", "u1", "this turn");
        current.meta.in_reply_to = "shared-row".to_string();
        current.meta.message_id = "shared-row".to_string();
        let rows = vec![row(1, "user_message", "u1", "older"), current];
        let block = build_block_b(
            &rows,
            0,
            &ids(&["envelope-id", "shared-row"]),
            "a1",
            &WindowBudget::default(),
        );
        assert_eq!(block.messages.len(), 1);
        assert!(block.messages[0].content.contains("older"));
    }

    #[test]
    fn cards_and_notices_use_fixed_text() {
        let mut card = row(1, "interaction_request", "a1", "ignored");
        card.meta.interaction = Some(RowInteraction {
            question: "Proceed?".to_string(),
            options: vec!["yes".to_string(), "no".to_string()],
        });
        let mut notice = row(2, "system_notice", "system", "raw");
        notice.meta.notice_type = "turn_started".to_string();
        notice.meta.agent_session_id = "0123456789abcdef".to_string();
        let mut dispatched = row(3, "system_notice", "system", "raw");
        dispatched.meta.notice_type = "turn_dispatched".to_string();
        dispatched.meta.agent_session_id = "0123456789abcdef".to_string();
        let block =
            build_block_b(&[card, notice, dispatched], 0, &ids(&[]), "a1", &WindowBudget::default());
        assert_eq!(block.messages[0].content, "[card] Proceed?\n- yes\n- no");
        assert_eq!(block.messages[1].content, "[system] A round started in session 01234567");
        // The dispatch milestone has its own fixed text (distinct from the
        // turn-start row the same turn also lands).
        assert_eq!(
            block.messages[2].content,
            "[system] Dispatched to session 01234567"
        );
    }

    #[test]
    fn anchor_stays_put_until_the_token_budget_trips() {
        let budget = WindowBudget::new(1_000, None);
        let per_row = row_tokens(100);
        // Rows that still fit under budget + slack — the largest window that
        // must NOT be rebuilt.
        let fitting = budget.threshold_tokens() / per_row;
        assert!(fitting >= 4, "test premise: window spans several rows");
        let body = "x".repeat(100);
        let rows_upto = |n: usize| -> Vec<ChatRow> {
            (1..=n as u64)
                .map(|i| row(i, "user_message", "u1", &body))
                .collect()
        };

        // At the threshold nothing is dropped, the anchor does not move.
        let block = build_block_b(&rows_upto(fitting), 0, &ids(&[]), "a1", &budget);
        assert_eq!(block.messages.len(), fitting);
        assert_eq!(block.anchor_seq, 0);
        assert!(!block.rebuilt);
        assert!(block.token_len <= budget.threshold_tokens());

        // One more row trips the budget: rebuild down to `retain_tokens`.
        let block = build_block_b(&rows_upto(fitting + 1), 0, &ids(&[]), "a1", &budget);
        assert!(block.rebuilt);
        let kept = block.messages.len();
        assert!(kept < fitting + 1);
        assert!(
            block.token_len <= budget.retain_tokens() + per_row,
            "rebuilt window must land near the retain target, got {}",
            block.token_len
        );
        assert_eq!(block.anchor_seq, (fitting + 1 - kept) as u64);

        // The next turn appends exactly one message on top of the rebuilt
        // window — the prefix is stable again.
        let next = build_block_b(&rows_upto(fitting + 2), block.anchor_seq, &ids(&[]), "a1", &budget);
        assert_eq!(next.messages.len(), kept + 1);
        assert_eq!(next.anchor_seq, block.anchor_seq);
        assert!(!next.rebuilt);
    }

    #[test]
    fn token_trim_keeps_the_newest_rows_and_moves_the_anchor_once() {
        let budget = WindowBudget::new(1_000, None);
        let body = "x".repeat(100);
        let per_row = row_tokens(100);
        let rows: Vec<ChatRow> = (1..=40)
            .map(|i| row(i, "user_message", "u1", &body))
            .collect();
        assert!(
            40 * per_row > budget.threshold_tokens(),
            "test premise: 40 rows are over budget"
        );

        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &budget);
        assert!(block.rebuilt);
        let kept = block.messages.len();
        assert!(kept < 40);
        // The window is the *newest* end of the history, in order.
        assert!(block.messages.last().unwrap().content.ends_with(&body));
        assert_eq!(block.anchor_seq, (40 - kept) as u64);

        // Same rows, new anchor → identical window, anchor stays put. This is
        // the anti-sliding guard: a rebuild must not happen twice per row.
        let again = build_block_b(&rows, block.anchor_seq, &ids(&[]), "a1", &budget);
        assert_eq!(again.messages.len(), kept);
        assert_eq!(again.anchor_seq, block.anchor_seq);
        assert!(!again.rebuilt);
    }

    #[test]
    fn an_oversized_row_never_empties_the_window() {
        // Budget far below one row's cost (a 2 KB body is ~570 tokens). The
        // newest row is still kept: losing the follow-up entirely is worse
        // for routing than carrying one row over budget.
        let budget = WindowBudget::new(1, None);
        let body = "x".repeat(2_000);
        let rows: Vec<ChatRow> = (1..=3)
            .map(|i| row(i, "user_message", "u1", &body))
            .collect();
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &budget);
        assert_eq!(block.messages.len(), 1);
        assert_eq!(block.anchor_seq, 2);
        assert!(block.rebuilt);
    }

    #[test]
    fn the_row_ceiling_still_caps_the_window() {
        // `history_window` survives as a transport-side row cap (hand-edited
        // configs only); it must still bound the window on its own.
        let budget = WindowBudget::new(64_000, Some(4));
        let rows: Vec<ChatRow> = (1..=10)
            .map(|i| row(i, "user_message", "u1", "x"))
            .collect();
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &budget);
        assert_eq!(block.messages.len(), budget.retain_rows());
        assert_eq!(block.anchor_seq, 10 - budget.retain_rows() as u64);
        assert!(block.rebuilt);
    }

    #[test]
    fn a_full_page_does_not_slide_the_window_one_row_per_turn() {
        // Short messages never reach the token budget, so the page ceiling is
        // what bounds the window. It must rebuild in a batch: dropping one
        // row per turn would re-hash the cached prefix on every single turn.
        let budget = WindowBudget::default();
        let body = "x";
        let rows_upto = |n: usize| -> Vec<ChatRow> {
            (1..=n as u64)
                .map(|i| row(i, "user_message", "u1", body))
                .collect()
        };
        let per_row = row_tokens(1);
        assert!(
            budget.page_size() * per_row < budget.threshold_tokens(),
            "test premise: the token budget never binds here"
        );

        // The page is full: the row pass rebuilds once, down to `retain_rows`.
        let block = build_block_b(&rows_upto(budget.max_rows + 1), 0, &ids(&[]), "a1", &budget);
        assert!(block.rebuilt);
        assert_eq!(block.messages.len(), budget.retain_rows());

        // The next turns append — no rebuild — until the ceiling is reached
        // again, exactly like the token pass.
        let next = build_block_b(&rows_upto(budget.max_rows + 2), block.anchor_seq, &ids(&[]), "a1", &budget);
        assert_eq!(next.messages.len(), budget.retain_rows() + 1);
        assert_eq!(next.anchor_seq, block.anchor_seq);
        assert!(!next.rebuilt);
    }

    #[test]
    fn the_window_always_fits_one_page() {
        let budget = WindowBudget::new(8_000, None);
        assert_eq!(budget.max_rows, PAGE_ROWS);
        assert_eq!(budget.page_size(), GATEWAY_MAX_PAGE);

        // A configured ceiling below the page cap is honoured ...
        let capped = WindowBudget::new(8_000, Some(10));
        assert_eq!(capped.max_rows, 10);
        assert_eq!(capped.page_size(), 12);

        // ... and one above it is trimmed to what the transport can serve.
        let huge = WindowBudget::new(8_000, Some(10_000));
        assert_eq!(huge.max_rows, PAGE_ROWS);
        assert_eq!(huge.page_size(), GATEWAY_MAX_PAGE);
    }

    #[test]
    fn long_bodies_are_truncated_on_char_boundaries() {
        let body = "汉".repeat(2000);
        let row = row(1, "user_message", "u1", &body);
        let block = build_block_b(&[row], 0, &ids(&[]), "a1", &WindowBudget::default());
        let content = &block.messages[0].content;
        assert!(content.ends_with(TRUNCATION_SUFFIX));
        assert!(content.len() <= DEFAULT_MAX_MESSAGE_BYTES + 32);
        // Valid UTF-8 by construction — the char boundary was respected.
        assert!(content.chars().all(|c| c.is_ascii() || c == '汉' || c == '…'));
    }

    #[test]
    fn attachment_metadata_is_bounded_and_single_line() {
        let mut row = row(1, "user_message", "u1", "see files");
        row.attachments = vec![
            ChatAttachmentMeta {
                filename: "report.pdf".into(),
                mime: "application/pdf".into(),
                size: 12 * 1024,
            },
            ChatAttachmentMeta {
                filename: "shot\n2.png".into(),
                mime: "image/png".into(),
                size: 340 * 1024,
            },
        ];
        let block = build_block_b(&[row], 0, &ids(&[]), "a1", &WindowBudget::default());
        let content = &block.messages[0].content;
        assert!(content.contains("[attachments] report.pdf (application/pdf, 12KB), shot 2.png (image/png, 340KB)"));
        assert_eq!(content.lines().count(), 2, "attachment metadata must be one line");
    }

    // ── Block C (the turn being routed) ───────────────────────────────
    // The router used to see the text alone: attachments reached only the
    // history projection, so the one turn that carried a file was the one turn
    // it could not see it on. These three pin the fix.

    #[test]
    fn block_c_attachment_line_matches_the_history_projection_byte_for_byte() {
        let mut row = row(1, "user_message", "u1", "look at this");
        row.attachments = vec![
            ChatAttachmentMeta {
                filename: "report.pdf".into(),
                mime: "application/pdf".into(),
                size: 12 * 1024,
            },
            ChatAttachmentMeta {
                filename: "shot 2.png".into(),
                mime: "image/png".into(),
                size: 340 * 1024,
            },
        ];
        let block = build_block_b(
            std::slice::from_ref(&row),
            0,
            &ids(&[]),
            "a1",
            &WindowBudget::default(),
        );
        // One renderer, two call sites: the same message must read the same
        // whether it is history or the turn being routed right now.
        let history_line = block.messages[0]
            .content
            .lines()
            .last()
            .expect("history row renders")
            .to_string();

        let ctx = TurnContext {
            surface: "dm",
            speaker: "u1",
            now: "2026-10-21T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "look at this",
            attachments: &row.attachments,
            refs: &[],
        };
        let rendered = ctx.render();
        let turn_line = rendered.lines().last().expect("block c renders");
        assert_eq!(turn_line, history_line);
        assert!(turn_line.starts_with("[attachments] report.pdf (application/pdf, 12KB)"));
    }

    /// A reference the human added from the workspace panel must read the same
    /// in the history window as it did when it was the live turn — otherwise
    /// every later turn reasons over a transcript that never looked like the
    /// one the router actually decided on.
    #[test]
    fn a_reference_reads_identically_as_a_turn_and_as_history() {
        let mut row = row(1, "user_message", "u1", "你先看看这个");
        row.meta.exec_attachments = vec![AttachmentLabel {
            filename: "chat_prompt.rs".into(),
            kind: "attached_file".into(),
            ..Default::default()
        }];
        let history_line = project_row(&row, "agent-1")
            .expect("row projects")
            .content
            .lines()
            .last()
            .expect("attachment line")
            .to_string();

        let turn_line = TurnContext {
            surface: "dm",
            speaker: "u1",
            now: "2026-11-06T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "你先看看这个",
            attachments: &row.attachments,
            refs: &row.meta.exec_attachments,
        }
        .render();
        let turn_line = turn_line.lines().last().expect("block c renders");
        assert_eq!(turn_line, history_line);
        assert!(turn_line.starts_with("[attached refs] chat_prompt.rs"));
    }

    #[test]
    fn an_attachment_only_turn_says_so_instead_of_going_blank() {
        let attachments = vec![ChatAttachmentMeta {
            filename: "shot.png".into(),
            mime: "image/png".into(),
            size: 4096,
        }];
        let rendered = TurnContext {
            surface: "dm",
            speaker: "u1",
            now: "2026-10-21T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "   ",
            attachments: &attachments,
            refs: &[],
        }
        .render();

        assert!(rendered.contains(ATTACHMENT_ONLY_PLACEHOLDER));
        assert!(rendered.contains("[attachments] shot.png (image/png, 4KB)"));
        // The placeholder IS the body — never an empty line followed by it.
        assert!(!rendered.contains("[user message]\n\n"));
    }

    #[test]
    fn a_text_only_turn_renders_no_attachment_line() {
        let rendered = TurnContext {
            surface: "dm",
            speaker: "u1",
            now: "2026-10-21T14:03:00+08:00",
            draft: None,
            candidates: &[],
            task: None,
            history_available: true,
            user_message: "hello",
            attachments: &[],
            refs: &[],
        }
        .render();
        assert!(rendered.ends_with("\n[user message]\nhello"));
        assert!(!rendered.contains("[attachments]"));
        assert!(!rendered.contains(ATTACHMENT_ONLY_PLACEHOLDER));
    }

    #[test]
    fn unknown_kinds_are_dropped_and_legacy_text_gets_a_role() {
        let rows = vec![
            row(1, "mystery", "u1", "?"),
            row(2, "text", "u1", "legacy human"),
            row(3, "text", "a1", "legacy agent"),
        ];
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &WindowBudget::default());
        assert_eq!(block.messages.len(), 2);
        assert!(block.messages[0].content.starts_with("[user:u1]"));
        assert!(block.messages[1].content.starts_with("[agent:a1]"));
    }

    #[test]
    fn block_b_marks_its_last_message_as_a_cache_breakpoint() {
        let rows = vec![row(1, "user_message", "u1", "a"), row(2, "user_message", "u1", "b")];
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &WindowBudget::default());
        assert!(block.messages[0].cache_control.is_none());
        assert!(matches!(block.messages[1].cache_control, Some(CacheControl::Ephemeral)));
    }

    #[test]
    fn block_c_carries_the_turn_view_and_the_message_once() {
        let candidates = vec![
            CandidateLine {
                alias: "S1".into(),
                current: true,
                status: "working".into(),
                age: "2m".into(),
                origin: "chat".into(),
                title: "Fix login redirect".into(),
            },
            CandidateLine {
                alias: "S2".into(),
                current: false,
                status: "idle".into(),
                age: "3h".into(),
                origin: "task".into(),
                title: "Review ADR-098".into(),
            },
        ];
        let ctx = TurnContext {
            surface: "group",
            speaker: "alice",
            now: "2026-10-21T14:03:00+08:00",
            draft: None,
            candidates: &candidates,
            task: None,
            history_available: true,
            user_message: "继续弄一下登录",
            attachments: &[],
            refs: &[],
        };
        let text = ctx.render();
        assert!(text.starts_with("[runtime context]\nsurface: group\nspeaker: alice\n"));
        assert!(text.contains("draft: (none)\n"));
        assert!(!text.contains("history: unavailable"));
        let candidate_lines: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "[candidates]")
            .skip(1)
            .take_while(|l| !l.is_empty())
            .collect();
        assert_eq!(candidate_lines.len(), 2);
        let s1: Vec<&str> = candidate_lines[0].split_whitespace().collect();
        assert_eq!(
            s1,
            vec!["S1", "current", "working", "2m", "chat", "Fix", "login", "redirect"]
        );
        let s2: Vec<&str> = candidate_lines[1].split_whitespace().collect();
        assert_eq!(s2, vec!["S2", "idle", "3h", "task", "Review", "ADR-098"]);
        assert!(text.ends_with("\n[user message]\n继续弄一下登录"));
        assert_eq!(text.matches("继续弄一下登录").count(), 1);
    }

    #[test]
    fn block_c_flags_unavailable_history_and_empty_candidates() {
        let ctx = TurnContext {
            surface: "dm",
            speaker: "alice",
            now: "2026-10-21T14:03:00+08:00",
            draft: Some("model=claude-sonnet, workspace=ws-1"),
            candidates: &[],
            task: Some("project=proj task=T-12"),
            history_available: false,
            user_message: "hi",
            attachments: &[],
            refs: &[],
        };
        let text = ctx.render();
        assert!(text.contains("draft: model=claude-sonnet, workspace=ws-1"));
        assert!(text.contains("history: unavailable"));
        assert!(text.contains("[candidates]\n(none)\n"));
        assert!(text.contains("[task]\nproject=proj task=T-12\n"));
    }

    #[test]
    fn relative_age_is_coarse_and_monotonic() {
        assert_eq!(relative_age(1000, 990), "0m");
        assert_eq!(relative_age(1000, 1000 - 120), "2m");
        assert_eq!(relative_age(10_000, 10_000 - 3 * 3600), "3h");
        assert_eq!(relative_age(10_000, 10_000 - 2 * 86_400), "2d");
        // A clock skew must not produce a negative age.
        assert_eq!(relative_age(100, 200), "0m");
    }

    #[test]
    fn prefix_hash_ignores_nothing_and_stays_stable() {
        let msgs = vec![ChatMessage::user("a")];
        let h1 = prefix_hash("A", "T", &msgs);
        assert_eq!(h1.len(), 16);
        assert_eq!(h1, prefix_hash("A", "T", &msgs));
        assert_ne!(h1, prefix_hash("A", "T2", &msgs));
        assert_ne!(h1, prefix_hash("A", "T", &[ChatMessage::user("b")]));
        // Appending one message changes the hash but keeps the old bytes a
        // prefix, which is exactly what provider prefix caching keys on.
        let mut longer = msgs.clone();
        longer.push(ChatMessage::user("b"));
        assert_ne!(prefix_hash("A", "T", &longer), h1);
    }

    #[test]
    fn history_parsing_sorts_and_tolerates_garbage() {
        let body = serde_json::json!({
            "messages": [
                {"seq": 2, "kind": "user_message", "from": "u", "body": "b"},
                {"seq": 1, "kind": "user_message", "from": "u", "body": "a"},
                {"seq": "not-a-number"}
            ]
        });
        let rows = parse_history(&body).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].seq, 1);
        assert!(parse_history(&serde_json::json!({})).is_err());
    }
}
