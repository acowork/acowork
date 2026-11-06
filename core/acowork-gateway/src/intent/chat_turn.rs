//! The one Gateway-side opening for a Chat turn (ADR-106 D3, rev. 2026-11-06).
//!
//! Every delivery that is *asked for* by a sentence in a Chat session must name
//! a durable Chat row, and the Runtime threads the round it opens under that
//! row — the block hangs beneath the message that asked for the work, the
//! transcript shows the sentence, unread counts it and the search index holds
//! it. Three producers in this crate do that: a 1:1 message
//! ([`crate::http::intent_api`]'s `deliver_chat_message`), a group `@` fan-out
//! (the same file's `mentions`), and a PM task dispatch. Each of them used to
//! call the Chat store itself, and one of them silently stopped doing so
//! (ADR-106 D12 as first written), which left a reference row whose
//! `in_reply_to` named a mailbox id no chat holds: a block with no sentence
//! above it, in a conversation that never heard the task was assigned.
//!
//! So the write lives here, once. Callers hand over the **voice** — a session
//! participant — and the id their delivery will carry, and get back the row id
//! the delivery must be keyed to. Two things are therefore structural rather
//! than remembered:
//!
//! 1. **A delivery is keyed to the row it wrote.** [`open`] takes the delivery's
//!    `message_id` and writes the row under it, so `DeliveryRequest::message_id`
//!    and `chat_row_id` are one value and there is nothing to reconstruct on the
//!    receiving side.
//! 2. **The speaker's identity decides the kind.** A human participant's turn is
//!    a `user_message`, an agent's is an `agent_message` — the exact rule the
//!    store enforces from the other side (`only human participants may write
//!    user_message`). A caller cannot label a row with a kind its author could
//!    not speak, because no caller passes a kind at all.
//!
//! What this module deliberately does **not** do: decide whether the author is
//! allowed to speak (membership is the Chat's own permission, checked by the
//! store), build the row's meta (each producer knows its own payload keys), or
//! deliver (that is [`crate::intent::delivery`]).

use acowork_core::protocol::ChatAttachmentMeta;

use crate::intent::chat_client::{self, AppendMessage, SessionInfo};

/// One Chat turn a Gateway producer is about to deliver.
#[derive(Debug, Clone)]
pub struct Turn<'a> {
    /// The Chat session the turn belongs to.
    pub session_id: &'a str,
    /// The **voice**: a participant of that session. A human id and an agent
    /// instance id are both legal; what they are decides the row's kind.
    pub from: &'a str,
    /// Who the sentence is addressed to (display only — delivery addressing is
    /// the caller's `DeliveryRequest`).
    pub to: &'a str,
    /// The coordinate this delivery carries. The row is written under it, so a
    /// redelivery of the same id writes nothing and the receiver's reference row
    /// lands on the same coordinate.
    pub message_id: &'a str,
    /// The human-readable sentence. Never a model-facing instruction: a PM
    /// dispatch's wake-up playbook travels on the envelope's `assignment_brief`
    /// and touches no chat surface (design 27 §5.4.2).
    pub body: &'a str,
    /// The row's meta. `message_id` is stamped here by [`open`]; everything else
    /// is the producer's payload and must be a declared key on the store's
    /// `IntentMeta` (design 27 decision 29).
    pub meta: serde_json::Value,
    /// Chat attachment ids pinned to the row (empty for a PM dispatch — its
    /// attachments live in the PM store).
    pub attachment_ids: Vec<String>,
}

/// The row kind a speaker's identity implies, or the reason the id has no voice
/// in this chat.
///
/// The voice must be a participant, and the store's identity rule turns that
/// into the row's kind: a human participant's turn is a `user_message`, an
/// agent's is an `agent_message`. An id the session does not hold is not a
/// voice — a sentence in someone else's name is what a transcript must never
/// invent. An untyped (pre-design-27-§8.1) participant is *not* evidence of an
/// agent, so its turn is human speech, which is also the only kind the store
/// would accept for it.
fn kind_of<'a>(session: &'a SessionInfo, from: &str) -> Result<&'static str, String> {
    let author = session
        .participants
        .iter()
        .find(|p| p.id() == from)
        .ok_or_else(|| {
            format!(
                "'{from}' is not a participant of chat session {} — a Chat turn is spoken by a member",
                session.session_id
            )
        })?;
    Ok(if author.is_agent() {
        acowork_core::intent::chat_kinds::AGENT_MESSAGE
    } else {
        acowork_core::intent::chat_kinds::USER_MESSAGE
    })
}

