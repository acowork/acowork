//! Tool execution module
//!
//! Extracted from [`super::loop_`] to provide reusable parallel tool execution
//! shared between production [`AgentLoop::run`] and debug `DebugSessionTask`.
//!
//! Handles:
//! - Tool activation filtering (done at registry build time — no runtime check needed)
//! - Parallel execution with per-tool timeout + iteration deadline
//! - Result collection with original ordering
//! - Tool call preparation (dedup, history, JSONL persist, chunk emit)
//! - Pre/post-execution loop detection
//! - Tool result persistence and emission

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use acowork_core::providers::traits::{ChatMessage, ToolCall};
use acowork_core::tools::traits::{Tool, ToolContext};
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use crate::agent::loop_detector::{LoopDetectionResult, LoopPattern, ResponseLevel};
use crate::agent::session_state::SessionStatus;
use crate::error::{Result, RuntimeError};
use crate::security::approval_gate::{ApprovalGate, ApprovalRequest};
use crate::security::shell_risk::{self, ShellRisk};
use acowork_core::ShellApprovalThreshold;

use super::loop_::{AgentLoop, ControlDecision};
use super::loop_approval::{
    APPROVAL_TIMEOUT, APPROVAL_TIMEOUT_SECS, ApprovalDecision, ApprovalHandle,
};
use crate::agent::inbound::InboundMessage;
use crate::util::text::TextPreview;

