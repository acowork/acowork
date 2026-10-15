//! Inbound message handling for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 3).
//! Contains all methods related to processing inbound messages:
//! - `apply_user_op`: central dispatch for UserOp variants
//! - `poll_stop`: non-blocking stop-signal check with message buffering
//! - `drain_inbound_queue`: full inbound queue drain with message injection
//!
//! D1 Deduplication: the message-into-history injection logic appeared 3 times
//! (deferred UserMessage/SystemNotification/IntentMessage, live ditto, and
//! run_inner iteration-limit pause). The `inject_inbound_into_history()` helper
//! consolidates these into a single implementation.

use acowork_core::providers::traits::{ChatMessage, MessageRole};

use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent, ControlDecision, IterationResult};
use crate::agent::loop_session::strip_think_block;
use crate::agent::session_state::SessionStatus;
use crate::debug::controller::DebugState;

// ── D1 Deduplication: shared message injection helper ──

/// Inject an inbound message into the history as a ChatMessage.
///
/// Used by `drain_inbound_queue` (deferred + live paths) and by
/// `run_inner` (iteration-limit pause re-injection). Returns `true`
/// if the message was injected (user/system/intent), `false` if it
/// was a control message that doesn't go into history.
pub(super) fn inject_inbound_into_history(
    msg: InboundMessage,
    history: &mut crate::agent::history::HistoryManager,
) -> bool {
    match msg {
        InboundMessage::UserMessage(text) => {
            tracing::info!(
                text_preview = %text.chars().take(80).collect::<String>(),
                "inject_inbound_into_history: injecting UserMessage"
            );
            history.append(ChatMessage::user(text));
            true
        }
        InboundMessage::SystemNotification {
            notification_type,
            data,
        } => {
            tracing::info!(
                "inject_inbound_into_history: system notification: {} = {:?}",
                notification_type,
                data
            );
            // Changed from System to User — MiniMax API rejects non-first system messages
            history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("[system:{}] {}", notification_type, data),
                name: Some("system".to_string()),
                ..Default::default()
            });
            true
        }
        InboundMessage::IntentMessage {
            from,
            action,
            params,
            message_id,
            conversation_id,
            source_chat_session_id,
            repo,
            kind,
            in_reply_to,
            status,
        } => {
            tracing::info!(
                "inject_inbound_into_history: intent from {}: {} params={:?}",
                from,
                action,
                params
            );
            // Design 27 §3.2: the receiver sees the full envelope — reply
            // coordinates (conversation / source sessions) and the
            // authorized repo are part of the message, not ambient. Shared
            // renderer with the new-turn path (`session_task`) so the two
            // renderings cannot drift apart.
            let rendered = crate::agent::intent_turn::render_intent_message(
                &from,
                &action,
                &params,
                &message_id,
                &conversation_id,
                &source_chat_session_id,
                &repo,
                &kind,
                &in_reply_to,
                &status,
            );
            history.append(ChatMessage::user(rendered));
            true
        }
        // Control messages are NOT injected into history
        _ => false,
    }
}

