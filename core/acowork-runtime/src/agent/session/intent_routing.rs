//! Intent → execution-session routing (design doc 27 §3.2).
//!
//! The wire addresses an **intent context** (a `conversation_id`, or a Chat
//! session for group chats), not an execution session — "which execution
//! session answers this" is the receiving agent's private decision (the
//! Gateway must not carry agent-private state across the boundary). This
//! module keeps that mapping: an in-memory table backed by a small JSON file
//! under the runtime work dir (`intent-sessions.json`), so a runtime restart
//! can still route a follow-up turn to the session that holds the
//! conversation, instead of silently opening a fresh one.
//!
//! Keys are namespaced by kind so a chat-session id and a conversation id can
//! never collide: `chat:{chat_session_id}` / `conv:{conversation_id}` /
//! `task:{project}:{task}:{agent}`. The entry also carries the §2.1 rotation
//! state (T1 idle clock + T2 compaction marker); the rotation *decision*
//! happens at routing time in `SessionManager::route_intent_session`, since
//! the rotation point (turn ended, response written back) has passed before
//! the next intent arrives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One routing entry: the execution session currently holding this context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentSessionEntry {
    pub key: String,
    pub session_id: String,
    /// RFC3339 creation stamp of the *current* physical session (bumped on
    /// every rotation rekey).
    pub created_at: String,
    /// RFC3339 stamp of the last intent turn end (or routing touch). T1 idle
    /// rotation compares the next arrival against this (design 27 §2.1).
    pub last_active_at: String,
    /// T2 compaction marker (§2.1): set when at least one LLM compaction
    /// happened during the last intent turn. The next arrival rotates to a
    /// successor instead of continuing on this session.
    pub compacted: bool,
}

/// Why a mapped session must be rotated (§2.1). Checked at routing time —
/// the rotation point ("after the result was written back") has passed by
/// the moment the next intent arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RotateReason {
    /// T1: idle interval since last activity exceeds the threshold.
    T1Idle,
    /// T2: the previous turn compacted at least once (long-running task).
    T2Compaction,
}

/// Idle threshold for T1 rotation. Default 24h; `ACOWORK_INTENT_IDLE_ROTATE_HOURS`
/// overrides (1–72; `0` disables T1).
///
/// ponytail: the design exposes these two knobs (idle hours, handoff token
/// cap) through the session settings card; that UI is a later milestone, so
/// the env var is the interim config surface. Upgrade path: read from agent
/// config once the settings card lands.
pub fn idle_rotate_threshold() -> Option<std::time::Duration> {
    let hours: u64 = std::env::var("ACOWORK_INTENT_IDLE_ROTATE_HOURS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(24);
    if hours == 0 {
        None
    } else {
        Some(std::time::Duration::from_secs(hours.min(72) * 3600))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FileShape(HashMap<String, IntentSessionEntry>);

/// In-memory map + JSON file mirror. Single-writer semantics are not needed
/// beyond the process lock: writes are tiny and rare (one per new context).
pub struct IntentSessionMap {
    path: PathBuf,
    entries: HashMap<String, IntentSessionEntry>,
}

impl IntentSessionMap {
    /// Load from `dir/intent-sessions.json`; a missing or corrupt file starts
    /// empty (the file is a routing hint, never a source of truth — a stale
    /// entry only costs one session re-open attempt).
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("intent-sessions.json");
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<FileShape>(&b).ok())
            .map(|f| f.0)
            .unwrap_or_default();
        Self { path, entries }
    }

    pub fn get(&self, key: &str) -> Option<&IntentSessionEntry> {
        self.entries.get(key)
    }

    pub fn insert(&mut self, key: String, session_id: String) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.entries.insert(
            key.clone(),
            IntentSessionEntry {
                key,
                session_id,
                created_at: now.clone(),
                last_active_at: now,
                compacted: false,
            },
        );
        self.flush()
    }

    /// Point a key at a successor session after a rotation (§2.1). Resets the
    /// compaction marker and the activity stamps.
    pub fn rekey(&mut self, key: &str, session_id: String) -> Result<()> {
        self.insert(key.to_string(), session_id)
    }

    /// Record the end of an intent turn on this key: refreshes the T1 idle
    /// clock and carries the T2 compaction marker forward.
    pub fn note_turn_end(&mut self, key: &str, compacted: bool) -> Result<()> {
        if let Some(e) = self.entries.get_mut(key) {
            e.last_active_at = chrono::Utc::now().to_rfc3339();
            e.compacted = compacted;
            return self.flush();
        }
        Ok(())
    }

    /// Rotation decision for an incoming intent against the mapped entry.
    /// T2 (compaction) is checked first — it is a business boundary, not a
    /// timing heuristic, and must win when both apply.
    pub fn needs_rotation(&self, key: &str, idle: Option<std::time::Duration>) -> Option<RotateReason> {
        let e = self.entries.get(key)?;
        if e.compacted {
            return Some(RotateReason::T2Compaction);
        }
        let idle = idle?;
        let last = chrono::DateTime::parse_from_rfc3339(&e.last_active_at).ok()?;
        let elapsed = chrono::Utc::now().signed_duration_since(last);
        match elapsed.to_std() {
            Ok(d) if d >= idle => Some(RotateReason::T1Idle),
            _ => None,
        }
    }

    /// Drop a mapping (PM task reached a terminal state → the logical
    /// execution context is closed, no successor; design 27 §2.1 T2-a).
    pub fn remove(&mut self, key: &str) -> Result<()> {
        if self.entries.remove(key).is_some() {
            return self.flush();
        }
        Ok(())
    }

    fn flush(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let shape = FileShape(self.entries.clone());
        let json = serde_json::to_string(&shape)
            .map_err(|e| crate::error::RuntimeError::Config(e.to_string()))?;
        // Atomic rewrite (temp + rename), same contract as the mailbox.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| crate::error::RuntimeError::Config(e.to_string()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| crate::error::RuntimeError::Config(e.to_string()))
    }
}