impl AgentLoop {
    /// Execute tool calls in parallel with per-tool timeout and iteration-level deadline.
    ///
    /// Tool activation filtering is done at registry build time via
    /// [`ToolRegistry::activate`] — no runtime permission check needed here.
    ///
    /// Returns results in the same order as input tool calls.
    /// Individual tool failures are captured as error strings, not propagated.
    /// Returns (results, interrupt) — `interrupt` is `Some(ControlDecision)` when
    /// a control signal (Stop/Pause) was received during tool execution, telling
    /// the caller whether to exit or pause the loop.
    pub(crate) async fn execute_tools_parallel(
        &mut self,
        tool_calls: &[ToolCall],
    ) -> (Vec<(String, bool)>, Option<ControlDecision>) {
        if tool_calls.is_empty() {
            return (Vec::new(), None);
        }

        tracing::info!(
            tool_calls_count = tool_calls.len(),
            tools = ?tool_calls
                .iter()
                .map(|t| &t.function.name)
                .collect::<Vec<_>>(),
            "Executing tool calls"
        );

        // Phase 1: Execute tools in parallel with spawn + select + deadline
        let all_indices: Vec<usize> = (0..tool_calls.len()).collect();

        let tool_timeout = Duration::from_millis(self.core.config.timeouts.tool_timeout_ms);
        let iteration_timeout =
            Duration::from_millis(self.core.config.timeouts.iteration_timeout_ms);

        // Channel to collect results from spawned tasks
        // Each item: (original_index, (content, is_transient))
        let (tx, mut rx) = mpsc::channel::<(usize, (String, bool))>(tool_calls.len());

        // Clone shared state for spawned tasks
        let approval_handle = self.approval_handle.clone();
        let approval_gate = self.core.approval_gate.clone();
        let shell_threshold = *self.core.shell_approval_threshold();
        let use_gateway_approval = self.session_core.approval_handle.is_some();
        tracing::info!(
            use_gateway_approval,
            has_approval_gate = approval_gate.is_some(),
            threshold = ?shell_threshold,
            "Shell approval gate status"
        );

        // Spawn each tool as an independent task
        let session_id = self.session_core.session_id.clone();
        let chunk_tx = self.session_core.chunk_tx.clone();
        let tool_timeout_ms = self.core.config.timeouts.tool_timeout_ms;

        let handles: Vec<tokio::task::JoinHandle<()>> = all_indices
            .iter()
            .map(|&idx| {
                let tools = self.core.all_tools.clone();
                let tc = tool_calls[idx].clone();
                let tx = tx.clone();
                let approval_handle = approval_handle.clone();
                let approval_gate = approval_gate.clone();
                let work_dir = self.session_core.current_work_dir.read().unwrap().clone();
                let session_id = session_id.clone();
                // Per-call session context for session-aware tools
                // (`execute_with_context`). Resolved from THIS session's
                // loop state — never from agent-shared tool instances —
                // so concurrent sessions cannot cross-contaminate.
                // Built LAST among the clones below so `session_id` stays
                // available for the chunk-event path.
                let chunk_tx = chunk_tx.clone();
                let shell_risk_rules = self.core.shell_risk_rules.clone();
                // ADR-078 git-refresh nudge. Cloned into the spawned task
                // so the publish can happen inside `execute_single_tool`
                // (where the arguments are already parsed) without
                // borrowing `self` across the spawn.
                let git_nudge = self.core.git_nudge.clone();
                let agent_id = self.core.config.agent_id.clone();
                let tool_ctx = ToolContext {
                    work_dir,
                    session_id: session_id.clone(),
                    current_user_message: self.current_turn_user_message.clone(),
                    chat_session_id: self
                        .session
                        .conversation
                        .as_ref()
                        .and_then(|c| c.chat_session_id()),
                };

                // ADR-045: per-tool cancel token. Created BEFORE `spawn`
                // so the sender is registered into `pending_tool_cancels`
                // synchronously — eliminating the race where a user cancel
                // arrives between spawn and registration. The receiver is
                // moved into the spawned task and used as the cancel branch
                // of the `tokio::select!` below.
                let (cancel_tx, mut cancel_rx) = tokio::sync::watch::channel(false);
                self.pending_tool_cancels.insert(tc.id.clone(), cancel_tx);

                tokio::spawn(async move {
                    // Shell risk check: if this is a shell command and risk >= threshold,
                    // request user approval before execution.
                    let is_shell_tool = is_shell_tool(&tc.function.name);
                    if is_shell_tool {
                        let rules_snapshot = shell_risk_rules.clone();
                        // Gateway mode: use ApprovalHandle -> main loop routes
                        // the ApprovalDecision via route_inbound.
                        if use_gateway_approval {
                            if let Some(rejection) = check_shell_approval_handle(
                                &approval_handle,
                                &mut cancel_rx,
                                &tc.function.name,
                                &tc.function.arguments,
                                &shell_threshold,
                                &tc.id,
                                &rules_snapshot,
                            )
                            .await
                            {
                                let _ = tx.send((idx, (rejection, false))).await;
                                return;
                            }
                        } else if let Some(ref gate) = approval_gate {
                            // CLI / test mode: use ApprovalGate trait directly
                            if let Some(rejection) = check_shell_approval(
                                gate.as_ref(),
                                &mut cancel_rx,
                                &tc.function.name,
                                &tc.function.arguments,
                                &shell_threshold,
                                &tc.id,
                                &rules_snapshot,
                            )
                            .await
                            {
                                let _ = tx.send((idx, (rejection, false))).await;
                                return;
                            }
                        }
                    }

                    // ADR-045: Tool execution progress heartbeat.
                    // Spawned BEFORE the `select!` so it starts ticking
                    // immediately. The first tick is skipped (interval
                    // fires at 5s, not 0s), so short tools (<5s) never
                    // emit a heartbeat event. The heartbeat task is
                    // aborted after the tool completes/cancels/times out.
                    //
                    // Uses `try_send_chunk` (non-blocking) and
                    // `try_send` (non-blocking) so the heartbeat cannot
                    // stall the tool execution or the result channel.
                    let heartbeat_task =
                        if let (Some(sid), Some(ct)) = (session_id.as_ref(), chunk_tx.as_ref()) {
                            let sid = sid.clone();
                            let ct = ct.clone();
                            let tool_call_id = tc.id.clone();
                            let heartbeat_interval =
                                acowork_core::timeout_config::constants::TOOL_HEARTBEAT;
                            Some(tokio::spawn(async move {
                                let mut interval = tokio::time::interval(heartbeat_interval);
                                // Skip the first (immediate) tick so the
                                // first heartbeat lands at 5s, not 0s.
                                interval.tick().await;
                                let tool_start = std::time::Instant::now();
                                loop {
                                    interval.tick().await;
                                    let elapsed = tool_start.elapsed();
                                    let event = crate::agent::loop_::ChunkEvent::ToolProgress {
                                        session_id: sid.clone(),
                                        tool_call_id: tool_call_id.clone(),
                                        elapsed_ms: elapsed.as_millis() as u64,
                                        timeout_ms: tool_timeout_ms,
                                    };
                                    // try_send: non-blocking; if the
                                    // channel is full or closed, skip
                                    // this heartbeat.
                                    if ct
                                        .try_send(crate::agent::loop_::SessionChunkEvent {
                                            session_id: sid.clone(),
                                            event,
                                        })
                                        .is_err()
                                    {
                                        break;
                                    }
                                    if elapsed.as_millis() as u64 >= tool_timeout_ms {
                                        break;
                                    }
                                }
                            }))
                        } else {
                            None
                        };

                    // ADR-045: tool completion vs. user cancel. When the
                    // cancel branch wins, the inner `execute_single_tool`
                    // future is dropped, which drops the `ProcessGuard`
                    // child process handle and triggers its `Drop` impl
                    // (`child.kill()` + `child.wait()`) — see
                    // `tools/builtin/shell.rs` for the Drop semantics.
                    let result = tokio::select! {
                        // Branch A: tool completes (or per-tool timeout fires)
                        res = tokio::time::timeout(
                            tool_timeout,
                            execute_single_tool(
                                &tools,
                                &tc,
                                &tool_ctx,
                                git_nudge.as_ref(),
                                &agent_id,
                            ),
                        ) => {
                            match res {
                                Ok((content, transient)) => (content, transient),
                                Err(_) => (
                                    format!(
                                        "Error: Tool '{}' timed out after {}ms",
                                        tc.function.name,
                                        tool_timeout.as_millis()
                                    ),
                                    false,
                                ),
                            }
                        }
                        // Branch B: user cancelled via CancelTool MQTT command.
                        // `wait_for(|cancelled| *cancelled)` is cancel-safe
                        // and does not consume the receiver.
                        _ = cancel_rx.wait_for(|cancelled| *cancelled) => {
                            tracing::info!(
                                tool_call_id = %tc.id,
                                tool_name = %tc.function.name,
                                "ADR-045: tool cancelled by user — returning CancelledByUser to LLM"
                            );
                            (
                                // The "Error: " prefix is load-bearing —
                                // `looks_like_tool_error` keys off it to
                                // surface a red ✗ in the UI. The plain
                                // "Cancelled by user ..." text without
                                // the prefix would render as a green ✓,
                                // which silently misleads users into
                                // thinking the tool succeeded.
                                format!(
                                    "Error: Cancelled by user while running '{}'",
                                    tc.function.name
                                ),
                                false,
                            )
                        }
                    };

                    // Abort the heartbeat task — tool is done/cancelled.
                    // Non-blocking; the task will be dropped at the next
                    // await point (which is the interval.tick().await).
                    if let Some(ht) = heartbeat_task {
                        ht.abort();
                    }

                    let _ = tx.send((idx, result)).await;
                    // `cancel_rx` is dropped here when this async block
                    // ends; after that point, any further `send(true)`
                    // from `cancel_tool_by_id` returns Err (no receivers
                    // alive) and is silently ignored. The map entry
                    // remains until overwritten by a future spawn with
                    // the same id — this is acceptable because
                    // `cancel_tool_by_id` already treats "key not found"
                    // as a no-op.
                })
            })
            .collect();

        // Drop the remaining sender so rx.recv() returns None when all tasks complete
        drop(tx);

        // Collect results with iteration-level deadline.
        // In Gateway mode, also listen for approval requests from spawned tasks.
        let mut deadline = Instant::now() + iteration_timeout;
        let mut collected: Vec<(usize, (String, bool))> = Vec::with_capacity(all_indices.len());
        let total = all_indices.len();
        let mut interrupt: Option<ControlDecision> = None;

        if use_gateway_approval {
            // ── Gateway mode: 5-way select ──
            // (results / timeout / approval-request / inbound-decision / cancel)
            //
            // Opening drain: if a prior phase (e.g. `await_question_answer`)
            // buffered any ApprovalDecision into `deferred_inbound`, route
            // them now so in-flight approvals registered before this call
            // are not stranded for the full APPROVAL_TIMEOUT.
            if !self.pending_approvals.is_empty() {
                let deferred: Vec<InboundMessage> =
                    std::mem::take(&mut self.session.deferred_inbound);
                let (decisions, others): (Vec<_>, Vec<_>) = deferred
                    .into_iter()
                    .partition(|m| matches!(m, InboundMessage::ApprovalDecision { .. }));
                self.session.deferred_inbound = others;
                for msg in decisions {
                    self.route_inbound(msg).await;
                }
            }
            // Approval requests are registered into `self.pending_approvals`
            // and announced to the Desktop via `send_tool_approval_needed`.
            // User decisions arriving on `inbound_rx` are routed by
            // `route_inbound`, which resolves the matching oneshot directly.
            // This is a flat loop - no recursion, no deferred_inbound
            // re-buffering for ApprovalDecision messages.
            let mut approval_deadline: Option<Instant> = None;

            while collected.len() < total {
                // ADR-044 §4.5: bind the *current request's* handle to a
                // local so the Arc inside it outlives the `cancelled()`
                // future. Re-read on every iteration so we observe any
                // slot swap that happened mid-tool-execution (none is
                // expected within one request - `begin_new_request` is
                // only called from `run_inner` entry - but defensive).
                let cancel_handle = self.session_core.cancel_handle();
                tokio::select! {
                    entry = rx.recv() => {
                        match entry {
                            Some((idx, result)) => collected.push((idx, result)),
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        tracing::warn!(
                            "Iteration timeout reached ({}ms), aborting {} remaining tool(s)",
                            iteration_timeout.as_millis(),
                            total - collected.len()
                        );
                        for handle in &handles {
                            handle.abort();
                        }
                        break;
                    }
                    // New approval request from a spawned tool task.
                    // Register it in `pending_approvals` and announce it;
                    // do NOT block waiting for the decision here - the
                    // inbound_rx branch below resolves it whenever the
                    // user responds.
                    approval_req = self.approval_rx.recv() => {
                        match approval_req {
                            Some((req, decision_tx)) => {
                                // Design 27 §5.5.4 (plan 3.1): a session the
                                // Gateway stamped human-free has nobody to
                                // answer — never announce the approval (no
                                // WaitingApproval state, no 5-min timeout
                                // wait); reject immediately so the model
                                // continues without the gated tool. A
                                // responder turn WITH humans keeps the
                                // bounded wait: the approval is also dropped
                                // as an `interaction_request` card below
                                // (WaitingApproval is time-boxed, not the
                                // indefinite pause §6.5 R3 forbids).
                                if self.turn_surface.has_human == Some(false) {
                                    let _ = decision_tx.send(ApprovalDecision {
                                        approved: false,
                                        allow_all_session: false,
                                        reason: Some(
                                            "this chat session has no human participant to approve"
                                                .to_string(),
                                        ),
                                    });
                                    tracing::info!(
                                        "Auto-rejected tool approval in a human-free chat session"
                                    );
                                    continue;
                                }
                                let request_id = uuid::Uuid::new_v4().to_string();
                                self.pending_approvals.insert(
                                    request_id.clone(),
                                    decision_tx,
                                );
                                self.send_tool_approval_needed(&request_id, &req);
                                self.transition_status(
                                    crate::agent::session_state::SessionStatus::WaitingApproval {
                                        request_id: request_id.clone(),
                                    },
                                );
                                // §5.5.4 (plan 3.1): the approval is also a
                                // Chat-session card. Summary only — tool
                                // name + risk reason, never the command line
                                // or tool payload (§2.2); the answer resolves
                                // the same request id through either surface.
                                self.write_interaction_card(
                                    serde_json::json!({
                                        "kind": "approval",
                                        "request_id": request_id.clone(),
                                        "question": format!(
                                            "Approve `{}`? {}",
                                            req.tool_name, req.reason
                                        ),
                                    }),
                                    &format!("Tool approval: {}", req.tool_name),
                                )
                                .await;
                                // First in-flight approval starts the
                                // 5-minute approval deadline; subsequent
                                // concurrent approvals share it.
                                if approval_deadline.is_none() {
                                    approval_deadline = Some(
                                        Instant::now() + APPROVAL_TIMEOUT,
                                    );
                                }
                                tracing::info!(
                                    request_id = %request_id,
                                    in_flight = self.pending_approvals.len(),
                                    "Approval request registered"
                                );
                                // Re-anchor the iteration deadline on each
                                // approval registration: a user decision can
                                // take minutes and must not count against the
                                // iteration deadline. Unlike the old recursive
                                // handle_approval_request (which preserved the
                                // *remaining* time across the blocking wait),
                                // this grants a fresh full iteration_timeout
                                // from the moment the approval is registered —
                                // deliberately more lenient, and simpler now
                                // that the wait is non-blocking (the deadline
                                // keeps ticking while the decision is pending).
                                deadline = Instant::now() + iteration_timeout;
                            }
                            None => {
                                tracing::warn!("Approval channel closed unexpectedly");
                            }
                        }
                    }
                    // User decision (ApprovalDecision / Stop / other inbound)
                    // arriving from Gateway. `route_inbound` resolves the
                    // matching oneshot, or cancels all pending on Stop.
                    //
                    // Only polled while approvals are in flight: a closed
                    // inbound channel (common in unit tests where the
                    // inbound_tx sender is dropped) must NOT terminate the
                    // tool-result collection loop early.
                    inbound = async {
                        if self.pending_approvals.is_empty() {
                            std::future::pending::<
                                Option<crate::agent::inbound::InboundMessage>,
                            >()
                            .await
                        } else {
                            self.inbound_rx.recv().await
                        }
                    } => {
                        match inbound {
                            Some(msg) => {
                                self.route_inbound(msg).await;
                                // Control signals may have arrived during the
                                // decision wait (Stop / Pause via InboundMessage).
                                match self.poll_control() {
                                    c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                        for handle in &handles {
                                            handle.abort();
                                        }
                                        interrupt = Some(c);
                                        break;
                                    }
                                    ControlDecision::Continue => {}
                                }
                            }
                            None => {
                                // Channel closed while approvals are pending:
                                // they will never be answered - reject now
                                // instead of waiting for the timeout.
                                let count = self.pending_approvals.len();
                                let reason =
                                    "Inbound channel closed; approval can never be answered"
                                        .to_string();
                                let ids: Vec<String> = self
                                    .pending_approvals
                                    .keys()
                                    .cloned()
                                    .collect();
                                for (_, tx) in self.pending_approvals.drain() {
                                    let _ = tx.send(ApprovalDecision {
                                        approved: false,
                                        allow_all_session: false,
                                        reason: Some(reason.clone()),
                                    });
                                }
                                // §5.5.4: the chat card for a request that can
                                // never be answered must not stay open.
                                for id in ids {
                                    self.close_chat_card("approval", &id, "unanswerable")
                                        .await;
                                }
                                tracing::warn!(
                                    count = count,
                                    "Inbound channel closed; auto-rejected pending approvals"
                                );
                            }
                        }
                    }
                    // Approval timeout: auto-reject all still-pending
                    // approvals so the loop cannot hang forever when the
                    // user walks away without answering.
                    _ = async {
                        match approval_deadline {
                            Some(d) => tokio::time::sleep_until(d).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        let count = self.pending_approvals.len();
                        let reason = format!(
                            "tool approval timed out after {}s",
                            APPROVAL_TIMEOUT_SECS
                        );
                        let ids: Vec<String> =
                            self.pending_approvals.keys().cloned().collect();
                        for (_, tx) in self.pending_approvals.drain() {
                            let _ = tx.send(ApprovalDecision {
                                approved: false,
                                allow_all_session: false,
                                reason: Some(reason.clone()),
                            });
                        }
                        // §5.5.4: a timed-out approval is resolved — close its
                        // chat card, otherwise it renders forever as an
                        // answerable request in a session that has moved on.
                        for id in ids {
                            self.close_chat_card("approval", &id, "timeout").await;
                        }
                        tracing::warn!(
                            count = count,
                            timeout_secs = APPROVAL_TIMEOUT_SECS,
                            "Pending approvals timed out, auto-rejecting"
                        );
                        approval_deadline = None;
                    }
                    // Cancellation handle - fires when external sources
                    // (MQTT `StopGeneration` dispatcher in
                    // `startup/gateway_loop.rs`, debug server Stop, CLI
                    // cancel, test harness) call `cancel()` on this
                    // session's `CancelHandle`.
                    //
                    // ADR-044 Phase 3: replaces the legacy `urgent_stop`
                    // Notify, which was only triggered in the debug path
                    // and consequently never fired under the production
                    // MQTT stop flow (the root cause of the TTFT stop bug).
                    // The handle's level-triggered `Notify` + persistent
                    // `AtomicU8` state means this branch wakes within
                    // microseconds of `cancel()` - long before the 500ms
                    // idle-poll fallback would observe the inbound Stop
                    // message.
                    //
                    // ADR-044 §4.5: this is the *current request's* handle
                    // - see `SessionCore::begin_new_request` for how the
                    // slot is swapped at every `run_inner` entry.
                    _ = cancel_handle.cancelled() => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal via cancellation handle - aborting tools");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                    // Periodic control polling during tool execution.
                    // Without this branch, a slow tool (e.g. file_read on large file)
                    // would block the select! at rx.recv() until timeout, making
                    // control signals unresponsive for potentially minutes.
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal detected during tool execution - aborting");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                }
            }

            // Iteration cleanup: any approvals still in flight (loop exited
            // via timeout / interrupt / all tools completed while a dialog
            // was unanswered) are auto-rejected so the spawned tasks do
            // not stay parked on their oneshots forever.
            if !self.pending_approvals.is_empty() {
                tracing::warn!(
                    count = self.pending_approvals.len(),
                    "Auto-rejecting pending approvals at iteration end"
                );
                let reason = "Iteration aborted before user response".to_string();
                let ids: Vec<String> = self.pending_approvals.keys().cloned().collect();
                for (request_id, tx) in self.pending_approvals.drain() {
                    tracing::warn!(
                        request_id = %request_id,
                        reason = %reason,
                        "Auto-rejecting pending approval at iteration end"
                    );
                    let _ = tx.send(ApprovalDecision {
                        approved: false,
                        allow_all_session: false,
                        reason: Some(reason.clone()),
                    });
                }
                // §5.5.4: the requests are gone, so their chat cards must go
                // too — this is the site that fires when a stop lands while an
                // approval dialog is still unanswered.
                for id in ids {
                    self.close_chat_card("approval", &id, "turn_ended").await;
                }
                // Let a reconnecting Desktop know no stale dialogs remain.
                let _ = self.session_core.try_send_chunk(
                    crate::agent::loop_::ChunkEvent::ClearRetainedEvent {
                        event_type: "tool_approval_needed".to_string(),
                    },
                );
            }
        } else {
            // ── CLI / test mode: 4-way select (results / timeout / stop / notify) ──
            while collected.len() < total {
                // ADR-044 §4.5: bind the *current request's* handle to a
                // local (see the gateway-mode branch above for the full
                // rationale on the binding pattern).
                let cancel_handle = self.session_core.cancel_handle();
                tokio::select! {
                    entry = rx.recv() => {
                        match entry {
                            Some(entry) => collected.push(entry),
                            None => break,
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        tracing::warn!(
                            "Iteration timeout reached ({}ms), aborting {} remaining tool(s)",
                            iteration_timeout.as_millis(),
                            total - collected.len()
                        );
                        for handle in &handles {
                            handle.abort();
                        }
                        break;
                    }
                    _ = cancel_handle.cancelled() => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal via cancellation handle — aborting tools");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                    _ = tokio::time::sleep(Duration::from_millis(500)) => {
                        match self.poll_control() {
                            c @ (ControlDecision::Stop | ControlDecision::Pause) => {
                                tracing::info!(signal = ?c, "Control signal detected during tool execution — aborting");
                                for handle in &handles {
                                    handle.abort();
                                }
                                interrupt = Some(c);
                                break;
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                }
            }
        }

        // ADR-045: drop this iteration's per-tool cancel tokens now that the
        // collection loop has exited (normal completion, timeout, or
        // interrupt). Without this the map would accumulate one entry per
        // tool_call_id across iterations. `cancel_tool_by_id` treats a
        // missing key as a no-op, so removing here is safe even if a tool
        // task is still winding down — its `cancel_rx` receiver is dropped
        // when the task ends, making any later `send(true)` a silent no-op
        // regardless.
        for tc in tool_calls {
            self.pending_tool_cancels.remove(&tc.id);
        }

        // Build final results in original order
        let results: Vec<(String, bool)> = (0..tool_calls.len())
            .map(|idx| {
                if let Some(pos) = collected.iter().find(|(i, _)| *i == idx) {
                    pos.1.clone()
                } else {
                    (
                        format!(
                            "Error: iteration timed out, tool {} not completed",
                            tool_calls[idx].function.name
                        ),
                        false,
                    )
                }
            })
            .collect();

        // If iteration timed out with incomplete tools, add a system note
        let incomplete_count = results
            .iter()
            .filter(|(r, _)| r.contains("iteration timed out"))
            .count();
        if incomplete_count > 0 {
            tracing::warn!(
                incomplete_count,
                "Iteration timed out with incomplete tool(s)"
            );
        }

        (results, interrupt)
    }
}

/// Execute a single tool call against the tool registry.
///
/// Returns `(content, is_transient)` where `is_transient` indicates whether
/// the tool result should be treated as one-shot (ADR-032 C3a).
/// No tool currently uses the transient path — the `is_transient` flag
/// is preserved for future hypothetical one-shot tools. Error paths
/// that don't reach the tool's `execute()` method always return
/// `is_transient = false`.
///
/// `git_nudge` / `agent_id` drive the ADR-078 git-refresh nudge. They
/// are passed in (rather than read from a global) so the nudge reuses
/// the `params` already parsed here — the same
/// [`crate::tools::arguments::resolve`] result the tool itself sees,
/// including its natural-language recovery. Re-parsing
/// `tool_call.function.arguments` a second time would risk the nudge
/// disagreeing with what actually ran. `None` disables the nudge
/// (tests, CLI mode).
pub(crate) async fn execute_single_tool(
    tools: &[Arc<dyn Tool>],
    tool_call: &ToolCall,
    ctx: &ToolContext,
    git_nudge: Option<&crate::agent::git_nudge::GitNudgeSlots>,
    agent_id: &str,
) -> (String, bool) {
    let work_dir = ctx.work_dir.as_deref();
    let tool_name = &tool_call.function.name;
    let params_str = &tool_call.function.arguments;

    // Parse arguments before tool lookup — we need to detect the
    // TOOL_CALL_INCOMPLETE marker (valid JSON injected by streaming assembler)
    // and reject genuinely unparseable arguments (e.g. LLM hallucinated output).
    // Early return on parse failure avoids the old silent "{}" degradation
    // that caused LLM retry loops.
    //
    // Route through `arguments::resolve` instead of bare `serde_json::from_str` so
    // we recover JSON wrapped in natural-language prefixes (DeepSeek 2026-08-24
    // incident) and surface the original content as a hint in the returned error
    // message — letting the LLM see its raw intent on the next iteration.
    let params: serde_json::Value = match crate::tools::arguments::resolve(params_str) {
        crate::tools::arguments::ResolvedArgs::Valid(v)
        | crate::tools::arguments::ResolvedArgs::Recovered { value: v, .. } => v,
        crate::tools::arguments::ResolvedArgs::Invalid { raw, reason } => {
            tracing::warn!(
                tool = %tool_name,
                params_len = raw.len(),
                reason = %reason,
                params_preview = %raw.preview(200),
                "Failed to parse tool call arguments as JSON — returning error to LLM with raw content as hint"
            );
            return (
                format!(
                    "Error: Tool '{tool_name}' arguments are not valid JSON and could not be parsed: {reason}.\n\
                     This call was NOT executed. Your original raw arguments are preserved below for reference:\n\n\
                     --- raw arguments ---\n{}\n--- end ---",
                    raw.preview(200)
                ),
                false,
            );
        }
    };

    // Detect truncated/incomplete tool calls: the streaming assembler injects a
    // special error marker when arguments are truncated. Skip actual execution
    // and return a clear error so the LLM knows to regenerate (not retry blindly).
    if let Some("TOOL_CALL_INCOMPLETE") = params.get("error").and_then(|v| v.as_str()) {
        return (
            params
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Tool call arguments were not valid JSON")
                .to_string(),
            false,
        );
    }

    // Find the tool
    let tool = tools.iter().find(|t| {
        let spec = t.spec();
        spec.name == *tool_name
    });

    match tool {
        Some(tool) => match tool.execute_with_context(params.clone(), ctx).await {
            Ok(result) => {
                // ADR-078 git-refresh nudge. Fires for every shell tool
                // whose command may have mutated index/HEAD — `git commit`
                // writes solely inside the gitdir, which the workspace
                // PollWatcher (ADR-058, NonRecursive) never observes, so
                // without this the Git Status Bar kept a stale count.
                //
                // Deliberately not gated on `result.ok`: a `git commit`
                // that failed partway (hook rejected, nothing staged)
                // still leaves a mutated index behind, and a user staring
                // at a wrong count deserves the refresh.
                //
                // Placement note: the shell tool is named by the
                // *registry* entry, not by a hardcoded list, so an
                // ADR-030 sidecar or a future shell alias is covered by
                // construction. Read-only tools never reach here.
                if is_shell_tool(&tool.name()) {
                    crate::agent::git_nudge::maybe_nudge_after_shell(
                        git_nudge,
                        agent_id,
                        work_dir,
                        &params,
                    )
                    .await;
                }
                // ADR-052: All tools return non-transient results. The transient
                // mechanism was specific to `context_recall` (ADR-032) to prevent a
                // recall -> compress -> recall death loop. The compression surface
                // was retired entirely.
                let transient = false;
                let content = if result.ok {
                    result.content
                } else {
                    // Include both the error output (stdout/stderr) and the error code
                    if result.content.is_empty() {
                        format!("Error: {}", result.error.unwrap_or_default())
                    } else {
                        format!(
                            "{}\nError: {}",
                            result.content,
                            result.error.as_deref().unwrap_or("unknown error")
                        )
                    }
                };
                (content, transient)
            }
            Err(e) => (format!("Tool execution error: {e}"), false),
        },
        None => (format!("Unknown tool: {tool_name}"), false),
    }
}

///
/// Returns `Some(error_message)` if the command was rejected by the user,
/// or `None` if the command can proceed (approved or below threshold).
async fn check_shell_approval(
    gate: &dyn ApprovalGate,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    tool_name: &str,
    params_json: &str,
    threshold: &ShellApprovalThreshold,
    tool_call_id: &str,
    rules: &crate::security::shell_risk::ShellRiskRules,
) -> Option<String> {
    // Parse the command from shell tool params
    let params: serde_json::Value = match serde_json::from_str(params_json) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("check_shell_approval: failed to parse shell params: {}", e);
            return None; // Can't parse params, let the tool handle the error
        }
    };

    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return None;
    }

    // Assess risk (with no provenance lookup in the spawned task context)
    let assessment = shell_risk::assess_shell_risk(command, |_path| None, rules);

    // Blocked commands are rejected unconditionally — this is a hard safety
    // floor that even "Auto-approve" cannot bypass (e.g. `rm -rf /`).
    if assessment.risk == ShellRisk::Blocked {
        return Some(format!(
            "Error: Shell command was blocked for safety reasons. Command: {}\nReason: {}",
            command, assessment.reason
        ));
    }

    // "Auto-approve" threshold: skip approval entirely (Blocked handled above).
    if *threshold == ShellApprovalThreshold::AutoApprove {
        return None;
    }

    // Convert ShellApprovalThreshold to ShellRisk for comparison
    let threshold_risk = match threshold {
        ShellApprovalThreshold::Low => ShellRisk::Low,
        ShellApprovalThreshold::Medium => ShellRisk::Medium,
        ShellApprovalThreshold::High => ShellRisk::High,
        ShellApprovalThreshold::AutoApprove => unreachable!(), // handled above
    };

    // Check if risk meets/exceeds threshold
    if !risk_meets_threshold(assessment.risk, threshold_risk) {
        return None; // Risk below threshold, proceed normally
    }

    // Build approval request
    let approval_req = ApprovalRequest {
        tool_name: tool_name.to_string(),
        action: command.to_string(),
        risk_level: assessment.risk,
        reason: assessment.reason.clone(),
        executable_paths: assessment.executable_paths.clone(),
        provenance_elevated: assessment.provenance_elevated,
        tool_call_id: tool_call_id.to_string(),
    };

    tracing::info!(
        risk = %assessment.risk.label(),
        command = %command,
        reason = %assessment.reason,
        "Requesting user approval for shell command"
    );

    // Request approval. ADR-045: also listen on cancel_rx so a CancelTool
    // signal interrupts the approval wait.
    let response = tokio::select! {
        r = gate.request_approval(&approval_req) => r,
        _ = cancel_rx.wait_for(|cancelled| *cancelled) => {
            tracing::info!(
                tool_call_id = %tool_call_id,
                command = %command,
                "Tool cancelled while waiting for approval (CLI mode)"
            );
            return Some(format!(
                "Error: Cancelled by user while waiting for approval of '{}'",
                command
            ));
        }
    };

    match response {
        crate::security::approval_gate::ApprovalResponse::Approved => {
            tracing::info!(command = %command, "Shell command approved by user");
            None
        }
        crate::security::approval_gate::ApprovalResponse::Rejected => {
            tracing::info!(command = %command, "Shell command rejected by user");
            Some(format!(
                "Error: Shell command was rejected by the user based on risk assessment.\n\
                 Command: {}\nRisk level: {}\nReason: {}",
                command,
                assessment.risk.label(),
                assessment.reason
            ))
        }
        crate::security::approval_gate::ApprovalResponse::AlwaysAllow { .. } => {
            tracing::info!(command = %command, "Shell command approved (always allow)");
            None
        }
    }
}

/// Check if a given risk level meets or exceeds the threshold.
/// Risk ordering: Low < Medium < High < Blocked.
fn risk_meets_threshold(risk: ShellRisk, threshold: ShellRisk) -> bool {
    fn risk_ordinal(r: ShellRisk) -> u8 {
        match r {
            ShellRisk::Low => 0,
            ShellRisk::Medium => 1,
            ShellRisk::High => 2,
            ShellRisk::Blocked => 3,
        }
    }
    risk_ordinal(risk) >= risk_ordinal(threshold)
}

/// True for the shell tool names registered by the Runtime.
///
/// Single source for two call sites that must agree: the approval gate
/// (which decides whether to ask the user) and the ADR-078 git-refresh
/// nudge (which decides whether to look at git state again). A shell
/// reached by one and missed by the other would be a security gap or a
/// stale-UI bug respectively, so the names live here once rather than
/// being spelled out at both sites.
fn is_shell_tool(name: &str) -> bool {
    matches!(name, "bash" | "powershell" | "pwsh" | "shell")
}

/// Check if a shell command requires user approval via ApprovalHandle (Gateway mode).
///
/// Same risk assessment as `check_shell_approval`, but uses the unified pause
/// architecture: sends request to AgentLoop main loop via ApprovalHandle mpsc,
/// which emits ChunkEvent::ToolApprovalNeeded to Gateway and blocks on inbound_rx
/// until the user's ApprovalDecision arrives (no timeout).
async fn check_shell_approval_handle(
    handle: &ApprovalHandle,
    cancel_rx: &mut tokio::sync::watch::Receiver<bool>,
    tool_name: &str,
    params_json: &str,
    threshold: &ShellApprovalThreshold,
    tool_call_id: &str,
    rules: &crate::security::shell_risk::ShellRiskRules,
) -> Option<String> {
    // Parse the command from shell tool params
    let params: serde_json::Value = match serde_json::from_str(params_json) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                "check_shell_approval_handle: failed to parse shell params: {}",
                e
            );
            return None;
        }
    };

    let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return None;
    }

    // Assess risk (with no provenance lookup in the spawned task context)
    let assessment = shell_risk::assess_shell_risk(command, |_path| None, rules);

    // Blocked commands are rejected unconditionally — this is a hard safety
    // floor that even "Auto-approve" cannot bypass (e.g. `rm -rf /`).
    if assessment.risk == ShellRisk::Blocked {
        return Some(format!(
            "Error: Shell command was blocked for safety reasons. Command: {}\nReason: {}",
            command, assessment.reason
        ));
    }

    // "Auto-approve" threshold: skip approval entirely (Blocked handled above).
    if *threshold == ShellApprovalThreshold::AutoApprove {
        return None;
    }

    // Convert ShellApprovalThreshold to ShellRisk for comparison
    let threshold_risk = match threshold {
        ShellApprovalThreshold::Low => ShellRisk::Low,
        ShellApprovalThreshold::Medium => ShellRisk::Medium,
        ShellApprovalThreshold::High => ShellRisk::High,
        ShellApprovalThreshold::AutoApprove => unreachable!(),
    };

    // Check if risk meets/exceeds threshold
    if !risk_meets_threshold(assessment.risk, threshold_risk) {
        return None;
    }

    // Build approval request
    let approval_req = ApprovalRequest {
        tool_name: tool_name.to_string(),
        action: command.to_string(),
        risk_level: assessment.risk,
        reason: assessment.reason.clone(),
        executable_paths: assessment.executable_paths.clone(),
        provenance_elevated: assessment.provenance_elevated,
        tool_call_id: tool_call_id.to_string(),
    };

    tracing::info!(
        risk = %assessment.risk.label(),
        command = %command,
        reason = %assessment.reason,
        "Requesting user approval for shell command (Gateway mode)"
    );

    // Request approval via ApprovalHandle. ADR-045: also listen on cancel_rx
    // so a CancelTool signal interrupts the approval wait - without this,
    // a cancelled tool would still execute if the user later approved the
    // stale dialog.
    let decision = tokio::select! {
        d = handle.request_approval(approval_req) => d,
        _ = cancel_rx.wait_for(|cancelled| *cancelled) => {
            tracing::info!(
                tool_call_id = %tool_call_id,
                command = %command,
                "Tool cancelled while waiting for approval"
            );
            return Some(format!(
                "Error: Cancelled by user while waiting for approval of '{}'",
                command
            ));
        }
    };

    if decision.approved {
        tracing::info!(command = %command, "Shell command approved by user");
        None
    } else {
        let reject_reason = decision.reason.as_deref().unwrap_or("rejected by user");
        tracing::info!(command = %command, reason = %reject_reason, "Shell command rejected");
        Some(format!(
            "Error: Shell command was rejected based on risk assessment.\n\
             Command: {}\nRisk level: {}\nReason: {}",
            command,
            assessment.risk.label(),
            reject_reason
        ))
    }
}

