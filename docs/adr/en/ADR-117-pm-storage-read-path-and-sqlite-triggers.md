# ADR-117: PM storage evolution — fix the read path first, then converge on SQLite as the single backend

**Status**: Decided and amended (read-path fix and D4's three corruption-defence layers implemented; on 2026-11-16 the SQLite migration started and the three-state rollout landed steps ①–⑥; **on 2026-11-17 the user explicitly dropped `filetree` and converged on SQLite as the single backend**: `PmConfig` defaults to `db`, `build_store()` runs a one-shot JSON-tree import automatically on first start, `FileTree` / `Dual` are config-compatibility types that are refused at assembly time, and step ⑦'s maintenance window plus its manual operator commands are now replaced by code; ⑧ transitional-code cleanup completed on 2026-11-17 (the three-state rollout scaffold, offline reconciliation, operator subcommands and the one-shot layout migration were deleted), see §3 D6 / §5)
**Date**: 2026-11-16
**Decided by**: 大鱼
**Predecessors**:
- [ADR-061: acowork-pm storage — directory tree + physical nesting as authority + zero redundant fields](./ADR-061-pm-storage-tree.md)
- [ADR-082: memory storage migrated to SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md) (source of the migration methodology and its lessons)
- [ADR-064: PM as a standalone process](./ADR-064-pm-standalone-process.md)

---

## 1. Decision summary

### 1.1 One sentence

**Fix the read path first, add SQLite as a configurable second implementation, then converge on SQLite as the single backend.** The read path and D4 corruption defence are done; the migration was a plan with quantitative triggers until the user started it unconditionally on 2026-11-16, first landing the `storage_backend = filetree | db | dual` three-state rollout. Hard invariant throughout: **exactly one commit point at any time**, never an equal-weight dual write. **On 2026-11-17 the user explicitly dropped `filetree`** and converged on SQLite as the only backend: the three-state config, `dual` projection, `migrate` / `reconcile` subcommands and the one-shot layout migration were all deleted in step ⑧. Only `SqlitePmStore` is assembled at runtime; first start imports the historical JSON tree automatically and retires `projects/` as `projects.retired`.

### 1.2 Key decisions

| # | Decision | Outcome |
|---|---|---|
| D1 | Storage authority | **Converged on SQLite as the single backend (2026-11-17).** `pm.sqlite` is the only commit point, `assets/projects/{pid}/` holds binaries, and `projects.retired` is a recoverable snapshot only; `filetree` / `dual` and the three-state config were deleted in ⑧. Historically ADR-061's four reasons (literal = logical / one atomic directory operation / zero redundancy so no double-write / idempotent walkdir rebuild) supported the tree, and the grey rollout let config pick the commit point; the new backend's hard constraint was always **one truth at a time**, never an equal-weight dual write |
| D2 | Read path | **Reads must go through the in-memory index.** Any implementation that filters on the index and then "tops it up" with `get_task()` is a defect: when the index already holds the field, hitting the disk is pure waste. `compute_blocked_by` and the project badge tally are fixed |
| D3 | New contract | `GET /projects/counts` (index-only, `total` / `submitted` per project). The home screen no longer does an N+1 over full task entities to render badges |
| D4 | Corruption defence | **Independent of storage format, implemented (see §5).** Three layers: leave a bad file **in place** and copy a forensic duplicate into `.corrupt/`, keep the last parseable version as `.bak` before overwriting, and **guard `shell` against protected prefixes** (`~/.acowork/`, whose path comes from the single source `acowork-core::workspace`). Layer three is the actual stop-loss; one and two only guarantee "damage is recoverable" |
| D5 | Migration trigger | The T1-T4 quantitative gates **were overridden by the user's explicit instruction on 2026-11-16**: the migration starts unconditionally. T1-T4 are demoted to scale baselines / regression criteria, no longer gating conditions — see §3 D5 |
| D6 | Migration method | Keep the `PmStore` trait object-safe; extract a `PmMaintenance` side interface for startup / dispatch / derived reads / project assets / diagnostics (everything non-CRUD) so a DB backend is not forced to implement tree-only operations. The three-state rollout scaffold (factory + `DualPmStore` + `reconcile` / `migrate` subcommands) was deleted in ⑧; only `SqlitePmStore` is assembled at runtime, with a one-shot automatic JSON-tree import and archive retirement on first start. Import contract and the historical rollout design are in §3 D6 |

### 1.3 Explicitly out of scope

- Attachments (`attachments/`) do not move into the DB. Binaries stay on the filesystem, metadata stays in `task.json` (ADR-061 decision 5).
- No **equal-weight** "tree + SQLite dual write". Dual write means two truths, which is exactly what ADR-061's zero-redundancy rule exists to prevent. D6's `dual` is not dual write but a **one-way projection**: after the tree commits, best-effort sync into SQLite; the DB never decides write success, never becomes the truth, and is fully rebuildable (see §3 D6; the implementation was deleted in ⑧).
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

### D4: Three corruption-defence layers, format-independent (**implemented**)

