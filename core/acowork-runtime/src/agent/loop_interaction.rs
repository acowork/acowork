//! User interaction handling for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 4).
//! Contains methods for "special tools" that intercept the normal tool
//! dispatch flow and involve user interaction sub-protocols:
//! - `handle_ask_user_question`: validates params, emits AskQuestion event,
//!   transitions to WaitingApproval, blocks until user answers
//! - `handle_todo_write`: updates the session todo list (the next
//!   `build_chat_request` picks it up from SessionState)
//!
//! These methods are independent of the main loop orchestration — they are
//! called from the tool dispatch step in execute_single_iteration when a
//! matching tool call is detected.

use acowork_core::providers::traits::ToolCall;

use crate::agent::loop_::{AgentLoop, ChunkEvent};
use crate::agent::session_state::SessionStatus;
use crate::tools::builtin::ask_user_question::AskUserQuestionTool;

/// A Chat card the Runtime wrote and has not yet closed.
///
/// The row coordinates are captured at write time (not read back from
/// `chat_writeback`) because the closure frequently runs AFTER the turn
/// context was taken by the write-back — the turn-end flush in
/// `session_task` is exactly that case.
#[derive(Debug, Clone)]
pub(crate) struct OpenChatCard {
    /// `approval` | `ask_question` | `pause` — the `interaction.kind`.
    kind: String,
    /// The interaction's own request id (approval / question id, or the
    /// write-back request id for a `pause` card).
    request_id: String,
    /// Chat session the card row lives in.
    chat_session_id: String,
    /// The human-side participant the card row was addressed `to`.
    to: String,
}

/// Classify an `await_question_answer` result for the closure notice.
///
/// The await helper reports every non-answer through its returned
/// tool-result string (`[Timeout: ...]` / `[Cancelled: ...]`), which is the
/// only signal available at the call site; everything else is a real answer.
fn question_resolution(answer: &str) -> &'static str {
    if answer.starts_with("[Timeout") {
        "timeout"
    } else if answer.starts_with("[Cancelled") {
        "cancelled"
    } else {
        "answered"
    }
}

/// Deterministic Chat-card row id for one interaction (§5.5.4).
///
/// Two consequences that the closure protocol depends on:
/// - a redelivered turn cannot produce a second card (the chat store
///   dedups on `meta.message_id`), and
/// - the closure notice derives its own id from the same coordinate
///   (`ir:{card_id}`), so an answer that arrives on both surfaces — the
///   chat card AND the exec dialog — stays idempotent.
pub(crate) fn chat_card_id(kind: &str, request_id: &str) -> String {
    format!("card:{kind}:{request_id}")
}

impl AgentLoop {
    /// Design 27 §5.5.4 (plan 3.1): drop an `interaction_request` card
    /// into the Chat session that triggered this turn. No-op unless the
    /// turn owes a chat write-back AND the session is known to have a
    /// human who can answer (`chat_has_human != Some(false)` — unknown
    /// fails open, the card is harmless if nobody picks it up, the row
    /// is the durable record of the question).
    ///
    /// The card is recorded in `open_chat_cards` so its resolution can
    /// close it (see [`AgentLoop::close_chat_card`]): a card that stays
    /// open forever is what left the Chat surface showing a status card
    /// for a run that had already ended.
    ///
    /// Best-effort: a failed card write logs; the in-flight
    /// approval/question/pause itself is unaffected.
    pub(crate) async fn write_interaction_card(
        &mut self,
        interaction: serde_json::Value,
        summary: &str,
    ) {
        let Some(wb) = self.chat_writeback.as_ref() else {
            return;
        };
        if self.chat_has_human == Some(false) {
            return;
        }
        let kind = interaction
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let request_id = interaction
            .get("request_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if kind.is_empty() || request_id.is_empty() {
            tracing::warn!(?interaction, "interaction_request card without kind/request_id, skipping");
            return;
        }
        let card_id = chat_card_id(&kind, &request_id);
        let meta = serde_json::json!({
            "message_id": card_id,
            "in_reply_to": wb.request_id,
            "agent_session_id": self.session_core.session_id.clone().unwrap_or_default(),
            "interaction": interaction,
        });
        let (chat_session_id, to) = (wb.source_chat_session_id.clone(), wb.from.clone());
        if let Err(e) = crate::agent::intent_turn::append_chat_row(
            &self.core.config.agent_instance_id,
            &self
                .core
                .config
                .gateway_host
                .clone()
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            &self.core.config.mqtt_password.clone().unwrap_or_default(),
            &chat_session_id,
            "interaction_request",
            &to,
            summary,
            meta,
        )
        .await
        {
            tracing::warn!(error = %e, card_id, "interaction_request card write failed (best-effort)");
            return;
        }
        // Only a row that actually landed needs a closure.
        if !self.open_chat_cards.iter().any(|c| c.kind == kind && c.request_id == request_id) {
            self.open_chat_cards.push(OpenChatCard {
                kind,
                request_id,
                chat_session_id,
                to,
            });
        }
    }

