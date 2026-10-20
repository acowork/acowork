//! Approval subsystem for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 2). After the routing-table
//! refactor (2026-04), this module contains:
//! - `ApprovalDecision` - the decision payload resolved to spawned tasks
//! - `ApprovalHandle` - spawned-task-side handle that registers a request
//!   and parks on a oneshot until the main loop routes a decision
//! - `AgentLoop::route_inbound` - main-loop-side router that resolves the
//!   matching in-flight approval's oneshot when the user's
//!   `ApprovalDecision` arrives on `inbound_rx`
//! - `send_tool_approval_needed` - emits the ChunkEvent to Gateway
//!
//! D4 Deduplication note: `route_inbound` does not handle
//! `InboundMessage::QuestionAnswer`; that path lives in
//! `await_question_answer` and is untouched by this refactor.

use acowork_core::timeout_config::constants;
use tokio::sync::{mpsc, oneshot};

use crate::agent::inbound::InboundMessage;
use crate::agent::loop_::{AgentLoop, ChunkEvent, ControlDecision};
use crate::agent::session_state::SessionStatus;
use crate::security::approval_gate::ApprovalRequest;

/// Default approval timeout: 5 minutes.
pub(crate) const APPROVAL_TIMEOUT: std::time::Duration = constants::APPROVAL;
pub(crate) const APPROVAL_TIMEOUT_SECS: u64 = APPROVAL_TIMEOUT.as_secs();

/// User's decision on a tool approval request.
#[derive(Debug, Clone)]
pub(crate) struct ApprovalDecision {
    pub approved: bool,
    #[allow(dead_code)]
    pub allow_all_session: bool,
    /// Human-readable reason for timeout or rejection (for LLM feedback)
    pub reason: Option<String>,
}

/// Lightweight handle for spawned tool tasks to request user approval.
///
/// The spawned task calls `request_approval()`, which sends the request to
/// the AgentLoop main loop via an mpsc channel and parks on a oneshot. The
/// main loop's `execute_tools_parallel` registers the request in
/// `pending_approvals` (keyed by a fresh UUID), emits
/// `ChunkEvent::ToolApprovalNeeded` to the Gateway, and later resolves the
/// oneshot when `route_inbound` sees the matching
/// `InboundMessage::ApprovalDecision`.
///
/// Concurrency: multiple spawned tasks may call `request_approval()`
/// simultaneously; each gets an independent oneshot, and the main loop
/// tracks them all in `pending_approvals`. There is no recursion and no
/// shared-inbound_rx contention - the deadlock of the old
/// `await_approval_decision` design is structurally impossible here.
#[derive(Clone)]
pub(crate) struct ApprovalHandle {
    pub(super) request_tx: mpsc::Sender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
}

impl ApprovalHandle {
    pub fn new(
        request_tx: mpsc::Sender<(ApprovalRequest, oneshot::Sender<ApprovalDecision>)>,
    ) -> Self {
        Self { request_tx }
    }

    /// Request user approval for a tool execution.
    /// Parks on a oneshot until the main loop routes the user's decision
    /// (or auto-rejects on Stop / timeout / iteration abort).
    pub async fn request_approval(&self, req: ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        if self.request_tx.send((req, tx)).await.is_err() {
            tracing::warn!("ApprovalHandle: request channel closed, auto-rejecting");
            return ApprovalDecision {
                approved: false,
                allow_all_session: false,
                reason: None,
            };
        }
        rx.await.unwrap_or_else(|_| {
            tracing::warn!("ApprovalHandle: oneshot sender dropped, auto-rejecting");
            ApprovalDecision {
                approved: false,
                allow_all_session: false,
                reason: None,
            }
        })
    }
}

