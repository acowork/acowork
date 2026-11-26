//! RuntimeDebugService — implements [`DebugService`].
//!
//! ADR-048: each trait method does exactly two things:
//! 1. Look up the per-session `DebugController` and `DebugEventSender`
//!    (acquiring locks).
//! 2. Call the corresponding `debug::handlers::*` business function.
//!
//! All business logic lives in `debug/handlers.rs` — this impl is a thin
//! locking + dispatch layer. The HTTP routes in `http/debug.rs` are the
//! only call site for the trait (ADR-048 D4 removed the legacy
//! `debug/server.rs` WebSocket server).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::agent::agent_core::AgentCore;
use crate::debug::DebugEventSender;
use crate::debug::controller::DebugController;
use crate::debug::handlers::{DebugError, DebugStateSnapshot, ReExecuteOutcome, StepOutcome};
use crate::debug::protocol::{
    GetContextSnapshotParams, GetContextSnapshotResult, GetSectionParams, GetSectionResult,
    PatchContextParams, RewindParams, RewindResult, StepGranularity,
};

use super::debug_service::DebugService;

/// Concrete implementation of [`DebugService`] backed by per-session
/// `DebugController` instances owned by `SessionManager`.
///
/// The same `sessions` map is shared with `SessionManager::debug_controllers`.
/// When a new session is created under DevMode, SessionManager inserts a
/// fresh controller AND calls `register_event_sender`, and HTTP routes
/// (via this service) pick both up on the next request without any extra
/// wiring.
pub struct RuntimeDebugService {
    /// Per-session debug controllers (keyed by session_id).
    sessions: Arc<tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>>,
    /// Per-session event senders — each sender is a fresh `DebugEventSender`
    /// instance with the corresponding session_id embedded.
    event_senders: Arc<tokio::sync::RwLock<HashMap<String, DebugEventSender>>>,
    /// ADR-063: the agent's `Arc<AgentCore>` — `reload_prompts` writes
    /// the 9 per-agent prompt overrides into its `Arc<RwLock<Option<String>>>`
    /// fields. Held as `Option` because the slot is filled in Phase B
    /// (after `RuntimeDebugService::new` is constructed in Phase A or
    /// `startup/debug_enable.rs`); the `reload_prompts` impl tolerates
    /// `None` with an `Internal` error rather than panicking.
    agent_core: Arc<std::sync::RwLock<Option<Arc<AgentCore>>>>,
    /// ADR-063: the agent's `.bot` package directory (where
    /// `prompts/<file>.md` lives). `reload_prompts` reads each canonical
    /// filename via `load_optional_prompt(package_dir, file)` and writes
    /// the result into the matching `AgentCore` field.
    package_dir: PathBuf,
}

impl RuntimeDebugService {
    /// Create a new `RuntimeDebugService` sharing the SessionManager's
    /// per-session state.
    pub fn new(
        sessions: Arc<
            tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>,
        >,
        event_senders: Arc<tokio::sync::RwLock<HashMap<String, DebugEventSender>>>,
    ) -> Self {
        // ADR-063: pre-R7 callers (tests that exercise the trait without
        // the agent-wide reload path) still need a working service.
        // Default `agent_core` to an empty slot and `package_dir` to a
        // dummy path; `reload_prompts` returns `Internal` in that case.
        // Production wiring (Phase B SessionManager + startup/debug_enable)
        // always calls the full 4-arg constructor below.
        Self {
            sessions,
            event_senders,
            agent_core: Arc::new(std::sync::RwLock::new(None)),
            package_dir: PathBuf::new(),
        }
    }

    /// ADR-063: full 4-arg constructor used by the production wiring.
    /// The empty `agent_core` slot is filled by Phase B (or by
    /// `startup/debug_enable` after a mid-loop `EnableDebugMode`), and
    /// `reload_prompts` becomes fully functional once that happens.
    pub fn new_with_agent(
        sessions: Arc<
            tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>,
        >,
        event_senders: Arc<tokio::sync::RwLock<HashMap<String, DebugEventSender>>>,
        agent_core: Arc<std::sync::RwLock<Option<Arc<AgentCore>>>>,
        package_dir: PathBuf,
    ) -> Self {
        Self {
            sessions,
            event_senders,
            agent_core,
            package_dir,
        }
    }

    /// Look up the per-session controller and event sender, returning
    /// `SessionNotFound` if either is missing.
    async fn get_session_state(
        &self,
        session_id: &str,
    ) -> Result<(Arc<tokio::sync::Mutex<DebugController>>, DebugEventSender), DebugError> {
        let ctrl = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        }
        .ok_or_else(|| DebugError::SessionNotFound(session_id.to_string()))?;

        let tx = {
            let senders = self.event_senders.read().await;
            senders.get(session_id).cloned()
        }
        .ok_or_else(|| DebugError::SessionNotFound(session_id.to_string()))?;