/// The written turn: the id the delivery must be keyed to, plus the stored
/// attachment labels the row now carries (the composer's display copy).
#[derive(Debug, Clone)]
pub struct Opened {
    /// The coordinate [`open`] wrote the row under — the delivery's
    /// `message_id` and `chat_row_id`, one value.
    pub row_id: String,
    /// Stored attachment metadata read back by the store, not the caller's
    /// word (design 27 §4.2.1).
    pub attachments: Vec<ChatAttachmentMeta>,
}

/// Write the turn's row and return the coordinate the delivery must carry.
///
/// `Err` means the sentence has no durable place in the chat: the caller decides
/// what that costs (the human surfaces refuse to deliver, a PM dispatch reports
/// `chat_log_unavailable` and lets the scanner retry). It is never papered over
/// by delivering without a row — that is the defect this module exists to close.
pub async fn open(port: u16, session: &SessionInfo, turn: Turn<'_>) -> Result<Opened, String> {
    if turn.message_id.trim().is_empty() {
        return Err("a Chat turn needs the delivery coordinate it will be keyed to".to_string());
    }
    let kind = kind_of(session, turn.from)?;
    let mut meta = turn.meta;
    if let Some(object) = meta.as_object_mut() {
        object.insert(
            "message_id".to_string(),
            serde_json::Value::String(turn.message_id.to_string()),
        );
    }
    chat_client::append_message(
        port,
        turn.session_id,
        &AppendMessage {
            from: turn.from.to_string(),
            to: turn.to.to_string(),
            kind: kind.to_string(),
            body: turn.body.to_string(),
            meta,
            attachment_ids: turn.attachment_ids,
        },
    )
    .await
    .map(|attachments| Opened {
        row_id: turn.message_id.to_string(),
        attachments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::chat_client::ParticipantRef;

    fn session(participants: Vec<ParticipantRef>) -> SessionInfo {
        SessionInfo {
            session_id: "cs-1".to_string(),
            kind: "project_group".to_string(),
            participants,
            repo: None,
            chat_prompt: None,
            default_responder: None,
        }
    }

    fn human(id: &str) -> ParticipantRef {
        ParticipantRef::Typed {
            id: id.to_string(),
            identity: Some("human".to_string()),
        }
    }

    fn agent(id: &str) -> ParticipantRef {
        ParticipantRef::Typed {
            id: id.to_string(),
            identity: Some("agent".to_string()),
        }
    }

    fn turn<'a>(from: &'a str, message_id: &'a str) -> Turn<'a> {
        Turn {
            session_id: "cs-1",
            from,
            to: "target",
            message_id,
            body: "sentence",
            meta: serde_json::json!({ "task_id": "t-1" }),
            attachment_ids: Vec::new(),
        }
    }

    /// The voice decides the kind — the same rule the store enforces from the
    /// other side. A human-filed task speaks as the human, a task the PM agent
    /// created speaks as the PM, and no caller passes a kind at all.
    #[test]
    fn the_speakers_identity_decides_the_kind() {
        let roster = session(vec![human("u-1"), agent("pm-1")]);
        // The kind is not a parameter; `open` derives it. What can be asserted
        // without a live store is the derivation itself, so it is exercised
        // through the same accessor the write uses.
        assert_eq!(kind_of(&roster, "u-1"), Ok("user_message"));
        assert_eq!(kind_of(&roster, "pm-1"), Ok("agent_message"));
    }

    /// A sentence in someone else's name is what a transcript must never invent:
    /// the author has to be a member.
    #[test]
    fn a_non_participant_has_no_voice() {
        let roster = session(vec![human("u-1"), agent("pm-1")]);
        let err = kind_of(&roster, "stranger").unwrap_err();
        assert!(err.contains("not a participant"), "{err}");
    }

    /// An untyped (pre-§8.1) participant is not evidence of an agent, so its
    /// turn is human speech — the store would refuse the reverse.
    #[test]
    fn an_untyped_participant_is_not_an_agent() {
        let roster = session(vec![ParticipantRef::Legacy("legacy-1".to_string())]);
        assert_eq!(kind_of(&roster, "legacy-1"), Ok("user_message"));
    }

    /// The delivery coordinate and the row's `message_id` are one value, and a
    /// turn without one is refused rather than written under a fresh id: the
    /// receiving Runtime threads its round reference under this coordinate, so
    /// inventing one would recreate the dangling row this module exists to
    /// prevent.
    #[tokio::test]
    async fn a_turn_without_its_delivery_coordinate_is_refused() {
        let roster = session(vec![human("u-1")]);
        let err = open(0, &roster, turn("u-1", "")).await.unwrap_err();
        assert!(err.contains("coordinate"), "{err}");
    }
}
