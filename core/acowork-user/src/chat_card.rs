//! Answering an addressing card (ADR-106 D6) — the **rule**, once.
//!
//! A `session_choice` / `confirm_continue` card asks "which task did you mean?"
//! about a message the human already wrote. Answering it is therefore not a new
//! utterance: it is the *same words*, now addressed, re-delivered under the
//! original row's `message_id` so the transcript keeps one message instead of
//! narrating one decision twice.
//!
//! That rule used to live in both frontends (`buildCardAnswer` in desktop and
//! mobile), which meant two copies of it and one shared defect: to rebuild the
//! utterance a surface needs the original row's text, attachments and pinned
//! `@session` material, and it looked them up **in the loaded window**. A card
//! whose message had since scrolled out of the window rendered enabled buttons
//! that did nothing when clicked — measured: `if (!answer) return;`, silently.
//! That is the same class of failure design 31 removed elsewhere (a surface
//! holding a fact it cannot see), so the resolution moved here: the service owns
//! the log, the surface owns the click.
//!
//! What this module returns is the **command**, not a new fact: the text, the
//! attachments, the re-delivery key and the reference material of the original
//! utterance. The address (which session, or new work) stays with the surface,
//! because that is what the human just chose — and the two surfaces spell it
//! differently (a group answer names a session per agent).

use std::path::Path;

use serde::Serialize;

use crate::chat_sessions::{
    IntentMeta, KIND_SYSTEM_NOTICE, KIND_USER_MESSAGE, SessionMessage, answering_rows,
    message_by_id,
};
use crate::chat_view::question_answered;
use acowork_core::intent::chat_kinds;

/// Everything a surface needs to re-deliver the human's own words, addressed.
///
/// Coordinates and content only — no address, no "which option": those are the
/// click, and the click is the human's fact, not the service's.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CardAnswer {
    /// The agent that asked (the card's author). A card is that agent's
    /// question about its own runs, so the answer may name no other.
    pub agent_id: String,
    /// The original row's id, sent as `message_id` on the re-delivery: it is
    /// what keeps the transcript at one row while the delivery goes out.
    pub message_id: String,
    /// The human's own words, verbatim.
    pub text: String,
    /// The attachments the original message carried, so a re-delivery cannot
    /// quietly drop the files the human attached.
    pub attachment_ids: Vec<String>,
    /// The original row's `@session` material, filtered to the answering agent
    /// and echoed back pre-pinned (`round_id` / `terminal` included): it was
    /// decided when the row was written and must not be re-decided here.
    pub refs: Vec<serde_json::Value>,
}

/// Why a card cannot be answered.
///
/// Every variant is a caller-visible fact with its own status code: answering a
/// card is a re-delivery, so a card that cannot be answered honestly must say
/// which of the four reasons applies rather than fall back to a paraphrase.
#[derive(Debug, Clone, PartialEq)]
pub enum AnswerError {
    /// No such session (or the caller is not in it).
    NotParticipant,
    /// The named row is not an addressing card.
    NotACard,
    /// The row the card asks about is not readable — the card is a dangling
    /// question; nothing can be re-delivered.
    OriginGone,
    /// A round already answers the message this card asked about, so the
    /// question was answered (by this click or another): re-delivering again
    /// would send the same words twice.
    AlreadyAnswered,
}

impl AnswerError {
    /// The stable reason code the surface can branch on (localised there).
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotParticipant => "not_a_participant",
            Self::NotACard => "not_a_card",
            Self::OriginGone => "origin_gone",
            Self::AlreadyAnswered => "already_answered",
        }
    }
}

/// Resolve what answering `card_message_id` would send.
///
/// `who` is the human asking to answer. ADR-106 D10: only the person the card
/// asked may decide where their words go, so a bystander is refused here rather
/// than by a surface hiding buttons (an empty `to` is local mode / a broadcast
/// card, where there is exactly one human and the question is theirs).
pub fn answer(
    data_dir: &Path,
    session_id: &str,
    who: &str,
    card_message_id: &str,
) -> Result<CardAnswer, AnswerError> {
    let session = crate::chat_sessions::load(data_dir, session_id)
        .map_err(|_| AnswerError::NotParticipant)?;
    if !session.participants.iter().any(|p| p.id == who) {
        return Err(AnswerError::NotParticipant);
    }
    let card = message_by_id(data_dir, session_id, card_message_id)
        .map_err(|_| AnswerError::NotACard)?
        .ok_or(AnswerError::NotACard)?;
    // A card is a notice of a known class; anything else has no options to
    // answer, and "answered already" would be a meaningless question for it.
    if card.kind != KIND_SYSTEM_NOTICE
        || !chat_kinds::is_card_notice(card.meta.notice_type.as_str())
    {
        return Err(AnswerError::NotACard);
    }
    if !card.to.is_empty() && card.to != who {
        return Err(AnswerError::NotParticipant);
    }
    let asked = card.meta.in_reply_to.trim();
    if asked.is_empty() {
        return Err(AnswerError::OriginGone);
    }
    let original = message_by_id(data_dir, session_id, asked)
        .map_err(|_| AnswerError::OriginGone)?
        .ok_or(AnswerError::OriginGone)?;
    if original.kind != KIND_USER_MESSAGE {
        return Err(AnswerError::OriginGone);
    }
    // The answeredness judgement is the projection's, not a second copy of it
    // (design 31 §6): a round answering that message is what settles it.
    let answering = answering_rows(data_dir, session_id, &[asked.to_string()])
        .map_err(|_| AnswerError::OriginGone)?
        .remove(asked)
        .unwrap_or_default();
    if question_answered(&answering) {
        return Err(AnswerError::AlreadyAnswered);
    }

    let agent_id = card.from.trim().to_string();
    if agent_id.is_empty() {
        return Err(AnswerError::NotACard);
    }
    Ok(CardAnswer {
        agent_id: agent_id.clone(),
        message_id: original.meta.message_id.clone(),
        text: original.body.clone(),
        attachment_ids: attachment_ids_of(&original),
        refs: refs_for(&original.meta, &agent_id),
    })
}