impl AgentLoop {
    // ── Tool pipeline methods (ADR-014 Phase 8: extracted from execute_single_iteration) ──

    /// Prepare tool calls for execution.
    ///
    /// Persists think block to JSONL (via D2 dedup helper), deduplicates
    /// same-iteration tool calls, appends assistant message to history,
    /// persists tool calls to JSONL, and emits ToolCall chunk events.
    ///
    /// Returns the deduplicated tool call list.
    pub(crate) fn prepare_tool_calls(
        &mut self,
        response: &acowork_core::providers::traits::ChatResponse,
    ) -> Vec<ToolCall> {
        // ADR-022: Flush the current streaming line to JSONL.
        //
        // During streaming, the provider layer split think tags into
        // ReasoningContent + Content events, and each role transition
        // already flushed the previous line via flush_and_new_streaming_line.
        // This final flush captures any remaining content in the last
        // streaming line (e.g., the assistant text that preceded the
        // tool_call).
        let flushed = self
            .session_core
            .flush_streaming_line(self.session.conversation.as_deref());
        tracing::info!(
            flushed_role = flushed
                .as_ref()
                .map(|c| {
                    // We can't easily get the role here, but we can log content length
                    c.len()
                })
                .unwrap_or(0),
            flushed_len = flushed.as_ref().map(|c| c.len()).unwrap_or(0),
            "ADR-022 prepare_tool_calls: flush_streaming_line result"
        );

        // ADR-022: Only use legacy persistence if no streaming flush occurred.
        // When streaming already flushed thought content on role transitions,
        // persist_think_to_conversation would create a duplicate entry.
        let streamed = self
            .session_core
            .streaming_flush_count
            .load(Ordering::Relaxed)
            > 0;
        tracing::info!(
            streamed,
            streaming_flush_count = self
                .session_core
                .streaming_flush_count
                .load(Ordering::Relaxed),
            has_reasoning = response.reasoning_content.is_some(),
            reasoning_len = response
                .reasoning_content
                .as_ref()
                .map(|r| r.len())
                .unwrap_or(0),
            "ADR-022 prepare_tool_calls: streamed={}",
            streamed
        );

        if !streamed {
            // Legacy path: persist think block from the final response.
            // Handles providers that use reasoning_content field (DeepSeek)
            // or  thinking... response tags without streaming them.
            if let Some(ref conversation) = self.session.conversation {
                crate::agent::loop_session::persist_think_to_conversation(conversation, response);
            }
        }

        // Has tool calls — process them
        let tool_calls = response.tool_calls.clone().unwrap_or_default();

        // Tool call deduplication (same iteration)
        let mut seen = HashSet::new();
        let deduped_calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|tc| {
                let sig = format!("{}:{}", tc.function.name, tc.function.arguments);
                seen.insert(sig)
            })
            .collect();

