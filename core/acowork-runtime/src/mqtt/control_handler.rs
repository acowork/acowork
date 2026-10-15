//! MQTT control handler (ADR-033 Phase 3).
//!
//! Receives ControlCommand protobuf messages from the MQTT `control_rx`
//! channel and dispatches to the Runtime agent loop, following the same
//! business logic as `gateway_loop::dispatch_inbound()` (ADR-040).
//!
//! Protocol: `docs/zh/protocols/mqtt.md` §3.2, §5.2
//!
//! ## Message flow
//!
//! ```text
//! MQTT topic: acowork/agents/{id}/sessions/control/{cmd}
//!   ↓
//! RuntimeMqttClient (subscription)
//!   ↓
//! control_rx: UnboundedReceiver<(topic: String, payload: Vec<u8>)>
//!   ↓
//! parse DataEnvelope → ControlCommand
//!   ↓
//! match command:
//!   Intent → push an intent into the agent loop
//! ```
//!
//! ## Scope (ADR-076 §决策 4)
//!
//! This channel carries **no identity** — the broker has no per-topic ACL,
//! so it cannot attribute a publisher, and `ControlCommand` has no field to
//! carry one. Therefore **nothing user-initiated may travel here**: any
//! client on the broker could otherwise act inside another account's
//! session (including `approval_decision{approved:true}`, i.e. arbitrary
//! command execution). Every user-triggered session action — lifecycle
//! (create / open / close / delete / retitle / visibility / workspace /
//! config) and the action wave (chat / stop / continue / approval /
//! question_answer / cancel_tool / compress) — goes over the Gateway's
//! authenticated HTTP API. Their proto fields are *gone*, so they are not
//! merely rejected: they cannot be expressed.
//!
//! What is left is one signal that is not a user action and needs no user
//! identity:
//!
//! - `Intent` — Gateway → Runtime (cron triggers, cross-agent messaging).
//!
//! ## Performance
//!
//! - Control commands (QoS 1): handled inline, not spawned
//! - Session events (QoS 0): fire-and-forget via `publish_session_event`
//! - `control_rx` is Unbounded → backpressure-safe

use acowork_core::mqtt_proto::{self, data_envelope::Payload};
use prost::Message as ProstMessage;

/// Parsed MQTT control command with routing metadata.
///
/// ADR-076 §决策 4: this enum holds **no user-initiated command**. All of
/// them run over the Gateway's authenticated HTTP API
/// (`http::session_control`), because this channel cannot carry identity.
/// What is left is the single non-user signal.
#[derive(Debug)]
pub enum ControlAction {
    /// Gateway pushed an Intent (cron trigger, cross-agent messaging).
    /// Envelope fields follow design doc 27 §3.2; every value is
    /// Gateway-stamped (tech-debt plan D4/D5).
    IntentReceived {
        from: String,
        action: String,
        params_json: String,
        message_id: String,
        conversation_id: String,
        source_agent_session_id: String,
        source_chat_session_id: String,
        repo: String,
        /// Initiator's session-config suggestion (design 27 §4; shape
        /// checked by the Gateway, clamped to the responder's allowed
        /// set at session creation, §13-6). Empty = none.
        session_config_json: String,
        /// "request" | "response" (design 27 §6.5 R2). Empty = request.
        kind: String,
        /// For a response: the request's message_id (§4).
        in_reply_to: String,
        /// For a response: §7.1 outcome. Empty = completed.
        status: String,
    },
}

/// Parse a raw MQTT payload (protobuf DataEnvelope bytes) into a ControlAction.
///
/// Exhaustive over `mqtt_proto::control_command::Command`: adding a field to
/// the oneof is a compile error here, which is the point — every new control
/// command has to be consciously classified as "not a user action" (the only
/// thing allowed on this channel) before it compiles.
pub fn parse_control_payload(topic: &str, payload: &[u8]) -> Option<ControlAction> {
    let envelope = mqtt_proto::DataEnvelope::decode(payload).ok()?;

    let command = match envelope.payload? {
        Payload::ControlCommand(cmd) => cmd,
        _ => {
            tracing::debug!(topic, "MQTT control message is not a ControlCommand");
            return None;
        }
    };

    let action = match command.command? {
        mqtt_proto::control_command::Command::Intent(intent) => ControlAction::IntentReceived {
            from: intent.from,
            action: intent.action,
            params_json: intent.params_json,
            message_id: intent.message_id,
            conversation_id: intent.conversation_id,
            source_agent_session_id: intent.source_agent_session_id,
            source_chat_session_id: intent.source_chat_session_id,
            repo: intent.repo,
            session_config_json: intent.session_config_json,
            kind: intent.kind,
            in_reply_to: intent.in_reply_to,
            status: intent.status,
        },
    };

    Some(action)
}
