//! Per-session state for Agent Runtime.
//!
//! `SessionState` holds all state that is scoped to a single conversation session:
//! history, conversation persistence, loop detector, and budget guard.
//! Each session gets its own independent instance, ensuring isolation
//! between sessions (e.g. loop detection does not cross session boundaries).
//!
//! Phase 1: direct ownership inside AgentLoop.
//! Phase 2: extracted into Session Actor for multi-session concurrency.

use std::sync::{Arc, RwLock};

use crate::agent::budget_guard::BudgetGuard;
use crate::agent::history::HistoryManager;
use crate::agent::inbound::InboundMessage;
use crate::agent::loop_detector::LoopDetector;
use crate::conversation::ConversationSession;
use acowork_core::providers::traits::ReasoningEffort;
/// Shared map of session runtime snapshots, keyed by session_id.
///
/// ADR-039: persisted per-session config (title/model/provider/etc.) now lives
/// in `data/meta/{session_id}.json` and is broadcast through the `session_meta`
/// MQTT channel. The Runtime HTTP `/sessions/{sid}/state` endpoint therefore
/// only holds *runtime* state — no duplication of meta data.
pub type SharedSessionSnapshots =
    Arc<RwLock<std::collections::HashMap<String, Arc<RwLock<SessionRuntimeSnapshot>>>>>;

/// Shared latest session info, updated by [`SessionManager`] and read by the
/// Runtime HTTP server's `GET /sessions/latest` endpoint.
///
/// Stored as `(session_id, title)`.  The SessionManager writes to this on
/// every session creation and on startup scan completion.  The HTTP handler
/// reads from the same `Arc`, so it always reflects the authoritative latest
/// session without any file-system scanning.
pub type SharedLatestSession = Arc<RwLock<Option<(String, Option<String>)>>>;

/// Lightweight snapshot of per-session runtime state.
///
/// ADR-039: persisted per-session config (model, provider, workspace_id,
/// reasoning_effort, temperature) lives in `data/meta/{session_id}.json`
/// and is broadcast through the `session_meta` MQTT channel; this struct
/// therefore only carries *runtime* fields.
///
/// ADR-039 (revised): `model` and `provider` are still mirrored here as
/// runtime-cached values, because `SessionManager::current_model_name` /
/// `current_model_and_provider` need sync, in-process reads of the live
/// model/provider without file I/O. The meta file remains the authoritative
/// source for the HTTP pull API and MQTT broadcast; the snapshot mirror is
/// best-effort and may briefly lag the meta file by one iteration.
///
/// Written by `AgentLoop::emit_session_state` on every status transition
/// and at iteration checkpoints. Read by `SessionManager::snapshot_session_state`
/// to serve the Gateway HTTP `GET /api/agents/{id}/sessions/{session_id}/state`
/// endpoint without a gRPC round-trip to the Runtime process.
///
/// Uses `Arc<std::sync::RwLock<...>>` so reads are lock-free on the happy path
/// and writes are isolated to the emit call site.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SessionRuntimeSnapshot {
    /// Session identifier.
    pub session_id: String,
    /// JSON-serialized `SessionStatus` (same format as `SessionStateChanged` event).
    pub status: String,
    /// Currently active model, if any.
    /// **Runtime mirror of `SessionState::model`** — see ADR-039 (revised).
    /// The authoritative value lives in `data/meta/{session_id}.json`.
    pub model: Option<String>,
    /// Currently active provider, if any.
    /// **Runtime mirror of `SessionState::provider`** — see ADR-039 (revised).
    /// The authoritative value lives in `data/meta/{session_id}.json`.
    pub provider: Option<String>,
    /// Multi-account `account_id` pick for the current session (ADR-012).
    ///
    /// Runtime mirror of `ConversationSession::account_id`, kept in sync
    /// by `SessionManager::route_model_switch` (the synchronous
    /// model/provider switch path) and by `ConversationSession::update_account_id`
    /// (the inheritance path on new-session creation). Read by
    /// `SessionManager::current_account_id`'s Level-2 fallback so a
    /// brand-new session created in a runtime whose `session_configs`
    /// map is `None` (tests, single-session mode) still inherits the
    /// most recent pick from the in-memory snapshot. `None` when the
    /// session has not picked an account yet.
    pub account_id: Option<String>,
    /// Calibrated chars/token ratio, if available.
    pub ratio: Option<f64>,
    /// Current todo list managed by the `todo_write` built-in tool.
    /// Serialized as JSON so the Gateway and frontend can consume it
    /// without additional protobuf message definitions.
    pub todos_json: Option<String>,
    /// Current context usage info (token counts, window, percentage).
    /// `None` if no LLM call has been made yet in this session.
    /// Serialized as JSON so the Gateway and frontend can consume it
    /// without additional protobuf message definitions.
    pub context_usage: Option<String>,
}

