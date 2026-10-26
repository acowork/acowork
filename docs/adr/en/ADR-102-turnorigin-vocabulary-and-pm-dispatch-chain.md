# ADR-102: TurnOrigin Vocabulary and PM Dispatch Chain Governance

- **Number**: ADR-102
- **Status**: Accepted
- **Date**: 2026-10-25
- **Deciders**: ACowork Architecture Group
- **Related**: Design 27 (v2.8, §4.5 / §5.4.2 / §5.4.5 / §6.4 / §13), ADR-099 (Chat Router), ADR-100 (chat event log), ADR-060 (system-block byte stability), ADR-076 (PM service), [fix plan](../../plan/zh/pm-dispatch-review-fix-plan.md) (zh working doc), runbook §7 (mixed-version skew)

## Summary

The Intent envelope's `kind` field used to be a string each producer filled in ad hoc, while the receiving side (Runtime's routing, rendering, role trimming, and write-back) **re-inferred** "why this turn exists" with different heuristics at every decision point — so a PM task dispatch got treated as an agent-to-agent responder turn. This ADR establishes "turn origin" as a first-class concept: the `acowork-core::intent::TurnOrigin` enum plus a policy table; producers stamp it explicitly and consumers must consult the table. It also fixes three rules for the PM dispatch chain: the human/model text split (the playbook travels as `assignment_brief` in the system block and is a high-trust field), task-coordinate constants (bare prefix matching is banned), and dispatch triggering plus retry idempotency owned by the PM scanner (a persisted `dispatch_message_id` reused per generation).

## Context

The PM dispatch refactor (the precursor of Design 27 v2.8) exposed three misalignments with one root cause: **the wire protocol had no first-class concept of "why this turn exists"**, so Gateway's 7 construction points and Runtime's 10-plus decision points each improvised, contradicting one another.

1. **Role trimming (R1)**: the dispatch envelope's `kind` was hardcoded to `"request"` (shared with agent-to-agent), and Runtime's `turn_kind_semantics` mapped `"request"` to responder — so a human-created task turn lost `ask_user_question` and `intent_send`: the executing agent could neither ask the human a question nor request another agent's help.
2. **Rendering misalignment**: `render_intent_message` printed `[intent:{from}:{action}] {params}` verbatim for `"request"`. The same dispatch message read as human text in the group chat (the chat row stores the body) but as a full JSON envelope in the execution session transcript; the session title is derived from the transcript's first user message, and the JSON leaked **back** into the group chat a second time via ADR-100's "启动新任务：{title}" reaction row. Three surfaces polluting each other.
3. **Coordinate folklore**: distinguishing "PM task turn" from "group-chat turn" relied on `conversation_id.starts_with("task:")` string matching, scattered across 5 sites in Gateway / Runtime — untyped, undocumented, silently breaking if the format ever changes.

The review (fix plan) found two implementation-level defects confirming the gap: after the playbook was split into the envelope field `assignment_brief`, the Runtime consumer **never checked the origin** — any path able to construct an envelope could inject instructions into an execution session's system block (missing trust boundary); and each PM dispatch retry minted a fresh `message_id`, so the chat row and the Gateway mailbox budget were charged again per retry (missing idempotency identity).

## Decision

### D1 — TurnOrigin vocabulary and policy table

`core/acowork-core/src/intent.rs` defines `TurnOrigin { Chat, PmTask, AgentRequest, AgentResponse }`, carried on the wire as the `kind` string (`"user_message"` / `"pm_task"` / `"request"` / `"response"`); `TurnOrigin::parse` falls back to `AgentRequest` for empty or unknown values (legacy mixed-version producers land on the conservative side: better one extra responder constraint than treating an agent response as a human turn). Each origin carries a `TurnPolicy`; all four decision points consult the table, no local heuristics:

| Field | Meaning | Chat | PmTask | AgentRequest | AgentResponse |
|---|---|---|---|---|---|
| `router_eligible` | Chat Router may pick the execution session | ✓ | ✗ (the task coordinate IS the route) | ✗ | ✗ |
| `render_envelope` | transcript renders the envelope structure | ✗ (human text) | ✗ (human text) | ✓ | ✓ |
| `responder` | responder role: trims `intent_send`/`ask_user_question` | ✗ | ✗ | ✓ | ✗ |
| `write_back` | terminal state mirrors back to the source chat | ✓ (when `message_id` present) | ✓ (group-chat row) | ✓ | ✗ |

Adding an origin = adding an enum variant + a policy row; the compiler forces every decision point to take a position. This is where "module design replaces point-patching" lands.

### D2 — Task-coordinate constants in one place

