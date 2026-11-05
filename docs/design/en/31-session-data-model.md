# Session Two-Layer Data Model (Persisted Terminal State + Runtime Superset)

> Version: v1.6 | Updated: 2026-11-20
>
> Status: model baseline (under review). This document fixes only *which layer a fact belongs to, who owns it, and who may derive it* — not API shapes or implementation shapes.
>
> Changelog
> - v1.6 (card interaction): the projection gains `question.answered_session`. An answered card is no longer a row of disabled buttons on screen but a receipt — "handed to <session>" — and the session that took the answer is the one half of the delivery fact the projection did not carry: `answered` only says *that* it was answered, so "where did my words go" had to be re-derived by scanning another row's coordinates. The criterion is unchanged ("an `execution_round` reference row exists for that message"); the name is the first such row in log order that carries an `agent_session_id`. When no round can name it (D3 forbids inventing one) the field stays absent and the receipt falls back to "answered" / "started a new task". The single-candidate row is now built by `chat_candidates::proposal_option` — title, router reason and status from one place, and no status claimed when the session is not in this turn's candidate list.
> - v1.5 (P4 landed): **answering a card belongs to the service too.** The answer to a `session_choice` / `confirm_continue` card is "the human's own sentence, re-delivered under a new address", and each frontend used to rebuild it with its own `buildCardAnswer` from its own loaded window — so a card whose message had scrolled away rendered enabled buttons that did nothing (`if (!answer) return`, silently). `acowork-user` now owns the rule (`chat_card::answer`: fetch the origin row by coordinate, refuse a card that is already answered, refuse a caller the card was not addressed to) and `GET /api/chat-sessions/{id}/messages/{message_id}/answer` hands back the words, the attachments, the already-pinned `refs` and the original row id; the surface folds in only the click (which session, or new work). Two shape defects left over from the previous round were fixed in the same pass: `delivery.target` became `targets` (a group `@` fan-out produces one round per target, and a `find` that took the first told "where it went" incompletely) and the sidecar append path now writes its `#v2` header (a file born headerless was judged v1 on every restart and fully re-scanned).
> - v1.4 (P3 landed): the red lines are in CI — `dev/ci.sh run_chat_projection_redline` asserts four things: the write door is still bolted (the registry fallback plus the contract check inside the single write entry), the public page still projects while the agent-facing page stays raw (both directions, so nobody "unifies" them), no surface builds an aggregate predicate over `in_reply_to` (`some/filter/every/new Set/includes`; a by-coordinate `.find` of one known row stays legal — that is exactly the presentation join §6 leaves to surfaces), and every `notice_type` literal a writer emits is already registered in `chat_kinds`. Implementing it exposed a missing class: `budget_exhausted` / `truncated` / `unavailable` report a **channel limit**, and the request they name never became a row, so no coordinate can exist — `LIMIT_NOTICES` now says so explicitly (`delivery_failed` is not one of them: the row it owed does exist). `chat_notice_write_classes_are_disjoint` pins the classes apart on the *write-contract* axis and records that `RETIRED_NOTICES` is an orthogonal axis (rendering and settling).
> - v1.3 (P2 landed): the single derivation point is implemented — `acowork-user` grew a `chat_view` read model, every row of the public `GET /api/chat-sessions/{id}/messages` now carries `delivery` (`answered` / `answered_by` / `target` / `question`), and all state derivation is deleted from both frontends. Three boundaries were confirmed while implementing and are recorded in §6.1: (1) derivation must be able to read *only the lines it asks about*, so the store gained a reverse `in_reply_to` edge (sidecar v2, answering rows fetched by byte span — no transcript scan); (2) `question.answered` is judged by "a round reference row exists for the message the card names" — the card is never its own answer, or a card would declare itself answered the moment it was written; (3) the agent-facing `chat_history_read` still returns bare rows, because that page is fed to a model and budgeted per row, where a derived field is neither rendered nor free. One upgrade defect was also fixed, forced out by a test: after a v1 sidecar was discarded the reconcile took the *incremental append* branch, so the rewritten rows carried no version marker and every process start re-scanned the whole log — an untrusted sidecar now forces "full re-scan + rewrite".
> - v1.2 (P1 landed): the write-entry contract is implemented — `IntentMeta` is the payload registry (`options` / `allow_new_task` / `refs` / `error_code`), undeclared keys are caught by `serde(flatten)` (preserved verbatim on read instead of dropped a second time), and the single write entry `check_write_contract` refuses them with 422; thread coordinates are enforced for reply-class rows (`agent_message` / `interaction_request` / threaded notices), and a card additionally needs `options` and `allow_new_task`. One correction came out of the facts: `execution_round` is **not** a reply-class row — it is an *answer or origin* row, and a turn dispatched from a PM task, a group `@` fan-out or the execution surface has no chat row above it, so an empty coordinate *is* that fact; forcing one would hang the block under a phantom message. Two more drops of the same class surfaced while implementing: `refs` (the ADR-106 revision's quadruples — written by the Gateway all along, dropped by the store, which is why the source chips never rendered) and `error_code` (a `delivery_failed` receipt's reason code). Both are registered now.
> - v1.1 (post-review): two over-reaching conclusions of v1.0 are withdrawn — (1) `turn` / `question` terminal state is **no longer settled into `rounds.sqlite`** (per ADR-106 D5 that file is a **disposable cache**; `messages.jsonl` is the chat's only truth, so putting authoritative state there would break R1 outright); (2) **no new `message.address` field** (the delivery address is already carried by the round reference row; a new field would be a second representation). The deficiency is therefore re-characterised from "missing storage" to "missing write contract + missing single derivation point". The review's other gaps are also filled: chat data ownership, the runtime-state holder, the relation to ADR-106 D3's "answered" criterion, and the affected write call sites.
>
> Related: [ADR-106](../../adr/en/ADR-106-chat-execution-reference-projection.md) (chat/execution reference projection), [ADR-084](../../adr/en/ADR-084-user-standalone-process.md) (the user domain, including chat persistence and API, lives in `acowork-user`), [ADR-082](../../adr/en/ADR-082-memory-storage-sqlite-vector-fts.md) (session metadata and index sidecar), [ADR-099](../../adr/en/ADR-099-chat-session-llm-router.md) (Chat Router), Design 15 (conversation persistence), Design 27 (Intent messaging and group chat), Design 29 (Chat Session context orchestration)

## 1. Purpose and scope

This document addresses exactly one class of problem: **does the backend data model hold an explicit, recomputable fact to answer every state the frontend has to display?**

Architectural rule: the backend is the single source of truth; the frontend only does interaction and display. In data-model terms this yields two criteria:

1. **Completeness** — every state the frontend displays must be answerable by the backend; the frontend must not derive business state itself.
2. **Single semantics** — one business fact has exactly one representation. Two representations inevitably drift, and once they drift nobody can tell which is right.

**Ownership**: Chat Session persistence and API belong to `acowork-user` ([ADR-084](../../adr/en/ADR-084-user-standalone-process.md)); execution Session persistence and API belong to its Runtime (metadata sidecar per ADR-082). This document discusses only those two internal models and the reference relation between them.

Out of scope: concrete API shapes, UI components, routing algorithms, payload key inventories.

## 2. Starting facts

### 2.1 The two session kinds are already isomorphic — the shape just never became a rule

| Role | Chat Session (`acowork-user`) | Execution Session (Runtime) |
|---|---|---|
| Immutable event log (only truth) | `messages.jsonl` (+ `lines.index` line offsets) | `conversations/{sid}.jsonl` |
| Round state projection | `rounds.sqlite` (**disposable cache**, ADR-106 D5) | `{sid}.jsonl.rounds` (round terminal markers) |
| Session metadata | `session.json` (participants / kind / title / config / counters) | session meta store (initiator / visibility / chat binding) |
| Disposable index | `search.sqlite` | — |
| Attachments | `files/` | workspace file tree |

Conclusion: both are "**append-only log + state projection + metadata**". They differ in content and writer, not in shape. The two-layer split therefore applies to both, and neither should grow its own variant.

### 2.2 Gaps in the current model (re-characterised by fact vs derivation)

| UI question | Where the fact belongs | Today |
|---|---|---|
| has this message been answered | the transcript (an answer row pointing at it) — **already a durable fact** | the fact exists, but the frontend matches rows on the client |
| which message does this card ask about, may I answer, is it answered | the question row + the answer row — **already durable facts** | facts exist, but writers leave coordinates incomplete and every consumer re-derives semantics |
| has this card expired | no such semantics | chat cards never expire; only the delivery layer has a response deadline |
| which execution session did the message go to | the round reference row's coordinates — **already a durable fact** | the fact exists, the frontend reverse-infers it from `in_reply_to` |
| the payload a message carries | one-shot terminal state on the log row | **no registry, no boundary validation**, undeclared keys silently dropped |

So the deficiency is not "missing storage"; it is three things: **an incomplete write contract** (coordinates/payload not always written), **derivation scattered across consumers**, and **no authoritative payload shape**.

### 2.3 Who holds the runtime state

- Execution Session: has a resident host (SessionTask); running state lives in memory, the file is the last checkpoint.
- Chat Session: **no resident session host**. One routing turn is stateless; its runtime state is held by the **Gateway** — the delivery table (mailbox: delivered / answered / accepted), in-flight routing turns, pending drafts. The mailbox has its own durable file, but it is keyed by `message_id` and **belongs to no session model**.

This asymmetry is exactly why the Chat side is the more fragile one: intermediate state has no in-session resting place, so unless derivation is centralised on the reader side it scatters into every consumer.

## 3. First principle: the two-layer model

### 3.1 Definitions

- **Persisted model (terminal layer)**: terminal state only. Terminal = a fact that, once true, no longer changes, or the conclusion of the final settlement.
- **Runtime model (runtime layer)**: a **superset** of the terminal layer, additionally holding live data (liveness, progress) and intermediate state (pending / delivered-but-unanswered / awaiting settlement).

### 3.2 Three rules

**R1 Reconstructability.** `runtime = f(persisted) + inputs`. Test: kill the process at an arbitrary moment; after restart, every state the frontend displays must be answerable from disk. A state that fails this test is a model gap.

**R2 Settlement rule.** What can be **uniquely derived** from other durable facts is not stored; what **cannot** be derived must be settled at the transition. **The converse also holds: never create new storage for a derived view** — that manufactures a second truth (v1.0 of this document fell into exactly that trap).

**R3 Reference, never own.** The runtime model may reference external truth (another node's round, delivery-layer state) but holds coordinates only, never a copy.

### 3.3 Deciding which layer a fact belongs to

1. Is it still needed after a restart? — No ⇒ pure runtime data.
2. Can it be uniquely derived from other durable facts? — Yes ⇒ do not store; compute at the single derivation point.
3. Does it belong to another service or node? — Yes ⇒ store coordinates only.

Everything else is settled into the terminal layer.

## 4. Model shape: four concepts, two natures

| Concept | Nature | Terminal carrier (durable) | Runtime part | Sole writer |
|---|---|---|---|---|
| `message` | durable entity | log row: `kind` / `body` / `from` / `to` / `mentions` / `refs` / payload | sending, uploading, retrying | the two chat-side writer classes (see §5.2) |
| `round_ref` | durable entity | log row: `{agent_instance_id, agent_session_id, round_id}` + `opened_new` | whether that round is still running (answered by the execution side) | the Runtime (via relay) |
| `turn` (one delivery) | **derived view, not an entity** | no carrier of its own: the terminal state is carried by the "answer row" (`round_ref` / `agent_message` / receipt `system_notice`, all naming the human row in `in_reply_to`) | pending / routed / liveness | the delivery layer (Gateway mailbox) holds the process and writes no transcript |
| `question` (one solicitation) | **derived view, not an entity** | no carrier of its own: the question content is terminal on write to its log row; "answered" is answered by the `round_ref` pointing at the asked message | open questions | the asker (agent via relay) writes the question row; the answerer writes the answer row |

Four hard consequences:

- **Do not create storage or a sidecar for `turn` / `question`.** They are a composite view of "delivery-layer process × transcript-layer terminal state", computed at the single derivation point (§6).
- **Log rows are immutable.** Every "current state" comes from a terminal row or the composite view; no consumer may scan the log and decide for itself.
- **Every answer row must thread to the row it answers** (`in_reply_to` = that row's `message_id`). This is the precondition that makes derivation recomputable, and its absence is the direct cause of the current defects.
- **Payload is terminal**: written once, persisted verbatim; unregistered keys must **fail loudly** at the write boundary, never be dropped silently.

**On "delivery address"**: the address is not a field on the human row. The human row is written before delivery (storage precedes delivery, §5.1), while "which session it went to" is decided later by routing or re-delivery — so the terminal carrier of the address is `round_ref.agent_session_id` (with `opened_new` distinguishing a new session from a continued one). The existing `to` means "which agent" (the target instance in 1:1, empty in group chats where `mentions` carries it), and `meta.agent_session_id` never appears on a human row. **A new `address` field would be a second representation**, so it is rejected.

## 5. Consistency rules

1. **One fact, one representation.** The address is carried only by `round_ref`; "which message is asked about" is carried only by the question row's threading field; no parallel representation (id-prefix encoding, etc.) may be introduced.
2. **Writer authority belongs to the model.** Chat is multi-writer, an execution Session is single-writer. Today the chat writers are 6 call sites in three classes: (1) the Gateway's own human-utterance entries (`deliver_mentions` / `deliver_chat_message`); (2) the Gateway's delivery receipts (`delivery_failed`, `budget_exhausted`); (3) the Runtime's `/api/intents/chat-append` relay writing `execution_round` / `agent_message` / `system_notice` (including question rows). The admission rules (participant check + kind whitelist) are part of the model, not scattered across entry points.
3. **No second truth across layers.** The delivery layer (mailbox) and the transcript are not two copies of one fact: the former is liveness, the latter is outcome. The transcript **must not** copy delivery internals (`delivered_at` / `accepted`, etc.); the delivery layer **must not** carry session content.

## 6. Read model: one derivation point

Derivation is permitted in exactly **one** place: the backend-assembled projection the surfaces render (composed on the Gateway; the existing chat block read is the sample). This is the write-model / read-model split — the log is the write model, the projection is the read model (the read side of CQRS): the read model may derive from the log, but derivation happens server-side only, in one place, and is invisible to consumers. Inside it: the messages, each block's `ok / pending / not_found / unreachable`, each human message's delivery outcome (answered or not, which session), and the open questions.

- **ADR-106 D3's "answered" criterion is unchanged**: it remains "the transcript holds a reference row pointing at that message"; only the **execution point moves from the surface to the projection**. Hence this document does not amend ADR-106. (If implementation later collapses `answered` into a state entity, ADR-106 must be amended in both languages in the same commit.)
- **Consumer forbidden-derivation list** (doing any of these indicates a model gap, not a frontend bug): matching `in_reply_to` against transcript rows to decide "answered"; parsing an id prefix to recover semantics; branching on `notice_type` / `kind` to infer an entity's state; inferring liveness from transcript shape ("is there a row after the newest human row").

### 6.1 Shape of the projection (landed)

The read model lives in `acowork-user::chat_view`. Every row of the public message page `GET /api/chat-sessions/{id}/messages` is "the row, unchanged, plus `delivery`" (`#[serde(flatten)]`: not a single field of the row itself changes, so older consumers keep working):

| Field | Meaning | Present on |
|---|---|---|
| `delivery.answered` | some row answers this one | `user_message` (an explicit `false`: waiting is a state, not a missing field) |
| `delivery.answered_by` | the row that answered this one (first in log order) | an answered `user_message` |
| `delivery.targets` | every execution session this turn landed in (`agent_instance_id` / `agent_session_id` / `round_id` / `opened_new`) — coordinates only, never the round's content. A list: a group `@` fan-out opens one round per target | a `user_message` with a round |
| `delivery.question.answered` | the message this card asks about now has a round, so the question is settled | a card (`session_choice` / `confirm_continue`) |
| `delivery.question.answered_session` | the execution session the answer landed in (the answering row's `agent_session_id`, first in log order) — what lets a settled card say "handed to <session>" | an answered card |
| `delivery.question.options` / `allow_new_task` | the card's own answers and exit, passed through verbatim | a card |

Three implementation constraints:

1. **Derivation must read only the rows it asks about.** The reverse edge `in_reply_to → answering rows` is maintained by the store (`LineIndex.answers`; sidecar v2 records each line's thread coordinate) and a query reads only the hit spans — no transcript scan anywhere. It is still a **disposable derived index**: lose it and rebuild it; `messages.jsonl` remains the authority.
2. **A card is not its own answer.** `question.answered` only accepts "an `execution_round` reference row exists for that message"; counting *any* answer row would make a card declare itself settled the moment it was written — exactly the trap of handing "is it answered" to the generic `is_answer_row`.
3. **The agent-facing read stays bare.** `chat_history_read` goes through `/internal/.../messages`, which returns rows without `delivery`: that page is budgeted per row and fed to a model, where derived state is neither rendered nor free. Need state? Ask the service; do not derive.

**Still the presentation layer's job (and not state derivation)**: folding `execution_round` reference rows under the human message they belong to (`chatTranscriptGroups`), and fetching the answered row's body/sender by coordinate (quotes, the optimistic title seed). Those are "fetch one known row by identity"; the judgement itself lives in the projection. As long as no second place decides *answered / delivered where / still alive*, §6 holds.

## 7. Phases and acceptance

Acceptance is always "**one piece of frontend derivation deleted**" or "**one terminal fact added by the writer**", never "the screen looks right".

| Phase | Content | Acceptance |
|---|---|---|
| P1 write contract **(landed)** | every answer/receipt row threads to the row it answers (question rows included); payloads persist verbatim per a registry, unregistered keys fail. Enumerate and fix the 6 call sites of §5.2 | no reply-class row can be written without its coordinate; payload fields stop disappearing |
| P2 single derivation point **(landed)** | the projection carries delivery outcome and open questions; the frontend deletes all derivation | zero frontend derivation; after a restart the system still answers "answered or not, delivered to which session" |
| P4 card answering **(landed)** | the service resolves what answering a card sends (origin words + attachments + already-pinned refs); the surface folds in the click alone | both frontends' `buildCardAnswer` and its window lookup are gone; a card outside the window still answers |
| P3 red lines **(landed)** | no-derivation / unregistered-key and unregistered-notice-type red lines in CI (following the existing red-line script) | newly added cross-row matching, unregistered notice types and a dismantled write door are blocked by CI (id-prefix parsing is left to the type system, see the last paragraph of §7.3) |

### 7.1 Where P1 landed

| Constraint | Implementation | Test |
|---|---|---|
| payload registry | `IntentMeta`'s fields *are* the registry; undeclared keys land in `IntentMeta::unregistered` (`flatten`, preserved on read) | `unregistered_keys_in_history_survive_the_read` |
| unregistered keys fail | `check_write_contract` (`acowork-user`), called from `append_message`; the internal HTTP boundary maps it to 422 | `write_contract_refuses_rows_no_reader_could_resolve`, `system_endpoints_…` (the 422 assertion) |
| reply-class rows need a coordinate | same entry; the notice classes live in `acowork_core::intent::chat_kinds` (`THREADED_NOTICES` / `CARD_NOTICES`) so three crates share one list | `chat_notice_classes_are_registered_once` |
| a card needs its answers and its exit | same entry (`options` non-empty, `allow_new_task` declared) | same as above |
| the card's coordinate is right | on the Runtime side the two parameters collapsed into one: `session_choice_meta(notice_type, request_message_id, options)` derives `in_reply_to` | `a_session_choice_card_answers_the_message_it_asks_about` |

### 7.2 Where P2 landed

| Constraint | Implementation | Test |
|---|---|---|
| the projection is the only derivation point | `chat_view::read_page` + `DeliveryView` / `QuestionState`; the public page returns `Vec<ChatRowView>` | `chat_view` unit tests (unanswered / answered / target, a card's own question, retired milestones never answering) and `message_page_projects_delivery_state` (end-to-end HTTP shape) |
| read only the rows asked about, never scan | `LineIndex.answers` reverse edge + sidecar v2 (one `in_reply_to` column per line); `answering_rows` reads hit spans by byte range and re-scans once when the file predates the version marker | `answering_rows_reads_only_the_hit_lines`, `a_v1_sidecar_is_rebuilt_into_v2_instead_of_trusted` |
| zero frontend derivation | Desktop: `turnSettled` reads `delivery.answered`, card options and answeredness read `delivery.question`, the `roundTriggers` set and the `cardAnswered` prop are gone; Mobile is the mirror image | `chatTurnInflight.test` / `AgentChatView.test` / `GroupChatPanel.waitLine.test`; mobile `chat-turn-inflight` / `chat-notice-row` / `chat-turn-status` |
| the agent-facing read carries no derivation | `RawMessagePage` (`/internal/.../messages`) | the existing `chat_history_read` cases |

### 7.3 Where P3 landed

`dev/ci.sh run_chat_projection_redline` (wired into both the `check` and `all` entries) guards **shape**, not style — both original failures were shape failures:

| Constraint | Implementation | What it refuses |
|---|---|---|
| the write door is not taken off | `IntentMeta::unregistered` and `check_write_contract` must exist, and the latter must be called from `append_message` | deleting the registry fallback or the contract check lets payload keys be dropped silently again |
| one derivation point, two distinct pages | the public page must be `Vec<chat_view::ChatRowView>`, the internal page must still be `RawMessagePage` | someone projecting the agent-facing page too (a per-row-budgeted page squeezed by derived fields) or reverting the public page to bare rows |
| no aggregate derivation | in a surface, `in_reply_to` may not share a line with `some(` / `filter(` / `every(` / `new Set` / `.includes(` (comments excluded) | the original bug: `rows.some(r => r.meta.message_id === x.meta.in_reply_to)` deciding "answered". A by-coordinate `.find(` of one known row is still allowed (last paragraph of §6) |
| the notice vocabulary is closed | every `notice_type` literal emitted by a writer (runtime / gateway) must appear in `chat_kinds` | inventing a notice type outside `THREADED_NOTICES` / `CARD_NOTICES` / `LIMIT_NOTICES`, which skips the coordinate requirement and lands as an orphan row |

The fourth check needed a third class: `LIMIT_NOTICES` (`budget_exhausted` / `truncated` / `unavailable`) report a **limit of the channel** — the request they name never became a row, so no coordinate can exist. `delivery_failed` is not among them: the message it owed does exist, so it threads like any other receipt. `chat_notice_write_classes_are_disjoint` pins the three apart on the *write-contract* axis (`RETIRED_NOTICES` is the other axis — rendering and settling — and overlaps by design: `interaction_resolved` is correctly on both).

Not done: there is no separate red line for **id-prefix parsing**. The P2 deletion (recovering `kind` from a `message_id` prefix) left no residue, and "this string may not appear anywhere" rules cost far more in false positives than they buy; the right guard against a relapse is to narrow `ChatRowView` until there is no prefix to parse (`kind` mandatory) — the type system rejecting it, not a grep.

### 7.4 Where P4 landed

| Constraint | Where | Tests |
|---|---|---|
| the content of an answer belongs to the service | `chat_card::answer` (origin row by `in_reply_to` → card class, unanswered, caller is the addressee → assemble `text` / `attachment_ids` / `refs`); `GET /api/chat-sessions/{id}/messages/{message_id}/answer` | `chat_card` unit tests (verbatim echo, already answered, bystander, missing origin, attachments carried), `a_card_is_answered_from_the_log_not_the_loaded_window` (HTTP: 200 / hidden 404 / 422 not-a-card / 409 spent) |
| the address is still the click | `answerWithChoice(facts, choice)` folds in "which session / new work"; `directCardAddress` / `groupCardAddress` unchanged | `chatCardAnswer.test` (desktop / mobile), `AgentChatView.test` (D6 re-delivery) |
| the status code is the semantics | 409 = a round settled the card (refresh, not an error); 404 / 422 = this card can never be answered again (must be said out loud) | `chatSessionStore.cardAnswer.test` (desktop) |
| a fan-out has several targets | `DeliveryView.targets: Vec<RoundTarget>` (one round per target, each holding a slot) | `chat_sessions_api` end-to-end assertion + the frontend types |
| a sidecar is born with its version | `sidecar_append` writes the `#v2` header on first write; `load_sidecar` tolerates a torn last line | `a_sidecar_created_by_an_append_carries_the_marker` |

**What presentation still keeps**: reading *one* known row by coordinate (the transcript grouping locates a trigger row via `in_reply_to`; the optimistic title seeds itself from the origin body). Those read a single row and decide no state, and each has an authoritative source behind it. **State** — answered or not, where it went, whether a card is open — is read from `delivery` only.

## 8. Non-goals

- No new durable entity, sidecar or database: `turn` / `question` are derived views (§4).
- Authoritative state must not go into `rounds.sqlite` (it is a disposable cache, ADR-106 D5).
- No change to execution Session log semantics or the round reference scheme (ADR-106 unchanged).
- No backfill of historical damaged rows (solicitations whose payload was already dropped stay permanently read-only).
- No cross-language code generation.

## 9. Open points

1. Payload registry boundary: the registered key set, the rejection semantics (422 vs warn-and-refuse-write), and whether "storage-required" and "display-only" keys are distinguished.
2. Whether chat cards need an "expired" semantics (today: none — a card stays answerable forever). If they do, expiry becomes the **first non-derivable terminal state** and needs a settlement record — written as a new event row, never a sidecar.
3. When delivery fails, the human's *intended* address leaves no trace (because the address is carried by `round_ref`, and no such row exists on failure). Whether the receipt row should carry an "intended address" field awaits a product decision.
4. Whether `delivery` should also describe "delivered to X, no round yet" (today `targets` appears only once a round exists): that is the "delivered but execution not started" window, and its product semantics are undecided — `answered=false` plus the wait line covers it for now.
5. Where `system_notice` belongs: the proposal is to converge it to "stateless notices" (receipts, budget limits) and give stateful solicitations a display-level class of their own; whether the kinds merge is decided at implementation time.
