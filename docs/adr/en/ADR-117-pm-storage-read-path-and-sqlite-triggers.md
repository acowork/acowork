# ADR-117: PM storage evolution — the tree stays authoritative, fix the read path first, start the SQLite migration only when a trigger fires

**Status**: Decided (read-path fix implemented; SQLite migration not started — triggers in §3 D5)
**Date**: 2026-11-16
**Decided by**: 大鱼
**Predecessors**:
- [ADR-061: acowork-pm storage — directory tree + physical nesting as authority + zero redundant fields](./ADR-061-pm-storage-tree.md)
- [ADR-082: memory storage migrated to SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md) (source of the migration methodology and its lessons)
- [ADR-064: PM as a standalone process](./ADR-064-pm-standalone-process.md)

---

## 1. Decision summary

### 1.1 One sentence

**Not migrating to SQLite. Fixing the read path first.** The directory tree (ADR-061) remains PM's single source of truth; SQLite is recorded as a **migration plan with quantitative triggers**, not as "we'll see".

### 1.2 Key decisions

| # | Decision | Outcome |
|---|---|---|
| D1 | Storage authority | **The tree stays.** `task.json` / `project.json` plus physical nesting remain the only truth. All four ADR-061 reasons still hold (literal = logical / single atomic directory operation / zero redundancy so no double-write / idempotent walkdir rebuild). No second store this round |
| D2 | Read path | **Reads must go through the in-memory index.** Any implementation that filters on the index and then "tops it up" with `get_task()` is a defect: when the index already holds the field, hitting the disk is pure waste. `compute_blocked_by` and the project badge tally are fixed |
| D3 | New contract | `GET /projects/counts` (index-only, `total` / `submitted` per project). The home screen no longer does an N+1 over full task entities to render badges |
| D4 | Corruption defence | **Independent of storage format.** Three layers: quarantine unparseable files into `.corrupt/`, keep the last good content as `.bak` before overwriting, and **guard `shell` against protected prefixes** (`~/.acowork/`). Layer three is the actual stop-loss; one and two only guarantee "damage is recoverable" |
| D5 | Migration trigger | The SQLite migration starts **only when one of the quantified conditions in §3 D5 holds**. Never "because it feels like it will get slow" |
| D6 | Migration method | If triggered: follow ADR-082 — keep the `PmStore` trait boundary, add a SQLite backend, **run the test suite against both**, switch under a flag, then delete the old one. No big-bang rewrite |

### 1.3 Explicitly out of scope

- Attachments (`attachments/`) do not move into the DB. Binaries stay on the filesystem, metadata stays in `task.json` (ADR-061 decision 5).
- No "tree + SQLite dual write". Dual write means two truths, which is exactly what ADR-061's zero-redundancy rule exists to prevent.
- No storage-format change for corruption defence (see §2.3).
- No TTL cache on the read path. A cache introduces "the index is no longer the truth", at a consistency risk far larger than the gain.

---

## 2. Background

### 2.1 Trigger one: one corrupt file took the whole PM service down

`projects/p-66e68d1a/tasks/t-96685f5d/task.json` had three trailing bytes after its closing brace (JSON already closed at line 24). `walk_tasks` propagated the single-file parse error with `read_json(...)?`, so `rebuild_index` failed, so `PmService::build` failed, so PM exited on startup and the UI showed "no projects".

Fixed in two layers:

1. **Tolerance**: `walk_tasks` / `rebuild_index` degrade every failure (directory read, entry read, id parse, JSON parse) to `warn/error + continue`, logging project / task / absolute path. The summary line became `PM index rebuilt count=N skipped=M`.
2. **Proof that PM cannot produce bad JSON itself**: every write path goes through `atomic_write_json`, whose payload is the output of `serde_json::to_string_pretty` — structurally valid JSON by construction. No `.tmp` residue was left behind either. **The corruption therefore came from an external process writing directly into PM's data directory.**

### 2.2 Trigger two: clicking the PM nav item takes over a second on a cold cache