/// The ids a re-delivery must carry: the resolved metadata when the row has it,
/// otherwise the raw ids (`append_message` resolves them at write time, so an
/// older row may only have the latter).
fn attachment_ids_of(original: &SessionMessage) -> Vec<String> {
    if original.attachments.is_empty() {
        original.attachment_ids.clone()
    } else {
        original.attachments.iter().map(|a| a.id.clone()).collect()
    }
}

/// The original row's reference material belonging to `agent_id`.
///
/// Only that agent's: a card is one agent's question about its own runs, so
/// echoing another agent's material back would address a bystander's session.
/// The quadruples are stored opaque (`meta.refs` is `Vec<Value>`, the pinner
/// owns the shape), so they travel verbatim rather than being re-serialised
/// through a type this service does not own.
fn refs_for(meta: &IntentMeta, agent_id: &str) -> Vec<serde_json::Value> {
    meta.refs
        .iter()
        .filter(|r| r.get("agent_instance_id").and_then(|v| v.as_str()) == Some(agent_id))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_sessions::{
        Participant, ParticipantIdentity, SessionKind, append_message, create_session,
    };

    fn temp_dir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "acowork-cardanswer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn session(dir: &Path) -> String {
        create_session(
            dir,
            SessionKind::UserAgent,
            &[
                Participant::new("human-1", ParticipantIdentity::Human),
                Participant::new("agent-a", ParticipantIdentity::Agent),
            ],
            "hi",
            None,
            None,
            1,
        )
        .unwrap()
        .session_id
    }

    fn human(id: &str) -> SessionMessage {
        SessionMessage {
            seq: 0,
            ts: 0,
            from: "human-1".into(),
            kind: KIND_USER_MESSAGE.into(),
            body: "整理上周的报表".into(),
            to: "agent-a".into(),
            meta: IntentMeta {
                message_id: id.into(),
                ..Default::default()
            },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
        }
    }

    fn card(id: &str, asks: &str) -> SessionMessage {
        SessionMessage {
            seq: 0,
            ts: 0,
            from: "agent-a".into(),
            kind: KIND_SYSTEM_NOTICE.into(),
            body: String::new(),
            to: "human-1".into(),
            meta: IntentMeta {
                message_id: id.into(),
                in_reply_to: asks.into(),
                notice_type: "session_choice".into(),
                options: vec![serde_json::json!({"session_id": "exec-1"})],
                allow_new_task: Some(true),
                ..Default::default()
            },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// Write a row the way a producer of an older build did: straight into the
    /// log, behind the write entry.
    ///
    /// The write entry now refuses a card whose subject is absent (a card is a
    /// question and an unanswerable question must not become durable), but the
    /// rows such a build already wrote are still on disk — and every read of them
    /// has to stay honest rather than pretend the question is live. That is what
    /// this fixture models.
    fn append_as_history(dir: &Path, sid: &str, row: &SessionMessage) {
        use std::io::Write;
        let path = crate::chat_sessions::session_dir(dir, sid).join("messages.jsonl");
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open log");
        writeln!(f, "{}", serde_json::to_string(row).expect("serialize")).expect("append");
    }

    fn round(id: &str, asks: &str) -> SessionMessage {
        SessionMessage {
            seq: 0,
            ts: 0,
            from: "agent-a".into(),
            kind: chat_kinds::EXECUTION_ROUND.into(),
            body: String::new(),
            to: "human-1".into(),
            meta: IntentMeta {
                message_id: id.into(),
                in_reply_to: asks.into(),
                agent_instance_id: "agent-a".into(),
                agent_session_id: "exec-1".into(),
                round_id: "r1".into(),
                ..Default::default()
            },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
        }
    }

    /// The rule: an answer is the human's own words, keyed to their own row, with
    /// the files and reference material that row carried. This is what the two
    /// frontends used to rebuild — from the window, which is why a scrolled-out
    /// card could not be answered at all.
    #[test]
    fn an_answer_echoes_the_original_utterance_verbatim() {
        let dir = temp_dir();
        let sid = session(&dir);
        let mut orig = human("m-1");
        orig.meta.refs = vec![
            serde_json::json!({"agent_instance_id": "agent-a", "session_id": "exec-1", "round_id": "r9", "terminal": true}),
            serde_json::json!({"agent_instance_id": "agent-b", "session_id": "exec-2"}),
        ];
        append_message(&dir, &sid, &orig, 2).unwrap();
        append_message(&dir, &sid, &card("c-1", "m-1"), 3).unwrap();

        let answer = answer(&dir, &sid, "human-1", "c-1").unwrap();
        assert_eq!(answer.agent_id, "agent-a");
        assert_eq!(answer.message_id, "m-1", "the re-delivery keeps one row");
        assert_eq!(answer.text, "整理上周的报表");
        assert!(answer.attachment_ids.is_empty());
        // Only the asking agent's material travels, and it travels pre-pinned.
        assert_eq!(answer.refs.len(), 1, "{:?}", answer.refs);
        assert_eq!(answer.refs[0]["round_id"], "r9");
        assert_eq!(answer.refs[0]["terminal"], true);
    }

    /// The ids a re-delivery carries come from the *resolved* metadata when the
    /// row has it (the store replaces the sender's raw ids at write time), and
    /// from the raw ids otherwise — dropping the files the human attached would
    /// make a card answer a different message than the one it asks about.
    #[test]
    fn attachment_ids_come_from_metadata_then_from_the_raw_list() {
        let mut row = human("m-1");
        row.attachment_ids = vec!["raw-1".into(), "raw-2".into()];
        assert_eq!(attachment_ids_of(&row), ["raw-1", "raw-2"]);

        row.attachments = vec![
            crate::chat::Attachment { id: "resolved-1".into(), ..raw_attachment() },
            crate::chat::Attachment { id: "resolved-2".into(), ..raw_attachment() },
        ];
        assert_eq!(
            attachment_ids_of(&row),
            ["resolved-1", "resolved-2"],
            "the resolved metadata wins: it is the durable part of the row"
        );
    }

    fn raw_attachment() -> crate::chat::Attachment {
        serde_json::from_value(serde_json::json!({
            "id": "x", "filename": "a.txt", "mime": "text/plain", "size": 1
        }))
        .expect("Attachment is the on-disk metadata shape")
    }

    /// ADR-106 D10: the card asks the person who spoke. A bystander is refused
    /// here, not merely by a hidden button.
    #[test]
    fn a_bystander_may_not_answer() {
        let dir = temp_dir();
        let sid = session(&dir);
        append_message(&dir, &sid, &human("m-1"), 2).unwrap();
        append_message(&dir, &sid, &card("c-1", "m-1"), 3).unwrap();
        assert_eq!(
            answer(&dir, &sid, "agent-a", "c-1"),
            Err(AnswerError::NotParticipant)
        );
    }

    /// Once a round answers the message, the card is answered: a second click
    /// must not send the same words again.
    #[test]
    fn an_answered_card_is_refused() {
        let dir = temp_dir();
        let sid = session(&dir);
        append_message(&dir, &sid, &human("m-1"), 2).unwrap();
        append_message(&dir, &sid, &card("c-1", "m-1"), 3).unwrap();
        assert!(answer(&dir, &sid, "human-1", "c-1").is_ok());
        append_message(&dir, &sid, &round("rr-1", "m-1"), 4).unwrap();
        assert_eq!(
            answer(&dir, &sid, "human-1", "c-1"),
            Err(AnswerError::AlreadyAnswered)
        );
    }

    /// A card whose message is gone is a dangling question, and the failure is
    /// named instead of silently clicking into nothing.
    #[test]
    fn a_card_without_its_message_says_so() {
        let dir = temp_dir();
        let sid = session(&dir);
        // The write entry refuses to create one...
        let err = append_message(&dir, &sid, &card("c-1", "m-ghost"), 2).unwrap_err();
        assert!(err.contains("in_reply_to"), "{err}");
        // ...but a row an older build wrote is still on disk, and the answer
        // still names why it cannot be given.
        append_as_history(&dir, &sid, &card("c-1", "m-ghost"));
        assert_eq!(
            answer(&dir, &sid, "human-1", "c-1"),
            Err(AnswerError::OriginGone)
        );
        // Not a card at all (a plain notice) has nothing to answer. Written as
        // history too: `delivery_failed` is a threaded notice, and the store
        // refuses a coordinate that resolves to nothing (ADR-106 D14).
        let mut notice = card("c-2", "m-ghost");
        notice.meta.notice_type = "delivery_failed".into();
        notice.meta.options.clear();
        append_as_history(&dir, &sid, &notice);
        assert_eq!(
            answer(&dir, &sid, "human-1", "c-2"),
            Err(AnswerError::NotACard)
        );
    }
}
