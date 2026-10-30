//! LLM streaming module
//!
//! Extracted from [`super::loop_`] to provide reusable LLM streaming logic
//! shared between production [`AgentLoop::run`] and debug `DebugSessionTask`.
//!
//! Handles:
//! - Stream processing (Content, ReasoningContent, ToolCallStart, ToolCallChunk, etc.)
//! - Context overflow recovery (emergency trim + retry)
//! - Interrupt handling during streaming
//! - Tool call argument accumulation and dedup

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use acowork_core::providers::traits::{ChatResponse, StreamEvent, ToolCall};
use chrono::Utc;
use futures::StreamExt;

use super::context::ContextBuilder;
use super::loop_::{AgentLoop, ChunkEvent, ControlDecision};
use crate::agent::session_state::SessionStatus;
use crate::cancellation::select_on_cancel;
use crate::error::{Result, RuntimeError};
use crate::util::text::TextPreview;

impl AgentLoop {
    /// Call LLM with streaming, accumulating content and tool calls.
    ///
    /// Handles context overflow recovery by detecting relevant errors
    /// from the stream and retrying after emergency trim.
    pub(crate) async fn call_llm_streaming(
        &mut self,
        chat_request: &acowork_core::providers::traits::ChatRequest,
        context_builder: &ContextBuilder,
    ) -> Result<ChatResponse> {
        self.call_llm_streaming_inner(chat_request, Some(context_builder))
            .await
    }

    /// Single-attempt streaming call (no retry on context overflow).
    ///
    /// Used after emergency trim to avoid infinite recursion.
    pub(crate) fn call_llm_streaming_no_retry<'a>(
        &'a mut self,
        chat_request: &'a acowork_core::providers::traits::ChatRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<ChatResponse>> + Send + 'a>>
    {
        Box::pin(async move { self.call_llm_streaming_inner(chat_request, None).await })
    }

