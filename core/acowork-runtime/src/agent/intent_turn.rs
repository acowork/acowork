//! Intent turn support (design doc 27 §5.4.5, §7.1).
//!
//! A turn triggered by an inbound Intent `request` puts the agent in the
//! **responder** role: `intent_send` / `ask_user_question` are hidden from
//! the tool set (R1), human-awaiting pause states are refused (R3), and the
//! turn's final output is written back to the Gateway as a `response` by
//! the *system* — never by a model tool call (§7.1).
//!
//! This module owns the pieces that are not loop mechanics: the context
//! struct, the user-visible rendering of an envelope, and the Gateway
//! write-back call.

use std::time::Duration;

use acowork_core::defaults::gateway_http_port;

use super::inbound::IntentEnvelope;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bound on the best-effort Gateway writes. All three calls below are
/// contractually fire-and-forget — a lost row degrades a surface, it never
/// loses the conversation — so an unbounded hang is strictly worse than a
/// logged failure: the Chat Router's write-back runs on a per-chat drain task,
/// and a stalled Gateway would otherwise park that chat's queue (and its
/// anchor) forever. Generous for a localhost hop, finite for a hung one.
const GATEWAY_WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Same bound for an attachment pull, scaled up for bytes over the loopback
/// relay rather than a JSON row.
const GATEWAY_PULL_TIMEOUT: Duration = Duration::from_secs(60);

/// The Chat surface a turn owes its visible behaviour to (ADR-099 §2.2
/// generalised; ADR-100).
///
/// Every obligation a turn has toward a Chat session — where an interactive
/// card is written, who may answer it, whether the turn may block on a human
/// at all, which Chat row the terminal echo belongs to — reads these facts
/// and nothing else. A turn carries exactly one, produced at the transport
/// boundary from the envelope fields the Gateway already stamped.
///
/// This struct exists because its absence was a root cause, not a symptom:
/// the same facts used to be re-derived ad hoc per consumer (`chat_writeback`
/// standing in for "is there a chat", `echo_chat_id` re-reading the session
/// binding, `has_human` copied into a separate field), so any transport that
/// dropped one of them silently turned a consumer's "no chat here" branch
/// into real behaviour — a card that never appears, a pause that aborts
/// instead of waiting. A consumer may not derive these facts from anything
/// else; a new transport or dispatch scenario produces a `TurnSurface` (or
/// states explicitly why it has no chat obligations).
#[derive(Debug, Clone, Default)]
pub struct TurnSurface {
    /// The Chat session this turn is accountable to. Empty = none.
    pub chat_session_id: String,
    /// The round this turn is (ADR-106 D2): `r:{inbound_message_id}`, derived
    /// here from the inbound id by the same function the turn boundary uses.
    ///
    /// It replaces the trigger-row coordinate the card writer and the terminal
    /// echo used to read: the chat holds no copy of this turn any more, it holds
    /// a reference to this round — so the round id, not a chat row id, is what
    /// everything on the chat side is addressed by. Empty = this transport
    /// carried no inbound id; callers must NOT mint from the empty string (that
    /// yields a random id, i.e. a second round for a redelivered message).
    pub round_id: String,
    /// Who may answer this turn's cards: the sender of the triggering row.
    /// `None` = the transport carried no sender, so a card would be
    /// unanswerable — the turn does not park on a human it cannot ask.
    pub answerable_by: Option<String>,
    /// Whether the accountable chat has a human participant, as derived by
    /// the Gateway from the session it bound to (never from the sender).
    /// `None` = unknown → fail open (interactive tools stay available). 
    /// `Some(false)` = agent-to-agent: a turn must not
    /// block on a human (the §2.2 availability gate, now transport-agnostic).
    pub has_human: Option<bool>,
}

impl TurnSurface {
    /// The single producer: turn-to-chat accountability is decided here,
    /// from the envelope, for every transport (Chat write endpoint, pinned
    /// turn, Intent delivery). A turn bound to a Chat with a known sender
    /// owes its card and its reference row to that Chat; the kind of the
    /// turn does not change that — it changes only whether the turn must
    /// *reply* to another agent, which is [`IntentTurnContext`]'s business.
    pub fn from_envelope(env: &IntentEnvelope) -> Self {
        Self::new(
            env.source_chat_session_id.clone(),
            &env.message_id,
            &env.from,
            env.has_human,
        )
    }

    /// The field-level form of [`Self::from_envelope`], for a caller that has
    /// already destructured the envelope (the deterministic Intent route).
    /// Same rule, one place: a Chat-bound turn with a known sender owes that
    /// Chat its card and its reference row.
    ///
    /// The round coordinate is derived here so that no transport can hand the
    /// loop a turn whose round was computed a second way.
    pub fn new(
        chat_session_id: String,
        inbound_message_id: &str,
        from: &str,
        has_human: Option<bool>,
    ) -> Self {
        Self {
            chat_session_id,
            // D2: no inbound id means no round to name — an empty id must never
            // reach `mint_round_id`, which would fall back to a random UUID and
            // hand the surface a round the loop never opens.
            round_id: match inbound_message_id.trim() {
                "" => String::new(),
                raw => crate::rounds::mint_round_id(raw),
            },
            answerable_by: (!from.is_empty()).then(|| from.to_string()),
            has_human,
        }
    }

    /// Whether this turn may park on a human, and where: the
    /// `(chat_session_id, round_id, answerable_by)` triple §5.5.4 asks for.
    /// `None` when there is nobody to answer — a run that stops waiting on a
    /// human nobody can reach is worse than one that ends.
    ///
    /// ADR-106 D8: the card is no longer a Chat row (it is the round's
    /// `card` in the projection), so the middle term names the **round** the
    /// card belongs to rather than the row it used to thread under. The gate
    /// itself is unchanged: a chat this turn is accountable to, and a
    /// participant who can answer it.
    pub fn card_target(&self) -> Option<(&str, &str, &str)> {
        if self.chat_session_id.is_empty() {
            return None;
        }
        self.answerable_by
            .as_deref()
            .map(|a| (self.chat_session_id.as_str(), self.round_id.as_str(), a))
    }

    /// The Chat this turn's reference row belongs to, if any.
    pub fn echo_chat(&self) -> Option<&str> {
        (!self.chat_session_id.is_empty()).then_some(self.chat_session_id.as_str())
    }
}

/// The facts a turn's Chat reference row needs that the loop cannot derive
/// (ADR-106 D3).
///
/// The row names the **round**, and only the loop knows which round it is about
/// to open — D2 makes a turn that answers a parked card continue the round it
/// was parked on, so the round is not a function of this turn's inbound id.
/// Writing the row where the round is minted is therefore the only placement
/// that cannot name a round that does not exist; the transport hands the loop
/// the three facts it has no view on, beside the surface it already sets.
#[derive(Debug, Clone, Default)]
pub struct TurnReference {
    /// The Chat row this turn answers. Empty when the transport's inbound id
    /// names no Chat row: a turn composed against the execution surface has no
    /// question there to thread under, and pointing the reference at a row that
    /// does not exist would render a block under a phantom message. A group `@`
    /// fan-out is the third case — its per-target id matches no durable row
    /// either, which is why the Intent arm prefers `in_reply_to`.
    pub trigger_message_id: String,
    /// Did this delivery resolve a session that was just created? Only the
    /// routing decision knows, so a transport that does not route leaves it
    /// `None` — it must not be faked as `Some(false)`, or every locally composed
    /// turn would read as "continues an existing task".
    pub opened_new: Option<bool>,
    /// The round's one-line name, which the Chat list and the block header read
    /// (D13). Written once with the row and never rewritten — reading the
    /// projection to name a list item is the ADR-096 red line.
    ///
    /// Deliberately NOT named `assignment_brief`: that is the envelope's
    /// HIGH-TRUST field (the PM wake-up playbook, see
    /// [`extract_assignment_brief`]), and for a peer origin this slot carries the
    /// *sender's own* summary instead ([`reference_name`]). One name, one producer
    /// — the two facts sharing a key is how a playbook nearly reached a chat log.
    pub round_name: Option<String>,
    /// The PM task this round works on, when the delivery came in through a task
    /// conversation. The block renders it as a task chip (D13), which is why it
    /// is durable on the reference rather than read back from the projection.
    /// Derived from the Gateway's routing contract (`task:{project}:{task}`),
    /// never from a peer's params — a chip naming a task the sender invented
    /// would be a claim the Chat has no way to check.
    pub task_id: Option<String>,
}

/// How a turn ended, in the Chat-facing vocabulary: the status of the
/// terminal row (ADR-100 D9) and of the §7.1 write-back.
///
/// One field on the loop carries it and both terminal writers read it. Before
/// it existed the loop held a `turn_aborted` bool for the echo while the
/// write-back context held a parallel `outcome` string mutated from inside
/// the loop — two mirrors of one fact, and the transport that only had the
/// bool echoed a turn paused *waiting for a human* as `aborted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Aborted,
    /// Ended waiting on a human answer (§5.5.4): the open card is the answer
    /// entry and the answer opens the next turn.
    Waiting,
    /// The loop returned `Err` — the terminal row is `failed`.
    Failed,
}

impl TurnOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnOutcome::Completed => "completed",
            TurnOutcome::Aborted => "aborted",
            TurnOutcome::Waiting => "waiting",
            TurnOutcome::Failed => "failed",
        }
    }
}

/// The request a responder turn must answer. Built when a `request`
/// IntentMessage starts a turn; consumed at turn end for the write-back.
///
/// This is the §7.1 **response channel** to the initiator agent — after
/// ADR-099 it means exactly that and nothing more. It used to double as the
/// "is there a Chat" signal for card writing and the human-awaiting gate;
/// those obligations moved to [`TurnSurface`], which every transport carries.
#[derive(Debug, Clone)]
pub struct IntentTurnContext {
    /// The request's stable message id — the write-back coordinate.
    pub request_id: String,
    /// Sender instance (the response is addressed back to it).
    pub from: String,
    pub action: String,
}

