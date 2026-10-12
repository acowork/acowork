//! Session lifecycle management for AgentLoop.
//!
//! Extracted from loop_.rs as part of ADR-014 Phase 5.
//!
//! Contains:
//! - Session status transitions
//! - Session close with distillation
//! - Session metadata updates (title, workspace_id)
//! - Think block utilities (extract, strip, build metadata)

use acowork_core::providers::traits::{ChatMessage, ChatResponse, MessageRole};
use std::sync::atomic::Ordering;

use crate::agent::context::{build_context_usage_from_persisted, patch_agent_totals};
use crate::agent::session_state::SessionStatus;
use crate::error::Result;

// ── Output-budget exhaustion (think-budget runaway) ─────────────────────
//
// A thinking model can spend its entire completion budget on reasoning and
// return `finish_reason="length"` with no content and no tool calls. Observed
// with MiniMax-M3 on 2026-09-16 (session 20260916_145211_eeb117): all 32768
// completion tokens were reasoning tokens, `content` was empty, and the turn
// silently ended in Idle with no reply at all. These helpers detect that
// signature and bound the recovery so it cannot loop forever.

/// `name` marker on the internal nudge appended to history. It identifies
/// nudge messages when counting retries, and keeps the nudge distinguishable
/// from a real user turn.
pub(crate) const INTERNAL_NUDGE_NAME: &str = "internal_nudge";

/// Maximum nudges injected per user turn before the failure is surfaced.
pub(crate) const MAX_OUTPUT_BUDGET_NUDGES: usize = 2;

/// Instruction injected into history after an output-budget exhaustion.
const OUTPUT_BUDGET_NUDGE: &str = "Your previous reply was cut off by the output token limit \
and produced no content and no tool calls. Respond now with a direct answer or a tool call — \
do not keep reasoning at length. Reply in the same language as the user.";

/// Signature of an output-budget exhaustion: the model was cut off by the
/// token limit and produced nothing usable.
pub(crate) fn is_output_budget_exhausted(response: &ChatResponse) -> bool {
    response.finish_reason.as_deref() == Some("length")
        && response.content.trim().is_empty()
        && response.tool_calls.is_none()
}

/// Heuristic for "this text response looks cut off mid-sentence".
///
/// WARN-only diagnostic — never changes control flow. Two signatures, both
/// observed when MiniMax-M3.1-Flash-Preview's Anthropic-compatible endpoint
/// ended a stream early (2026-10-07: a 43-char reply that stopped right
/// after a lead-in colon, and the run silently went Idle mid-task):
///
/// - Short text (fewer than `SHORT_RESPONSE_MAX_CHARS`) that still ends on
///   a lead-in colon (`:` / `：`) — the model was introducing something
///   that never arrived.
/// - An unclosed ``` code fence (odd count) in short text — a code block
///   was opened and the stream ended before it was closed.
///
/// Deliberately conservative: bounded to short responses so long, complete
/// replies that happen to end on a colon are not flagged.
fn is_suspiciously_truncated_text(content: &str) -> bool {
    const SHORT_RESPONSE_MAX_CHARS: usize = 250;
    if content.chars().count() >= SHORT_RESPONSE_MAX_CHARS {
        return false;
    }
    let trimmed = content.trim_end();
    let ends_with_lead_in = trimmed.ends_with(':') || trimmed.ends_with('：');
    let unclosed_code_fence = content.matches("```").count() % 2 == 1;
    ends_with_lead_in || unclosed_code_fence
}

/// Count internal nudges injected since the last genuine user turn.
///
/// Stateless on purpose: counting from history avoids a per-turn counter that
/// would need resetting at every turn boundary.
///
/// A **genuine user turn** is a `User` message with no `name`. Every internal
/// user-role injection carries a `name` (see `inject_inbound_into_history`:
/// system notifications use `"system"`, compaction summaries
/// `"compaction_summary"`, the nudge `"internal_nudge"`), so those do NOT
/// reset the budget. This is load-bearing: the token-budget warning is injected
/// as `User { name: "system" }` between iterations, and a thinking model that
/// burns its whole output budget also blows the token budget — so treating the
/// warning as a turn boundary would reset the nudge budget on every iteration
/// and defeat the bound.
fn count_output_budget_nudges(history: &[ChatMessage]) -> usize {
    history
        .iter()
        .rev()
        .take_while(|m| !(m.role == MessageRole::User && m.name.is_none()))
        .filter(|m| m.role == MessageRole::User && m.name.as_deref() == Some(INTERNAL_NUDGE_NAME))
        .count()
}

impl super::loop_::AgentLoop {
    // ── Session lifecycle methods ──────────────────────────────────────────

    /// Transition session status and emit SessionStateChanged event if the status changed.
    ///
    /// ADR-014 helper: ensures every status transition is paired with an event emission.
    /// Returns true if the status actually changed (and event was emitted).
    pub(crate) fn transition_status(&mut self, new_status: SessionStatus) -> bool {
        if self.session.set_status(new_status) {
            self.emit_session_state();
            true
        } else {
            false
        }
    }