This is not a storage-format problem. Every read re-walks the disk, and the home screen pulls whole task entities to compute two integers. Measured against the real store (4 projects / 83 tasks / 143 JSON files / 1.6 MB):

| Step | Where | Disk opens |
|---|---|---|
| `GET /projects` → `list_projects()` does `read_dir` and reads each `project.json` | `store/tree.rs` `list_projects` | 4 |
| The client calls `listProjectTasks` **per project** → `find_tasks()` filters on the index, then `get_task()` per task | `apps/acowork-desktop/src/stores/pm/projectStore.ts` `refreshCounts` | 82 (largest project) |
| The same tasks then go through `compute_blocked_by()`, which calls `get_task()` **again** | `api/tasks.rs` `list` → `store/tree.rs` `compute_blocked_by` | 82 |
| The other three projects | | 1 |

**≈ 170 file opens and 503 KB transferred, to produce one `total` and one `submitted` per project.** On a cold cache each open also pays a Windows Defender first-touch scan, which lines up exactly with the observed one second; warm it is ~155 ms.

The most telling detail: `compute_blocked_by` already looked up dependency status in the index, yet first spent a disk read loading the whole task just to reach `depends_on` — while those forward edges were already resident as `TaskEntry::blocks`, which is precisely what the dispatch scanner's due-set dependency gate uses.

### 2.3 An intuition that needs taking apart

"Moving to SQLite would eliminate this class of corruption" — **false, but not entirely backwards.** It separates into three claims:

| Claim | Verdict |
|---|---|
| SQLite prevents an external process from corrupting the data | **No.** `echo 'x' >> pm.sqlite` yields `database disk image is malformed` for the **whole database**, not for one task. Blast radius goes from "1 of 83 tasks" to "all 83" |
| SQLite makes external writes **less likely** | **Yes.** Misuse follows the medium: JSON is text, so an Agent reaches for `echo >>`; SQLite is binary, so it either goes for the `sqlite3` CLI (three layers of grammar/schema/type rejection) or leaves it alone. The probability is an order of magnitude lower |
| SQLite corruption is detectable | **Yes.** WAL frame checksums plus `PRAGMA integrity_check` make it fail loudly instead of silently returning garbage. But PM also fails loudly and degrades now, so the gap is small |

**Conclusion**: a storage format is the wrong tool for corruption defence. The real stop-loss is **access control** — `shell` is not in `PathGuardedTool::is_filesystem_tool()` (only `file_read` / `file_write` / `file_edit` / `glob_search` / `content_search` / `doc_reader` are), so it has no path guard at all: one `echo ... >> ~/.acowork/acowork-pm/.../task.json` corrupts a task, while `file_write` would have been blocked. **Plugging that hole costs ~60 lines.**

### 2.4 Why not migrate now

