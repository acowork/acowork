//! RuntimeSessionConfigService - implements SessionConfigService (ADR-047 §3.4.2).
//!
//! Uses interior mutability via `Arc<RwLock<HashMap>>` to access session
//! config stores, unblocking the ADR-040 `&mut self` problem. All methods
//! are `&self`, so the struct can be safely wrapped in `Arc<dyn SessionConfigService>`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::agent::session_config::llm_effects;
use crate::agent::session_config::{SessionConfigDelta, SessionConfigSnapshot};
use crate::conversation::ConversationSession;
use crate::error::{Result, RuntimeError};
use crate::http::SharedAgentCore;
use crate::tools::workspace_resolver::WorkspaceResolver;
use crate::usecases::session_config::{SessionConfigService, UsageRecompute};

/// Shared map of session config stores, keyed by session_id.
///
/// Populated by `SessionManager` when sessions are created, and depopulated
/// when sessions are removed. Read by `RuntimeSessionConfigService` to
/// apply config changes without going through the serial inference queue.
pub type SharedSessionConfigs = Arc<RwLock<HashMap<String, Arc<ConversationSession>>>>;

pub struct RuntimeSessionConfigService {
    /// Shared session config stores, keyed by session_id.
    sessions: SharedSessionConfigs,
    /// For workspace validation (optional - CLI mode may not have one).
    resolver: Option<Arc<RwLock<WorkspaceResolver>>>,
    /// Late-bind slot for `AgentCore` — populated by `Phase B` once
    /// `AgentCore::new` + post-construction injection (compat cache,
    /// global provider list, memory, etc.) is complete.
    ///
    /// `get_config` consults this slot on every external read so the
    /// HTTP `GET /sessions/{sid}/config` and MQTT retained responses
    /// surface the *effective* `reasoning_effort` (persisted → provider
    /// default → Auto fallback → None) rather than the raw persisted
    /// value. Without this layer, old sessions whose `meta.json` never
    /// persisted `reasoning_effort` would report `null` indefinitely,
    /// hiding the reasoning-effort toggle button until the user
    /// explicitly switched model once — see
    /// `agent::session_config::llm_effects::resolve_effective_reasoning_effort`
    /// for the full priority chain.
    ///
    /// NOTE: held as the same `SharedAgentCore` slot that already
    /// exists for the HTTP server's late-bind pattern. If the slot is
    /// still empty when `get_config` is called (e.g. the test harness
    /// never wires one up), `get_config` falls back to the raw
    /// persisted value — the legacy behaviour, which tests for the
    /// resolver itself do not depend on.
    core_slot: SharedAgentCore,
    /// Late-bind slot for the idle-session usage recompute callback
    /// (ADR-074 §11.1). Empty until Phase B wires the `SessionManager`
    /// closure. `apply_config` invokes it after a `context_window`
    /// change so the UI context-usage total refreshes immediately —
    /// a running loop is skipped inside the callback and the next
    /// per-turn usage push takes over.
    usage_recompute: std::sync::RwLock<Option<UsageRecompute>>,
}

impl RuntimeSessionConfigService {
    pub fn new(
        sessions: SharedSessionConfigs,
        resolver: Option<Arc<RwLock<WorkspaceResolver>>>,
        core_slot: SharedAgentCore,
    ) -> Self {
        Self {
            sessions,
            resolver,
            core_slot,
            usage_recompute: std::sync::RwLock::new(None),
        }
    }

    /// Inject the idle-session usage recompute callback (ADR-074 §11.1).
    /// Called once in Phase B after `SessionManager` construction.
    pub fn set_usage_recompute(&self, cb: UsageRecompute) {
        if let Ok(mut slot) = self.usage_recompute.write() {
            *slot = Some(cb);
        }
    }
}

#[async_trait]
impl SessionConfigService for RuntimeSessionConfigService {
    async fn apply_config(&self, session_id: &str, delta: SessionConfigDelta) -> Result<()> {
        // Validate workspace_id if provided and resolver is available.
        if let Some(ref workspace_id) = delta.workspace_id
            && workspace_id != "__agent_home__"
            && let Some(ref resolver) = self.resolver
        {
            let guard = resolver.read().map_err(|e| {
                RuntimeError::Config(format!("WorkspaceResolver lock poisoned: {}", e))
            })?;
            if guard.find_by_id(workspace_id).is_none() {
                return Err(RuntimeError::Config(format!(
                    "Workspace not found: {}",
                    workspace_id
                )));
            }
        }

        let sessions = self
            .sessions
            .read()
            .map_err(|e| RuntimeError::Config(format!("SessionConfigs lock poisoned: {}", e)))?;

        let conv = sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        // apply_config is &self (interior mutability via Mutex), so no
        // additional locking is needed beyond the RwLock read guard.
        conv.apply_config(&delta);

        tracing::info!(
            session_id = %session_id,
            has_model = delta.model.is_some(),
            has_provider = delta.provider.is_some(),
            has_workspace = delta.workspace_id.is_some(),
            has_effort = delta.reasoning_effort.is_some(),
            has_temperature = delta.temperature.is_some(),
            has_context_window = delta.context_window.is_some(),
            has_title = delta.title.is_some(),
            "SessionConfigService: apply_config completed"
        );

        // ADR-074 §11.1: after a context_window change, immediately
        // recompute + re-broadcast context usage so the UI total does not
        // sit on the stale window. Idle sessions have no per-turn push to
        // fall back on, so this is the only chance to refresh the number;
        // a running loop is skipped inside the callback (next per-turn
        // push takes over). Best-effort: the slot may be empty in tests
        // or before Phase B wiring.
        if delta.context_window.is_some()
            && let Ok(slot) = self.usage_recompute.read()
            && let Some(cb) = slot.as_ref()
        {
            cb(session_id.to_string());
        }

        Ok(())
    }

