//! Intent → execution-session routing (design doc 27 §4.2.3).
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
//! `task:{project}:{task}:{agent}`. The entry is exactly that pair: a
//! long-lived execution session stays mapped until its context is explicitly
//! closed (`remove` on a PM task's terminal state) or the Chat Router points
//! the key at another session.
//!
//! There is no rotation. The former T1 (idle) / T2 (post-compaction) triggers
//! were machine rules from before an LLM decided routing (design 27 §2.1/§4.1,
//! retired by ADR-099's follow-up): a compacted session is designed to keep
//! accepting turns in place, and rotating cost the successor everything the
//! predecessor held beyond its ≤ `handoff` token summary — with no tool able to
//! read another execution session's history. Starting a fresh session is now
//! always an explicit decision: the router's `target="new"`, the PM task
//! lifecycle, or the user.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use acowork_core::intent::{is_task_conversation, TurnOrigin};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// One routing entry: the execution session currently holding this context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentSessionEntry {
    pub key: String,
    pub session_id: String,
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

    /// Every live mapping. Used by the Chat Router to read the routing table
    /// backwards: which session the chat currently points at, and which
    /// sessions belong to a PM task chain (`origin` in the candidate list).
    pub fn iter(&self) -> impl Iterator<Item = (&String, &IntentSessionEntry)> {
        self.entries.iter()
    }

    pub fn insert(&mut self, key: String, session_id: String) -> Result<()> {
        self.entries
            .insert(key.clone(), IntentSessionEntry { key, session_id });
        self.flush()
    }

    /// Drop a mapping (PM task reached a terminal state → the logical
    /// execution context is closed, design 27 §6.2).
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

/// One execution session whose PM task reached a terminal state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TerminalSession {
    pub session_id: String,
    /// RFC3339 close stamp.
    pub closed_at: String,
}

/// Cap on the persisted list. A terminal id only ever costs one candidate
/// slot, so the cap exists to keep the file cheap to read on every router
/// turn — an id old enough to fall off is long past any follow-up that could
/// name it.
const MAX_TERMINAL_SESSIONS: usize = 200;

#[derive(Debug, Default, Serialize, Deserialize)]
struct TerminalFile {
    #[serde(default)]
    sessions: Vec<TerminalSession>,
}

/// Session ids whose PM task reached a terminal state (design 27 §6.2,
/// ADR-099 D5): the Chat Router's candidate list must never offer a finished
/// task again — routing a follow-up into a closed context answers into a
/// conversation nobody reads.
///
/// Written by `SessionManager::close_intent_context`, which the Gateway calls
/// exactly once on the task's terminal transition; read by the candidate
/// builder. The PM service stays the authority on task status — this file
/// only mirrors the one event the Runtime observes. A missing or corrupt file
/// starts empty: a stale id costs one candidate slot, never a wrong route.
///
/// A `chat:` / `conv:` key is never recorded here: closing a conversation
/// context is not a completion, and under the no-rotation model nothing else
/// drops those mappings.
pub struct TerminalSessions {
    path: PathBuf,
    sessions: Vec<TerminalSession>,
}

impl TerminalSessions {
    /// Load from `dir/terminal-sessions.json`; absent / corrupt starts empty.
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("terminal-sessions.json");
        let sessions = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<TerminalFile>(&b).ok())
            .map(|f| f.sessions)
            .unwrap_or_default();
        Self { path, sessions }
    }

    pub fn contains(&self, session_id: &str) -> bool {
        self.sessions.iter().any(|s| s.session_id == session_id)
    }

    /// Set form for the candidate query.
    pub fn ids(&self) -> std::collections::HashSet<String> {
        self.sessions.iter().map(|s| s.session_id.clone()).collect()
    }

    /// Record a terminal session. Idempotent and bounded (oldest dropped
    /// first).
    pub fn record(&mut self, session_id: &str) -> Result<()> {
        if session_id.is_empty() || self.contains(session_id) {
            return Ok(());
        }
        self.sessions.push(TerminalSession {
            session_id: session_id.to_string(),
            closed_at: chrono::Utc::now().to_rfc3339(),
        });
        if self.sessions.len() > MAX_TERMINAL_SESSIONS {
            let excess = self.sessions.len() - MAX_TERMINAL_SESSIONS;
            self.sessions.drain(0..excess);
        }
        self.flush()
    }

    /// Drop an id that came back to life (a task reopened by the PM service).
    pub fn forget(&mut self, session_id: &str) -> Result<()> {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.session_id != session_id);
        if self.sessions.len() == before {
            return Ok(());
        }
        self.flush()
    }

    fn flush(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let shape = TerminalFile {
            sessions: self.sessions.clone(),
        };
        let json = serde_json::to_string(&shape)
            .map_err(|e| crate::error::RuntimeError::Config(e.to_string()))?;
        // Atomic rewrite (temp + rename), same contract as the routing table.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json)
            .map_err(|e| crate::error::RuntimeError::Config(e.to_string()))?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| crate::error::RuntimeError::Config(e.to_string()))
    }
}

