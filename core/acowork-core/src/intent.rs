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

/// The task half of a task conversation id, or `None` for any other
/// conversation. Inverse of [`task_conversation_id`].
///
/// ADR-106 D13: the round reference row renders a task chip, and reading the id
/// out of the Gateway's own routing contract is what lets the Runtime do that
/// without trusting a `task_id` in a peer's arbitrary params JSON.
pub fn task_id_from_conversation(conversation_id: &str) -> Option<String> {
    let rest = conversation_id.strip_prefix(TASK_CONVERSATION_PREFIX)?;
    let task = rest.split_once(':').map(|(_, task)| task)?;
    (!task.is_empty()).then(|| task.to_string())
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

/// Machine tokens for a **live** delivery alert (ADR-105 D5, revision
/// 2026-11-06).
///
/// One spelling for both ends of the internal alert call: the Gateway reports
/// the code, the user service stores and serves it, and the surfaces decide the
/// wording. Only the codes the Gateway itself invents live here — the rest are
/// the delivery entry's own `error_code`s (`target_not_installed`,
/// `not_authorized`, a publish failure's code), passed through verbatim.
pub mod chat_alert_codes {
    /// The addressee accepted the delivery and then produced nothing before the
    /// response deadline: the answer never came (design 27 §5.5.1). Formerly the
    /// sweeper's `fallback-<message_id>` receipt.
    pub const NO_RESPONSE: &str = "no_response";
}

/// The Chat content vocabulary (ADR-106 D1).
///
/// Exactly four kinds may exist in a Chat transcript, and three processes touch
/// one — the Gateway writes the human's row, the Runtime writes the round's
/// reference row and its own speech, the user service stores rows and rejects
/// what it cannot name. The spelling is stated once here instead of as string
/// literals in three crates, because a typo across that boundary is not a
/// compile error: it is a row that silently stops rendering.
pub mod chat_kinds {
    /// A human utterance. The Gateway is its only writer, through the chat
    /// write entries — never through `chat-append`.
    pub const USER_MESSAGE: &str = "user_message";
    /// Agent speech inside the chat channel: the Chat Router's own answer,
    /// which is the only row it still writes (ADR-106 D12). An agent-to-agent
    /// exchange reaches the chat as a round reference instead, and it is never
    /// a copy of execution output.
    pub const AGENT_MESSAGE: &str = "agent_message";
    /// ADR-106 D3: one round of execution work, recorded as a bodyless
    /// reference row. The Runtime writes it at the turn boundary and it is the
    /// only exec-side fact that reaches the Chat transcript.
    pub const EXECUTION_ROUND: &str = "execution_round";
    /// A delivery the chat owed the human and could not make, or a limit it
    /// hit: `delivery_failed` / `budget_exhausted` / `truncated` /
    /// `unavailable`. Centered status line, never a bubble. The same kind
    /// carries the addressing questions (ADR-106 D6): `session_choice` when an
    /// `@` left several sessions it could continue, `confirm_continue` when the
    /// router proposes one — those rows are empty-bodied and carry `options` in
    /// `meta`, so the surfaces own the wording and the answer is an ordinary
    /// second-level `@` rather than a new reply mechanism.
    pub const SYSTEM_NOTICE: &str = "system_notice";

    /// The kinds `POST /api/intents/chat-append` admits (ADR-106 D1's split of
    /// producers): everything a cross-process writer may own. `user_message` is
    /// deliberately absent — a human utterance is written by the Gateway's own
    /// chat entries, after participant and content gates an append cannot run.
    pub fn is_appendable(kind: &str) -> bool {
        matches!(kind, EXECUTION_ROUND | SYSTEM_NOTICE | AGENT_MESSAGE)
    }

    /// Notice types that answer a row already in the transcript, and therefore
    /// MUST carry `meta.in_reply_to` = that row's `message_id`.
    ///
    /// The coordinate is not decoration: it is what makes every projected state
    /// ("was this message answered", "is this card still open", "which block
    /// does this reply belong to") recomputable from the log alone (design 31
    /// §4, hard constraint 3). A receipt that cannot be threaded is a receipt a
    /// reader has to guess at, so the write entry refuses it instead of storing
    /// an orphan — the failure this list exists to prevent was a card whose
    /// `in_reply_to` was the (empty) answer-coordinate of a fresh request rather
    /// than the request itself.
    ///
    /// `budget_exhausted` / `truncated` / `unavailable` are deliberately absent:
    /// they report a limit of the channel, and the request they name never
    /// became a row (design 31 §9 pending 3 tracks that evidence gap).
    /// `delivery_failed` has **no live writer** since the 2026-11-06 revision of
    /// ADR-105 D5 (a failed send is an in-memory alert, not a row) but stays
    /// listed: the type is still what the rows already in old transcripts are,
    /// and a vocabulary that forgets it would make those rows unreadable.
    pub const THREADED_NOTICES: &[&str] = &[
        "delivery_failed",
        "interaction_resolved",
        "session_choice",
        "confirm_continue",
    ];

    /// ADR-106 D6 levels 3 and 5: an addressing question to the human. Card
    /// rows are empty-bodied — the options ARE the question — so a card without
    /// `meta.options` is not a card but an unanswerable line, and the surfaces
    /// render exactly that: a localized sentence and no buttons.
    pub const CARD_NOTICES: &[&str] = &["session_choice", "confirm_continue"];

    /// The `system_notice` types that report a **limit of the channel** rather
    /// than an outcome of a row: the request they name never became a
    /// transcript row, so there is no `message_id` they could thread to and
    /// they are deliberately absent from [`THREADED_NOTICES`] (design 31 §9-3
    /// tracks the resulting evidence gap). They still settle a turn — the human
    /// was told something happened — which is why [`is_answer_row`] counts them.
    ///
    /// `delivery_failed` is NOT here even though it reports a failure: the row
    /// it owed does exist, so it carries `in_reply_to` like any other receipt
    /// and belongs to [`THREADED_NOTICES`].
    ///
    /// This list exists so the write contract's vocabulary is closed: every
    /// notice type a live writer can emit is threaded (a card is a threaded
    /// question) or a limit. `dev/ci.sh run_chat_projection_redline` fails on a
    /// notice literal that appears in neither, which is the only way a new one
    /// gets to skip the threading contract by accident.
    pub const LIMIT_NOTICES: &[&str] = &["budget_exhausted", "truncated", "unavailable"];

    /// The `system_notice` types the transcript renders as **nothing**
    /// (ADR-106 D14, design 27 §5.5.5). v3.1 removed the writers, not the rows:
    /// old transcripts still carry routing milestones and card-resolution
    /// receipts, and hiding them at read time is what keeps history readable
    /// without narrating a decision the reference row now owns.
    ///
    /// One spelling, three readers: the two surfaces paint them as nothing, and
    /// [`is_answer_row`] must not count one as the outcome that settled a turn —
    /// a row nobody can see cannot be the fact that made a message visibly
    /// handled.
    /// `delivery_failed` joined the set with the 2026-11-06 revision of ADR-105
    /// D5: the yellow receipt is no longer written (the failure is a live alert
    /// on the human's row), so the rows history holds are the retracted kind —
    /// the same "not delivered" line that stayed on screen long after a retry
    /// succeeded. Hiding them at read time is what stops that.
    pub const RETIRED_NOTICES: &[&str] = &[
        "turn_dispatched",
        "turn_started",
        "turn_routed",
        "router_fallback",
        "interaction_resolved",
        "delivery_failed",
    ];

    /// True when a row of this kind settles the row it threads to (ADR-106 D9):
    /// the round reference (the turn was picked up), the router's own answer, or
    /// a live receipt / addressing card. This is the answer to "did anything
    /// happen to that message" and it is asked in exactly one place — the chat
    /// read model (`acowork-user`) — never by a surface.
    pub fn is_answer_row(kind: &str, notice_type: &str) -> bool {
        match kind {
            EXECUTION_ROUND | AGENT_MESSAGE => true,
            SYSTEM_NOTICE => !RETIRED_NOTICES.contains(&notice_type),
            _ => false,
        }
    }

    /// True when this notice answers a specific row (see [`THREADED_NOTICES`]).
    pub fn is_threaded_notice(notice_type: &str) -> bool {
        THREADED_NOTICES.contains(&notice_type)
    }

    /// True when this notice is an addressing card (see [`CARD_NOTICES`]).
    pub fn is_card_notice(notice_type: &str) -> bool {
        CARD_NOTICES.contains(&notice_type)
    }

    /// The `meta` of a live `execution_round` row with every optional coordinate
    /// present — the golden row of the one chat row a Runtime writes (ADR-106 D3).
    ///
    /// Three processes meet on this shape and none of them can see the others'
    /// types: the Runtime's `execution_round_meta` **produces** it, the user
    /// service's `IntentMeta` is the write-time **registry** that refuses any key
    /// it does not declare (design 31 §5.2), and the two surfaces read it back by
    /// key. The registry and the producer live in crates that must not depend on
    /// each other, so nothing checks them against one another — and a rename on
    /// the producing side that misses the registry is not a refactor but an
    /// outage: the reference row is a *precondition* of delivery (ADR-106 D3), so
    /// a refused key kills the turn before the agent reads a word. That is
    /// exactly what `round_name` did on 2026-11-09: renamed across the Runtime,
    /// the Desktop and the Mobile app, and not registered on the fourth process,
    /// which greeted every PM dispatch with `422 unregistered chat row meta keys`.
    ///
    /// So the row is pinned here, once, and asserted from both ends: the Runtime's
    /// builder must produce **exactly** this object, and this object must
    /// deserialize into `IntentMeta` leaving `unregistered` empty. Adding a key
    /// therefore costs a deliberate edit to the fixture, the builder and the
    /// registry in one change — which is the point of putting it here instead of
    /// in either end.
    ///
    /// Values are representative, not meaningful; only the key set and the value
    /// *types* are contractual (`opened_new` is a bool, the coordinates strings),
    /// because a type mismatch is refused by serde just as fatally as a missing
    /// declaration.
    pub const EXECUTION_ROUND_META_SAMPLE: &str = r#"{
        "message_id": "round:r:1f0c",
        "round_id": "r:1f0c",
        "agent_session_id": "20260101_000000_abcdef",
        "agent_instance_id": "inst-1",
        "in_reply_to": "m-1",
        "opened_new": true,
        "round_name": "修复分页不满屏",
        "task_id": "t-a4f21dbc"
    }"#;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Design 31 §5.2: the notice classes the chat write entry enforces are
    /// vocabulary, not a store-local list — three crates have to agree on which
    /// notice answers a row and which one is only a channel fact.
    #[test]
    fn chat_notice_classes_are_registered_once() {
        assert!(chat_kinds::is_card_notice("session_choice"));
        assert!(chat_kinds::is_card_notice("confirm_continue"));
        assert!(chat_kinds::is_threaded_notice("delivery_failed"));
        assert!(chat_kinds::is_threaded_notice("interaction_resolved"));
        // A card is a question about a row, so it is threaded by definition.
        for card in chat_kinds::CARD_NOTICES {
            assert!(chat_kinds::is_threaded_notice(card), "{card} must be threaded");
        }
        // A limit is a fact about the channel: the request it names never
        // became a row, so it cannot be threaded (design 31 §9-3).
        assert!(!chat_kinds::is_threaded_notice("budget_exhausted"));
        assert!(!chat_kinds::is_card_notice("delivery_failed"));
    }

    /// The golden `execution_round` row is a fixture two other crates assert
    /// against, so it must at least be the JSON object it claims to be: an
    /// unparseable or accidentally-empty fixture would let both ends "pass"
    /// against nothing.
    #[test]
    fn the_execution_round_fixture_is_a_non_trivial_object() {
        let row: serde_json::Value = serde_json::from_str(chat_kinds::EXECUTION_ROUND_META_SAMPLE)
            .expect("the fixture must parse as JSON");
        let map = row.as_object().expect("the fixture must be a JSON object");
        // The three coordinates a reference row cannot exist without
        // (`check_write_contract`) plus the name it carries for the surfaces.
        for key in ["message_id", "round_id", "agent_session_id", "round_name"] {
            assert!(map.contains_key(key), "the fixture must carry {key}");
        }
        assert!(
            map.values().all(|v| !v.is_null()),
            "a fixture value may be absent from a live row, never null: null is a type mismatch"
        );
    }

    /// Design 31 §7 P3: the write-contract classes are exhaustive and
    /// disjoint on the axis they describe, so
    /// `run_chat_projection_redline` can decide "registered or not" by asking
    /// whether a writer's notice literal appears in one of them.
    ///
    /// Two axes, not one: `THREADED_NOTICES` / `LIMIT_NOTICES` say **what the
    /// write entry must enforce**, while [`RETIRED_NOTICES`] says **how the
    /// transcript renders and settles** a row. They overlap on purpose —
    /// `interaction_resolved` is retired as a live writer and rendering yet
    /// still threaded, because a row of that type that does get written must
    /// stay resolvable. Asserting them as one partition would force a false
    /// choice between two true statements.
    #[test]
    fn chat_notice_write_classes_are_disjoint() {
        let threaded = chat_kinds::THREADED_NOTICES;
        let cards = chat_kinds::CARD_NOTICES;
        let limits = chat_kinds::LIMIT_NOTICES;
        for card in cards {
            assert!(threaded.contains(card), "{card} is a card, so it is threaded");
        }
        for limit in limits {
            assert!(
                !threaded.contains(limit),
                "{limit} reports a channel limit; its referent is not a row, so it cannot be threaded",
            );
            assert!(!cards.contains(limit), "{limit} is both a card and a limit");
        }
    }

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