    /// Common streaming implementation.
    ///
    /// When `context_builder` is `Some`, context overflow recovery is enabled
    /// (retry after emergency trim). When `None`, errors are returned directly.
    pub(crate) async fn call_llm_streaming_inner(
        &mut self,
        chat_request: &acowork_core::providers::traits::ChatRequest,
        context_builder: Option<&ContextBuilder>,
    ) -> Result<ChatResponse> {
        let retry_on_overflow = context_builder.is_some();

        tracing::debug!(
            system_prompt_len = chat_request
                .messages
                .first()
                .map(|m| m.content.len())
                .unwrap_or(0),
            tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            messages_count = chat_request.messages.len(),
            "Sending LLM request"
        );
        // ADR-021: Frontend polls via HTTP — ReasoningStarted is no longer
        // sent via channel. The frontend detects streaming state via poll.
        //
        // ADR-044 Phase 3 (TTFT stop bug fix, ADR §1.3 / §4.4): wrap the
        // `chat_stream()` future in `select_on_cancel` so a Stop signal that
        // arrives *during* TCP/TLS/HTTP-headers/SSE-first-chunk — any of
        // which can take 10–30s — drops the underlying `reqwest::send()`
        // future within microseconds. Without this wrapper, the bare
        // `.await` is not contained in any `tokio::select!` so neither the
        // 500ms idle poll nor a `Notify` can wake it; the user-perceived
        // stop latency equals the upstream TTFT.
        //
        // `select_on_cancel` races the future against the session's
        // `CancelHandle`. On cancel, the `chat_stream` future is
        // dropped (HTTP request aborted), we receive `Ok(None)` and return
        // a stopped ChatResponse so the upstream `loop_::run_inner` sees the
        // same shape it would after a mid-stream Stop. The chunk channel
        // also receives `ChunkEvent::Stopped { content: "" }` so the UI
        // can clear its streaming indicator immediately.
        //
        // ADR-044 §4.5: the handle here is the *current request's* handle
        // — `run_inner` called `begin_new_request()` before entering this
        // method, so a Cancel from a prior request cannot short-circuit
        // this run.
        let cancel_handle = self.session_core.cancel_handle();
        // Session perf timing starts here — one `Instant`, three marks:
        // `t0` (this line), the first output event (TTFT), and stream end
        // (generation duration). The slot is cleared on entry and written
        // once on completion, so the consumer's `take()` sees exactly one
        // sample per call — a value left over from the previous round would
        // otherwise be folded into the session mean a second time.
        self.last_call_perf = None;
        let call_t0 = std::time::Instant::now();
        let provider_stream = select_on_cancel(
            cancel_handle.clone(),
            self.core.provider.chat_stream(chat_request.clone()),
        )
        .await?;
        let mut stream = match provider_stream {
            Some(s) => Box::into_pin(s),
            None => {
                tracing::info!(
                    "chat_stream() cancelled by user before first chunk arrived — \
                     aborting TTFT wait"
                );
                // Flush any half-written streaming line and emit a Stopped
                // chunk event so the UI drops its streaming indicator even
                // though no content was accumulated. Returns a stopped
                // ChatResponse so the upstream loop sees the same control
                // flow shape it would after a mid-stream Stop.
                self.session_core
                    .flush_streaming_line(self.session.conversation.as_deref());
                let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                    content: String::new(),
                });
                return Ok(build_stopped_response(String::new(), String::new()));
            }
        };
        let mut accumulated_content = String::new();
        let mut accumulated_reasoning_content = String::new();
        let mut tool_calls: Option<Vec<ToolCall>> = None;
        let mut usage = None;
        let mut finish_reason: Option<String> = None;
        let mut reasoning_started_at: Option<i64> = None;
        let mut reasoning_finished_at: Option<i64> = None;
        let mut reasoning_in_progress = false;
        // Timestamp of the first event that carries model output
        // (content / reasoning / tool call) — the TTFT mark. `None` until
        // one arrives; a stream cancelled before any output leaves it
        // unset and the whole measurement is dropped.
        let mut first_output_at: Option<std::time::Instant> = None;

        // ADR-022: No think tag state machine needed here.
        // The provider layer (openai.rs ThinkTagParser) already splits
        // delta.content containing <!think> tags into ReasoningContent +
        // Content events. This loop only needs to handle the structural
        // events and flush the streaming line on role transitions.
        //
        // Reset the streaming flush counter at the start of each stream.
        // handle_text_response / prepare_tool_calls check this counter
        // to decide whether to skip legacy persistence paths.
        self.session_core.reset_streaming_flush_count();

        // ToolCallChunk accumulation buffer: indexed by tool_call sequential index
        let mut tool_call_args_buffer: HashMap<u64, String> = HashMap::new();
        // Track which tool_call indices have accumulated valid JSON so far.
        // Once complete JSON is formed, any further delta chunks for that index
        // are stale duplicates (observed with some OpenAI-compatible APIs) and
        // must be discarded to avoid corrupting the arguments.
        let mut finished_tool_indices: HashSet<u64> = HashSet::new();

        // ── Stream processing loop with periodic stop polling ──
        // Use tokio::select! to check for user stops during stream idle
        // periods (e.g., long LLM reasoning between chunks). Without this,
        // stream.next().await can block for tens of seconds without responding
        // to STOP signals, because poll_stop() would only run between
        // received chunks.
        //
        // When the stream actively sends data, the event branch wins immediately
        // (no 500ms latency). When idle, the sleep branch fires every 500ms.
        loop {
            // ADR-044 §4.5: bind the *current request's* handle to a
            // local so the Arc inside it outlives the `cancelled()` future.
            // Re-read on every iteration so any slot swap that happened
            // mid-stream is observed (defensive — `begin_new_request` is
            // only called from `run_inner` entry, so within one stream
            // consumption the slot is stable in practice).
            let cancel_handle = self.session_core.cancel_handle();
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(event) => {
                            if first_output_at.is_none()
                                && matches!(
                                    event,
                                    StreamEvent::Content(_)
                                        | StreamEvent::ReasoningContent(_)
                                        | StreamEvent::ToolCallStart(_)
                                )
                            {
                                first_output_at = Some(std::time::Instant::now());
                            }
                            // Check for control signals before processing each stream event
                            match self.poll_control() {
                                ControlDecision::Stop => {
                                    tracing::info!("LLM stream stopped by user — aborting");
                                    // ADR-021: Flush partial content to JSONL before stopping
                                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                                    let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                        content: accumulated_content.clone(),
                                    });
                                    return Ok(build_stopped_response(
                                        accumulated_content,
                                        accumulated_reasoning_content,
                                    ));
                                }
                                ControlDecision::Pause => {
                                    tracing::info!("LLM stream paused by debug — aborting");
                                    self.pending_interrupt = Some(ControlDecision::Pause);
                                    // ADR-021: Flush partial content to JSONL before pausing
                                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                                    let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                        content: accumulated_content.clone(),
                                    });
                                    return Ok(build_stopped_response(
                                        accumulated_content,
                                        accumulated_reasoning_content,
                                    ));
                                }
                                ControlDecision::Continue => {}
                            }
                            match event {
                StreamEvent::Content(chunk) => {
                    // ADR-049: First content chunk after `LlmAwaitingFirstChunk`
                    // promotes to `LlmStreaming`. The `accumulated_content.is_empty()`
                    // check is the precise first-content-event marker — reasoning
                    // deltas go to `accumulated_reasoning_content` (a separate
                    // accumulator) so a `Content` event here is always the first
                    // visible reply byte, even after reasoning → assistant role
                    // transition. Frontend flips from "waiting" → "replying".
                    if accumulated_content.is_empty() {
                        self.transition_status(SessionStatus::LlmStreaming {
                            message_id: None,
                        });
                    }

                    // Mark reasoning finished when content starts after reasoning
                    if reasoning_in_progress {
                        reasoning_finished_at = Some(Utc::now().timestamp_millis());
                        reasoning_in_progress = false;
                    }
                    accumulated_content.push_str(&chunk);

                    // ADR-022: Role transition. If we were in a thought
                    // streaming line, flush it to JSONL and start a new
                    // assistant line. The provider layer already split think
                    // tags into separate ReasoningContent events, so a Content
                    // event here means we are in assistant mode.
                    self.session_core.flush_and_new_streaming_line(
                        "assistant", self.session.conversation.as_deref(),
                    );
                    self.session_core.append_streaming_delta("assistant", &chunk);
                    self.session_core.notify_new_data_available();
                }
                StreamEvent::ReasoningContent(chunk) => {
                    // Record start of reasoning on first chunk
                    if reasoning_started_at.is_none() {
                        reasoning_started_at = Some(Utc::now().timestamp_millis());
                        // ADR-049: Transition to Thinking on the first reasoning
                        // chunk. This lets the frontend show "Thinking…" instead
                        // of the generic "Waiting for model…" during reasoning.
                        self.transition_status(SessionStatus::Thinking);
                    }
                    reasoning_in_progress = true;
                    accumulated_reasoning_content.push_str(&chunk);

                    // ADR-022: Role transition. If we were in an assistant
                    // streaming line, flush it to JSONL and start a new
                    // thought line.
                    self.session_core.flush_and_new_streaming_line(
                        "thought", self.session.conversation.as_deref(),
                    );
                    self.session_core.append_streaming_delta("thought", &chunk);
                    self.session_core.notify_new_data_available();
                }
                StreamEvent::ToolCallStart(tc) => {
                    // Mark reasoning finished when tool calls start after reasoning
                    if reasoning_in_progress {
                        reasoning_finished_at = Some(Utc::now().timestamp_millis());
                        reasoning_in_progress = false;
                    }

                    // ADR-021: Do NOT flush streaming line on ToolCallStart.
                    // The thought content is properly persisted with metadata
                    // via persist_think_to_conversation in prepare_tool_calls().
                    // Flushing here caused duplicate thought records (one
                    // without metadata from flush, one with metadata from persist).

                    tracing::info!(
                        tool_name = %tc.function.name,
                        tool_id = %tc.id,
                        initial_args = %tc.function.arguments,
                        "ToolCallStart received"
                    );
                    tool_calls.get_or_insert_with(Vec::new).push(tc);
                    // ADR-021: Notify frontend that a tool call has started.
                    // Without this, the frontend has no way to know that the LLM
                    // emitted tool calls until the entire agent loop iteration
                    // completes — leaving the UI blank while tools execute.
                    // (Text/reasoning chunks already notify during streaming;
                    //  this fills the gap for tool-call-only responses.)
                    self.session_core.notify_new_data_available();
                }
                StreamEvent::ToolCallChunk { index, arguments } => {
                    // LOG-001: fires once per streamed argument chunk (tens
                    // per tool call) — raw accumulation noise, enable TRACE
                    // only when debugging tool-arg assembly.
                    tracing::trace!(index, chunk_len = arguments.len(), "ToolCallChunk received");
                    // Discard stale delta chunks for tool calls that already have complete JSON
                    if !finished_tool_indices.contains(&index) {
                        let buffer = tool_call_args_buffer.entry(index).or_default();
                        buffer.push_str(&arguments);
                        // Check if accumulated arguments now form valid JSON
                        if serde_json::from_str::<serde_json::Value>(buffer).is_ok() {
                            finished_tool_indices.insert(index);
                        }
                    }
                }
                StreamEvent::Finished(resp) => {
                    // Mark reasoning finished on stream end (edge case: no Content/ToolCall after reasoning)
                    if reasoning_in_progress {
                        reasoning_finished_at = Some(Utc::now().timestamp_millis());
                    }
                    // Capture finish_reason for diagnostics
                    if resp.finish_reason.is_some() {
                        finish_reason = resp.finish_reason;
                    }
                    // Use final response data; prefer stream-accumulated content
                    if accumulated_content.is_empty() {
                        accumulated_content = resp.content;
                    }
                    if accumulated_reasoning_content.is_empty() {
                        accumulated_reasoning_content =
                            resp.reasoning_content.unwrap_or_default();
                    }
                    if resp.tool_calls.is_some() {
                        // Prefer Finished event's tool_calls as they are complete
                        tool_calls = resp.tool_calls;
                    } else if tool_calls.is_some() {
                        // Finished has no tool_calls — apply accumulated argument chunks
                        // from the stream to the ToolCallStart entries.
                        // When ToolCallStart already carries initial arguments
                        // (e.g. GLM/DeepSeek send name+args together), do NOT
                        // append buffer content — they are already complete.
                        if let Some(ref mut tcs) = tool_calls {
                            for (i, tc) in tcs.iter_mut().enumerate() {
                                if let Some(args) =
                                    tool_call_args_buffer.get(&(i as u64))
                                    && (tc.function.arguments.is_empty()
                                        || tc.function.arguments == "{}")
                                {
                                    // Validate JSON before applying — stream interruption can
                                    // leave incomplete arguments that would fail at tool execution.
                                    //
                                    // Route through `arguments::resolve` instead of
                                    // `serde_json::from_str(...).is_ok()` so we recover
                                    // valid JSON wrapped in a natural-language prefix
                                    // (DeepSeek 2026-08-24 incident) and preserve the
                                    // original content as a hint in the marker message.
                                    match crate::tools::arguments::resolve(args) {
                                        crate::tools::arguments::ResolvedArgs::Valid(value) => {
                                            // Serialize the JSON back to a string for
                                            // storage in ToolCall.function.arguments
                                            // (the provider trait is typed as String).
                                            if let Ok(s) = serde_json::to_string(&value) {
                                                tc.function.arguments = s;
                                            }
                                        }
                                        crate::tools::arguments::ResolvedArgs::Recovered {
                                            value,
                                            ..
                                        } => {
                                            if let Ok(s) = serde_json::to_string(&value) {
                                                tc.function.arguments = s;
                                            }
                                            tracing::warn!(
                                                tool_name = %tc.function.name,
                                                index = i,
                                                raw_len = args.len(),
                                                "Recovered tool call arguments from natural-language-wrapped stream"
                                            );
                                        }
                                        crate::tools::arguments::ResolvedArgs::Invalid { raw, reason } => {
                                            tracing::error!(
                                                tool_name = %tc.function.name,
                                                index = i,
                                                raw_len = raw.len(),
                                                reason = %reason,
                                                raw_preview = %raw.preview(200),
                                                "Accumulated tool call arguments are not valid JSON and could not be recovered"
                                            );
                                            // Preserve the raw content as the marker hint so
                                            // the LLM can see its original intent next iteration.
                                            tc.function.arguments =
                                                crate::tools::arguments::make_incomplete_marker(
                                                    &tc.function.name,
                                                    &raw,
                                                );
                                        }
                                    }
                                }
                                // If arguments already non-empty (GLM/DeepSeek same-chunk pattern),
                                // they are already complete — do not append buffer content.
                                // DeepSeek sends duplicate complete arguments in subsequent chunks,
                                // appending would produce invalid JSON like {"path": "."}{"path": "."}
                            }
                        }
                    }
                    usage = resp.usage;

                    // Stream end semantics: deliver the trailing partial line
                    // (if any) as a final stream_delta, then freeze the
                    // streaming line as record_complete. Both events are
                    // needed for the frontend to render the complete reply:
                    //
                    //   - `force_flush_stream_delta` bypasses the 500ms
                    //     notify throttle that would otherwise drop short,
                    //     fast streams before record_complete lands. It
                    //     pushes the trailing partial line (no terminating
                    //     '\n') as a final stream_delta line so the last
                    //     line of the reply is not held back forever.
                    //   - `flush_streaming_line` writes the JSONL record
                    //     and emits the terminal `record_complete` event so
                    //     the frontend freezes its active stream buffer
                    //     into `messages[]`.
                    //
                    // The downstream `handle_text_response` will take Path 1
                    // (streaming_flush_count > 0), where its own call to
                    // `flush_streaming_line` is a no-op because
                    // `streaming_lines` is already empty — no duplicate
                    // JSONL write and no duplicate record_complete push.
                    self.session_core.force_flush_stream_delta();
                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());

                    // Diagnostic: log stream completion summary
                    tracing::info!(
                        finish_reason = ?finish_reason,
                        content_len = accumulated_content.len(),
                        reasoning_len = accumulated_reasoning_content.len(),
                        has_tool_calls = tool_calls.is_some(),
                        tool_call_count = tool_calls.as_ref().map(|t| t.len()).unwrap_or(0),
                        "LLM stream Finished event received"
                    );
                    break;
                }
                StreamEvent::Error(e) => {
                    // ADR-021: Flush partial content to JSONL on error
                    // so the user can see what the AI was trying to say
                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());

                    // Check for context overflow and attempt recovery.
                    // Use structured error_type instead of string matching.
                    if retry_on_overflow
                        && e.error_type == acowork_core::providers::traits::ProviderErrorType::ContextOverflow
                    {
                        tracing::warn!(
                            error = %e.message,
                            current_tokens = self.session.history.token_count(),
                            "Context overflow detected in stream, running 8-level compaction"
                        );
                        // ADR-061 §11.2: the overflow retry routes through
                        // the 8-level plan instead of `emergency_trim`
                        // (deleted). When the compaction LLM is unavailable
                        // (or the plan fails), the retry is abandoned —
                        // explicit failure (§11.3) beats retrying a request
                        // that cannot fit the window.
                        let model_name = self.resolve_current_model(context_builder);
                        let before = self.session.history.token_count();
                        // Compaction failure is explicit and terminal —
                        // surface it (non-retryable → GiveUp → Idle)
                        // instead of retrying a request that cannot fit
                        // the window or silently looping.
                        self.compact_history_if_needed(&model_name, true).await?;
                        let after = self.session.history.token_count();
                        if after < before {
                            tracing::info!(
                                before,
                                remaining_tokens = after,
                                "8-level compaction completed, retrying with trimmed context"
                            );
                            let caps = self.get_model_capabilities(&model_name);
                            let max_output_limit = self.core.max_output_tokens_limit_for_model(&model_name);
                            let mut chat_request = context_builder.unwrap().build(
                                &self.session.history,
                                // ADR-060 §5.5 erratum: no Block D arg. The
                                // current user message survives compaction in
                                // the retained tail (Block B), so it is still
                                // present exactly once after the 8-level trim.
                                caps.as_ref(),
                                max_output_limit,
                            );
                            // Preserve reasoning_effort from the original request
                            // (already resolved in build_chat_request()).
                            chat_request.reasoning_effort = self.last_reasoning_effort;
                            chat_request.thinking_mode =
                                self.last_thinking_mode.clone();
                            return self
                                .call_llm_streaming_no_retry(&chat_request)
                                .await;
                        } else {
                            tracing::error!(
                                error = %e.message,
                                "Compaction did not shrink history — aborting overflow retry (ADR-061 §11.3)"
                            );
                            return Err(RuntimeError::StreamError(e));
                        }
                    }
                    return Err(RuntimeError::StreamError(e));
                }
            }
                        }
                        None => {
                            // Stream ended without a Finished event
                            // (common with OpenAI-compatible APIs like MiniMax).
                            // ADR-022: No scratch buffer to flush — the provider
                            // layer's ThinkTagParser already handled tag splitting,
                            // and each Content/ReasoningContent event was flushed
                            // to JSONL via flush_and_new_streaming_line.
                            break;
                        }
                    }
                }
                // Cancellation handle — fires when external sources (MQTT
                // `StopGeneration` dispatcher in `startup/gateway_loop.rs`,
                // debug server Stop, CLI cancel, test harness) call
                // `cancel()` on the session's `CancelHandle`.
                //
                // ADR-044 Phase 3: replaces the legacy `urgent_stop` Notify,
                // which was only triggered in the debug path. The handle's
                // level-triggered `Notify` + persistent `AtomicU8` state
                // means this branch wakes within microseconds of `cancel()`
                // and remains armed even if `cancel()` lands between select!
                // iterations (a race the legacy Notify could miss).
                //
                // ADR-044 §4.5: this is the *current request's* handle —
                // see `SessionCore::begin_new_request` for how the slot is
                // swapped at every `run_inner` entry.
                _ = cancel_handle.cancelled() => {
                    match self.poll_control() {
                        ControlDecision::Stop => {
                            tracing::info!("LLM stream stopped via cancellation token — aborting");
                        }
                        ControlDecision::Pause => {
                            tracing::info!("LLM stream paused via cancellation token — aborting");
                            self.pending_interrupt = Some(ControlDecision::Pause);
                        }
                        ControlDecision::Continue => {}
                    }
                    // ADR-021: Flush partial content to JSONL before stopping
                    self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                    let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                        content: accumulated_content.clone(),
                    });
                    return Ok(build_stopped_response(
                        accumulated_content,
                        accumulated_reasoning_content,
                    ));
                }
                // Periodic control polling during stream idle periods.
                // tokio::select! polls ALL branches simultaneously:
                // - When stream has data ready: event branch wins immediately, sleep is dropped
                // - When stream is idle (waiting for next chunk): sleep fires every 500ms
                _ = tokio::time::sleep(Duration::from_millis(500)) => {
                    match self.poll_control() {
                        ControlDecision::Stop => {
                            tracing::info!(
                                "LLM stream stopped by user during idle period — aborting"
                            );
                            // ADR-021: Flush partial content to JSONL before stopping
                            self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                            let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                content: accumulated_content.clone(),
                            });
                            return Ok(build_stopped_response(
                                accumulated_content,
                                accumulated_reasoning_content,
                            ));
                        }
                        ControlDecision::Pause => {
                            tracing::info!("LLM stream paused during idle period — aborting");
                            self.pending_interrupt = Some(ControlDecision::Pause);
                            // ADR-021: Flush partial content to JSONL before pausing
                            self.session_core.flush_streaming_line(self.session.conversation.as_deref());
                            let _ = self.session_core.try_send_chunk(ChunkEvent::Stopped {
                                content: accumulated_content.clone(),
                            });
                            return Ok(build_stopped_response(
                                accumulated_content,
                                accumulated_reasoning_content,
                            ));
                        }
                        ControlDecision::Continue => {}
                    }
                }
            }
        }

        // Post-stream: Apply accumulated argument chunks to tool calls.
        // This handles the case where the OpenAI SSE stream ends without
        // a Finished event (common with OpenAI-compatible APIs like MiniMax).
        // When ToolCallStart already carries initial arguments from the same
        // SSE chunk (e.g. GLM, DeepSeek), do NOT append buffer content —
        // they are already complete.
        if tool_calls.is_some()
            && !tool_call_args_buffer.is_empty()
            && let Some(ref mut tcs) = tool_calls
        {
            for (i, tc) in tcs.iter_mut().enumerate() {
                if let Some(args) = tool_call_args_buffer.get(&(i as u64))
                    && (tc.function.arguments.is_empty() || tc.function.arguments == "{}")
                {
                    // Validate JSON before applying — stream interruption can
                    // leave incomplete arguments that would fail at tool execution.
                    //
                    // Route through `arguments::resolve` instead of
                    // `serde_json::from_str(...).is_ok()` so we recover
                    // valid JSON wrapped in a natural-language prefix
                    // (DeepSeek 2026-08-24 incident) and preserve the
                    // original content as a hint in the marker message.
                    match crate::tools::arguments::resolve(args) {
                        crate::tools::arguments::ResolvedArgs::Valid(value) => {
                            if let Ok(s) = serde_json::to_string(&value) {
                                tracing::info!(
                                    tool_name = %tc.function.name,
                                    index = i,
                                    accumulated_len = s.len(),
                                    "Applying accumulated arguments to tool call"
                                );
                                tc.function.arguments = s;
                            }
                        }
                        crate::tools::arguments::ResolvedArgs::Recovered { value, .. } => {
                            if let Ok(s) = serde_json::to_string(&value) {
                                tracing::info!(
                                    tool_name = %tc.function.name,
                                    index = i,
                                    accumulated_len = s.len(),
                                    "Applying accumulated arguments to tool call"
                                );
                                tc.function.arguments = s;
                            }
                            tracing::warn!(
                                tool_name = %tc.function.name,
                                index = i,
                                raw_len = args.len(),
                                "Recovered tool call arguments from natural-language-wrapped stream"
                            );
                        }
                        crate::tools::arguments::ResolvedArgs::Invalid { raw, reason } => {
                            tracing::error!(
                                tool_name = %tc.function.name,
                                index = i,
                                raw_len = raw.len(),
                                reason = %reason,
                                raw_preview = %raw.preview(200),
                                "Accumulated tool call arguments are not valid JSON and could not be recovered"
                            );
                            tc.function.arguments = crate::tools::arguments::make_incomplete_marker(
                                &tc.function.name,
                                &raw,
                            );
                        }
                    }
                }
                // If arguments already non-empty (GLM/DeepSeek same-chunk pattern),
                // they are already complete — do not append buffer content.
                // DeepSeek sends duplicate complete arguments in subsequent chunks,
                // appending would produce invalid JSON like {"path": "."}{"path": "."}
            }
        }

        // ── Empty-response retry (single attempt) ──
        //
        // A stream can end "cleanly" — a `Finished` event or a plain EOF —
        // with zero content AND zero tool calls. The upstream loop would
        // treat that as the model's final answer and silently end the turn.
        // Observed on MiniMax-M3.1-Flash-Preview (2026-10-07): a mid-task
        // run ended with 0 chars while the API still billed 128 output
        // tokens, and the user got no reply until manually re-prompting.
        // That is not an intended completion — replay the same request once
        // so a transient upstream truncation recovers transparently.
        //
        // `length` is excluded on purpose: that signature (output budget
        // burned on reasoning) feeds `handle_output_budget_exhausted`,
        // which must reach the model with a nudge instead of re-sending the
        // identical request. Gated on `retry_on_overflow` (i.e. the
        // top-level call) so the compaction retry, which passes `None`,
        // cannot re-enter this branch — the retry is single-shot by
        // construction.
        if accumulated_content.is_empty()
            && tool_calls.is_none()
            && finish_reason.as_deref() != Some("length")
            && retry_on_overflow
        {
            let first_completion_tokens = usage
                .as_ref()
                .map(|u| u.completion_tokens)
                .unwrap_or(0);
            tracing::warn!(
                finish_reason = ?finish_reason,
                completion_tokens = first_completion_tokens,
                "Empty response (no content, no tool calls) — retrying the request once"
            );

            let retry_response = self.call_llm_streaming_no_retry(chat_request).await?;
            if retry_response.finish_reason.as_deref() == Some("stopped") {
                // The user stopped (or debug-paused) during the retry — the
                // stopped response already flushed partial content and the
                // UI got its `Stopped` chunk. Nothing more to do.
                tracing::info!("Empty-response retry interrupted by stop/pause");
            } else if retry_response.content.is_empty() && retry_response.tool_calls.is_none() {
                tracing::error!(
                    finish_reason = ?retry_response.finish_reason,
                    "Empty response persisted after retry — ending the turn with \
                     an empty reply and a user-visible error"
                );
                // User-visible fallback: a normal error chunk (same mechanism
                // as `handle_output_budget_exhausted`), so a silently empty
                // turn surfaces in the UI instead of a bare Idle.
                let _ = self.session_core.try_send_chunk(ChunkEvent::Error {
                    user_message: "The model returned an empty response twice in a row \
(no text, no tool calls). The turn was ended without a reply — please retry; \
if this keeps happening the upstream provider may be truncating responses."
                        .to_string(),
                    detail: format!(
                        "empty_response_after_retry finish_reason={:?}",
                        retry_response.finish_reason
                    ),
                    error_type: "EmptyResponseAfterRetry".to_string(),
                    message_id: format!("empty-response-{}", Utc::now().timestamp_millis()),
                });
            } else {
                tracing::info!(
                    content_len = retry_response.content.len(),
                    has_tool_calls = retry_response.tool_calls.is_some(),
                    "Empty-response retry succeeded"
                );
            }
            return Ok(retry_response);
        }

        self.last_call_perf = CallPerf::measure(call_t0, first_output_at, usage.as_ref());

        Ok(ChatResponse {
            content: accumulated_content,
            reasoning_content: if accumulated_reasoning_content.is_empty() {
                None
            } else {
                Some(accumulated_reasoning_content)
            },
            tool_calls,
            usage,
            reasoning_started_at,
            reasoning_finished_at,
            finish_reason,
        })
    }
}