/// Inbound Intent's optional `session_config` suggestion (design 27 §4.5
/// field table, §13-6: "只是应方允许范围内的建议"). `model`/`provider`
/// are clamped against the responder's provider list; `workspace` is
/// validated against the responder's workspace registry at creation
/// (an unknown id is dropped, never left to poison the session into the
/// pending-workspace state); `reasoning_effort` must parse as a known
/// level. Skills are deliberately NOT part of the seed: skills are
/// auto-loaded by the model, not session-seeded, so an initiator
/// suggestion must never name one. A `provider` without a `model` is
/// meaningless (the pair is suggested together) and dropped. Unknown
/// keys are ignored (additive fields land without a wire change).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSeed {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub workspace: Option<String>,
    pub reasoning_effort: Option<String>,
}

impl SessionSeed {
    /// Parse the wire JSON string. Empty / malformed / non-object input
    /// yields `None` (cron senders and pre-upgrade intents never set it;
    /// a bad suggestion must not block delivery — it degrades to the
    /// responder's own defaults).
    pub fn from_json(raw: &str) -> Option<Self> {
        if raw.trim().is_empty() {
            return None;
        }
        let obj = serde_json::from_str::<serde_json::Value>(raw).ok()?.as_object()?.clone();
        let get = |k: &str| {
            obj.get(k)
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let seed = Self {
            model: get("model"),
            provider: get("provider"),
            workspace: get("workspace"),
            // Only a level the enum understands may reach the session —
            // a garbage string would otherwise persist and resolve to None
            // on every later read.
            reasoning_effort: get("reasoning_effort").filter(|s| {
                acowork_core::providers::traits::ReasoningEffort::from_str_loose(s).is_some()
            }),
        };
        // A lone provider (no model) cannot pin anything — drop it.
        let seed = if seed.model.is_none() {
            Self {
                provider: None,
                ..seed
            }
        } else {
            seed
        };
        let empty = seed.model.is_none()
            && seed.provider.is_none()
            && seed.workspace.is_none()
            && seed.reasoning_effort.is_none();
        if empty {
            None
        } else {
            Some(seed)
        }
    }

    /// Clamp the model suggestion to the responder's allowed set (§13-6):
    /// the model must exist in the responder's provider list — under the
    /// suggested provider when one is given, otherwise under any provider
    /// that serves it (the provider is then pinned to the hit so the pair
    /// is never half-suggested). Only the (model, provider) pair degrades
    /// to the responder's defaults when nothing of it survives; `workspace`
    /// and `reasoning_effort` are provider-independent and pass through
    /// (workspace still gets validated against the resolver at creation).
    pub fn clamp_to_responder(
        self,
        providers: &[acowork_core::protocol::ProviderListItem],
    ) -> Option<Self> {
        let Self {
            model,
            provider,
            workspace,
            reasoning_effort,
        } = self;
        let mut clamped_model = None;
        let mut clamped_provider = None;
        if let Some(m) = model.as_deref() {
            let want_provider = provider.as_deref().filter(|p| !p.is_empty());
            for p in providers {
                let provider_ok = want_provider.is_none_or(|w| w == p.id);
                if provider_ok && p.models.iter().any(|mid| mid.id == m) {
                    clamped_model = Some(m.to_string());
                    clamped_provider = Some(p.id.clone());
                    break;
                }
            }
            if clamped_model.is_none() {
                tracing::warn!(
                    model = m,
                    provider = want_provider.unwrap_or(""),
                    "Intent session_config model suggestion outside the responder's allowed set — using defaults"
                );
            }
        }
        let out = Self {
            model: clamped_model,
            provider: clamped_provider,
            workspace,
            reasoning_effort,
        };
        let empty = out.model.is_none()
            && out.provider.is_none()
            && out.workspace.is_none()
            && out.reasoning_effort.is_none();
        if empty {
            None
        } else {
            Some(out)
        }
    }

    /// True when every field is `None` — nothing the initiator pinned
    /// survives clamping/validation, so the seed carries no opinion.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.provider.is_none()
            && self.workspace.is_none()
            && self.reasoning_effort.is_none()
    }
}