impl AgentLoop {
    /// Apply a user operation delivered via the `send_inbound()` fast channel.
    ///
    /// This is the central dispatch point for all `UserOp` variants received
    /// through `InboundMessage::UserOperation`. It handles operations that
    /// must take effect immediately even while the agent loop is mid-execution.
    ///
    /// Returns `true` if the operation is an interrupt (caller should abort
    /// the current loop).
    pub(crate) fn apply_user_op(&mut self, op: &crate::agent::inbound::UserOp) -> bool {
        match op {
            crate::agent::inbound::UserOp::StopLoop { reason } => {
                tracing::info!(reason = %reason, "UserOp: stop loop");
                true
            }
            crate::agent::inbound::UserOp::ContinueLoop { reason } => {
                tracing::info!(reason = %reason, "UserOp: continue loop (no-op here; handled at iteration limit pause)");
                false
            }
            crate::agent::inbound::UserOp::ApprovalDecision { .. } => {
                tracing::debug!(
                    "UserOp: approval decision (no-op here; handled via approval subsystem)"
                );
                false
            }
            crate::agent::inbound::UserOp::QuestionAnswer { .. } => {
                tracing::debug!(
                    "UserOp: question answer (no-op here; handled via ask_user_question subsystem)"
                );
                false
            }
            // ADR-045: Cancel a single in-flight tool. Returns false (not
            // an interrupt) — the iteration continues normally. The actual
            // tool-cancellation is dispatched via `pending_tool_cancels`
            // by the loop_tools module (Step 1).
            crate::agent::inbound::UserOp::CancelTool { tool_call_id } => {
                tracing::info!(
                    tool_call_id = %tool_call_id,
                    "UserOp: cancel tool (handled by loop_tools::pending_tool_cancels)"
                );
                self.cancel_tool_by_id(tool_call_id);
                false
            }
            crate::agent::inbound::UserOp::UpdateRuntimeConfig(overrides) => {
                tracing::info!(
                    max_output_tokens = overrides.max_output_tokens,
                    max_iterations = overrides.max_iterations,
                    temperature = overrides.temperature,
                    context_window = overrides.context_window,
                    system_prompt_override = overrides.system_prompt_override.as_deref(),
                    shell_approval_threshold = overrides.shell_approval_threshold.as_deref(),
                    "UserOp: applying runtime config immediately in AgentLoop"
                );
                self.apply_runtime_config(overrides);
                // Push updated state to frontend immediately so the
                // ResultsPanel temperature display reflects the new value
                // without waiting for the next LLM iteration.
                self.emit_session_state();
                false
            }
        }
    }

    /// Non-blocking check for ALL control signals (Stop, Pause, DebugStop).
    ///
    /// This is the single, unified checkpoint that every blocking wait point
    /// in the agent loop calls.  It replaces the ad-hoc `poll_stop()` + scattered
    /// `DebugState` checks with one exhaustive decision.
    ///
    /// Checks four sources in order:
    /// 1. `pending_interrupt` — set by sub-modules when a control signal was
    ///    consumed during a nested blocking wait (e.g. approval subsystem)
    /// 2. `inbound_rx` — `InboundMessage::Stop` / `UserOp::StopLoop` from
    ///    the Gateway chat-panel stop button
    /// 3. `DebugController::state` — `Paused` / `Stopped` set by the debug
    ///    panel (checked via non-blocking `try_lock()`)
    /// 4. **ADR-044 Phase 3**: `SessionCore::cancel_handle` — lowest
    ///    priority; only fires `Stop` (no Pause semantics). This is the
    ///    *active* stop signal source: the MQTT
    ///    `ControlAction::StopGeneration` dispatcher in
    ///    `startup/gateway_loop.rs` calls `cancel(UserStop { ... })` here
    ///    before forwarding the inbound message, so the cancel becomes
    ///    observable on the next checkpoint regardless of whether the
    ///    session task is currently inside a `tokio::select!`. Pause
    ///    remains DebugController's exclusive responsibility — see
    ///    ADR §4.3.
    ///
    /// Non-stop, non-pause messages are buffered into `deferred_inbound`
    /// for later re-injection by `drain_inbound_queue()`.
    pub(crate) fn poll_control(&mut self) -> ControlDecision {
        // 1. Check pending_interrupt — set by sub-modules when a control signal
        //    was consumed during a nested blocking wait.
        if let Some(interrupt) = self.pending_interrupt.take() {
            return interrupt;
        }

        // 2. Check inbound channel for Stop messages
        while let Ok(msg) = self.inbound_rx.try_recv() {
            match msg {
                InboundMessage::Stop { .. } => {
                    return ControlDecision::Stop;
                }
                InboundMessage::UserOperation(op) => {
                    match &op {
                        crate::agent::inbound::UserOp::StopLoop { .. } => {
                            return ControlDecision::Stop;
                        }
                        // ADR-045: Cancel a single in-flight tool. Unlike
                        // StopLoop (which aborts the whole iteration),
                        // CancelTool is a tool-scoped signal — it must be
                        // dispatched IMMEDIATELY to the pending tokio task,
                        // not deferred to drain_inbound_queue at the end of
                        // the current iteration. Otherwise a long-running
                        // tool would receive its cancel signal 30+ seconds
                        // late, only to find it had already completed.
                        crate::agent::inbound::UserOp::CancelTool { tool_call_id } => {
                            tracing::info!(
                                tool_call_id = %tool_call_id,
                                "poll_control: immediate dispatch of CancelTool (skip deferral)"
                            );
                            self.cancel_tool_by_id(tool_call_id);
                            // Do NOT return Stop; the iteration continues
                            // and the cancelled tool will produce a normal
                            // tool_result via its own drop semantics.
                        }
                        _ => {
                            self.session
                                .deferred_inbound
                                .push(InboundMessage::UserOperation(op));
                        }
                    }
                }
                other => {
                    self.session.deferred_inbound.push(other);
                }
            }
        }

        // 3. Check debug controller state (non-blocking try_lock)
        if let Some(ctrl) = self.core.debug_observer.debug_ctrl()
            && let Ok(guard) = ctrl.try_lock()
        {
            match guard.state {
                DebugState::Paused => return ControlDecision::Pause,
                DebugState::Stopped => return ControlDecision::Stop,
                _ => {}
            }
        }

        // 4. ADR-044 Phase 2: CancelHandle (lowest priority).
        //    `is_cancelled()` is a single atomic load — zero contention
        //    even when called on every checkpoint. Only Stop is returned;
        //    Pause is DebugController's exclusive responsibility.
        //
        //    ADR-044 §4.5: the handle read here is the *current request's*
        //    handle (see `SessionCore::begin_new_request`). It is replaced
        //    with a fresh Active handle at every `run_inner` entry so that
        //    a Stop from a prior request cannot short-circuit the next.
        if self.session_core.cancel_handle().is_cancelled() {
            return ControlDecision::Stop;
        }

        ControlDecision::Continue
    }