The `task:` prefix format (conversation id `task:{project}:{task}`, routing key) has exactly one implementation, in `intent.rs`: `TASK_CONVERSATION_PREFIX` / `task_conversation_id()` / `is_task_conversation()` / `task_routing_key()` / `is_task_routing_key()`. Feature code must not write bare `starts_with("task:")` (all 5 original sites replaced).

### D3 — Dispatch message split: human text to chat, playbook to the system block

`build_dispatch_message` splits into `build_human_text` (title + description, into the group-chat row body and the envelope's `message` field) and `build_assignment_brief` (the claim/submit playbook, a separate envelope field `assignment_brief`). Only on `PmTask` turns does Runtime inject the brief into the execution session's **system block** (ADR-060: a per-generation constant, byte-stable) — never into rendered text, never onto any chat surface. The chat row's meta keeps only `task_id` (the frontend renders a task chip, click to copy); `project_id` is removed — the row already lives inside that project's group. **The key must be declared on the chat storage `IntentMeta` struct**: undeclared meta keys are silently dropped at this boundary (the known trap from decision 29, hit for real this time), pinned by tests on both sides (producer pins the key set, consumer round-trips the JSON).

### D4 — `assignment_brief` is a high-trust field: origin gating plus edge stripping

The brief reaches the system block (equivalent to instruction injection), therefore: on the Runtime side both extraction (`extract_assignment_brief`) and consumption (`session_task`) gate on origin — non-`PmTask` is always empty; on the Gateway side, agent-supplied params (`/api/intents/send`) have the `assignment_brief` key stripped before delivery — defense in depth, which also protects mixed-version old Runtimes that do not know the field from third-party injection. The PM service is the only legitimate brief producer (the task-dispatch endpoint).

### D5 — Dispatch triggering converges on the PM scanner

PM no longer calls Gateway synchronously on state changes; a scanner periodically pulls due tasks (three gates: `start_at` reached + approval + dependencies satisfied) and dispatches in batches. Event hooks were rejected: they leak domain knowledge (dependency trees, approval state) into the call site, and restarts lose events; the scanner is idempotent by construction and crash-self-healing.

### D6 — Retry idempotency: generation identity + bounded backoff

`Task.dispatch_message_id` is persisted **before the first attempt** (serde default, compatible with old data) and reused on every retry — the chat row dedupes on `meta.message_id` and the Gateway mailbox dedupes its budget on message_id (one charge per generation); it is cleared on task state transitions (reopen / reassign) and the backoff counter restarts per generation. Backoff: 60s → 5m → 15m → 30m, abandon after 5 consecutive failures with a `tracing::error!`. Gateway's `TaskDispatchRequest.message_id` changes from "server-minted only" to "accepts an optional external id" (still self-mints when absent).

### D7 — Version-skew discipline

In a new-Gateway → old-Runtime window, `pm_task` falls back to `AgentRequest` (functional regression, no data loss). Multi-node deployments upgrade **Runtime first (or same batch)**, then Gateway; rollback only needs a Gateway-side revert, Runtime need not follow. Table in runbook §7.

## Consequences

- Fixed: R1 role trimming (human task turns regain `ask_user_question` / `intent_send`), transcript JSON leakage, title double-pollution, missing group-chat event rows for PM dispatch turns (now covered by ADR-100 reaction rows).
- Narrower protocol surface: `kind` goes from free-form string to a four-value vocabulary; new construction points must declare themselves; `assignment_brief` / `message_id` join the task-dispatch contract (Design 27 §4.5's field table is the source of truth; internal endpoints are not duplicated into protocols/http).
- Deferred debt (recorded in fix plan §8, out of scope here): Mobile task chip, scanner metrics / graceful shutdown, a task-visible state after dispatch abandonment, TaskEditDialog date-only timezone drift, `dispatch_due_ids` performance at 10k+ tasks.

## Alternatives rejected

- **Flip the dispatch envelope's kind to `"user_message"` Gateway-side** (minimal patch): rejected — sidesteps the root cause ("origin is a first-class concept"), cannot fix the responder trimming, and lets the `starts_with("task:")` folklore keep spreading.
- **Judge origin by `source_service == "acowork-pm"` string**: rejected — still an untyped cross-process convention, same disease as D1.
- **Persist the brief as a system-role entry in session history**: rejected — ADR-060 requires byte-stable system blocks, and storing runtime-concatenated text in history is exactly the redundancy this ADR removes; the brief is injected per generation and never lands in the transcript.
- **Keep both the event hook and the scanner**: rejected — two sources of truth, and the idempotency identity (D6) cannot hold on two paths at once.