1. **Forensic copy**: when `walk_tasks` meets an unparseable `task.json`, it **copies** it to `{data_dir}/.corrupt/{project_id}/{task_id}/task.json` and **leaves the original in the authoritative tree** — moving it away would delete the task from the tree, which directly contradicts ADR-061's "the tree is the authority". The fixed path (rather than `{ts}/`) stops repeated restarts from accumulating duplicates. The rebuild continues, and records (project / task / original path / copy path / error / time) are exposed as `corrupt_files` / `corrupt_count` on `GET /health`; the list is rebuilt from scratch on every `rebuild_index`, so a repaired file drops off. `skipped` now counts every skipped entry, and the remaining `?` propagations in `rebuild_index` / `walk_tasks` are gone — `walk_tasks` no longer returns `Result` at all — so an I/O failure can no longer take the service down or drop an entire project. **Review fixes this round**: descent into `children/` now happens *before* reading `task.json` (a corrupt parent record used to `continue` past the push and swallow the whole healthy subtree), with `saturating_add` on depth; the forensic copy is staged + renamed; `/health` returns only a redacted projection (project/task id, relative path, quarantine flag, timestamp, coarse reason) instead of echoing absolute paths and raw IO errors.
2. **Keep the last good version**: before overwriting, `atomic_write_json` writes the old content to `*.json.bak` **only if the target currently parses as JSON** (one copy per file, overwritten, no rotation, and itself staged + renamed so a half-written backup cannot destroy the only good copy). An already-corrupt target leaves `.bak` alone, so the only good backup cannot be replaced by garbage; a failed backup only logs a warning and never blocks the write. Scans look only for `task.json` / `project.json`, so `.bak` is never indexed. **Boundary**: "parses" means valid JSON syntax, not "deserializes into the PM schema"; a JSON object with a wrong field type still counts as good and can overwrite `.bak` (see §6). **Review fix**: a failed `rename` now removes the staged `*.json.bak.tmp` and the main write's `*.json.tmp`, so failed writes leave no orphans.
3. **Guard `shell` against protected prefixes**: `ShellTool::execute` scans `command` before any spawn; a hit against `~/.acowork/` (including `$HOME` / `$USERPROFILE` / `%USERPROFILE%` and the absolute path expanded from `acowork_core::workspace::platform_data_dir()`, with separators and case normalised) is rejected with a pointer to "platform data directory — use the corresponding App's API". All three shell tool names (`bash` / `powershell` / `shell`) share this one implementation, and the path constant lives in `acowork-core`, so PM's data directory is not hard-coded into the Runtime. **Review fix**: the guard now requires **both** write intent and a protected prefix, so read-only inspection (`ls` / `cat` / `grep` / `Get-Content` …) is no longer blocked; custom data directories from `ACOWORK_PM_HOME` / `ACOWORK_PM_DATA_DIR` and their expanded absolute paths are protected too.

**State the boundary honestly**: string scanning cannot stop variable interpolation, relative paths after a `cd`, or an interpreter one-liner. It is **misuse mitigation, not a security boundary**. The threat model is "an Agent writes the wrong path", not "a malicious attacker"; the end state is OS-level sandboxing (Windows Job Object / AppContainer, Linux namespaces), out of scope here. Note that `shell` is wrapped by `PathGuardedTool` but is not in `is_filesystem_tool()`, so the guard belongs to the tool itself rather than to the path-validation layer. Write intent is a coarse keyword heuristic (redirects, `rm`/`cp`/`mv`, `*-Item` / `Set-Content`, `sqlite3`, …): it can over-reject and can be evaded, and it exists only to lower the probability of an accidental write — never as a security guarantee.

> None of this depends on the storage format. If SQLite happens later, layers 1 and 2 map directly (quarantine bad rows, snapshot before write) and layer 3 is unchanged.

### D5: Triggers for the SQLite migration

**Status change (2026-11-16)**: the user explicitly started the migration unconditionally ("start now, dual backend + flag-gated rollout"). **T1-T4 are therefore no longer gating conditions**; they are demoted to scale baselines and regression criteria. We still record `count` / `elapsed_ms` before each release to build a curve that informs whether `db` should become primary, but they do not block this implementation. The earlier "starts only if one holds" wording is retired.

**Boundary (still valid)**: these are **human decision inputs evaluated before a release**, not runtime auto-detection. The PM process does not watch task counts, does not evaluate `elapsed_ms`, and never imports data or switches backends on its own; the `elapsed_ms` log field only makes the threshold *evaluable*. Every step in D6 is likewise a development / ops action that needs its own scheduling and human confirmation. **This ADR contains no runtime auto-migration or auto-switch mechanism** — auto-migration would tie an irreversible data move to a momentary runtime metric, with far too large a failure domain (one Defender cold-start scan is enough to trip a "> 1 s" threshold). The backend is chosen **once, at startup**, from configuration.

T1-T4 (now scale baselines / regression criteria; both sub-conditions of T1 must hold together):

| # | Trigger | Why |
|---|---|---|
| T1 | A store reaching **≥ 10 000 tasks** with `PM index rebuilt count=N` **exceeding 1 second** | A tree's cold start is O(N) disk reads; SQLite open is O(1) regardless of size (the core ADR-082 argument) |
| T2 | **Cross-project full-text search / aggregation** becomes a requirement (FTS5 turns into a hard need) | Today cross-project search is index + title substring, with real limits |
| T3 | A **second process** needs to read and write PM data concurrently | A file tree has no write lock; two writers will collide |
| T4 | **Field-level audit history** is required (who changed which field when) | `activity.json` is an activity feed, not a field diff |

Measure the T1 baseline now and record it (rebuild time versus `count`) so a future decision has data.

### D6: Migration method (historical: three-state rollout → SQLite single backend)

**Configuration (historical; `StorageBackend`, `storage_backend` and `--storage-backend` were deleted in ⑧)**: `PmConfig.storage_backend`, resolved in this order — CLI `--storage-backend` > env `ACOWORK_PM_STORAGE_BACKEND` > TOML `storage_backend` > default `filetree`. A factory assembled it **once at startup**; there was **no runtime switching**. **Review fix — the TOML layer became real**: `main.rs` gained `--config` (loading `./acowork-pm.toml` by convention when present) and `ACOWORK_PM_DATA_DIR`; `StorageBackend`'s serde aliases were unified with `FromStr` so CLI and TOML accepted the same spellings; an invalid value, malformed TOML or a missing explicit `--config` file is a hard `exit(1)`. Today `config.rs` only keeps the `STORAGE_BACKEND = "db"` constant: `deny_unknown_fields` rejects a leftover `storage_backend` key, clap rejects `--storage-backend`, and a non-`db` `ACOWORK_PM_STORAGE_BACKEND` still exits 1 — a stale deployment must never believe it can switch back.

| Value | Meaning | Commit point | Business reads |
|---|---|---|---|
| `filetree` | Today's behaviour, `TreePmStore` | Tree | Tree + in-memory index |
| `dual` | Observation mode: tree commits, SQLite projects synchronously | **Tree (only)** | Tree + in-memory index, identical to `filetree`; the DB is for offline reconciliation only |
| `db` | Target state, `SqlitePmStore` | SQLite | SQLite |

**Four rules for `dual`** (historical design; implementation deleted in ⑧ — the line between it and the rejected "always dual-write" in §4):

