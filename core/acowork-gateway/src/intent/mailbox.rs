//! Intent mailbox — the minimal delivery buffer (tech-debt plan D3).
//!
//! A delivery buffer, **not** a workflow engine (plan §8 decision 3): it
//! records the facts the sender and the chat model need —
//! `message_id / conversation_id / from / to / body / created_at /
//! delivered_at / result_ref` — and the per-`conversation_id` request
//! count. Nothing else lives here; no state machine, no retries, no
//! orchestration.
//!
//! Persistence is a JSON file in the Gateway data dir (same shape as
//! [`crate::cron::store::CronStore`], atomic rewrite on every mutation).
//! Two facts make persistence mandatory rather than optional:
//!
//! - `intent_send` must report a truthful delivery state, and
//! - design doc 27 §13-13: the `conversation_id` request count must
//!   survive a Gateway restart, otherwise the conversation budget
//!   (§6.5 R4) can be bypassed by restarting the Gateway.
//!
//! ponytail: the entry list is capped (FIFO drop of the oldest entries);
//! counts are never dropped. If a deployment needs the full message
//! history, that belongs in the `acowork-user` chat store (design doc
//! 27 Phase 1), not here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Maximum number of mailbox entries retained (delivery facts, not
/// history). Counts live in a separate map and are never evicted.
const MAX_ENTRIES: usize = 10_000;

/// One delivered (or attempted) Intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEntry {
    /// Stable message identity (design doc 27 §4 `message_id`).
    pub message_id: String,
    /// Conversation this request belongs to (§6.5). Empty string for
    /// pre-conversation senders (cron triggers).
    #[serde(default)]
    pub conversation_id: String,
    /// Sender instance id, Gateway-stamped (§D4). Cron uses `cron:<id>`.
    pub from_instance: String,
    /// Sender execution session id when the sender is an Agent
    /// (`source_agent_session_id`); empty for user/cron originators.
    #[serde(default)]
    pub from_session_id: String,
    /// Target instance id.
    pub to_instance: String,
    /// The message body handed to the target (action + params JSON).
    pub body: String,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// RFC 3339 timestamp of the successful MQTT publish; empty until
    /// delivered. An entry with `delivered_at` set is the "已投递" fact.
    #[serde(default)]
    pub delivered_at: String,
    /// Response association (design doc 27 §4 `in_reply_to`): for a
    /// response entry, the request `message_id` it answers. Empty on
    /// requests until their response is routed back.
    #[serde(default)]
    pub result_ref: String,
    /// "request" (default) | "response" (§6.5 R2).
    #[serde(default)]
    pub kind: String,
    /// For a response: §7.1 outcome (completed/failed/aborted/timeout).
    #[serde(default)]
    pub status: String,
    /// Source Chat session this message belongs to (§4
    /// `source_chat_session_id`); the reply address. Empty for
    /// chat-less sends (cron, plain agent-to-agent before pairing).
    #[serde(default)]
    pub source_chat_session_id: String,
}

impl MailboxEntry {
    pub fn delivered(&self) -> bool {
        !self.delivered_at.is_empty()
    }
}

/// Persisted mailbox state.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MailboxData {
    entries: Vec<MailboxEntry>,
    /// `conversation_id` → accepted request count. Survives restart by
    /// construction (§13-13); never evicted with the entries.
    conversation_requests: HashMap<String, u32>,
}

/// Thread-safe persisted Intent mailbox.
#[derive(Debug)]
pub struct IntentMailbox {
    inner: Mutex<MailboxData>,
    path: Option<PathBuf>,
}

/// Error type mirroring the cron store's shape.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    #[error("io: {0}")]
    Io(String),
    #[error("serialization: {0}")]
    Serialize(String),
}