    /// Emit a SessionStateChanged event with the current session state.
    ///
    /// Called on status transitions (via [`transition_status`]) and at the
    /// end of each iteration checkpoint in [`execute_single_iteration`], so
    /// the frontend always sees the latest status, ratio and todos.
    ///
    /// ADR-039: persisted fields (model, provider, workspace_id,
    /// reasoning_effort, temperature) are no longer emitted in this event —
    /// they are broadcast through the `session_meta` MQTT channel which
    /// carries the authoritative values from `data/meta/{session_id}.json`.
    pub(crate) fn emit_session_state(&mut self) {
        let status = self.session.status.clone();
        // Build context_usage from persisted session tokens (if available)
        // so the frontend can show token counts immediately on session state push,
        // without waiting for the first LLM call or a MQTT context_usage event.
        let context_usage = self.session.conversation.as_ref().and_then(|conv| {
            let persisted = conv.tokens()?;
            let model_name = self.session.model().unwrap_or("unknown");
            let caps = self.core.get_model_capabilities(model_name)?;
            let max_output = self.core.max_output_tokens_limit_for_model(model_name);
            // Prefer the live in-memory history token count over the
            // persisted snapshot. `restore_anchor` has already seeded
            // `history` from `persisted.last_input`, so at cold start the two
            // agree; the live value wins afterwards because messages appended
            // after resume are only counted in `history`. Empty/new
            // histories keep the persisted fallback so the pre-first-LLM
            // snapshot still shows something sensible.
            //
            // Both figures are whole-prompt API counts (history + system +
            // tool schemas), never a messages-only estimate — mixing the two
            // 口径 is what made a resumed session report a bogus `~109K`
            // against a persisted `last_input` of `167098`.
            let live_tokens = self.session.history.token_count();
            let use_live = live_tokens > 0;
            let input_tokens = if use_live {
                live_tokens
            } else {
                persisted.last_input
            };
            let output_tokens = if use_live { 0 } else { persisted.last_output };
            let mut ctx = build_context_usage_from_persisted(
                &caps,
                input_tokens,
                output_tokens,
                max_output,
                // ADR-074: pass the session-effective resolved cap (session
                // meta → agent_config → manifest → default → min(model
                // window)), so window display (96K) and the usable
                // denominator (96K − output reserve) agree with the
                // runtime trim/compaction thresholds.
                Some(
                    crate::agent::session_config::resolve_effective_context_window(
                        conv.context_window(),
                        self.core.context_window_override,
                        self.core.manifest_context_window,
                        Some(&caps),
                    ),
                ),
                Some(&persisted),
                // Per-session lifetime LLM-call count so the retained
                // session_state snapshot carries the same "Iterations" as
                // the live ContextUsage push — same reason for the
                // persisted speed / TTFT averages below.
                conv.llm_call_counter(),
                conv.perf().as_ref(),
            );
            // ADR-067: this path builds from persisted tokens and has no
            // `ContextBuilder`, so it cannot recompute sections. Merge the
            // most recent sections cached by `process_llm_response_usage`
            // so the retained session_state snapshot doesn't clobber the
            // input-box popover breakdown with an empty `sections`.
            if let Some(sections) = conv.last_context_usage_sections() {
                ctx.sections = Some(sections);
            }
            // ADR-028 / ADR-066: snapshot the agent-scoped cumulative
            // counters so the `session_state` push carries the same
            // `agent_total_*` figures as the live LLM-call
            // `ContextUsage` push. Without this patch the frontend's
            // `mergeContextUsage` wipes the four fields back to `None`
            // on every `session_state` re-emit, and the Agent Status
            // panel's "累计输入 Token" row flickers between the live
            // value and the stale fallback (session-list fetch cache).
            patch_agent_totals(&mut ctx, self.core.agent_token_totals());
            let json = serde_json::to_string(&ctx).ok();
            // LOG-001: fires on every emit_session_state (multiple per turn)
            // and only confirms the JSON was built — the value itself is
            // carried in the MQTT state snapshot. Demoted to DEBUG.
            tracing::debug!(
                session_id = %self.session_core.session_id.as_deref().unwrap_or("?"),
                model = %model_name,
                live = use_live,
                live_tokens,
                last_input = persisted.last_input,
                last_output = persisted.last_output,
                has_json = json.is_some(),
                "emit_session_state: built context_usage"
            );
            json
        });
        if context_usage.is_none() {
            let has_conv = self.session.conversation.is_some();
            let has_tokens = self
                .session
                .conversation
                .as_ref()
                .and_then(|c| c.tokens())
                .is_some();
            let model_for_caps = self.session.model().unwrap_or("unknown");
            let has_caps = self.core.get_model_capabilities(model_for_caps).is_some();
            tracing::warn!(
                session_id = %self.session_core.session_id.as_deref().unwrap_or("?"),
                has_conv,
                has_tokens,
                model = %model_for_caps,
                has_caps,
                "emit_session_state: context_usage is None"
            );
        }

        // ADR-043: Update the runtime state cache on ConversationSession
        // and notify the state relay. The relay publishes a retained
        // SessionState snapshot to `sessions/{sid}/state`.
        // This replaces the old ChunkEvent::SessionStateChanged path.
        if let Some(ref conv) = self.session.conversation {
            let status = serde_json::to_string(&status).unwrap_or_else(|_| r#""idle""#.to_string());
            let ratio = self.session
                .model_ratio()
                .unwrap_or(crate::token::counter::DEFAULT_RATIO);
            let cu = context_usage.clone().unwrap_or_default();
            conv.update_runtime_state_cache(&status, ratio, &cu);
            conv.notify_state_change();
        }
        // Update watch channel for SessionHandle reads.
        // Use `send_modify` instead of `send` because `send` silently
        // fails (returns Err without updating) when there are no
        // receivers – e.g. after the SessionHandle is dropped but the
        // agent loop hasn't exited yet.
        if let Some(ref tx) = self.session_core.status_tx {
            tx.send_modify(|v| *v = status.clone());
        }
        // Update shared snapshot for Gateway pull API.
        // The snapshot Arc is shared between SessionState and SessionHandle;
        // writes here are immediately visible to snapshot_session_state().
        {
            let status_json =
                serde_json::to_string(&status).unwrap_or_else(|_| r#""idle""#.to_string());
            // Serialize the todo list to JSON for the snapshot. Skip the allocation
            // entirely when the list is empty (common case — most iterations have
            // no active todo list). Errors are logged so they can be distinguished
            // from "no todos" on the consumer side.
            let todos_json = if self.session.todos.is_empty() {
                None
            } else {
                let sid = self.session_core.session_id.clone().unwrap_or_default();
                match serde_json::to_string(&self.session.todos) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(
                            session_id = %sid,
                            error = %e,
                            "Failed to serialize todos for session state snapshot; emitting empty"
                        );
                        None
                    }
                }
            };
            if let Ok(mut guard) = self.session.snapshot.write() {
                guard.status = status_json;
                // ADR-039 (revised): mirror model + provider into the runtime
                // snapshot so SessionManager::current_model_name and
                // current_model_and_provider have sync access without
                // hitting the meta file. The authoritative value still lives
                // in data/meta/{session_id}.json; this mirror may briefly
                // lag by one emit cycle.
                guard.model = self.session.model().map(|s| s.to_string());
                guard.provider = self.session.provider().map(|s| s.to_string());
                guard.account_id = self.session.account_id().map(|s| s.to_string());
                guard.ratio = self.session.model_ratio();
                guard.todos_json = todos_json;
                // Only overwrite context_usage if we successfully computed a
                // new value.  Otherwise preserve the existing value set by
                // build_initial_session_state — emit_session_state may fail to
                // compute context_usage (e.g. model capabilities not yet available
                // for the session's model name), and unconditionally writing None
                // would wipe the initial snapshot populated at session creation.
                if context_usage.is_some() || guard.context_usage.is_none() {
                    guard.context_usage = context_usage;
                }
            }
        }
    }