        // Persist assistant turn to in-memory history.
        //
        // IMPORTANT: We intentionally store `reasoning_content` here, despite it
        // looking like internal reasoning detail, **because the DeepSeek thinking
        // mode API requires it on the wire**: when an assistant turn contains
        // tool_calls, the `reasoning_content` for that turn MUST be echoed back
        // to the DeepSeek API in all subsequent user turns — otherwise the API
        // returns HTTP 400. Reference: https://api-docs.deepseek.com/zh-cn/guides/thinking_mode
        // section "工具调用 → 必须完整回传 reasoning_content".
        //
        // For the OpenAI-compatible code path, `reasoning_content` is naturally
        // None on non-DeepSeek endpoints (o1/o3 Chat Completions do not surface
        // reasoning in the response), so serializing the field back is a no-op
        // for non-DeepSeek providers.
        //
        // Why this is NOT a bug: looking at it in isolation, storing
        // reasoning_content into history seems risky (it grows context); it is
        // NOT optional for DeepSeek, and removing it will silently break
        // DeepSeek tool-call iterations with 400 errors. See the companion
        // comment in loop_session.rs::handle_text_response for the matching
        // text-only-path behavior.
        self.session.history.append(ChatMessage {
            reasoning_content: response.reasoning_content.clone(),
            tool_calls: Some(deduped_calls.clone()),
            ..ChatMessage::assistant(response.content.clone())
        });

