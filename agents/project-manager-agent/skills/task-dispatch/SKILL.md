---
name: task-dispatch
description: Hand work to another agent through the PM service — roster, assignment, start time, approval gate, and what to do when dispatch does not land
version: "1.0.0"
author: developer
triggers:
  - dispatch
  - assign work
  - 派工
  - 分派任务
  - hand off to an agent
tool_deps:
  - pm_list_agents
  - pm_get_project
  - pm_create_task
  - pm_get_task
  - pm_list_tasks
  - pm_update_task
  - pm_add_activity
  - pm_list_activity
  - doc_write
---

# Task Dispatch Skill

## Core Rule

Dispatch is a handoff to an agent that may be **offline right now**. It happens
through the PM service, never through chat: a message in a chat does not exist
next session, does not survive compaction, and cannot be claimed, blocked,
approved, or reported on.

Never type an id you were not given. `project_id` comes from a tool response,
`instance_id` from `pm_list_agents`, `task_id` from `pm_create_task`. If you
find yourself guessing an id, you are about to invent a coordinate that no
other agent can resolve.

## The chain, and who performs each step

```
pm_create_task            you        → status=pending, review_status=pending (agent-created)
human approval            a human    → review_status=approved, still pending
dispatch scanner          automatic  → at start_at, wakes the assignee with the brief
pm_claim_task             assignee   → pending → in_progress
work + pm_submit_task     assignee   → in_progress → submitted
human acceptance          a human    → submitted → done  (checkpoint/bug always; others auto-done)
```

Two steps are not yours: **approval** and **acceptance**. An agent cannot
approve its own plan, and cannot mark its own work accepted. Do not promise
either.

## Execution Steps

1. **Confirm the assignable set**
   - `pm_get_project` → `members[].instance_id`, or `pm_list_agents` with
     `project_id` → the same set. Anything outside it is rejected with
     `assignee_not_project_member`.
   - `online: false` is **not** a reason to hold back: the Gateway starts an
     offline agent when the intent is delivered.

2. **Create one task per unit of work**
   - `pm_create_task` with `project_id`, `title` (imperative, concrete),
     `description` (acceptance criteria — this is the whole brief the assignee
     will get), `assignee`, `priority`, `type`.
   - `start_at`: when the work should *begin*. Omit it and the store fills
     `now` — the wake-up goes out as soon as approval lands; set a future
     `start_at` and the scanner waits. Use it for real sequencing (a sprint
     start, a dependency's expected completion), not as a reminder for yourself
     — a past `start_at` means "immediately". A task whose `start_at` is
     **empty** is 未排期 and is **never dispatched** — only `pm_update_task`
     with `start_at: null` can unschedule one.
   - `depends_on`: declare blockers rather than describing them in prose. A
     blocked task is **not dispatched** and `pm_claim_task` refuses it with
     `409 dependency_not_satisfied`; prose is invisible to both.
   - `parent_task_id` for nesting. Nothing assigned yet? Leave `assignee` off —
     an unassigned task is honest. Never assign a human account: the chain
     delivers to agent instances, humans take work from the Desktop app.

3. **Tell the assignee what is not in the ticket**
   - `pm_add_activity` with the context that must not overwrite the acceptance
     criteria: where the spec lives, what was already decided, what a blocked
     run means. Comments are read by the assignee and by reviewers later.

4. **Say what happens next, accurately**
   - "Created and assigned; a human must approve it before {agent} can start."
     Saying "dispatched" here is false — approval has not happened.
   - Do not poll in a loop waiting for approval. Nothing changes until a human
     acts, and the scanner is what wakes the assignee, not your polling.

## Reading dispatch state

`pm_get_task` carries the dispatch fields; read them instead of re-dispatching:

| Field | Reading |
|---|---|
| `dispatched_at` set | the wake-up was delivered for this generation |
| `dispatched_at` null, `review_status=pending` | waiting on a human |
| `dispatched_at` null, approved, `start_at` in the future | waiting for the clock — normal |
| `dispatched_at` null, approved, `start_at` empty | 未排期 — never dispatched until someone schedules it |
| `dispatch_error` non-null | delivery failed; the reason is the Gateway/target's own text |
| `dispatch_attempts` rising | the scanner is retrying with backoff |

A task that exhausted its attempts stays undispatched **until its generation
changes** — reassign it or move its status. That is a human-visible decision:
report `dispatch_error` and stop rather than reassigning to make the error go
away.

## When the assignee reports back

- `409 review_not_approved` → the task still awaits a human. Report and stop.
- `409 dependency_not_satisfied` → read `blocked_by`, and either finish or
  cancel the prerequisite, or move `start_at`. Do not re-claim.
- `forbidden` on claim/submit → the task is assigned to a different instance.
  You cannot act for another agent; hand the task id back to the human.
- Submitted work needs a decision: check `review_status` and, for
  `checkpoint` / `bug`, say explicitly that acceptance is still pending.

## Anti-patterns

- Dispatching by chat message, or "I told the agent in the group chat".
- A task so coarse that its description cannot state acceptance criteria.
- Re-creating a task because the old one is blocked — fix the blocker instead,
  or the duplicate becomes the thing that gets worked on.
- Editing `assignee` back and forth to force a re-dispatch. Clearing the
  generation is the effect, but the cause is never fixed and the history lies.

## Output Format

```markdown
## Dispatch

| Task | Assignee | Start | Blocked by | Status |
|---|---|---|---|---|
| `{id}` {title} | {name} (`{instance_id}`) | {RFC3339 or "immediate"} | {ids or —} | pending / awaiting approval |

**Waiting on a human**: {approval / acceptance — name which}
**Next**: {what actually happens, and who does it}
```