/// Wall-clock measurement of one streaming LLM call.
///
/// Lives on [`AgentLoop::last_call_perf`] rather than on `ChatResponse`:
/// the provider structs are shared with the debug path and every literal
/// site would have to learn a new field, while the consumer
/// (`process_llm_response_usage`) is one call away from the producer.
///
/// The slot is a **one-shot handshake**: `call_llm_streaming_inner` clears
/// it on entry and writes it on completion, and the consumer `take()`s it.
/// That pairing is what guarantees one sample per call — a folded mean that
/// counted the same call twice would silently inflate the session figure.
///
/// `tps` is `None` when the call cannot be measured honestly — a Provider
/// that reported no completion count, or a generation window too short to
/// divide by (see [`MIN_MEASURABLE_WINDOW_MS`]). The frontend renders "—"
/// instead of 0, matching the token counters' "宁可 miss 也不估计" rule.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CallPerf {
    /// Time from "call started" to the first output event.
    pub ttft_ms: f64,
    /// Output tokens per second for this call.
    pub tps: Option<f64>,
}

/// Shortest generation window (first output token → stream end) that can
/// produce a credible tokens/second figure.
///
/// Under this, the number is dominated by clock granularity and by how the
/// provider batched its chunks, not by generation speed. A provider that
/// delivers the entire completion in one chunk would otherwise report
/// `completion_tokens * 1000` t/s — 5 tokens becoming a permanent 5000 t/s
/// outlier in the session mean. Speed is then reported as unmeasurable.
/// TTFT is unaffected: a single-chunk response still has a real first-token
/// time, so responsiveness keeps being sampled.
const MIN_MEASURABLE_WINDOW_MS: u64 = 50;