impl IntentTurnContext {
    pub fn new(request_id: &str, from: &str, action: &str) -> Self {
        Self {
            request_id: request_id.to_string(),
            from: from.to_string(),
            action: action.to_string(),
        }
    }
}

/// Design 27 §5.4.5 / plan D-C/C1: an inbound Intent kind decides **two
/// orthogonal things**, and conflating them is what made 1:1 user-agent
/// chat unusable (a human message judged as a `request` stripped
/// `ask_user_question` from the turn). The decision is a pure lookup in
/// the [`acowork_core::intent::TurnOrigin`] policy table — no inference
/// here, the producer stamped it.
///
/// Returns `(responder_role, needs_write_back)`:
///
/// - `"request"` (and empty = legacy request): the turn must answer an
///   agent-to-agent request — responder role (R1 tool trim, R3 no
///   human-awaiting pause) **and** a system write-back at turn end. The
///   write-back goes through `POST /api/intents/{request_id}/response`
///   even when no Chat session is bound (mailbox-only degradation), so a
///   request always owes a write-back.
/// - `"user_message"`: a human chat message routed through the Intent
///   transport (§5.4.4) — an ordinary initiator turn with the full tool
///   set, but the turn owes its Chat session a terminal row. Needs a
///   `message_id` to answer against; without one there is no coordinate.
/// - `"pm_task"`: a PM task dispatch — also human input (same role and
///   write-back as `user_message`), but routed on the task key, not by
///   the Chat Router.
/// - `"response"`: the answer arriving back at the initiator — an
///   ordinary turn with **no** write-back (the response row is already
///   durable; the Gateway appended it when the answer was posted).
pub fn turn_kind_semantics(kind: &str, message_id: &str) -> (bool, bool) {
    let origin = acowork_core::intent::TurnOrigin::parse(kind);
    (origin.policy().responder, origin.write_back(message_id))
}

/// Extract the PM dispatch's wake-up playbook from an envelope's params.
///
/// `assignment_brief` is a HIGH-TRUST field: it lands in the receiving
/// session's system block (Block A), above everything the model reads.
/// Only the Gateway's task-dispatch pipeline — a *system* producer — may
/// fill it, so the value is honored for [`TurnOrigin::PmTask`] and empty
/// for every other origin. Without this gate any agent holding
/// `intent:send` could smuggle persistent system-level instructions into
/// a peer's trust slot through the arbitrary JSON of an `intent_send`
/// params object (design 27 §5.4.5 trust boundary; the same rule the
/// Gateway enforces on its side by stripping the key from agent-supplied
/// params).
pub fn extract_assignment_brief(kind: &str, params: &Value) -> String {
    brief_for_system_block(
        kind,
        params
            .get("assignment_brief")
            .and_then(|v| v.as_str())
            .unwrap_or_default(),
    )
    .to_string()
}

/// The origin gate itself: the brief may reach the system block only on a
/// [`TurnOrigin::PmTask`] turn. Applied at extraction (inbound envelope)
/// and again at consumption (the stored Intent message), so neither layer
/// can hand the system block to an untrusted producer.
pub fn brief_for_system_block<'a>(kind: &str, brief: &'a str) -> &'a str {
    if acowork_core::intent::TurnOrigin::parse(kind) == acowork_core::intent::TurnOrigin::PmTask {
        brief
    } else {
        ""
    }
}

/// The one-line name a round's reference row carries for the **human** surfaces
/// (ADR-106 D13: the Chat list preview and the block header, neither of which
/// may read a projection to name a task — ADR-096).
///
/// Two producers of a human name, one gate:
///
/// - an **agent** origin has a display projection ([`IntentDisplay`]) — its
///   `summary` is the sentence the sender's own envelope yields (a title, else
///   the prose it opened with, else the action), and that is what names the
///   round. Without this arm a peer round had no human name at all, so every
///   surface fell through to the envelope body and the block read
///   `[intent:2f3c…:analyze] {"title":…}`.
/// - a **human** origin keeps [`reference_brief`]'s rule (the dispatch text for a
///   PM task, the router's brief for a chat turn, never the wake-up playbook).
///
/// The envelope field both arms read is the one field with two meanings: for a
/// [`TurnOrigin::PmTask`] delivery it is the wake-up playbook — a HIGH-TRUST
/// system-slot value that must never be painted on a chat surface or persisted
/// with the chat log (design 27 §5.4.2 / §5.4.5) — while for every other origin
/// it is the sender's own brief. The split is the producer's, not this
/// function's invention: `acowork-pm` builds the two facts separately —
/// `build_dispatch_message` ("what the project group chat shows") and
/// `build_assignment_brief` ("the wake-up playbook … never persisted, never
/// displayed on a chat surface").
///
/// The failure this prevents, measured on a real project group: the playbook was
/// stamped onto the reference row, so the group's list preview read "You are
/// being woken to execute PM task …" and a block that had no projection read yet
/// printed the claim/submit instructions as the round's question.
pub fn reference_name(
    kind: &str,
    brief: &str,
    human_text: &str,
    display: Option<&IntentDisplay>,
) -> Option<String> {
    if let Some(summary) = display
        .map(|d| d.summary.trim())
        .filter(|s| !s.is_empty())
    {
        return Some(summary.to_string());
    }
    reference_brief(kind, brief, human_text)
}

/// The envelope-derived name of a row, for a turn with no human projection.
///
/// This is the pre-projection behaviour, kept as the fallback: [`reference_name`]
/// asks the display first and lands here when there is none (a human origin, or a
/// row written before the projection existed). The source is chosen by origin — a
/// dispatch is named by the human's own words, everything else by the brief the
/// sender addressed it with.
pub fn reference_brief(kind: &str, brief: &str, human_text: &str) -> Option<String> {
    // The source is chosen FIRST: a dispatch whose playbook is missing still has
    // its dispatch text, and that is what names the row. Testing `brief` for
    // emptiness before choosing would drop the only usable name.
    let source = if acowork_core::intent::TurnOrigin::parse(kind)
        == acowork_core::intent::TurnOrigin::PmTask
    {
        human_text
    } else {
        brief
    };
    // One line, bounded: this value is a row's durable name, not a body. The
    // dispatch text is title + description, and a description is arbitrarily
    // long — the same cap the display's `summary` uses, because both name a
    // round on the same surfaces.
    let collapsed = one_line(source);
    if collapsed.is_empty() {
        return None;
    }
    Some(capped_one_line(&collapsed, DISPLAY_SUMMARY_MAX_CHARS))
}

/// Render an Intent envelope as the user-role message the receiving model
/// sees. Shared by the mid-turn injection path (`loop_inbound`) and the
/// new-turn path (`session_task`) so both renderings stay identical.
#[allow(clippy::too_many_arguments)]
pub fn render_intent_message(
    from: &str,
    action: &str,
    params: &Value,
    message_id: &str,
    conversation_id: &str,
    source_chat_session_id: &str,
    repo: &str,
    kind: &str,
    in_reply_to: &str,
    status: &str,
) -> String {
    // Design 27 §3.2: the receiver sees the full envelope — reply
    // coordinates (conversation / source sessions) and the authorized
    // repo are part of the message, not ambient.
    //
    // §5.4.4: a human turn (`user_message`, or a `pm_task` dispatch —
    // both are people speaking) must reach the model as the person, not
    // an envelope. No reply coordinates either: the write-back is
    // system-driven from the envelope, not from anything the model reads
    // here. A `pm_task`'s assignment playbook is NOT part of this
    // message — the turn handler injects it into the system block.
    if !acowork_core::intent::TurnOrigin::parse(kind)
        .policy()
        .render_envelope
    {
        return match params.get("message").and_then(|v| v.as_str()) {
            Some(text) => text.to_string(),
            None => params.to_string(),
        };
    }
    let mut rendered = if kind == "response" || kind == "intent_response" {
        format!(
            "[intent_response:{from}:{action}] {params}
(in_reply_to: {in_reply_to}, status: {})",
            if status.is_empty() { "completed" } else { status }
        )
    } else {
        format!("[intent:{from}:{action}] {params}")
    };
    if !message_id.is_empty() {
        rendered.push_str(&format!("\n(message_id: {message_id})"));
    }
    if !conversation_id.is_empty() {
        rendered.push_str(&format!("\n(conversation_id: {conversation_id})"));
    }
    if !source_chat_session_id.is_empty() {
        rendered.push_str(&format!(
            "\n(source_chat_session_id: {source_chat_session_id})"
        ));
    }
    if !repo.is_empty() {
        rendered.push_str(&format!("\n(repo: {repo})"));
    }
    rendered
}

/// Longest Markdown body one display projection may carry.
///
/// The display text rides the turn's user row as `metadata.intent`, next to the
/// machine envelope that row body already holds, so it is storage the transcript
/// pays twice for. 16 KiB is the point where "the human's whole request is
/// readable" stops being the common case: an inbound `params` beyond it is
/// carrying payload (documents, code) rather than words, and the raw envelope is
/// still there behind the display for whoever needs it.
pub const DISPLAY_BODY_MAX_BYTES: usize = 16 * 1024;

/// Bound on the one-line `summary`. Matches `reference_brief`'s cap: both name
/// a round on the same surfaces, and two different lengths would let the row
/// and the block header shorten the same sentence differently.
pub const DISPLAY_SUMMARY_MAX_CHARS: usize = 240;