    /// Close the Chat card for one interaction (§5.5.4 answer-then-done).
    ///
    /// Writes a `system_notice` row whose `in_reply_to` is the card id —
    /// which is precisely the fact the read-side projection treats as
    /// "this card is answered" — so the card stops rendering the moment
    /// the underlying interaction resolves, whichever surface resolved
    /// it (chat card, exec dialog, timeout, stop, turn end).
    ///
    /// A no-op when no card is open for that interaction: an answer that
    /// already arrived as a chat row, a card that never got written, or
    /// a second resolve path for the same request must not put a
    /// redundant row on the wire.
    pub(crate) async fn close_chat_card(&mut self, kind: &str, request_id: &str, resolution: &str) {
        let Some(idx) = self
            .open_chat_cards
            .iter()
            .position(|c| c.kind == kind && c.request_id == request_id)
        else {
            return;
        };
        let card = self.open_chat_cards.remove(idx);
        let card_id = chat_card_id(kind, request_id);
        let meta = serde_json::json!({
            // Deterministic: the same resolution never appends twice, even
            // across a retry of the turn.
            "message_id": format!("ir:{card_id}"),
            "notice_type": "interaction_resolved",
            "in_reply_to": card_id,
            "resolution": resolution,
        });
        if let Err(e) = crate::agent::intent_turn::append_chat_row(
            &self.core.config.agent_instance_id,
            &self
                .core
                .config
                .gateway_host
                .clone()
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            &self.core.config.mqtt_password.clone().unwrap_or_default(),
            &card.chat_session_id,
            "system_notice",
            &card.to,
            "",
            meta,
        )
        .await
        {
            // Keep the card open so a later resolve path retries the
            // closure — an unclosable card is the bug this function
            // exists to fix, and swallowing the failure would bring it
            // straight back.
            self.open_chat_cards.push(card);
            tracing::warn!(error = %e, card_id, "interaction_resolved notice write failed (best-effort)");
        }
    }

    /// Close every open card whose kind is listed in `kinds`, optionally
    /// restricted to one Chat session (`chat_session_id`).
    ///
    /// Used at turn end for the safety net: an in-loop interaction
    /// (`approval` / `ask_question`) can only still be open when the turn
    /// died around it — stop, error, crash — and the run is over, so the
    /// card is no longer answerable. `pause` cards are deliberately NOT
    /// included there: §5.5.4 releases the execution slot precisely so a
    /// pause card survives its own turn, and the human's answer opens a
    /// new one — which is when [`Self::supersede_pause_cards`] closes it.
    pub(crate) async fn close_chat_cards_of_kind(
        &mut self,
        kinds: &[&str],
        chat_session_id: Option<&str>,
        resolution: &str,
    ) {
        let targets: Vec<(String, String)> = self
            .open_chat_cards
            .iter()
            .filter(|c| kinds.contains(&c.kind.as_str()))
            .filter(|c| {
                chat_session_id.is_none_or(|cs| c.chat_session_id == cs)
            })
            .map(|c| (c.kind.clone(), c.request_id.clone()))
            .collect();
        for (kind, request_id) in targets {
            self.close_chat_card(&kind, &request_id, resolution).await;
        }
    }

    /// Close the `pause` cards a new turn in the same Chat session replaces.
    ///
    /// A pause card asks the human to reply to continue; the reply IS the new
    /// turn, so from that moment the old card can no longer be answered —
    /// leaving it open shows a stale "continue?" request under a run that has
    /// already moved past it.
    pub(crate) async fn supersede_pause_cards(&mut self, chat_session_id: &str) {
        self.close_chat_cards_of_kind(
            &["pause"],
            Some(chat_session_id),
            "superseded",
        )
        .await;
    }

    /// Mark the turn's chat write-back as user-interrupted (§7.1
    /// `aborted`).
    ///
    /// A stop ends the loop with no text, and the write-back used to
    /// record that as `status=completed` with an empty body — so the Chat
    /// surface could only guess at what happened ("no output") and the
    /// transcript could not tell an interrupted run from an empty answer.
    pub(crate) fn mark_turn_aborted(&mut self) {
        if let Some(ctx) = self.chat_writeback.as_mut() {
            ctx.outcome = "aborted".to_string();
        }
    }