/// One queued inbound intent routed into a session while it was already busy
/// (ADR-099, design 27 §5.5.5).
///
/// Held on `ConversationSession` as the source of truth and projected through
/// the retained `SessionState` snapshot, so the execution UI can show the real
/// pending queue — the chat-forwarded messages that the local composer queue
/// box (frontend-only state) cannot see. `preview` is a bounded excerpt, never
/// a full body (ADR-096). Recorded on enqueue only when the target session is
/// active; cleared when the session dequeues it to start the turn.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PendingIntent {
    /// Durable id of the user message this intent answers — the dedup key.
    pub message_id: String,
    /// Display name of the sender, for the queue row label.
    pub from: String,
    /// Bounded single-line excerpt of the user's message.
    pub preview: String,
    /// `true` when the message was routed from a Chat session (vs an
    /// agent-to-agent or PM intent), so the UI can label it "from chat".
    pub from_chat: bool,
}

/// Max characters kept in a [`PendingIntent::preview`] excerpt (bounded read).
pub const PENDING_INTENT_PREVIEW_CHARS: usize = 120;

/// Collapse raw message text to a single line and cut it to
/// [`PENDING_INTENT_PREVIEW_CHARS`] characters on a `char` boundary, appending
/// an ellipsis when trimmed. Used to build a queue preview without shipping the
/// whole body.
pub fn pending_preview(text: &str) -> String {
    let flat = text.trim().replace(['\r', '\n'], " ");
    if flat.chars().count() <= PENDING_INTENT_PREVIEW_CHARS {
        flat
    } else {
        let cut: String = flat.chars().take(PENDING_INTENT_PREVIEW_CHARS).collect();
        format!("{cut}…")
    }
}

/// A single item in the session-level todo list.
///
/// Re-exported from `acowork_core::session_meta::TodoItem` (canonical
/// definition, since the `SessionMetaStore` trait needs it there). Kept
/// here under the original name so existing call sites
/// (`crate::agent::session_state::TodoItem`) keep compiling.
pub use acowork_core::session_meta::TodoItem;

/// Status of a todo item. Re-exported alongside [`TodoItem`].
pub use acowork_core::session_meta::TodoStatus;