1. Project only **after** the tree commits. The write result is **decided by the tree alone**; a projection failure only warns and increments a counter — it never returns an error and never rolls back the tree.
2. The DB is **disposable derived data**: if it is missing, behind, or fails validation at startup, rebuild it wholesale from the tree (tens of milliseconds at current scale). No `write_seq`, no startup reconciliation, no conflict resolution.
3. The DB never serves a business read. A lagging DB therefore cannot change what a user sees; it only affects how fresh offline reconciliation is.
4. Switching to `db` requires a **maintenance window**: stop writes → full import → `MigrationReport` must pass → change config and restart. Reads must never be flipped to the DB while `dual` is live.

**Trait boundary**: the 31 business methods of `PmStore` stay object-safe and HTTP / MCP / dispatch depend on `Arc<dyn PmStore>`. The reconnaissance also found **15 methods the business layer historically called directly on the concrete `TreePmStore`** (startup rebuild / `start_at` backfill / dispatch candidate scan and state recording / derived reads such as `depth`, `parent_id` and per-project task counts / project icon I/O / the corrupt-file registry / `shutdown`). None of them are business mutations, so all 15 move into a `PmMaintenance` side interface (`PmStore: PmMaintenance`) that each backend answers with whatever it actually owns; the (now deleted) `DualPmStore` delegated the hooks to the tree. A DB backend is thus neither forced to implement `walkdir`-style operations nor to fabricate a `TaskEntry`. Icons stay on the filesystem under ADR-061 decision 5 and are never projected into SQLite.

**JSON → SQLite import contract** (filling the gap in the first draft of D6): clauses 1–6 and 8–10 still run in `import.rs` on first start; clauses 7 and 11–13 describe `dual` / `reconcile` / `rebuild_into` / `CorruptPolicy`, whose implementations were deleted in ⑧.

1. **Dry-run first**: read-only scan of the whole tree producing a `MigrationReport` (project / task / attachment counts, skipped entries, a reason for every inconsistency) without writing anything; abort on any blocking finding.
2. **Consistency checks**: directory name ↔ `task.id` ↔ `project_id` must agree. `define_id!`'s `#[serde(transparent)]` Deserialize **does not call `parse()`**, so the importer must explicitly `parse::<TaskId>()` / `parse::<ProjectId>()` instead of trusting ID strings inside JSON. Fields with `#[serde(default)]` must distinguish a legitimate default from semantic damage, and every default taken must be recorded in the report.
3. **Model inversion**: physical `children/` nesting → `parent_id` / `depth`, with cycle detection and a `max_task_depth` check.
4. **Sidecar ownership**: enumerate `activity.json`, `project.json`, icons, `attachments/`, `children/`, `.trash/`, `.corrupt/`, `.bak` one by one. Only business entities are imported; forensic copies and backups never enter the DB.
5. **Attachment base path**: `AttachmentMeta.storage_path` / `thumb_path` are **relative to the task directory**, so the DB must also persist the `task_dir` base. Binaries stay on the filesystem (ADR-061 decision 5).
6. **Write safety**: bound parameters, one all-or-nothing transaction, skip a non-empty target (idempotency gate), `PRAGMA integrity_check` + `foreign_key_check`, and `user_version` for the schema version. Reuse the stage → commit → report pattern from `acowork-memory/src/sqlite/legacy_import.rs` and `acowork-user/src/chat_sessions.rs`; **ADR-082 §4's "delete the import code afterwards" does not apply to PM production data**.
7. **Rollback window**: during `dual`, flip back to `filetree` at any time (the DB is only a projection and can be dropped). After switching to `db`, keep a read-only archive of the tree for at least one release; rollback = stop → flip back to `filetree` → restore from the archive.
8. **Nested IDs and path safety**: `depends_on[].task_id`, `result.attachment_ids[]`, `TaskActivity.id` and other nested IDs must be explicitly `parse()`d too; `storage_path` / `thumb_path` are validated component-by-component as safe relative paths (`safe_rel`), with absolute paths and `..` blocked, plus a `canonicalize()` + root-containment check at read time.
9. **Legacy activity streams**: for a task with no `activity.json` but a `result`, the tree's `load_activity` seeds one `Submitted` entry in memory. The importer must call the same `seeded_activity()` helper, or the history silently disappears after a `db` cutover — and `reconcile` would give a false green because both sides have zero rows (23 such tasks in the real store).
10. **Directory I/O is never silent**: a `read_dir` error, a `p-`-prefixed directory whose ID fails to parse, a missing `project.json` beside a non-empty `tasks/`, or a failed `strip_prefix(data_dir)` are all blocking findings rather than sidecar skips. Silently dropping a project is worse than refusing to start.
11. **Two corruption policies**: one-shot `import` / `dry_run` / `reconcile` use `Blocking` (a new authority is being created, so bad files must stop it); `dual` full projection rebuild uses `SkipAndRecord` (the tree already skipped them, so the projection must too — one bad file must not stall the rollout forever), while `reconcile` still refuses a green light.
12. **Backend hard gate**: `rebuild_into()` asserts `storage_backend == dual` — under `db` the database is the authority and must never be overwritten by a tree scan. A projection that fails to open is renamed to `.quarantine-<ts>` together with `-wal` / `-shm`, never deleted, because a transient failure must not destroy a healthy database.
13. **Reconciliation is more than `props`**: projected columns (`parent_id` / `depth` / `status` / `task_dir` …) are compared too, since the DB backend actually reads them; otherwise column drift would still earn a false green light.

**Rollout and acceptance order**: ① three-state config + factory ✅ → ② object-safe `PmStore` + `PmMaintenance` ✅ → ③ `SqlitePmStore` + importer ✅ → ④ `DualPmStore` ✅ → ⑤ **run the existing suite against all three** `filetree` / `db` / `dual` backends (same cases, no DB-specific skips) ✅ → ⑥ `dual` observation and offline reconciliation capability ✅ → ⑦ switch to `db` ✅ (on 2026-11-17 this became a one-shot automatic import at startup — no maintenance window and no manual commands) → ⑧ delete the old implementation ✅ (completed 2026-11-17, see "⑧ transitional-code cleanup" below). The full pre-push review fixed every confirmed defect (see §5); automating ⑦ does not relax the "exactly one commit point at any time" constraint, and ⑧ deletes only code outside that live path.