    /// Get the current conversation session ID (S1.14)
    ///
    /// Returns the session ID of the active ConversationSession,
    /// or None if no session is active.
    pub fn current_session_id(&self) -> Option<&str> {
        self.session.conversation.as_ref().map(|c| c.session_id())
    }

    /// Lazy-persist any async-generated session title to the conversation
    /// JSONL metadata and index.json.
    ///
    /// The title generation task (spawned in [`super::AgentLoop::run_inner`])
    /// writes the LLM-generated title to `session_core.title` asynchronously.
    /// This method is called at iteration checkpoints to pick up the title
    /// at the earliest opportunity and persist it to disk.
    ///
    /// Idempotent: `ConversationSession::update_title_force` is a no-op if
    /// the title hasn't changed.
    pub(crate) fn flush_pending_title(&mut self) {
        if let Some(title) = self.session_core.title.read().unwrap().clone()
            && let Some(ref conversation) = self.session.conversation
        {
            conversation.update_title_force(&title);
        }
    }

    /// Close the conversation session and trigger session-level distillation.
    ///
    /// This method:
    /// 1. Spawns an async distillation task for the entire session
    /// 2. Closes the conversation writer
    ///
    /// Distillation is best-effort and non-blocking.
    pub async fn close_session_with_distillation(&mut self) -> Result<()> {
        self.close_session_inner().await
    }