/// Lifecycle status of a session, managed by Runtime as the source of truth.
///
/// ADR-014: The Runtime owns session status; the frontend is read-only.
/// State transitions are emitted as `ChunkEvent::SessionStateChanged` via
/// the chunk channel, so the Gateway and frontend stay in sync without
/// optimistic local writes.
///
/// ADR-049: `Streaming` is split into three semantic sub-states so the
/// frontend can derive the processing phase directly from `SessionStatus`
/// without composing from data parameters (e.g. `stream_delta` line counts).
/// The 6-variant state machine eliminates the "semantic black hole" where
/// TTFT wait, streaming output, and tool execution all looked identical
/// to the UI.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "detail")]
#[derive(Default)]
pub enum SessionStatus {
    /// Session is idle — no LLM call in progress.
    #[default]
    Idle,
    /// LLM HTTP request has been sent; waiting for the first content chunk
    /// (TTFT phase — TCP/TLS/HTTP-headers/SSE-first-chunk, can take 10-30s).
    ///
    /// ADR-049: distinguishes the "waiting for reply" perception from active
    /// streaming. Frontend renders a "waiting" indicator without a row count
    /// threshold (the pre-ADR-049 3-line visual delay is gone).
    LlmAwaitingFirstChunk,
    /// LLM is producing reasoning/thinking content. Entered when the first
    /// `ReasoningContent` stream event arrives. Promotes to `LlmStreaming`
    /// when the first visible `Content` chunk arrives, or to `ToolExecuting`
    /// if tool calls follow directly after reasoning.
    Thinking,
    /// LLM is actively streaming content. The first chunk has arrived.
    /// `message_id` matches the streaming message, if available.
    LlmStreaming { message_id: Option<String> },
    /// Tool calls have been dispatched to the tool registry; waiting for
    /// their results. Covers both parallel tool execution and special
    /// tools (`ask_user_question`, `todo_write`).
    ///
    /// ADR-049: distinct from `LlmStreaming` so the UI can show a "tool
    /// running" indicator instead of generic "replying".
    ToolExecuting,
    /// A tool requires user approval before execution.
    ///
    /// **Concurrent semantics**: when multiple tools await approval
    /// simultaneously (fan-out), `request_id` reflects the most recent
    /// transition only. The frontend must NOT use this field to
    /// locate/dismiss dialogs - it should key off
    /// `ChunkEvent::ToolApprovalNeeded.request_id`, which is unique per
    /// approval request.
    WaitingApproval { request_id: String },
    /// Iteration limit reached, debug pause, or 429 retry wait — awaiting user decision.
    Paused {
        iteration: Option<u32>,
        max_iterations: Option<u32>,
        /// 429 retry wait info. `None` for non-retry pauses (iteration limit / debug).
        /// When present, the frontend shows a countdown timer and skip button.
        #[serde(skip_serializing_if = "Option::is_none")]
        retry_info: Option<RetryPauseInfo>,
        /// Why the session paused. Lets the frontend derive the exact UX
        /// (continue/stop banner vs countdown vs debug controls) directly
        /// from the status instead of mirroring transient events.
        ///
        /// `None` for 429 retry waits — `retry_info` is already sufficient
        /// to disambiguate those from the other pause reasons.
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<PauseReason>,
        /// Human-readable pause message (e.g. iteration limit hint or loop
        /// detection detail). Lets the frontend render the banner text
        /// directly from the status — no separate event channel needed.
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Session entered an unrecoverable error state and the SessionTask has
    /// stopped processing further inputs. The frontend should display the
    /// message and offer the user a way to start a new session.
    ///
    /// **Trigger**: emitted by [`crate::agent::session::session_manager`]
    /// when the underlying `SessionTask` panics, or when any other
    /// unrecoverable runtime error (e.g. a non-resumable loop-detector
    /// tripwire) terminates the loop. Previously such incidents left the
    /// session frozen in `Thinking`/`LlmStreaming` forever, with no signal
    /// to the frontend that the agent had died (the user-perceived 2026-08-24
    /// incident: 1500+ seconds of "replying…" until manual runtime restart).
    ///
    /// **Wire format** (`tag = "status", content = "detail"`, `rename_all = "snake_case"`):
    /// ```json
    /// {"status": "errored", "detail": {"reason": "session_task_panicked",
    ///                                  "message": "...",
    ///                                  "last_iteration": 2,
    ///                                  "recoverable": false}}
    /// ```
    ///
    /// `recoverable: false` today is the contract: there is no continuation. The
    /// frontend should clear any "replying…" indicator and let the user start a
    /// fresh session.
    Errored {
        /// Short machine-readable reason code (e.g. `"session_task_panicked"`,
        /// `"infinite_loop_detected"`).  Stable across releases — safe for
        /// analytics / telemetry correlation.
        reason: String,
        /// Human-readable message safe to display in the UI.
        message: String,
        /// Iteration at which the error occurred, if known.  `None` when the
        /// failure happened *before* the first iteration (e.g. during session
        /// initialisation).
        #[serde(skip_serializing_if = "Option::is_none")]
        last_iteration: Option<u32>,
        /// Whether the session can be safely resumed by submitting another
        /// `ChatMessage`.  Currently always `false` — the frontend should
        /// redirect to a new session.
        recoverable: bool,
    },
}

/// Reason a session entered [`SessionStatus::Paused`].
///
/// ADR-014: the Runtime owns session status; the frontend is read-only and
/// derives pause UX from this field rather than caching `iteration_limit_paused`
/// / `loop_detected_paused` events in separate store slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseReason {
    /// Iteration limit reached (`iteration == max_iterations`) — awaiting a
    /// user decision to continue or stop.
    IterationLimit,
    /// Loop detector triggered (repeated identical tool call) — awaiting a
    /// user decision to continue or stop.
    LoopDetected,
    /// Debugger paused the session (DevMode debug protocol).
    Debug,
}