/// Derive the routing key from an inbound intent envelope. Preference:
/// task-scoped conversation (§6.2: one execution session per PM task, even
/// when the task lives in a shared project group chat) > Chat session
/// (group chats address one session per chat) > conversation (C/S
/// follow-ups) > message id (one-shot, never re-keyed).
///
/// The task branch is driven by the stamped origin
/// ([`TurnOrigin::PmTask`]), with the conversation-coordinate test kept as
/// the mixed-version fallback: an older Gateway still sends
/// `kind="request"` on a `task:` conversation, and mis-keying it onto the
/// group's chat session would break §6.2 for the whole task.
pub fn routing_key(
    kind: &str,
    chat_session_id: &str,
    initiator: &str,
    conversation_id: &str,
    message_id: &str,
) -> String {
    if TurnOrigin::parse(kind) == TurnOrigin::PmTask || is_task_conversation(conversation_id) {
        format!("conv:{}", conversation_id)
    } else if !chat_session_id.is_empty() {
        chat_key(chat_session_id, initiator)
    } else if !conversation_id.is_empty() {
        format!("conv:{}", conversation_id)
    } else {
        format!("msg:{}", message_id)
    }
}

/// The routing key for a Chat-surface turn: `chat:{chat_session_id}:{initiator}`.
///
/// The initiator is part of the key because an execution session has exactly
/// one initiator (ADR-076 §决策 4). Two people addressing the same agent in
/// the same group chat are two different pieces of work, and without the
/// suffix the second one would land inside the first one's session — the
/// multi-user session the design forbids. Empty initiator keeps the 2-part
/// form (local mode / legacy callers), which is also what old mappings on
/// disk look like.
///
/// Upgrade consequence, deliberate: a group's execution session that was
/// keyed before this change (`chat:{csid}`) is not found by the new 3-part
/// key, so the first mention after the upgrade opens a fresh session. The
/// old one stays in the session list as history. Falling back to the 2-part
/// key would re-open a session whose initiator may be somebody else — the
/// exact bug this key exists to prevent.
pub fn chat_key(chat_session_id: &str, initiator: &str) -> String {
    let initiator = initiator.trim();
    if initiator.is_empty() {
        format!("chat:{chat_session_id}")
    } else {
        format!("chat:{chat_session_id}:{initiator}")
    }
}

