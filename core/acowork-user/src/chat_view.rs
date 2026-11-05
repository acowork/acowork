//! The chat read model (design 31 §6) — the **one** place a chat row's state is
//! derived.
//!
//! A transcript line is a fact; the *state* around it ("did anything happen to
//! my message", "which session did it go to", "is this question still open") is
//! a composition of facts. That composition lives here, server-side, and the
//! surfaces render it verbatim. The rule it exists to enforce: a client that
//! scans the transcript for a row whose `in_reply_to` matches a coordinate is
//! deriving business state, and every consumer that does it re-implements the
//! rule slightly differently — which is how a card landed with an empty
//! coordinate and a send kept claiming to be in flight for 300 s.
//!
//! Two properties make this a *view* and not a second truth:
//! - it is computed from the log on every read, never stored (design 31 §3.2 R2);
//! - it names facts that are already durable (the answering row, the round
//!   coordinates) instead of summarising them into new ones.
//!
//! Scope: only rows that can be answered (`user_message`) and rows that ask
//! (`session_choice` / `confirm_continue` cards) carry a `delivery`; every other
//! row is a fact in itself and gets none.

use std::collections::HashMap;
use std::path::Path;

use serde::Serialize;

use crate::chat_sessions::{KIND_SYSTEM_NOTICE, KIND_USER_MESSAGE, SessionMessage};
use acowork_core::intent::chat_kinds;

/// One transcript row as the surfaces receive it: the durable row, unchanged,
/// plus the state derived for it.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRowView {
    #[serde(flatten)]
    pub row: SessionMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryView>,
}

/// What happened to one row, in the terms a surface renders.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DeliveryView {
    /// True when a row in this session answers this one ([`chat_kinds::is_answer_row`]).
    /// This is the fact a client used to compute by scanning for `in_reply_to`.
    pub answered: bool,
    /// The answering row, when there is one. Log order: the first answer wins,
    /// which is also the one the transcript shows first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered_by: Option<AnswerRef>,
    /// The execution round this row's turn opened or continued — absent until a
    /// round exists, so "delivered, no session yet" is visible as such.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<RoundTarget>,
    /// Present on an addressing card: the question it asks and whether it has
    /// been answered yet (design 31 §4, `question`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<QuestionState>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnswerRef {
    pub message_id: String,
    pub kind: String,
}

/// Where a turn went (design 31 §4, `round_ref`): coordinates only, never the
/// round's content — that is read from the execution session.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoundTarget {
    pub agent_instance_id: String,
    pub agent_session_id: String,
    pub round_id: String,
    /// `Some(true)` = this turn opened the session, `Some(false)` = it carried
    /// on, `None` = the delivery never said (ADR-106 D3 forbids faking it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_new: Option<bool>,
}

/// An addressing card's own state: the answers it offers and whether the human
/// has already taken one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuestionState {
    /// A round now answers the message this card asked about — the card was
    /// answered (its re-delivery leaves the reference row, ADR-106 D3).
    pub answered: bool,
    pub options: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_new_task: Option<bool>,
}

/// A page of the transcript, rows plus their derived state.
///
/// The window and its answers are read under separate locks, so a row landing
/// between them is not reflected here — the state can only move forward
/// (unanswered → answered), and the next read is consistent. Reading both under
/// one lock would mean holding the write path open for the duration of a page.
pub fn read_page(
    data_dir: &Path,
    session_id: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<ChatRowView>, u64), String> {
    let (rows, total) = crate::chat_sessions::read_messages(data_dir, session_id, offset, limit)?;
    // Each row asks about itself (a message: what happened to it) or about the
    // message it names (a card: is the question still open).
    let mut ids: Vec<String> = Vec::with_capacity(rows.len() * 2);
    for row in &rows {
        ids.push(row.meta.message_id.clone());
        ids.push(row.meta.in_reply_to.clone());
    }
    let answers = crate::chat_sessions::answering_rows(data_dir, session_id, &ids)?;
    Ok((
        rows.into_iter().map(|row| view(row, &answers)).collect(),
        total,
    ))
}

/// Project one row.
///
/// `answers` maps a `message_id` to the rows that thread themselves to it, in
/// log order (see `chat_sessions::answering_rows`).
pub fn view(
    row: SessionMessage,
    answers: &HashMap<String, Vec<SessionMessage>>,
) -> ChatRowView {
    let delivery = delivery_of(&row, answers);
    ChatRowView { row, delivery }
}