/// 429 rate-limit retry pause information.
///
/// Emitted inside [`SessionStatus::Paused::retry_info`] when the provider
/// enters a retry wait whose duration exceeds the UX threshold (10 s).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetryPauseInfo {
    /// Wait duration in milliseconds
    pub wait_ms: u64,
    /// Current retry attempt (1-based)
    pub attempt: u32,
    /// Maximum retry attempts
    pub max_attempts: u32,
    /// Name of the provider that was rate-limited
    pub provider: String,
}

impl SessionStatus {
    /// Returns true if the session is actively processing (non-idle).
    ///
    /// ADR-049: covers all 6 variants of the state machine. Previously this
    /// excluded `Paused`, which caused a semantic mismatch with the
    /// frontend's `isSessionActive()` (frontend considered `Paused` as
    /// active, backend considered it inactive — a latent bug).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::LlmAwaitingFirstChunk
                | Self::Thinking
                | Self::LlmStreaming { .. }
                | Self::ToolExecuting
                | Self::WaitingApproval { .. }
                | Self::Paused { .. }
        )
    }

    /// Returns true if the session is in the [`Self::Errored`] terminal state.
    ///
    /// The frontend uses this to break out of "agent is replying…" indicators
    /// that would otherwise remain stuck forever (see the [`Self::Errored`]
    /// doc comment for the 2026-08-24 incident).
    pub fn is_errored(&self) -> bool {
        matches!(self, Self::Errored { .. })
    }
}