impl AgentLoop {
    /// Route an inbound message received while tools are executing.
    ///
    /// Dispatch rules:
    /// - `ApprovalDecision` -> look up `pending_approvals` by `request_id`;
    ///   resolve the matching oneshot, clear the retained MQTT event, and
    ///   transition back to `ToolExecuting` when no approvals remain.
    ///   Unknown `request_id`s (stale replay, cross-session) are dropped
    ///   with a warning - they cannot corrupt in-flight state.
    /// - `Stop` -> reject every pending approval and set
    ///   `pending_interrupt` so `execute_tools_parallel` aborts on its
    ///   next `poll_control()` checkpoint.
    /// - Anything else (QuestionAnswer / UserOperation / chat input) ->
    ///   buffered into `deferred_inbound` for `drain_inbound_queue()`,
    ///   preserving the pre-refactor behavior.
    ///
    /// ADR-034 Phase 2: `ApprovalDecision.session_id` is validated when
    /// the current session has a known id; decisions addressed to another
    /// session are dropped before they can resolve the wrong oneshot.
    pub(crate) async fn route_inbound(&mut self, msg: InboundMessage) {
        match msg {
            InboundMessage::ApprovalDecision {
                ref session_id,
                ref request_id,
                approved,
                allow_all_session,
                ref reason,
                ..
            } => {
                // ADR-034 Phase 2: cross-session decisions are not ours.
                // An empty expected id means the loop has not bound to a
                // session yet (unit tests, early startup) - accept rather
                // than drop. An empty incoming id means a pre-ADR-034
                // sender - also accept.
                if let Some(expected_sid) = self.session_core.session_id.as_ref()
                    && !expected_sid.is_empty()
                    && !session_id.is_empty()
                    && session_id != expected_sid
                {
                    tracing::warn!(
                        session_id = %session_id,
                        expected = %expected_sid,
                        request_id = %request_id,
                        "ApprovalDecision for different session, dropping"
                    );
                    return;
                }

                if let Some(tx) = self.pending_approvals.remove(request_id) {
                    tracing::info!(
                        request_id = %request_id,
                        approved,
                        allow_all_session,
                        ?reason,
                        in_flight = self.pending_approvals.len(),
                        "Routing approval decision to spawned task"
                    );
                    let _ = tx.send(ApprovalDecision {
                        approved,
                        allow_all_session,
                        reason: reason.clone(),
                    });
                    // Clear the retained `tool_approval_needed` message so
                    // a Desktop reconnecting later does not see a stale
                    // approval dialog from this turn.
                    let _ = self
                        .session_core
                        .try_send_chunk(ChunkEvent::ClearRetainedEvent {
                            event_type: "tool_approval_needed".to_string(),
                        });
                    // §5.5.4: the request is answered, so its Chat card (if
                    // one was written) is closed here — whichever surface the
                    // answer came from, both surfaces resolve through this
                    // same request id.
                    self.close_chat_card(
                        "approval",
                        request_id,
                        if approved { "approved" } else { "rejected" },
                    )
                    .await;
                    if self.pending_approvals.is_empty() {
                        // ADR-049: tools (possibly more of them) are still
                        // running - resume ToolExecuting, not LlmAwaitingFirstChunk.
                        self.transition_status(SessionStatus::ToolExecuting);
                    }
                } else {
                    tracing::warn!(
                        request_id = %request_id,
                        "ApprovalDecision for unknown request_id, dropping"
                    );
                }
            }
            InboundMessage::Stop { reason } => {
                tracing::info!(
                    ?reason,
                    count = self.pending_approvals.len(),
                    "Stop signal: rejecting all pending approvals"
                );
                let reason_text = reason.clone();
                let ids: Vec<String> = self.pending_approvals.keys().cloned().collect();
                for (request_id, tx) in self.pending_approvals.drain() {
                    tracing::warn!(
                        request_id = %request_id,
                        ?reason_text,
                        "Stop: auto-rejecting pending approval"
                    );
                    let _ = tx.send(ApprovalDecision {
                        approved: false,
                        allow_all_session: false,
                        reason: Some(reason_text.clone()),
                    });
                }
                // §5.5.4: a stop is a resolution — the cards it cancels must
                // not stay open in the Chat session afterwards.
                for id in ids {
                    self.close_chat_card("approval", &id, "stopped").await;
                }
                // Let a reconnecting Desktop know no stale dialogs remain.
                let _ = self
                    .session_core
                    .try_send_chunk(ChunkEvent::ClearRetainedEvent {
                        event_type: "tool_approval_needed".to_string(),
                    });
                // Propagate the stop signal upward via the existing
                // pending_interrupt / poll_control() checkpoint chain.
                self.pending_interrupt = Some(ControlDecision::Stop);
            }
            other => {
                // Non-approval messages during tool execution - keep the
                // pre-refactor buffering semantics.
                tracing::debug!(
                    ?other,
                    "Buffering non-approval message during tool execution"
                );
                self.session.deferred_inbound.push(other);
            }
        }
    }

