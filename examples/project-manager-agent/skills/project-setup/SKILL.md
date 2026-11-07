---
name: project-setup
description: Bootstrap a project so work can be dispatched and its deliverables have a home — project record, document directory, README index, and the member roster
version: "1.0.0"
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

2. **Create the directory chain — explicitly, in order**
   - `doc_mkdir` with `项目管理`, then `doc_mkdir` with `doc_path`.
   - Parents are **not** created implicitly: `doc_write` into a path whose
     parent is missing fails with `DirNotFound` naming the segment that does
     not exist. That is deliberate — a typo becomes an error instead of a
     silent parallel tree — so create the chain before the first write.
   - `NameConflict` on either call means the directory already exists (a
     re-run, or another agent got there first). Use it. Do not invent a
     suffixed name like `项目管理-2`.

3. **Write the README index**
   - `doc_write` `{doc_path}/README.md`, `mode: create`, with an H1 that is
     **the project title** and a list of the deliverables you expect.
   - The directory name is a machine handle; the README is the human face and
     the thing `doc_search` matches. This is why an unreadable directory name
     is acceptable.

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
   - `doc_list` with `path: doc_path` → the README is there.
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
- **Document directory**: `{doc_path}` (README index written)
- **Roster**: {n} agent(s) — {names with instance ids}
- **Not done**: {anything you could not complete, and why}
```
