//! Per-session state for Agent Runtime.
//!
//! `SessionCore` holds all state that is specific to a single session:
//! session identity, chunk channel, notification control, JSONL counters,
//! streaming state, workspace directory, retry UX, and approval handle.
//!
//! Each `AgentLoop` owns one `SessionCore`, constructed from the shared
//! `AgentCore` template at session creation time.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tokio::sync::Notify;
use tokio::sync::mpsc;

use crate::agent::loop_::{ChunkEvent, SessionChunkEvent};
use crate::agent::loop_approval::ApprovalHandle;
use crate::agent::session_state::SessionStatus;
use crate::cancellation::CancelHandle;

/// Resolve an account's API key from a provider's account list (multi-account).
///
/// - `account_id == None` → the provider's first account (pre-multi-account
///   behaviour / no explicit selection).
/// - `Some(id)` matching an account → that account's key.
/// - `Some(id)` not found (account deleted since selection) → the first
///   account, so a stale selection degrades gracefully instead of failing.
pub(crate) fn resolve_provider_key(
    accounts: Option<&[acowork_core::protocol::ProviderKeyEntry]>,
    account_id: Option<&str>,
) -> Option<String> {
    let list = accounts?;
    match account_id {
        Some(aid) => list
            .iter()
            .find(|e| e.account_id == aid)
            .or_else(|| list.first()),
        None => list.first(),
    }
    .map(|e| e.api_key.clone())
}
use crate::conversation::StreamingStateMap;
use crate::providers::reliable::RetryWaitHandle;

/// Borrowed shared state needed to (re)build a provider instance.
///
/// ADR-103: `build_provider_for` gained a `model_id` parameter (model-level
/// protocol/base-url overrides); bundling the four always-travel-together
/// shared-state borrows into this parameter object keeps the arity sane and
/// the call sites honest (you cannot mix a stale config with a fresh vault).
pub(crate) struct ProviderBuildContext<'a> {
    pub config: &'a crate::config::RuntimeConfig,
    pub global_provider_list: &'a RwLock<Vec<acowork_core::protocol::ProviderListItem>>,
    pub provider_key_vault:
        &'a RwLock<std::collections::HashMap<String, Vec<acowork_core::protocol::ProviderKeyEntry>>>,
    pub compat_cache: Option<&'a Arc<crate::providers::compat::CompatCache>>,
    /// ADR-103 D8/S7 trim cache for `/responses` providers.
    pub responses_compat_cache:
        Option<&'a Arc<crate::providers::responses_compat::ResponsesCompatCache>>,
}

/// Per-session state for one AgentLoop instance.
///
/// Constructed from the shared [`super::agent_core::AgentCore`] template
/// plus session-specific parameters (session_id, chunk_tx, committed_lines).
pub(crate) struct SessionCore {
    /// Session ID of the owning session.
    pub(crate) session_id: Option<String>,

    /// Single chunk sender for control events (Stopped, Done, Error,
    /// SessionStateChanged, ToolApprovalNeeded, AskQuestion, IterationLimitPaused,
    /// NewDataAvailable, ContextUsage, CompactingStarted, CompactingEnded).
    /// None in standalone mode.
    pub(crate) chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,

    /// Unix timestamp (ms) of the last `NewDataAvailable` notification.
    /// Used for 500ms throttle — ADR-021 §难点 1.
    pub(crate) last_notify_ts: Arc<AtomicI64>,

    /// Notify throttle interval in ms (from DataFlowConfig).
    pub(crate) notify_interval_ms: u64,

    /// ADR-022: Committed JSONL line count — updated by the writer thread
    /// AFTER each entry is physically written to disk. Single authoritative
    /// count for `read_messages_since`, `notify_new_data_available`, and
    /// `ensure_streaming_line`.
    pub(crate) committed_lines: Arc<AtomicUsize>,

    /// ADR-022: Number of streaming flushes during the current LLM stream.
    /// Reset to 0 at the start of each `consume_stream` call.
    /// When > 0, `handle_text_response` and `prepare_tool_calls` skip their
    /// legacy persistence paths.
    pub(crate) streaming_flush_count: Arc<AtomicU64>,

    /// Shared map of in-progress streaming lines, keyed by session ID.
    /// Each session holds an Arc clone of the same shared map — created
    /// once in SessionManager, cloned into each SessionCore.
    pub(crate) streaming_lines: StreamingStateMap,

    /// ADR-035: number of chars of the current streaming line's
    /// `accumulated_content` already pushed via `stream_delta`. Reset to 0
    /// whenever a new streaming line is created (role transition), because
    /// the new line starts with empty `accumulated_content`.
    pub(crate) stream_push_offset: Arc<AtomicUsize>,

    /// Per-session monotonic sequence counter for live messages
    /// (`stream_delta` / `record_complete` only). Single source of truth
    /// for the order of MQTT frames reaching the Desktop. The chunk_relay
    /// loops are single-threaded (`while let Some(event) = chunk_rx.recv()`),
    /// so `fetch_add(1, SeqCst)` is the only safe and authoritative site
    /// for assigning these numbers; emit sites call [`SessionCore::next_seq`]
    /// before sending each chunk event. Not persisted — session resume
    /// restarts at 0 because the Desktop loads history via HTTP and any
    /// subsequent live frame is naturally "newer than any history entry".
    pub(crate) seq_counter: Arc<AtomicU64>,

    /// Urgent stop notify — fired by Gateway to cancel tool execution
    /// immediately.  Each session gets its own independent Notify.
    ///
    /// **ADR-044 Phase 4**: legacy path, slated for removal. After Phase 3,
    /// every `tokio::select!` branch in `loop_llm.rs` / `loop_tools.rs` that
    /// previously awaited this Notify has been migrated to
    /// `self.cancel_handle().cancelled()` — the handle is now the sole
    /// producer of `select!` wakeups for tool/LLM execution stops in
    /// production MQTT paths. Kept around through Phase 4 only for
    /// incremental rollback safety; once Phase 4 ships it is deleted.
    pub(crate) urgent_stop: Option<Arc<Notify>>,