    /// Inner implementation for closing the current session.
    ///
    /// Per [ADR-011]: uses `last_compaction_index()` to determine the tail
    /// distillation range. The `is_compacted` flag is used as a fast-path hint
    /// but is NOT sufficient alone — the assistant response from the same round
    /// that triggered compaction may land after the compaction marker, and must
    /// still be distilled.
    async fn close_session_inner(&mut self) -> Result<()> {
        if let Some(ref conversation) = self.session.conversation {
            let session_id = conversation.session_id().to_string();

            // ADR-051 P3 post-compaction maintenance tasks were removed at
            // session-close: semantic-layer promotion runs exclusively in the
            // EpisodicDistiller background step (ADR-068 single-producer
            // rule, gated by [memory.distiller].enabled).

            // Determine tail range: everything after the last compaction marker,
            // or full history (skipping leading system messages) if never compacted.
            let tail_start = self
                .session
                .history
                .last_compaction_index()
                .map(|idx| idx + 1) // Start after compaction marker
                .unwrap_or_else(|| {
                    // No compaction ever — skip leading system messages
                    self.session
                        .history
                        .messages()
                        .iter()
                        .take_while(|m| matches!(m.role, MessageRole::System))
                        .count()
                });

            let messages = self.session.history.messages();
            let tail_messages: Vec<ChatMessage> = messages[tail_start..].to_vec();

            // Tail distillation removed (post-2026-09-10): the spawned
            // LLM-call task existed solely to write a session-close
            // distillation summary to Grafeo via
            // `EpisodeDistiller::write_summary_to_provider`. Memory
            // persistence is now delegated to the agent itself — see
            // `prompts/system.md` for the prompt guidance asking the LLM
            // to call `memory_store` when a task is complete.
            //
            // We keep the `tail_start`/`tail_messages` computation above
            // for two reasons: (1) it documents the historical context
            // window boundary (after the last compaction), (2) future
            // agent-driven summarization may want to read this same
            // boundary through history accessors. The LLM call, fallback
            // chain, Grafeo write, and `DistillationFailed` user error
            // path are all gone with the distillation task.
            if !tail_messages.is_empty() {
                tracing::debug!(
                    session_id = %session_id,
                    tail_message_count = tail_messages.len(),
                    is_compacted = self.session.is_compacted,
                    "Session close: tail range computed but no LLM distillation spawned (memory persistence delegated to agent via memory_store)"
                );
            }

            // Close the conversation writer
            conversation.close().await?;
        }
        Ok(())
    }

    // ── Iteration result helpers ──────────────────────────────────────────

    /// Handle pure text response (no tool calls).
    ///
    /// Persists think block + assistant response to JSONL, appends to
    /// in-memory history, increments turn counter, and emits debug
    /// phase events. Returns `TextResponse(content)`.
    pub(crate) async fn handle_text_response(
        &mut self,
        response: &ChatResponse,
        iteration: u32,
    ) -> super::loop_::IterationResult {
        let content = response.content.clone();

        // Diagnostic: log detailed response summary for post-mortem analysis
        let reasoning_len = response
            .reasoning_content
            .as_ref()
            .map(|r| r.len())
            .unwrap_or(0);
        let reasoning_tokens = response
            .usage
            .as_ref()
            .map(|u| u.reasoning_tokens)
            .unwrap_or(0);
        let completion_tokens = response
            .usage
            .as_ref()
            .map(|u| u.completion_tokens)
            .unwrap_or(0);
        if content.is_empty() {
            tracing::warn!(
                iteration,
                finish_reason = ?response.finish_reason,
                reasoning_len,
                reasoning_tokens,
                completion_tokens,
                has_reasoning = response.reasoning_content.is_some(),
                "Empty text response — model may have exhausted token budget on reasoning"
            );
        } else {
            tracing::info!(
                iteration,
                content_len = content.len(),
                reasoning_len,
                reasoning_tokens,
                completion_tokens,
                finish_reason = ?response.finish_reason,
                "Agent returned text response"
            );

            // DIAG (2026-10-07): flag text that looks like a truncated
            // stream rather than a finished answer. MiniMax-M3.1-Flash-
            // Preview has been seen ending a response mid-sentence —
            // "Now dedupe `canClaim` against the new flag:" (43 chars,
            // nothing after the colon) — and the loop treated it as the
            // final answer, silently Idling a half-done task. WARN-only;
            // the heuristic never changes control flow.
            if is_suspiciously_truncated_text(&content) {
                let tail: String = {
                    let chars: Vec<char> = content.chars().collect();
                    let start = chars.len().saturating_sub(80);
                    chars[start..].iter().collect()
                };
                tracing::warn!(
                    iteration,
                    content_len = content.len(),
                    reasoning_len,
                    finish_reason = ?response.finish_reason,
                    tail = %tail,
                    "Text response looks truncated — short text ending mid-sentence \
                     (possibly a cut-off stream); the turn ends here, verify the \
                     provider behaved"
                );
            }
        }

        // ADR-022: Persist response to JSONL.
        //
        // Two paths:
        // 1. Streaming flush occurred (role transitions already wrote content
        //    to JSONL during the stream). Just flush any remaining content in
        //    the last streaming line — no legacy persistence needed.
        // 2. No streaming flush occurred (non-streaming provider, or all
        //    content arrived in the Finished event). Use the legacy path:
        //    persist_think_to_conversation + strip_think_block.
        let streamed = self.finalize_streaming_line(response);

        if streamed {
            tracing::debug!(
                iteration,
                "ADR-022: streaming flush path — skipping legacy persistence"
            );
        } else if let Some(ref conversation) = self.session.conversation {
            // Path 2: Legacy persistence for non-streaming responses.
            // The think block was already persisted by `finalize_streaming_line`;
            // only the assistant text remains.
            let assistant_text = strip_think_block(&content);
            if !assistant_text.is_empty() {
                conversation.append_message("assistant", &assistant_text, None);
            }
        }

        // Persist the final assistant turn to in-memory history.
        //
        // IMPORTANT: We DELIBERATELY do not store `reasoning_content` here,
        // even though it is available on `response`. This is by design and
        // aligns with DeepSeek's thinking mode contract:
        //
        //   - If the user turn triggered NO tool calls, DeepSeek says the
        //     intermediate `reasoning_content` "无需参与上下文拼接,在后续
        //     轮次中将其传入 API 会被忽略" — passing it is harmless but
        //     bloats context for nothing, so we drop it.
        //   - Tool-call rounds are handled in `loop_tools.rs::prepare_tool_calls`
        //     where we DO persist `reasoning_content` because DeepSeek REQUIRES
        //     round-tripping it on tool-call turns.
        //
        // Reference: https://api-docs.deepseek.com/zh-cn/guides/thinking_mode
        //
        // Notes:
        //  - This drops reasoning ONLY for the text-final path. If the assistant
        //    happens to have called tools earlier in the same user turn, the
        //    earlier tool-turn `reasoning_content` is already in history via
        //    `prepare_tool_calls` — that is the DeepSeek-required signal.
        //  - Anthropic currently parses thinking into `reasoning_content` but
        //    never reads it back from history (Anthropic echoes thinking blocks,
        //    not a single string field); the field is effectively inert for
        //    Anthropic and harmless.
        //  - For OpenAI's Chat Completions (o-series), `response.reasoning_content`
        //    is always None, so dropping it is a no-op.
        self.session.history.append(ChatMessage {
            ..ChatMessage::assistant(content.clone())
        });

        // Note: the primary "Agent returned text response" log is now
        // emitted above with full diagnostic fields. The legacy log here
        // is kept for backward compatibility with existing log parsers.
        if content.is_empty() {
            tracing::info!(iteration, "Agent returned EMPTY text response");
        }

        // Debug: enter AppendHistory phase and push step event
        self.core
            .debug_observer
            .on_phase_enter(crate::debug::protocol::DebugPhase::AppendHistory)
            .await;
        self.core.debug_observer.on_phase_step(
            crate::debug::protocol::DebugPhase::Idle,
            None,
            Some(serde_json::json!({"content": content})),
        );
        self.core.debug_observer.on_phase_step_done().await;

        super::loop_::IterationResult::TextResponse(content)
    }

