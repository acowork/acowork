---
name: project-setup
description: Bootstrap a project so work can be dispatched and its deliverables have a home — project record, document directory, README index, and the member roster
version: "1.1.0"
author: developer
triggers:
  - set up a project
  - bootstrap project
  - 立项
  - 新建项目
  - project kickoff
tool_deps:
  - pm_create_project
  - pm_get_project
  - pm_add_project_member
  - pm_list_agents
  - doc_mkdir
  - doc_list
  - doc_write
  - mcp_memory__memory_store
---

# Project Setup Skill

## Core Rule

A project is set up when **three** things exist, not one:

1. the project record in PM,
2. the document directory its deliverables go into,
3. the roster of agents that may be assigned work.

Creating the record and stopping is the common failure: the first task then has
nowhere to put its PRD and nobody to dispatch to, and someone rebuilds both by
hand a week later.

## The document directory is published, not derived

`pm_create_project` / `pm_get_project` return `doc_path` — the project's
directory inside the document library (design §5.5, shape
`项目管理/{project_id}`). **Read it, never rebuild it**:

- It is keyed on the `project_id`, not on the title. A retitle — which
  `pm_update_project` makes routine — must not move the deliverables.
- It is the one value both you and the document-manager agent agree on without
  a lookup. Hand-building the path from the title is how two agents end up
  writing into two different trees for the same project.

## Execution Steps

1. **Create the project record**
   - `pm_create_project` with `title` and a `description` that states the goal
     in one or two sentences (the description is what other agents read first).
   - Keep `id` and `doc_path` from the response.

2. **Create the whole directory tree — in one pass, root first**
   - Parents are **not** created implicitly: `doc_write` into a path whose
     parent is missing fails with `DirNotFound` naming the segment that does
     not exist. That is deliberate — a typo becomes an error instead of a
     silent parallel tree — so the full chain goes in before the first write.
   - The tree is **seven directories under `doc_path`**, plus the two above it.
     Build all of them now, not on demand: a participant has no `doc_mkdir`, so
     an area you did not create is a gap it can only report back to you.

     | # | `doc_mkdir` path | owner of what lands here |
     |---|---|---|
     | 1 | `项目管理` | PM (shared root for every project) |
     | 2 | `{doc_path}` | PM (`README.md`, `PRD.md`, `风险登记册.md`, `冲刺计划.md`, `复盘.md`) |
     | 3 | `{doc_path}/周报` | PM |
     | 4 | `{doc_path}/会议纪要` | PM |
     | 5 | `{doc_path}/技术方案` | Architect, SSE |
     | 6 | `{doc_path}/评审` | Architect, SSE |
     | 7 | `{doc_path}/测试` | QA |
     | 8 | `{doc_path}/验收` | Product |
     | 9 | `{doc_path}/调研` | Product |

   - **Idempotent: check before you create.** `doc_list` with `path: doc_path`
     first and `doc_mkdir` only the segments that are missing — a re-run on a
     half-built tree should create the rest, not lean on errors. `NameConflict`
     means the directory exists already (another agent got there first, or the
     tree was built by hand); **use it**, and never invent a suffixed name like
     `项目管理-2`. Treat `NameConflict` as "already handled", not as a normal
     path to retry through.
   - **You are the only one who creates directories in this tree.** Participants
     (Product / QA / SSE / Architect) write files but hold no `doc_mkdir` —
     whoever creates a directory governs it (see *Ownership and ACL facts*
     below), so a self-made area could later be renamed or pruned only by the
     agent that made it, and your IA restructuring would be refused with
     `forbidden`. One tree, one governor.