impl IntentMailbox {
    /// Open (or create) the mailbox at `{data_dir}/intent_mailbox.json`.
    pub fn open(path: &Path) -> Self {
        let data = if path.exists() {
            std::fs::read_to_string(path)
                .map(|s| serde_json::from_str(&s).unwrap_or_default())
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "intent mailbox unreadable — starting fresh");
                    MailboxData::default()
                })
        } else {
            MailboxData::default()
        };
        Self {
            inner: Mutex::new(data),
            path: Some(path.to_path_buf()),
        }
    }

    /// In-memory mailbox (tests, or no data dir configured).
    pub fn in_memory() -> Self {
        Self {
            inner: Mutex::new(MailboxData::default()),
            path: None,
        }
    }

    /// Record a new Intent (status: not yet delivered) and count it
    /// against its conversation. Returns the entry snapshot.
    ///
    /// §6.5 R4: only `request` messages count against the conversation
    /// limit — a response closes a round, it does not extend it. An
    /// empty `kind` reads as request (cron / pre-Phase-2 producers).
    pub fn record(&self, entry: MailboxEntry) -> Result<MailboxEntry, MailboxError> {
        let mut inner = self.inner.lock().unwrap();
        if !entry.conversation_id.is_empty() && entry.kind != "response" {
            *inner
                .conversation_requests
                .entry(entry.conversation_id.clone())
                .or_insert(0) += 1;
        }
        inner.entries.push(entry.clone());
        if inner.entries.len() > MAX_ENTRIES {
            let excess = inner.entries.len() - MAX_ENTRIES;
            inner.entries.drain(..excess);
        }
        self.save_locked(&inner)?;
        Ok(entry)
    }

    /// Stamp the delivery fact after a successful publish.
    pub fn mark_delivered(&self, message_id: &str) -> Result<(), MailboxError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(e) = inner.entries.iter_mut().find(|e| e.message_id == message_id) {
            e.delivered_at = chrono::Utc::now().to_rfc3339();
            self.save_locked(&inner)?;
        }
        Ok(())
    }

    /// Look up one delivery fact by `message_id`.
    pub fn get(&self, message_id: &str) -> Option<MailboxEntry> {
        let inner = self.inner.lock().unwrap();
        inner.entries.iter().find(|e| e.message_id == message_id).cloned()
    }

    /// Requests already accepted for a conversation (§6.5 R4 gate).
    /// A never-seen conversation returns 0.
    pub fn conversation_request_count(&self, conversation_id: &str) -> u32 {
        let inner = self.inner.lock().unwrap();
        inner
            .conversation_requests
            .get(conversation_id)
            .copied()
            .unwrap_or(0)
    }

    fn save_locked(&self, data: &MailboxData) -> Result<(), MailboxError> {
        let Some(path) = &self.path else { return Ok(()); };
        let json = serde_json::to_string(data).map_err(|e| MailboxError::Serialize(e.to_string()))?;
        // Atomic rewrite: temp file + rename, same contract as the cron
        // store — a crash mid-write never truncates the mailbox.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| MailboxError::Io(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| MailboxError::Io(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, conv: &str) -> MailboxEntry {
        MailboxEntry {
            message_id: id.to_string(),
            conversation_id: conv.to_string(),
            from_instance: "a".into(),
            from_session_id: String::new(),
            to_instance: "b".into(),
            body: "{}".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            delivered_at: String::new(),
            result_ref: String::new(),
            kind: "request".into(),
            status: String::new(),
            source_chat_session_id: String::new(),
        }
    }

    #[test]
    fn record_then_mark_delivered() {
        let mb = IntentMailbox::in_memory();
        mb.record(entry("m1", "c1")).unwrap();
        assert!(!mb.get("m1").unwrap().delivered());
        mb.mark_delivered("m1").unwrap();
        assert!(mb.get("m1").unwrap().delivered());
    }

    #[test]
    fn conversation_count_survives_reload() {
        let dir = std::env::temp_dir().join(format!("acowork-mailbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("intent_mailbox.json");
        {
            let mb = IntentMailbox::open(&path);
            mb.record(entry("m1", "c1")).unwrap();
            mb.record(entry("m2", "c1")).unwrap();
            mb.record(entry("m3", "c2")).unwrap();
        }
        let mb = IntentMailbox::open(&path);
        assert_eq!(mb.conversation_request_count("c1"), 2, "§13-13: count must survive restart");
        assert_eq!(mb.conversation_request_count("c2"), 1);
        assert_eq!(mb.conversation_request_count("nope"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