**Single-backend landing (2026-11-17)**: `PmConfig::default()` is now `db`; `build_store()` assembles only `SqlitePmStore` and, on first start, probes the DB read-only for emptiness before running `dry_run` → one-transaction `import` automatically; a blocking finding aborts startup. Every start then runs the idempotent `retire_tree_archive()`: project icons and task attachment binaries are copied under `assets/projects/{pid}/` (`icon/` beside `tasks/{tid}/`), the `tasks` / `attachments` `task_dir` columns are rewritten, the absence of any remaining `projects/` reference is verified, and `projects/` is renamed to `projects.retired` (renamed, never deleted — the snapshot stays recoverable). From then on `pm.sqlite` + `assets/` are the only data source. `FileTree` / `Dual` are config-compatibility types refused at assembly time; `dual` projection and `reconcile` left the runtime path then, and both implementations were deleted in ⑧.

**Asset directory layout (revised 2026-11-17)**: `assets/` is grouped by project — `assets/projects/{pid}/icon/` holds project icons and `assets/projects/{pid}/tasks/{tid}/attachments/` holds task binaries, so one project's entire binary footprint lives in a single `{pid}` subtree and `delete_project` can clean it in one pass without leaving orphan directories. The first cut used a flat `assets/tasks/{tid}/` sibling to the icon tree, which made project-scoped cleanup impossible. The one-shot `import::normalize_asset_layout()` startup migration upgraded the 93 existing rows of the live store at 23:29 on 2026-11-17 and was then deleted in ⑧; a fresh install only ever produces the canonical layout. This is safe because `create_task_as` / `reparent_task` both reject cross-project parents, so `tasks.project_id` never changes and no directory ever has to move between projects.

Scope of ③ (historical; superseded by ⑦/⑧): `SqlitePmStore` implements all 31 `PmStore` business methods plus the 15 `PmMaintenance` hooks, and `db` is wireable through the factory for REST / MCP / dispatch. The importer is an **explicit operator command** — `acowork-pm --data-dir <dir> migrate` (read-only dry run by default, `--apply` to write) — never a runtime side effect of startup. The factory hard-fails on "empty DB + non-empty tree" to force the maintenance window, and newly uploaded attachments in `db` mode go under `{data_dir}/assets/` (now project-scoped, see above) so the tree archive stays a clean rollback source. **Review hardening**: icons moved out of `projects/{pid}/icon` (the rollback source) into `assets/projects/{pid}/icon` with a read-only archived fallback; `schema::open()` now checks `user_version` *before* WAL/DDL, so refusing an unknown database leaves zero writes (an unversioned DB that already has PM tables is refused too) and `integrity_check` collects every result row; the `db` refusal path runs a dry run first, so a rejected startup can never conjure a `pm.sqlite` that a later `--apply` would mistake for a non-empty target.

Scope of ④⑤⑥ (historical; implementations deleted in ⑧): `DualPmStore` delegates all 31 business methods and 15 maintenance hooks to the tree — reads via `dual_read!`, writes via `dual_write!` (projection happens only after a successful tree commit, and its result is discarded). The projection is a **full tree scan + single replace-all transaction** rather than 31 hand-written incremental hooks: it costs O(tree) per write but makes it impossible to miss a field, a sidecar or a subtree during the observation window. A `projection_lock` serializes "scan → replace" so concurrent writes cannot let an older snapshot commit last. When the DB cannot be opened at all, the store degrades to tree-only and never blocks business traffic. `reconcile` is a **read-only** gate: it opens `pm.sqlite` read-only (never via `schema::open()`, which would create tables, set WAL and bump `user_version`), refuses to give a green light for a missing DB, a version skew or an integrity failure, compares entity key sets, `props` blobs and projected columns verbatim, and makes the CLI exit non-zero on any mismatch. `/health.details.projection` is served by an **instance-scoped** provider exposing `ok` / `failed` / `consecutive_failures` / `last_error` / `last_report`, turning "the DB is behind" into a reconciliation signal instead of a service failure. **Review hardening**: `tree_write_seq` makes a projection re-scan when a tree write landed during its scan, so a half-moved subtree can never be committed as the final DB state; an unopenable projection DB is quarantined instead of deleted, and non-`dual` backends are refused outright; `dispatch_identity` now matches the tree (an existing identity returns without any write), and `migrate_null_start_at` / `shutdown` no longer trigger gratuitous projections; both backends' `reparent_task` now validate the whole subtree's depth after the move, and SQLite clamps `depth` updates with `MAX(0, …)` so a negative value cannot wrap into a bogus `u8`.

**⑧ transitional-code cleanup (2026-11-17)**: after the live store had completed the cutover, archive retirement and asset-layout migration, every piece of code that was only useful during the migration was deleted — the `StorageBackend` enum, `PmConfig.storage_backend`, the `--storage-backend` CLI flag, `store/dual.rs` (`DualPmStore`, projection metrics, `dual_read!` / `dual_write!`), `PmMaintenance::projection_health`, `rebuild_into()` / `rebuild()` / `reconcile()` / `ReconcileReport`, the two `CorruptPolicy` modes, the `migrate` / `reconcile` subcommands and their re-exports, `/health.details.projection`, the archived-icon fallback and the one-shot `normalize_asset_layout()`. `config.rs` keeps only the `STORAGE_BACKEND = "db"` constant and a guard that makes leftover rollout config fail loudly; `ensure_sqlite_authority()` keeps only the one live path: read-only emptiness probe → dry run → single-transaction import → `retire_tree_archive()`. `store/tree.rs` / `index.rs` are deliberately kept: they define the `PmStore` / `PmMaintenance` traits, hold the five rule helpers the SQLite backend reuses, and generate real tree layouts for nine integration suites and the importer tests; `build_store()` no longer assembles them.

---

## 4. Rejected alternatives