/// `params` keys that already carry the sender's words, in the order a reader
/// wants them. A peer's `params` object is free-form (`intent_send` schemas it
/// as "key-value data"), so this is a preference list, not a contract: the first
/// key present wins and everything else becomes the detail list.
const PROSE_KEYS: [&str; 6] = ["message", "request", "description", "prompt", "question", "text"];

/// `params` keys that name the work. Rendered as the body's heading, never
/// repeated in the detail list.
const TITLE_KEYS: [&str; 3] = ["title", "subject", "name"];

/// A value longer than this (or carrying a newline) is rendered as its own
/// block instead of an inline `- **key**: …` bullet. Long enough for a sentence
/// or a path, too short to swallow a paragraph into a bullet.
const INLINE_VALUE_MAX: usize = 200;

/// The **human** rendering of one inbound Intent envelope.
///
/// [`render_intent_message`] is model-facing prompt material: the reply
/// coordinates have to be in it (design 27 §3.2) and `params` prints as a single
/// line of compact JSON. Since ADR-106 that same string is also the durable user
/// row of the execution session — the *only* content every human surface has for
/// an agent-origin round — so a block used to read
/// `[intent:2f3c…:analyze] {"title":…}`.
///
/// This type is the other audience: a one-line `summary` and a Markdown `body`,
/// both derived from the envelope *facts* and never from the rendered prompt. It
/// is produced once, at the transport boundary, and carried as data
/// (`metadata.intent` on the user row → `RoundProjection.user.display`) so the
/// human surfaces stop having to interpret a machine format.
///
/// Human origins produce none: a `user_message` / `pm_task` turn's content is
/// already the person's own words.
///
/// Every field is `#[serde(default)]` so a partially written row (a producer
/// ahead of this reader, or a truncated write) reads leniently instead of failing
/// the whole round; emptiness is then judged once, by
/// [`IntentDisplay::from_metadata`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDisplay {
    /// Wire `kind` the display came from (`request` / `response`) — the only
    /// two origins that have one.
    #[serde(default)]
    pub origin: String,
    /// The action the sender named (`analyze`, `notify`, …), kept as the summary
    /// fallback for a payload with no words in it.
    #[serde(default)]
    pub action: String,
    /// One line for a block header, a list preview, a round row.
    #[serde(default)]
    pub summary: String,
    /// Markdown the human reads: the sender's own prose when it has any, plus
    /// the structured remainder as a detail list.
    #[serde(default)]
    pub body: String,
    /// `body` was cut at [`DISPLAY_BODY_MAX_BYTES`]; the machine envelope still
    /// holds the whole thing.
    #[serde(default)]
    pub truncated: bool,
}

/// Build the human rendering of an envelope (see [`IntentDisplay`]).
///
/// `None` for a human origin (no display layer needed) and for an envelope whose
/// `params` hold nothing printable at all — a surface that gets `None` keeps
/// today's behaviour, which is the honest degrade rather than an empty panel.
pub fn render_intent_display(kind: &str, action: &str, params: &Value) -> Option<IntentDisplay> {
    let origin = acowork_core::intent::TurnOrigin::parse(kind);
    if !origin.policy().render_envelope {
        return None;
    }
    let (body, headline) = intent_display_body(params);
    if body.trim().is_empty() {
        return None;
    }
    let summary = display_summary(headline.as_deref().unwrap_or_default(), action);
    let (body, truncated) = truncate_marked(&body, DISPLAY_BODY_MAX_BYTES);
    Some(IntentDisplay {
        origin: kind.trim().to_string(),
        action: action.trim().to_string(),
        summary,
        body,
        truncated,
    })
}

/// The Markdown body of an envelope's `params`, plus the phrase that should name
/// the round: its `title` if it has one, else the prose it opened with.
///
/// Shape, in the order a reader needs it: the sender's value for
/// [`TITLE_KEYS`] as a heading, then its value for [`PROSE_KEYS`] verbatim (it is
/// usually already Markdown — a `response`'s `message` is the peer's whole
/// answer), then every other key as a detail bullet. That last part is what
/// keeps a structured request (`workspace` / `scope` / `requirements`) from
/// collapsing into one undifferentiated line, which is the JSON problem in a
/// different costume.
fn intent_display_body(params: &Value) -> (String, Option<String>) {
    let Value::Object(map) = params else {
        // Non-object params are the whole payload: a string renders as itself,
        // anything structured renders as the machine form it is.
        return match params {
            Value::String(s) => (s.trim().to_string(), Some(s.trim().to_string())),
            Value::Null => (String::new(), None),
            other => (fenced_json(other, ""), None),
        };
    };

    let title_key = TITLE_KEYS
        .iter()
        .copied()
        .find(|k| non_empty_str(map.get(*k)).is_some());
    let prose_key = PROSE_KEYS
        .iter()
        .copied()
        .find(|k| non_empty_str(map.get(*k)).is_some());
    let title = title_key.and_then(|k| non_empty_str(map.get(k)));
    let prose = prose_key.and_then(|k| non_empty_str(map.get(k)));
    let headline = title.or(prose).map(str::to_string);

    let mut out = String::new();
    if let Some(title) = title {
        out.push_str(&format!("### {}\n\n", one_line(title)));
    }
    if let Some(prose) = prose {
        out.push_str(prose.trim());
        out.push_str("\n\n");
    }
    let mut details = String::new();
    for (key, value) in map {
        if Some(key.as_str()) == title_key || Some(key.as_str()) == prose_key {
            continue;
        }
        write_detail(&mut details, key, value);
    }
    out.push_str(&details);
    (out.trim_end().to_string(), headline)
}

/// One `params` entry as a detail-list item.
fn write_detail(out: &mut String, key: &str, value: &Value) {
    match value {
        Value::Null => {}
        Value::String(s) => {
            let s = s.trim();
            if s.is_empty() {
                return;
            }
            if s.len() <= INLINE_VALUE_MAX && !s.contains('\n') {
                out.push_str(&format!("- **{key}**: {s}\n"));
            } else {
                // A paragraph keeps its own line breaks: indenting by two
                // spaces makes it part of the bullet instead of a new block.
                out.push_str(&format!("- **{key}**:\n\n{}\n\n", indent(s, "  ")));
            }
        }
        Value::Number(_) | Value::Bool(_) => {
            out.push_str(&format!("- **{key}**: {value}\n"));
        }
        Value::Array(items) if items.iter().all(Value::is_string) && !items.is_empty() => {
            out.push_str(&format!("- **{key}**:\n"));
            for item in items {
                if let Some(s) = non_empty_str(Some(item)) {
                    out.push_str(&format!("  - {}\n", one_line(s)));
                }
            }
        }
        other => {
            out.push_str(&format!("- **{key}**:\n\n{}\n\n", fenced_json(other, "  ")));
        }
    }
}

/// A JSON value as a fenced block, every line prefixed with `indent` so the
/// fence stays inside the list item it belongs to.
fn fenced_json(value: &Value, indent_by: &str) -> String {
    let pretty = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let mut out = String::new();
    out.push_str(&format!("{indent_by}```json\n"));
    for line in pretty.lines() {
        out.push_str(indent_by);
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&format!("{indent_by}```"));
    out
}

fn indent(text: &str, by: &str) -> String {
    text.lines()
        .map(|line| if line.is_empty() { String::new() } else { format!("{by}{line}") })
        .collect::<Vec<_>>()
        .join("\n")
}

fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A row's durable name: one line, bounded, ellipsis-marked when cut.
///
/// Two producers shorten the same sentence for the same surfaces — the display's
/// `summary` and the envelope-derived row name ([`reference_brief`]) — so the cap
/// and the marker live here once. Two lengths would let a round row and its block
/// header shorten one sentence differently.
fn capped_one_line(text: &str, max_chars: usize) -> String {
    let folded = one_line(text);
    if folded.chars().count() <= max_chars {
        return folded;
    }
    let cut: String = folded.chars().take(max_chars - 1).collect();
    format!("{}…", cut.trim_end())
}

/// The one-line name of a round, cut from the phrase that should name it.
///
/// The caller passes the envelope's own headline (its `title`, else the prose it
/// opened with) — not the rendered body — so a payload that carries no words is
/// named by its `action` instead of by whichever machine key happened to sort
/// first.
fn display_summary(headline: &str, action: &str) -> String {
    let line = headline
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty() && !l.starts_with("```"));
    let folded = one_line(line.unwrap_or_default());
    if folded.is_empty() {
        return one_line(action);
    }
    capped_one_line(&folded, DISPLAY_SUMMARY_MAX_CHARS)
}

/// The `metadata.intent` object a turn's user row carries for the human surfaces.
///
/// Keyed under one member (`intent`) rather than spread over the row's metadata:
/// the row's metadata is a shared bag (`round_id`, `internal`, tool fields), and
/// a display that could be mistaken for one of those would change their meaning.
pub const META_KEY_INTENT_DISPLAY: &str = "intent";

/// Wrap a display for the user row's `metadata`.
///
/// Serialization cannot fail for this shape (four strings and a bool); the `Null`
/// arm keeps the signature total rather than panicking on a value only a schema
/// change could produce.
pub fn intent_display_meta(display: &IntentDisplay) -> Value {
    let mut map = serde_json::Map::new();
    if let Ok(value) = serde_json::to_value(display) {
        map.insert(META_KEY_INTENT_DISPLAY.to_string(), value);
    }
    Value::Object(map)
}

impl IntentDisplay {
    /// The display a row's `metadata` carries, when it carries a usable one.
    ///
    /// The **only** reader of [`META_KEY_INTENT_DISPLAY`] and the only place that
    /// decides what "usable" means: every display surface (round projection,
    /// session-message view, …) comes through here, so a change to the key or to
    /// the emptiness rule cannot reach one surface and miss another.
    ///
    /// `None` folds three honest absences into one answer — a human turn (no
    /// display layer), a row written before the projection existed, and a
    /// projection whose producer could not print anything. Every caller renders
    /// the stored row body for `None`: that is the fallback, not a blank panel.
    pub fn from_metadata(metadata: &Value) -> Option<Self> {
        let raw = metadata.get(META_KEY_INTENT_DISPLAY)?;
        let display: Self = serde_json::from_value(raw.clone()).ok()?;
        let usable = !display.body.trim().is_empty() || !display.summary.trim().is_empty();
        usable.then_some(display)
    }
}

