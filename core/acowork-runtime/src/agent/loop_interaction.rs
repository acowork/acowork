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

impl AgentLoop {
    /// Design 27 §5.5.4 (plan 3.1): drop an `interaction_request` card
    /// into the Chat session that triggered this turn. No-op unless the
    /// turn owes a chat write-back AND the session is known to have a
    /// human who can answer (`chat_has_human != Some(false)` — unknown
    /// fails open, the card is harmless if nobody picks it up, the row
    /// is the durable record of the question).
    ///
    /// The card is append-only state: pending = no later row carries an
    /// `in_reply_to` pointing at it (§5.5.4). Best-effort: a failed card
    /// write logs; the in-flight approval/question/pause itself is
    /// unaffected.
    pub(crate) async fn write_interaction_card(
        &self,
        interaction: serde_json::Value,
        summary: &str,
    ) {
        let Some(wb) = self.chat_writeback.as_ref() else {
            return;
        };
        if self.chat_has_human == Some(false) {
            return;
        }
        let card_id = uuid::Uuid::new_v4().to_string();
        let meta = serde_json::json!({
            "message_id": card_id,
            "in_reply_to": wb.request_id,
            "agent_session_id": self.session_core.session_id.clone().unwrap_or_default(),
            "interaction": interaction,
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
            &wb.source_chat_session_id,
            "interaction_request",
            &wb.from,
            summary,
            meta,
        )
        .await
        {
            tracing::warn!(error = %e, card_id, "interaction_request card write failed (best-effort)");
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