impl CallPerf {
    /// Build the measurement, or `None` when the call produced nothing
    /// measurable (no output event → no TTFT and no generation window).
    pub(crate) fn measure(
        started: std::time::Instant,
        first_output: Option<std::time::Instant>,
        usage: Option<&acowork_core::providers::traits::UsageInfo>,
    ) -> Option<Self> {
        let first_output = first_output?;
        Self::from_window(
            first_output.saturating_duration_since(started),
            first_output.elapsed(),
            usage.map(|u| u.completion_tokens).unwrap_or(0),
        )
    }

    /// Pure half of [`Self::measure`]: two wall-clock windows plus the
    /// Provider's completion count → the per-call figures. Split out so the
    /// credible-window rule is testable without sleeping on a real clock.
    fn from_window(
        ttft: std::time::Duration,
        generation: std::time::Duration,
        completion_tokens: u64,
    ) -> Option<Self> {
        let ttft_ms = ttft.as_secs_f64() * 1000.0;
        let window_ms = generation.as_millis().min(u64::MAX as u128) as u64;
        let credible_window = window_ms >= MIN_MEASURABLE_WINDOW_MS;
        let tps = (credible_window && completion_tokens > 0)
            .then(|| completion_tokens as f64 / (window_ms as f64 / 1000.0));
        tracing::debug!(
            ttft_ms = (ttft_ms * 1000.0).round() / 1000.0,
            window_ms,
            tps = ?tps,
            "LLM call performance measured"
        );
        Some(Self { ttft_ms, tps })
    }
}

