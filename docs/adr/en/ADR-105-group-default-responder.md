# ADR-105: Group Default Responder — an un-@'d group message goes to one declared agent

- **Number**: ADR-105
- **Status**: Accepted, **partially superseded by ADR-106** (the evidence slot of D4 and the chat-row shape of D5)
- **Date**: 2026-11-01
- **Deciders**: ACowork architecture group
- **Related**: ADR-098 (unified standard group model), ADR-099 (chat-session LLM router), ADR-100 (chat event log); design 27 §5.3.1 / §5.4.1 / §5.5

> **Supersession notice (2026-11-03)**: [ADR-106](./ADR-106-chat-execution-reference-projection.md) D14 supersedes this ADR's **D4** (`auto_routed_to` as the pending-row evidence slot) and the **chat-row shape of D5** (the absence-inference behind the wait line is deleted; `delivery_failed` survives as a channel fact). D1 / D2 / D3 / D6 / D7 — the default responder itself, the four write gates, the lifecycle, and not introducing a group-level LLM router — remain fully in force: the default responder still works on the `mentions` delivery path today, only the trace it leaves no longer decides how a surface paints a wait line.

## Summary

A group message that @-mentions nobody is no longer archive-only. The chat-session row gains one field, `default_responder` (one agent participant of that group), and the Gateway delivers un-addressed group messages to it; a group with no such field configured **keeps the original behaviour** (archive only). The client no longer branches its send path on "has @ / has none": every group send goes through `POST /api/intents/mentions`, and `targets: []` changes meaning from "invalid request" to "the server decides".

## Context

- **Fact: an un-@'d group message is a dead letter.** An ordinary `user_message` in a group is written to the Chat session and not delivered (§5.3.1), so the most common interaction of all — an agent just spoke and the human answers it — produces nothing: no delivery, no reply, no notice. The human must re-send with an @. One-to-one chats do not have this problem, because every input there enters the agent's Chat Router (ADR-099).
- **Fact: an existing client-side branch hid it.** `chatSessionStore.sendGroupText` used to branch on `targets.length > 0`: with @ it called `/api/intents/mentions`, without @ it called the archive-only `POST /api/chat-sessions/{id}/messages`. The archive entry does not deliver by design, so "the human answered the agent" was, on disk, a row that woke nobody.
- **Fact: no configuration means no choice — it does not mean an LLM is needed.** There are two possible sources for "who should take this message": (a) one declared receiver per group, a human decision made once and reused; (b) an LLM routing every un-addressed message from a public group chat session owned by no agent. Option (b) turns the topology from "one group message → one target" into "one group message → one routing step → one target", on top of a per-agent group-bound session set, and makes the outcome unpredictable for the human.
- **Fact: the chosen agent already owns routing.** Once it receives the message it still runs its own Chat Router (ADR-099) to decide between answering and handing the turn to a running execution session. This ADR therefore only has to name the first receiver; it must not implement semantic routing a second time.

## Decision

### D1 — The default responder is one field on the session row, not a new session

`ChatSession` gains `default_responder: Option<String>` (an agent instance id). `None` = un-addressed messages stay archive-only (the historical behaviour, and the only sane default for a group of humans).

No new session, no new kind, no new message type. Semantically it is a role on a member, at the same level as `owner`.

### D2 — Write path: owner-or-admin, and the target must be a current typed agent participant

`set_default_responder` (`core/acowork-user/src/chat_sessions.rs`) checks four gates inside the session lock:

1. the kind is a group (`user_group` / `project_group`; `user_user` / `user_agent` / `agent_agent` answer `not_a_group_session`);
2. the caller is a participant (`not_a_participant`);
3. the caller is owner-or-admin (reuses `can_govern_group`, the same gate as `chat.md`, rename and transfer);
4. the target is a participant with `identity == Agent` (`ParticipantIdentity::Agent`). An untyped legacy row is not evidence and is not guessed.

`PUT /api/chat-sessions/{id}/responder` only forwards; `GET` is open to every participant and returns `editable`, so the frontend never re-derives the governance rule.

### D3 — Delivery: `targets: []` means "the server decides", not "malformed"

`POST /api/intents/mentions` drops the non-empty `targets` check. With an empty list:

- the session's `default_responder` is set and still a live agent participant → deliver to it;
- otherwise → archive, answering `{"routed": "archived", "results": []}`.

The `routed` field lets a caller tell "nobody was asked" from "everybody was asked and all failed" — both used to be silent.

### D4 — Auto-routing is an independent fact on the row: `meta.auto_routed_to`

The delivered row is still a broadcast row (`to: ""`); `meta.mentions` keeps meaning "the names the human typed", and the automatic target goes into `meta.auto_routed_to`.

