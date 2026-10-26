//! Turn-origin vocabulary for Intent-routed turns (design 27 §5.4.5).
//!
//! The wire carries `kind` as a plain string (MQTT `Intent.kind`,
//! `DeliveryRequest.kind`, `SessionMessage::Intent.kind`); this module is
//! the single typed view over it. Producers (the Gateway endpoints) stamp
//! the value explicitly; every consumer that used to *infer* the scene
//! from a `kind` string or a `conversation_id` prefix goes through
//! [`TurnOrigin`] / [`TurnPolicy`] instead — the inference was what let a
//! PM dispatch (human-initiated) and an `intent_send` request
//! (agent-to-agent) collapse onto one `"request"` value and be misread
//! downstream (responder tool-trim on a human turn, envelope JSON shown
//! in execution transcripts).
//!
//! Task coordinates (`task:{project}:{task}` conversation ids and the
//! `conv:task:{project}:{task}` runtime routing keys) keep their wire
//! shapes — they are documented contracts (design 27 §6.2, the
//! `/intent-context/close` endpoint) — but the format lives *here*: build
//! with [`task_conversation_id`] / [`task_routing_key`], test with
//! [`is_task_conversation`] / [`is_task_routing_key`], never a raw
//! `starts_with` in feature code.

/// Who produced this turn and through which channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOrigin {
    /// A human chat message routed through the Intent transport —
    /// 1:1 chat, group @-mention, or the pinned execution composer
    /// (design 27 §5.4.4). Wire: `"user_message"`.
    Chat,
    /// An acowork-pm task dispatch: the human owner speaks, the PM
    /// service pushes. Human-initiated, task-keyed, owes the chat a
    /// terminal row. Wire: `"pm_task"`.
    PmTask,
    /// An agent-to-agent `intent_send` request — the responder answers a
    /// peer, not a person. Wire: `"request"` (and the empty legacy
    /// value, e.g. cron triggers, which keep the conservative
    /// pre-split default).
    AgentRequest,
    /// The answer relayed back to an initiator (system-generated at the
    /// responder's turn end, or the sweeper's fallback receipt).
    /// Wire: `"response"`.
    AgentResponse,
}

/// What an origin implies for the receiving turn. One table, queried at
/// every decision point; the fields are orthogonal by design (design 27
/// plan D-C/C1 split role from write-back — do not re-couple them).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnPolicy {
    /// The receiving agent's Chat Router may choose the execution
    /// session. `false` means the route is deterministic (task /
    /// conversation / designated-session key) and the router must not
    /// second-guess it.
    pub router_eligible: bool,
    /// Render the full envelope (action + params JSON + coordinates) as
    /// the model-facing user message. `false` means the turn carries
    /// human text and the model must see the person speaking, not a
    /// structure.
    pub render_envelope: bool,
    /// Responder role: trim `intent_send` / `ask_user_question`, refuse
    /// human-awaiting pauses (R1/R3). Only a peer request answers on a
    /// human's behalf — a PM task must keep the full tool set.
    pub responder: bool,
    /// The turn owes a write-back: a terminal chat row, a response
    /// posted to `POST /api/intents/{request_id}/response`, or both.
    /// For [`TurnOrigin::Chat`] this additionally requires a
    /// `message_id` coordinate — enforced at the single consumer
    /// ([`TurnOrigin::write_back`]).
    pub write_back: bool,
}

impl TurnOrigin {
    /// The wire value for the `kind` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "user_message",
            Self::PmTask => "pm_task",
            Self::AgentRequest => "request",
            Self::AgentResponse => "response",
        }
    }

    /// Parse from the wire. Unknown and empty values keep request
    /// semantics — the same conservative default the pre-split code used
    /// (an unrecognized turn must still answer its sender), and it is
    /// what a cron trigger (`kind: ""`) resolves to.
    pub fn parse(kind: &str) -> Self {
        match kind {
            "user_message" => Self::Chat,
            "pm_task" => Self::PmTask,
            "response" => Self::AgentResponse,
            _ => Self::AgentRequest,
        }
    }

    /// The policy row for this origin. The single source of truth for
    /// routing / rendering / role decisions on inbound turns.
    pub fn policy(self) -> TurnPolicy {
        match self {
            // A human turn: router picks the session, plain text reaches
            // the model, full tool set, the chat owes a terminal row.
            Self::Chat => TurnPolicy {
                router_eligible: true,
                render_envelope: false,
                responder: false,
                write_back: true,
            },
            // A PM task: the task IS the conversation (§6.2), so the
            // route is keyed, not router-chosen. Otherwise identical to
            // a chat turn — it is human input and must behave like one.
            Self::PmTask => TurnPolicy {
                router_eligible: false,
                render_envelope: false,
                responder: false,
                write_back: true,
            },
            Self::AgentRequest => TurnPolicy {
                router_eligible: false,
                render_envelope: true,
                responder: true,
                write_back: true,
            },
            Self::AgentResponse => TurnPolicy {
                router_eligible: false,
                render_envelope: true,
                responder: false,
                write_back: false,
            },
        }
    }

    /// Whether this turn owes a write-back, given the reply coordinate.
    /// A `Chat` turn without a `message_id` has nothing to answer
    /// against (design 27 §5.4.4); every other origin's answer is the
    /// static policy.
    pub fn write_back(self, message_id: &str) -> bool {
        match self {
            Self::Chat => !message_id.is_empty(),
            other => other.policy().write_back,
        }
    }
}