| Alternative | Why rejected |
|---|---|
| Migrate to SQLite now | Scale does not support it (1.6 MB / 83 tasks); atomicity is no better (reparent is already a single rename); heavy cost (31 methods / 4143 lines / 164 tests); operational regression; and it masks the §2.2 hotspot so the migration's own effect cannot be attributed |
| Persist only the index into SQLite, tree stays authoritative | The only gain is skipping the cold-start walk. At 143 files that is milliseconds — zero gain — while introducing a new failure mode ("index disagrees with files") and contradicting ADR-061's "the index is a cache, always rebuildable" |
| Keep the N+1 but add an HTTP cache on `/projects/{pid}/tasks` | A cache introduces "the index is not the truth" and needs invalidation. The home screen never wanted task entities; fixing the caller is 60 lines, caching is more work for worse semantics |
| Store `total` / `submitted` inside `project.json` | Violates ADR-061's zero-redundancy rule (double write, guaranteed drift). Counts are derived; they belong on the index side |
| Use filesystem ACLs to block external writes | Cannot stop a same-user Agent process (same identity). Ineffective for this threat model |
| Always dual-write (write JSON + SQLite, read only from DB, JSON as backup) | Writes really are single-task and cheap (one process, one writer; one extra transaction costs ~1-3 ms), so **performance is not the issue**. The problem is two commit points that are not atomic together: a file `rename` is atomic, a DB transaction is atomic, the pair is not — the number of crash windows equals the number of writes, so lighter and more frequent writes mean more windows. Making it reliable needs a `write_seq` plus startup reconciliation, which turns the JSON "backup" into a versioned second master and breaks ADR-061's zero-redundancy rule (dual write was already rejected in §1.3). Falling back to "single write to DB + one-way JSON export" has one commit point, but it **triggers D5 unconditionally**: the read path is already fixed (155 ms to 8 ms), `rebuild_index` is 55 ms and not a bottleneck, so there is no gain at this scale while giving up "fix data in a text editor" and the tree-as-authority invariant. PM writes are not all single-task either: `reparent_task` updates a whole subtree's `depth`/`dir_path`, `delete_task` removes a subtree, `delete_project` archives a whole project, and a dispatch tick writes `dispatched_at` in bulk. Verdict: not worth it; the tree stays authoritative. **This does not rehabilitate §3 D6's `dual`**: `dual` has one commit point (the tree), the DB neither decides write success nor serves business reads, and it is fully rebuildable — a one-way projection, not a dual write |

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
| `core/acowork-pm/src/store/corrupt.rs` | D4-1 new: `CorruptRecord` / `CorruptRegistry` / `quarantine_copy` (fixed path, idempotent, original untouched) + 2 unit tests |
| `core/acowork-pm/src/store/tree.rs` | D4-1: `TreePmStore` holds the corrupt-file registry; `rebuild_index` / `walk_tasks` lose the remaining `?` propagations (`walk_tasks` no longer returns `Result`), count every skipped entry, and copy + record bad files; new `corrupt_files()` / `corrupt_registry()` + 1 end-to-end test (corrupt → forensic copy → repair → list clears) |
| `core/acowork-pm/src/health.rs`, `server.rs` | D4-1: `/health` gains `corrupt_files` / `corrupt_count`, sharing the store's registry |
| `core/acowork-pm/src/store/atomic.rs` | D4-2: keep `*.json.bak` before overwrite only when the target parses (staged + renamed atomically, one copy, overwritten, failures only warn) + 2 unit tests |
| `core/acowork-core/src/workspace.rs` | D4-3: new `PLATFORM_DATA_DIR` / `platform_data_dir()`, the single source for protected prefixes |
| `core/acowork-runtime/src/tools/builtin/shell.rs` | D4-3: `protected_prefix_hit()` + rejection before spawn (shared by bash / powershell / shell) + 4 unit tests |
| `core/acowork-pm/src/config.rs` | D6-①: `StorageBackend` (`filetree` / `db` / `dual`) + the `storage_backend` field, from CLI / env / TOML, default `filetree` |
| `core/acowork-pm/src/store/tree.rs` | D6-②: new `PmMaintenance` side interface; `PmStore: PmMaintenance` stays object-safe; `TreePmStore` implements the maintenance hooks |
| `core/acowork-pm/src/api/{mod,routes,tasks}.rs`, `mcp/{mod,tools}.rs`, `dispatch.rs`, `server.rs` | D6-②: `Arc<TreePmStore>` → `Arc<dyn PmStore>`; `index_entry().depth` replaced by `task_depth()` |
| `core/acowork-pm/src/health.rs` | D6-①: `/health` exposes the effective `storage_backend` so a rollout can confirm the config actually took effect |
| `core/acowork-pm/Cargo.toml` | D6-③: reuse the workspace's bundled `rusqlite`; no system SQLite needed |
| `core/acowork-pm/src/store/sqlite/schema.rs` | D6-③: `projects` / `tasks` / `attachments` / `activity` / `meta` tables with projected columns (`parent_id` / `depth` / `task_dir`); WAL, `foreign_keys=ON`, `user_version=1`; `integrity_check` + `foreign_key_check`; a newer schema is explicitly refused |
| `core/acowork-pm/src/store/sqlite/import.rs` | D6-③: `dry_run` / `import` + `MigrationReport`; explicit ID `parse()`, directory name ↔ `task.id` ↔ `project_id` agreement, defaulted-field accounting, `children/` → `parent_id`/`depth`, sidecar ownership, one transaction + non-empty idempotency gate + verify-before-commit; 5 unit tests |
| `core/acowork-pm/src/store/sqlite/mod.rs` | D6-③: `SqlitePmStore` connection layer + entity primitives + 15 `PmMaintenance` methods; dispatch candidate rules aligned with the tree (single lock acquisition, no `Mutex` re-entry deadlock); icons reuse `icon/`; 2 unit tests |
| `core/acowork-pm/src/store/sqlite/business.rs` | D6-③: all 31 `PmStore` business methods; reuses the tree's `validate_transition` / `ensure_creation_approved` / `parse_task_id` / `priority_rank` / `seeded_activity` as one rule source; activity history and task record commit in one transaction; 2 unit tests |
| `core/acowork-pm/src/store/tree.rs` | D6-③: five rule helpers made `pub(super)` so both backends share them — no logic drift |
| `core/acowork-pm/src/server.rs` | D6-③: `build_store()` wires `db`; refuses to start on an empty DB beside a populated tree and points at `migrate --apply`; `dual` still rejected explicitly (step ④); 3 unit tests |
| `core/acowork-pm/src/main.rs`, `lib.rs` | D6-③: new `migrate [--apply]` operator subcommand (dry run by default, prints a JSON report) + importer / `SqlitePmStore` re-exports |
| `core/acowork-pm/src/store/dual.rs` | D6-④⑤⑥: new `DualPmStore`; tree as the only commit point + post-write full projection; `dual_read!` / `dual_write!` delegate 31 `PmStore` + 15 `PmMaintenance` methods; `projection_lock` prevents an older scan from committing last; degrades to tree-only when the DB cannot be opened; 6 unit tests |
| `core/acowork-pm/src/store/sqlite/import.rs` | D6-④⑥: extracted `insert_batch()` shared by the one-shot import and projection rebuild; added `rebuild_into()` / `rebuild()` (single-transaction replace-all) and `reconcile()` / `ReconcileReport` (entity key sets + verbatim `props`); reconciliation opens read-only and never gives a green light for a missing DB, a version skew or an integrity failure; +3 unit tests |
| `core/acowork-pm/src/store/sqlite/mod.rs` | D6-④: `SqlitePmStore::rebuild_projection()`, for `dual` to overwrite derived data only — never called in `db` mode |
| `core/acowork-pm/src/store/mod.rs` | D6-④: export the `dual` module and update the module docs |
| `core/acowork-pm/src/server.rs` | D6-④: `build_store()` wires all three backends; `dual` rebuilds its projection at startup; the old "reject `dual`" test becomes "wireable and consistent at startup" |
| `core/acowork-pm/src/health.rs` | D6-⑥: expose `details.projection` (`ok` / `failed` / `last_error` / `last_report`) in `dual` mode — a lagging projection is a reconciliation signal, not a service failure |
| `core/acowork-pm/src/main.rs`, `lib.rs` | D6-⑥: new read-only `reconcile` subcommand (exit 1 on mismatch) + `DualPmStore` / `rebuild` / `reconcile` / `ReconcileReport` re-exports |
| `core/acowork-pm/src/main.rs`, `config.rs` | Review-A: real `--config` / conventional `./acowork-pm.toml` loader plus `ACOWORK_PM_DATA_DIR`; `StorageBackend` serde aliases unified with `FromStr`; a failed `migrate --apply` also prints the read-only dry-run report |
| `core/acowork-pm/src/store/tree.rs` | Review-A/C: `children/` descent moved before reading `task.json` (a corrupt parent no longer swallows the healthy subtree); new default `PmMaintenance::projection_health`; `reparent_task` validates the whole subtree's post-move maximum depth |
| `core/acowork-pm/src/store/sqlite/import.rs` | Review-A/B: legacy `seeded_activity()` seeding, dual `CorruptPolicy`, `read_dir` / nested-ID / `p-*` parse-failure handling, `safe_rel` path validation, `reconcile` projected-column comparison, `assert_dual_projection` hard gate |
| `core/acowork-pm/src/store/sqlite/business.rs` | Review-A/B: `delete_task` now deletes DB rows first and asset directories best-effort afterwards; attachment paths validated at both insert and read; `create_task_as` / `write_attachment` roll back their FS writes on failure; subtree-depth gate + `MAX(0, …)` |
| `core/acowork-pm/src/store/sqlite/schema.rs` | Review-C: version gate moved ahead of WAL / DDL; an unversioned DB with PM tables is refused; `integrity_check` collects every result row |
| `core/acowork-pm/src/store/sqlite/mod.rs` | Review-B/C: icon root moved out of the tree archive with a read-only archived fallback; `dispatch_identity` matches the tree (an existing identity returns without a write); new icon regression case |
| `core/acowork-pm/src/store/dual.rs` | Review-C: instance-scoped projection metrics + `consecutive_failures`; `tree_write_seq` convergence; an unopenable projection DB is quarantined rather than deleted; non-`dual` backends refused |
| `core/acowork-pm/src/store/{corrupt,atomic}.rs`, `health.rs`, `server.rs`, `acowork-runtime/src/tools/builtin/shell.rs` | Review-C: atomic forensic copies, `.tmp` cleanup on failure, redacted `/health` corrupt-file entries, write-intent + custom-data-dir shell guard, zero DB side effects on a refused `db` startup |
| `core/acowork-pm/src/config.rs` | Single backend: `StorageBackend::default()` is now `Db`; `FileTree` / `Dual` remain as types so archived TOML still deserializes; field docs describe the one backend and the default-assertion test was updated |
| `core/acowork-pm/src/server.rs` | Single backend: `build_store()` assembles only `SqlitePmStore` and calls `ensure_sqlite_authority()` automatically on first start (read-only emptiness probe → dry run → one-transaction import → version gate), then idempotently retires the archive on every start; non-`db` configs are refused; the four assembly tests became "retired backends rejected / first start auto-imports and retires the tree with every file preserved / second start idempotent / zero-config defaults to db" |
| `core/acowork-pm/src/store/sqlite/import.rs` | Archive retirement: new `retire_tree_archive()` (icons → `assets/projects/{pid}/icon/`, attachments → `assets/projects/{pid}/tasks/{tid}/`, `task_dir` rewrite, verify no lingering reference, `projects/` → `projects.retired`); resumable after a crash; new "binaries relocated + rename + idempotence" regression case |
| `core/acowork-pm/src/store/sqlite/{mod,business,import}.rs`, `server.rs`, `api/icon.rs` | Project-scoped asset layout: `task_asset_rel()` is the single source for `assets/projects/{pid}/tasks/{tid}` and `is_db_owned_asset_dir()` applies a strict shape check; `delete_project` removes the whole `{pid}` subtree in one pass; new `normalize_asset_layout()` idempotently upgrades existing flat rows (including `attachments` rows and a physical directory rename) on startup and is wired into `ensure_sqlite_authority()`; two new regression cases (layout migration + project cleanup) |
| `config.rs` / `main.rs` / `health.rs` / `lib.rs` / `server.rs` / `store/{mod,dual,tree}.rs` / `store/sqlite/{mod,import,schema,business}.rs` | ⑧ transitional-code cleanup: delete the three-state config, `DualPmStore`, projection / reconciliation / rebuild, the `migrate` / `reconcile` subcommands, the archived-icon fallback and the one-shot layout migration; `/health` reports `db` unconditionally; new regression case for hard-rejecting a leftover `storage_backend` key |