        // Persist tool calls to JSONL
        if let Some(ref conversation) = self.session.conversation {
            for tc in &deduped_calls {
                let metadata = serde_json::json!({
                    "tool_name": tc.function.name,
                    "tool_call_id": tc.id,
                });
                // ADR-035 M2: pre-assign a stable message_id so the JSONL
                // entry and the record_complete event share the same id.
                let message_id = uuid::Uuid::new_v4().to_string();
                conversation.append_message_with_id(
                    "tool_call",
                    &tc.function.arguments,
                    Some(metadata),
                    Some(message_id.clone()),
                );
                // ADR-035 C1: emit record_complete so the frontend inserts
                // the tool_call directly into messages[] (no streaming phase).
                // Tool metadata is forwarded here so the frontend can pair
                // the eventual tool_result without an extra HTTP fetch.
                //
                // `seq` is the per-session monotonic counter from
                // `SessionCore::next_seq()`. Each tool_call gets its own seq
                // so the four parallel reads are ordered correctly even under
                // cross-publisher reorder; the Desktop inserts them at the
                // correct position in `messages[]` via `insertBySeq`.
                let _ = self.session_core.try_send_chunk(
                    crate::agent::loop_::ChunkEvent::RecordComplete {
                        session_id: self.session_core.session_id.clone().unwrap_or_default(),
                        role: "tool_call".to_string(),
                        message_id,
                        content: tc.function.arguments.clone(),
                        tool_name: tc.function.name.clone(),
                        tool_call_id: tc.id.clone(),
                        is_error: false,
                        seq: self.session_core.next_seq(),
                    },
                );
            }
        }