    /// ADR-022: close out the current streaming line at turn end.
    ///
    /// Two cases:
    /// 1. The provider already flushed on role transitions
    ///    (`streaming_flush_count > 0`): flush the final line (e.g. the last
    ///    assistant segment) to JSONL + emit `RecordComplete`.
    /// 2. Nothing was flushed: persist the response's think block via the
    ///    legacy path and drop the line.
    ///
    /// Returns `true` when case 1 applied, i.e. the caller must **skip** its
    /// own legacy think/assistant persistence to avoid a duplicate entry.
    ///
    /// This MUST be called on every terminal (return-to-Idle) path. Leaving the
    /// line open means a later role transition flushes it as a *late*
    /// `record_complete`, surfacing stale content as a bubble on the next turn.
    fn finalize_streaming_line(&self, response: &ChatResponse) -> bool {
        let streamed = self
            .session_core
            .streaming_flush_count
            .load(Ordering::Relaxed)
            > 0;

        if streamed {
            self.session_core
                .flush_streaming_line(self.session.conversation.as_deref());
        } else if let Some(ref conversation) = self.session.conversation {
            persist_think_to_conversation(conversation, response);
            self.session_core.remove_streaming_line();
        }

        streamed
    }

    /// Handle a response whose output budget was exhausted (see
    /// [`is_output_budget_exhausted`]).
    ///
    /// Injects an internal nudge and asks the loop for another iteration while
    /// the per-turn nudge budget lasts. The nudge is appended to the LLM
    /// history AND persisted to JSONL (marked `metadata.internal`), but it is
    /// deliberately NOT broadcast over MQTT (it never goes through
    /// `ChunkEvent::RecordComplete`). Readers treat it as an internal artifact:
    /// the Desktop keeps it out of the transcript, and the session restorer
    /// skips it (an interrupted turn is never resumed, so replaying the nudge
    /// would leak a stale instruction into later turns).
    ///
    /// Once the budget is spent the turn ends and a user-visible error is
    /// emitted, so the user is never left with a silent empty reply.
    pub(crate) async fn handle_output_budget_exhausted(
        &mut self,
        response: &ChatResponse,
        iteration: u32,
    ) -> super::loop_::IterationResult {
        let reasoning_tokens = response
            .usage
            .as_ref()
            .map(|u| u.reasoning_tokens)
            .unwrap_or(0);
        let completion_tokens = response
            .usage
            .as_ref()
            .map(|u| u.completion_tokens)
            .unwrap_or(0);
        let nudges_used = count_output_budget_nudges(self.session.history.messages());

        if nudges_used < MAX_OUTPUT_BUDGET_NUDGES {
            tracing::warn!(
                iteration,
                attempt = nudges_used + 1,
                max_attempts = MAX_OUTPUT_BUDGET_NUDGES,
                finish_reason = ?response.finish_reason,
                reasoning_tokens,
                completion_tokens,
                "Output budget exhausted on reasoning with no content — nudging model to wrap up"
            );

            // LLM context: a `User` turn is what makes the model respond again.
            self.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: OUTPUT_BUDGET_NUDGE.to_string(),
                name: Some(INTERNAL_NUDGE_NAME.to_string()),
                ..Default::default()
            });