3. **Write the README index**
   - `doc_write` `{doc_path}/README.md`, `mode: create`, with an H1 that is
     **the project title** and a list of the deliverables you expect.
   - The directory name is a machine handle; the README is the human face and
     the thing `doc_search` matches. This is why an unreadable directory name
     is acceptable.
   - The README carries the **ownership table** — the closed vocabulary that
     tells every participant where its output goes. Without it each agent
     invents a path, and two agents writing "the same" report into two places
     is the failure this step exists to prevent. Include this table verbatim
     (it is the contract, not a suggestion):

     | path | owner | content |
     |---|---|---|
     | `README.md` | PM | this file, the project entry point |
     | `PRD.md` | PM | requirements (participants read-only; changes go through `doc_write` → `pending_review`) |
     | `风险登记册.md` | PM | risk register |
     | `冲刺计划.md` | PM | sprint plan |
     | `复盘.md` | PM | retrospective |
     | `周报/{YYYY-WW}.md` | PM | weekly report |
     | `会议纪要/{YYYY-MM-DD}-{slug}.md` | PM | meeting minutes |
     | `技术方案/{slug}.md` | Architect, SSE | system design, ADR, trade-off analysis |
     | `评审/{YYYY-MM-DD}-{slug}.md` | Architect, SSE | design review, code review |
     | `测试/{测试计划\|测试报告\|回归报告}.md` | QA | test-side deliverables |
     | `验收/{slug}-验收报告.md` | Product | feature acceptance |
     | `调研/{slug}.md` | Product | user and market research |

   - The vocabulary is **closed**: when a deliverable has no row, the owner of
     the tree extends the table — a participant does not coin a filename. Add
     the row here (and in the README) when the request arrives, then tell the
     requester where to write.

4. **Build the roster**
   - `pm_list_agents` **without** `project_id` → every agent installed on this
     machine (the set to invite from; filter with `query` on name/role).
   - `pm_add_project_member` per agent, using `instance_id` — never a package
     id like `com.acowork.foo`. Requires project ownership.
   - The roster is also the *permission* boundary: `pm_create_task` rejects any
     `assignee` outside it (`assignee_not_project_member`). Invite before you
     plan to dispatch.

5. **Verify before reporting**
   - `pm_get_project` → `members` contains every instance id you intend to
     dispatch to.
   - `doc_list` with `path: doc_path` → the README is there and the seven
     sub-directories are all present; a missing one is a tree you will have to
     finish later, when a participant's write fails on it.
   - `mcp_memory__memory_store` the bootstrap facts worth carrying across
     sessions: project id, `doc_path`, roster, and the naming decisions.

## Ownership and ACL facts that change what you do

These are enforced in code, not conventions:

- **Whoever creates a directory governs it.** `doc_mkdir` stamps the caller as
  the directory's creator, and the structural gate reads that stamp later when
  anyone tries to rename or prune it. So *you* create `项目管理/**` — if a
  helper agent creates it, you cannot reorganize it afterwards.
- **Creating into a restricted directory is refused** (`AclDenied`). Library
  ACLs are public by default, so this only happens when a human tightened one.
  Report it and stop; do not look for another path to the same content — an
  alternative path is exactly what the restriction is protecting.
- **You may write another agent's document, but it becomes a review proposal**
  rather than an apply. Structural changes to *their* nodes are refused
  outright. In both cases the right move is to ask the human, not to retry.

## Failure Modes

| Symptom | Meaning | What to do |
|---|---|---|
| `DirNotFound: directory path segment: 项目管理` | the chain was never created | `doc_mkdir` each segment, root first |
| `NameConflict` on `doc_mkdir` | directory exists already | use it; do not suffix |
| `assignee_not_project_member` | agent not on the roster | `pm_add_project_member` (you must own the project) |
| `assignee_not_found` | id is not an installed instance | `pm_list_agents` and use its `instance_id` |
| `AclDenied` on `doc_mkdir` | a human restricted that directory | report and stop |
| `forbidden` on `pm_add_project_member` | you are not the project owner | report; the owner invites |

## Output Format

```markdown
## Project bootstrapped: {title}

- **Project**: `{id}` — {title}
- **Document directory**: `{doc_path}` (README index written, tree complete:
  `周报` `会议纪要` `技术方案` `评审` `测试` `验收` `调研`)
- **Roster**: {n} agent(s) — {names with instance ids}
- **Not done**: {anything you could not complete, and why}
```