    async fn get_config(&self, session_id: &str) -> Result<SessionConfigSnapshot> {
        let sessions = self
            .sessions
            .read()
            .map_err(|e| RuntimeError::Config(format!("SessionConfigs lock poisoned: {}", e)))?;

        let conv = sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        // Read raw in-memory snapshot first.
        let mut snapshot = conv.config_snapshot();

        // Apply the shared `resolve_effective_reasoning_effort` chain so
        // that the HTTP/MQTT read path observes the SAME effective
        // value as the in-memory session state. Without this layer, an
        // old session whose `meta.json` never persisted
        // `reasoning_effort` (e.g. the user never toggled it, or the
        // model at the time didn't support reasoning) would always
        // report `null` here, hiding the reasoning-effort toggle button
        // until the user explicitly switched model once.
        //
        // `config_snapshot()` itself stays raw because the in-process
        // turn-boundary diff detection in `session_task` reads it
        // directly — adding a fallback there would silently make every
        // diff look like a change.
        if snapshot.reasoning_effort.is_none()
            && let Ok(slot) = self.core_slot.read()
            && let Some(core) = slot.as_ref()
        {
            let caps = snapshot
                .model
                .as_deref()
                .and_then(|m| core.get_model_capabilities(m));
            if let Some(effort) = llm_effects::resolve_effective_reasoning_effort(
                caps.as_ref(),
                snapshot.reasoning_effort.as_deref(),
            ) {
                snapshot.reasoning_effort = Some(effort.to_string());
            }
        }

        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for `resolve_effective_reasoning_effort` invocation
    //! from `get_config`. Higher-level chain tests live in
    //! `agent::session_config::llm_effects` — here we just verify the
    //! integration with the snapshot field.

    use crate::agent::session_config::llm_effects::resolve_effective_reasoning_effort;

    fn empty_caps() -> acowork_core::ModelCapabilitiesInfo {
        acowork_core::ModelCapabilitiesInfo {
            context_window: 128_000,
            max_output_tokens: 16_384,
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
        }
    }

    #[test]
    fn resolver_persisted_wins_over_caps_default() {
        let caps = empty_caps();
        let got = resolve_effective_reasoning_effort(Some(&caps), Some("high"));
        assert_eq!(
            got,
            Some(acowork_core::providers::traits::ReasoningEffort::High)
        );
    }

    #[test]
    fn resolver_falls_back_to_caps_default_when_persisted_is_none() {
        let mut caps = empty_caps();
        caps.default_reasoning_effort = Some("medium".to_string());
        let got = resolve_effective_reasoning_effort(Some(&caps), None);
        assert_eq!(
            got,
            Some(acowork_core::providers::traits::ReasoningEffort::Medium)
        );
    }

    #[test]
    fn resolver_falls_back_to_auto_when_supports_reasoning() {
        let mut caps = empty_caps();
        caps.supports_reasoning = Some(true);
        let got = resolve_effective_reasoning_effort(Some(&caps), None);
        assert_eq!(
            got,
            Some(acowork_core::providers::traits::ReasoningEffort::Auto)
        );
    }

    #[test]
    fn resolver_returns_none_when_model_does_not_support_reasoning() {
        let mut caps = empty_caps();
        caps.supports_reasoning = Some(false);
        let got = resolve_effective_reasoning_effort(Some(&caps), None);
        assert_eq!(got, None);
    }

    #[test]
    fn resolver_returns_none_when_caps_unknown_and_persisted_none() {
        let got = resolve_effective_reasoning_effort(None, None);
        assert_eq!(got, None);
    }

    #[test]
    fn resolver_ignores_unparseable_persisted_value() {
        // Frontend sends "auto" / "off" / "low" / "medium" / "high" /
        // "xhigh"; anything else is treated as "no persisted value"
        // and falls through to caps.default_reasoning_effort.
        let mut caps = empty_caps();
        caps.default_reasoning_effort = Some("low".to_string());
        let got = resolve_effective_reasoning_effort(Some(&caps), Some("garbage"));
        assert_eq!(
            got,
            Some(acowork_core::providers::traits::ReasoningEffort::Low)
        );
    }

    #[test]
    fn resolver_treats_empty_string_persisted_as_absent() {
        let mut caps = empty_caps();
        caps.default_reasoning_effort = Some("auto".to_string());
        let got = resolve_effective_reasoning_effort(Some(&caps), Some(""));
        assert_eq!(
            got,
            Some(acowork_core::providers::traits::ReasoningEffort::Auto)
        );
    }
}