/// Derive the routing key from an inbound intent envelope. Preference:
/// task-scoped conversation (§6.2: one execution session chain per PM task,
/// even when the task lives in a shared project group chat) > Chat session
/// (group chats address one session per chat) > conversation (C/S
/// follow-ups) > message id (one-shot, never re-keyed).
pub fn routing_key(chat_session_id: &str, conversation_id: &str, message_id: &str) -> String {
    if conversation_id.starts_with("task:") {
        format!("conv:{}", conversation_id)
    } else if !chat_session_id.is_empty() {
        format!("chat:{}", chat_session_id)
    } else if !conversation_id.is_empty() {
        format!("conv:{}", conversation_id)
    } else {
        format!("msg:{}", message_id)
    }
}

/// Pointer handoff note prepended to the first intent routed into a
/// successor session after a rotation (§2.1).
///
/// ponytail: the design's distilled handoff (open items / key decisions,
/// 500-token T1 / 1,500-token T2 caps) needs an LLM distillation call at
/// rotation time. The interim form is a pointer only — the old session's
/// full history (and its compaction summary) stays in Session history, and
/// the conversation itself lives in the Chat session store, so the
/// invariant "never copy the old JSONL wholesale" holds. Upgrade path:
/// distill from the old session's last compaction summary when the
/// settings card exposes the token caps.
pub fn rotation_handoff(reason: RotateReason, old_session_id: &str) -> String {
    match reason {
        RotateReason::T1Idle => format!(
            "[intent rotation] The previous execution session for this context (session id \"{old}\") was rotated after an idle interval past the T1 threshold; no open items were recorded for carry-over. Its full history stays available under that session id.",
            old = old_session_id
        ),
        RotateReason::T2Compaction => format!(
            "[intent rotation] The previous execution session for this context (session id \"{old}\") was rotated after an in-turn compaction (T2: long-running task). Its full history and compaction summary stay under that session id.",
            old = old_session_id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_by_preference() {
        assert_eq!(
            routing_key("cs1", "c1", "m1"),
            "chat:cs1".to_string(),
            "chat session wins"
        );
        assert_eq!(routing_key("", "c1", "m1"), "conv:c1".to_string());
        assert_eq!(routing_key("", "", "m1"), "msg:m1".to_string());
    }

    #[test]
    fn task_conversation_beats_the_chat_binding() {
        // §6.2: one execution session per (task, agent) even inside the
        // project group chat.
        assert_eq!(
            routing_key("cs1", "task:p1:t1", "m1"),
            "conv:task:p1:t1".to_string()
        );
    }

    #[test]
    fn map_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("intent-map-{}", uuid::Uuid::new_v4()));
        {
            let mut m = IntentSessionMap::load(&dir);
            m.insert("conv:c1".into(), "s-1".into()).unwrap();
        }
        let m2 = IntentSessionMap::load(&dir);
        assert_eq!(m2.get("conv:c1").unwrap().session_id, "s-1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t2_compaction_marker_forces_rotation_and_survives_reload() {
        let dir = std::env::temp_dir().join(format!("intent-map-{}", uuid::Uuid::new_v4()));
        let mut m = IntentSessionMap::load(&dir);
        m.insert("conv:c1".into(), "s-1".into()).unwrap();
        // Fresh entry, no idle threshold: no rotation.
        assert_eq!(m.needs_rotation("conv:c1", None), None);
        // Turn ends with a compaction → T2 fires regardless of idle config.
        m.note_turn_end("conv:c1", true).unwrap();
        let reloaded = IntentSessionMap::load(&dir);
        assert_eq!(
            reloaded.needs_rotation("conv:c1", Some(std::time::Duration::from_secs(24 * 3600))),
            Some(RotateReason::T2Compaction)
        );
        // Rotating (rekey) clears the marker.
        let mut m = reloaded;
        m.rekey("conv:c1", "s-2".into()).unwrap();
        assert_eq!(m.needs_rotation("conv:c1", None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn t1_idle_rotation_uses_last_active_stamp() {
        let dir = std::env::temp_dir().join(format!("intent-map-{}", uuid::Uuid::new_v4()));
        let mut m = IntentSessionMap::load(&dir);
        m.insert("chat:cs1".into(), "s-1".into()).unwrap();
        // Backdate activity 30h; threshold 24h → T1.
        let e = m.entries.get_mut("chat:cs1").unwrap();
        e.last_active_at = (chrono::Utc::now() - chrono::Duration::hours(30)).to_rfc3339();
        assert_eq!(
            m.needs_rotation("chat:cs1", Some(std::time::Duration::from_secs(24 * 3600))),
            Some(RotateReason::T1Idle)
        );
        // Threshold disabled (None) → no T1.
        assert_eq!(m.needs_rotation("chat:cs1", None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove_drops_the_mapping() {
        let dir = std::env::temp_dir().join(format!("intent-map-{}", uuid::Uuid::new_v4()));
        let mut m = IntentSessionMap::load(&dir);
        m.insert("conv:c9".into(), "s-9".into()).unwrap();
        m.remove("conv:c9").unwrap();
        assert!(m.get("conv:c9").is_none());
        assert!(IntentSessionMap::load(&dir).get("conv:c9").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