    /// Non-blocking check for stop signals.
    ///
    /// Thin wrapper around [`poll_control`] for callers that only need
    /// to drain Stop messages (e.g. `run_inner` consuming redundant
    /// channel-based Stops after the Notify has already aborted the stream).
    pub(crate) fn poll_stop(&mut self) -> bool {
        self.poll_control() == ControlDecision::Stop
    }

    /// Drain inbound message queue (non-blocking).
    ///
    /// First processes any messages buffered by `poll_stop()` from
    /// the `deferred_inbound` stash, then drains the live channel.
    /// Injects external messages (user, system, intent) into history
    /// before each loop iteration. Applies size limits to prevent
    /// token explosion from oversized payloads.
    ///
    /// Returns `true` if at least one stop signal was found
    /// (the caller should stop the current agent loop).  ALL stop
    /// messages are consumed (not just the first one) to prevent
    /// residual stops from poisoning subsequent `run_inner()` calls.
    pub(crate) fn drain_inbound_queue(&mut self) -> bool {
        let mut should_stop = false;

        // ── Step 1: process messages deferred from poll_stop() ──
        // Detach the whole stash (swap with an empty Vec) so the loop below
        // can call apply_user_op(&mut self) — iterating
        // self.session.deferred_inbound in place would hold a borrow across
        // it. `mem::take` is one swap, not a drain + re-collect. Same idiom
        // as loop_tools.rs.
        let deferred = std::mem::take(&mut self.session.deferred_inbound);
        for msg in deferred {
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::Stop { reason } => {
                    tracing::info!(reason = %reason, "Received deferred stop signal (consumed)");
                    should_stop = true;
                }
                InboundMessage::ContinueExecution { .. } => {
                    tracing::debug!("Ignoring deferred ContinueExecution");
                }
                InboundMessage::ApprovalDecision { .. } => {
                    tracing::debug!("Ignoring deferred ApprovalDecision");
                }
                InboundMessage::QuestionAnswer { .. } => {
                    tracing::debug!("Ignoring deferred QuestionAnswer");
                }
                InboundMessage::UserOperation(user_op) => {
                    tracing::info!(
                        op = ?std::mem::discriminant(&user_op),
                        "drain_inbound_queue: processing deferred UserOperation"
                    );
                    if self.apply_user_op(&user_op) {
                        should_stop = true;
                    }
                }
                // D1 dedup: all injectable message types handled by helper
                other => {
                    inject_inbound_into_history(other, &mut self.session.history);
                }
            }
        }