/// Build a partial [`ChatResponse`] for stream stop.
///
/// Returns the accumulated content so far and discards any partial tool calls.
fn build_stopped_response(content: String, reasoning_content: String) -> ChatResponse {
    ChatResponse {
        content,
        reasoning_content: if reasoning_content.is_empty() {
            None
        } else {
            Some(reasoning_content)
        },
        tool_calls: None,
        usage: None,
        reasoning_started_at: None,
        reasoning_finished_at: None,
        finish_reason: Some("stopped".to_string()),
    }
}

/// Build a structured error marker for truncated/incomplete tool call arguments.
///
/// **Moved to [`crate::tools::arguments::make_incomplete_marker`]**.
#[deprecated(
    since = "0.1.0",
    note = "use crate::tools::arguments::make_incomplete_marker instead — the streaming assembler (loop_llm.rs) and the tool executor (loop_tools.rs) now share the same marker factory"
)]
#[allow(dead_code)]
pub(crate) fn make_incomplete_marker(tool_name: &str, raw_len: usize) -> String {
    // Legacy signature kept for backwards compatibility.  New code should
    // call `crate::tools::arguments::make_incomplete_marker(name, hint)` directly.
    crate::tools::arguments::make_incomplete_marker_with_len(tool_name, raw_len)
}

