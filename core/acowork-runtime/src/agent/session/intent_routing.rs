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
//! never collide: `chat:{chat_session_id}` / `conv:{conversation_id}`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One routing entry: the execution session currently holding this context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentSessionEntry {
    pub key: String,
    pub session_id: String,
    /// RFC3339 creation stamp (bookkeeping only; no rotation logic lives here
    /// in Phase 2 — physical rotation is a later milestone of §3.2.3).
    pub created_at: String,
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
        self.entries.insert(
            key.clone(),
            IntentSessionEntry {
                key,
                session_id,
                created_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        self.flush()
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
/// Chat session (group chats address one session per chat) > conversation
/// (C/S follow-ups) > message id (one-shot, never re-keyed).
pub fn routing_key(chat_session_id: &str, conversation_id: &str, message_id: &str) -> String {
    if !chat_session_id.is_empty() {
        format!("chat:{}", chat_session_id)
    } else if !conversation_id.is_empty() {
        format!("conv:{}", conversation_id)
    } else {
        format!("msg:{}", message_id)
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
}