            // Persistence: JSONL only. `append_internal_message` goes straight
            // to the conversation writer; it does NOT emit
            // `ChunkEvent::RecordComplete` (that happens only in
            // `SessionCore::flush_streaming_line`), so the live UI never sees
            // this entry. The `metadata.internal` marker lets readers (Desktop
            // transcript, session restorer) recognize it.
            if let Some(conversation) = self.session.conversation.as_deref() {
                conversation.append_internal_message("user", OUTPUT_BUDGET_NUDGE);
            }

            return super::loop_::IterationResult::Nudged;
        }

        tracing::error!(
            iteration,
            finish_reason = ?response.finish_reason,
            reasoning_tokens,
            completion_tokens,
            "Output budget exhausted and nudge budget spent — surfacing failure to user"
        );

        // ADR-022: close the streaming line before returning to Idle. The
        // truncated response's reasoning was streamed as a live `thought` line;
        // without this it would dangle and surface as a late `record_complete`
        // on the next turn's first role transition.
        self.finalize_streaming_line(response);

        // User-visible fallback: a normal error chunk (not a JSONL note), so it
        // reaches the live UI and renders like every other runtime error.
        let _ = self
            .session_core
            .try_send_chunk(super::loop_::ChunkEvent::Error {
                user_message: "Model output hit the token limit without producing usable content \
(reasoning consumed the entire output budget). Retry, split the task, or lower the reasoning effort."
                    .to_string(),
                detail: format!(
                    "finish_reason=length reasoning_tokens={} completion_tokens={}",
                    reasoning_tokens, completion_tokens
                ),
                error_type: "OutputBudgetExhausted".to_string(),
                message_id: format!("output-budget-exhausted-{}", iteration),
            });

        // Close the turn in history the same way `handle_text_response` does
        // for an empty response, so the next user turn is not preceded by a
        // dangling internal `user` message (two consecutive user turns).
        self.session.history.append(ChatMessage::assistant(String::new()));

        super::loop_::IterationResult::TextResponse(String::new())
    }
}

// ── Think block utilities (free functions) ──────────────────────────────
// Note: These are public so loop_llm.rs and loop_.rs can use them via
// `crate::agent::loop_session::{extract_think_block, strip_think_block, build_think_metadata}`.

/// Extract content inside `<think>...</think>` tags if present.
pub fn extract_think_block(content: &str) -> Option<String> {
    let start_tag = "<think>";
    let end_tag = "</think>";
    let start = content.find(start_tag)?;
    let end = content.find(end_tag)?;
    if end <= start + start_tag.len() {
        return None;
    }
    Some(content[start + start_tag.len()..end].trim().to_string())
}

/// Remove think blocks from content, returning the remaining text.
///
/// Supports two tag formats (matching `ThinkTagParser` in the provider layer):
///   - Standard: `<think>...</think>`
///   - MiniMax:  `<!think>...willReturn`
///
/// All occurrences of both formats are stripped. If no think blocks are found,
/// the original content is returned unchanged.
pub fn strip_think_block(content: &str) -> String {
    const PAIRS: &[(&str, &str)] = &[("<think>", "</think>"), ("<!think>", "willReturn")];

    let mut result = content.to_string();
    for &(start_tag, end_tag) in PAIRS {
        while let Some(start) = result.find(start_tag) {
            if let Some(end) = result[start + start_tag.len()..].find(end_tag) {
                let end_abs = start + start_tag.len() + end + end_tag.len();
                let before = &result[..start];
                let after = &result[end_abs..];
                result = format!("{}{}", before, after);
            } else {
                // No closing tag — strip everything from the opening tag onward.
                result = result[..start].to_string();
                break;
            }
        }
    }
    result.trim().to_string()
}

/// Build think message metadata with timing info from ChatResponse.
pub fn build_think_metadata(response: &ChatResponse) -> Option<serde_json::Value> {
    if response.reasoning_started_at.is_some() || response.reasoning_finished_at.is_some() {
        Some(serde_json::json!({
            "startTime": response.reasoning_started_at,
            "endTime": response.reasoning_finished_at,
        }))
    } else {
        None
    }
}