#[cfg(test)]
mod perf_tests {
    use super::{CallPerf, MIN_MEASURABLE_WINDOW_MS};
    use std::time::Duration;

    fn window(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    /// A credible window divides normally.
    #[test]
    fn measures_speed_over_the_generation_window_only() {
        // 900 tokens generated in 3s after the first token; the 500ms
        // pre-token wait must NOT count as generation time.
        let perf = CallPerf::from_window(window(500), window(3_000), 900).unwrap();
        assert!((perf.ttft_ms - 500.0).abs() < 1e-6);
        assert!((perf.tps.unwrap() - 300.0).abs() < 1e-6);
    }

    /// Regression: a response delivered in (near) one chunk used to hit a
    /// 1ms floor and report `completion_tokens * 1000` t/s — 5 tokens became
    /// a permanent 5000 t/s outlier in the session mean.
    #[test]
    fn sub_credible_window_reports_speed_unmeasurable() {
        let perf = CallPerf::from_window(window(420), window(MIN_MEASURABLE_WINDOW_MS - 1), 5)
            .unwrap();
        assert_eq!(perf.tps, None, "window below the credibility floor");
        // TTFT is still a real measurement — responsiveness keeps sampling.
        assert!((perf.ttft_ms - 420.0).abs() < 1e-6);
    }

    #[test]
    fn window_at_the_floor_is_measured() {
        let perf =
            CallPerf::from_window(window(100), window(MIN_MEASURABLE_WINDOW_MS), 10).unwrap();
        assert!((perf.tps.unwrap() - 200.0).abs() < 1e-6);
    }

    /// A Provider that reports no completion count still yields TTFT.
    #[test]
    fn zero_completion_tokens_yields_ttft_without_speed() {
        let perf = CallPerf::from_window(window(300), window(1_000), 0).unwrap();
        assert_eq!(perf.tps, None);
        assert!((perf.ttft_ms - 300.0).abs() < 1e-6);
    }

    /// No output event at all (cancelled before the first token) — there is
    /// nothing to measure, and folding a 0 would claim the model stalled.
    #[test]
    fn no_output_event_is_not_a_measurement() {
        let started = std::time::Instant::now();
        assert!(CallPerf::measure(started, None, None).is_none());
    }
}