/// POST a Runtime-owned chat row through the Gateway relay
/// (`POST /api/intents/chat-append`, plan 3.0).
/// Same source as the §5.5.1 write-back — the Runtime never speaks to
/// the user service directly. Idempotent by `meta.message_id`: a
/// redelivered turn produces no second row of the same kind.
///
/// Since ADR-106 the only row this writes is the [`KIND_EXECUTION_ROUND`]
/// reference, and that one is a **precondition** of the delivery (D3): the
/// caller writes it before handing the message to the execution session and
/// refuses the delivery when it fails. The row-shape rules (no body, agent
/// author, required coordinates) are enforced at the storage boundary, so a
/// malformed call fails loudly rather than writing a row the reader cannot
/// resolve.
#[allow(clippy::too_many_arguments)] // positional row shape; a struct here is boilerplate
pub async fn append_chat_row(
    instance_id: &str,
    gateway_host: &str,
    node_token: &str,
    chat_session_id: &str,
    kind: &str,
    to: &str,
    body: &str,
    meta: serde_json::Value,
) -> Result<(), String> {
    let url = format!("http://{gateway_host}:{}/api/intents/chat-append", gateway_http_port());
    let payload = serde_json::json!({
        "from": instance_id,
        "chat_session_id": chat_session_id,
        "kind": kind,
        "to": to,
        "body": body,
        "meta": meta,
    });
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(GATEWAY_WRITE_TIMEOUT)
        .json(&payload);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req.send().await.map_err(|e| format!("chat-append failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("chat-append rejected (HTTP {status}): {body}"))
    }
}

/// The one chat kind the Runtime writes (ADR-106 D1/D3).
///
/// Named here rather than spelled at each call site because the row's shape is
/// a storage-boundary contract: a site that typo'd the kind would write a row
/// the reader silently ignores, and the block would be missing with no error.
pub const KIND_EXECUTION_ROUND: &str = "execution_round";

/// The `meta` of an `execution_round` reference row (ADR-106 D3).
///
/// This is the only row a turn of work leaves in the Chat, and it carries no
/// content: the round's user row, its answer and the card it waits on are read
/// from the execution session's projection (D4). What stays durable here is the
/// reference — which round, in which session, on which instance — plus the two
/// presentation hints the Chat may render without a projection read
/// (`round_name` and `task_id`; D13 forbids reading the projection to paint a
/// list).
///
/// The idempotency key is the round's own natural key, `round:{round_id}`.
/// `round_id` is a deterministic function of the inbound message id (D2), so a
/// redelivery dedups by construction — which is why the prefix-key family the
/// retired mirror rows needed (`exec:` / `tr:` / `card:` / `resp-`) has no
/// remaining consumer.
///
/// The turn-only facts arrive on [`TurnReference`], the struct the transport
/// fills precisely because the loop cannot derive them — so adding a hint costs
/// a field there, not a positional argument here.
///
/// `opened_new` is `None` when the delivery did not resolve whether a session
/// was created (an execution-surface turn already had one). It must not be
/// faked as `Some(false)`: the block header says "new task" only when something
/// actually opened one.
pub fn execution_round_meta(
    round_id: &str,
    reference: &TurnReference,
    agent_session_id: &str,
    agent_instance_id: &str,
) -> serde_json::Value {
    let mut meta = serde_json::json!({
        "message_id": format!("round:{round_id}"),
        "round_id": round_id,
        "agent_session_id": agent_session_id,
        "agent_instance_id": agent_instance_id,
    });
    // A turn composed against the execution surface has no Chat row to answer:
    // the reference still names the round, it just has no thread above it.
    if !reference.trigger_message_id.is_empty() {
        meta["in_reply_to"] =
            serde_json::Value::String(reference.trigger_message_id.clone());
    }
    if let Some(new) = reference.opened_new {
        meta["opened_new"] = serde_json::Value::Bool(new);
    }
    // The durable key states the fact it holds: this is the round's name for
    // every origin, not the envelope's PM playbook (see `TurnReference::round_name`).
    if let Some(name) = reference
        .round_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta["round_name"] = serde_json::Value::String(name.to_string());
    }
    if let Some(task) = reference
        .task_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        meta["task_id"] = serde_json::Value::String(task.to_string());
    }
    meta
}

/// The `meta` of the Chat Router's own answer row (ADR-106 D12).
///
/// A router that replies without routing opened no round, so its answer is
/// agent speech *inside* the chat channel rather than a projection of an
/// execution session — which is why `agent_message` stays in the vocabulary at
/// all. `in_reply_to` is the id of the human row that triggered the turn, and
/// the Gateway delivers a chat turn under exactly that id, so the relay reads
/// the row as the answer to the request it is still waiting for.
///
/// The idempotency key is the request it answers: a redelivered turn leaves no
/// second answer row beside the first.
pub fn router_answer_meta(request_message_id: &str) -> serde_json::Value {
    serde_json::json!({
        "message_id": format!("reply:{request_message_id}"),
        "in_reply_to": request_message_id,
    })
}

/// The `meta` of a Chat choice card (ADR-106 D6 levels 3 and 5).
///
/// A card is the Chat's own question, so it rides the already-appendable
/// `system_notice` kind with its `meta` doing the talking: no new write entry,
/// no new kind, no whitelist change. Two notice types share this shape —
/// `session_choice` (several sessions could continue, which one?) and
/// `confirm_continue` (the router proposes one, yes or no?) — and both carry
/// `options` plus `allow_new_task`, because D6 requires the cheap escape to sit
/// beside every suggestion: "new task" is always offered, so a question can be
/// answered with one click and never traps the human in a context they did not
/// choose.
///
/// **The question is about the inbound message, so `in_reply_to` IS that
/// message's id** — one coordinate, taken from one argument. The card used to
/// carry a separate `in_reply_to` parameter, and every caller filled it with the
/// envelope's own answer-coordinate (`in_reply_to`), which on a fresh request is
/// the empty string: the card landed in the transcript answering nothing, the
/// surface could not find the message it asked about, and the send it belonged
/// to never settled. Two parameters for one fact is how that happened, so there
/// is now only one.
///
/// The idempotency key is `{notice_type}:{author}:{inbound message}`.
///
/// The key names three facts, not two: a redelivered turn re-asks one question
/// instead of stacking cards, and a group `@` fan-out stays one card **per
/// asking agent**. The row alone is not enough to key on — `@A @B` is one
/// human row delivered to two routers, each asking about its OWN candidates, so
/// a key of `{notice_type}:{row}` lets the first card written swallow the
/// second writer's question (`append_message` dedupes on `message_id` and the
/// second ask returns `already_written` as success). The human would answer one
/// card, one agent would work, and the other `@` would be silently unanswered.
pub fn session_choice_meta(
    notice_type: &str,
    author: &str,
    request_message_id: &str,
    options: Vec<serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "message_id": format!("{notice_type}:{author}:{request_message_id}"),
        "notice_type": notice_type,
        "in_reply_to": request_message_id,
        "options": options,
        "allow_new_task": true,
    })
}

/// POST a Chat choice card (see [`session_choice_meta`]).
///
/// Body is empty by contract: the row is a question whose text is the options,
/// and the surfaces localise `notice_type` rather than rendering a
/// Runtime-authored sentence into a human transcript.
#[allow(clippy::too_many_arguments)] // positional row shape; a struct here is boilerplate
pub async fn emit_session_choice(
    instance_id: &str,
    gateway_host: &str,
    node_token: &str,
    chat_session_id: &str,
    to: &str,
    request_message_id: &str,
    notice_type: &str,
    options: Vec<serde_json::Value>,
) -> Result<(), String> {
    let meta = session_choice_meta(notice_type, instance_id, request_message_id, options);
    append_chat_row(
        instance_id,
        gateway_host,
        node_token,
        chat_session_id,
        acowork_core::intent::chat_kinds::SYSTEM_NOTICE,
        to,
        "",
        meta,
    )
    .await
}

/// POST the responder's final output back through the Gateway write entry
/// (`POST /api/intents/{request_id}/response`). The Gateway verifies the
/// caller is the recorded target, appends the `intent_response` chat row,
/// and pushes the response to the initiator (§7.1).
///
/// Best-effort by contract: a failed write-back is logged by the caller;
/// the responder's turn is already over and must not re-run.
pub async fn post_intent_response(
    instance_id: &str,
    gateway_host: &str,
    node_token: &str,
    session_id: &str,
    ctx: &IntentTurnContext,
    outcome: TurnOutcome,
    body: &str,
) -> Result<(), String> {
    let url = format!(
        "http://{gateway_host}:{}/api/intents/{}/response", gateway_http_port(),
        ctx.request_id
    );
    let payload = serde_json::json!({
        "sender": instance_id,
        "sender_session_id": session_id,
        "target": ctx.from,
        "action": ctx.action,
        "params": { "message": body },
        "kind": "response",
        "in_reply_to": ctx.request_id,
        "status": outcome.as_str(),
    });
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(GATEWAY_WRITE_TIMEOUT)
        .json(&payload);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| format!("gateway write-back failed: {e}"))?;
    let status = resp.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = resp.text().await.unwrap_or_default();
        Err(format!("gateway rejected write-back (HTTP {status}): {body}"))
    }
}