/// Persist think block to conversation JSONL (if present).
///
/// Shared by text response path and tool calls path — D2 deduplication.
/// DeepSeek `reasoning_content` (separate field) takes priority over
/// `<think />` tags embedded in `content`.
pub fn persist_think_to_conversation(
    conversation: &crate::conversation::ConversationSession,
    response: &ChatResponse,
) {
    let think_meta = build_think_metadata(response);
    if let Some(ref reasoning) = response.reasoning_content {
        // Trim-guard: a whitespace-only reasoning (streamed blank deltas)
        // must not persist as a thought record — it renders as an empty
        // ThinkBlock in the chat UI.
        if !reasoning.trim().is_empty() {
            conversation.append_message("thought", reasoning, think_meta);
        }
    } else if let Some(think_content) = extract_think_block(&response.content) {
        // `extract_think_block` trims, so an empty think-tag pair surfaces
        // as `Some("")` — must not persist a blank thought record either.
        if !think_content.is_empty() {
            conversation.append_message("thought", &think_content, think_meta);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_think_no_tags() {
        assert_eq!(strip_think_block("hello world"), "hello world");
    }

    #[test]
    fn strip_think_standard_single() {
        assert_eq!(
            strip_think_block("<think>reasoning</think>answer"),
            "answer"
        );
    }

    #[test]
    fn strip_think_standard_with_surrounding() {
        assert_eq!(
            strip_think_block("before<think>inner</think>after"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_think_minimax_single() {
        assert_eq!(
            strip_think_block("<!think>reasoningwillReturnanswer"),
            "answer"
        );
    }

    #[test]
    fn strip_think_minimax_with_surrounding() {
        assert_eq!(
            strip_think_block("before<!think>innerwillReturnafter"),
            "beforeafter"
        );
    }

    #[test]
    fn strip_think_multiple_blocks() {
        assert_eq!(
            strip_think_block("A<think>T1</think>B<think>T2</think>C"),
            "ABC"
        );
    }

    #[test]
    fn strip_think_mixed_formats() {
        assert_eq!(
            strip_think_block("A<think>T1</think>B<!think>T2willReturnC"),
            "ABC"
        );
    }

    #[test]
    fn strip_think_unclosed_standard() {
        assert_eq!(
            strip_think_block("answer<think>unclosed reasoning"),
            "answer"
        );
    }

    #[test]
    fn strip_think_unclosed_minimax() {
        assert_eq!(
            strip_think_block("answer<!think>unclosed reasoning"),
            "answer"
        );
    }

    #[test]
    fn strip_think_empty_content() {
        assert_eq!(strip_think_block(""), "");
    }

    #[test]
    fn strip_think_only_think_block() {
        assert_eq!(strip_think_block("<think>just thinking</think>"), "");
    }

    // ── Output-budget exhaustion ────────────────────────────────────────

    /// The exact 2026-09-16 MiniMax-M3 signature: `finish_reason="length"`,
    /// all completion tokens are reasoning, and content is a single space.
    fn exhausted_response() -> ChatResponse {
        ChatResponse {
            content: " ".to_string(),
            reasoning_content: Some("long internal monologue".to_string()),
            finish_reason: Some("length".to_string()),
            usage: Some(acowork_core::providers::traits::UsageInfo {
                prompt_tokens: 67_712,
                completion_tokens: 32_768,
                total_tokens: 100_480,
                reasoning_tokens: 32_768,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn budget_exhausted_detects_truncated_whitespace_only() {
        assert!(is_output_budget_exhausted(&exhausted_response()));
    }

    #[test]
    fn budget_exhausted_false_when_truncated_with_content() {
        // Truncated but there IS an answer to continue from — different path.
        let mut r = exhausted_response();
        r.content = "partial answer".to_string();
        assert!(!is_output_budget_exhausted(&r));
    }

    #[test]
    fn budget_exhausted_false_for_normal_stop() {
        let mut r = exhausted_response();
        r.finish_reason = Some("stop".to_string());
        assert!(!is_output_budget_exhausted(&r));
    }

    #[test]
    fn budget_exhausted_false_when_tool_calls_present() {
        let mut r = exhausted_response();
        r.tool_calls = Some(vec![acowork_core::providers::traits::ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: acowork_core::providers::traits::FunctionCall {
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        assert!(!is_output_budget_exhausted(&r));
    }

    fn nudge() -> ChatMessage {
        ChatMessage {
            role: MessageRole::User,
            content: OUTPUT_BUDGET_NUDGE.to_string(),
            name: Some(INTERNAL_NUDGE_NAME.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn nudge_count_zero_without_nudges() {
        let history = vec![
            ChatMessage::user("do the thing"),
            ChatMessage::assistant("working"),
        ];
        assert_eq!(count_output_budget_nudges(&history), 0);
    }

    #[test]
    fn nudge_count_tallies_since_last_user_turn() {
        let history = vec![
            ChatMessage::user("first turn"),
            nudge(),
            ChatMessage::assistant("still working"),
            nudge(),
        ];
        assert_eq!(count_output_budget_nudges(&history), 2);
    }

    #[test]
    fn nudge_count_resets_at_new_user_turn() {
        // Two nudges in the previous turn must not consume the new turn's budget.
        let history = vec![
            ChatMessage::user("first turn"),
            nudge(),
            nudge(),
            // next turn starts here
            ChatMessage::user("second turn"),
            nudge(),
        ];
        assert_eq!(count_output_budget_nudges(&history), 1);
    }

    #[test]
    fn nudge_count_ignores_internal_user_injections() {
        // A thinking model that burns its whole output budget also blows the
        // token budget, so the Runtime injects `User { name: "system" }` budget
        // warnings between iterations. Those are NOT genuine turns — if they
        // reset the budget, the nudge bound is silently defeated.
        let internal = |name: &str| ChatMessage {
            role: MessageRole::User,
            content: "[System Warning] Daily token limit".to_string(),
            name: Some(name.to_string()),
            ..Default::default()
        };
        let history = vec![
            ChatMessage::user("real turn"),
            nudge(),
            internal("system"),
            nudge(),
            internal("system"),
        ];
        assert_eq!(count_output_budget_nudges(&history), 2);
    }

    #[test]
    fn nudge_budget_allows_two_then_stops() {
        let mut history = vec![ChatMessage::user("turn")];
        assert!(count_output_budget_nudges(&history) < MAX_OUTPUT_BUDGET_NUDGES);
        history.push(nudge());
        assert!(count_output_budget_nudges(&history) < MAX_OUTPUT_BUDGET_NUDGES);
        history.push(nudge());
        // Budget spent — the handler now surfaces the failure instead of nudging.
        assert!(count_output_budget_nudges(&history) >= MAX_OUTPUT_BUDGET_NUDGES);
    }

    // ── Truncated-text heuristic ────────────────────────────────────────

    #[test]
    fn truncated_text_flags_short_trailing_colon() {
        // The exact 2026-10-07 MiniMax-M3.1 shape: 43 chars ending on a
        // lead-in colon, nothing after it.
        assert!(is_suspiciously_truncated_text(
            "Now dedupe `canClaim` against the new flag:"
        ));
    }

    #[test]
    fn truncated_text_flags_chinese_trailing_colon() {
        assert!(is_suspiciously_truncated_text("接下来看渲染层："));
    }

    #[test]
    fn truncated_text_flags_unclosed_code_fence() {
        assert!(is_suspiciously_truncated_text(
            "改完了，先看代码：\n```ts\nconst a = 1;"
        ));
    }

    #[test]
    fn truncated_text_false_for_complete_sentence() {
        assert!(!is_suspiciously_truncated_text("任务已完成，测试全部通过。"));
        assert!(!is_suspiciously_truncated_text(
            "Fixed the render and ran vitest."
        ));
    }

    #[test]
    fn truncated_text_false_for_closed_code_fence() {
        assert!(!is_suspiciously_truncated_text(
            "示例：\n```ts\nconst a = 1;\n```\n已完成。"
        ));
    }

    #[test]
    fn truncated_text_false_for_long_reply() {
        // The heuristic is bounded to short responses: a long reply that
        // happens to end on a colon is not flagged, to keep noise low.
        let long = format!("{}收尾说明：", "步骤说明 ".repeat(80));
        assert!(!is_suspiciously_truncated_text(&long));
    }

    #[test]
    fn persist_think_skips_whitespace_only_reasoning() {
        use crate::conversation::{generate_session_id, ConversationSession, SessionConfig};
        use std::sync::atomic::AtomicUsize;
        use std::sync::Arc;

        let temp_dir = tempfile::TempDir::new().unwrap();
        let session_id = generate_session_id();
        let (conversation, _cfg_rx, _state_rx) = ConversationSession::new(
            temp_dir.path(),
            &session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();

        // Whitespace-only reasoning (signature-only Claude thinking blocks
        // reaching the accumulate path) must NOT persist a thought record.
        let blank = ChatResponse {
            content: "answer".to_string(),
            reasoning_content: Some("  \n ".to_string()),
            ..Default::default()
        };
        persist_think_to_conversation(&conversation, &blank);

        // Real reasoning must still persist.
        let real = ChatResponse {
            content: "answer".to_string(),
            reasoning_content: Some("weighing options".to_string()),
            ..Default::default()
        };
        persist_think_to_conversation(&conversation, &real);

        // Empty think-tag pair in content (no reasoning_content) must NOT
        // persist either — extract_think_block returns Some("").
        let open = "<\u{74}hink>".to_string();
        let close = "</\u{74}hink>".to_string();
        let empty_tag = ChatResponse {
            content: format!("{open}{close}answer"),
            ..Default::default()
        };
        persist_think_to_conversation(&conversation, &empty_tag);

        std::thread::sleep(std::time::Duration::from_millis(50));
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async { conversation.close().await.unwrap() });

        let raw = std::fs::read_to_string(
            temp_dir
                .path()
                .join("conversations")
                .join(format!("{session_id}.jsonl")),
        )
        .unwrap();
        let roles: Vec<String> = raw
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v.get("role").and_then(|r| r.as_str()).map(str::to_string))
            .collect();
        assert_eq!(
            roles,
            vec!["thought".to_string()],
            "blank reasoning must not persist; real reasoning must"
        );
    }
}