/// Conversation-id prefix marking a PM task conversation
/// ([`task_conversation_id`]). Public so the Runtime's registry and the
/// Gateway's mailbox can describe the same coordinate with one helper.
pub const TASK_CONVERSATION_PREFIX: &str = "task:";

/// The Runtime intent-map key prefix for a task context — the output of
/// the task branch of `routing_key` and the key the Gateway's
/// `/intent-context/close` call addresses.
pub const TASK_ROUTING_KEY_PREFIX: &str = "conv:task:";

/// The task IS the conversation (design 27 §6.2): one conversation — and
/// one execution session per (task, agent) — even inside a shared
/// project group chat. Also the §6.5 R4 budget key.
pub fn task_conversation_id(project_id: &str, task_id: &str) -> String {
    format!("{TASK_CONVERSATION_PREFIX}{project_id}:{task_id}")
}

/// True when `conversation_id` names a PM task conversation.
pub fn is_task_conversation(conversation_id: &str) -> bool {
    conversation_id.starts_with(TASK_CONVERSATION_PREFIX)
}

/// The runtime routing key for a task context (see
/// [`TASK_ROUTING_KEY_PREFIX`]).
pub fn task_routing_key(project_id: &str, task_id: &str) -> String {
    format!("{TASK_ROUTING_KEY_PREFIX}{project_id}:{task_id}")
}

/// True when an intent-map key names a task context (a closed one is
/// terminal-mirrored, ADR-099 D5).
pub fn is_task_routing_key(key: &str) -> bool {
    key.starts_with(TASK_ROUTING_KEY_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pm_task_is_a_human_turn_on_a_keyed_route() {
        // The bug this enum kills: a PM dispatch used to ride
        // `kind="request"` and take the responder role (tool trim,
        // envelope JSON rendered at the model). It is human input.
        let p = TurnOrigin::PmTask.policy();
        assert!(!p.responder, "a PM task turn keeps ask_user_question");
        assert!(!p.render_envelope, "a PM task turn renders human text");
        assert!(!p.router_eligible, "the task key beats the router (§6.2)");
        assert!(p.write_back, "the project group owes a terminal row");
    }

    #[test]
    fn wire_round_trip_and_legacy_defaults() {
        for origin in [
            TurnOrigin::Chat,
            TurnOrigin::PmTask,
            TurnOrigin::AgentRequest,
            TurnOrigin::AgentResponse,
        ] {
            assert_eq!(TurnOrigin::parse(origin.as_str()), origin);
        }
        // Empty (cron) and unknown kinds keep the conservative default.
        assert_eq!(TurnOrigin::parse(""), TurnOrigin::AgentRequest);
        assert_eq!(TurnOrigin::parse("nonsense"), TurnOrigin::AgentRequest);
    }

    #[test]
    fn chat_write_back_needs_a_coordinate() {
        assert!(!TurnOrigin::Chat.write_back(""));
        assert!(TurnOrigin::Chat.write_back("m1"));
        // A request always owes its answer even without one (mailbox-only
        // degradation, design 27 §5.4.5).
        assert!(TurnOrigin::AgentRequest.write_back(""));
        assert!(!TurnOrigin::AgentResponse.write_back("m1"));
    }

    #[test]
    fn task_coordinates_are_stable_contracts() {
        // These shapes are cross-process contracts (design 27 §6.2 and
        // POST /intent-context/close) — changing them breaks a running
        // Gateway/Runtime pair. Pin them.
        assert_eq!(
            task_conversation_id("p-1", "t-2"),
            "task:p-1:t-2".to_string()
        );
        assert!(is_task_conversation("task:p-1:t-2"));
        assert!(!is_task_conversation("conv-abc"));
        assert_eq!(task_routing_key("p-1", "t-2"), "conv:task:p-1:t-2".to_string());
        assert!(is_task_routing_key("conv:task:p-1:t-2"));
        assert!(!is_task_routing_key("conv:agent-conv"));
        assert!(!is_task_routing_key("chat:cs1"));
    }
}