**Migration progress (final state)**: D6 steps ①–⑧ are all complete. At 22:25 on 2026-11-17 the live store restarted, imported automatically and retired the archive; at 22:45 a task created from the UI was verified to land only in `pm.sqlite` + WAL with zero writes to `projects.retired`; at 23:29 a second restart completed the 93-row asset-layout migration; ⑧ then deleted every transitional implementation. Today `pm.sqlite` + `assets/projects/{pid}/` are the only data source, `projects.retired` is a recoverable snapshot, and no second commit point exists at runtime. The ①–⑥ implementation details and both review rounds remain in the table as history; the then-required `migrate --apply` / `reconcile` gates served their purpose and were deleted with ⑧.

**Pre-push review and real-data re-verification (this round)**: the real `data_dir` is `C:\Users\nicholas\.acowork\acowork-pm`. A full backup was taken first at `.workbuddy/backups/pm-data-20261116-2300` (robocopy, 157/157 files; two Bash `cp` attempts were killed by the environment, PowerShell + robocopy succeeded), then only read-only commands were run. Initial dry run: `projects=4 tasks=91 activity_rows=168 skipped=[] blocking=[]`; a precise count found **39 tasks without `activity.json`, 23 of them carrying a `result`** — exactly the `seeded_activity()` legacy shape, which would have erased history after a `db` cutover and produced a false reconciliation green light. After the fix (the live store had meanwhile grown to 92 tasks / 53 `activity.json` files / 175 rows / 159 files): `activity_rows=198`, matching `175 + 23` exactly, with `skipped=[] blocking=[]`; file count stayed 159 before and after and no `pm.sqlite` appeared. `reconcile` on the tree-only install exits 1 with an explicit "no projection to reconcile" blocking entry, also with zero writes. At that time steps ⑦/⑧ had not run, and no `--apply` or any other write touched the real directory; both were completed on 2026-11-17.

