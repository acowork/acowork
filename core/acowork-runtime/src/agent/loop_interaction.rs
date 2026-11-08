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

/// An interaction this turn has opened and has not yet resolved.
///
/// ADR-106 D8: this is bookkeeping, not a Chat write. A card used to be an
/// `interaction_request` row in the Chat session, which made the Chat the store
/// of an execution fact and gave it a second copy to keep closed. The card is
/// now the round's `card` in the projection (D4), and this list is what supplies
/// it at turn end — coordinates plus the payload the block renders.
#[derive(Debug, Clone)]
pub(crate) struct OpenInteraction {
    /// `approval` | `ask_question` | `pause` — the interaction's kind.
    kind: String,
    /// The interaction's own request id (approval / question id, or the round id
    /// for a `pause`), which is the coordinate the answer resolves against.
    request_id: String,
    /// The projection card payload: `{kind, request_id, prompt, options}`.
    card: serde_json::Value,
}

impl AgentLoop {
    /// Record the interaction this turn is about to block on (ADR-106 D8).
    ///
    /// The gates retired with the Chat row: a card is a fact about the round, so
    /// it is recorded whether or not this turn is accountable to a Chat, and
    /// whether or not the Chat is known to be human-free (D8's third bullet).
    /// Whoever may *answer* it is a separate question, settled at the write
    /// entry by `answerable_by` (D10) — which is why an unanswerable card parks
    /// nothing: it is state, not an obligation.
    ///
    /// Callers that need an answerer to exist before the interaction is even
    /// opened (the `pause` cards, which would hold a round hostage otherwise)
    /// keep checking `has_human == Some(true)` themselves.
    pub(crate) fn track_interaction(&mut self, interaction: serde_json::Value) {
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
            tracing::warn!(?interaction, "interaction without kind/request_id, not tracked");
            return;
        }
        if self
            .open_interactions
            .iter()
            .any(|c| c.kind == kind && c.request_id == request_id)
        {
            return;
        }
        // The D4 card shape: what the human is being asked, and what they can
        // answer with. Coordinates only for `pause`, which has no option set.
        let card = serde_json::json!({
            "kind": kind,
            "request_id": request_id,
            "prompt": interaction
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
            "options": interaction
                .get("options")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        });
        self.open_interactions.push(OpenInteraction {
            kind,
            request_id,
            card: card.clone(),
        });
        // ADR-106 D8, second half: the block becomes visible when the question
        // opens, not when the turn ends. A round parked inside a live turn is
        // still non-terminal, so this is never cached (D5) and a reader always
        // folds the current state — and the stamp stays armed because the round
        // has rows left to write.
        self.publish_round_card_state(Some(card));
    }

    /// State the round's card on the side table without closing the round, and
    /// ring the round doorbell.
    ///
    /// `Some(card)` parks the round on that card (`waiting`); `None` says nothing
    /// is pending anymore, which is what a resolved mid-turn interaction means
    /// for the block: the live turn is still working, and offering a button that
    /// was already answered would be a second, lying copy of the fact.
    ///
    /// The doorbell is not optional. A round's card appears and disappears
    /// **inside** a live turn, so neither end of this transition is a turn
    /// boundary — and the Chat block's interval backstop is 15 s, which a
    /// webview that is occluded (the human reading the execution surface) does
    /// not even honour. Measured on a real ask-question: card recorded 22:22:21,
    /// resolved 22:23:15, and the only block read in that window was at 22:22:17
    /// — the question was live for its whole life and the Chat never saw it. The
    /// same hole swallowed the retraction, which would have left a stale card
    /// offering a button whose question was already answered. ADR-106's revision
    /// says a round that "parks on a card a human must answer" rings; this is
    /// where that round state actually changes.
    ///
    /// Ringing after the write, not before: the bell only means "worth
    /// re-reading", and the reader's bounded 500 ms retry (published path) is
    /// what covers the writer's few-microsecond lag — the same ordering the
    /// turn-boundary bell already relies on.
    pub(crate) fn publish_round_card_state(&self, card: Option<serde_json::Value>) {
        let Some(conv) = &self.session.conversation else {
            return;
        };
        let round_id = match card {
            Some(c) => conv.note_round_state("waiting", Some(c)),
            None => conv.note_round_state(
                crate::rounds::STATUS_IN_PROGRESS,
                Some(serde_json::Value::Null),
            ),
        };
        // Ring for the round the write named, never for a guess: `None` means no
        // round was armed, so nothing was staged for a reader either.
        if let Some(round_id) = round_id {
            self.publish_round_state(&round_id);
        }
    }

    /// The card the round is parked on right now, if any.
    fn current_round_card(&self) -> Option<serde_json::Value> {
        self.open_interactions.last().map(|c| c.card.clone())
    }

    /// Drop the record for one resolved interaction.
    ///
    /// Idempotent by design: an approval answered through either surface, a
    /// resolve path that ran twice, or an interaction that was never tracked
    /// (no Chat this turn was accountable to) must all be no-ops.
    pub(crate) fn forget_interaction(&mut self, kind: &str, request_id: &str) {
        let Some(idx) = self
            .open_interactions
            .iter()
            .position(|c| c.kind == kind && c.request_id == request_id)
        else {
            return;
        };
        self.open_interactions.remove(idx);
        // The answer came in while the turn was live, so the block has to stop
        // showing the question now (ADR-106 D8).
        self.publish_round_card_state(self.current_round_card());
    }

    /// Drop every open record whose kind is listed in `kinds`.
    ///
    /// Used at turn end for the safety net: an in-loop interaction
    /// (`approval` / `ask_question`) can only still be open when the turn
    /// died around it — stop, error, crash — and the run is over, so it is no
    /// longer answerable. `pause` is deliberately NOT included there: §5.5.4
    /// releases the execution slot precisely so a pause survives its own turn,
    /// and the human's answer opens a new one — which is when
    /// [`Self::supersede_pause_cards`] drops it.
    pub(crate) fn forget_interactions_of_kind(&mut self, kinds: &[&str]) {
        self.open_interactions
            .retain(|c| !kinds.contains(&c.kind.as_str()));
    }

    /// Drop the `pause` a new turn in this session replaces.
    ///
    /// A pause asks the human to reply to continue; the reply IS the new turn, so
    /// from that moment the old pause is no longer answerable. Keeping it would
    /// have the next snapshot publish a stale "continue?" onto a round that has
    /// already moved past it.
    pub(crate) fn supersede_pause_cards(&mut self) {
        self.forget_interactions_of_kind(&["pause"]);
    }

    /// The card the current round is parked on, as a reference.
    ///
    /// ADR-106 D2/D4: a round's `card` is recorded when its turn ends `waiting`, so
    /// the block the Chat renders can show what it is stalled on. Only `pause`
    /// qualifies — `approval` and `ask_question` are resolved inside the turn they
    /// were opened in, so a round that ended with one still open ended by stop or
    /// error and is not waiting on anybody.
    ///
    /// Those two kinds are nevertheless tracked, and not as leftovers of the old
    /// Chat-row writes: they block *inside* a live turn, so [`Self::track_interaction`]
    /// publishes the round's `card` the moment they open (a non-terminal snapshot,
    /// so it is never cached) and [`Self::forget_interaction`] retracts it when they
    /// are answered. The block and the execution dialog therefore read one fact,
    /// not two copies.
    pub(crate) fn open_pause_card(&self) -> Option<serde_json::Value> {
        Some(self.open_interactions.iter().find(|c| c.kind == "pause")?.card.clone())
    }

    /// Mark the turn as user-interrupted (§7.1 `aborted`).
    ///
    /// A stop ends the loop with no text, and the write-back used to
    /// record that as `status=completed` with an empty body — so the Chat
    /// surface could only guess at what happened ("no output") and the
    /// transcript could not tell an interrupted run from an empty answer.
    ///
    /// Sets the loop's [`Self::turn_outcome`] — the one field both terminal
    /// writers read (the Chat echo row and the §7.1 write-back). A turn
    /// composed directly in the execution session carries no write-back
    /// context, yet the chat bound to that session still has to learn that the
    /// turn was stopped (ADR-100 revision).
    pub(crate) fn mark_turn_aborted(&mut self) {
        self.turn_outcome = crate::agent::intent_turn::TurnOutcome::Aborted;
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

        // ADR-106 D8: the same question is recorded on the round this turn is
        // running, so the Chat block can show what its round is stalled on — one
        // question, two answer surfaces (the live dialog and the block), never a
        // replay. It is not a Chat row anymore, which is why nothing here has to
        // keep a Chat copy closed.
        self.track_interaction(serde_json::json!({
            "kind": "ask_question",
            "request_id": request_id.clone(),
            "question": parsed.question.clone(),
            "title": parsed.title.clone(),
            "options": card_options,
        }));

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
        // by a stop — so it is no longer an open interaction. Leaving it
        // recorded is what kept a status card on the Chat surface for a run
        // that had already ended.
        self.forget_interaction("ask_question", &request_id);

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
