//! Intent → execution-session routing (design doc 27 §4.2.3, ADR-106 D6/D11).
//!
//! The wire addresses an **intent context** (a `conversation_id`), not an
//! execution session — "which execution session answers this" is the receiving
//! agent's private decision (the Gateway must not carry agent-private state
//! across the boundary). This module derives that context's key; the key →
//! session mapping itself lives in the session's own row (`intent_key`,
//! ADR-106 D11), read and written through the session-meta store. There is no
//! side file any more: `intent-sessions.json` and `terminal-sessions.json` were
//! a second durable truth that could fall out of step with the session it
//! named.
//!
//! Keys are namespaced so a conversation id and a message id can never
//! collide: `conv:{conversation_id}` / `msg:{message_id}`. A PM task's
//! conversation (`task:{project}:{task}`) is therefore `conv:task:{project}:{task}`,
//! which is exactly [`acowork_core::intent::task_routing_key`] — one execution
//! session per task even when the task lives in a shared project group chat
//! (design 27 §6.2).
//!
//! **A Chat is not an intent context here** (ADR-106 D6). The former
//! `chat:{chat_session_id}:{initiator}` branch — "continue whatever this chat
//! ran last" — is deleted: it was a silent verdict on a guess, and it needed a
//! pointer the Runtime had to keep in step with sessions it does not own.
//! Chat turns address a session through the binding on the session's row
//! instead (`SessionMeta::chat_session_id`, D11), and the deterministic route
//! only reuses one when the chat has exactly one candidate for this speaker.
//!
//! There is no rotation. The former T1 (idle) / T2 (post-compaction) triggers
//! were machine rules from before an LLM decided routing (design 27 §2.1/§4.1,
//! retired by ADR-099's follow-up): a compacted session is designed to keep
//! accepting turns in place, and rotating cost the successor everything the
//! predecessor held beyond its ≤ `handoff` token summary — with no tool able to
//! read another execution session's history. Starting a fresh session is now
//! always an explicit decision: the router's `target="new"`, the PM task
//! lifecycle, or the user.

/// Derive the routing key from an inbound intent envelope.
///
/// A turn that names its conversation continues that conversation; a turn that
/// names none is a one-shot (`msg:`), keyed by its own message id so it is
/// never re-keyed onto somebody else's session. Human Chat messages carry no
/// `conversation_id` (design 27 §4.2: a human-initiated round never counts
/// against the R4 budget) and so never land here — they are addressed through
/// their chat binding (ADR-106 D6/D11).
pub fn routing_key(conversation_id: &str, message_id: &str) -> String {
    if conversation_id.is_empty() {
        format!("msg:{message_id}")
    } else {
        format!("conv:{conversation_id}")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_by_preference() {
        // A named conversation is continued; the PM task's coordinate is just
        // such a conversation id, so it keys as `conv:task:…` (= the key the
        // Gateway's task close calls with).
        assert_eq!(
            routing_key("task:p1:t1", "m1"),
            "conv:task:p1:t1".to_string()
        );
        assert_eq!(routing_key("c1", "m1"), "conv:c1".to_string());
        assert_eq!(routing_key("", "m1"), "msg:m1".to_string());
    }

    #[test]
    fn an_unkeyed_turn_never_borrows_another_contexts_session() {
        // A Chat message carries no conversation id, so its key is its own
        // message id: two such turns can never collide on a mapping (D6 — the
        // deterministic route has no "default to the last session" any more).
        assert_ne!(routing_key("", "m1"), routing_key("", "m2"));
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
}