/// Recover the Chat session id from a [`chat_key`], old (2-part) or new
/// (3-part) form.
pub fn chat_id_from_key(key: &str) -> Option<&str> {
    let rest = key.strip_prefix("chat:")?;
    match rest.split_once(':') {
        Some((chat_id, _initiator)) => Some(chat_id),
        None => Some(rest),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_by_preference() {
        assert_eq!(
            routing_key("user_message", "cs1", "u-1", "c1", "m1"),
            "chat:cs1:u-1".to_string(),
            "chat session wins"
        );
        assert_eq!(routing_key("", "", "u-1", "c1", "m1"), "conv:c1".to_string());
        assert_eq!(routing_key("", "", "u-1", "", "m1"), "msg:m1".to_string());
    }

    #[test]
    fn task_conversation_beats_the_chat_binding() {
        // §6.2: one execution session per (task, agent) even inside the
        // project group chat. Origin-stamped dispatch:
        assert_eq!(
            routing_key("pm_task", "cs1", "u-1", "task:p1:t1", "m1"),
            "conv:task:p1:t1".to_string()
        );
        // Mixed-version fallback — an older Gateway still sends
        // `kind="request"` on the task conversation; the coordinate keeps
        // the key correct either way:
        assert_eq!(
            routing_key("request", "cs1", "u-1", "task:p1:t1", "m1"),
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
    fn remove_drops_the_mapping() {
        let dir = std::env::temp_dir().join(format!("intent-map-{}", uuid::Uuid::new_v4()));
        let mut m = IntentSessionMap::load(&dir);
        m.insert("conv:c9".into(), "s-9".into()).unwrap();
        m.remove("conv:c9").unwrap();
        assert!(m.get("conv:c9").is_none());
        assert!(IntentSessionMap::load(&dir).get("conv:c9").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_seed_parses_only_string_model_provider() {
        assert!(SessionSeed::from_json("").is_none());
        assert!(SessionSeed::from_json("not json").is_none());
        assert!(SessionSeed::from_json("[1,2]").is_none(), "non-object");
        assert!(
            SessionSeed::from_json(r#"{"temperature": 0.5}"#).is_none(),
            "recognized keys absent -> responder defaults"
        );
        let s = SessionSeed::from_json(r#"{"model":" m1 ","provider":"p1","skills":true}"#).unwrap();
        assert_eq!(s.model.as_deref(), Some("m1"));
        assert_eq!(s.provider.as_deref(), Some("p1"));
        let s2 = SessionSeed::from_json(r#"{"model":"m2"}"#).unwrap();
        assert_eq!(s2.provider, None);
    }

    #[test]
    fn session_seed_clamps_to_responder_allowed_set() {
        use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry};
        let caps = ModelCapabilitiesInfo {
            context_window: 100_000,
            max_output_tokens: 8192,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
            reasoning_efforts: None,
        };
        let entry = |id: &str| ProviderModelEntry {
            id: id.to_string(),
            capabilities: caps.clone(),
            max_output_tokens_limit: 0,
            protocol_type: None,
            base_url: None,
            protocol_probe: None,
        };
        let provider = |id: &str, models: Vec<ProviderModelEntry>| ProviderListItem {
            id: id.into(),
            base_url: String::new(),
            protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
            models,
            compact_model: None,
            custom: false,
        };
        let list = vec![
            provider("p1", vec![entry("m1")]),
            provider("p2", vec![entry("m2")]),
        ];
        // Valid model, no provider pinned -> provider resolved from the hit.
        let s = SessionSeed {
            model: Some("m2".into()),
            provider: None,
            ..Default::default()
        }
        .clamp_to_responder(&list)
        .unwrap();
        assert_eq!((s.model.as_deref(), s.provider.as_deref()), (Some("m2"), Some("p2")));
        // Provider/model pair mismatch -> suggestion dropped entirely.
        let bad = SessionSeed {
            model: Some("m2".into()),
            provider: Some("p1".into()),
            ..Default::default()
        }
        .clamp_to_responder(&list);
        assert!(bad.is_none());
        // Unknown model -> None (responder defaults).
        assert!(SessionSeed {
            model: Some("nope".into()),
            provider: None,
            ..Default::default()
        }
        .clamp_to_responder(&list)
        .is_none());
    }

    #[test]
    fn session_seed_parses_workspace_and_valid_reasoning() {
        // workspace passes through; reasoning_effort only when the enum
        // understands it — an unknown effort string is dropped at parse
        // so it can never persist and resolve to None on every later read.
        let s = SessionSeed::from_json(r#"{"workspace":"w-9","reasoning_effort":"high"}"#)
            .unwrap();
        assert_eq!(s.workspace.as_deref(), Some("w-9"));
        assert_eq!(s.reasoning_effort.as_deref(), Some("high"));
        // A reasoning/workspace-only seed is valid (no model required).
        assert_eq!(s.model, None);
        // An unparseable effort alone -> every field empty -> no seed.
        assert!(
            SessionSeed::from_json(r#"{"reasoning_effort":"turbo"}"#).is_none(),
            "unparseable effort must not reach the session"
        );
    }

    #[test]
    fn terminal_sessions_persist_dedupe_and_forget() {
        let dir = std::env::temp_dir().join(format!("intent-terminal-{}", uuid::Uuid::new_v4()));
        let mut terminal = TerminalSessions::load(&dir);
        assert!(terminal.ids().is_empty(), "absent file starts empty");

        terminal.record("s-1").unwrap();
        terminal.record("s-2").unwrap();
        terminal.record("s-1").unwrap(); // idempotent
        assert!(terminal.contains("s-1"));
        assert!(terminal.contains("s-2"));
        assert_eq!(terminal.ids().len(), 2);

        // The mirror survives a restart — that is the point of the file.
        let reloaded = TerminalSessions::load(&dir);
        assert!(reloaded.contains("s-1") && reloaded.contains("s-2"));

        // A task that comes back must leave the list again.
        terminal.forget("s-1").unwrap();
        assert!(!terminal.contains("s-1"));
        assert!(!TerminalSessions::load(&dir).contains("s-1"));
        assert!(TerminalSessions::load(&dir).contains("s-2"));

        // An empty id is a no-op, not an entry that swallows every warning.
        terminal.record("").unwrap();
        assert_eq!(terminal.ids().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn terminal_sessions_drop_the_oldest_beyond_the_cap() {
        let dir = std::env::temp_dir().join(format!("intent-termcap-{}", uuid::Uuid::new_v4()));
        let mut terminal = TerminalSessions::load(&dir);
        for i in 0..(MAX_TERMINAL_SESSIONS + 5) {
            terminal.record(&format!("s-{i}")).unwrap();
        }
        assert_eq!(terminal.ids().len(), MAX_TERMINAL_SESSIONS);
        assert!(!terminal.contains("s-0"), "oldest dropped first");
        assert!(terminal.contains(&format!("s-{}", MAX_TERMINAL_SESSIONS + 4)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