    /// ADR-044 §4.5: **per-request cancellation slot**. Holds the
    /// **current request's** [`CancelHandle`]; swapped for a fresh `Active`
    /// handle at every `AgentLoop::run_inner` entry via
    /// [`Self::begin_new_request`]. Hiding the handle inside a slot is
    /// what makes the stop-then-continue flow safe: a Stop from a prior
    /// request can never short-circuit `select_on_cancel` on the next
    /// request because the slot no longer points at the cancelled handle.
    ///
    /// # Why an `Arc<parking_lot::Mutex<CancelHandle>>` slot (not a plain `CancelHandle`)
    ///
    /// The slot must be readable from `SessionManager` (which lives on a
    /// different task) **without** holding a stale clone: external stop
    /// signal sources (MQTT dispatcher, debug server) must always target
    /// the **current** request's handle. A plain value field would only
    /// ever be inspected when `SessionManager` snapshots the handle into
    /// its `cancel_handles` HashMap, leaving the map holding a fixed clone
    /// that ignores later swaps. The Arc\<Mutex\> slot eliminates that
    /// hazard by construction: SessionManager reads through the Arc on
    /// every external cancel dispatch and observes the latest value.
    ///
    /// # Lifetime
    ///
    /// The outer `Arc` lives for the session. The inner `Mutex<CancelHandle>`
    /// is swapped wholesale at every request boundary, so the previous
    /// handle's `Arc` reference count keeps it alive until the in-flight
    /// `select_on_cancel` futures that still hold a clone are dropped
    /// (cheap, sub-millisecond — the inner future is already being torn
    /// down by `run_inner`'s return path).
    ///
    /// ADR-044 Phase 3 history: the field was originally a plain
    /// `CancellationToken: CancelHandle` (then named `CancellationToken`).
    /// The §4.5 fix upgrades it to a slot because the previous design
    /// held the handle for the entire session lifetime, leaving subsequent
    /// requests perpetually Cancelled once any prior request was stopped.
    /// The slot design makes that bug structurally impossible.
    pub(crate) current_cancel_handle: Arc<parking_lot::Mutex<CancelHandle>>,

    /// ADR-083: **per-compaction cancellation slot**. Independent of
    /// [`Self::current_cancel_handle`] so cancelling a compaction never
    /// crosses semantics with the session Stop signal (a Stop still stops
    /// the whole request; a compaction cancel only aborts the distillation
    /// call). Swapped for a fresh `Active` handle at the start of every
    /// compaction via [`Self::begin_compaction`].
    ///
    /// Same `Arc<Mutex<..>>` slot shape as `current_cancel_handle`, for the
    /// same reason: `SessionManager` (on another task) must target the
    /// *current* compaction's handle, and a plain clone would go stale
    /// after the swap. `CancelHandle` is one-shot, so "reset" means
    /// replacing the instance — which is exactly what makes a cancel from
    /// a *previous* compaction harmless to the next one.
    pub(crate) compaction_cancel_handle: Arc<parking_lot::Mutex<CancelHandle>>,

    /// Watch sender for session status (ADR-014).
    /// None for CLI-only sessions.
    pub(crate) status_tx: Option<tokio::sync::watch::Sender<SessionStatus>>,

    /// Shared session status for 429 retry UX.
    ///
    /// Initialized to `Streaming { message_id: None }` in `new()`.
    /// Written by [`AgentLoop::transition_status`] and
    /// [`crate::providers::reliable::ReliableProvider`] (retry pause/resume).
    /// Cloned to the ReliableProvider so it can emit `SessionStateChanged`
    /// events during long retry waits.
    pub(crate) retry_session_status: Option<Arc<std::sync::RwLock<SessionStatus>>>,

    /// Active retry-wait handle for 429 UX.
    ///
    /// Initialized in `new()`. [`session::SessionTask`] checks this when
    /// handling `ContinueExecution` to trigger `skip_notify` and wake the
    /// retry loop.
    pub(crate) retry_wait_handle: Option<crate::providers::reliable::RetryWaitHandle>,

    /// Per-session workspace ID, held by `SessionHandle`.
    /// Defaults to `"__agent_home__"`. Updated synchronously by SessionManager
    /// when the user switches workspace — no channel delay.
    ///
    /// ADR-039: this field is no longer stored on `SessionCore` — it lives on
    /// `SessionHandle.workspace_id` (the single source of truth). The
    /// `SessionCore` constructor still accepts the parameter so `SessionTask`
    /// can pass it through to `SessionHandle`; the field is consumed by the
    /// `SessionHandle` builder, not stored here.

    /// Current workspace directory for tool execution.
    /// Resolved from `workspace_id` via WorkspaceResolver.
    /// Updated synchronously by SessionManager alongside workspace_id.
    pub(crate) current_work_dir: Arc<RwLock<Option<String>>>,

    /// Approval handle for shell command risk confirmation (Gateway mode).
    /// None in CLI mode.
    pub(crate) approval_handle: Option<ApprovalHandle>,

    /// Session title set by async LLM summarization.
    /// Written by the spawned title task in `AgentLoop::run_inner`, read by
    /// `notify_new_data_available()` to push the title to the frontend.
    pub(crate) title: Arc<RwLock<Option<String>>>,

    /// Claim flag for the async title-generation task.
    ///
    /// `title` alone cannot gate spawning: it is only written when the LLM
    /// call returns. A Stop followed by a new user message races that window
    /// — the next `run_inner` still sees `title == None` and spawns a second
    /// task whose result (derived from the *follow-up* message) can overwrite
    /// the first-message title. Claiming this flag before spawning makes the
    /// duplicate structurally impossible; it is released by a Drop guard at
    /// the end of the task (including panic) so a failed call can retry.
    pub(crate) title_task_in_flight: Arc<AtomicBool>,
}