fn delivery_of(
    row: &SessionMessage,
    answers: &HashMap<String, Vec<SessionMessage>>,
) -> Option<DeliveryView> {
    let is_card = row.kind == KIND_SYSTEM_NOTICE
        && chat_kinds::is_card_notice(row.meta.notice_type.as_str());

    if is_card {
        // A card is a question about the message it names — never about itself:
        // nothing threads to a question. It is answered when a round now
        // answers that message.
        let asked = answers.get(row.meta.in_reply_to.as_str());
        let answered = asked
            .map(|rows| {
                rows.iter()
                    .any(|r| r.kind == chat_kinds::EXECUTION_ROUND)
            })
            .unwrap_or(false);
        return Some(DeliveryView {
            answered: false,
            answered_by: None,
            target: None,
            question: Some(QuestionState {
                answered,
                options: row.meta.options.clone(),
                allow_new_task: row.meta.allow_new_task,
            }),
        });
    }

    if row.kind != KIND_USER_MESSAGE {
        return None;
    }

    let answering = answers
        .get(row.meta.message_id.as_str())
        .map(Vec::as_slice)
        .unwrap_or_default();
    let first = answering
        .iter()
        .find(|r| chat_kinds::is_answer_row(r.kind.as_str(), r.meta.notice_type.as_str()));
    let Some(first) = first else {
        // No answer yet. Reported explicitly rather than as an absent field: a
        // message that is still waiting is a state, not a missing value.
        return Some(DeliveryView::default());
    };
    let target = answering
        .iter()
        .find(|r| r.kind == chat_kinds::EXECUTION_ROUND)
        .map(|r| RoundTarget {
            agent_instance_id: r.meta.agent_instance_id.clone(),
            agent_session_id: r.meta.agent_session_id.clone(),
            round_id: r.meta.round_id.clone(),
            opened_new: r.meta.opened_new,
        });
    Some(DeliveryView {
        answered: true,
        answered_by: Some(AnswerRef {
            message_id: first.meta.message_id.clone(),
            kind: first.kind.clone(),
        }),
        target,
        question: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_sessions::IntentMeta;

    fn row(kind: &str, message_id: &str, in_reply_to: &str) -> SessionMessage {
        SessionMessage {
            seq: 0,
            ts: 0,
            from: "u".into(),
            kind: kind.into(),
            body: String::new(),
            to: String::new(),
            meta: IntentMeta {
                message_id: message_id.into(),
                in_reply_to: in_reply_to.into(),
                ..Default::default()
            },
            attachment_ids: Vec::new(),
            attachments: Vec::new(),
        }
    }

    fn human(message_id: &str) -> SessionMessage {
        let mut m = row(KIND_USER_MESSAGE, message_id, "");
        m.from = "u".into();
        m
    }

    fn card(message_id: &str, asks: &str) -> SessionMessage {
        let mut m = row(KIND_SYSTEM_NOTICE, message_id, asks);
        m.meta.notice_type = "confirm_continue".into();
        m.meta.options = vec![serde_json::json!({"session_id": "s-1", "label": "task"})];
        m.meta.allow_new_task = Some(true);
        m
    }

    fn round_ref(message_id: &str, answers: &str, session: &str) -> SessionMessage {
        let mut m = row(chat_kinds::EXECUTION_ROUND, message_id, answers);
        m.meta.round_id = "r:ref".into();
        m.meta.agent_session_id = session.into();
        m.meta.agent_instance_id = "inst-1".into();
        m.meta.opened_new = Some(false);
        m
    }

    fn answers(pairs: &[(&str, SessionMessage)]) -> HashMap<String, Vec<SessionMessage>> {
        let mut map: HashMap<String, Vec<SessionMessage>> = HashMap::new();
        for (id, r) in pairs {
            map.entry((*id).to_string()).or_default().push(r.clone());
        }
        map
    }

    /// The whole point of the view: "did anything happen to my message, and
    /// where did it go" is answered by the backend, not by scanning rows.
    #[test]
    fn a_human_row_reports_its_answer_and_where_the_turn_went() {
        let m = human("m-1");
        let target = round_ref("round-ref", "m-1", "s-9");
        let map = answers(&[("m-1", target)]);
        let view = view(m, &map);
        let delivery = view.delivery.expect("a human row always carries delivery");
        assert!(delivery.answered);
        assert_eq!(delivery.answered_by.as_ref().map(|a| a.kind.as_str()), Some("execution_round"));
        let target = delivery.target.expect("the round coordinates");
        assert_eq!(target.agent_session_id, "s-9");
        assert_eq!(target.round_id, "r:ref");
        assert_eq!(target.opened_new, Some(false));
    }

    /// Waiting is a state, not a missing field: the composer lock reads it.
    #[test]
    fn an_unanswered_message_says_so_instead_of_omitting_the_field() {
        let view = view(human("m-1"), &answers(&[]));
        assert_eq!(view.delivery, Some(DeliveryView::default()));
    }

    /// A retired milestone is not an outcome (ADR-106 D14): a row nobody can see
    /// must not be the fact that made a message visibly handled.
    #[test]
    fn a_retired_notice_does_not_answer() {
        let mut notice = row(KIND_SYSTEM_NOTICE, "n-1", "m-1");
        notice.meta.notice_type = "turn_dispatched".into();
        let view = view(human("m-1"), &answers(&[("m-1", notice)]));
        assert!(!view.delivery.unwrap().answered);
    }

    /// A card is answered when a round answers the message it names — its own
    /// row is a question, and nothing threads to it.
    #[test]
    fn a_card_reports_whether_it_has_been_answered() {
        let open = view(card("c-1", "m-1"), &answers(&[]));
        let question = open.delivery.expect("a card carries delivery").question.expect("question state");
        assert!(!question.answered);
        assert_eq!(question.options.len(), 1);
        assert_eq!(question.allow_new_task, Some(true));

        let asked = answers(&[("m-1", round_ref("round-ref", "m-1", "s-9"))]);
        let done = view(card("c-1", "m-1"), &asked);
        assert!(done.delivery.unwrap().question.unwrap().answered);
    }
}
