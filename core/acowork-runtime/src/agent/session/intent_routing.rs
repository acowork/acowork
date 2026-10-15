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
    /// Crash-recovery marker (§2.1 "先幂等创建 successor 再原子切换映射"):
    /// the id allocated for the next rotation, persisted **before** the
    /// session is created. A crash between allocation and rekey replays to
    /// the same id — no second successor, no orphan. Cleared by `rekey`.
    #[serde(default)]
    pub pending_successor: Option<String>,
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

/// Idle threshold for T1 rotation. Layer 1 = `AgentConfig::intent_idle_rotate_hours`
/// (design 27 §2.1 settings-card surface); falls back to the deployment-level
/// `ACOWORK_INTENT_IDLE_ROTATE_HOURS`, then the design default 24h.
/// `0` disables T1. Values above the design range (1–72) are clamped.
pub fn idle_rotate_threshold(work_dir: &Path) -> Option<std::time::Duration> {
    let hours = agent_config_value(work_dir)
        .and_then(|c| c.intent_idle_rotate_hours)
        .or_else(|| {
            std::env::var("ACOWORK_INTENT_IDLE_ROTATE_HOURS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
        });
    match hours.unwrap_or(24) {
        0 => None,
        h => Some(std::time::Duration::from_secs(h.min(72) * 3600)),
    }
}

/// T2 handoff token cap (design 27 §2.1: target 1,500, configurable
/// 500–4,000 via `AgentConfig::intent_handoff_max_tokens`). T1's 500-token
/// cap is design-fixed and not exposed here.
pub fn handoff_max_tokens(work_dir: &Path) -> u64 {
    agent_config_value(work_dir)
        .and_then(|c| c.intent_handoff_max_tokens)
        .unwrap_or(1500)
        .clamp(500, 4000)
}

fn agent_config_value(work_dir: &Path) -> Option<crate::agent_config::AgentConfig> {
    crate::agent_config::load_agent_config(work_dir).ok().flatten()
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
                pending_successor: None,
            },
        );
        self.flush()
    }

    /// Allocate (or replay) the successor id for the next rotation of `key`
    /// (§2.1 crash recovery). Idempotent: once a planned id is persisted,
    /// every call returns the same id until `rekey` consumes it. Persisted
    /// before the session file exists, so a crash between "plan" and
    /// "create" cannot produce a second successor.
    pub fn plan_rotation(&mut self, key: &str) -> Result<String> {
        if let Some(id) = self
            .entries
            .get(key)
            .and_then(|e| e.pending_successor.clone())
        {
            return Ok(id);
        }
        let id = crate::conversation::generate_session_id();
        match self.entries.get_mut(key) {
            Some(e) => e.pending_successor = Some(id.clone()),
            None => return Err(crate::error::RuntimeError::Config(format!(
                "intent routing entry '{key}' missing — cannot plan a rotation"
            ))),
        }
        self.flush()?;
        Ok(id)
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

/// Inbound Intent's optional `session_config` suggestion (design 27 §4
/// field table, §13-6: "只是应方允许范围内的建议"). Only `model` and
/// `provider` are honored today — the two knobs the responder's session
/// creation path already accepts; unknown keys are ignored (additive
/// fields land without a wire change).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSeed {
    pub model: Option<String>,
    pub provider: Option<String>,
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
        let seed = Self { model: get("model"), provider: get("provider") };
        if seed.model.is_none() && seed.provider.is_none() {
            None
        } else {
            Some(seed)
        }
    }

    /// Clamp the suggestion to the responder's allowed set (§13-6): the
    /// model must exist in the responder's provider list — under the
    /// suggested provider when one is given, otherwise under any provider
    /// that serves it (the provider is then pinned to the hit so the pair
    /// is never half-suggested). If nothing of the suggestion survives it
    /// returns `None` → responder defaults.
    pub fn clamp_to_responder(
        self,
        providers: &[acowork_core::protocol::ProviderListItem],
    ) -> Option<Self> {
        let model = self.model.as_deref()?;
        let want_provider = self.provider.as_deref().filter(|p| !p.is_empty());
        for p in providers {
            let provider_ok = want_provider.is_none_or(|w| w == p.id);
            if provider_ok && p.models.iter().any(|m| m.id == model) {
                return Some(Self {
                    model: Some(model.to_string()),
                    provider: Some(p.id.clone()),
                });
            }
        }
        tracing::warn!(
            model,
            provider = want_provider.unwrap_or(""),
            "Intent session_config suggestion outside the responder's allowed set — using defaults"
        );
        None
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

/// Handoff note prepended to the first intent routed into a successor
/// session after a rotation (design 27 §2.1 Handoff grading).
///
/// Grading per the design table:
/// - **T1 idle**: open items / key decisions only — the Runtime records no
///   such ledger between turns, and the design allows the T1 handoff to be
///   empty ("若无未决事项可完全省略，不带对话摘要"), so T1 stays a pointer.
/// - **T2 compaction**: `summary` carries the old session's LAST persisted
///   compaction entry (already an LLM distillation of goals / decisions /
///   open items — reusing it costs no second LLM call and never copies the
///   JSONL wholesale, the two invariants §2.1 asks for). Truncated to
///   `cap_tokens` (the Agent-configurable 500–4,000 target). A missing
///   summary degrades to the pointer form with a WARN, per §2.1's
///   "Compact Model 不可用" fallback row.
pub fn rotation_handoff(
    reason: RotateReason,
    old_session_id: &str,
    summary: Option<&str>,
    cap_tokens: u64,
) -> String {
    let pointer = match reason {
        RotateReason::T1Idle => format!(
            "[intent rotation] The previous execution session for this context (session id \"{old}\") was rotated after an idle interval past the T1 threshold; no open items were recorded for carry-over. Its full history stays available under that session id.",
            old = old_session_id
        ),
        RotateReason::T2Compaction => format!(
            "[intent rotation] The previous execution session for this context (session id \"{old}\") was rotated after an in-turn compaction (T2: long-running task).",
            old = old_session_id
        ),
    };
    match (reason, summary) {
        (RotateReason::T2Compaction, Some(s)) => {
            let (body, truncated) = truncate_to_tokens(s, cap_tokens);
            let body_note = format!(
                "{pointer}\n[distilled handoff — last compaction summary of the previous session{}]\n{body}",
                if truncated { ", truncated to the token cap" } else { "" }
            );
                if truncated {
                    tracing::warn!(
                        cap_tokens,
                        "Intent T2 handoff truncated to token cap (design 27 §2.1)"
                    );
                }
                body_note
            }
        (RotateReason::T2Compaction, None) => {
            tracing::warn!(
                old_session = %old_session_id,
                "Intent T2 rotation without a readable compaction summary — pointer handoff only"
            );
            format!("{pointer} Its full history and compaction summary stay under that session id.")
        }
        _ => pointer,
    }
}

/// Cut `text` to roughly `max_tokens` tokens (chars / 3.5, the Runtime's
/// standard estimate) on a char boundary. Returns `(body, truncated)`.
fn truncate_to_tokens(text: &str, max_tokens: u64) -> (String, bool) {
    let budget = (max_tokens as usize).saturating_mul(4); // conservative: 4 bytes/token
    if text.len() <= budget {
        return (text.to_string(), false);
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
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
    fn plan_rotation_is_idempotent_and_survives_reload_until_rekey() {
        // §2.1 crash recovery: plan → "crash" (reload) → plan again must
        // yield the SAME successor id; rekey consumes the plan.
        let dir = std::env::temp_dir().join(format!("intent-map-{}", uuid::Uuid::new_v4()));
        let planned = {
            let mut m = IntentSessionMap::load(&dir);
            m.insert("conv:c1".into(), "s-1".into()).unwrap();
            m.plan_rotation("conv:c1").unwrap()
        };
        // Replay after a simulated crash (fresh load from the file).
        let replayed = {
            let mut m = IntentSessionMap::load(&dir);
            m.plan_rotation("conv:c1").unwrap()
        };
        assert_eq!(planned, replayed, "replay must reuse the planned id");
        // rekey (successor consumed) clears the plan; the next rotation
        // allocates a fresh id.
        let mut m = IntentSessionMap::load(&dir);
        m.rekey("conv:c1", planned.clone()).unwrap();
        assert_eq!(m.get("conv:c1").unwrap().pending_successor, None);
        let next = m.plan_rotation("conv:c1").unwrap();
        assert_ne!(next, planned);
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
        };
        let entry = |id: &str| ProviderModelEntry {
            id: id.to_string(),
            capabilities: caps.clone(),
            max_output_tokens_limit: 0,
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
        }
        .clamp_to_responder(&list)
        .unwrap();
        assert_eq!((s.model.as_deref(), s.provider.as_deref()), (Some("m2"), Some("p2")));
        // Provider/model pair mismatch -> suggestion dropped entirely.
        let bad = SessionSeed {
            model: Some("m2".into()),
            provider: Some("p1".into()),
        }
        .clamp_to_responder(&list);
        assert!(bad.is_none());
        // Unknown model -> None (responder defaults).
        assert!(SessionSeed {
            model: Some("nope".into()),
            provider: None,
        }
        .clamp_to_responder(&list)
        .is_none());
    }

    #[test]
    fn t2_handoff_carries_distilled_summary_t1_stays_pointer() {
        let summary = "Goal: ship X. Decisions: A, B. Open: C.";
        let t2 = rotation_handoff(RotateReason::T2Compaction, "s-old", Some(summary), 1500);
        assert!(t2.contains("distilled handoff"), "{t2}");
        assert!(t2.contains(summary), "summary embedded");
        assert!(t2.contains("s-old"), "predecessor id kept");
        // T1: design fixes pointer-only (no conversation summary).
        let t1 = rotation_handoff(RotateReason::T1Idle, "s-old", Some(summary), 1500);
        assert!(!t1.contains("distilled handoff"), "T1 never carries a summary");
        // T2 with no readable summary degrades to the pointer form.
        let degraded = rotation_handoff(RotateReason::T2Compaction, "s-old", None, 1500);
        assert!(!degraded.contains("distilled handoff"));
        assert!(degraded.contains("s-old"));
    }

    #[test]
    fn handoff_truncates_to_token_cap_on_char_boundary() {
        // CJK content: 3 bytes per char — a naive byte cut would panic.
        let long = "结".repeat(5000); // 15000 bytes
        let (body, truncated) = truncate_to_tokens(&long, 1000); // 4000-byte budget
        assert!(truncated);
        assert!(body.len() <= 4000);
        assert_eq!(body.chars().count() * 3, body.len(), "clean char boundary");
        let (whole, not) = truncate_to_tokens("short", 1000);
        assert!(!not);
        assert_eq!(whole, "short");
    }

    #[test]
    fn agent_config_knobs_override_defaults() {
        let dir = std::env::temp_dir().join(format!("intent-cfg-{}", uuid::Uuid::new_v4()));
        let mut cfg = crate::agent_config::AgentConfig {
            intent_idle_rotate_hours: Some(6),
            intent_handoff_max_tokens: Some(9000), // above design max -> clamped to 4000
            ..Default::default()
        };
        crate::agent_config::save_agent_config(&dir, &cfg).unwrap();
        assert_eq!(
            idle_rotate_threshold(&dir),
            Some(std::time::Duration::from_secs(6 * 3600))
        );
        assert_eq!(handoff_max_tokens(&dir), 4000);
        // 0 disables T1.
        cfg.intent_idle_rotate_hours = Some(0);
        crate::agent_config::save_agent_config(&dir, &cfg).unwrap();
        assert_eq!(idle_rotate_threshold(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