    /// Wait for an `InboundMessage::QuestionAnswer` matching `request_id`.
    ///
    /// Non-matching messages are buffered in `session.deferred_inbound`.
    /// A concurrent approval request arriving on `approval_rx` (which can
    /// only happen with defensive/debug timing - `ask_user_question` runs
    /// sequentially before `execute_tools_parallel` spawns shell tasks) is
    /// registered into `pending_approvals` and announced; its decision,
    /// if it arrives during the question wait, is buffered into
    /// `deferred_inbound` and routed by `execute_tools_parallel`'s opening
    /// drain once the question completes.
    ///
    /// The wait timeout is driven by `core.approval_timeout_secs` (the user's
    /// preference set in agent config / Settings). This is intentionally NOT
    /// overridable per call - the question wait is an agent scheduling concern,
    /// not an LLM-controlled tool parameter.
    pub(crate) async fn await_question_answer(&mut self, request_id: &str) -> String {
        let effective_timeout_secs = self
            .core
            .approval_timeout_secs
            .unwrap_or(APPROVAL_TIMEOUT_SECS);
        let timeout_duration = std::time::Duration::from_secs(effective_timeout_secs);

        let ctrl_notify = self.core.debug_observer.control_notify().cloned();
        let timeout_future = tokio::time::timeout(timeout_duration, async {
            loop {
                tokio::select! {
                    msg = self.inbound_rx.recv() => {
                        match msg {
                            Some(InboundMessage::QuestionAnswer {
                                session_id: _,
                                request_id: rid,
                                answer,
                            }) if rid == request_id => {
                                return answer;
                            }
                            Some(InboundMessage::QuestionAnswer {
                                session_id: _,
                                request_id: rid,
                                answer,
                            }) => {
                                // Answer for a different question - buffer it
                                tracing::debug!(
                                    expected = %request_id,
                                    got = %rid,
                                    "Buffering question answer for different request"
                                );
                                self.session.deferred_inbound.push(InboundMessage::QuestionAnswer {
                                    session_id: self.session_core.session_id.clone().unwrap_or_default(),
                                    request_id: rid,
                                    answer,
                                });
                            }
                            Some(InboundMessage::Stop { reason }) => {
                                tracing::info!(
                                    reason = %reason,
                                    request_id = %request_id,
                                    "Question wait stopped, returning cancelled"
                                );
                                self.pending_interrupt = Some(ControlDecision::Stop);
                                return "[Cancelled: user stopped]".to_string();
                            }
                            Some(other) => {
                                tracing::debug!(
                                    ?other,
                                    "Buffering non-question message during question wait"
                                );
                                self.session.deferred_inbound.push(other);
                            }
                            None => {
                                tracing::warn!(
                                    request_id = %request_id,
                                    "Inbound channel closed during question wait, returning cancelled"
                                );
                                return "[Cancelled: channel closed]".to_string();
                            }
                        }
                    }
                    // Defensive: register a concurrent approval request. The
                    // decision (if any) will be buffered to deferred_inbound
                    // by the QuestionAnswer matcher above and routed by
                    // execute_tools_parallel's opening drain.
                    approval_req = self.approval_rx.recv() => {
                        match approval_req {
                            Some((req, decision_tx)) => {
                                let new_request_id = uuid::Uuid::new_v4().to_string();
                                self.pending_approvals.insert(
                                    new_request_id.clone(),
                                    decision_tx,
                                );
                                self.send_tool_approval_needed(&new_request_id, &req);
                                tracing::info!(
                                    request_id = %new_request_id,
                                    "Registered concurrent approval request during question wait"
                                );
                            }
                            None => {
                                tracing::warn!("Approval channel closed during question wait");
                            }
                        }
                    }
                    // DevMode control-signal wakeup (Pause / DebugStop)
                    _ = async {
                        if let Some(ref notify) = ctrl_notify {
                            notify.notified().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        match self.poll_control() {
                            ControlDecision::Pause => {
                                tracing::info!("Question wait paused via debug");
                                return "[Cancelled: debug paused]".to_string();
                            }
                            ControlDecision::Stop => {
                                tracing::info!("Question wait stopped via debug");
                                return "[Cancelled: debug stopped]".to_string();
                            }
                            ControlDecision::Continue => {}
                        }
                    }
                }
            }
        });
        let result = timeout_future.await;

        match result {
            Ok(answer) => answer,
            Err(_elapsed) => {
                tracing::warn!(
                    request_id = %request_id,
                    timeout_secs = %effective_timeout_secs,
                    "Question answer timed out"
                );
                "[Timeout: user did not respond]".to_string()
            }
        }
    }

    /// Send ToolApprovalNeeded chunk event to Gateway (via chunk channel).
    pub(crate) fn send_tool_approval_needed(&self, request_id: &str, req: &ApprovalRequest) {
        let timeout = self
            .core
            .approval_timeout_secs
            .unwrap_or(APPROVAL_TIMEOUT_SECS);
        let _ = self
            .session_core
            .try_send_chunk(ChunkEvent::ToolApprovalNeeded {
                request_id: request_id.to_string(),
                tool_name: req.tool_name.clone(),
                action: req.action.clone(),
                risk_level: req.risk_level.label().to_string(),
                reason: req.reason.clone(),
                tool_call_id: req.tool_call_id.clone(),
                approval_timeout_secs: timeout,
            });
    }
}

#[cfg(test)]
mod tests {
    use crate::agent::agent_core::BuiltinToolEntry;
    use crate::agent::inbound::InboundMessage;
    use crate::config::RuntimeConfig;
    use crate::security::approval_gate::ApprovalRequest;
    use crate::security::shell_risk::ShellRisk;
    use acowork_core::providers::mock::MockProvider;
    use acowork_core::providers::traits::Provider;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    fn test_manifest() -> acowork_core::AgentManifest {
        acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.approval"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "mock-model"
            "#,
        )
        .unwrap()
    }

