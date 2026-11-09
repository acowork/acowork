---
name: project-participation
description: How this Senior Engineer agent claims, publishes and submits project work — task intake from the PM service, design and review records published into the project doc tree, and the submission that closes the loop
version: "1.0.0"
author: developer
triggers:
  - task intake
  - claim a task
  - 承接任务
  - 提交交付物
tool_deps:
  - pm_list_tasks
  - pm_get_task
  - pm_get_project
  - pm_claim_task
  - pm_submit_task
  - pm_add_activity
  - pm_list_activity
  - pm_create_task
  - doc_list
  - doc_search
  - doc_read
  - doc_stat
  - doc_write
  - doc_move
  - doc_list_requests
  - doc_check_request
  - mcp_memory__memory_recall
  - mcp_memory__memory_store
---

## Core Rule

Not claimed is not yours. Not in the library is not shared. A chat reply is not a submission.

## Intake

1. `pm_list_tasks(assignee="me")` — your queue; `review_status=pending` rows are not startable yet.
2. `pm_get_task` — read `review_status`, `is_blocked`, `blocked_by`, the acceptance criteria, and `doc_read` what the brief cites.
3. `pm_claim_task` — pending → in_progress, the only legal way in. An unclaimed task has no owner.
4. Work only after the claim returns. A failed claim is information, not a hiccup.

| Claim result | Means | Do |
|---|---|---|
| 409 `review_not_approved` | a human has not approved the task yet | stop; report "waiting for approval" |
| 409 `dependency_not_satisfied` | a blocking task is unfinished | stop; name the blocker and the task holding it |
| 403 `forbidden` | another instance's task — a twin of your package, or the instance you were reinstalled from | stop; report to the human |

**You cannot take an unassigned task.** `pm_claim_task` answers `forbidden: task has no assignee`, and you cannot read your own instance id to fill it in — so you only do work that was assigned to you. Report the gap to the human or the PM: never create a duplicate task, never fall back to a local file.

## Publishing

- `doc_path` comes from `pm_get_project().doc_path` — read it there, never derive it from a title or an id; search first (`doc_search`, `doc_list`) so you extend the tree instead of duplicating it.
- Write inside your area only: `{doc_path}/技术方案/`, `{doc_path}/评审/`. The tree is built by the PM's `project-setup`; if an area is missing, report it with `pm_add_activity` — do not create directories yourself.
- `mode="create"` is O_EXCL: it fails when the name is taken, never the update path. Long content: `create` a `*.tmp.md`, `append` it, `doc_move` onto the final name — a half-written page is never readable.
- Someone else's document: `doc_stat` for the version, then `doc_write(if_version=N)`. Never blind `overwrite`.
- **Role.** Code goes to the repository — `file_write` is correct there, not in the doc library.
- **Role.** A review conclusion must land in `{doc_path}/评审/` to be traceable.
- **Role.** A design note that exists only in a chat reply does not exist.

## Submitting

- `pm_submit_task` `text` carries what a reader cannot reconstruct: what you changed, each acceptance criterion with its result, the deliverable's doc path.
- `submitted ≠ done`: ordinary tasks auto-close, `checkpoint`/`bug` wait for a human — say "waiting for acceptance". A chat reply is not a submission: if the board did not move, nothing was delivered.

## Handoffs

| Situation | Action |
|---|---|
| The brief is ambiguous | `pm_add_activity`: ask; `pm_list_activity` for the answer |
| The work exceeds the task | `pm_add_activity`: report it; do not silently widen scope |
| You need a change outside your area | `doc_write` it, keep the `request_id`, record it in `pm_add_activity` |
| You are not a project member | tell the human or the PM; do not work around it |

## Anti-patterns

- Reporting completion in chat — no state changed, the next session redoes it.
- `pm_create_task` as a private to-do — a task is a commitment someone accepts.
- Editing a task `description` to log progress — it destroys the acceptance criteria.
- Guessing ids or paths — read them from tool responses.

## Output Format

| Field | Value |
|---|---|
| Claimed | `task_id` + new state |
| Artifact | doc path in the library |
| Proposals | `doc_list_requests` → `request_id`s awaiting review |
| Submitted | task state + who accepts it |
| Waiting on | the human decision, and since when |
