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
//! *excluded* from the front of the window). While the window stays under
//! `max_history + slack` the anchor never moves, so consecutive turns append
//! one message each and reuse the whole prefix; once the threshold trips the
//! window is rebuilt down to `retain` rows in one step.

use std::collections::HashSet;

use acowork_core::providers::traits::{CacheControl, ChatMessage};
use serde::Deserialize;
use serde_json::Value;

use super::prompt::truncate_utf8;

/// Default history window (design 29 §4.4).
pub const DEFAULT_MAX_HISTORY: usize = 24;
/// Extra rows tolerated before the window is rebuilt.
pub const DEFAULT_SLACK: usize = 8;
/// Rows kept after a rebuild.
pub const DEFAULT_RETAIN: usize = 16;
/// Rows the Gateway serves per `GET /api/intents/chat-history` call.
pub const GATEWAY_MAX_PAGE: usize = 50;
/// Per-row body cap (`max_message_bytes`).
pub const DEFAULT_MAX_MESSAGE_BYTES: usize = 2048;
/// Attachment metadata cap per row.
pub const ATTACHMENT_META_MAX_BYTES: usize = 512;
/// Fixed truncation marker.
pub const TRUNCATION_SUFFIX: &str = "…[truncated]";

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
    pub attachments: Vec<RowAttachment>,
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
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RowInteraction {
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RowAttachment {
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub mime: String,
    #[serde(default)]
    pub size: u64,
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

/// Window + per-row caps, constructed from `agent_config.json`
/// `chat_router` (already clamped by `ChatRouterConfig`).
#[derive(Debug, Clone, Copy)]
pub struct WindowBudget {
    pub max_history: usize,
    pub slack: usize,
    pub retain: usize,
    pub max_message_bytes: usize,
}

impl Default for WindowBudget {
    fn default() -> Self {
        Self {
            max_history: DEFAULT_MAX_HISTORY,
            slack: DEFAULT_SLACK,
            retain: DEFAULT_RETAIN,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        }
    }
}

impl WindowBudget {
    /// Build from the configured `history_window`.
    ///
    /// `max_history` is capped below the Gateway page cap: the whole window
    /// must fit in one page, otherwise the window silently falls off the
    /// page's oldest end each turn — the prefix would shrink from the front
    /// and every turn would miss the cache, exactly what the anchor exists
    /// to prevent.
    pub fn from_history_window(window: usize) -> Self {
        let mut budget = Self::default();
        let ceiling = GATEWAY_MAX_PAGE.saturating_sub(budget.slack + 2).max(1);
        budget.max_history = window.clamp(1, ceiling);
        budget.retain = budget.retain.min(budget.max_history);
        budget
    }

    /// Rows requested per turn: the window, one turn of growth, and the
    /// current message (which Block B excludes but the page still holds).
    pub fn page_size(&self) -> usize {
        (self.max_history + self.slack + 2).min(GATEWAY_MAX_PAGE)
    }

    /// Total byte ceiling for Block B. `max_history` rows at the per-row cap
    /// is the worst case; the byte pass only ever bites past it.
    pub fn max_bytes(&self) -> usize {
        self.max_history.saturating_mul(self.max_message_bytes).max(1024)
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
    /// Sum of projected content lengths — approximate, for the log only.
    pub byte_len: usize,
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
/// Trimming order is count first, then bytes (design 29 §4.4). Both move the
/// anchor to the seq of the last dropped row, so the boundary survives a
/// restart as long as the caller persists it.
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

    let threshold = budget.max_history.saturating_add(budget.slack);
    if window.len() > threshold {
        let retain = budget.retain.min(window.len());
        let drop_count = window.len() - retain;
        if drop_count > 0 {
            anchor = window[drop_count - 1].seq.max(anchor);
            window.drain(..drop_count);
            rebuilt = true;
        }
    }

    let mut projected: Vec<(u64, ChatMessage)> = window
        .iter()
        .filter_map(|row| project_row(row, agent_instance).map(|m| (row.seq, m)))
        .collect();

    // Byte backstop: drop from the front until the block fits. Rare (only
    // very long rows), hence kept after the count pass so the common path
    // never pays for it.
    let mut bytes: usize = projected.iter().map(|(_, m)| m.content.len()).sum();
    let mut dropped = 0usize;
    while bytes > budget.max_bytes() && projected.len() - dropped > 1 {
        bytes -= projected[dropped].1.content.len();
        anchor = anchor.max(projected[dropped].0);
        dropped += 1;
        rebuilt = true;
    }
    projected.drain(..dropped);

    let mut messages: Vec<ChatMessage> = projected.into_iter().map(|(_, m)| m).collect();
    // Anthropic breakpoint at the end of the append-only prefix (design 29
    // §6.1). Providers with automatic prefix caching ignore the field.
    if let Some(last) = messages.last_mut() {
        last.cache_control = Some(CacheControl::Ephemeral);
    }

    BlockB {
        anchor_seq: anchor,
        messages,
        rebuilt,
        byte_len: bytes,
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
        ))),
        "agent_message" => Some(ChatMessage::assistant(prefix_body(
            &format!("[agent:{}]", row.from),
            &row.body,
            &row.attachments,
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
            let content = prefix_body(&format!("{tag}:{}]", row.from), &row.body, &row.attachments);
            Some(if role_is_agent {
                ChatMessage::assistant(content)
            } else {
                ChatMessage::user(content)
            })
        }
        _ => None,
    }
}

/// Fixed notice texts (design 29 §4.3). `turn_started` embeds the first 8
/// characters of the execution session id, which is durable in the row meta.
fn notice_text(row: &ChatRow) -> String {
    match row.meta.notice_type.as_str() {
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

/// `<tag> <body>` with the attachment metadata appended, both bounded.
fn prefix_body(tag: &str, body: &str, attachments: &[RowAttachment]) -> String {
    let mut out = String::with_capacity(tag.len() + body.len() + 8);
    out.push_str(tag);
    out.push(' ');
    out.push_str(&truncate_body(body, DEFAULT_MAX_MESSAGE_BYTES));
    if let Some(meta) = attachments_line(attachments) {
        out.push_str("\n[attachments] ");
        out.push_str(&meta);
    }
    out
}

/// `report.pdf (application/pdf, 12KB), screenshot.png (image/png, 340KB)`,
/// capped at [`ATTACHMENT_META_MAX_BYTES`]. Entries that do not fit are
/// dropped whole so the line never ends mid-entry.
fn attachments_line(attachments: &[RowAttachment]) -> Option<String> {
    if attachments.is_empty() {
        return None;
    }
    let mut out = String::new();
    for att in attachments {
        let entry = format!(
            "{}{} ({}, {})",
            if out.is_empty() { "" } else { ", " },
            sanitize_inline(&att.filename),
            sanitize_inline(&att.mime),
            human_size(att.size)
        );
        if out.len() + entry.len() > ATTACHMENT_META_MAX_BYTES {
            break;
        }
        out.push_str(&entry);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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
}

impl TurnContext<'_> {
    /// Render the final `user` message: `[runtime context]` … blank line …
    /// `[user message]`.
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
        out.push_str(self.user_message);
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
        let block = build_block_b(&[card, notice], 0, &ids(&[]), "a1", &WindowBudget::default());
        assert_eq!(block.messages[0].content, "[card] Proceed?\n- yes\n- no");
        assert_eq!(block.messages[1].content, "[system] A round started in session 01234567");
    }

    #[test]
    fn anchor_stays_put_until_the_slack_threshold_trips() {
        let budget = WindowBudget::default();
        let rows: Vec<ChatRow> = (1..=30).map(|i| row(i, "user_message", "u1", "x")).collect();
        // 30 rows <= 24 + 8: nothing is dropped, the anchor does not move.
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &budget);
        assert_eq!(block.messages.len(), 30);
        assert_eq!(block.anchor_seq, 0);
        assert!(!block.rebuilt);

        // One more row trips the threshold: rebuild down to `retain` rows.
        let rows: Vec<ChatRow> = (1..=33).map(|i| row(i, "user_message", "u1", "x")).collect();
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &budget);
        assert_eq!(block.messages.len(), budget.retain);
        assert_eq!(block.anchor_seq, 33 - budget.retain as u64);
        assert!(block.rebuilt);

        // The next turn appends exactly one message on top of the rebuilt
        // window — the prefix is stable again.
        let rows2: Vec<ChatRow> = (1..=34).map(|i| row(i, "user_message", "u1", "x")).collect();
        let next = build_block_b(&rows2, block.anchor_seq, &ids(&[]), "a1", &budget);
        assert_eq!(next.messages.len(), budget.retain + 1);
        assert_eq!(next.anchor_seq, block.anchor_seq);
    }

    #[test]
    fn configured_window_always_fits_one_page() {
        let budget = WindowBudget::from_history_window(200);
        assert!(budget.page_size() >= budget.max_history);
        assert!(budget.page_size() <= GATEWAY_MAX_PAGE);
        assert_eq!(budget.retain, budget.max_history.min(DEFAULT_RETAIN));

        let tiny = WindowBudget::from_history_window(1);
        assert_eq!(tiny.max_history, 1);
        assert_eq!(tiny.retain, 1);
    }

    #[test]
    fn byte_budget_drops_from_the_front() {
        let budget = WindowBudget {
            max_history: 3,
            slack: 3,
            retain: 3,
            max_message_bytes: 1000,
        };
        // Four 900-byte rows stay under the count threshold (4 <= 6) but
        // exceed the 3000-byte ceiling, so only the byte pass can save us.
        let body = "x".repeat(900);
        let rows: Vec<ChatRow> = (1..=4).map(|i| row(i, "user_message", "u1", &body)).collect();
        let block = build_block_b(&rows, 0, &ids(&[]), "a1", &budget);
        assert_eq!(block.messages.len(), 3);
        assert_eq!(block.anchor_seq, 1);
        assert!(block.rebuilt);
        assert!(block.byte_len <= budget.max_bytes());
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
            RowAttachment {
                filename: "report.pdf".into(),
                mime: "application/pdf".into(),
                size: 12 * 1024,
            },
            RowAttachment {
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
