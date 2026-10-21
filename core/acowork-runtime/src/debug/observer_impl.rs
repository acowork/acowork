//! Concrete debug observer implementation backed by DebugController.
//!
//! [`DebugObserverImpl`] consolidates all debug logic that was previously
//! scattered across `loop_.rs`, `agent_core.rs`, and `session_task.rs`:
//!
//! - Phase tracking (`update_debug_phase`)
//! - Step event pushing (`push_debug_step`)
//! - Auto-pause in stepping mode (`debug_auto_pause_if_stepping`)
//! - Context snapshot capture (`capture_context_snapshot`)
//! - Pause/resume/rewind loop (`await_debug_resume`)
//! - Pending patches application
//! - Bypass injection (`check_and_apply_pending_debug`)

use std::sync::Arc;

use acowork_core::providers::traits::ChatMessage;
use tokio::sync::Notify;

use super::DebugHandles;
use super::controller::{
    ContextSnapshot, ContextSnapshotSections, DebugController, DebugState, NamedSection,
    SectionContent,
};
use super::events::{DebugEvent, DebugEventSender};
use super::observer::ContextSnapshotRequest;
use super::protocol::{DebugPhase, RequestParams};
use crate::agent::context::{ContextBuilder, compute_section_sizes};
use crate::agent::history::HistoryManager;
use crate::agent::session_state::SessionStatus;
use crate::util::text::TextPreview;

// ── Debug Observer Implementation ─────────────────────────────────────

/// Re-materialize the source string for a `ContextUsageSection` key from
/// a [`ContextBuilder`]. Used by `DebugObserverImpl::on_context_built` to
/// enrich the size-only section list with full content for the Debug
/// Panel.
///
/// ADR-067: the helper is the **only** place that maps section keys to
/// their source strings. The always-on emission path
/// (`process_llm_response_usage`) is free to carry *byte-size-only* keys
/// that have no single source string at all — `skills_loaded` is the first,
/// and a key added there without a content arm here is deliberately dropped
/// from the Debug Panel rather than warned about. Every other key MUST
/// agree across the two paths, because only the DevMode path needs the full
/// content. Returns `None` for keys that don't have a single source string
/// (`tool_definitions` and `messages` are handled specially by the caller
/// because they need additional JSON / SHA-256 work).
fn section_text_from_builder(key: &str, builder: &ContextBuilder) -> Option<String> {
    match key {
        "system_prompt" => Some(builder.system_prompt().to_string()),
        "identity_context" => builder.identity_context().map(|s| s.to_string()),
        "workspace_context" => builder.workspace_context().map(|s| s.to_string()),
        "retrieved_memory" => builder.retrieved_memory().map(|s| s.to_string()),
        "ambiguous_confirmation_hint" => {
            builder.ambiguous_confirmation_hint().map(|s| s.to_string())
        }
        "abstention_prompt" => builder.abstention_prompt().map(|s| s.to_string()),
        "skill_instructions" => builder.skill_instructions().map(|s| s.to_string()),
        "environment" => Some(
            builder
                .environment_override()
                .map(|s| s.to_string())
                .unwrap_or_else(|| crate::agent::context::detect_environment_text().to_string()),
        ),
        "workspace_prompt_file" => builder.workspace_prompt_file().map(|s| s.to_string()),
        // Byte-size-only section: the caller handles these specially.
        //
        // `skills_loaded` is deliberately NOT re-materialized. Its content
        // is N `load_skill` Tool results scattered through history, not one
        // string, so there is nothing to render here; the Debug Panel keeps
        // showing those bodies where they actually live — inside the
        // lazily-loaded `messages` section. The size still ships on the
        // always-on usage path, which reads it straight off
        // `HistoryManager`.
        //
        // This arm exists to keep the key out of the `other =>` branch
        // below, which warns. Without it every context build with a loaded
        // skill logs a spurious "unknown section key".
        "skills_loaded" => None,
        // Caller handles these specially.
        "tool_definitions" | "messages" => None,
        other => {
            tracing::warn!(
                key = other,
                "section_text_from_builder: unknown section key"
            );
            None
        }
    }
}

