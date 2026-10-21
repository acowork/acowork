//! Append-only Chat Router decision log (ADR-099 §5, design 29 §7).
//!
//! One JSON object per router turn in `{work_dir}/logs/chat-router-decisions.jsonl`.
//! The log is an observability surface, not state: nothing reads it back for
//! routing, and a missing/locked file must never fail a turn. Every write is
//! therefore best-effort and returns whether it landed, so the caller can log
//! a warning without changing behaviour.
//!
//! Invariants:
//! - fields are additive; unknown fields on older lines are ignored by any
//!   reader (serde defaults);
//! - the log holds routing metadata only — never message bodies, never
//!   session JSONL content (ADR-096).

use std::path::Path;

use serde::{Deserialize, Serialize};

/// One line of `chat-router-decisions.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// RFC3339 timestamp of the decision.
    pub ts: String,
    /// Chat session the turn belongs to.
    pub chat_id: String,
    /// Durable `user_message` row id this turn answers.
    pub message_id: String,
    /// `user_agent` / `user_group` / `project_group` — from
    /// `params_json.chat_kind`, never inferred.
    pub chat_kind: String,
    /// Model id actually used for the router call.
    pub model: String,
    /// Provider id serving `model`.
    pub provider: String,
    /// Aliases shown to the model, in display order (`S1`, `S2`, …).
    pub candidates: Vec<String>,
    /// `reply` / `dispatch:S1` / `dispatch:new` / `fallback`.
    pub decision: String,
    /// Resolved target session for `dispatch:*`; `None` for a reply or a
    /// fallback that never reached a session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_session_id: Option<String>,
    /// Wall-clock duration of the router call (including one tool retry).
    pub latency_ms: u64,
    /// `ok` / `error`.
    pub outcome: String,
    /// Why the turn degraded to the legacy route, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// Hash over `Block A bytes + tools schema bytes + Block B bytes`
    /// (design 29 §6.4) — stable across steady-state turns.
    #[serde(default)]
    pub prefix_hash: String,
    /// Absolute chat offset the history window started at.
    #[serde(default)]
    pub anchor_seq: usize,
    /// Number of projected history rows (excluding the current message).
    #[serde(default)]
    pub history_len: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_tokens: Option<u64>,
}

impl DecisionRecord {
    /// Short human-readable form for the `tracing` line that accompanies
    /// every file write. Not a replacement for the JSONL.
    pub fn summary(&self) -> String {
        let target = self
            .target_session_id
            .as_deref()
            .map(|s| format!(" -> {s}"))
            .unwrap_or_default();
        format!(
            "chat={} decision={}{} latency={}ms outcome={}",
            self.chat_id, self.decision, target, self.latency_ms, self.outcome
        )
    }
}

/// Append one record to `{work_dir}/logs/chat-router-decisions.jsonl`.
///
/// Best-effort by contract (see module docs): returns an error the caller
/// logs, never a failure that would abort the turn.
pub fn append_decision(work_dir: &Path, rec: &DecisionRecord) -> std::io::Result<()> {
    use std::io::Write as _;

    let dir = work_dir.join("logs");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("chat-router-decisions.jsonl");
    let line = serde_json::to_string(rec)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record() -> DecisionRecord {
        DecisionRecord {
            ts: "2026-10-21T14:03:00+08:00".into(),
            chat_id: "cs-1".into(),
            message_id: "m-1".into(),
            chat_kind: "user_agent".into(),
            model: "compact-x".into(),
            provider: "p1".into(),
            candidates: vec!["S1".into(), "S2".into()],
            decision: "dispatch:S2".into(),
            target_session_id: Some("sess-2".into()),
            latency_ms: 412,
            outcome: "ok".into(),
            fallback_reason: None,
            prefix_hash: "9f2c1a7b".into(),
            anchor_seq: 40,
            history_len: 18,
            prompt_tokens: Some(3120),
            cache_read_tokens: Some(2816),
            cache_write_tokens: Some(0),
        }
    }

    #[test]
    fn append_writes_one_json_line_per_record() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-router-log-{}",
            uuid::Uuid::new_v4()
        ));
        append_decision(&dir, &record()).unwrap();
        append_decision(&dir, &record()).unwrap();
        let text = std::fs::read_to_string(dir.join("logs/chat-router-decisions.jsonl")).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let parsed: DecisionRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(parsed.decision, "dispatch:S2");
        assert_eq!(parsed.target_session_id.as_deref(), Some("sess-2"));
        assert_eq!(parsed.anchor_seq, 40);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_is_forward_compatible_with_missing_optional_fields() {
        // An older line without the design-29 fields must still parse.
        let line = r#"{
            "ts":"2026-10-20T00:00:00Z","chat_id":"c","message_id":"m",
            "chat_kind":"user_agent","model":"m","provider":"p","candidates":[],
            "decision":"reply","latency_ms":1,"outcome":"ok"
        }"#;
        let parsed: DecisionRecord = serde_json::from_str(line).unwrap();
        assert_eq!(parsed.anchor_seq, 0);
        assert_eq!(parsed.history_len, 0);
        assert_eq!(parsed.prefix_hash, "");
    }
}