/// Parse the ADR-046 attachment union an Intent envelope carries (design 27
/// §4.2.1, ADR-100).
///
/// Both transports that take a human Chat turn read the same `attached_items`
/// key the Gateway stamped into `params_json`: the execution surface's write
/// feeds `SessionMessage::ChatMessage`'s own arm, the Chat surface's turn the
/// `Intent` one.
/// One parse for both, so a reference never means two things on the wire.
///
/// An entry the Runtime cannot read is dropped, not fatal — the turn still
/// runs with what could be parsed (same rule the Chat-attachment pull applies).
pub fn attached_items_from_params(
    params: &Value,
) -> Option<Vec<acowork_core::protocol::AttachedItem>> {
    params
        .get("attached_items")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|d| {
                    serde_json::from_value::<acowork_core::protocol::AttachedItem>(d.clone()).ok()
                })
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<acowork_core::protocol::AttachedItem>| !v.is_empty())
}

/// Parse the reference parts an Intent envelope carries (ADR-107).
///
/// A reference is context the Runtime supplies and the human did not type —
/// today only the Chat Router's `exec_session_create(background)`, plus the
/// degraded route's chat tail. It rides `params.context_refs` for the same
/// reason attachments ride `params.attached_items`: the envelope has one
/// opaque `params` object, and a second transport field would need a proto
/// change for data only the Runtime produces and only the Runtime reads.
///
/// Empty and unparseable entries are dropped rather than failing the turn —
/// a producer bug degrades one block, it does not cost the human's message.
/// The same rule the attachment parse applies, and for the same reason.
pub fn context_refs_from_params(params: &Value) -> Vec<acowork_core::MessageRef> {
    params
        .get("context_refs")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|d| serde_json::from_value::<acowork_core::MessageRef>(d.clone()).ok())
                .filter(|r| !r.is_empty() && r.model_header().is_some())
                .collect()
        })
        .unwrap_or_default()
}

/// The model-facing message for a turn that carries reference parts: the
/// human's words FIRST, then one block per reference.
///
/// Order is the whole decision (ADR-107). The words lead because they are the
/// instruction, because a model reads the head of a message as the ask, and
/// because the router's previous brief-first composition is exactly what made
/// every chat-dispatched session open with the same sentence. A reference is
/// framing the Session needs to act — it follows, clearly delimited, and
/// labelled with [`acowork_core::MessageRef::model_header`] so it can never be
/// mistaken for something the human said.
///
/// `content` is returned untouched when there are no usable references, so a
/// turn without them keeps byte-identical prompt input to before this change.
pub fn render_user_turn(content: &str, refs: &[acowork_core::MessageRef]) -> String {
    let blocks: Vec<String> = refs
        .iter()
        .filter(|r| !r.is_empty())
        .filter_map(|r| r.model_header().map(|h| format!("{h}\n{}", r.content.trim())))
        .collect();
    if blocks.is_empty() {
        return content.to_string();
    }
    if content.trim().is_empty() {
        // A turn that carried no words (the human attached something, or the
        // router routed an attachment-only message). The blocks are the entire
        // message, so no empty user line is put in front of them — an empty
        // first line reads to a model as "the human said nothing", which is
        // true, but rendering it as a blank the blocks hang off is noise.
        return blocks.join("\n\n");
    }
    format!("{}\n\n{}", content.trim_end(), blocks.join("\n\n"))
}