/// Real debug observer backed by DebugController, event sender, and notify handles.
///
/// This struct absorbs the 5 `Option<T>` fields that were previously on
/// `AgentCore` and the 5 debug methods that were on `AgentLoop`.
pub struct DebugObserverImpl {
    ctrl: Arc<tokio::sync::Mutex<DebugController>>,
    event_tx: DebugEventSender,
    rewind_notify: Arc<Notify>,
    resume_notify: Arc<Notify>,
    control_notify: Arc<Notify>,
    /// Pending debug handles injected by SessionManager while the agent loop
    /// is already running. The bypass injection path.
    pending_injection: Option<Arc<tokio::sync::Mutex<Option<DebugHandles>>>>,
}

impl DebugObserverImpl {
    /// Create a new DevMode observer from the given handles.
    pub fn new(handles: DebugHandles) -> Self {
        Self {
            ctrl: handles.debug_ctrl,
            event_tx: handles.debug_event_tx,
            rewind_notify: handles.rewind_notify,
            resume_notify: handles.resume_notify,
            control_notify: handles.control_notify,
            pending_injection: None,
        }
    }

    /// Set the bypass injection channel (called by SessionManager).
    pub fn set_pending_injection(&mut self, ch: Arc<tokio::sync::Mutex<Option<DebugHandles>>>) {
        self.pending_injection = Some(ch);
    }

    /// Access the debug controller Arc.
    pub fn ctrl(&self) -> &Arc<tokio::sync::Mutex<DebugController>> {
        &self.ctrl
    }

    /// Access the rewind notify handle.
    pub fn rewind_notify(&self) -> &Arc<Notify> {
        &self.rewind_notify
    }

    /// Access the resume notify handle.
    pub fn resume_notify(&self) -> &Arc<Notify> {
        &self.resume_notify
    }

    /// Access the control notify handle.
    pub fn control_notify(&self) -> &Arc<Notify> {
        &self.control_notify
    }

    /// Access the event sender.
    pub fn event_tx(&self) -> &DebugEventSender {
        &self.event_tx
    }

    /// Await debug resume: blocks if the debug controller is in Paused state.
    ///
    /// Uses `rewind_notify` via `tokio::select!` so that rewinds are applied
    /// immediately (notification-driven) rather than after up to 100ms of polling.
    ///
    /// Returns `true` if execution should continue, `false` if stopped.
    pub async fn await_resume(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        poll_stop: &mut dyn FnMut() -> bool,
        transition_status: &mut dyn FnMut(SessionStatus),
    ) -> bool {
        loop {
            // Check for Chat Panel STOP (arrives via inbound channel).
            if poll_stop() {
                tracing::info!("Debug: agent loop stopped via inbound channel");
                let mut ctrl_guard = self.ctrl.lock().await;
                let iteration = ctrl_guard.iteration;
                ctrl_guard.state = DebugState::Stopped;
                drop(ctrl_guard);
                let _ = self.event_tx.send(DebugEvent::ExecutionStateChanged {
                    new_state: DebugState::Stopped,
                    iteration,
                });
                return false;
            }

            // Consume any pending rewind target during polling.
            {
                let mut ctrl = self.ctrl.lock().await;
                apply_rewind_locked(&mut ctrl, session_id, history);
            }

            let state = {
                let ctrl = self.ctrl.lock().await;
                ctrl.state
            };
            match state {
                DebugState::Running => {
                    // ADR-049: HTTP request about to be sent → LlmAwaitingFirstChunk.
                    transition_status(SessionStatus::LlmAwaitingFirstChunk);
                    return true;
                }
                DebugState::Stepping => {
                    // ADR-049: HTTP request about to be sent → LlmAwaitingFirstChunk.
                    transition_status(SessionStatus::LlmAwaitingFirstChunk);
                    return true;
                }
                DebugState::Stopped => {
                    tracing::info!("Debug: agent loop stopped");
                    transition_status(SessionStatus::Idle);
                    return false;
                }
                DebugState::Paused => {
                    transition_status(SessionStatus::Paused {
                        iteration: None,
                        max_iterations: None,
                        retry_info: None,
                        reason: Some(crate::agent::session_state::PauseReason::Debug),
                        message: None,
                    });
                    // Use tokio::select! with rewind_notify so that
                    // rewinds are applied immediately (notification-driven)
                    // instead of waiting up to 100ms for the next poll.
                    tokio::select! {
                        _ = self.rewind_notify.notified() => {},
                        _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                    }
                }
            }
        }
    }
}