    fn test_budget() -> acowork_core::Budget {
        acowork_core::Budget {
            daily_tokens: Some(100000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "warn".to_string(),
        }
    }

    fn make_request(tool_call_id: &str, action: &str) -> ApprovalRequest {
        ApprovalRequest {
            tool_name: "bash".to_string(),
            action: action.to_string(),
            risk_level: ShellRisk::High,
            reason: "test reason".to_string(),
            executable_paths: vec![],
            provenance_elevated: false,
            tool_call_id: tool_call_id.to_string(),
        }
    }

    /// Regression test for the concurrent-approval deadlock.
    ///
    /// Old design: 4 spawned tasks each called `request_approval`, the
    /// main loop recursively entered `handle_approval_request` ->
    /// `await_approval_decision`, and 4 nested waits raced on the shared
    /// `inbound_rx`. A decision consumed by the WRONG waiter was buffered
    /// into `deferred_inbound` where nobody drained it - the matching
    /// waiter then hung until the 5-minute timeout.
    ///
    /// New design: the main loop registers each request into
    /// `pending_approvals` (UUID keyed), and `route_inbound` resolves the
    /// matching oneshot directly. Order of decisions is irrelevant.
    ///
    /// This test drives the new path end-to-end at the unit level:
    /// 1. 4 spawned tasks park on `ApprovalHandle::request_approval`
    /// 2. Main test loop drains `approval_rx`, registers each into
    ///    `pending_approvals` exactly like `execute_tools_parallel` does
    /// 3. Decisions are delivered in scrambled order via `route_inbound`
    /// 4. All 4 tasks must resolve within 5s with the correct decision
    #[tokio::test]
    async fn test_four_concurrent_approvals_routed_correctly() {
        let manifest = test_manifest();
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<BuiltinToolEntry> = vec![];
        let budget = test_budget();

        let (mut agent_loop, _inbound_tx) = crate::agent::loop_::AgentLoop::new(
            RuntimeConfig::default(),
            manifest,
            provider,
            tools,
            budget,
            None,
            None,
        );

        // 1. Spawn 4 tasks, each requesting approval for a distinct command.
        let handle = agent_loop.approval_handle.clone();
        let tasks: Vec<_> = (0..4)
            .map(|i| {
                let h = handle.clone();
                let req = make_request(&format!("call_{}", i), &format!("dangerous_command_{}", i));
                tokio::spawn(async move {
                    let decision = h.request_approval(req).await;
                    (i, decision)
                })
            })
            .collect();

        // 2. Drain the 4 requests exactly as `execute_tools_parallel`
        //    does: register each into pending_approvals with a fresh UUID.
        //    Track which UUID belongs to which tool_call index.
        let mut uuid_by_index: HashMap<usize, String> = HashMap::new();
        for _ in 0..4 {
            let (req, decision_tx) = agent_loop
                .approval_rx
                .recv()
                .await
                .expect("approval request should arrive");
            let idx: usize = req
                .tool_call_id
                .trim_start_matches("call_")
                .parse()
                .expect("tool_call_id should be call_N");
            let request_id = uuid::Uuid::new_v4().to_string();
            agent_loop
                .pending_approvals
                .insert(request_id.clone(), decision_tx);
            uuid_by_index.insert(idx, request_id);
        }
        assert_eq!(agent_loop.pending_approvals.len(), 4);

        // 3. Deliver decisions in scrambled order (3, 0, 2, 1), approving
        //    the even indexes and rejecting the odd ones. Under the old
        //    design this ordering is exactly what triggered the deadlock.
        let scrambled_order = [3usize, 0, 2, 1];
        for idx in scrambled_order {
            let rid = uuid_by_index[&idx].clone();
            let msg = InboundMessage::ApprovalDecision {
                request_id: rid,
                approved: idx % 2 == 0,
                allow_all_session: false,
                reason: None,
                session_id: "test-session".to_string(),
            };
            agent_loop.route_inbound(msg).await;
        }
        assert!(
            agent_loop.pending_approvals.is_empty(),
            "all 4 approvals should have been routed"
        );

        // 4. All 4 tasks must resolve within 5s with the correct decision.
        let results = tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::future::join_all(tasks),
        )
        .await
        .expect("deadlock regression: tasks did not resolve within 5s");

        for result in results {
            let (idx, decision) = result.expect("spawned task should not panic");
            let expected_approved = idx % 2 == 0;
            assert_eq!(
                decision.approved, expected_approved,
                "task {idx} must receive its own decision, not another task's"
            );
        }
    }