    /// Handle an `ask_user_question` tool call.
    ///
    /// Validates the params, emits ChunkEvent::AskQuestion, transitions
    /// status to WaitingApproval, and blocks until the user responds.
    /// Returns the user's answer as a tool result string.
    pub(crate) async fn handle_ask_user_question(&mut self, tc: &ToolCall) -> String {
        // Validate params
        let params: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(p) => p,
            Err(e) => {
                return format!(
                    "Error: ask_user_question arguments are not valid JSON: {}",
                    e
                );
            }
        };

        let parsed = match AskUserQuestionTool::validate_params(&params) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: ask_user_question invalid params: {}", e);
            }
        };

        // Compute effective timeout from agent config (NOT from LLM-provided params).
        // The LLM cannot know how long the user will take — this is a scheduling
        // decision owned by the agent runtime / user preference.
        let effective_timeout_secs: u32 = self
            .core
            .approval_timeout_secs
            .map(|t| t as u32)
            .unwrap_or(acowork_core::timeout_config::constants::APPROVAL.as_secs() as u32);

        // Generate unique request ID (UUID v4; global uniqueness means
        // question ids and approval ids can never collide)
        let request_id = format!("q-{}", uuid::Uuid::new_v4());

        tracing::info!(
            request_id = %request_id,
            question = %parsed.question,
            options_count = parsed.options.len(),
            timeout_secs = %effective_timeout_secs,
            "AskUserQuestion: emitting AskQuestion event and waiting for answer"
        );

        // Emit ChunkEvent::AskQuestion (timeout_seconds is runtime-computed,
        // not LLM-provided, so the frontend always sees the user-preferred value)
        let card_options: Vec<String> =
            parsed.options.iter().map(|o| o.label.clone()).collect();
        let _ = self.session_core.try_send_chunk(ChunkEvent::AskQuestion {
            request_id: request_id.clone(),
            question: parsed.question.clone(),
            options: parsed.options,
            title: parsed.title.clone(),
            timeout_seconds: Some(effective_timeout_secs),
        });

        // §5.5.4 (plan 3.1): the same question is also a Chat-session card
        // when the turn was chat-triggered — one question, two answer
        // surfaces (the live dialog and the chat card), never a replay.
        self.write_interaction_card(
            serde_json::json!({
                "kind": "ask_question",
                "request_id": request_id.clone(),
                "question": parsed.question.clone(),
                "title": parsed.title.clone(),
                "options": card_options,
            }),
            &parsed.question,
        )
        .await;

        // Transition to WaitingApproval
        self.transition_status(SessionStatus::WaitingApproval {
            request_id: request_id.clone(),
        });

        // Heartbeat: mirror ADR-045's tool progress pattern so the
        // frontend's countdown is driven by the backend's wall-clock,
        // not by a local setInterval (which can drift under throttling
        // and let the UI show 1-3 minutes while the backend has already
        // fired the 5-minute timeout). We reuse `ChunkEvent::ToolProgress`
        // — ask_user_question is a tool that asks the user to do work,
        // and the data shape (correlation id + elapsed + timeout) is
        // identical. The frontend reads the entry via
        // `toolProgress[event.request_id]`.
        let hb_session_id = self.session_core.session_id.clone();
        let hb_chunk_tx = self.session_core.chunk_tx.clone();
        let hb_request_id = request_id.clone();
        let hb_timeout_ms = (effective_timeout_secs as u64) * 1000;
        let heartbeat_task = if let (Some(sid), Some(ct)) = (hb_session_id, hb_chunk_tx) {
            let heartbeat_interval = acowork_core::timeout_config::constants::TOOL_HEARTBEAT;
            Some(tokio::spawn(async move {
                let mut interval = tokio::time::interval(heartbeat_interval);
                // Skip the first (immediate) tick so the first heartbeat
                // lands at 5s, not 0s — matches loop_tools.rs.
                interval.tick().await;
                let q_start = std::time::Instant::now();
                loop {
                    interval.tick().await;
                    let elapsed = q_start.elapsed();
                    let event = crate::agent::loop_::ChunkEvent::ToolProgress {
                        session_id: sid.clone(),
                        // Wire-compatible with ToolProgressPayload. The
                        // semantic key here is the ask_user_question
                        // request_id; the field name reflects the
                        // generic "correlation id" intent.
                        tool_call_id: hb_request_id.clone(),
                        elapsed_ms: elapsed.as_millis() as u64,
                        timeout_ms: hb_timeout_ms,
                    };
                    if ct
                        .try_send(crate::agent::loop_::SessionChunkEvent {
                            session_id: sid.clone(),
                            event,
                        })
                        .is_err()
                    {
                        break;
                    }
                    if elapsed.as_millis() as u64 >= hb_timeout_ms {
                        break;
                    }
                }
            }))
        } else {
            None
        };

        // Wait for the user's answer (timeout driven by agent config)
        let answer = self.await_question_answer(&request_id).await;

        // §5.5.4: the question is settled — answered, timed out or cancelled
        // by a stop — so close its Chat card. Leaving it open is what kept a
        // status card on the Chat surface for a run that had already ended.
        self.close_chat_card("ask_question", &request_id, question_resolution(&answer))
            .await;

        // Abort the heartbeat — answer received (or timeout/cancel).
        // Non-blocking; the task will be dropped at the next await point
        // (which is the interval.tick().await). Matches loop_tools.rs.
        if let Some(ht) = heartbeat_task {
            ht.abort();
        }

        // Clear the retained `ask_question` message so a Desktop
        // reconnecting later does not see a stale question card.
        let _ = self
            .session_core
            .try_send_chunk(ChunkEvent::ClearRetainedEvent {
                event_type: "ask_question".to_string(),
            });

        // Transition back to Streaming (the loop will continue)
        // ADR-049: HTTP request about to be sent → LlmAwaitingFirstChunk.
        self.transition_status(SessionStatus::LlmAwaitingFirstChunk);

        tracing::info!(
            request_id = %request_id,
            answer_preview = %answer.chars().take(100).collect::<String>(),
            "AskUserQuestion: received answer"
        );

        // Return the answer as the tool result
        answer
    }

    /// Handle a `todo_write` tool call by updating SessionState.todos.
    ///
    /// The updated list reaches the system prompt through
    /// `build_chat_request()`, which unconditionally injects
    /// `session.format_todos()` into the ContextBuilder before every LLM
    /// call - no eager write is needed here.
    ///
    /// This is synchronous (no I/O or user interaction) since todos are
    /// pure in-memory state on SessionState.
    pub(crate) fn handle_todo_write(&mut self, tc: &ToolCall) -> String {
        use crate::agent::session_state::TodoItem;

        let params: serde_json::Value = match serde_json::from_str(&tc.function.arguments) {
            Ok(p) => p,
            Err(e) => {
                return format!("Error: todo_write arguments are not valid JSON: {}", e);
            }
        };

        let todos_array = match params.get("todos").and_then(|v| v.as_array()) {
            Some(arr) => arr,
            None => return "Error: todo_write requires a 'todos' array parameter".to_string(),
        };

        let merge = params
            .get("merge")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let mut items: Vec<TodoItem> = Vec::with_capacity(todos_array.len());
        for item in todos_array {
            let id = match item.get("id").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => return "Error: each todo item must have a string 'id' field".to_string(),
            };
            let content = match item.get("content").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => {
                    return format!("Error: todo item '{}' missing required 'content' field", id);
                }
            };
            let status = match item.get("status").and_then(|v| v.as_str()) {
                Some("pending") => crate::agent::session_state::TodoStatus::Pending,
                Some("in_progress") => crate::agent::session_state::TodoStatus::InProgress,
                Some("completed") => crate::agent::session_state::TodoStatus::Completed,
                Some(other) => {
                    return format!(
                        "Error: todo item '{}' has invalid status '{}'. Must be one of: pending, in_progress, completed",
                        id, other
                    );
                }
                None => {
                    return format!("Error: todo item '{}' missing required 'status' field", id);
                }
            };
            items.push(TodoItem {
                id,
                content,
                status,
            });
        }

        // Update the session todos
        self.session.update_todos(items, merge);

        // Emit TodoListUpdated event to frontend for UI rendering
        let _ = self
            .session_core
            .try_send_chunk(ChunkEvent::TodoListUpdated {
                todos: self.session.todos.clone(),
            });

        // Return formatted list as the tool result
        match self.session.format_todos() {
            Some(formatted) => {
                let count = self.session.todos.len();
                format!(
                    "Todo list updated ({} items, merge={}):\n{}",
                    count, merge, formatted
                )
            }
            None => "Todo list is now empty.".to_string(),
        }
    }
}