        deduped_calls
    }

    /// Pre-execution loop detection: check tool calls before executing them.
    ///
    /// Tool calls that trigger Block or Break level are filtered out;
    /// Warning-level calls are allowed through (handled post-execution).
    ///
    /// Returns `(calls_to_execute, blocked_info)` where blocked_info
    /// contains the original index and detected pattern for each blocked call.
    pub(crate) fn pre_check_loop_detection(
        &mut self,
        deduped_calls: &[ToolCall],
    ) -> (Vec<ToolCall>, Vec<(usize, LoopPattern)>) {
        let mut calls_to_execute: Vec<ToolCall> = Vec::new();
        let mut blocked_info: Vec<(usize, LoopPattern)> = Vec::new();

        for (idx, tc) in deduped_calls.iter().enumerate() {
            match self
                .session
                .loop_detector
                .peek_check(&tc.function.name, &tc.function.arguments)
            {
                LoopDetectionResult::NoLoop => {
                    calls_to_execute.push(tc.clone());
                }
                LoopDetectionResult::LoopDetected { level, pattern, .. } => {
                    match level {
                        ResponseLevel::Warning => {
                            // Warning is handled post-execution; allow the call
                            calls_to_execute.push(tc.clone());
                        }
                        ResponseLevel::Block | ResponseLevel::Break => {
                            tracing::warn!(
                                tool = %tc.function.name,
                                level = ?level,
                                "Loop detected (pre-execution), blocking tool call"
                            );
                            blocked_info.push((idx, pattern));
                        }
                    }
                }
            }
        }

        (calls_to_execute, blocked_info)
    }

    /// Dispatch tool calls for execution and merge results.
    ///
    /// Intercepts special tools (ask_user_question, todo_write) for sequential
    /// processing, executes remaining tools in parallel, then merges all
    /// results back in original order. Also merges with pre-blocked results
    /// from `pre_check_loop_detection`.
    ///
    /// Returns `(tool_results, interrupt)` where each tool_result is `(content, is_transient)`.
    /// `is_transient` propagates from `ToolResult.transient` (ADR-032 C3a).
    pub(crate) async fn dispatch_and_merge_tools(
        &mut self,
        calls_to_execute: Vec<ToolCall>,
        deduped_calls: &[ToolCall],
        blocked_info: &[(usize, LoopPattern)],
    ) -> (Vec<(String, bool)>, Option<ControlDecision>) {
        // ADR-049: Transition from `LlmStreaming` to `ToolExecuting` at the
        // entry of tool dispatch. The frontend then shows the "running tool"
        // indicator instead of "replying". Special tools (`ask_user_question`,
        // `todo_write`) and parallel tool execution are all covered — they
        // share the same `ToolExecuting` semantic ("LLM produced tool calls,
        // awaiting results").
        self.transition_status(SessionStatus::ToolExecuting);

        // Intercept special tools
        let mut ask_question_results: Vec<(usize, (String, bool))> = Vec::new();
        let mut todo_write_results: Vec<(usize, (String, bool))> = Vec::new();
        let mut parallel_calls: Vec<(usize, ToolCall)> = Vec::new();

        for (idx, tc) in calls_to_execute.into_iter().enumerate() {
            // Design 27 §6.5 R1 / §5.5.4 backstop (plan 2b.3): the gated
            // tools are hidden from the wire tool set, but a hallucinated
            // call (e.g. from a cached prompt) must not hang the turn
            // awaiting a human or spawn a nested Intent — reject it as a
            // tool error instead. Same two gates: role for `intent_send`,
            // answerability for `ask_user_question`.
            let hallucinated_gate = match tc.function.name.as_str() {
                "intent_send" => self.pending_intent.is_some(),
                "ask_user_question" => self.turn_surface.has_human == Some(false),
                _ => false,
            };
            if hallucinated_gate {
                let result = format!(
                    "Error: `{}` is not available in this turn (design 27 §5.4.5 R1 / \
                     §5.5.4). Finish the answer; the system writes the response back.",
                    tc.function.name
                );
                ask_question_results.push((idx, (result, false)));
                continue;
            }
            if tc.function.name == "ask_user_question" {
                let result = self.handle_ask_user_question(&tc).await;
                ask_question_results.push((idx, (result, false)));
            } else if tc.function.name == "todo_write" {
                let result = self.handle_todo_write(&tc);
                todo_write_results.push((idx, (result, false)));
            } else {
                parallel_calls.push((idx, tc));
            }
        }

        // Execute non-question tools in parallel
        let calls_for_parallel: Vec<ToolCall> =
            parallel_calls.iter().map(|(_, tc)| tc.clone()).collect();
        let (parallel_results, interrupt) = self.execute_tools_parallel(&calls_for_parallel).await;

        // Merge results: ask_question + todo_write + parallel, mapped back to original indices
        let ask_result_map: std::collections::HashMap<usize, (String, bool)> =
            ask_question_results.into_iter().collect();
        let todo_result_map: std::collections::HashMap<usize, (String, bool)> =
            todo_write_results.into_iter().collect();

        let mut final_results: Vec<(usize, (String, bool))> = Vec::new();
        for (parallel_idx, result) in parallel_results.into_iter().enumerate() {
            if let Some((orig_idx, _)) = parallel_calls.get(parallel_idx) {
                final_results.push((*orig_idx, result));
            }
        }
        for (orig_idx, result) in &ask_result_map {
            final_results.push((*orig_idx, result.clone()));
        }
        for (orig_idx, result) in &todo_result_map {
            final_results.push((*orig_idx, result.clone()));
        }
        final_results.sort_by_key(|(idx, _)| *idx);

        let executed_results: Vec<(String, bool)> =
            final_results.into_iter().map(|(_, r)| r).collect();

        // Merge executed results with pre-blocked results, preserving original order
        let mut tool_results: Vec<(String, bool)> = Vec::with_capacity(deduped_calls.len());
        let mut executed_iter = executed_results.into_iter();
        for idx in 0..deduped_calls.len() {
            if let Some(pos) = blocked_info.iter().position(|(i, _)| *i == idx) {
                let msg = match &blocked_info[pos].1 {
                    LoopPattern::SameToolFlood => {
                        "Loop detected: this tool has been called too many times in a short period. \
                         Please STOP using this tool and try a different approach \
                         (e.g., use file_read to verify results, or switch to another tool)."
                    }
                    _ => {
                        "Loop detected: this tool call has been blocked because it was repeated too many times with the same parameters. Try a different approach."
                    }
                };
                // Blocked results are never transient
                tool_results.push((msg.to_string(), false));
            } else {
                tool_results.push(executed_iter.next().unwrap_or(("".to_string(), false)));
            }
        }

        (tool_results, interrupt)
    }

    /// Persist tool results to JSONL and emit ToolResult chunk events.
    ///
    /// ADR-035 D9.2: tool_result content is truncated to first 5 lines at
    /// the MQTT publish layer (full content stays in JSONL).
    ///
    /// `is_error` is derived heuristically from the result content. The
    /// runtime's error paths produce results starting with `"Error"` or
    /// `"Loop detected"` (see `execute_single_tool` and the blocked-info
    /// branch in `dispatch_and_merge_tools`). The frontend uses this flag
    /// to color the tool_result bubble red and to compute the success/
    /// failure counter on the tool_call pairing header.
    pub(crate) fn persist_and_emit_tool_results(
        &mut self,
        deduped_calls: &[ToolCall],
        tool_results: &[String],
    ) {
        // Persist tool results to JSONL
        if let Some(ref conversation) = self.session.conversation {
            for (tc, result_content) in deduped_calls.iter().zip(tool_results.iter()) {
                let metadata = serde_json::json!({
                    "tool_name": tc.function.name,
                    "tool_call_id": tc.id,
                });
                // ADR-035 M2: pre-assign a stable message_id so the JSONL
                // entry (full content) and the record_complete event
                // (truncated to 5 lines at publish time, D9.2) share the
                // same id.
                let message_id = uuid::Uuid::new_v4().to_string();
                conversation.append_message_with_id(
                    "tool_result",
                    result_content,
                    Some(metadata),
                    Some(message_id.clone()),
                );
                // ADR-035 C1/D9.2: emit record_complete with the FULL
                // content; the MQTT publish layer truncates tool_result to
                // the first 5 lines for frontend display. JSONL keeps the
                // full content for LLM context.
                let is_error = looks_like_tool_error(result_content);
                // Each tool_result gets a fresh seq that fits after its
                // corresponding tool_call's seq. Both source-of-truth
                // ordering (JSONL append order) and the chunk_relay FIFO
                // guarantee the relative order here; the seq just makes the
                // Desktop's reorder-defensive `insertBySeq` self-healing.
                let _ = self.session_core.try_send_chunk(
                    crate::agent::loop_::ChunkEvent::RecordComplete {
                        session_id: self.session_core.session_id.clone().unwrap_or_default(),
                        role: "tool_result".to_string(),
                        message_id,
                        content: result_content.clone(),
                        tool_name: tc.function.name.clone(),
                        tool_call_id: tc.id.clone(),
                        is_error,
                        seq: self.session_core.next_seq(),
                    },
                );
            }
        }
    }

    /// Post-execution loop detection.
    ///
    /// Checks tool results for loop patterns, appends deferred warning
    /// messages to history after all tool results, and returns
    /// `Err(RuntimeError::LoopDetected)` if a Break-level loop is detected.
    /// Skips already-blocked tool calls to avoid false positives.
    pub(crate) fn post_check_loop_detection(
        &mut self,
        deduped_calls: &[ToolCall],
        tool_results: &[String],
        blocked_info: &[(usize, LoopPattern)],
    ) -> Result<()> {
        let mut deferred_warnings: Vec<String> = Vec::new();
        let mut break_error: Option<String> = None;

        for (idx, (tc, result_content)) in deduped_calls.iter().zip(tool_results.iter()).enumerate()
        {
            // Skip loop detection for pre-blocked tool calls to avoid self-reinforcing
            // false positives: blocked tools return uniform error messages whose identical
            // hashes would incorrectly trigger NoProgress detection.
            if blocked_info.iter().any(|(i, _)| *i == idx) {
                continue;
            }

            match self.session.loop_detector.check(
                &tc.function.name,
                &tc.function.arguments,
                result_content,
            ) {
                LoopDetectionResult::NoLoop => {}
                LoopDetectionResult::LoopDetected {
                    pattern,
                    level,
                    count: _,
                    message,
                } => {
                    tracing::warn!(message = %message, level = ?level, "Loop detected");
                    match level {
                        ResponseLevel::Warning => {
                            let warning_content = match &pattern {
                                LoopPattern::SameToolFlood => {
                                    format!(
                                        "[System Warning] {message} \
                                         This tool has been called excessively. \
                                         Please STOP using this tool and try a different approach \
                                         (e.g., use file_read to verify results, or switch to another tool) \
                                         to complete the task."
                                    )
                                }
                                _ => format!("[System Warning] {message}"),
                            };
                            deferred_warnings.push(warning_content);
                        }
                        ResponseLevel::Block => {
                            // Block was already handled by returning error as tool result
                        }
                        ResponseLevel::Break => {
                            break_error = Some(message);
                            break;
                        }
                    }
                }
            }
        }

        // Append deferred warning messages AFTER all tool results
        for warning_content in deferred_warnings {
            self.session.history.append(ChatMessage {
                role: acowork_core::providers::traits::MessageRole::User,
                content: warning_content,
                name: Some("system".to_string()),
                ..Default::default()
            });
        }

        // Handle Break-level loop detection
        if let Some(msg) = break_error {
            self.transition_status(SessionStatus::Idle);
            return Err(RuntimeError::LoopDetected(msg));
        }

        Ok(())
    }
}