// ── DebugObserver trait implementation ────────────────────────────────

impl super::observer::DebugObserver for DebugObserverImpl {
    // ── Iteration lifecycle ──

    fn on_iteration_start(&self, history_len: usize) -> Option<u32> {
        let Ok(mut ctrl) = self.ctrl.try_lock() else {
            return None;
        };

        ctrl.iteration += 1;
        let current_iter = ctrl.iteration;

        // Create conversation snapshot for rewind support.
        let usage = super::protocol::DebugUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        ctrl.create_conversation_snapshot(history_len, usage);

        Some(current_iter)
    }

    fn check_pending_injection(&self) {
        let Some(pending_arc) = self.pending_injection.as_ref() else {
            return;
        };

        let handles = match pending_arc.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };

        if let Some(_handles) = handles {
            // The bypass injection is handled externally —
            // SessionManager replaces the entire DebugObserverSlot.
            tracing::info!("DebugObserver: pending injection detected (bypass path)");
        }
    }

    // ── Pause / Resume / Rewind ──
    //
    // NOTE: `await_resume` is NOT on the trait because it needs &mut HistoryManager
    // which is owned by AgentLoop. It is called directly on DebugObserverImpl
    // (or via DebugObserverSlot::await_resume which delegates).

    async fn apply_rewind(&self, session_id: &str, history: &mut HistoryManager) {
        let mut ctrl = self.ctrl.lock().await;
        apply_rewind_locked(&mut ctrl, session_id, history);
    }

    async fn apply_rewind_and_patches(
        &self,
        session_id: &str,
        history: &mut HistoryManager,
        context_builder: &mut ContextBuilder,
    ) -> bool {
        let mut ctrl = self.ctrl.lock().await;

        // Apply rewind
        apply_rewind_locked(&mut ctrl, session_id, history);

        // Apply pending patches
        let mut patches_applied = false;
        if let Some(patches) = ctrl.pending_patches.take() {
            match context_builder.apply_patches(&patches) {
                Ok(()) => {
                    tracing::info!(
                        session_id = %session_id,
                        "Debug: pending patches applied to context builder and consumed"
                    );
                    patches_applied = true;
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "Debug: pending patches rejected (not applied)"
                    );
                }
            }
        }

        // Consume re-execute pending flag
        if ctrl.take_re_execute_pending() {
            tracing::info!(
                session_id = %session_id,
                "Debug: re-execute flag consumed, agent will process next message with updated context"
            );
        }

        patches_applied
    }

    // ── Phase tracking ──

    async fn on_phase_enter(&self, phase: DebugPhase) -> bool {
        let mut ctrl_guard = self.ctrl.lock().await;
        let old_phase = ctrl_guard.phase;
        ctrl_guard.phase = phase;

        // Push state change event
        let _ = self.event_tx.send(DebugEvent::StateChanged {
            old_phase,
            new_phase: phase,
            iteration: ctrl_guard.iteration,
        });

        false
    }

    fn on_phase_step(
        &self,
        phase: DebugPhase,
        input: Option<serde_json::Value>,
        output: Option<serde_json::Value>,
    ) {
        // Read iteration from controller (avoid holding lock across send).
        let iteration = {
            let Ok(ctrl) = self.ctrl.try_lock() else {
                return;
            };
            ctrl.iteration
        };
        let _ = self.event_tx.send(DebugEvent::Step {
            iteration,
            phase,
            input,
            output,
            usage: None,
        });
    }

    async fn on_phase_step_done(&self) {
        let mut ctrl_guard = self.ctrl.lock().await;
        if ctrl_guard.state == DebugState::Stepping {
            ctrl_guard.state = DebugState::Paused;
            let iteration = ctrl_guard.iteration;
            drop(ctrl_guard);

            let _ = self.event_tx.send(DebugEvent::ExecutionStateChanged {
                new_state: DebugState::Paused,
                iteration,
            });
            tracing::info!("Debug: stepping complete, auto-pausing");
        }
    }

    // ── Context ──

    async fn on_context_built(&self, req: ContextSnapshotRequest<'_>) {
        let Some(iter) = req.iteration else {
            return;
        };

        // Build the merged tool_definitions list (built-in + MCP).
        // ADR-067: MCP tools come from `req.mcp_tools` (the exact set
        // injected into `ChatRequest.tools`) — NOT `req.all_tools` filtered
        // by the `mcp_` prefix, which would over-count by including the
        // built-in `mcp_install` / `mcp_uninstall` tools.
        let mut mcp_tools: Vec<serde_json::Value> = Vec::new();
        for tool in req.mcp_tools {
            let spec = tool.spec();
            let val = serde_json::to_value(&spec).unwrap_or_default();
            mcp_tools.push(val);
        }
        // tool_defs_str is still needed below to materialize the full
        // NamedSection content for the Debug Panel. Cheap (typically
        // < 50KB) and reused via serde_json::Value::to_string() once.
        let mut all_defs: Vec<serde_json::Value> = req
            .context_builder
            .tool_definitions()
            .map(|defs| defs.to_vec())
            .unwrap_or_default();
        all_defs.extend(mcp_tools.iter().cloned());
        let tool_defs_str = serde_json::Value::Array(all_defs).to_string();

        tracing::info!(
            iter = iter,
            ws_has = req.context_builder.workspace_context().is_some(),
            ws_len = req.context_builder.workspace_context().map(|s| s.len()).unwrap_or(0),
            ws_preview = ?req.context_builder.workspace_context().map(|s| s.preview(80)),
            "capture_context_snapshot: workspace_context status"
        );

        // ADR-067: section list now comes from `compute_section_sizes`
        // (the always-on observability helper). The helper owns the
        // ordering and the section keys — both consumers (this
        // DevMode snapshot path and `process_llm_response_usage`'s
        // always-on emission) MUST go through it so the frontend sees
        // consistent section keys regardless of DevMode state.
        let base_sections =
            compute_section_sizes(req.context_builder, req.history, &mcp_tools, req.model);

        // Enrich each base section with DevMode-only metadata (full
        // content, token estimate, hash for `messages`). The byte size
        // on `base_sections` is the authoritative value; we don't
        // recompute it here.
        // ADR-054 step 4: hoist messages_snapshot + JSON out of the loop
        // so the controller's `store_messages` (post-loop) can reuse the
        // exact Arc without re-cloning. Hash is computed once.
        let messages_snapshot: Arc<Vec<ChatMessage>> = req.history.messages_arc();
        // `&*` rather than `&`: serialising the `Arc` itself would need
        // serde's `rc` feature, and the JSON is identical either way.
        let messages_json = serde_json::to_string(&*messages_snapshot).unwrap_or_default();
        let messages_hash = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(messages_json.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        let messages_token_estimate = crate::token::count_text(&messages_json);

        let mut named: Vec<NamedSection> = Vec::with_capacity(base_sections.len());
        for sec in base_sections {
            match sec.key.as_str() {
                "messages" => {
                    // ADR-054 step 4: store metadata only (no content);
                    // full content is lazy-loaded via `getSection`.
                    named.push(NamedSection {
                        key: sec.key,
                        content: SectionContent::metadata_only(
                            messages_json.len(),
                            messages_token_estimate,
                            messages_hash.clone(),
                        ),
                    });
                }
                "tool_definitions" => {
                    // Reuse the JSON we already serialized above.
                    named.push(NamedSection {
                        key: sec.key,
                        content: SectionContent::new(tool_defs_str.clone()),
                    });
                }
                _ => {
                    // Standard text section: re-materialize from
                    // ContextBuilder. We dispatch by key (rather than
                    // caching the source string at helper-call time) so
                    // the helper stays a pure size-only function with
                    // no borrow conflicts.
                    let content = section_text_from_builder(&sec.key, req.context_builder);
                    if let Some(text) = content {
                        named.push(NamedSection {
                            key: sec.key,
                            content: SectionContent::new(text),
                        });
                    }
                }
            }
        }

        let sections = ContextSnapshotSections { sections: named };

        let total_token_estimate = sections.total_token_estimate();

        // ADR-054 step 2: surface the ChatRequest control params so the
        // panel can show what the LLM was actually asked with.
        let request_params = RequestParams {
            model: req.model.to_string(),
            temperature: req.context_builder.temperature().map(|t| t as f64),
            max_tokens: req.max_tokens,
            reasoning_effort: req
                .context_builder
                .reasoning_effort()
                .map(|r| r.to_string()),
            thinking_mode: req.context_builder.thinking_mode().map(|s| s.to_string()),
        };

        let snapshot = ContextSnapshot {
            iteration: iter,
            built_at: chrono::Utc::now(),
            sections,
            total_token_estimate,
            request_params,
        };

        // Store in controller — snapshot metadata + lazy message content.
        let mut ctrl_guard = self.ctrl.lock().await;
        ctrl_guard.store_messages(iter, messages_snapshot);
        ctrl_guard.store_context_snapshot(snapshot.clone());

        // Push onContextBuilt event (carries request_params so the panel's
        // metadata bar renders without a follow-up RPC — ADR-054 step 2).
        let context_sections = super::protocol::ContextSections::from(&snapshot.sections);
        let sent = self.event_tx.send(DebugEvent::ContextBuilt {
            iteration: snapshot.iteration,
            sections: context_sections,
            total_token_estimate: snapshot.total_token_estimate,
            request_params: snapshot.request_params.clone(),
        });

        tracing::info!(
            iteration = snapshot.iteration,
            total_token_estimate,
            event_sent = sent,
            "Debug: context snapshot captured and event pushed"
        );
    }

    fn on_iteration_complete(&self, history: &HistoryManager) {
        let Ok(mut ctrl) = self.ctrl.try_lock() else {
            return;
        };
        let iteration = ctrl.iteration;
        if iteration == 0 {
            return;
        }
        ctrl.store_messages_with_meta(iteration, history.messages_arc());
        tracing::info!(
            iteration,
            message_count = history.len(),
            "Debug: iteration messages snapshot refreshed (incl. assistant reply)"
        );
    }

    fn apply_pending_patches(&self, builder: &mut ContextBuilder) -> bool {
        let Ok(mut ctrl_guard) = self.ctrl.try_lock() else {
            return false;
        };
        if let Some(patches) = ctrl_guard.pending_patches.take() {
            match builder.apply_patches(&patches) {
                Ok(()) => {
                    tracing::info!(
                        "Debug: pending patches applied to context builder after resume"
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "Debug: pending patches rejected after resume"
                    );
                    false
                }
            }
        } else {
            false
        }
    }

    fn take_re_execute_pending(&self) -> bool {
        let Ok(mut ctrl_guard) = self.ctrl.try_lock() else {
            return false;
        };
        if ctrl_guard.take_re_execute_pending() {
            tracing::info!("Debug: re_execute_pending consumed inside agent loop after resume");
            true
        } else {
            false
        }
    }

    fn is_dev_mode(&self) -> bool {
        true
    }
}

// ── Rewind Logic ──────────────────────────────────────────────────────

/// Core rewind logic — assumes the caller already holds the controller lock.
///
/// Extracted into a lock-free helper so that rewind + patches + re-execute
/// can be applied within a single lock acquisition.
fn apply_rewind_locked(ctrl: &mut DebugController, session_id: &str, history: &mut HistoryManager) {
    if let Some(target_iter) = ctrl.take_rewind_target() {
        let msg_count = ctrl
            .conversation_snapshots
            .iter()
            .find(|s| s.iteration == target_iter)
            .map(|s| s.message_count);

        if let Some(count) = msg_count {
            history.truncate_to(count);
            tracing::info!(
                session_id = %session_id,
                target_iteration = target_iter,
                messages_trimmed_to = count,
                "Debug rewind: history truncated"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                target_iteration = target_iter,
                "Debug rewind: no snapshot found for target iteration, history unchanged"
            );
        }

        ctrl.iteration = target_iter;
        tracing::debug!(
            session_id = %session_id,
            target_iteration = target_iter,
            "Debug: rewind applied, iteration reset"
        );
    }
}