/// Pull one Chat attachment pinned to the durable `user_message` row this
/// Intent answers (design 27 §4.2.1). The bytes travel over node-auth HTTP
/// through the Gateway (`GET /api/intents/chat-attachments/…`), never over
/// MQTT: the Intent envelope carries ids only. The Gateway re-reads the
/// blob from the user service, which verifies the row is addressed to
/// `target` and pins `attachment_id` — so an id the agent was never sent
/// resolves to 404, not to somebody else's file.
///
/// Returns `(filename, mime, bytes)`. Filename comes from the streamed
/// response's `Content-Disposition` (`filename*=UTF-8''` preferred, plain
/// `filename="…"` fallback, attachment id as last resort).
pub async fn pull_chat_attachment(
    gateway_host: &str,
    node_token: &str,
    target_instance: &str,
    chat_session_id: &str,
    attachment_id: &str,
    message_id: &str,
) -> Result<(String, String, Vec<u8>), String> {
    let url = format!(
        "http://{gateway_host}:{}/api/intents/chat-attachments/{target_instance}/{chat_session_id}/{attachment_id}/{message_id}", gateway_http_port()
    );
    let mut req = reqwest::Client::new()
        .get(&url)
        .timeout(GATEWAY_PULL_TIMEOUT);
    if !node_token.is_empty() {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = req.send().await.map_err(|e| format!("chat attachment pull failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("chat attachment pull rejected (HTTP {status})"));
    }
    let headers = resp.headers().clone();
    let mime = headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let filename = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_filename_from_content_disposition)
        .unwrap_or_else(|| attachment_id.to_string());
    let bytes = resp.bytes().await.map_err(|e| format!("chat attachment read failed: {e}"))?;
    Ok((filename, mime, bytes.to_vec()))
}

/// Extract the real filename from a `Content-Disposition` value, preferring
/// the percent-encoded `filename*=UTF-8''…` form (what the user service's
/// `content_disposition` emits for non-ASCII names).
fn parse_filename_from_content_disposition(cd: &str) -> Option<String> {
    if let Some(rest) = cd.to_ascii_lowercase().find("filename*=utf-8''").map(|i| &cd[i + 17..]) {
        let enc = rest.split(';').next().unwrap_or(rest).trim();
        return percent_decode(enc);
    }
    let (_, after) = cd.split_once("filename=\"")?;
    let name = after.split('"').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// Minimal percent-decoding (UTF-8) for the `filename*` form — the same
/// charset rules the user service's encoder keeps unescaped.
fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Truncate a response body to the byte ceiling with an explicit marker
/// (design 27 §5.5.1 length bound; the initiator asks for more with a new
/// request rather than the wire carrying an unbounded turn).
pub fn truncate_response_body(body: &str, max_bytes: usize) -> String {
    truncate_marked(body, max_bytes).0
}

/// Cut `body` at `max_bytes` and mark it: on a char boundary, and — when one is
/// close enough — on a line boundary, so a list or a fence is not severed
/// mid-token.
///
/// The ceiling is the caller's: a response body and a display body are bounded by
/// different numbers (design 27 §5.5.1 vs [`DISPLAY_BODY_MAX_BYTES`]). How a cut is
/// made, and what it is marked with, is not: two spellings of "there is more" would
/// be two things every surface had to know.
fn truncate_marked(body: &str, max_bytes: usize) -> (String, bool) {
    if body.len() <= max_bytes {
        return (body.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(nl) = body[..end].rfind('\n')
        && nl > max_bytes / 2
    {
        end = nl;
    }
    (format!("{}\n[truncated]", &body[..end]), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An envelope of the real wire shape (every field but from/action/params
    /// is `#[serde(default)]`), so a producer-side field rename cannot quietly
    /// become a test-side one.
    fn env(from: &str, chat: &str, has_human: Option<bool>) -> IntentEnvelope {
        serde_json::from_value(serde_json::json!({
            "from": from,
            "action": "chat",
            "params": { "message": "hi" },
            "message_id": "m-1",
            "source_chat_session_id": chat,
            "has_human": has_human,
        }))
        .unwrap()
    }

    /// Design 31 §5.2: a card's thread coordinate is the message it asks about.
    ///
    /// It used to be a separate parameter, and every caller filled it with the
    /// envelope's own answer-coordinate — the empty string on a fresh request.
    /// The card then answered nothing: the surface could not find the message
    /// the question belonged to, so the send kept waiting ("@X is working on
    /// it…") long after the question had landed, and the card's re-delivery had
    /// no row to answer. One fact, one parameter.
    #[test]
    fn a_session_choice_card_answers_the_message_it_asks_about() {
        let meta = session_choice_meta(
            "confirm_continue",
            "agent-a",
            "m-1",
            vec![serde_json::json!({"session_id": "s-1", "label": "task"})],
        );
        assert_eq!(meta["in_reply_to"], "m-1");
        // The idempotency key names the notice type, the asking agent and the
        // message: a redelivered turn re-asks one question instead of stacking
        // cards, and two agents asked the same row each keep their own card.
        assert_eq!(meta["message_id"], "confirm_continue:agent-a:m-1");
        assert_eq!(meta["allow_new_task"], true);
        assert_eq!(meta["options"].as_array().map(Vec::len), Some(1));
    }

    /// A group `@A @B` is ONE human row delivered to two routers. Each asks
    /// about its own candidates, so the store's `message_id` idempotency must not
    /// let the first card swallow the second question: `append_message` answers a
    /// repeated id with `already_written`, which its writer reads as success —
    /// the losing agent would return "asked", write nothing, and never be given a
    /// session while the human believes both were addressed.
    #[test]
    fn two_agents_asking_about_one_row_write_two_cards() {
        let options = || vec![serde_json::json!({"session_id": "s-1", "label": "task"})];
        let a = session_choice_meta("session_choice", "agent-a", "row-1", options());
        let b = session_choice_meta("session_choice", "agent-b", "row-1", options());
        assert_ne!(a["message_id"], b["message_id"]);
        // Both are threaded under the same human row — that is what makes each
        // one answerable (the `GET …/answer` lookup is by `in_reply_to`).
        assert_eq!(a["in_reply_to"], "row-1");
        assert_eq!(b["in_reply_to"], "row-1");
        // A redelivery of the same agent's ask is still one row.
        assert_eq!(
            a["message_id"],
            session_choice_meta("session_choice", "agent-a", "row-1", options())["message_id"]
        );
    }

    #[test]
    fn a_surface_names_the_round_derived_from_its_inbound_message() {
        // The root cause this pins: a turn written straight against the
        // execution surface arrived carrying content + row id only, and the
        // card writer's guard read `chat_writeback` — a field that path never
        // set. `ask_user_question` then wrote no Chat card
        // while the execution dialog worked fine. The surface is produced
        // from the envelope by BOTH transports, so the coordinates survive
        // the conversion.
        //
        // ADR-106 D2: the middle term is now the **round**, not the chat row it
        // threaded under. The chat holds no copy of this turn any more, so
        // nothing on the chat side is addressed by a row id any more.
        let s = TurnSurface::from_envelope(&env("user-1", "chat-9", Some(true)));
        assert_eq!(s.card_target(), Some(("chat-9", "r:m-1", "user-1")));
        assert_eq!(s.echo_chat(), Some("chat-9"));
        assert_eq!(s.has_human, Some(true));
    }

    #[test]
    fn request_turn_owes_its_card_to_the_pair_session_it_was_bound_to() {
        // Same producer for the Intent transport: the pair session the
        // Gateway bound (and stamped) is where the card lands.
        let s = TurnSurface::from_envelope(&env("inst-a", "chat-pair", Some(false)));
        assert_eq!(s.card_target(), Some(("chat-pair", "r:m-1", "inst-a")));
        // A human-free chat is the one fact that must block *blocking*
        // behaviour (§6.5 R3) while the card itself stays writable — the gate
        // reads `has_human`, not the presence of a write-back.
        assert_eq!(s.has_human, Some(false));
    }

    #[test]
    fn unbound_or_senderless_turn_writes_no_card() {
        // Mailbox-only degradation: no chat bound → nothing to answer.
        let mut e = env("inst-a", "", Some(true));
        e.source_chat_session_id = String::new();
        assert_eq!(TurnSurface::from_envelope(&e).card_target(), None);
        // A chat with no known sender would be a dead button → not written.
        let mut e = env("", "chat-9", Some(true));
        e.from = String::new();
        assert_eq!(TurnSurface::from_envelope(&e).card_target(), None);
    }

    #[test]
    fn a_bound_chat_with_an_answering_human_makes_a_standalone_card() {
        // A turn composed straight against the execution surface (ADR-100 D9
        // and the Rev. 6 note): the session's chat binding supplies the chat,
        // the writer's identity supplies who can answer, and there is no
        // triggering row — the card still lands, standalone.
        //
        // This is the regression that made a group chat blind to a question
        // the run had stopped on: the answer coordinate is the card's own id
        // (§5.5.4), so a missing thread must not suppress the card.
        //
        // The empty inbound id must NOT mint a round here: `mint_round_id("")`
        // yields a random id, which would name a second round for a message
        // that has not been delivered yet.
        let s = TurnSurface::new(String::new(), "", "user-1", None);
        assert_eq!(s.card_target(), None, "no chat yet — the binding fills it in");
        assert_eq!(s.round_id, "", "no inbound id — no round");
        // What `session_task` produces for a chat-bound session: the binding
        // supplies the chat, the write endpoint's scope supplies the answerer.
        let s = TurnSurface {
            chat_session_id: "chat-9".into(),
            ..s
        };
        assert_eq!(s.card_target(), Some(("chat-9", "", "user-1")));
        assert_eq!(s.echo_chat(), Some("chat-9"));
        // Unknown human presence fails OPEN (no `Some(false)`), so the
        // ask_user_question tool stays available and the card is written.
        assert_eq!(s.has_human, None);
    }

    #[test]
    fn waiting_is_not_aborted() {
        // Symptom 3 of the same root cause: the execution surface's arm had only a
        // `turn_aborted` bool, so a turn that ended *waiting for a human* was
        // echoed to the Chat as `aborted`. One outcome field, read by both
        // terminal writers, and the two states are distinct strings.
        assert_eq!(TurnOutcome::Waiting.as_str(), "waiting");
        assert_eq!(TurnOutcome::Aborted.as_str(), "aborted");
        assert_ne!(TurnOutcome::Waiting.as_str(), TurnOutcome::Aborted.as_str());
    }

    #[test]
    fn render_carries_envelope_coordinates() {
        let s = render_intent_message(
            "inst-a",
            "query",
            &serde_json::json!({"message": "hi"}),
            "m1",
            "c1",
            "cs1",
            "repo1",
            "request",
            "",
            "",
        );
        assert!(s.contains("[intent:inst-a:query]"));
        assert!(s.contains("(message_id: m1)"));
        assert!(s.contains("(conversation_id: c1)"));
        assert!(s.contains("(source_chat_session_id: cs1)"));
        assert!(s.contains("(repo: repo1)"));
    }

    #[test]
    fn render_response_variant() {
        let s = render_intent_message(
            "inst-b",
            "query",
            &serde_json::json!("answer"),
            "m2",
            "c1",
            "",
            "",
            "response",
            "m1",
            "completed",
        );
        assert!(s.contains("[intent_response:inst-b:query]"));
        assert!(s.contains("in_reply_to: m1"));
        assert!(!s.contains("source_chat_session_id"));
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "héllo wörld";
        let t = truncate_response_body(s, 4);
        assert!(t.ends_with("[truncated]"));
        assert!(s.starts_with(&t[..t.len() - "\n[truncated]".len()]));
        assert_eq!(truncate_response_body(s, 100), s);
    }

    #[test]
    fn both_bodies_are_cut_by_one_rule_and_wear_one_marker() {
        // The response body and the display body held two copies of "cut on a
        // char boundary, then write the marker". A surface must not be able to
        // tell which copy produced the text it is painting.
        let body = format!("{}{}{}", "a".repeat(30), "\n", "b".repeat(50));
        // The break sits past halfway through the window, so the cut moves back
        // to it instead of severing the line that follows.
        let expected = format!("{}{}[truncated]", "a".repeat(30), "\n");
        assert_eq!(truncate_response_body(&body, 40), expected);
        assert_eq!(truncate_marked(&body, 40), (expected, true));
        // A long first line has no usable break: the cut stays on the window
        // edge, on a char boundary (40 bytes lands inside the 14th `字`).
        let unbroken = "字".repeat(30);
        assert_eq!(
            truncate_response_body(&unbroken, 40),
            format!("{}{}[truncated]", "字".repeat(13), "\n")
        );
    }

    // ── Human display projection (M1) ──────────────────────────────────────

    #[test]
    fn a_response_displays_as_the_peers_own_markdown() {
        // The whole point of the response side: `params.message` is the peer's
        // answer, already Markdown. It must reach the human verbatim — no JSON
        // wrapper, no escaped `\n`.
        let answer = "# 结论\n\n- 第一项\n- 第二项\n\n| a | b |\n|---|---|\n| 1 | 2 |";
        let d = render_intent_display("response", "notify", &serde_json::json!({ "message": answer }))
            .expect("a response with a body has a display");
        assert_eq!(d.body, answer);
        assert_eq!(d.summary, "结论", "the heading names the round");
        assert!(!d.truncated);
        assert!(!d.body.contains('{'), "no machine envelope in the human body");
        assert_eq!(d.origin, "response");
        assert_eq!(d.action, "notify");
    }

    #[test]
    fn a_structured_request_keeps_its_shape() {
        // A peer request is free-form JSON. The reader still needs the prose to
        // lead and the structure to survive as a list — not one line of JSON,
        // and not a prose blob that drops `scope` / `requirements`.
        let d = render_intent_display(
            "request",
            "analyze",
            &serde_json::json!({
                "title": "审计 usecase 分层",
                "workspace": "E:/projects/tranxon/ACowork",
                "request": "检查 runtime 的 HTTP / MQTT 接口是否统一走 usecase。",
                "scope": ["HTTP handlers", "MQTT topics"],
                "requirements": ["给出 file:line 证据", "统计口径要可解释"],
                "budget_days": 3,
                "attached_items": [{ "kind": "file", "path": "a.rs" }],
            }),
        )
        .expect("a request with content has a display");
        assert!(d.body.starts_with("### 审计 usecase 分层\n\n"), "{}", d.body);
        assert!(d.body.contains("检查 runtime 的 HTTP / MQTT 接口是否统一走 usecase。"));
        assert!(d.body.contains("- **workspace**: E:/projects/tranxon/ACowork"));
        assert!(d.body.contains("- **scope**:\n  - HTTP handlers\n  - MQTT topics\n"));
        assert!(d.body.contains("- **requirements**:\n  - 给出 file:line 证据"));
        assert!(d.body.contains("- **budget_days**: 3"));
        // Structured leftovers ride a fence rather than an inline blob.
        assert!(d.body.contains("- **attached_items**:\n\n  ```json\n"));
        // The heading is not repeated as a detail bullet.
        assert!(!d.body.contains("- **title**"));
        assert_eq!(d.summary, "审计 usecase 分层", "a title names the round");
    }

    #[test]
    fn a_payload_with_no_words_falls_back_to_its_action() {
        // `intent_send` schemas `params` as arbitrary data: a sender can ship
        // only machine keys. The round still needs a name — the action is the
        // only human-meaningful fact left.
        let d = render_intent_display("request", "schedule", &serde_json::json!({ "at": "2026-11-09T10:00:00Z" }))
            .expect("a non-empty payload still displays");
        assert_eq!(d.summary, "schedule");
        assert!(d.body.contains("- **at**: 2026-11-09T10:00:00Z"));
    }

    #[test]
    fn an_eventless_payload_has_no_display() {
        // Nothing printable: a surface that gets `None` keeps today's rendering,
        // which is the honest degrade rather than an empty panel.
        assert_eq!(render_intent_display("request", "ping", &serde_json::json!({})), None);
        assert_eq!(render_intent_display("request", "ping", &serde_json::json!({ "x": null })), None);
    }

    #[test]
    fn human_origins_get_no_display_layer() {
        // A `user_message` / `pm_task` turn's content IS the person's words; a
        // display projection on top of it would be a second paraphrase.
        for kind in ["user_message", "pm_task"] {
            assert_eq!(
                render_intent_display(kind, "notify", &serde_json::json!({ "message": "hi" })),
                None,
                "{kind} must not get a display projection"
            );
        }
    }

    #[test]
    fn an_oversized_body_is_cut_on_a_line_boundary() {
        let long = "x".repeat(DISPLAY_BODY_MAX_BYTES);
        let body = format!("first line\n{}\nlast line", long);
        let d = render_intent_display("response", "notify", &serde_json::json!({ "message": body }))
            .expect("oversized still displays");
        assert!(d.truncated);
        assert!(d.body.ends_with("[truncated]"));
        assert!(
            d.body.len() <= DISPLAY_BODY_MAX_BYTES + "\n[truncated]".len(),
            "the marker is the only overflow: {} bytes",
            d.body.len()
        );
        assert_eq!(d.summary, "first line", "the summary is cut from what survives");
    }

    #[test]
    fn a_peer_round_is_named_by_its_display_summary() {
        // The reference row is the ONE name a chat surface may print without
        // reading a projection (D13). A peer round used to have no name here, so
        // the surfaces fell through to the envelope body.
        let display = render_intent_display(
            "request",
            "analyze",
            &serde_json::json!({ "title": "审计 usecase 分层", "request": "检查接口" }),
        );
        assert_eq!(
            reference_name("request", "", "[intent:…] {}", display.as_ref()).as_deref(),
            Some("审计 usecase 分层")
        );
        // No display (legacy / empty payload): today's behaviour, unchanged.
        assert_eq!(reference_name("request", "", "the brief", None).as_deref(), None);
        // A human origin still cannot be named by the high-trust playbook.
        assert_eq!(
            reference_name("pm_task", "claim it first", "修复分页", None).as_deref(),
            Some("修复分页")
        );
    }

    #[test]
    fn the_display_meta_is_keyed_under_one_member() {
        // The user row's metadata is a shared bag (`round_id`, `internal`, tool
        // fields). Nesting under one key is what keeps a display from being read
        // as one of those.
        let d = render_intent_display("response", "notify", &serde_json::json!({ "message": "done" }))
            .expect("display");
        let meta = intent_display_meta(&d);
        assert_eq!(meta[META_KEY_INTENT_DISPLAY]["summary"], "done");
        assert_eq!(meta[META_KEY_INTENT_DISPLAY]["body"], "done");
        assert_eq!(meta[META_KEY_INTENT_DISPLAY]["origin"], "response");
        assert_eq!(meta.as_object().unwrap().len(), 1);
    }

    #[test]
    fn the_writer_and_the_reader_round_trip() {
        // The producer serialises a display into the row's metadata and every read
        // model parses it back with `from_metadata`. Nothing else asserts that the
        // two agree, so a renamed field on either side would strand the projection
        // on every surface at once.
        let d = render_intent_display(
            "request",
            "analyze",
            &serde_json::json!({ "title": "审计 usecase 分层", "request": "检查接口" }),
        )
        .expect("display");
        let meta = intent_display_meta(&d);
        assert_eq!(IntentDisplay::from_metadata(&meta), Some(d));
    }

    #[test]
    fn a_partial_display_reads_leniently_and_an_empty_one_is_absent() {
        // A producer ahead of this reader (or a truncated write) must not fail the
        // whole round: missing fields default, and only an empty payload is judged
        // unusable.
        let partial = serde_json::json!({ META_KEY_INTENT_DISPLAY: { "summary": "审计" } });
        let read = IntentDisplay::from_metadata(&partial).expect("partial display still reads");
        assert_eq!(read.summary, "审计");
        assert_eq!(read.body, "");
        assert!(!read.truncated);

        // Whitespace is not content: an empty panel is worse than the stored row.
        let blank = serde_json::json!({ META_KEY_INTENT_DISPLAY: { "summary": " ", "body": "
" } });
        assert_eq!(IntentDisplay::from_metadata(&blank), None);

        // Absence has one answer, whether the key, the value or the shape is wrong.
        assert_eq!(IntentDisplay::from_metadata(&serde_json::json!({})), None);
        assert_eq!(
            IntentDisplay::from_metadata(&serde_json::json!({ META_KEY_INTENT_DISPLAY: null })),
            None
        );
        assert_eq!(
            IntentDisplay::from_metadata(&serde_json::json!({ META_KEY_INTENT_DISPLAY: "junk" })),
            None
        );
    }

    #[test]
    fn user_message_renders_as_plain_human_input() {
        // §5.4.4: a human 1:1 message routed through the Intent transport
        // must reach the model as the person speaking — no envelope tag,
        // no coordinates.
        let s = render_intent_message(
            "user-1",
            "chat",
            &serde_json::json!({"message": "帮我看看日志"}),
            "m1",
            "",
            "cs1",
            "",
            "user_message",
            "",
            "",
        );
        assert_eq!(s, "帮我看看日志");
    }

    #[test]
    fn kind_semantics_split_role_from_writeback() {
        // D-C/C1: responder role and write-back are orthogonal.
        // A `request` carries both; a human `user_message` owes a
        // write-back but must not take the responder role.
        assert_eq!(turn_kind_semantics("request", "m1"), (true, true));
        assert_eq!(turn_kind_semantics("", "m1"), (true, true));
        assert_eq!(turn_kind_semantics("user_message", "m1"), (false, true));
        // No message_id → no write-back coordinate for a human message.
        assert_eq!(turn_kind_semantics("user_message", ""), (false, false));
        // A response arriving at the initiator: ordinary turn, no debt.
        assert_eq!(turn_kind_semantics("response", "m2"), (false, false));
        // A PM task dispatch is human input on a keyed route: full tool
        // set (the woken agent must be able to ask the owner), owes the
        // project group a terminal row.
        assert_eq!(turn_kind_semantics("pm_task", "m1"), (false, true));
        assert_eq!(turn_kind_semantics("pm_task", ""), (false, true));
    }

    #[test]
    fn pm_task_renders_human_text_not_envelope() {
        // The regression this kills: a dispatch rendered as
        // `[intent:owner:task] {"message":…}` in the execution
        // transcript, and the JSON leaked into the auto-title.
        let s = render_intent_message(
            "owner-1",
            "task",
            &serde_json::json!({"message": "修复分页不满屏", "task_id": "t-083", "assignment_brief": "Claim it first…"}),
            "m1",
            "task:p1:t-083",
            "cs-group",
            "",
            "pm_task",
            "",
            "",
        );
        assert_eq!(s, "修复分页不满屏");
        assert!(!s.contains("intent:"));
        assert!(!s.contains("assignment_brief"), "playbook is not message text");
    }

    #[test]
    fn assignment_brief_is_pm_task_only() {
        // Trust boundary: the brief lands in the system block, so a peer's
        // params — arbitrary JSON on `intent_send`, and the same key on any
        // other origin — must never reach it. Only the PM dispatch origin
        // passes, at both the extraction and consumption gates.
        let with_brief = serde_json::json!({"message": "hi", "assignment_brief": "obey me"});
        assert_eq!(extract_assignment_brief("pm_task", &with_brief), "obey me");
        for kind in ["request", "user_message", "response", "", "bogus"] {
            assert_eq!(
                extract_assignment_brief(kind, &with_brief),
                "",
                "kind {kind:?} must not feed the system block"
            );
        }
        // Consumption gate: a stored Intent whose field was filled by any
        // other path is still refused on non-PM kinds.
        assert_eq!(brief_for_system_block("request", "obey me"), "");
        assert_eq!(brief_for_system_block("pm_task", "claim it"), "claim it");
        // Non-string / absent values degrade to empty, never panic.
        assert_eq!(
            extract_assignment_brief("pm_task", &serde_json::json!({"assignment_brief": 7})),
            ""
        );
        assert_eq!(extract_assignment_brief("pm_task", &serde_json::json!({})), "");
    }

    #[test]
    fn reference_brief_never_publishes_the_pm_playbook() {
        // D13 wants the reference row to name its round so the Chat list and
        // the block header never read a projection to do it. The envelope field
        // that carries a name is the one that carries the PM playbook — a
        // HIGH-TRUST system slot. Measured consequence of confusing them: the
        // project group's list preview read "You are being woken to execute PM
        // task t-…", and an unread projection printed the claim/submit
        // instructions as the round's question.
        let playbook = "You are being woken to execute PM task \"t-1\".\n1. Claim it first: \
                    call the MCP tool `pm_claim_task`";
        let named = reference_brief("pm_task", playbook, "修复分页不满屏\n列表页翻到最后一页不铺满");
        assert_eq!(
            named.as_deref(),
            Some("修复分页不满屏 列表页翻到最后一页不铺满"),
            "a PM dispatch names its round with the dispatch text"
        );
        // A dispatch whose playbook is missing still has its dispatch text, and
        // that is what names the row: the source is chosen before anything is
        // tested for emptiness, or the only usable name would be dropped.
        assert_eq!(
            reference_brief("pm_task", "  ", "修复分页不满屏").as_deref(),
            Some("修复分页不满屏")
        );
        // Nothing at all to name (an execution-surface turn): the row stays
        // unnamed rather than named by an empty string.
        assert_eq!(reference_brief("pm_task", "", ""), None);
        assert_eq!(reference_brief("user_message", "   ", "words"), None);
        // Another origin keeps the brief it was given — that one is the router's
        // human-facing line, not a system slot.
        assert_eq!(
            reference_brief("user_message", "继续之前的任务：分页", "words").as_deref(),
            Some("继续之前的任务：分页")
        );
        // A row name is one line and bounded; the description is arbitrarily
        // long and must not become a body.
        let long = format!("标题\n{}", "很长的描述".repeat(200));
        let capped = reference_brief("pm_task", playbook, &long).unwrap();
        assert!(!capped.contains('\n'), "one line: {capped:?}");
        assert!(capped.chars().count() <= 240, "bounded: {}", capped.chars().count());
        assert!(capped.starts_with("标题 很长的描述"));
    }

    #[test]
    fn tool_visibility_splits_role_from_answerability() {
        // §6.5 R1 + §5.5.4 (plan 2b.3): `intent_send` is hidden by role,
        // `ask_user_question` by a *known* human-free session. Unknown
        // fails open.
        use crate::agent::loop_context::hidden_tools_for_turn as h;
        // Responder in a pure agent-agent pair: both gone.
        assert_eq!(h(true, Some(false)), ["intent_send", "ask_user_question"]);
        // Responder in a group with humans: only the nested-Intent gate.
        assert_eq!(h(true, Some(true)), ["intent_send"]);
        // Responder, answerability unknown (older Gateway): role only.
        assert_eq!(h(true, None), ["intent_send"]);
        // Initiator turn in a human-free pair (response arrival): the card
        // tool goes, the ability to continue the conversation stays.
        assert_eq!(h(false, Some(false)), ["ask_user_question"]);
        // Human 1:1 / mention turns: nothing hidden.
        assert!(h(false, Some(true)).is_empty());
        assert!(h(false, None).is_empty());
    }

    /// The transport's hand-over, built from the one or two facts a test cares
    /// about (the shape `execution_round_meta` reads). `task_id` arrives here
    /// already parsed out of the delivery's conversation id, which is the
    /// Gateway's routing contract — not a params value a peer could set.
    fn reference(
        trigger: &str,
        opened_new: Option<bool>,
        brief: Option<&str>,
        task_id: Option<&str>,
    ) -> TurnReference {
        TurnReference {
            trigger_message_id: trigger.to_string(),
            opened_new,
            round_name: brief.map(str::to_string),
            task_id: task_id.map(str::to_string),
        }
    }

    #[test]
    fn execution_round_meta_is_a_contentless_reference_keyed_by_the_round() {
        // ADR-106 D3: the row carries the round's natural key and the
        // coordinates the block is read with — nothing else. No body, no status,
        // no mirrored answer text (that would be the old echo back again).
        let m = execution_round_meta(
            "r:m-42",
            &reference("m-42", Some(true), None, None),
            "20261023_131156_a3bb7d",
            "inst-a",
        );
        assert_eq!(m["message_id"], "round:r:m-42");
        assert_eq!(m["round_id"], "r:m-42");
        assert_eq!(m["in_reply_to"], "m-42");
        assert_eq!(m["agent_session_id"], "20261023_131156_a3bb7d");
        assert_eq!(m["agent_instance_id"], "inst-a");
        assert_eq!(m["opened_new"], true);
        assert!(m.get("body").is_none());
        assert!(m.get("status").is_none());

        // A reuse is a fact worth keeping distinct from "unknown": only the
        // routing decision can say a session was not newly opened.
        assert_eq!(
            execution_round_meta("r:m-43", &reference("m-43", Some(false), None, None), "s-1", "inst-a")
                ["opened_new"],
            false
        );
        // And `None` must not degrade to `false` — the block header only says
        // "new task" when something actually opened one.
        assert!(
            execution_round_meta("r:m-44", &reference("m-44", None, None, None), "s-1", "inst-a")
                .get("opened_new")
                .is_none()
        );
    }

    /// The producer's half of the cross-process contract, and the regression for
    /// 2026-11-09: the row built here is refused by the user service's registry
    /// unless every key is declared there, and a refused reference row kills the
    /// whole turn (it is a precondition of delivery, ADR-106 D3). The two crates
    /// share no type, so the shape is pinned once in `acowork-core` and this test
    /// is exact equality — not a subset check, which would let a key the registry
    /// has never seen escape into production again.
    #[test]
    fn execution_round_meta_is_the_registered_golden_row() {
        let produced = execution_round_meta(
            "r:1f0c",
            &reference("m-1", Some(true), Some("修复分页不满屏"), Some("t-a4f21dbc")),
            "20260101_000000_abcdef",
            "inst-1",
        );
        let golden: serde_json::Value =
            serde_json::from_str(acowork_core::intent::chat_kinds::EXECUTION_ROUND_META_SAMPLE)
                .expect("the fixture must be JSON");
        assert_eq!(
            produced, golden,
            "the row this builder writes must be the row the registry is asserted against \
             (acowork_core::intent::chat_kinds::EXECUTION_ROUND_META_SAMPLE)"
        );
    }

    #[test]
    fn execution_round_meta_omits_the_coordinates_a_delivery_does_not_have() {
        // An execution-surface turn has no Chat row above it: no thread, rather
        // than a thread pointing at a row that does not exist.
        let no_trigger = execution_round_meta("r:msg-7", &reference("", None, None, None), "s-2", "inst-a");
        assert!(no_trigger.get("in_reply_to").is_none());

        // The PM hints are the only presentation the Chat may render without a
        // projection read (D13); a blank one is absent, not empty — an empty
        // string would render an empty task name or an empty chip.
        let with_brief = execution_round_meta(
            "r:m-45",
            &reference("m-45", Some(true), Some("  draft the release note  "), Some("t-083")),
            "s-1",
            "inst-a",
        );
        assert_eq!(with_brief["round_name"], "draft the release note");
        assert_eq!(with_brief["task_id"], "t-083", "the chip's coordinate");
        for blank in [Some(""), Some("   "), None] {
            let m = execution_round_meta("r:m-46", &reference("m-46", None, blank, blank), "s-1", "inst-a");
            for key in ["round_name", "task_id"] {
                assert!(
                    m.get(key).filter(|v| v.as_str() != Some("")).is_none(),
                    "a blank {key} must not be written as a task name: {m}"
                );
            }
        }
    }

    #[test]
    fn the_task_chip_is_read_off_the_routing_contract() {
        // D13 / D12: the reference row is now the only place a PM dispatch's
        // task identity reaches the Chat, so the parse of the Gateway's own
        // conversation key is the trust gate — a peer's `task_id` param is not.
        assert_eq!(
            acowork_core::intent::task_id_from_conversation("task:p-1:t-083").as_deref(),
            Some("t-083")
        );
        assert_eq!(acowork_core::intent::task_id_from_conversation("chat-9"), None);
        assert_eq!(acowork_core::intent::task_id_from_conversation("task:p-1:"), None);
        assert_eq!(acowork_core::intent::task_id_from_conversation("task:"), None);
    }

    // ── ADR-107: reference parts ─────────────────────────────────────

    fn refs(value: serde_json::Value) -> Vec<acowork_core::MessageRef> {
        context_refs_from_params(&serde_json::json!({ "context_refs": value }))
    }

    #[test]
    fn reference_parts_are_parsed_off_the_envelope() {
        let parsed = refs(serde_json::json!([
            { "kind": "chat_background", "content": "the user asked to commit" }
        ]));
        assert_eq!(
            parsed,
            vec![acowork_core::MessageRef::chat_background("the user asked to commit")]
        );
        // No key, an empty array, or a non-array all mean "no references" —
        // the turn must still run.
        for absent in [
            serde_json::json!({}),
            serde_json::json!({ "context_refs": [] }),
            serde_json::json!({ "context_refs": "nope" }),
        ] {
            assert!(context_refs_from_params(&absent).is_empty());
        }
    }

    #[test]
    fn unusable_reference_parts_are_dropped_not_fatal() {
        // An unreadable entry, an empty body and an unknown kind are all
        // "nothing this build can render": the good entry still arrives, and
        // a bad one never makes the human's turn fail.
        let parsed = refs(serde_json::json!([
            { "kind": "chat_background", "content": "   " },
            { "kind": "some_future_kind", "content": "future body" },
            { "content": "kind missing" },
            { "kind": "chat_background", "content": "kept" }
        ]));
        assert_eq!(
            parsed,
            vec![acowork_core::MessageRef::chat_background("kept")]
        );
    }

    #[test]
    fn the_words_lead_and_the_reference_follows() {
        // The reason this function exists (ADR-107). The old composition was
        // `{brief}\n\n[user message]\n{words}`: the router's framing came
        // first, so the first `user` row — and therefore the auto-title —
        // opened with the same sentence in every dispatched session.
        let out = render_user_turn(
            "提交本地修改",
            &[acowork_core::MessageRef::chat_background(
                "The user asked to commit the pending work.",
            )],
        );
        assert!(
            out.starts_with("提交本地修改"),
            "the human's words must open the message: {out}"
        );
        let background = out.find("[background:").expect("the block is labelled");
        assert!(
            background > "提交本地修改".len(),
            "the reference must follow the words: {out}"
        );
        assert!(out.contains("The user asked to commit the pending work."));
    }

    #[test]
    fn a_turn_without_references_is_byte_identical_to_the_words() {
        assert_eq!(render_user_turn("hello  ", &[]), "hello  ");
        assert_eq!(
            render_user_turn("hello", &[acowork_core::MessageRef::chat_background("  ")]),
            "hello",
            "an empty reference must not add a header with nothing under it"
        );
    }

    #[test]
    fn a_wordless_turn_is_the_reference_blocks_alone() {
        // Attachment-only, or a routed message with no typed text: the blocks
        // are the message. No empty first line is put in front of them.
        let out = render_user_turn(
            "  ",
            &[acowork_core::MessageRef::chat_background("tail of the chat")],
        );
        assert!(out.starts_with("[background:"), "{out}");
        assert!(out.ends_with("tail of the chat"), "{out}");
    }
}