/// Per-session state for the agent loop.
///
/// Each field is scoped to a single session and is not shared across sessions.
/// This ensures that loop detection, budget tracking, and history are isolated
/// per session, preventing cross-session interference.
pub struct SessionState {
    /// Conversation history manager (message list + token tracking + trimming)
    pub(crate) history: HistoryManager,
    /// Optional conversation session for JSONL persistence.
    ///
    /// ADR-047: wrapped in `Arc` so it can be shared between `SessionState`
    /// (owned by the SessionTask / AgentLoop) and `SessionHandle` (owned by
    /// SessionManager). This allows config mutations to bypass the serial
    /// inference queue.
    pub(crate) conversation: Option<Arc<ConversationSession>>,
    /// Loop detector (per-session to avoid cross-session false positives)
    pub(crate) loop_detector: LoopDetector,
    /// Budget guard (per-session for independent token accounting)
    pub(crate) budget_guard: BudgetGuard,
    /// Messages deferred from `poll_interrupt()` during active execution.
    /// These are non-Interrupt messages that arrived in the AgentLoop's
    /// inbound channel while it was polling mid-iteration. They are
    /// re-injected at the next `drain_inbound_queue()` call so no
    /// message is silently lost.
    pub(crate) deferred_inbound: Vec<InboundMessage>,
    /// Current lifecycle status of the session (source of truth).
    /// ADR-014: Runtime owns this; frontend reads it via SessionStateChanged events.
    pub(crate) status: SessionStatus,
    /// Session-level todo list managed by the `todo_write` built-in tool.
    ///
    /// ADR-060: mirrored into `ConversationSession` (the single persistence
    /// owner) on every update, so a session restart restores the list from
    /// `meta/{session_id}.json` — no longer memory-only.
    pub(crate) todos: Vec<TodoItem>,
    /// Whether compaction has occurred with zero new messages since.
    ///
    /// Per [ADR-011], compaction summaries sit in the middle of history
    /// (not at the tail), so we can't use message position to detect
    /// whether new messages arrived after compaction. This boolean flag
    /// provides a clean signal:
    /// - Set to `true` when compaction completes.
    /// - Reset to `false` when a new message is appended to history.
    /// - At session close: `true` means skip distillation (no new content),
    ///   `false` means distill the tail (new messages after last compaction).
    pub(crate) is_compacted: bool,
    /// Per-session model selection (ADR-012).
    /// Initialized from JSONL metadata, ProviderListUpdate, or model_switch.
    pub(crate) model: Option<String>,
    /// Per-session provider selection (ADR-012).
    pub(crate) provider: Option<String>,
    /// Per-session account selection (multi-account, ADR-012): which API key
    /// of `provider` this session uses. `None` = provider's first account.
    pub(crate) account_id: Option<String>,
    /// Current model chars/token ratio (calibrated from API feedback).
    /// Updated after each LLM call via `calibrate_from_usage`.
    pub(crate) model_ratio: Option<f64>,
    /// Per-session reasoning effort override (set by frontend toggle).
    /// When None, falls back to model capabilities default_reasoning_effort.
    /// Reset to None on model switch (so new model's default applies).
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
    /// Resolved temperature for this session (always Some after session init,
    /// set by SessionManager::create_or_resume_session).
    /// NOT a user-setting — this is the final value after applying the chain:
    /// agent_config.json (Layer 1) → manifest (Layer 2) → DEFAULT_TEMPERATURE (Layer 3).
    pub(crate) temperature: Option<f32>,
    /// Per-session user identity context (formatted `UserProfile` text).
    ///
    /// Mirrors the value held by [`crate::agent::context::ContextBuilder`]
    /// and is updated whenever the SessionTask receives an
    /// `UpdateIdentityContext` message. Stored on `SessionState` so the
    /// compaction paths ([`crate::agent::loop_context`],
    /// [`crate::agent::loop_session`]) can inject the user's preferred
    /// language into the compact model's system prompt without having to
    /// thread `ContextBuilder` through every call site.
    ///
    /// `None` means "no profile yet" — default English summary is fine.
    pub(crate) identity_context: Option<String>,
    /// Shared snapshot of per-session state for the Gateway pull API.
    ///
    /// Initialized with persistent data (model, provider, tokens, etc.) during
    /// [`build_initial_session_state`]. Updated at runtime by
    /// [`AgentLoop::emit_session_state`] on every status transition.
    /// Read by [`SessionManager::snapshot_session_state`] to serve
    /// `GET /api/agents/{id}/sessions/{sid}/state` without a gRPC round-trip.
    pub(crate) snapshot: Arc<RwLock<SessionRuntimeSnapshot>>,
}