**Verification (review round)**: `cargo test -p acowork-pm` fully green (lib **213 passed** plus 42 cases across 9 integration / doc suites, 255 total); `cargo test -p acowork-runtime --lib shell` 79 passed; `cargo clippy -p acowork-pm --all-targets -- -D warnings` and `cargo clippy -p acowork-runtime --lib -- -D warnings` clean; `./dev/ci.sh redlines` all 14 green; `git diff --check` clean; the 13 changed Rust files were formatted with `--edition 2024 --config skip_children=true` (`tree.rs` was restored and its three edits replayed precisely to avoid 694 lines of unrelated churn). Both ADR language versions were updated in the same commit and remain mirrored.

**⑧ cleanup verification**: `cargo check -p acowork-pm --all-targets` passes; `cargo test -p acowork-pm` fully green (lib **199 passed** plus 42 integration / doc cases); `cargo clippy -p acowork-pm --all-targets -- -D warnings` clean; `./dev/ci.sh redlines` all 14 green; `git diff --check` clean; new and changed files formatted with `--edition 2024 --config skip_children=true`. End-to-end smoke on a copy: a pure-tree install rebuilt from the `projects.retired` snapshot starts with an automatic import + archive rename and `/health` reporting `db`, while a second start logs no import; the real `data_dir` was re-checked read-only with zero writes. Both ADR language versions were updated in the same commit and remain mirrored.

**D6 ③ verification (previous round)**: `cargo test -p acowork-pm` fully green (lib **188 passed** plus 42 cases across 9 integration / doc suites); `cargo clippy -p acowork-pm --all-targets -- -D warnings` clean; `./dev/ci.sh redlines` all 14 green; `acowork-pm --data-dir <tmp> migrate` was run for real — it printed a report and created zero files. Eleven new cases that round: schema / connection 2 (idempotent open + verify, newer schema refused), importer 5 (read-only dry run, full round-trip with parent inversion + idempotency, forged ID blocking, corrupt JSON blocking, new attachment never touches the archive), business 2 (end-to-end smoke of all 31 methods, dispatch does not deadlock when dependencies exist), server 2 (`db` is object-safe and wireable, skipped import is refused).

**D6 ④⑤⑥ verification (this round)**: `cargo test -p acowork-pm` fully green (lib **197 passed** plus 42 integration / doc cases, 239 in total); `cargo clippy -p acowork-pm --all-targets -- -D warnings` clean; `./dev/ci.sh redlines` all 14 green; `git diff --check` clean. Ten new cases landed: `dual.rs` 6 (one shared assertion set across three backends, business reads ignore a tampered DB while reconciliation detects the drift, a failed projection never fails the tree commit, startup rebuilds a stale projection, concurrent writes stay reconcilable, verbatim consistency after a mixed write flow), `import.rs` 3 (reconcile creates no DB, reconciliation after import, version skew refused), and `server.rs` 1 (`dual` is wireable and consistent at startup). One of them replaces the former "reject `dual`" test, so the lib count grew by 9 relative to ③.