        Ok((ctrl, tx))
    }

    /// Register an event sender for a specific session. Called by
    /// `SessionManager` when a new session is created under DevMode.
    pub async fn register_event_sender(&self, session_id: &str, sender: DebugEventSender) {
        let mut senders = self.event_senders.write().await;
        senders.insert(session_id.to_string(), sender);
    }
}

#[async_trait]
impl DebugService for RuntimeDebugService {
    async fn resume(&self, session_id: &str) -> Result<(), DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_resume(&mut ctrl, &tx).await
    }

    async fn pause(&self, session_id: &str) -> Result<(), DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_pause(&mut ctrl, &tx).await
    }

    async fn step(
        &self,
        session_id: &str,
        granularity: StepGranularity,
    ) -> Result<StepOutcome, DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_step(&mut ctrl, &tx, granularity).await
    }

    async fn stop(&self, session_id: &str) -> Result<(), DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_stop(&mut ctrl, &tx).await
    }

    async fn get_state(&self, session_id: &str) -> Result<DebugStateSnapshot, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_get_state(&mut ctrl).await
    }

    async fn get_context_snapshot(
        &self,
        session_id: &str,
        params: GetContextSnapshotParams,
    ) -> Result<GetContextSnapshotResult, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_get_context_snapshot(&mut ctrl, params).await
    }

    async fn get_section(
        &self,
        session_id: &str,
        params: GetSectionParams,
    ) -> Result<GetSectionResult, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_get_section(&mut ctrl, params).await
    }

    async fn rewind(
        &self,
        session_id: &str,
        params: RewindParams,
    ) -> Result<RewindResult, DebugError> {
        let (ctrl_arc, tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_rewind(&mut ctrl, &tx, params).await
    }

    async fn patch_context(
        &self,
        session_id: &str,
        params: PatchContextParams,
    ) -> Result<(), DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_patch_context(&mut ctrl, params).await
    }

    async fn re_execute(&self, session_id: &str) -> Result<ReExecuteOutcome, DebugError> {
        let (ctrl_arc, _tx) = self.get_session_state(session_id).await?;
        let mut ctrl = ctrl_arc.lock().await;
        crate::debug::handlers::handle_re_execute(&mut ctrl).await
    }

    /// ADR-063 §3.7.5 L2 reload — re-read every `prompts/<file>.md`
    /// from `<package_dir>/prompts/` and overwrite the corresponding
    /// `Arc<RwLock<Option<String>>>` field on the canonical `AgentCore`.
    ///
    /// Reads use the canonical filename table in
    /// [`crate::package::prompt_builder::OVERRIDABLE_PROMPTS`] so the
    /// reload covers the same 9 fields Phase A loaded at startup.
    /// Writes go through `Arc::clone(&core.<field>)` first — every
    /// session of this agent holds its own `Arc<AgentCore>` clone that
    /// shares the inner `Arc<RwLock<Option<String>>>` with the canonical
    /// one (see `Clone for AgentCore`), so a single write propagates to
    /// all live sessions.
    ///
    /// Failure modes (all map to `DebugError::Internal`):
    ///   - `agent_core` slot still empty (Phase B hasn't run yet, or
    ///     this is a pre-R7 2-arg-constructed service) — the caller
    ///     should retry after Phase B finishes.
    ///   - I/O error reading a prompt file — surfaced via the
    ///     `load_optional_prompt` warning path; missing files are
    ///     silently treated as "no override" (same contract as Phase A).
    async fn reload_prompts(&self) -> Result<(), DebugError> {
        let core_arc = {
            let slot = self.agent_core.read().unwrap();
            slot.clone()
        }
        .ok_or_else(|| {
            DebugError::Internal(
                "agent_core slot is empty — reload_prompts requires Phase B to have constructed AgentCore"
                    .to_string(),
            )
        })?;

        if self.package_dir.as_os_str().is_empty() {
            return Err(DebugError::Internal(
                "package_dir not configured for this DebugService — pre-R7 2-arg constructor"
                    .to_string(),
            ));
        }

        // Delegate to the package-level free function — single source of
        // truth for the filename→AgentCore-field dispatch table. The HTTP
        // `POST /agents/{id}/prompts/reload` handler also calls it
        // directly (without going through this trait), which is what lets
        // the Debug panel reload while DevMode is still disabled.
        crate::package::prompt_builder::reload_prompts_into_core(&self.package_dir, &core_arc)
            .map_err(|e| DebugError::Internal(e.to_string()))?;

        tracing::info!(
            package_dir = %self.package_dir.display(),
            "ADR-063: reload_prompts — 9 LLM prompt overrides reloaded from disk into AgentCore"
        );
        Ok(())
    }
}