        // ── Step 2: drain the live channel ──
        while let Ok(msg) = self.inbound_rx.try_recv() {
            // Enforce size limits before injecting
            let (msg, _truncated) = msg.enforce_size_limit();
            match msg {
                InboundMessage::Stop { reason } => {
                    tracing::info!(reason = %reason, "Received stop signal (consumed)");
                    should_stop = true;
                    // Consume and continue — multiple stops may be queued
                    // from rapid Stop button clicks.  We must drain ALL of them
                    // so subsequent run_inner() calls aren't poisoned.
                }
                InboundMessage::ContinueExecution { .. } => {
                    // Continue is only meaningful during iteration limit pause;
                    // during normal drain, ignore it.
                    tracing::debug!("Ignoring ContinueExecution during normal drain");
                }
                InboundMessage::ApprovalDecision { .. } => {
                    // Approval decisions are only meaningful during approval pause.
                    tracing::debug!("Ignoring ApprovalDecision during normal drain");
                }
                InboundMessage::QuestionAnswer { .. } => {
                    // Question answers are only meaningful during question wait.
                    tracing::debug!("Ignoring QuestionAnswer during normal drain");
                }
                InboundMessage::UserOperation(user_op) => {
                    tracing::info!(
                        op = ?std::mem::discriminant(&user_op),
                        "drain_inbound_queue: processing live UserOperation"
                    );
                    if self.apply_user_op(&user_op) {
                        should_stop = true;
                    }
                }
                // D1 dedup: all injectable message types handled by helper
                other => {
                    inject_inbound_into_history(other, &mut self.session.history);
                }
            }
        }
        should_stop
    }

    /// Handle agent stop (user clicked Stop or debug controller stopped).
    ///
    /// D3 deduplication: consolidates the nearly identical stopped-result
    /// construction from pre-tool stop and post-tool stop paths.
    /// Transitions to Idle, persists partial assistant response to JSONL,
    /// emits Stopped chunk event, and pushes debug step event.
    pub(crate) async fn handle_stopped(&mut self, content: &str) -> IterationResult {
        self.transition_status(SessionStatus::Idle);

        // Persist assistant message to JSONL conversation
        if let Some(ref conversation) = self.session.conversation {
            let assistant_text = strip_think_block(content);
            if !assistant_text.is_empty() {
                conversation.append_message("assistant", &assistant_text, None);
            }
        }

        // ADR-021: Remove streaming line after stop persistence
        self.session_core.remove_streaming_line();

        // Notify frontend via chunk channel
        let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
            content: content.to_string(),
        });

        // Debug: push step event and auto-pause if stepping
        self.core.debug_observer.on_phase_step(
            crate::debug::protocol::DebugPhase::Idle,
            None,
            Some(serde_json::json!({"stopped": true, "content": content})),
        );
        self.core.debug_observer.on_phase_step_done().await;

        IterationResult::Stopped(content.to_string())
    }

    /// ADR-045: Dispatch a single-tool cancel request.
    ///
    /// Looks up the per-tool cancel token registered by
    /// [`crate::agent::loop_tools`] and fires it. Unknown `tool_call_id`
    /// is a silent no-op (race vs. tool natural completion — tool already
    /// returned and removed itself from `pending_tool_cancels`).
    ///
    /// `send(true)` on a `watch::Sender` is fire-and-forget; the watcher on
    /// the tool-task side will trip its `select!` branch and exit
    /// gracefully. Process cleanup for in-flight child processes (e.g. shell
    /// `wait_with_output()`) is handled by the existing `ProcessGuard`
    /// Drop semantics in `tools/builtin/shell.rs` — no extra kill path.
    pub(crate) fn cancel_tool_by_id(&mut self, tool_call_id: &str) {
        match self.pending_tool_cancels.get(tool_call_id) {
            Some(tx) => {
                // watch::Sender::send is non-blocking and returns Err only
                // when there are no receivers — i.e. the tool task has
                // already returned and removed its receiver. Treat as no-op
                // (race vs. natural completion), same as "key not found".
                match tx.send(true) {
                    Ok(_) => {
                        tracing::info!(
                            tool_call_id = %tool_call_id,
                            "ADR-045: cancel signal fired to tool task"
                        );
                    }
                    Err(_) => {
                        tracing::debug!(
                            tool_call_id = %tool_call_id,
                            "ADR-045: cancel token fire ignored — receiver already dropped (tool done)"
                        );
                    }
                }
            }
            None => {
                tracing::debug!(
                    tool_call_id = %tool_call_id,
                    "ADR-045: cancel requested for unknown/done tool_call_id (no-op)"
                );
            }
        }
    }
}