    /// A decision for an unknown request_id must be dropped with a warning,
    /// not panic, and must not disturb any in-flight approvals.
    #[tokio::test]
    async fn test_unknown_request_id_is_dropped() {
        let manifest = test_manifest();
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<BuiltinToolEntry> = vec![];
        let budget = test_budget();

        let (mut agent_loop, _inbound_tx) = crate::agent::loop_::AgentLoop::new(
            RuntimeConfig::default(),
            manifest,
            provider,
            tools,
            budget,
            None,
            None,
        );

        // One in-flight approval for real.
        let handle = agent_loop.approval_handle.clone();
        let task = tokio::spawn(async move {
            let req = make_request("call_real", "real_command");
            handle.request_approval(req).await
        });
        let (_req, decision_tx) = agent_loop
            .approval_rx
            .recv()
            .await
            .expect("approval request should arrive");
        let real_id = uuid::Uuid::new_v4().to_string();
        agent_loop
            .pending_approvals
            .insert(real_id.clone(), decision_tx);

        // Stale decision for a request that no longer exists.
        agent_loop
            .route_inbound(InboundMessage::ApprovalDecision {
                request_id: "stale-id".to_string(),
                approved: true,
                allow_all_session: false,
                reason: None,
                session_id: "test-session".to_string(),
            })
            .await;
        assert_eq!(
            agent_loop.pending_approvals.len(),
            1,
            "stale decision must not remove the real entry"
        );

        // Real decision resolves the task.
        agent_loop
            .route_inbound(InboundMessage::ApprovalDecision {
                request_id: real_id,
                approved: false,
                allow_all_session: false,
                reason: Some("user denied".to_string()),
                session_id: "test-session".to_string(),
            })
            .await;

        let decision = tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("task should resolve")
            .expect("task should not panic");
        assert!(!decision.approved);
    }

    /// A Stop signal must auto-reject every in-flight approval so spawned
    /// tasks do not hang waiting for a dialog that will never be answered.
    #[tokio::test]
    async fn test_stop_rejects_all_pending_approvals() {
        let manifest = test_manifest();
        let provider: Arc<dyn Provider> = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<BuiltinToolEntry> = vec![];
        let budget = test_budget();

        let (mut agent_loop, _inbound_tx) = crate::agent::loop_::AgentLoop::new(
            RuntimeConfig::default(),
            manifest,
            provider,
            tools,
            budget,
            None,
            None,
        );

        // Two concurrent in-flight approvals.
        let handle = agent_loop.approval_handle.clone();
        let tasks: Vec<_> = (0..2)
            .map(|i| {
                let h = handle.clone();
                let req = make_request(&format!("call_{}", i), &format!("cmd_{}", i));
                tokio::spawn(async move { h.request_approval(req).await })
            })
            .collect();

        for _ in 0..2 {
            let (_req, decision_tx) = agent_loop
                .approval_rx
                .recv()
                .await
                .expect("approval request should arrive");
            let request_id = uuid::Uuid::new_v4().to_string();
            agent_loop.pending_approvals.insert(request_id, decision_tx);
        }
        assert_eq!(agent_loop.pending_approvals.len(), 2);

        // Stop signal -> both tasks must be auto-rejected.
        agent_loop
            .route_inbound(InboundMessage::Stop {
                reason: "user stopped".to_string(),
            })
            .await;
        assert!(agent_loop.pending_approvals.is_empty());

        let results = tokio::time::timeout(
            Duration::from_secs(5),
            futures_util::future::join_all(tasks),
        )
        .await
        .expect("tasks should resolve after Stop");
        for result in results {
            let decision = result.expect("task should not panic");
            assert!(!decision.approved, "Stop must auto-reject");
            assert!(
                decision.reason.as_deref().unwrap_or("").contains("stopped"),
                "rejection reason should carry the stop reason; got {:?}",
                decision.reason
            );
        }
    }
}