**D4 verification this round**: `cargo test -p acowork-pm --lib` → **169 passed** (+5); `cargo test -p acowork-runtime --lib shell` → **77 passed** (+4); `cargo clippy -p acowork-core|acowork-pm|acowork-runtime --lib -- -D warnings` passes on all three crates; `./dev/ci.sh redlines` all 14 green. The workspace-wide `--all-targets -D warnings` run is still blocked by 8 **pre-existing** clippy errors in `acowork-gateway` (`supervisor.rs` / `ticket.rs` / `auth_middleware.rs`), unrelated to this change.

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
- D4's three layers are implemented, but layer 3 is only string-level misuse mitigation: variable interpolation, relative paths after a `cd`, and interpreter one-liners still get through; if the store ever moves to SQLite, `shell` can write `pm.sqlite` just as well. OS-level sandboxing remains the end state.
- Layer 2's "parseable" check stops at JSON syntax: an object like `{"id": 123}` with a wrong field type still counts as a good version and can overwrite `.bak`, displacing the real previous version. Closing that gap needs a `DeserializeOwned` bound on `atomic_write_json`, which the current `activity.json` writer (`&[TaskActivity]`, a slice) does not satisfy, so it is untouched this round. The `.bak` write itself is now atomic, so a backup can no longer be destroyed by its own partial write — but the "semantically broken yet syntactically valid" hole remains.
- The three-state config (deleted in ⑧) made backend choice a **startup decision** and every switch required a restart plus a maintenance window. With only one backend there is nothing left to choose, but online A/B and a runtime rollback path are gone for good.
- With `PmStore` objectified, every business call pays one dynamic dispatch (negligible next to the file I/O inside those 31 methods), but `Arc<dyn PmStore>` also means the concrete implementation is no longer visible at compile time. Regression coverage must now come from SQLite-backend tests, not from the type system (the three-backend suite was deleted in ⑧).
- The SQLite backend moves "single writer" from OS file semantics to one in-process `Mutex<Connection>`: every statement runs synchronously inside that critical section. Correct, but it gives up concurrent reads (WAL now only keeps external read-only inspection from blocking the writer). If a second process ever opens the same `pm.sqlite`, this lock is no longer sufficient and must become connection-level transactions with a busy-timeout policy.
- In `db` mode, attachment binaries never enter the DB (ADR-061 decision 5): they live at `assets/projects/{pid}/tasks/{tid}/` in the same `{pid}` subtree as icons. `delete_project` removes that whole subtree in one pass, while `delete_task` removes only its own DB-owned directory. The cost is that `task_dir` is a path string stored in the DB, so any future layout change needs a new explicit migration (the one-shot `normalize_asset_layout()` was deleted in ⑧); cross-project parenting is rejected, so the `{pid}` segment stays stable for a task's whole lifetime.
- `dual` (deleted in ⑧) traded O(tree) write amplification for the one property a grey rollout needs most — no projection hook could miss a field, a sidecar or a subtree — via a full tree scan plus one replace-all transaction per write. Under a single backend that cost disappears entirely.
- The projection `projection_lock` / `tree_write_seq` (deleted in ⑧) solved half-moved subtrees under concurrent writes. A single SQLite transaction now provides that consistency natively, so there is no longer any cross-medium concurrency window.
- The instance-scoped `/health.details.projection` counters (deleted in ⑧) used to signal that the DB was behind. Under a single backend the DB *is* the authority, so "behind" is not a state that exists; `/health` simply reports `db`.
- `db_err()` collapses every `rusqlite` error into `PmError::Internal` (HTTP 500). Deliberate: a new error variant would ripple through the client-visible `http_status` / `error_code` contract, and with a single in-process writer a constraint violation almost always means a real internal fault rather than bad user input. The cost is that clients cannot yet distinguish a uniqueness conflict from a disk failure — only the logs say which.
- Projected timestamp columns are TEXT produced exclusively by `DateTime<Utc>::to_rfc3339()`, so `ORDER BY start_at` relies on lexicographic order matching chronological order. Both the importer and the write path go through that serializer; **the invariant is that every timestamp is UTC and produced by it**. If externally hand-written timestamps ever enter, this column must become integer epoch or gain a normalization constraint.
- `create_task_as` / `write_attachment` still write the filesystem first and commit to the DB second. This round added best-effort directory rollback on both failure paths, but a process killed between the two can still leave an empty directory or orphan binaries. Fixing that fully needs startup GC or transactional binaries; step ⑧ only removed transitional code and did not add startup GC, so this debt remains.
- `schema::open()` now refuses an unknown database without writing a single byte, which means both "version skew" and "empty DB" recovery require explicit human action (quarantine, then rebuild) instead of automatic convergence on startup. That is the deliberate price of replacing silent migration with a loud error.
- D4 layer 3's write-intent heuristic is lossy: read-only text containing `>`, or a quoted string containing `cp `, can be over-rejected, while a write assembled through variables can still slip through. It lowers the accident rate and must never be written into any security guarantee.
- Converging on one backend removes the runtime rollback path of "switch back to `filetree` and keep working": `projects/` is renamed to `projects.retired` and no runtime path reads it any more, and every post-cutover write lives only in `pm.sqlite` + `assets/`. Rolling back means stopping the service, renaming `projects.retired` back and reverting to an older binary. The gain is that two diverging commit points can no longer exist; the cost is that the cutover is not painlessly reversible.

**Neutral**

- The client gained one more contract surface. It is deliberately narrow (three fields) and cannot grow into a second task list.

---

## 7. Open questions

1. ~~**How to measure T1**: `rebuild_index` logs `count` / `skipped` but no duration.~~ **Resolved**: the log line now carries `elapsed_ms`; first reading is 55 ms at 85 tasks (§5). Record it before each release so a curve exists.
2. ~~**Should `.corrupt/` surface in the UI**~~ **Partly resolved**: the list is exposed as `corrupt_files` on `/health` as a redacted projection (relative path / coarse reason / quarantine flag); the Desktop UI is not wired up yet — leaning towards a non-blocking notice on the project list.
3. **Beyond the command scan**: should PM's data directory carry a process-held `.lock` recording the last writer, for post-hoc attribution? Observable only, not preventive.
4. **`/projects` itself still opens 4 files**: not worth changing at this project count. At hundreds of projects, `list_projects` should also read from the index (which currently holds no project entries — a `by_project_meta` map would be needed).
5. **Attachment metadata lives inside `task.json`**: one corrupt file also loses attachment index entries. The `attachments/` directories survive and are rebuildable, but the rebuild rules are undefined.

---

## 8. Related documents

- [ADR-061: PM storage — directory tree + physical nesting + zero redundant fields](./ADR-061-pm-storage-tree.md)
- [ADR-082: memory storage migrated to SQLite](./ADR-082-memory-storage-sqlite-vector-fts.md)
- [ADR-064: PM as a standalone process](./ADR-064-pm-standalone-process.md)
- [Design doc 21: PM project management](../design/en/21-pm-project-management.md)