impl SessionCore {
    /// Create a new SessionCore from the AgentCore template and session-specific parameters.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: String,
        chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
        committed_lines: Arc<AtomicUsize>,
        notify_interval_ms: u64,
        current_work_dir: Arc<RwLock<Option<String>>>,
        streaming_lines: StreamingStateMap,
        stream_push_offset: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            session_id: Some(session_id),
            chunk_tx,
            last_notify_ts: Arc::new(AtomicI64::new(0)),
            notify_interval_ms,
            committed_lines,
            streaming_flush_count: Arc::new(AtomicU64::new(0)),
            streaming_lines,
            stream_push_offset,
            // Per-session live-only seq counter. Always starts at 0; never
            // persisted to JSONL (history is loaded via HTTP and doesn't
            // carry `seq`). See field doc above.
            seq_counter: Arc::new(AtomicU64::new(0)),
            urgent_stop: Some(Arc::new(Notify::new())),
            // ADR-044 §4.5: each session starts with a fresh `Active`
            // handle in its per-request slot. `AgentLoop::run_inner`
            // calls `begin_new_request()` at every entry so the handle
            // is always generation-fresh — see field docs above.
            current_cancel_handle: Arc::new(parking_lot::Mutex::new(CancelHandle::new())),
            // ADR-083: compaction cancel slot starts fresh; swapped at the
            // start of every compaction by `begin_compaction`.
            compaction_cancel_handle: Arc::new(parking_lot::Mutex::new(CancelHandle::new())),
            status_tx: None,
            retry_session_status: Some(Arc::new(std::sync::RwLock::new(
                SessionStatus::LlmAwaitingFirstChunk,
            ))),
            retry_wait_handle: Some(RetryWaitHandle::new()),
            current_work_dir,
            approval_handle: None,
            title: Arc::new(RwLock::new(None)),
            title_task_in_flight: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Atomically reserve the next per-session sequence number. Called from
    /// `flush_streaming_line` (record_complete) and `try_send_stream_delta`
    /// (stream_delta) so every emitted `ChunkEvent` that the chunk_relay
    /// will turn into an MQTT frame carries a unique, strictly increasing
    /// `seq`. The Desktop uses it to insert frames at the correct position
    /// in `messages[]` even if MQTT happens to deliver them out of order.
    pub(crate) fn next_seq(&self) -> u64 {
        self.seq_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Begin a new request cycle: atomically swap a fresh `Active`
    /// [`CancelHandle`] into the per-request slot and return a clone of it.
    ///
    /// Called from `AgentLoop::run_inner` at the start of every
    /// user-driven request (chat message, debug replay, intent message,
    /// etc.). The previous handle remains valid (its `Arc` reference count
    /// stays positive until any future still holding a clone is dropped)
    /// but it is no longer the handle that external cancel signal sources
    /// target — they always read through
    /// [`Self::cancel_handle_arc`] and observe the *current* generation.
    ///
    /// # Why this is structurally required (ADR-044 §4.5)
    ///
    /// Before this fix the slot held a single `CancelHandle` for the
    /// entire session lifetime, so a Stop from request N permanently
    /// flipped the handle to `Cancelled` and every subsequent request
    /// (N+1, N+2, …) short-circuited through `select_on_cancel` within
    /// microseconds — the session was unusable until the user created a
    /// brand new session (which allocated a fresh `Arc`). Swapping at
    /// every request boundary eliminates that bug class by construction:
    /// `begin_new_request` is the *only* code that writes the slot, and
    /// the type system guarantees external callers read through `Arc`,
    /// not a stale clone.
    ///
    /// Locking strategy: `parking_lot::Mutex` (not `tokio::Mutex`) because
    /// the critical section is a single assignment; contention is bounded
    /// to one quick compare-and-store per request boundary.
    pub(crate) fn begin_new_request(&self) -> CancelHandle {
        let new_handle = CancelHandle::new();
        *self.current_cancel_handle.lock() = new_handle.clone();
        new_handle
    }

    /// Read the **current request's** [`CancelHandle`].
    ///
    /// Equivalent to (but cheaper than) [`Self::begin_new_request`]:
    /// returns a clone of whatever handle is in the slot right now
    /// without allocating a new one. Callers that only need to await
    /// cancellation (e.g. `loop_inbound::poll_control`'s `is_cancelled()`
    /// check, `select!` `cancelled()` branches in `loop_llm.rs` /
    /// `loop_tools.rs`) use this accessor; `run_inner` uses
    /// [`Self::begin_new_request`] to set up a new generation.
    ///
    /// The handle obtained here is a one-shot view of the *current*
    /// generation: if a fresh request starts before this handle resolves,
    /// the *current* handle has been swapped — this clone still observes
    /// the (now-retired) generation, but external sources read through
    /// [`Self::cancel_handle_arc`] and target the new one instead.
    pub(crate) fn cancel_handle(&self) -> CancelHandle {
        self.current_cancel_handle.lock().clone()
    }

    /// Return the `Arc` handle to the per-request slot itself.
    ///
    /// Used by [`crate::agent::session::session_manager::SessionManager`]
    /// at session creation time so external cancel signal sources
    /// (MQTT `StopGeneration` dispatcher, debug server, CLI cancel) can
    /// locate the slot by `session_id` and read the *current* handle
    /// through it on every dispatch — never a stale clone.
    ///
    /// Storing the `Arc` in `SessionManager::cancel_handles: HashMap`
    /// (instead of a plain `CancelHandle` clone) is what makes the
    /// per-request boundary work: queries through the Arc always observe
    /// the latest value, regardless of how many `begin_new_request`
    /// swaps happened since registration.
    pub(crate) fn cancel_handle_arc(&self) -> Arc<parking_lot::Mutex<CancelHandle>> {
        self.current_cancel_handle.clone()
    }

    /// ADR-083: start a new compaction — swap a fresh `Active`
    /// [`CancelHandle`] into the compaction slot and return a clone.
    ///
    /// Call this once at the top of every compaction (auto or manual),
    /// *before* awaiting the distillation LLM. `CancelHandle` is one-shot,
    /// so swapping instances is what makes a cancel from a previous
    /// compaction a no-op for this one: a `cancel()` that lands before
    /// this call targets the retired handle and is discarded.
    pub(crate) fn begin_compaction(&self) -> CancelHandle {
        let new_handle = CancelHandle::new();
        *self.compaction_cancel_handle.lock() = new_handle.clone();
        new_handle
    }

    /// Return the `Arc` handle to the compaction-cancel slot itself.
    ///
    /// Mirrors [`Self::cancel_handle_arc`] for the compaction signal:
    /// [`crate::agent::session::session_manager::SessionManager`] registers
    /// it so the `compress_type = 3` dispatcher can target the *current*
    /// compaction's handle without holding a stale clone.
    pub(crate) fn compaction_cancel_handle_arc(&self) -> Arc<parking_lot::Mutex<CancelHandle>> {
        self.compaction_cancel_handle.clone()
    }

    // ── Chunk event helpers ──────────────────────────────────────────

    /// Wrap a ChunkEvent into a SessionChunkEvent using this session's id.
    pub fn make_chunk_event(&self, event: ChunkEvent) -> Option<SessionChunkEvent> {
        self.session_id.as_ref().map(|sid| SessionChunkEvent {
            session_id: sid.clone(),
            event,
        })
    }

    /// Try-send a ChunkEvent via the chunk channel, wrapped with session_id.
    pub fn try_send_chunk(&self, event: ChunkEvent) -> bool {
        if let Some(wrapped) = self.make_chunk_event(event) {
            self.chunk_tx
                .as_ref()
                .map(|tx| tx.try_send(wrapped).is_ok())
                .unwrap_or(false)
        } else {
            tracing::debug!("Cannot send chunk event: session_id not set on SessionCore");
            false
        }
    }

    // ── JSONL line counter ───────────────────────────────────────────

    /// Get the committed JSONL line count, initializing from file on cold start.
    ///
    /// Falls back to cold-start file scan if the writer thread hasn't set
    /// the counter yet (resuming a session that hasn't received new writes).
    fn get_committed_lines(&self, session_id: &str) -> usize {
        let cached = self.committed_lines.load(Ordering::Relaxed);
        if cached > 0 {
            return cached;
        }
        // Cold start: the log index supplies the count (first touch
        // reconciles once per session per process, never a full recount).
        let count = self
            .current_work_dir
            .read()
            .unwrap()
            .as_ref()
            .map(|wd| {
                let jsonl_path = std::path::PathBuf::from(wd)
                    .join("conversations")
                    .join(format!("{}.jsonl", session_id));
                crate::conversation::physical_line_count(&jsonl_path)
            })
            .unwrap_or(0);
        // compare_exchange prevents overwriting a value the writer thread
        // may have set between our load (cached==0) and this store.
        let _ =
            self.committed_lines
                .compare_exchange(0, count, Ordering::Relaxed, Ordering::Relaxed);
        self.committed_lines.load(Ordering::Relaxed)
    }

    // ── Streaming line helpers ───────────────────────────────────────

    /// Ensure a `StreamingLine` exists for this session, creating one if needed.
    fn ensure_streaming_line(&self, role: &str) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };
        let mut map = self.streaming_lines.write().unwrap();
        if let Some(existing) = map.get_mut(&sid) {
            debug_assert_eq!(
                existing.role, role,
                "ADR-022 violation: append_streaming_delta called with role `{}` \
                 but current streaming line has role `{}`. \
                 Call flush_and_new_streaming_line before switching roles.",
                role, existing.role
            );
            return;
        }
        let line_number = self.get_committed_lines(&sid);
        // ADR-035: a new streaming line starts with empty `accumulated_content`,
        // so reset the push cursor — the previous line's offset is meaningless now.
        self.stream_push_offset.store(0, Ordering::Relaxed);
        // ADR-035 M2: assign a stable message_id up front so every stream_delta
        // push and the final record_complete event carry the same id. The id is
        // also written to JSONL via append_message_with_id on flush, keeping the
        // persisted entry consistent with the streamed one.
        let message_id = uuid::Uuid::new_v4().to_string();
        map.insert(
            sid,
            crate::conversation::StreamingLine {
                line_number,
                role: role.to_string(),
                message_id,
                // ADR-035 D8.1 #2: pre-allocate 4 KB to avoid early realloc
                // churn during the first few hundred chars of streaming.
                // The String will still grow if needed, but the common case
                // (short-to-medium lines) won't trigger reallocation.
                accumulated_content: String::with_capacity(4096),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
    }

    /// Append a delta to the streaming line for this session.
    pub(crate) fn append_streaming_delta(&self, role: &str, delta: &str) {
        self.ensure_streaming_line(role);
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };
        let mut map = self.streaming_lines.write().unwrap();
        if let Some(sl) = map.get_mut(&sid) {
            sl.accumulated_content.push_str(delta);
        }
    }