Rationale: `mentions` renders as @ chips, so folding an automatic target in would put a name in the sender's mouth. Meanwhile the wait line ("working on it") is keyed on "did this human row wake an agent", and it needs that evidence slot — `auto_routed_to` is it. The storage layer must declare the field explicitly or it is dropped at the serialization boundary (the same trap as `meta.task_id`).

> **Overturned by ADR-106 D9 / D14**: keying the wait line on `auto_routed_to` reads "cannot see it" as "still running" — a restart, an offline agent or a failed projection each leaves a permanent wait line. The field is still written (it records where the message went) but drives no rendering. The wait line was then shrunk to "this surface's own POST that has not returned yet" (`sending`), and that version was too short as well: a POST answers in about 150 ms while that round's first row can land a minute later. The rule in force is in [ADR-106, Revision (2026-11-03)](./ADR-106-chat-execution-reference-projection.md): a claim on this device's own send, keyed by the row id its response carried, settled by the first row that answers it, with a 300 s deadline as the backstop.

### D5 — A failed delivery must be visible

When `wake_and_deliver` fails, the Gateway appends a `system_notice`: `notice_type: "delivery_failed"`, `in_reply_to` naming the original `user_message`, `error_code` carrying the reason, and an **empty body**. Wording belongs to each surface (the same convention as `router_fallback`: the producer writes the row, the surface owns the prose). A failure to write the notice itself is a warn only and never affects the response.

Two frontend rules:

- `ChatNoticeEventRow` renders the row by `notice_type` — it must paint even with an empty body, since the row exists precisely so that the failure stops being silent;
- `chatTurnState.answeredAfter` treats a `delivery_failed` notice bound to the same trigger as the end of the turn: the receipt is the outcome, and keeping the wait line up would be a louder version of the same lie.

> **Status after ADR-106 D14**: the semantics ("a failure must be visible") and the `delivery_failed` channel row are kept — the first rule still holds. The host of the second rule, `chatTurnState.answeredAfter`, was deleted with P6. As of 2026-11-03 the receipt takes back a smaller and more honest role: it is not an inference-closing signal but the **settlement row** for this device's send claim — on the group surface a failed delivery is visible and ends the wait at the same time, because those are one rule (a 1:1 failure arms no claim, so there is nothing to settle).

### D6 — Lifecycle: the role leaves with its member

- `remove_participant`: if the removed id is the `default_responder`, the field is cleared;
- `sync_group` (PM roster sync): if `default_responder` is absent from the new roster, the field is cleared;
- pre-delivery re-validation: `SessionInfo::live_default_responder()` requires, on every read, that the target is still in `participants` and is an agent; otherwise the group reads as "no responder".

An uninstalled agent that is still a member **deliberately passes** re-validation: the delivery then fails loudly with `target_not_installed` and writes the `delivery_failed` row (D5) instead of being silently downgraded to archive-only.

### D7 — No LLM routing in this ADR (phase two is out of scope)

"An LLM decides which agent should get an un-addressed message" is explicitly excluded:

- a group default responder plus that agent's own Chat Router already covers the overwhelming majority of "who is the human talking to";
- a public group session would need a context owned by no agent to maintain group state, which conflicts with ADR-098's single kind-ed container;
- an unpredictable routing decision is harder to explain than silence, not easier.

If it is ever needed, it should be a per-message judgement **inside the receiving runtime** (answer_self / handoff / stay_silent), not a new public session.

## Rejected alternatives

| Alternative | Why rejected |
| --- | --- |
| Public group chat session + LLM routing (option b) | One more session to maintain next to every agent's group-bound sessions; the topology and the ownership of state get complicated (D7) |
| Hard-code the PM agent as a platform-wide default receiver | The receiver is a fact about the group, not a platform constant; groups of humans and non-PM groups would get an agent forced on them |
| Keep the client-side "has @ / has none" branch and only add a backend fallback | Routing is the server's business; a client branch means every new surface (mobile) branches again, and the branch is what caused the bug |
| Fold the automatic target into `meta.mentions` to reuse the existing wait-line logic | It would paint an @ chip the human never typed; `mentions` must keep meaning "what the human said" |
| Log a failed delivery instead of writing a notice | The human still sees "nothing happened", which is the very symptom this ADR removes |

## Impact

- **Data**: one optional field on `chat_sessions`, `serde(default)`, no migration; likewise `meta.auto_routed_to`.
- **Protocol**: `/api/intents/mentions` accepts empty `targets` (a 400 before) and answers with `routed`. An old client never sends an empty list, so its behaviour is unchanged.
- **UI**: the group settings panel gains a "Default responder" card (read open to every participant, write owner-or-admin); a group send has exactly one entry point.
- **Rollback**: clearing `default_responder` restores the full historical behaviour; both the field and `routed` are safe to ignore.