- **Scale does not support it.** 4 projects / 83 tasks / 1.6 MB; `rebuild_index` is milliseconds. ADR-082 moved memory to SQLite because of a **measured 7-9 second startup**. There is no such measurement here.
- **Atomicity is no better.** `reparent_task` is currently **one `rename`, zero file writes** (the direct payoff of ADR-061's zero redundancy) — not worse than a SQLite `UPDATE`.
- **The cost is heavy.** `PmStore` has 31 methods, `store/tree.rs` is 4143 lines, and PM has 164 tests.
- **Operational regression.** Fixing data goes from "open a text editor" to "install the sqlite3 CLI". For a desktop app whose data lives in `~/.acowork/`, that is a real loss.
- **It would mask the hotspot rather than fix it.** SQLite fixes neither the N+1 nor "503 KB to render two integers". Migrating first means using a 4000-line migration to bury a 60-line problem — and the migration's own performance numbers would be polluted by the hotspot, so its value could not be attributed.

---

## 3. Decisions

### D1: The tree remains authoritative

ADR-061 is unchanged. `task.json` / `project.json` and physical nesting are the only truth; the index remains derived (rebuildable, idempotent).

### D2: Reads must go through the in-memory index

The test: **if the index already holds the field, hitting the disk is a defect.**

Two fixes:

- `compute_blocked_by` is now pure index: `TaskEntry::blocks` (= `depends_on` filtered to `kind == Blocks`) replaces "read the task, then read `depends_on`", and dependency status also comes from the index. The blocking predicate is word-for-word the dispatch scanner's gate: `Done` / `Cancelled` satisfies a prerequisite, everything else (including a dangling id) blocks.
- Project badge tallies became a single index aggregation, see D3.

### D3: Add `GET /projects/counts`

- Response: `[{ project_id, total, submitted }]`, produced by one pass over `TaskIndex::by_id` — **zero disk reads, no task materialised**.
- Only projects with **at least one task** are returned; callers treat a missing project as `{0, 0}` (the client already seeds zeros).
- The route is a static segment coexisting with `/projects/{pid}` (`matchit` prefers literals); a router-level test locks this down.
- Boundary against existing endpoints: `enable/disable` is App admission and unrelated; this is purely a list projection.

### D4: Three corruption-defence layers, format-independent (**not yet implemented**)

1. **Quarantine**: when `walk_tasks` meets an unparseable `task.json`, move it to `{data_dir}/.corrupt/{ts}/…` (keep the original, never delete), continue the rebuild, and surface the list through `/health` or `/diagnostics`.
2. **Keep the last good version**: before overwriting, `atomic_write_json` copies the old content to `*.json.bak` **only if the target currently parses**. Parse-first prevents a PM write from replacing the only good backup with content an external process already corrupted. The `.bak` suffix keeps `walk_tasks` from treating it as a `.json` to index.
3. **Guard `shell` against protected prefixes**: scan the command string before execution; on a hit against `~/.acowork/` (including `$HOME` / `$USERPROFILE` / expanded absolute forms, separators normalised) reject and point at "platform data directory — use the corresponding App's API".

**State the boundary honestly**: string scanning cannot stop variable interpolation or relative paths after a `cd`. It is **misuse mitigation, not a security boundary**. The threat model is "an Agent writes the wrong path", not "a malicious attacker", and that covers the overwhelming majority of real cases. The end state is OS-level sandboxing (Windows Job Object / AppContainer, Linux namespaces), out of scope here.

> None of this depends on the storage format. If SQLite happens later, layers 1 and 2 map directly (quarantine bad rows, snapshot before write) and layer 3 is unchanged.

### D5: Triggers for the SQLite migration

Any one of these starts it; absent all of them, it does not start:

| # | Trigger | Why |
|---|---|---|
| T1 | A store reaching **≥ 10 000 tasks** with `PM index rebuilt count=N` **exceeding 1 second** | A tree's cold start is O(N) disk reads; SQLite open is O(1) regardless of size (the core ADR-082 argument) |
| T2 | **Cross-project full-text search / aggregation** becomes a requirement (FTS5 turns into a hard need) | Today cross-project search is index + title substring, with real limits |
| T3 | A **second process** needs to read and write PM data concurrently | A file tree has no write lock; two writers will collide |
| T4 | **Field-level audit history** is required (who changed which field when) | `activity.json` is an activity feed, not a field diff |

Measure the T1 baseline now and record it (rebuild time versus `count`) so a future decision has data.

### D6: Migration method (if triggered)

Follow the ADR-082 path that worked:

1. Leave the 31-method `PmStore` trait boundary alone; add `SqlitePmStore`.
2. **Run the suite against both backends** — all 164 tests green on each.
3. One-shot import: `walkdir` the tree into the database; make the importer idempotent.
4. Flag-gated rollout → observe → delete the old implementation.
5. Attachments stay on the filesystem; the DB stores `storage_path`.

**No** rewrite-then-switch.

---

## 4. Rejected alternatives

| Alternative | Why rejected |
|---|---|
| Migrate to SQLite now | Scale does not support it (1.6 MB / 83 tasks); atomicity is no better (reparent is already a single rename); heavy cost (31 methods / 4143 lines / 164 tests); operational regression; and it masks the §2.2 hotspot so the migration's own effect cannot be attributed |
| Persist only the index into SQLite, tree stays authoritative | The only gain is skipping the cold-start walk. At 143 files that is milliseconds — zero gain — while introducing a new failure mode ("index disagrees with files") and contradicting ADR-061's "the index is a cache, always rebuildable" |
| Keep the N+1 but add an HTTP cache on `/projects/{pid}/tasks` | A cache introduces "the index is not the truth" and needs invalidation. The home screen never wanted task entities; fixing the caller is 60 lines, caching is more work for worse semantics |
| Store `total` / `submitted` inside `project.json` | Violates ADR-061's zero-redundancy rule (double write, guaranteed drift). Counts are derived; they belong on the index side |
| Use filesystem ACLs to block external writes | Cannot stop a same-user Agent process (same identity). Ineffective for this threat model |

---

## 5. Implementation record (done this round)

| File | Change |
|---|---|
| `core/acowork-pm/src/store/tree.rs` | `compute_blocked_by` now index-only; new `project_counts` (index-only); `HashMap` import |
| `core/acowork-pm/src/types.rs` | New `ProjectCounts` (`project_id` / `total` / `submitted`) |
| `core/acowork-pm/src/api/projects.rs` | New `counts` handler |
| `core/acowork-pm/src/api/routes.rs` | Registered `GET /projects/counts` |
| `apps/acowork-desktop/src/lib/pm-types.ts` | New `PmProjectCounts` |
| `apps/acowork-desktop/src/lib/pm-api.ts` | New `listProjectCounts()` |
| `apps/acowork-desktop/src/stores/pm/projectStore.ts` | `refreshCounts` uses one call; no N+1 |
| `apps/acowork-desktop/src/stores/pm/projectStore.counts.test.ts` | New: single call / absence means zero / failure does not blank the board |
| `core/acowork-pm/src/store/tree.rs` `rebuild_index` | Log line gained `elapsed_ms` — §7 open question 1, landed; the D5-T1 threshold is now evaluable |
| `apps/acowork-desktop/src/stores/pm/projectStore.ts` | New `errorStatus` (keeps the HTTP status) so "service not ready, 503" is distinguishable from a real error |
| `apps/acowork-desktop/src/views/ProjectsView.tsx` | Four first-load states: loading / service booting / real error / genuinely empty. Skeleton moved to `surface-strong` with a spinner and copy; 503 and network failures retry on a 1→2→4→8 s backoff (max 15) |
| `apps/acowork-desktop/src/views/pm/ProjectBoard.tsx` | Same skeleton colour fix; the view now derives `loading` from "this project has never loaded successfully" |
| `apps/acowork-desktop/src/views/ProjectsView.boot.test.tsx` | 5 new cases: loading never reaches the empty state / 503 shows booting / 4xx also never empty / only a successful empty response shows empty / board is loading on project switch |
| `src/i18n/locales/{zh-CN,en}.json` | Added `pm.loadingProjects` / `pm.bootingTitle` / `pm.bootingDesc` / `pm.bootingRetry`; also fixed `pm.offlineHint` in zh-CN, which still carried the English string |

**The first-load state machine (why this was a bug, not just a missing animation)**

The original branch was `projects.length === 0 && !loadingProjects`, which merged two unrelated situations:

- **cold start** — the Gateway accepts `/api/pm/*` before the PM process has finished rebuilding its index, so the first request legitimately answers `503`; `loading` goes false and `projects` is empty;
- **a genuinely empty store**.

Both rendered "No projects yet". On a cold start that reads as a freeze and as **data loss** — and this app had already been mistaken for having lost every project once (§2.1). `projectsUpdatedAt === null` is now the "never loaded successfully" predicate: show visible boot feedback, retry automatically, and **only render the empty state after a successful response that really is empty**.

**The invisible skeleton was the other half**

The placeholder blocks were `bg-surface` (#27272a) on `bg-page-bg` (#131316) — about **1.25:1** contrast, then halved again by `animate-pulse`'s 50% opacity. In the dark theme that is effectively nothing. Switched to `surface-strong` (#52525b) and paired with a spinner and status copy, there is now unmistakable feedback even if the blocks stay subtle.

**Tests**: `cargo test -p acowork-pm --lib` → **164 passed** (2 new; both prove the "no disk" property by deleting the files and asserting the result is unchanged, rather than by adding an I/O probe); `vitest src/stores/pm src/views/ProjectsView.boot.test.tsx` → **24 passed** (2 + 5 new).

**Measurements** (same machine, same store, warm cache, the 82-task project):

| Metric | Before | After |
|---|---|---|
| Home badge source | 4 × `listProjectTasks` | 1 × `GET /projects/counts` |
| Home payload | 503 KB (largest project) + small ones | **106 B** |
| Home disk opens | ≈ 170 | **4** (the `project.json` reads in `list_projects`) |
| Home latency (warm) | ≈ 155 ms | **≈ 8 ms** |
| `GET /projects/{pid}/tasks` (82 tasks) | 145–148 ms | **75–92 ms** (−48%, identical payload) |

A/B via an isolated instance: old and new binaries each against a copy of the data on ports 18082 / 18183, with the user's live process untouched.

**Cold-start baseline** (first measurement after adding `elapsed_ms`; 85 tasks, warm cache):

| Metric | Measured |
|---|---|
| `rebuild_index` duration | **55 ms** |
| Process spawn → `/health` 200 (end to end) | **415 ms** |

Conclusion: **at the current scale the rebuild (55 ms) is not the bottleneck.** The "slow startup" feel came from the read path (now fixed), from cold I/O, and from rendering "service not ready" as an empty store. But the cost is linear: extrapolating 0.65 ms/task, 10 000 tasks is roughly **6.5 s** (worse on a cold cache) — which is exactly where D5-T1 comes from, and why the first-load state has to exist.

---

## 6. Consequences

**Positive**

- Home latency is decoupled from task volume: badges are an index aggregation, so 83 → 10 000 tasks only costs one more memory pass.
- `compute_blocked_by` drops from one disk read to O(dependencies) in memory, benefiting every caller (list views, `only_blocked` filtering).
- Blocking and the dispatch gate now share one data source, reducing the chance of the two drifting apart.
- The tree's authority is intact: the index is still a cache, and deleting it still rebuilds by `walkdir` (the new tests rely on exactly that).

**Negative / costs**

- `/projects/{pid}/tasks` still returns full entities (503 KB). The board needs that; it is not a defect — but it must never be reused to compute badge counts.
- `project_counts` is an O(N) memory pass. At 10 000 tasks it is a few milliseconds, acceptable; beyond that, D5 T1 applies.
- D4's three layers are unimplemented, so external writes are still possible — no longer fatal, since tolerance is in place.

**Neutral**

- The client gained one more contract surface. It is deliberately narrow (three fields) and cannot grow into a second task list.

---

## 7. Open questions

1. ~~**How to measure T1**: `rebuild_index` logs `count` / `skipped` but no duration.~~ **Resolved**: the log line now carries `elapsed_ms`; first reading is 55 ms at 85 tasks (§5). Record it before each release so a curve exists.
2. **Should `.corrupt/` surface in the UI**: today only the log has the path, and users do not read logs. Leaning towards a non-blocking notice on the project list.
3. **Beyond the command scan**: should PM's data directory carry a process-held `.lock` recording the last writer, for post-hoc attribution? Observable only, not preventive.
4. **`/projects` itself still opens 4 files**: not worth changing at this project count. At hundreds of projects, `list_projects` should also read from the index (which currently holds no project entries — a `by_project_meta` map would be needed).
5. **Attachment metadata lives inside `task.json`**: one corrupt file also loses attachment index entries. The `attachments/` directories survive and are rebuildable, but the rebuild rules are undefined.

---

## 8. Related documents

- [ADR-061: PM storage — directory tree + physical nesting + zero redundant fields](./ADR-061-pm-storage-tree.md)
- [ADR-082: memory storage migrated to SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md)
- [ADR-064: PM as a standalone process](./ADR-064-pm-standalone-process.md)
- [Design doc 21: PM project management](../design/en/21-pm-project-management.md)