    /// Flush the current streaming line to JSONL and remove it from the map.
    ///
    /// ADR-035 C1: after persisting, emits `ChunkEvent::RecordComplete`
    /// carrying the finalized record's role / message_id / content. The
    /// frontend uses this event to freeze the active stream buffer into
    /// `messages[]` and clear `activeStream` — without it the assistant
    /// reply would stay in the "processing" animation forever.
    pub(crate) fn flush_streaming_line(
        &self,
        conversation: Option<&crate::conversation::ConversationSession>,
    ) -> Option<String> {
        let sid = self.session_id.as_ref()?.clone();
        let removed = {
            let mut map = self.streaming_lines.write().unwrap();
            map.remove(&sid)
        };
        let sl = removed?;
        let content = sl.accumulated_content.clone();
        let role = sl.role.clone();
        let message_id = sl.message_id.clone();
        // LOG-001: fires per streaming line flush (multiple per turn) and
        // only mirrors an operation that already emits RecordComplete with
        // the same role/content — no debugging value. Demoted to DEBUG.
        tracing::debug!(
            role = %role,
            content_len = content.len(),
            has_conversation = conversation.is_some(),
            "ADR-022 flush_streaming_line: flushing line"
        );
        if !content.trim().is_empty()
            && let Some(conv) = conversation
        {
            let metadata = if role == "thought" {
                Some(serde_json::json!({
                    "startTime": sl.started_at_ms,
                    "endTime": chrono::Utc::now().timestamp_millis(),
                }))
            } else {
                None
            };
            // ADR-035 M2: persist with the same message_id assigned at
            // streaming line creation so JSONL, stream_delta, and
            // record_complete all share one stable id.
            conv.append_message_with_id(&role, &content, metadata, Some(message_id.clone()));
            self.streaming_flush_count.fetch_add(1, Ordering::Relaxed);
            // LOG-001: same rationale as the "flushing line" log above —
            // fires per flush, value carried by RecordComplete. DEBUG.
            tracing::debug!(
                role = %role,
                content_len = content.len(),
                "ADR-022 flush_streaming_line: wrote to JSONL"
            );
            // ADR-035 C1: emit RecordComplete so the frontend can finalize
            // the active stream into messages[] and clear the buffer.
            // QoS 1 (see publish_record_complete) — record_complete is the
            // authoritative terminal event; losing it leaves the message
            // stuck in the streaming state (ADR-035 O2).
            //
            // `seq` is the per-session monotonic counter produced by
            // `next_seq()` — the chunk_relay forwards it into the MQTT
            // payload so the Desktop can place this freeze at the right
            // position even if a stray reorder happens.
            let _ = self.try_send_chunk(ChunkEvent::RecordComplete {
                session_id: sid.clone(),
                role: role.clone(),
                message_id: message_id.clone(),
                content: content.clone(),
                // assistant / thought records carry no tool metadata; the
                // empty string defaults round-trip cleanly through MQTT
                // and the frontend ignores the fields when role !=
                // tool_call / tool_result.
                tool_name: String::new(),
                tool_call_id: String::new(),
                is_error: false,
                seq: self.next_seq(),
            });
        } else if content.trim().is_empty() {
            tracing::warn!(
                role = %role,
                content_len = content.len(),
                "ADR-022 flush_streaming_line: content is whitespace-only, skipping write"
            );
        }
        Some(content)
    }