impl SessionState {
    /// Create a new SessionState with the given history parameters and budget.
    pub fn new(
        max_tokens: u64,
        budget: acowork_core::Budget,
        conversation: Option<Arc<ConversationSession>>,
    ) -> Self {
        let status =
            serde_json::to_string(&SessionStatus::Idle).unwrap_or_else(|_| r#""idle""#.to_string());
        Self {
            history: HistoryManager::new(max_tokens),
            conversation,
            loop_detector: LoopDetector::with_defaults(),
            budget_guard: BudgetGuard::new(budget),
            deferred_inbound: Vec::new(),
            status: SessionStatus::Idle,
            todos: Vec::new(),
            is_compacted: false,
            model: None,
            provider: None,
            account_id: None,
            model_ratio: None,
            reasoning_effort: None,
            temperature: None,
            identity_context: None,
            snapshot: Arc::new(RwLock::new(SessionRuntimeSnapshot {
                session_id: String::new(),
                status,
                model: None,
                provider: None,
                account_id: None,
                ratio: None,
                todos_json: None,
                context_usage: None,
            })),
        }
    }

    /// Access the history manager.
    pub fn history(&self) -> &HistoryManager {
        &self.history
    }

    /// Access the history manager (mutable).
    pub fn history_mut(&mut self) -> &mut HistoryManager {
        &mut self.history
    }

    /// Access the conversation session.
    ///
    /// ADR-047: returns `Option<&Arc<ConversationSession>>` so callers
    /// can also clone the Arc if needed. Method calls on the inner
    /// `ConversationSession` work through auto-deref.
    pub fn conversation(&self) -> Option<&Arc<ConversationSession>> {
        self.conversation.as_ref()
    }

    /// Access the loop detector.
    pub fn loop_detector(&self) -> &LoopDetector {
        &self.loop_detector
    }

    /// Access the loop detector (mutable).
    pub fn loop_detector_mut(&mut self) -> &mut LoopDetector {
        &mut self.loop_detector
    }

    /// Access the budget guard.
    pub fn budget_guard(&self) -> &BudgetGuard {
        &self.budget_guard
    }

    /// Access the budget guard (mutable).
    pub fn budget_guard_mut(&mut self) -> &mut BudgetGuard {
        &mut self.budget_guard
    }

    /// Access the session status.
    pub fn status(&self) -> &SessionStatus {
        &self.status
    }

    /// Transition session status and return true if the status actually changed.
    /// Returns false if the new status equals the current one (no-op).
    pub fn set_status(&mut self, new_status: SessionStatus) -> bool {
        if self.status == new_status {
            return false;
        }
        tracing::info!(old = ?self.status, new = ?new_status, "Session status changed");
        self.status = new_status;
        true
    }

    /// Update the todo list from a `todo_write` tool call.
    ///
    /// * `merge`: if true, replace the entire list; if false, merge by id
    ///   (update existing items, append new items, remove items not present).
    pub fn update_todos(&mut self, items: Vec<TodoItem>, merge: bool) {
        if merge {
            // Merge: update existing by id, add new, keep items not in input
            for incoming in &items {
                if let Some(existing) = self.todos.iter_mut().find(|t| t.id == incoming.id) {
                    existing.content = incoming.content.clone();
                    existing.status = incoming.status.clone();
                } else {
                    self.todos.push(incoming.clone());
                }
            }
        } else {
            // Replace: full swap
            self.todos = items;
        }
        // ADR-060 §6.1: mirror to the conversation — the single persistence
        // owner. Content-equal snapshots are skipped inside `set_todos`
        // (no disk write for a byte-stable todo list).
        if let Some(ref conv) = self.conversation {
            conv.set_todos(&self.todos);
        }
    }

    /// Format the current todo list as a markdown text for system prompt injection.
    /// Returns `None` if the list is empty.
    pub fn format_todos(&self) -> Option<String> {
        if self.todos.is_empty() {
            return None;
        }
        let lines: Vec<String> = self
            .todos
            .iter()
            .map(|t| {
                let status_mark = match t.status {
                    TodoStatus::Pending => " ",
                    TodoStatus::InProgress => "-",
                    TodoStatus::Completed => "x",
                };
                format!("- [{}] {} ({})", status_mark, t.content, t.id)
            })
            .collect();
        Some(lines.join("\n"))
    }

    /// Get the per-session model (ADR-012).
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    /// Set the per-session model (ADR-012).
    pub fn set_model(&mut self, model: String) {
        self.model = Some(model);
    }

    /// Get the per-session provider (ADR-012).
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    /// Set the per-session provider (ADR-012).
    pub fn set_provider(&mut self, provider: String) {
        self.provider = Some(provider);
    }

    /// Get the per-session account selection (multi-account, ADR-012).
    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    /// Set the per-session account selection (multi-account, ADR-012).
    pub fn set_account_id(&mut self, account_id: String) {
        self.account_id = Some(account_id);
    }

    /// Get the current model chars/token ratio (from API calibration).
    pub fn model_ratio(&self) -> Option<f64> {
        self.model_ratio
    }

    /// Set the current model chars/token ratio (from API calibration).
    pub fn set_model_ratio(&mut self, ratio: f64) {
        self.model_ratio = Some(ratio);
    }

    /// Get the per-session reasoning effort override.
    /// Returns None if no override has been set (use model default).
    pub fn reasoning_effort(&self) -> Option<&ReasoningEffort> {
        self.reasoning_effort.as_ref()
    }

    /// Set the per-session reasoning effort override.
    /// Set to None to clear the override and fall back to model default.
    pub fn set_reasoning_effort(&mut self, effort: Option<ReasoningEffort>) {
        self.reasoning_effort = effort;
    }

    /// Get the resolved temperature for this session.
    /// Returns None only before the session has been fully initialized.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature
    }