/// Heuristic detection of a tool error from its result content.
///
/// The runtime's error paths produce results that begin with one of the
/// well-known prefixes below. We use this so the MQTT `record_complete`
/// payload can carry `is_error = true` without restructuring
/// `execute_tools_parallel`'s return type. The frontend treats the flag
/// as best-effort: any result that renders the tool_call as failed will
/// be styled accordingly, and a `false` here just means we have no
/// positive signal (tool may still have failed in ways we did not tag).
fn looks_like_tool_error(result_content: &str) -> bool {
    let trimmed = result_content.trim_start();
    trimmed.starts_with("Error")
        || trimmed.starts_with("error:")
        || trimmed.starts_with("Loop detected")
        || trimmed.starts_with("Tool call")
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Shell approval threshold semantics ──────────────────────────────

    /// Blocked commands are a hard safety floor: rejected even under the
    /// AutoApprove threshold. "Auto-approve" skips the *prompt*, it must
    /// never bypass the blocklist (e.g. `rm -rf /`).
    #[tokio::test]
    async fn test_auto_approve_still_blocks_blocked_commands() {
        let gate = crate::security::approval_gate::AutoApproveGate;
        let rules = crate::security::shell_risk::ShellRiskRules::default();
        let result = check_shell_approval(
            &gate,
            &mut tokio::sync::watch::channel(false).1,
            "shell",
            r#"{"command": "rm -rf /"}"#,
            &ShellApprovalThreshold::AutoApprove,
            "tc-blocked",
            &rules,
        )
        .await;
        assert!(
            result.is_some(),
            "Blocked command must be rejected even under AutoApprove"
        );
        let err = result.unwrap();
        assert!(
            err.contains("blocked"),
            "error should mention blocking; got: {err}"
        );
    }

    /// Safe commands pass through untouched under AutoApprove.
    #[tokio::test]
    async fn test_auto_approve_allows_safe_commands() {
        let gate = crate::security::approval_gate::AutoApproveGate;
        let rules = crate::security::shell_risk::ShellRiskRules::default();
        let result = check_shell_approval(
            &gate,
            &mut tokio::sync::watch::channel(false).1,
            "shell",
            r#"{"command": "echo hello"}"#,
            &ShellApprovalThreshold::AutoApprove,
            "tc-safe",
            &rules,
        )
        .await;
        assert!(result.is_none(), "safe command must pass under AutoApprove");
    }

    /// Low threshold prompts for Low-risk commands as well; a rejecting
    /// gate surfaces the rejection as an error.
    #[tokio::test]
    async fn test_low_threshold_prompts_for_low_risk() {
        let gate = crate::security::approval_gate::AutoRejectGate;
        let rules = crate::security::shell_risk::ShellRiskRules::default();
        let result = check_shell_approval(
            &gate,
            &mut tokio::sync::watch::channel(false).1,
            "shell",
            r#"{"command": "echo hello"}"#,
            &ShellApprovalThreshold::Low,
            "tc-low",
            &rules,
        )
        .await;
        assert!(
            result.is_some(),
            "Low threshold must ask before Low-risk commands"
        );
        let err = result.unwrap();
        assert!(
            err.contains("rejected"),
            "expected rejection message; got: {err}"
        );
    }
}