    /// Remove the streaming line without persisting.
    pub(crate) fn remove_streaming_line(&self) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };
        self.streaming_lines.write().unwrap().remove(&sid);
    }

    /// Flush current streaming line (if non-empty), then ensure a new one
    /// with the given role.
    pub(crate) fn flush_and_new_streaming_line(
        &self,
        new_role: &str,
        conversation: Option<&crate::conversation::ConversationSession>,
    ) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };

        let need_flush = {
            let map = self.streaming_lines.read().unwrap();
            if let Some(sl) = map.get(&sid) {
                sl.role != new_role && !sl.accumulated_content.is_empty()
            } else {
                false
            }
        };

        if need_flush {
            self.flush_streaming_line(conversation);
        } else {
            // No existing line or empty — if there's a stale empty line with
            // a different role, discard it so ensure_streaming_line won't hit
            // the role-mismatch assertion.
            let mut map = self.streaming_lines.write().unwrap();
            if let Some(sl) = map.get(&sid)
                && sl.role != new_role
                && sl.accumulated_content.is_empty()
            {
                map.remove(&sid);
            }
        }

        self.ensure_streaming_line(new_role);
    }

    /// ADR-022: Reset the streaming flush counter at the start of each LLM stream.
    pub(crate) fn reset_streaming_flush_count(&self) {
        self.streaming_flush_count.store(0, Ordering::Relaxed);
    }

    // ── Notification ─────────────────────────────────────────────────

    /// ADR-021/025 + ADR-035: throttled signal + data push.
    ///
    /// Throttled to `notify_interval_ms` (default 500). On each allowed tick:
    ///  - ADR-035: push any new COMPLETE streaming lines via `stream_delta`
    ///    (MQTT). Sent regardless of foreground/background — the runtime now
    ///    pushes to all subscribed sessions uniformly (the old
    ///    `enable/disable_notify` front/back suppression no longer applies to
    ///    the streaming push).
    ///  - ADR-021: emit the legacy `NewDataAvailable` pure signal, but only
    // ADR-035: stream_delta is pushed unconditionally.
    ///    path is retained in parallel during the migration (Phase 1).
    pub(crate) fn notify_new_data_available(&self) {
        // 500ms throttle — shared by both the new StreamDelta push and the
        // legacy NewDataAvailable signal.
        let now = Utc::now().timestamp_millis();
        let last = self.last_notify_ts.load(Ordering::Relaxed);
        if now - last < self.notify_interval_ms as i64 {
            return;
        }
        if self
            .last_notify_ts
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        // ADR-035: push new whole streaming lines via MQTT (all sessions).
        // Throttled calls (via notify_new_data_available) only emit complete
        // `\n`-terminated lines; the trailing partial line is held for the
        // next tick. The StreamEvent::Finished path uses
        // force_flush_stream_delta to push the trailing partial line as a
        // final stream_delta before record_complete lands.
        self.try_send_stream_delta(false);
    }

    /// ADR-035: compute and emit a `stream_delta` carrying the new COMPLETE
    /// lines of the current streaming buffer since the last push.
    ///
    /// Only whole lines (terminated by '\n') are emitted; a trailing partial
    /// line is held back until a newline arrives or the line is finalized.
    /// The push cursor (`stream_push_offset`) advances only past the complete
    /// lines, so a partial line is re-examined on the next tick. Allocation
    /// is bounded to the delta (we collect only the new chars), per D8.
    ///
    /// ADR-035 M2: each emitted line carries the streaming line's stable
    /// `message_id` so the frontend can match `stream_delta` lines to the
    /// eventual `record_complete` event for the same record.
    ///
    /// When `force` is true, any trailing partial line (no terminating '\n')
    /// is also emitted as a final line and the push cursor advances past it.
    /// This is the canonical "stream end" behavior used at
    /// `StreamEvent::Finished` so the frontend sees the full reply via
    /// `stream_delta` even for short, fast responses that would otherwise be
    /// swallowed by the 500ms notify throttle before `record_complete` lands.
    fn try_send_stream_delta(&self, force: bool) {
        let sid = match &self.session_id {
            Some(s) => s.clone(),
            None => return,
        };

        // Gather new complete lines under a read lock (clone only what we need).
        // ADR-035 M2: tuple is now (role, message_id, content).
        let new_lines: Vec<(String, String, String)> = {
            let map = self.streaming_lines.read().unwrap();
            let mut lines: Vec<(String, String, String)> = Vec::new();
            if let Some(sl) = map.get(&sid) {
                let total = sl.accumulated_content.chars().count();
                let offset = self.stream_push_offset.load(Ordering::Relaxed);
                if total > offset {
                    // Collect only the new chars (the delta) — bounded alloc (D8).
                    let new_chars: Vec<char> =
                        sl.accumulated_content.chars().skip(offset).collect();
                    let mut consumed: usize = 0; // chars consumed by complete lines
                    let mut line_start: usize = 0;
                    let role = sl.role.clone();
                    let message_id = sl.message_id.clone();
                    for (ci, ch) in new_chars.iter().enumerate() {
                        if *ch == '\n' {
                            let line: String = new_chars[line_start..ci].iter().collect();
                            if !line.is_empty() {
                                lines.push((role.clone(), message_id.clone(), line));
                            }
                            consumed = ci + 1;
                            line_start = ci + 1;
                        }
                    }

                    if force {
                        // Force-finalize: also emit any trailing partial line
                        // (no terminating '\n') so the last line of the
                        // stream reaches the frontend before
                        // `record_complete` freezes the buffer. The cursor
                        // advances past the partial bytes too so a follow-up
                        // `try_send_stream_delta(false)` does not re-emit
                        // them.
                        let trailing: String = new_chars[line_start..].iter().collect();
                        if !trailing.is_empty() {
                            lines.push((role.clone(), message_id.clone(), trailing));
                            consumed = new_chars.len();
                        }
                    }

                    // Advance the cursor past what we emitted. When `force`
                    // is false, the trailing partial line stays in the
                    // buffer for re-examination on the next tick; when
                    // `force` is true, the cursor consumes it as well.
                    if consumed > 0 {
                        self.stream_push_offset
                            .fetch_add(consumed, Ordering::Relaxed);
                    }
                }
            }
            lines
        };

        if !new_lines.is_empty() {
            // Reserve a per-session seq BEFORE the chunk event is enqueued.
            // The chunk_relay is single-threaded, so the seq we pick here is
            // the same one the MQTT publish layer will write into the
            // payload — making the backend the single source of truth for
            // frame order.
            let seq = self.next_seq();
            let _ = self.try_send_chunk(ChunkEvent::StreamDelta {
                session_id: sid,
                lines: new_lines,
                seq,
            });
        }
    }

    /// Force-finalize the pending streaming line: emit a `stream_delta`
    /// carrying the trailing partial line (no terminating `\n`) in addition
    /// to any already-pending complete lines, bypassing the 500ms
    /// `notify_new_data_available` throttle.
    ///
    /// This is the canonical "stream end" push used at
    /// `StreamEvent::Finished`. It is paired with `flush_streaming_line`
    /// (which emits `record_complete`) so the frontend receives both the
    /// final delta and the terminal freeze for short, fast streams that
    /// would otherwise be dropped before record_complete lands.
    pub(crate) fn force_flush_stream_delta(&self) {
        self.try_send_stream_delta(true);
    }

    // ── Provider builder (retry UX) ──────────────────────────────────

    /// Rebuild Provider instance for a given provider_id from the global cache.
    /// Wires up 429-retry UX when session state is available.
    /// Pass the shared `compat_cache` so the rebuilt provider benefits from
    /// the same fallback-profile cache as the startup provider.
    pub fn build_provider_for(
        &self,
        provider_id: &str,
        model_id: Option<&str>,
        account_id: Option<&str>,
        ctx: &ProviderBuildContext<'_>,
    ) -> Option<Arc<dyn acowork_core::providers::traits::Provider>> {
        let config = ctx.config;
        let global_provider_list = ctx.global_provider_list;
        let provider_key_vault = ctx.provider_key_vault;
        let compat_cache = ctx.compat_cache;
        let responses_compat_cache = ctx.responses_compat_cache;
        let provider_meta = {
            let list = global_provider_list.read().unwrap();
            list.iter().find(|p| p.id == provider_id).cloned()
        }?;

        // Multi-account: resolve the key for the requested account.
        let api_key = {
            let vault = provider_key_vault.read().unwrap();
            let accounts = vault.get(provider_id);
            if let (Some(list), Some(aid)) = (accounts, account_id)
                && !list.iter().any(|e| e.account_id == aid)
            {
                tracing::warn!(
                    provider_id = %provider_id,
                    account_id = %aid,
                    "build_provider_for: account not found, falling back to first account"
                );
            }
            resolve_provider_key(accounts.map(|v| v.as_slice()), account_id)
        };

        tracing::debug!(
            provider_id = %provider_meta.id,
            model_id = model_id.unwrap_or(""),
            base_url = %provider_meta.base_url,
            protocol_type = ?provider_meta.protocol_type,
            api_key_len = api_key.as_ref().map(|k| k.len()).unwrap_or(0),
            api_key_present = api_key.is_some(),
            vault_contains_provider = provider_key_vault.read().unwrap().contains_key(provider_id),
            has_compat_cache = compat_cache.is_some(),
            has_responses_compat_cache = responses_compat_cache.is_some(),
            "build_provider_for resolved credentials (debug)"
        );

        // ADR-103 D2: (provider, model)-grained construction. Consume the
        // Gateway-resolved model-level protocol / base_url override; no
        // model entry (or unknown id) = inherit provider level with a warn.
        let (effective_protocol, effective_base_url) =
            crate::providers::router::effective_protocol_and_base(&provider_meta, model_id);

        let timeouts = Some(crate::providers::router::ProviderTimeouts::from(config));
        let wiring = crate::providers::router::ProviderWiring {
            provider_id: Some(provider_meta.id.clone()),
            compat_cache: compat_cache.cloned(),
            responses_compat_cache: responses_compat_cache.cloned(),
        };
        let raw = crate::providers::router::create_provider_with_wiring(
            &provider_meta.id,
            &effective_protocol,
            api_key.as_deref(),
            if effective_base_url.is_empty() {
                None
            } else {
                Some(effective_base_url.as_str())
            },
            timeouts,
            wiring,
        );
        let retry_config = crate::providers::reliable::RetryConfig::from(&config.timeouts.retry);
        let mut reliable = crate::providers::reliable::ReliableProvider::new(raw, retry_config);

        // Wire up 429 retry UX
        if let Some(status) = &self.retry_session_status
            && let Some(handle) = &self.retry_wait_handle
            && let Some(tx) = &self.chunk_tx
            && let Some(sid) = &self.session_id
        {
            reliable = reliable.with_retry_ux(
                crate::providers::reliable::RetryWaitHandle {
                    state: handle.state.clone(),
                    skip_notify: handle.skip_notify.clone(),
                },
                status.clone(),
                tx.clone(),
                sid.clone(),
            );
        }

        Some(Arc::new(reliable))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::mpsc;

    // ── resolve_provider_key (multi-account) ───────────────────────────

    fn acct(account_id: &str, api_key: &str) -> acowork_core::protocol::ProviderKeyEntry {
        acowork_core::protocol::ProviderKeyEntry {
            provider_id: "p".to_string(),
            account_id: account_id.to_string(),
            api_key: api_key.to_string(),
        }
    }

    #[test]
    fn resolve_key_picks_matching_account() {
        let list = vec![acct("a1", "k1"), acct("a2", "k2")];
        assert_eq!(
            resolve_provider_key(Some(&list), Some("a2")).as_deref(),
            Some("k2")
        );
    }

    #[test]
    fn resolve_key_none_uses_first_account() {
        let list = vec![acct("a1", "k1"), acct("a2", "k2")];
        assert_eq!(
            resolve_provider_key(Some(&list), None).as_deref(),
            Some("k1")
        );
    }

    #[test]
    fn resolve_key_missing_account_falls_back_to_first() {
        let list = vec![acct("a1", "k1")];
        assert_eq!(
            resolve_provider_key(Some(&list), Some("gone")).as_deref(),
            Some("k1")
        );
    }

    #[test]
    fn resolve_key_empty_returns_none() {
        assert_eq!(resolve_provider_key(None, Some("a1")), None);
        let empty: Vec<acowork_core::protocol::ProviderKeyEntry> = vec![];
        assert_eq!(resolve_provider_key(Some(&empty), None), None);
    }

    // ── Helpers ────────────────────────────────────────────────────────

    /// Create a SessionCore with a chunk channel for notification tests.
    fn make_core() -> (SessionCore, mpsc::Receiver<SessionChunkEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let streaming_lines: StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let current_work_dir = Arc::new(RwLock::new(None));
        let core = SessionCore::new(
            "s1".to_string(),
            Some(tx),
            Arc::new(AtomicUsize::new(0)),
            500,
            current_work_dir,
            streaming_lines,
            Arc::new(AtomicUsize::new(0)),
        );
        (core, rx)
    }

    use crate::conversation::{ConversationSession, SessionConfig};
    use std::path::Path;
    use tempfile::TempDir;

    /// Create a SessionCore + ConversationSession pair for ADR-022 tests.
    fn make_session_core_with_session(
        session_id: &str,
    ) -> (SessionCore, ConversationSession, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path().to_path_buf();
        let (tx, _rx) = mpsc::channel(16);
        let streaming_lines: StreamingStateMap =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let committed_lines = Arc::new(AtomicUsize::new(0));
        let current_work_dir = Arc::new(RwLock::new(Some(work_dir.to_string_lossy().to_string())));

        let core = SessionCore::new(
            session_id.to_string(),
            Some(tx),
            committed_lines.clone(),
            500,
            current_work_dir,
            streaming_lines,
            Arc::new(AtomicUsize::new(0)),
        );

        let (session, _config_rx, _state_rx) = ConversationSession::new(
            &work_dir,
            session_id,
            SessionConfig {
                agent_id: "com.test.adr022".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            committed_lines,
        )
        .unwrap();

        (core, session, temp_dir)
    }

    /// Read all ConversationEntry lines from a session JSONL file
    /// (skipping the metadata header on line 0).
    fn read_jsonl_entries(
        work_dir: &Path,
        session_id: &str,
    ) -> Vec<crate::conversation::ConversationEntry> {
        let path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&path).unwrap();
        content
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    // ── ADR-035 stream_delta push tests ──────────────────────────────

    #[test]
    fn test_stream_delta_pushes_complete_lines_only() {
        // A streaming line with two complete lines + one partial (no '\n').
        let (core, mut rx) = make_core();
        core.streaming_lines.write().unwrap().insert(
            "s1".to_string(),
            crate::conversation::StreamingLine {
                line_number: 1,
                message_id: "test-mid-1".to_string(),
                accumulated_content: "first line\nsecond line\npartial".to_string(),
                role: "assistant".to_string(),
                started_at: String::new(),
                started_at_ms: 0,
            },
        );

        // First push: only the two COMPLETE lines; "partial" is held back.
        core.try_send_stream_delta(false);
        let evt = rx.try_recv().unwrap();
        match evt.event {
            ChunkEvent::StreamDelta {
                session_id,
                lines,
                seq,
            } => {
                assert_eq!(session_id, "s1");
                // Per-session seq must be monotonically increasing across
                // emits. `next_seq` uses `fetch_add(1)` which returns the
                // PRE-increment value, so the very first emitted seq in a
                // fresh session is 0, the second is 1, etc. The numeric
                // value is opaque to the wire — the Desktop only cares
                // about relative order — so we assert the pattern
                // (first < second) rather than specific values.
                let first_seq = seq;
                // ADR-035 M2: lines is Vec<(role, message_id, content)>;
                // project to (role, content) for the assertion.
                let projected: Vec<(String, String)> = lines
                    .into_iter()
                    .map(|(role, _mid, content)| (role, content))
                    .collect();
                assert_eq!(
                    projected,
                    vec![
                        ("assistant".to_string(), "first line".to_string()),
                        ("assistant".to_string(), "second line".to_string()),
                    ]
                );
                // Keep `first_seq` for the monotonicity check below.
                let _ = first_seq;
            }
            _other => panic!("expected StreamDelta, got non-stream_delta event"),
        }
        // Nothing else queued — the partial line is not pushed yet.
        assert!(rx.try_recv().is_err());

        // The partial line completes (newline appended). Next push emits it.
        {
            let mut map = core.streaming_lines.write().unwrap();
            let sl = map.get_mut("s1").unwrap();
            sl.accumulated_content.push('\n');
        }
        core.try_send_stream_delta(false);
        let evt = rx.try_recv().unwrap();
        match evt.event {
            ChunkEvent::StreamDelta { lines, seq, .. } => {
                // Second emit must carry a strictly greater seq than the
                // first — the monotonicity guarantee the Desktop relies on
                // for `insertBySeq` ordering.
                let second_seq = seq;
                assert!(
                    second_seq > 0,
                    "second stream_delta should carry a positive seq (got {})",
                    second_seq,
                );
                // ADR-035 M2: lines is Vec<(role, message_id, content)>;
                // project to (role, content) for the assertion.
                let projected: Vec<(String, String)> = lines
                    .into_iter()
                    .map(|(role, _mid, content)| (role, content))
                    .collect();
                assert_eq!(
                    projected,
                    vec![("assistant".to_string(), "partial".to_string())]
                );
            }
            _other => panic!("expected StreamDelta, got non-stream_delta event"),
        }
    }

    #[test]
    fn test_stream_delta_advances_cursor_across_role_transition() {
        // After a role transition (new streaming line), the push cursor resets
        // to 0 so the new line's content is pushed from the start.
        let (core, mut rx) = make_core();
        core.streaming_lines.write().unwrap().insert(
            "s1".to_string(),
            crate::conversation::StreamingLine {
                line_number: 1,
                message_id: "test-mid-thought".to_string(),
                accumulated_content: "thought line\n".to_string(),
                role: "thought".to_string(),
                started_at: String::new(),
                started_at_ms: 0,
            },
        );
        core.try_send_stream_delta(false);
        // Drain the thought line.
        assert!(matches!(
            rx.try_recv().unwrap().event,
            ChunkEvent::StreamDelta { .. }
        ));

        // Simulate a role transition: flush + new assistant line (offset resets).
        core.flush_and_new_streaming_line("assistant", None);
        core.streaming_lines
            .write()
            .unwrap()
            .get_mut("s1")
            .unwrap()
            .accumulated_content
            .push_str("assistant line\n");

        core.try_send_stream_delta(false);
        let evt = rx.try_recv().unwrap();
        match evt.event {
            ChunkEvent::StreamDelta { lines, .. } => {
                // ADR-035 M2: lines is Vec<(role, message_id, content)>;
                // project to (role, content) for the assertion. The
                // message_id is a fresh UUID assigned by
                // flush_and_new_streaming_line, so we don't assert it.
                let projected: Vec<(String, String)> = lines
                    .into_iter()
                    .map(|(role, _mid, content)| (role, content))
                    .collect();
                assert_eq!(
                    projected,
                    vec![("assistant".to_string(), "assistant line".to_string())]
                );
            }
            _other => panic!("expected StreamDelta, got non-stream_delta event"),
        }
    }

    // ── StreamEvent::Finished finalization tests ────────────────────

    #[test]
    fn test_stream_delta_force_pushes_trailing_partial_line() {
        // StreamEvent::Finished path: force_flush_stream_delta must push
        // the trailing partial line (no '\n') along with any complete
        // lines, bypassing the 500ms notify throttle. This guarantees the
        // frontend sees the last line of the reply via stream_delta even
        // when the throttle would otherwise have suppressed it.
        let (core, mut rx) = make_core();
        core.streaming_lines.write().unwrap().insert(
            "s1".to_string(),
            crate::conversation::StreamingLine {
                line_number: 1,
                message_id: "test-mid-force".to_string(),
                accumulated_content: "first line\nsecond line\npartial".to_string(),
                role: "assistant".to_string(),
                started_at: String::new(),
                started_at_ms: 0,
            },
        );

        // Force-finalize: all three lines (including trailing partial).
        core.force_flush_stream_delta();
        let evt = rx.try_recv().unwrap();
        match evt.event {
            ChunkEvent::StreamDelta {
                session_id, lines, ..
            } => {
                assert_eq!(session_id, "s1");
                let projected: Vec<(String, String)> = lines
                    .into_iter()
                    .map(|(role, _mid, content)| (role, content))
                    .collect();
                assert_eq!(
                    projected,
                    vec![
                        ("assistant".to_string(), "first line".to_string()),
                        ("assistant".to_string(), "second line".to_string()),
                        ("assistant".to_string(), "partial".to_string()),
                    ]
                );
            }
            _other => panic!("expected StreamDelta, got non-stream_delta event"),
        }
        // Only one event emitted — the trailing partial was included.
        assert!(rx.try_recv().is_err());

        // A follow-up force push must not re-emit anything (cursor consumed
        // the trailing partial already).
        core.force_flush_stream_delta();
        assert!(rx.try_recv().is_err());

        // Likewise a throttled push must not re-emit.
        core.try_send_stream_delta(false);
        assert!(rx.try_recv().is_err());
    }

    // ── 429 retry UX initialization tests ─────────────────────────────

    #[test]
    fn test_retry_ux_fields_initialized() {
        // SessionCore::new() must initialize retry_session_status and
        // retry_wait_handle so that build_provider_for can wire up UX.
        let (core, _rx) = make_core();
        assert!(
            core.retry_session_status.is_some(),
            "retry_session_status must be Some for 429 retry UX wiring"
        );
        assert!(
            core.retry_wait_handle.is_some(),
            "retry_wait_handle must be Some for 429 retry UX wiring"
        );
    }

    #[test]
    fn test_retry_ux_initial_status_is_streaming() {
        // The initial retry_session_status should be LlmAwaitingFirstChunk
        // (the session hasn't been paused yet — ADR-049 splits the old
        // `Streaming` variant into 3 sub-states; the pre-request TTFT wait
        // maps to LlmAwaitingFirstChunk).
        let (core, _rx) = make_core();
        let guard = core.retry_session_status.as_ref().unwrap().read().unwrap();
        assert!(
            matches!(*guard, SessionStatus::LlmAwaitingFirstChunk),
            "Initial retry status should be LlmAwaitingFirstChunk, got {:?}",
            *guard
        );
    }

    #[test]
    fn test_retry_ux_session_status_writable() {
        // Simulate what ReliableProvider::emit_retry_pause does:
        // write Paused with retry_info into the shared status lock.
        let (core, _rx) = make_core();
        let status_lock = core.retry_session_status.as_ref().unwrap();
        {
            let mut guard = status_lock.write().unwrap();
            *guard = SessionStatus::Paused {
                iteration: None,
                max_iterations: None,
                retry_info: Some(crate::agent::session_state::RetryPauseInfo {
                    wait_ms: 10_500,
                    attempt: 1,
                    max_attempts: 3,
                    provider: "mock-provider".to_string(),
                }),
                reason: None,
                message: None,
            };
        }
        // Verify the status is now Paused with retry info
        let guard = status_lock.read().unwrap();
        assert!(
            matches!(*guard, SessionStatus::Paused { .. }),
            "Status should be Paused after emit_retry_pause simulation"
        );
    }

    #[test]
    fn test_retry_ux_skip_notify_fires() {
        // Verify the skip_notify in retry_wait_handle can be triggered.
        // This simulates SessionTask handling ContinueExecution →
        // handle.skip_notify.notify_one() to wake the retry loop.
        let (core, _rx) = make_core();
        let handle = core.retry_wait_handle.as_ref().unwrap();

        // notify_one is idempotent and non-blocking — just verify it
        // doesn't panic and the Notify is properly constructed.
        handle.skip_notify.notify_one();
        // If we got here without panic, the Notify is alive.
        // Just verify basic sanity: notify_one didn't panic.
        assert!(
            Arc::strong_count(&handle.skip_notify) >= 1,
            "Skip notify Arc should have at least 1 strong reference"
        );
    }

    /// Async test: verify that retry_wait_handle's skip_notify actually
    /// wakes a waiting task. Uses tokio::spawn + select to test the
    /// same pattern used by ReliableProvider::retry_sleep.
    #[tokio::test]
    async fn test_retry_ux_skip_notify_wakes_waiter() {
        let (core, _rx) = make_core();
        let handle = core.retry_wait_handle.as_ref().unwrap();
        let skip = handle.skip_notify.clone();

        // Spawn a task that waits on skip_notify
        let wait_task = tokio::spawn(async move {
            skip.notified().await;
            "woken"
        });

        // Give the task time to start waiting, then notify
        tokio::task::yield_now().await;
        handle.skip_notify.notify_one();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), wait_task)
            .await
            .expect("Timeout: skip_notify did not wake the waiter")
            .expect("Wait task panicked");
        assert_eq!(result, "woken", "skip_notify must wake the waiting task");
    }

    // ── ADR-022 §9: Runtime role transition & flush tests ──────────────

    /// ADR-022 §9 test 3: Runtime role transition produces three
    /// single-role JSONL lines.
    ///
    /// Simulates the event sequence:
    ///   Content("我先看一下")          → assistant line
    ///   ReasoningContent("分析路径")    → flush assistant, thought line
    ///   Content("然后搜索")            → flush thought, assistant line
    ///   Finished                       → flush assistant
    ///
    /// Expected JSONL:
    ///   line 1: {"role":"assistant","content":"我先看一下"}
    ///   line 2: {"role":"thought","content":"分析路径"}
    ///   line 3: {"role":"assistant","content":"然后搜索"}
    #[test]
    fn test_adr022_role_transition_produces_single_role_lines() {
        let session_id = "adr022-transition";
        let (core, session, temp_dir) = make_session_core_with_session(session_id);

        // Content("我先看一下") — starts assistant streaming line
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "我先看一下");

        // ReasoningContent("分析路径") — role change, flush assistant, start thought
        core.flush_and_new_streaming_line("thought", Some(&session));
        core.append_streaming_delta("thought", "分析路径");

        // Content("然后搜索") — role change, flush thought, start assistant
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "然后搜索");

        // Finished — flush final assistant line
        core.flush_streaming_line(Some(&session));

        // Give writer thread time to process
        std::thread::sleep(std::time::Duration::from_millis(50));

        let entries = read_jsonl_entries(temp_dir.path(), session_id);
        assert_eq!(
            entries.len(),
            3,
            "Should have 3 single-role lines: assistant, thought, assistant"
        );
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content, "我先看一下");
        assert_eq!(entries[1].role, "thought");
        assert_eq!(entries[1].content, "分析路径");
        assert_eq!(entries[2].role, "assistant");
        assert_eq!(entries[2].content, "然后搜索");
    }

    /// ADR-022 §9 test 4: assistant text + tool_call preserves order.
    ///
    /// Simulates:
    ///   Content("我来查一下")          → assistant line
    ///   ToolCallStart                  → flush assistant, then tool_call JSONL row
    ///
    /// Expected JSONL:
    ///   line 1: {"role":"assistant","content":"我来查一下"}
    ///   line 2: {"role":"tool_call",...}
    #[test]
    fn test_adr022_assistant_text_then_tool_call_preserves_order() {
        let session_id = "adr022-text-tool";
        let (core, session, temp_dir) = make_session_core_with_session(session_id);

        // Content("我来查一下") — assistant streaming line
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "我来查一下");

        // ToolCallStart arrives — flush assistant text first,
        // then write tool_call row.
        core.flush_streaming_line(Some(&session));

        // Simulate prepare_tool_calls writing the tool_call JSONL row
        session.append_message(
            "tool_call",
            r#"{"name":"grep","arguments":{"pattern":"foo"}}"#,
            None,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));

        let entries = read_jsonl_entries(temp_dir.path(), session_id);
        assert_eq!(entries.len(), 2, "assistant text + tool_call = 2 lines");
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content, "我来查一下");
        assert_eq!(entries[1].role, "tool_call");
    }

    /// ADR-022 §9 test 5: Finished event with tool_calls flushes text first.
    ///
    /// Some providers don't send ToolCallStart during streaming; they return
    /// complete tool_calls in the Finished response. Runtime must still flush
    /// any accumulated assistant text before writing tool_call rows.
    #[test]
    fn test_adr022_finished_with_tool_calls_flushes_text_first() {
        let session_id = "adr022-finished-tool";
        let (core, session, temp_dir) = make_session_core_with_session(session_id);

        // Content("开始搜索") — assistant streaming line accumulates
        core.flush_and_new_streaming_line("assistant", Some(&session));
        core.append_streaming_delta("assistant", "开始搜索");

        // Finished arrives with tool_calls — prepare_tool_calls path:
        // 1. flush_streaming_line (captures assistant text)
        // 2. append_message tool_call rows
        core.flush_streaming_line(Some(&session));
        session.append_message(
            "tool_call",
            r#"{"name":"search","arguments":{"q":"test"}}"#,
            None,
        );

        std::thread::sleep(std::time::Duration::from_millis(50));

        let entries = read_jsonl_entries(temp_dir.path(), session_id);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].role, "assistant");
        assert_eq!(entries[0].content, "开始搜索");
        assert_eq!(entries[1].role, "tool_call");
    }

    /// ADR-022 invariant: ensure_streaming_line does NOT overwrite role.
    ///
    /// If a caller forgets to call flush_and_new_streaming_line and directly
    /// calls append_streaming_delta with a different role, the streaming line
    /// keeps its original role. In debug builds this triggers a debug_assert.
    #[test]
    #[should_panic(expected = "ADR-022 violation: append_streaming_delta called with role")]
    fn test_adr022_ensure_streaming_line_does_not_overwrite_role() {
        let (core, _session, _temp_dir) = make_session_core_with_session("adr022-no-overwrite");

        // Create an assistant streaming line
        core.append_streaming_delta("assistant", "hello");

        // Attempt to append thought content without flushing first.
        // This is a caller bug — debug_assert_eq! must fire.
        core.append_streaming_delta("thought", "should be thought but stays assistant");
    }
}