    /// Set the resolved temperature for this session.
    /// Called by SessionManager at session creation/resume with the value
    /// from the full resolution chain (agent_config.json → manifest → default).
    pub fn set_temperature(&mut self, temperature: Option<f32>) {
        self.temperature = temperature;
    }

    /// Get the per-session workspace_id (from JSONL metadata, persisted by
    /// ConversationSession::update_workspace_id).
    /// Returns None if no conversation is attached or no workspace has been set.
    pub fn workspace_id(&self) -> Option<String> {
        self.conversation.as_ref().and_then(|c| c.workspace_id())
    }

    /// User identity context (formatted `UserProfile` text), if a profile
    /// has been pushed from the Gateway. Used by the compaction paths to
    /// write summaries in the user's preferred language.
    pub fn identity_context(&self) -> Option<&str> {
        self.identity_context.as_deref()
    }

    /// Set the user identity context. Called by `SessionTask` on session
    /// creation (mirroring the value passed to `ContextBuilder`) and on
    /// every `UpdateIdentityContext` broadcast from the SessionManager.
    pub fn set_identity_context(&mut self, ctx: Option<String>) {
        self.identity_context = ctx;
    }

    /// Access the shared snapshot Arc for external reads (SessionHandle).
    pub fn snapshot_arc(&self) -> &Arc<RwLock<SessionRuntimeSnapshot>> {
        &self.snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_preview_collapses_newlines_and_keeps_short_text() {
        assert_eq!(pending_preview("  hi\nthere\r "), "hi there");
    }

    #[test]
    fn pending_preview_cuts_on_char_boundary_with_ellipsis() {
        let long = "字".repeat(PENDING_INTENT_PREVIEW_CHARS + 10);
        let out = pending_preview(&long);
        // exactly the cap characters + the ellipsis marker, no split char
        assert_eq!(out.chars().count(), PENDING_INTENT_PREVIEW_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn paused_serializes_reason_and_message() {
        let paused = SessionStatus::Paused {
            iteration: Some(3),
            max_iterations: Some(5),
            retry_info: None,
            reason: Some(PauseReason::IterationLimit),
            message: Some("Iteration limit reached (3/5). Click Continue to proceed.".into()),
        };
        let json = serde_json::to_string(&paused).unwrap();
        assert!(json.contains("\"status\":\"paused\""));
        assert!(json.contains("\"reason\":\"iteration_limit\""));
        assert!(json.contains("Iteration limit reached"));
    }

    #[test]
    fn paused_retry_omits_reason_and_message() {
        // 429 retry pause: reason/message are None → omitted from JSON.
        let paused = SessionStatus::Paused {
            iteration: None,
            max_iterations: None,
            retry_info: Some(RetryPauseInfo {
                wait_ms: 300_000,
                attempt: 1,
                max_attempts: 3,
                provider: "mock-provider".into(),
            }),
            reason: None,
            message: None,
        };
        let json = serde_json::to_string(&paused).unwrap();
        assert!(json.contains("retry_info"));
        assert!(!json.contains("reason"));
        assert!(!json.contains("message"));
    }

    #[test]
    fn pause_reason_roundtrips_snake_case() {
        for (reason, expected) in [
            (PauseReason::IterationLimit, "iteration_limit"),
            (PauseReason::LoopDetected, "loop_detected"),
            (PauseReason::Debug, "debug"),
        ] {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            let back: PauseReason = serde_json::from_str(&json).unwrap();
            assert_eq!(back, reason);
        }
    }

    // ── Errored status ────────────────────────────────────────────

    #[test]
    fn errored_serializes_with_snake_case_tag() {
        let status = SessionStatus::Errored {
            reason: "session_task_panicked".into(),
            message: "end byte index 200 is not a char boundary; it is inside '原'".into(),
            last_iteration: Some(2),
            recoverable: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"status\":\"errored\""));
        assert!(json.contains("\"reason\":\"session_task_panicked\""));
        assert!(json.contains("\"recoverable\":false"));
        assert!(json.contains("\"last_iteration\":2"));
    }

    #[test]
    fn errored_omits_last_iteration_when_none() {
        let status = SessionStatus::Errored {
            reason: "session_task_panicked".into(),
            message: "init failed".into(),
            last_iteration: None,
            recoverable: false,
        };
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("last_iteration"));
    }

    #[test]
    fn errored_is_not_active_but_is_errored() {
        let status = SessionStatus::Errored {
            reason: "x".into(),
            message: "y".into(),
            last_iteration: None,
            recoverable: false,
        };
        assert!(status.is_errored());
        assert!(
            !status.is_active(),
            "Errored must NOT be active — frontend must exit 'replying' indicator"
        );
    }

    #[test]
    fn errored_roundtrips_through_json() {
        // 序列化 → 反序列化 → 再序列化，验证 wire format 双向稳定
        let original = SessionStatus::Errored {
            reason: "session_task_panicked".into(),
            message: "byte index 200 is not a char boundary".into(),
            last_iteration: Some(2),
            recoverable: false,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: SessionStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, original);

        // 全 variant roundtrip（状态机完整可逆性）
        for status in [
            SessionStatus::Idle,
            SessionStatus::LlmAwaitingFirstChunk,
            SessionStatus::Thinking,
            SessionStatus::LlmStreaming {
                message_id: Some("m1".into()),
            },
            SessionStatus::ToolExecuting,
            SessionStatus::WaitingApproval {
                request_id: "r1".into(),
            },
            SessionStatus::Paused {
                iteration: Some(1),
                max_iterations: Some(3),
                retry_info: None,
                reason: Some(PauseReason::LoopDetected),
                message: Some("loop".into()),
            },
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let back: SessionStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(back, status, "roundtrip failed for {json}");
        }
    }

    #[test]
    fn idle_is_neither_active_nor_errored() {
        assert!(!SessionStatus::Idle.is_active());
        assert!(!SessionStatus::Idle.is_errored());
    }

    #[test]
    fn default_is_idle() {
        assert_eq!(SessionStatus::default(), SessionStatus::Idle);
    }
}
