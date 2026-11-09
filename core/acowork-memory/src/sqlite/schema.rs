//! SQLite schema for the workspace store (ADR-082 D1).
//!
//! One `nodes` row per memory node. `props` is the JSON serialization of the
//! node struct (every type-specific field — see the crate docs for the exact
//! contract); `status` / `created_at` / `updated_at` are projected columns so
//! filtering and ordering never have to parse JSON. Embeddings live in
//! `vectors` as little-endian `f32` BLOBs. Each memory label gets its own FTS5
//! table using the `trigram` tokenizer (ADR-082 D3) so CJK text matches as
//! substrings instead of collapsing into one token under `unicode61`.
//!
//! The DDL is idempotent: opening an existing database re-runs it harmlessly.
//! Every open runs [`apply_migrations`], which does two things: it executes the
//! version-gated [`MIGRATIONS`] steps whose target version is strictly greater
//! than the stored `user_version`, and then converges [`TARGET_SCHEMA`] by
//! presence. The second pass is not redundant — `user_version` is a cache, and
//! a database that carries a stamp it never earned (two branches allocating the
//! same version number) stays broken forever under the version gate alone.

use crate::labels;

/// DDL applied on every [`super::SqliteStore::open`]. `IF NOT EXISTS` throughout.
pub const SCHEMA_SQL: &str = r#"
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA synchronous = NORMAL;
-- `user_version` is stamped by `apply_migrations` after every step has run,
-- NOT by this string — see `apply_migrations`.

CREATE TABLE IF NOT EXISTS nodes (
    id         INTEGER PRIMARY KEY,
    label      TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'Active',
    props      JSON NOT NULL,
    created_at TEXT NOT NULL DEFAULT '',
    updated_at TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_nodes_label ON nodes(label);

-- Store-level metadata (currently just the embedding dimension). Keeping it in
-- the database means a reopen reports the dimension actually stored rather
-- than whatever the caller guessed.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS vectors (
    node_id   INTEGER PRIMARY KEY REFERENCES nodes(id) ON DELETE CASCADE,
    dim       INTEGER NOT NULL,
    embedding BLOB NOT NULL
);

CREATE VIRTUAL TABLE IF NOT EXISTS fts_episodic USING fts5(content, node_id UNINDEXED, tokenize = 'trigram');
CREATE VIRTUAL TABLE IF NOT EXISTS fts_knowledge USING fts5(content, node_id UNINDEXED, tokenize = 'trigram');
CREATE VIRTUAL TABLE IF NOT EXISTS fts_procedural USING fts5(content, node_id UNINDEXED, tokenize = 'trigram');
CREATE VIRTUAL TABLE IF NOT EXISTS fts_autobiographical USING fts5(content, node_id UNINDEXED, tokenize = 'trigram');

-- Conversation index (ADR-082 §4 step 2). Separate from the memory labels: it
-- is a different store file driven by a different subsystem (ADR-081), but it
-- reuses the same `nodes` / `vectors` / FTS machinery so `/search` gets the
-- same trigram CJK matching and exact vector scan as memory recall.
CREATE VIRTUAL TABLE IF NOT EXISTS fts_conversation USING fts5(content, node_id UNINDEXED, tokenize = 'trigram');

-- Session metadata (ADR-082 §4 step 3, ADR-024 successor). One row per
-- session, the JSON sidecar (`conversations/meta/*.json`) is now only a
-- bootstrap input on first boot — the runtime reads and writes this table.
-- `last_active_at` is INTEGER epoch-ms, not ISO string, so listing is a
-- range scan + index, not a parse on every comparison.
CREATE TABLE IF NOT EXISTS sessions (
    session_id             TEXT PRIMARY KEY,
    agent_id               TEXT NOT NULL,
    created_at             INTEGER NOT NULL,
    last_active_at         INTEGER NOT NULL,
    title                  TEXT,
    workspace_id           TEXT,
    model                  TEXT,
    provider               TEXT,
    account_id             TEXT,
    reasoning_effort       TEXT,
    temperature            REAL,
    context_window         INTEGER,
    todos                  JSON,
    message_count          INTEGER NOT NULL DEFAULT 0,
    llm_call_counter       INTEGER,
    model_ratio            REAL,
    last_compaction_offset INTEGER,
    -- Flattened token counters. The dashboard reads these on every render,
    -- so a JSON blob here would still be parse-on-read.
    token_last_input       INTEGER NOT NULL DEFAULT 0,
    token_last_output      INTEGER NOT NULL DEFAULT 0,
    token_total_input      INTEGER NOT NULL DEFAULT 0,
    token_total_output     INTEGER NOT NULL DEFAULT 0,
    token_last_cache_read  INTEGER NOT NULL DEFAULT 0,
    token_last_cache_write INTEGER NOT NULL DEFAULT 0,
    token_total_cache_read INTEGER NOT NULL DEFAULT 0,
    token_total_cache_write INTEGER NOT NULL DEFAULT 0
    -- Columns added by post-base-schema migrations live in `MIGRATIONS`,
    -- never here. Bumping `SCHEMA_VERSION` means adding a step there.
);
CREATE INDEX IF NOT EXISTS idx_sessions_last_active ON sessions(last_active_at DESC);
CREATE VIRTUAL TABLE IF NOT EXISTS fts_sessions USING fts5(
    session_id UNINDEXED,
    title,
    agent_id,
    workspace_id,
    tokenize = 'trigram'
);

-- Forgetting archive (ADR-082 D1, replacing the grafeo PurgeLog).
-- Decay is the one path that can destroy memory data, so an expired node is
-- copied here *before* deletion instead of being dropped outright. Keeping the
-- whole row (props + text + embedding) makes recovery a re-insert.
CREATE TABLE IF NOT EXISTS purge_log (
    id        INTEGER PRIMARY KEY,
    node_id   INTEGER NOT NULL,
    label     TEXT NOT NULL,
    props     JSON NOT NULL,
    content   TEXT NOT NULL DEFAULT '',
    embedding BLOB,
    reason    TEXT NOT NULL DEFAULT '',
    purged_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_purge_log_purged_at ON purge_log(purged_at);
"#;

/// Current SQLite schema version (ADR-082 §4).
///
/// Bump this every time [`MIGRATIONS`] gains an entry; the upgrade is keyed
/// strictly off `PRAGMA user_version`, so the new migration must use the
/// next monotonic version (see [`MIGRATIONS`] for the format).
///
/// History: 1 = `version` / `corrupted` columns; 2 = `user_id` / `visibility`
/// (ADR-076 §决策 4 session ownership); 3 = `perf_*` LLM speed averages;
/// 4 = `chat_session_id`; 5 = `last_compaction_summary` + `conversation_state`;
/// 6 = `model_usage` breakdown column + `token_stats` aggregate table;
/// 7 = drop `last_compaction_summary` (its only reader, the retired intent
/// rotation handoff, is gone); 8 = rename `sessions.user_id` to `initiator`
/// (ADR-076 §决策 4 — the column is the session's initiator, and `user_id`
/// belongs to the account system); 9 = `chat_token_usage` ledger table
/// (Chat Router token accounting — one aggregate row per Chat session,
/// no conversation state); 10 = `sessions.intent_key` / `intent_closed`
/// (ADR-106 D11 — the intent context a session owns, and that context's
/// terminal mark, both on the session's own row instead of a side file);
/// 11 = `chat_token_usage.history_tokens` (the router's projected-history
/// occupancy — the only number comparable to the
/// `chat_router.history_max_tokens` budget); 12 = `sessions.aware_users`
/// (ADR-076 §决策 4 rev. — human accounts with read-only awareness of an
/// execution session, seeded by the Gateway for agent-to-agent intents);
/// 13 = `sessions.tool_policy` (per-session narrowing of the agent-level
/// builtin-tool / MCP configuration — JSON, `NULL` = inherit the agent
/// layer); 14 = `sessions.interveners` (the human accounts allowed to steer
/// an execution session they did not initiate: append turns and re-point the
/// run at a usable model — plan D2).
pub const SCHEMA_VERSION: i64 = 14;

/// Versioned upgrade steps, ordered.
///
/// Each tuple is `(target_version, sql)`. The migration runs against any
/// database whose stored `PRAGMA user_version` is `< target_version`, in
/// ascending order, until the stored version equals [`SCHEMA_VERSION`]. This
/// is the SQLite-idiomatic shape: `user_version` is the upgrade key, each
/// step is a normal forward SQL.
///
/// [`SCHEMA_SQL`] is frozen at the v0 baseline: it never gains a column that a
/// migration adds. A fresh database therefore runs the whole chain (0 →
/// [`SCHEMA_VERSION`]) right after the DDL, and a database from any earlier
/// version runs only the steps it is missing. One code path, no "fresh install
/// vs upgrade" branch, and no way for a table to exist at version 0.
///
/// ponytail: idempotency belongs inside the step. SQLite has no
/// `ALTER TABLE ADD COLUMN IF NOT EXISTS`, and it rejects a `WHERE`
/// clause on `ADD COLUMN` outright (a syntax error, not a no-op), so a
/// column-adding step cannot guard itself in SQL. The two steps that
/// need the guard do it in Rust instead: [`apply_migrations`] checks
/// `has_column` and skips the statement when the column is already
/// there. The same step is then a no-op against a fresh database (which
/// got the column from [`SCHEMA_SQL`]) and a real change against an
/// older one (which did not). [`SCHEMA_SQL`] does not stamp
/// `user_version`; that stamp is the last statement of the migration
/// transaction.
const MIGRATIONS: &[(i64, &str)] = &[
    (
        1,
        "ALTER TABLE sessions ADD COLUMN version INTEGER NOT NULL DEFAULT 3; \
         ALTER TABLE sessions ADD COLUMN corrupted INTEGER NOT NULL DEFAULT 0;",
    ),
    // ADR-076 §决策 4: session ownership. Both columns are nullable and carry
    // no default, so every pre-existing row migrates to `NULL` / `NULL` —
    // which reads as "ownerless and public", i.e. exactly the pre-accounts
    // behaviour. No backfill, and nothing retroactively hidden.
    (
        2,
        "ALTER TABLE sessions ADD COLUMN user_id TEXT; \
         ALTER TABLE sessions ADD COLUMN visibility TEXT;",
    ),
    // Session-level LLM speed / responsiveness accumulation. Stored as
    // sum + count per figure (never a folded mean), so each mean divides by
    // its own sample count. All nullable with no default: a pre-existing row
    // migrates to NULL, which reads as "no measurement yet" — the frontend
    // renders "—", exactly as it renders a session that has never made an
    // LLM call. No backfill (a fabricated speed for past calls would be a
    // lie).
    (
        3,
        "ALTER TABLE sessions ADD COLUMN perf_tps_sum REAL; \
         ALTER TABLE sessions ADD COLUMN perf_tps_samples INTEGER; \
         ALTER TABLE sessions ADD COLUMN perf_ttft_ms_sum REAL; \
         ALTER TABLE sessions ADD COLUMN perf_ttft_ms_samples INTEGER;",
    ),
    // Design 27 §8.2: the Chat session a group execution session belongs to.
    // Nullable, no default: every pre-existing row migrates to NULL, which
    // only matters for rows with `visibility = 'members'` (none exist yet),
    // so nothing changes retroactively.
    (
        4,
        "ALTER TABLE sessions ADD COLUMN chat_session_id TEXT;",
    ),
    // Design 27 §4 iron rule (no whole-session reads on request paths):
    // - `last_compaction_summary`: write-time derived state, so the rotation
    //   handoff read was O(1) instead of a full JSONL scan. Nullable, no
    //   default: pre-existing rows read as NULL. Superseded by migration 7,
    //   which drops it again once its only reader was retired.
    // - `conversation_state`: per-session indexer watermark (next physical
    //   line + byte offset), updated in the same transaction as the indexed
    //   row. Lets the sweep skip untouched logs via file size alone and
    //   reopen in O(#sessions) instead of a full-table pass.
    (
        5,
        "ALTER TABLE sessions ADD COLUMN last_compaction_summary TEXT; \n         CREATE TABLE IF NOT EXISTS conversation_state ( \n             session_id  TEXT PRIMARY KEY, \n             next_line   INTEGER NOT NULL DEFAULT 0, \n             byte_offset INTEGER NOT NULL DEFAULT 0 \n         );",
    ),
    // Per-model token statistics. Two pieces:
    // - `sessions.model_usage`: per-session `"{provider}|{model}"` breakdown
    //   (JSON object). This is the SOURCE OF TRUTH for the aggregate below;
    //   the runtime's upsert maintains `token_stats` as a same-transaction
    //   diff of these maps, and `rebuild_token_stats` can always re-derive
    //   the aggregate from them (the disposable-derived-index pattern).
    // - `token_stats`: agent-scoped aggregate, one row per (provider, model).
    //   Read path for the model-share panel — O(#models), never a session scan.
    // Backfill: pre-upgrade sessions have cumulative `token_total_*` but no
    // attribution. Every such row seeds a single `unknown|unknown` entry
    // carrying its totals (cost NULL — "宁可 miss 也不估计": seeding the
    // row's current `model` would misbill history written before a model
    // switch). The aggregate is then seeded by summing the breakdowns, so
    // Σ(token_stats) == Σ(model_usage) holds from the first open.
    //
    // Keys without a `|` are skipped by the seed query (the Rust side always
    // writes `provider|model`; a hand-edited row degrades to breakdown-only
    // until `rebuild_token_stats` is called).
    (
        6,
        "ALTER TABLE sessions ADD COLUMN model_usage TEXT; \n         CREATE TABLE IF NOT EXISTS token_stats ( \n             provider              TEXT NOT NULL, \n             model                 TEXT NOT NULL, \n             total_input           INTEGER NOT NULL DEFAULT 0, \n             total_output          INTEGER NOT NULL DEFAULT 0, \n             total_cache_read      INTEGER NOT NULL DEFAULT 0, \n             total_cache_write     INTEGER NOT NULL DEFAULT 0, \n             total_cost            REAL, \n             PRIMARY KEY (provider, model) \n         ); \n         UPDATE sessions SET model_usage = json_object( \n             'unknown|unknown', json_object( \n                 'input', token_total_input, \n                 'output', token_total_output, \n                 'cache_read', token_total_cache_read, \n                 'cache_write', token_total_cache_write, \n                 'cost', NULL \n             ) \n         ) \n         WHERE model_usage IS NULL \n           AND (token_total_input + token_total_output + token_total_cache_read + token_total_cache_write) > 0; \n         INSERT INTO token_stats (provider, model, total_input, total_output, total_cache_read, total_cache_write, total_cost) \n         SELECT substr(j.key, 1, instr(j.key, '|') - 1), \n                substr(j.key, instr(j.key, '|') + 1), \n                SUM(json_extract(j.value, '$.input')), \n                SUM(json_extract(j.value, '$.output')), \n                SUM(json_extract(j.value, '$.cache_read')), \n                SUM(json_extract(j.value, '$.cache_write')), \n                CASE WHEN SUM(CASE WHEN json_extract(j.value, '$.cost') IS NULL THEN 1 ELSE 0 END) > 0 THEN NULL \n                     ELSE SUM(json_extract(j.value, '$.cost')) END \n         FROM sessions s, json_each(s.model_usage) j \n         WHERE s.model_usage IS NOT NULL AND instr(j.key, '|') > 0 \n         GROUP BY j.key;",
    ),
    // The `last_compaction_summary` column added at 5 lost its only reader when
    // the intent-rotation mechanism was retired: nothing reads or writes it any
    // more. It is write-time derived state, so dropping it loses no
    // conversation data — the JSONL stays the ground truth (ADR-032). A plain
    // `DROP COLUMN` is safe: this step only runs for a database stored below
    // version 7, and every such database has the column (step 5 added it, and
    // a fresh one runs the whole chain in order).
    (
        7,
        "ALTER TABLE sessions DROP COLUMN last_compaction_summary;",
    ),
    // ADR-076 §决策 4 naming: the column holds *who initiated the session*
    // (an account, or an agent instance for agent-initiated work), not "the
    // user of the row". Keeping the old name in SQL while the code says
    // `initiator` is exactly the confusion the rename removes, and SQLite
    // renames in place — no table rebuild, no data moved.
    (
        8,
        "ALTER TABLE sessions RENAME COLUMN user_id TO initiator;",
    ),
    // Chat Router token accounting (design 29 / ADR-099 follow-up): one
    // aggregate row per Chat session for the router's own LLM spend. A Chat
    // session is NOT an execution session — the row carries no conversation
    // state, only cumulative counters in the same shape as `sessions`
    // (token_* columns + `model_usage` breakdown keyed "{provider}|{model}",
    // folded by the shared ADR-027/066 primitives in acowork-core). The
    // breakdown feeds the agent-scoped `token_stats` aggregate through the
    // same same-transaction diff as session rows, so the model-share panel
    // accounts for chat-side spend without the sessions table gaining a
    // fake-session entity.
    (
        9,
        "CREATE TABLE IF NOT EXISTS chat_token_usage ( \n             chat_session_id          TEXT PRIMARY KEY, \n             chat_kind                TEXT, \n             created_at               INTEGER NOT NULL, \n             last_active_at           INTEGER NOT NULL, \n             llm_call_count           INTEGER NOT NULL DEFAULT 0, \n             token_last_input         INTEGER NOT NULL DEFAULT 0, \n             token_last_output        INTEGER NOT NULL DEFAULT 0, \n             token_total_input        INTEGER NOT NULL DEFAULT 0, \n             token_total_output       INTEGER NOT NULL DEFAULT 0, \n             token_last_cache_read    INTEGER NOT NULL DEFAULT 0, \n             token_last_cache_write   INTEGER NOT NULL DEFAULT 0, \n             token_total_cache_read   INTEGER NOT NULL DEFAULT 0, \n             token_total_cache_write  INTEGER NOT NULL DEFAULT 0, \n             model_usage              TEXT \n         ); \n         CREATE INDEX IF NOT EXISTS idx_chat_token_usage_last_active \n             ON chat_token_usage(last_active_at DESC);",
    ),
    // ADR-106 D11: the intent-routing binding stops living in a side file with a
    // competing truth (`intent-sessions.json` / `terminal-sessions.json`) and
    // moves onto the session's own row. `intent_key` is the intent context the
    // session owns (`conv:task:{project}:{task}` for a PM task); `intent_closed`
    // is that context's terminal mark, so a finished task is never offered as a
    // routing candidate again. Both are NULL/0 on every pre-existing row, which
    // reads as "bound to nothing" — the routing table they replace was itself a
    // hint, and a lost binding costs one fresh session, never a wrong route.
    // Written only by `SessionMetaStore::intent_*`, never by the session's own
    // meta upsert, so the two writers can never erase each other.
    (
        10,
        "ALTER TABLE sessions ADD COLUMN intent_key TEXT; \n         ALTER TABLE sessions ADD COLUMN intent_closed INTEGER NOT NULL DEFAULT 0; \n         CREATE INDEX IF NOT EXISTS idx_sessions_intent_key \n             ON sessions(intent_key);",
    ),
    // The Chat Session Router's *history occupancy* — how many tokens the
    // projected history window (Block B, design 29 §4) actually cost on the
    // most recent turn. This is NOT the provider-reported prompt count
    // (which also carries the system prompt and the tool schemas, and
    // therefore routinely exceeds the history budget): the budget
    // (`chat_router.history_max_tokens`) bounds the history window alone,
    // so only this column can be compared against it truthfully.
    //
    // Nullable with no default: a row written before this migration reads
    // as NULL, i.e. "no occupancy measured yet" — the UI renders a dash,
    // never a fabricated 0 (same "宁可 miss 也不估计" rule as ADR-027).
    // The column-adding statement cannot guard itself in SQL (SQLite
    // rejects a `WHERE` clause on `ADD COLUMN`), so `apply_migrations`
    // checks `has_column` and skips it when the column is already there.
    (
        11,
        "ALTER TABLE chat_token_usage ADD COLUMN history_tokens INTEGER;",
    ),
    // ADR-076 §决策 4 (rev.): the human accounts with read-only awareness of
    // an execution session. Nullable JSON array with no default: a row written
    // before this migration reads as NULL → an empty list, i.e. "no extra
    // readers" — exactly the pre-feature behaviour, and no backfill (a
    // fabricated reader would be a privacy leak). Only agent-to-agent sessions
    // created after the upgrade carry a non-empty list.
    (
        12,
        "ALTER TABLE sessions ADD COLUMN aware_users JSON;",
    ),
    // Per-session tool narrowing (`SessionMeta.tool_policy`): which builtin
    // tools and MCP servers/tools this one session keeps, out of what the
    // agent layer exposes. Nullable with no default: a pre-existing row (and
    // every session the user has never toggled) reads as NULL = "inherit the
    // agent layer verbatim", so no backfill and no behaviour change. Written
    // only when the user flips a switch for one session — the row is an
    // override, never a copy of the agent list.
    //
    // Same guard as v11/v12: SQLite rejects a `WHERE` on `ADD COLUMN`, so
    // `apply_migrations` does the `has_column` check.
    (
        13,
        "ALTER TABLE sessions ADD COLUMN tool_policy JSON;",
    ),
    // Plan D2: the human accounts allowed to **intervene** in a session they
    // did not initiate — append turns and change the run's launch parameters
    // (model / provider / reasoning effort). Orthogonal to `aware_users`
    // (reading), deliberately a second column: one list cannot express "may
    // know this run exists" without also granting "may steer it".
    //
    // Nullable with no default: a row written before this migration reads as
    // NULL -> an empty list -> exactly the previous single-writer behaviour,
    // and nothing is retroactively opened to anybody. Unlike `aware_users`
    // this list is NOT write-once: the human-join route appends to it when a
    // person is added to the bound Chat as a participant.
    //
    // Same guard as v11-v13: SQLite rejects a `WHERE` on `ADD COLUMN`, so
    // `apply_migrations` does the `has_column` check.
    (
        14,
        "ALTER TABLE sessions ADD COLUMN interveners JSON;",
    ),
];

/// One structural fact the **target** schema asserts, reconciled by presence.
///
/// [`MIGRATIONS`] is *history*: it carries a database forward one version at a
/// time and must stay version-gated, because some steps hold one-shot data
/// work (step 6 seeds `token_stats` — replaying it would double-count). History
/// alone cannot be the whole contract, because `user_version` is only a *cache*
/// of what ran and a version number is unique only within one branch: two
/// branches that both allocate v10 merge into a chain where the step that lost
/// the numbering never runs on a database the other branch already stamped. The
/// consequence is silent — the retained `user_version` satisfies
/// `stored >= SCHEMA_VERSION`, so `apply_migrations` skipped the upgrade and the
/// column was never added.
///
/// That is exactly how four workspaces ended up stamped `user_version = 11`
/// without `sessions.intent_key` (v10 on the intent branch) while carrying
/// `chat_token_usage.history_tokens` (v10 on the response-api branch). Every PM
/// task wake-up then died in the Runtime with
/// `no such column: intent_key`, the Gateway had already reported the intent as
/// delivered, and PM had stamped `dispatched_at` — so the task was stuck
/// `pending` forever and saving it again could not recover it.
///
/// So the target shape is asserted here independently of the version: a
/// missing column is added, a retired column is dropped, a half-done rename is
/// finished, a missing table / index is created — whatever `user_version`
/// claims. Every entry is idempotent, so reconciliation is also the integrity
/// check: a database already in shape costs one pragma lookup per entry.
#[derive(Clone, Copy, Debug)]
enum SchemaObject {
    /// Must exist: `ALTER TABLE {table} ADD COLUMN {decl}`.
    Column {
        table: &'static str,
        name: &'static str,
        decl: &'static str,
    },
    /// Must be gone (a later step retired it): `DROP COLUMN {name}`.
    Retired {
        table: &'static str,
        name: &'static str,
    },
    /// Must be `{to}`: rename `{from}` while it is still there.
    Renamed {
        table: &'static str,
        from: &'static str,
        to: &'static str,
    },
    /// Must exist: a `CREATE TABLE IF NOT EXISTS` this chain introduced.
    Table {
        name: &'static str,
        sql: &'static str,
    },
    /// Must exist: a `CREATE INDEX IF NOT EXISTS` this chain introduced.
    Index {
        name: &'static str,
        sql: &'static str,
    },
}

/// The shape this build expects, in migration order.
///
/// Objects that [`SCHEMA_SQL`] already creates on every open are deliberately
/// absent — that DDL is itself the assertion. This list covers only what the
/// version chain adds on top of the frozen v0 baseline.
const TARGET_SCHEMA: &[SchemaObject] = &[
    // v1
    SchemaObject::Column {
        table: "sessions",
        name: "version",
        decl: "version INTEGER NOT NULL DEFAULT 3",
    },
    SchemaObject::Column {
        table: "sessions",
        name: "corrupted",
        decl: "corrupted INTEGER NOT NULL DEFAULT 0",
    },
    // v2 adds `user_id`, v8 renames it in place — the target state is
    // `initiator`, so the rename is the fact to assert, not the old column.
    SchemaObject::Renamed {
        table: "sessions",
        from: "user_id",
        to: "initiator",
    },
    SchemaObject::Column {
        table: "sessions",
        name: "visibility",
        decl: "visibility TEXT",
    },
    // v3
    SchemaObject::Column {
        table: "sessions",
        name: "perf_tps_sum",
        decl: "perf_tps_sum REAL",
    },
    SchemaObject::Column {
        table: "sessions",
        name: "perf_tps_samples",
        decl: "perf_tps_samples INTEGER",
    },
    SchemaObject::Column {
        table: "sessions",
        name: "perf_ttft_ms_sum",
        decl: "perf_ttft_ms_sum REAL",
    },
    SchemaObject::Column {
        table: "sessions",
        name: "perf_ttft_ms_samples",
        decl: "perf_ttft_ms_samples INTEGER",
    },
    // v4
    SchemaObject::Column {
        table: "sessions",
        name: "chat_session_id",
        decl: "chat_session_id TEXT",
    },
    // v5 added `last_compaction_summary`, v7 retired it: the target state is
    // "absent", so that is what convergence enforces.
    SchemaObject::Retired {
        table: "sessions",
        name: "last_compaction_summary",
    },
    SchemaObject::Table {
        name: "conversation_state",
        sql: "CREATE TABLE IF NOT EXISTS conversation_state (\n \
              session_id  TEXT PRIMARY KEY,\n \
              next_line   INTEGER NOT NULL DEFAULT 0,\n \
              byte_offset INTEGER NOT NULL DEFAULT 0\n \
              )",
    },
    // v6 (the column; the aggregate's one-shot seed stays version-gated)
    SchemaObject::Column {
        table: "sessions",
        name: "model_usage",
        decl: "model_usage TEXT",
    },
    SchemaObject::Table {
        name: "token_stats",
        sql: "CREATE TABLE IF NOT EXISTS token_stats (\n \
              provider              TEXT NOT NULL,\n \
              model                 TEXT NOT NULL,\n \
              total_input           INTEGER NOT NULL DEFAULT 0,\n \
              total_output          INTEGER NOT NULL DEFAULT 0,\n \
              total_cache_read      INTEGER NOT NULL DEFAULT 0,\n \
              total_cache_write     INTEGER NOT NULL DEFAULT 0,\n \
              total_cost            REAL,\n \
              PRIMARY KEY (provider, model)\n \
              )",
    },
    // v9
    SchemaObject::Table {
        name: "chat_token_usage",
        sql: "CREATE TABLE IF NOT EXISTS chat_token_usage (\n \
              chat_session_id          TEXT PRIMARY KEY,\n \
              chat_kind                TEXT,\n \
              created_at               INTEGER NOT NULL,\n \
              last_active_at           INTEGER NOT NULL,\n \
              llm_call_count           INTEGER NOT NULL DEFAULT 0,\n \
              token_last_input         INTEGER NOT NULL DEFAULT 0,\n \
              token_last_output        INTEGER NOT NULL DEFAULT 0,\n \
              token_total_input        INTEGER NOT NULL DEFAULT 0,\n \
              token_total_output       INTEGER NOT NULL DEFAULT 0,\n \
              token_last_cache_read    INTEGER NOT NULL DEFAULT 0,\n \
              token_last_cache_write   INTEGER NOT NULL DEFAULT 0,\n \
              token_total_cache_read   INTEGER NOT NULL DEFAULT 0,\n \
              token_total_cache_write  INTEGER NOT NULL DEFAULT 0,\n \
              model_usage              TEXT\n \
              )",
    },
    SchemaObject::Index {
        name: "idx_chat_token_usage_last_active",
        sql: "CREATE INDEX IF NOT EXISTS idx_chat_token_usage_last_active\n \
              ON chat_token_usage(last_active_at DESC)",
    },
    // v10 — the pair that this design exists for: the intent-routing binding
    // and its terminal mark on the session's own row (ADR-106 D11).
    SchemaObject::Column {
        table: "sessions",
        name: "intent_key",
        decl: "intent_key TEXT",
    },
    SchemaObject::Column {
        table: "sessions",
        name: "intent_closed",
        decl: "intent_closed INTEGER NOT NULL DEFAULT 0",
    },
    SchemaObject::Index {
        name: "idx_sessions_intent_key",
        sql: "CREATE INDEX IF NOT EXISTS idx_sessions_intent_key ON sessions(intent_key)",
    },
    // v11
    SchemaObject::Column {
        table: "chat_token_usage",
        name: "history_tokens",
        decl: "history_tokens INTEGER",
    },
    // v12 — ADR-076 §决策 4 (rev.): read-only human awareness of a session.
    SchemaObject::Column {
        table: "sessions",
        name: "aware_users",
        decl: "aware_users JSON",
    },
    // v13 — per-session narrowing of the agent-layer tool configuration.
    SchemaObject::Column {
        table: "sessions",
        name: "tool_policy",
        decl: "tool_policy JSON",
    },
    // v14 — plan D2: human interveners (steer an execution session they did
    // not initiate).
    SchemaObject::Column {
        table: "sessions",
        name: "interveners",
        decl: "interveners JSON",
    },
];

/// Apply the version-gated [`MIGRATIONS`] chain, then converge [`TARGET_SCHEMA`],
/// then stamp `user_version`. Safe to call on every open — a database that is
/// already at [`SCHEMA_VERSION`] **and** in shape opens no transaction and
/// executes no DDL (the per-object presence checks still run on every open).
/// All work that does run happens in one transaction, so a crash
/// mid-upgrade leaves the database at its previous version, never
/// half-migrated.
///
/// Stamp: `PRAGMA user_version = SCHEMA_VERSION` is the last statement of the
/// transaction, written only when the stored version is behind (a database
/// stamped by a *newer* build keeps its stamp — this build must not downgrade
/// it). This is the only path that writes `user_version`; the DDL string and
/// the open code never touch it.
pub(crate) fn apply_migrations(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    let stored: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0);
    let behind = stored < SCHEMA_VERSION;
    if !behind && is_converged(conn)? {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for (target, sql) in MIGRATIONS {
        if stored < *target {
            // The v8 rename is the one step that cannot guard inside the SQL
            // itself (SQLite has no conditional DDL, and `ADD COLUMN`-style
            // `WHERE NOT EXISTS` guards do not exist for renames). A stored
            // version below 8 whose `sessions` table already has no `user_id`
            // — e.g. a database written by an intermediate branch build that
            // shipped the rename before stamping 8 — would otherwise hard-
            // fail `open` and strand every session row. Skip the rename when
            // the source column is absent; the end state (`initiator`) is
            // identical.
            if *target == 8 && !has_column(&tx, "sessions", "user_id")? {
                continue;
            }
            // The v11 column-adding step has the same problem from the other
            // side: SQLite rejects a `WHERE` clause on `ADD COLUMN`
            // outright, so the guard cannot live in the SQL. Skip the
            // statement when the column is already there — a database
            // written by a build that shipped the column before stamping
            // the version must still open.
            if *target == 11 && has_column(&tx, "chat_token_usage", "history_tokens")? {
                continue;
            }
            // The v12 column-add has the same guard: SQLite rejects a
            // `WHERE` on `ADD COLUMN`, so a build that shipped
            // `sessions.aware_users` before stamping 12 must not hard-fail
            // here. The end state is identical and `TARGET_SCHEMA` converges
            // it anyway.
            if *target == 12 && has_column(&tx, "sessions", "aware_users")? {
                continue;
            }
            // v13 is a column-add of the same shape; same guard, same reason.
            if *target == 13 && has_column(&tx, "sessions", "tool_policy")? {
                continue;
            }
            // v14 likewise.
            if *target == 14 && has_column(&tx, "sessions", "interveners")? {
                continue;
            }
            tx.execute_batch(sql)?;
        }
    }
    // The chain only covers databases whose stamp is behind. Whatever the stamp
    // claims, the shape is now enforced by presence — this is the repair for a
    // step that lost a version-number collision on another branch.
    for object in TARGET_SCHEMA {
        converge(&tx, object)?;
    }
    if behind {
        tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    }
    tx.commit()
}

/// `true` when every [`TARGET_SCHEMA`] fact already holds — the no-op open.
fn is_converged(conn: &rusqlite::Connection) -> rusqlite::Result<bool> {
    for object in TARGET_SCHEMA {
        if !is_in_place(conn, object)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether one [`SchemaObject`] already holds.
fn is_in_place(conn: &rusqlite::Connection, object: &SchemaObject) -> rusqlite::Result<bool> {
    Ok(match object {
        SchemaObject::Column { table, name, .. } => has_column(conn, table, name)?,
        SchemaObject::Retired { table, name } => !has_column(conn, table, name)?,
        // A rename is still owed only while the source column exists and the
        // target does not; both-present is not a shape this chain can produce,
        // and renaming over an existing column would hard-fail `open`.
        SchemaObject::Renamed { table, from, to } => {
            !(has_column(conn, table, from)? && !has_column(conn, table, to)?)
        }
        SchemaObject::Table { name, .. } => has_object(conn, "table", name)?,
        SchemaObject::Index { name, .. } => has_object(conn, "index", name)?,
    })
}

/// Idempotent repair of one [`SchemaObject`]; a no-op when already in place.
fn converge(conn: &rusqlite::Connection, object: &SchemaObject) -> rusqlite::Result<()> {
    match object {
        SchemaObject::Column { table, name, decl } => {
            if !has_column(conn, table, name)? {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {decl}"))?;
            }
        }
        SchemaObject::Retired { table, name } => {
            if has_column(conn, table, name)? {
                conn.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {name}"))?;
            }
        }
        SchemaObject::Renamed { table, from, to } => {
            if has_column(conn, table, from)? && !has_column(conn, table, to)? {
                conn.execute_batch(&format!(
                    "ALTER TABLE {table} RENAME COLUMN {from} TO {to}"
                ))?;
            }
        }
        SchemaObject::Table { name, sql } | SchemaObject::Index { name, sql } => {
            if !has_object(conn, object_kind(object), name)? {
                conn.execute_batch(sql)?;
            }
        }
    }
    Ok(())
}

fn object_kind(object: &SchemaObject) -> &'static str {
    match object {
        SchemaObject::Index { .. } => "index",
        _ => "table",
    }
}

/// Whether `name` currently exists in `sqlite_master` as `kind`
/// (`"table"` / `"index"`).
fn has_object(conn: &rusqlite::Connection, kind: &str, name: &str) -> rusqlite::Result<bool> {
    use rusqlite::OptionalExtension;
    let found: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = ?1 AND name = ?2",
            rusqlite::params![kind, name],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Whether `table` currently has a column named `column` (pragma lookup, the
/// single presence primitive behind both the v8 rename guard in the version
/// chain and [`converge`]).
fn has_column(conn: &rusqlite::Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    use rusqlite::OptionalExtension;
    let found: Option<String> = conn
        .query_row(
            "SELECT name FROM pragma_table_info(?) WHERE name = ?",
            rusqlite::params![table, column],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Map a node label to its FTS5 table name.
///
/// Returns `None` for labels without a text index (e.g. `Session`,
/// `ToolInvocation`) — those never reach the memory retrieval paths.
pub(crate) fn fts_table(label: &str) -> Option<&'static str> {
    if label == labels::EPISODIC {
        Some("fts_episodic")
    } else if label == labels::KNOWLEDGE {
        Some("fts_knowledge")
    } else if label == labels::PROCEDURAL {
        Some("fts_procedural")
    } else if label == labels::AUTOBIOGRAPHICAL {
        Some("fts_autobiographical")
    } else if label == super::CONVERSATION_LABEL {
        Some("fts_conversation")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::OptionalExtension;

    use super::{apply_migrations, SCHEMA_VERSION};

    /// ADR-076 §决策 4: an existing store keeps its rows across the
    /// `sessions.user_id` → `initiator` rename. A broken step here does not
    /// fail loudly — it leaves the memory service unable to open a database
    /// that holds the user's whole session index.
    #[test]
    fn migration_8_renames_the_column_and_keeps_the_data() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        // The v7 shape that every installed store has on disk.
        conn.execute_batch(
            "CREATE TABLE sessions (session_id TEXT PRIMARY KEY, user_id TEXT); \
             INSERT INTO sessions (session_id, user_id) VALUES ('s1', 'u-alice'); \
             PRAGMA user_version = 7;",
        )
        .expect("seed v7 store");

        apply_migrations(&mut conn).expect("migrate");

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(sessions)")
            .expect("table_info")
            .query_map([], |r| r.get::<_, String>(1))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert!(cols.iter().any(|c| c == "initiator"), "columns: {cols:?}");
        assert!(!cols.iter().any(|c| c == "user_id"), "columns: {cols:?}");

        let owner: String = conn
            .query_row("SELECT initiator FROM sessions WHERE session_id = 's1'", [], |r| {
                r.get(0)
            })
            .expect("row survives the rename");
        assert_eq!(owner, "u-alice");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The v8 rename is guarded against a missing source column: a database
    /// written below version 8 that already renamed the column (an
    /// intermediate branch build) must still open, not strand the whole
    /// session index on a hard DDL failure.
    #[test]
    fn migration_8_is_a_noop_when_the_source_column_is_absent() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (session_id TEXT PRIMARY KEY, initiator TEXT); \
             INSERT INTO sessions (session_id, initiator) VALUES ('s1', 'u-alice'); \
             PRAGMA user_version = 7;",
        )
        .expect("seed already-renamed store");

        apply_migrations(&mut conn).expect("open must not fail on the rename");

        let owner: String = conn
            .query_row("SELECT initiator FROM sessions WHERE session_id = 's1'", [], |r| {
                r.get(0)
            })
            .expect("row intact");
        assert_eq!(owner, "u-alice");
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The v11 step adds the router's history-occupancy column to the
    /// Chat ledger. A store written by v10 must gain the column AND keep
    /// its rows — a broken step here would leave every installed
    /// workspace's chat ledger unreadable.
    #[test]
    fn migration_11_adds_the_history_tokens_column_and_keeps_the_rows() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        // The v10 shape: the ledger table without the column. `sessions` exists
        // because `SCHEMA_SQL` always runs before the migrations.
        conn.execute_batch(
            "CREATE TABLE sessions (session_id TEXT PRIMARY KEY); \
             CREATE TABLE chat_token_usage ( \
                 chat_session_id TEXT PRIMARY KEY, \
                 chat_kind TEXT, \
                 created_at INTEGER NOT NULL, \
                 last_active_at INTEGER NOT NULL, \
                 llm_call_count INTEGER NOT NULL DEFAULT 0 \
             ); \
             INSERT INTO chat_token_usage (chat_session_id, created_at, last_active_at) \
                 VALUES ('c-1', 0, 0); \
             PRAGMA user_version = 10;",
        )
        .expect("seed v10 store");

        apply_migrations(&mut conn).expect("migrate");

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(chat_token_usage)")
            .expect("table_info")
            .query_map([], |r| r.get::<_, String>(1))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert!(
            cols.iter().any(|c| c == "history_tokens"),
            "columns: {cols:?}"
        );

        // The pre-existing row survives, and its occupancy reads as
        // "not measured" (NULL) rather than a fabricated zero.
        let occupancy: Option<i64> = conn
            .query_row(
                "SELECT history_tokens FROM chat_token_usage WHERE chat_session_id = 'c-1'",
                [],
                |r| r.get(0),
            )
            .expect("row survives the migration");
        assert_eq!(occupancy, None);

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The v11 step is guarded against an already-present column: a
    /// database written by a build that shipped the column before
    /// stamping the version must still open, not fail on a duplicate
    /// `ADD COLUMN`.
    #[test]
    fn migration_11_is_a_noop_when_the_column_is_already_present() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (session_id TEXT PRIMARY KEY); \
             CREATE TABLE chat_token_usage ( \
                 chat_session_id TEXT PRIMARY KEY, \
                 chat_kind TEXT, \
                 created_at INTEGER NOT NULL, \
                 last_active_at INTEGER NOT NULL, \
                 llm_call_count INTEGER NOT NULL DEFAULT 0, \
                 history_tokens INTEGER \
             ); \
             PRAGMA user_version = 10;",
        )
        .expect("seed already-migrated store");

        apply_migrations(&mut conn).expect("open must not fail on the duplicate column");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// ADR-076 §决策 4 (rev.): the v12 step adds `sessions.aware_users` and
    /// keeps existing rows, reading the absent column back as "no extra
    /// readers" (NULL) rather than a fabricated grant.
    #[test]
    fn migration_12_adds_the_aware_users_column_and_keeps_the_rows() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        // The v11 shape: `sessions` without the awareness column.
        conn.execute_batch(
            "CREATE TABLE sessions ( \
                 session_id TEXT PRIMARY KEY, \
                 agent_id TEXT, \
                 initiator TEXT, \
                 chat_session_id TEXT \
             ); \
             INSERT INTO sessions (session_id, agent_id) VALUES ('s-1', 'com.test.a'); \
             PRAGMA user_version = 11;",
        )
        .expect("seed v11 store");

        apply_migrations(&mut conn).expect("migrate");

        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(sessions)")
            .expect("table_info")
            .query_map([], |r| r.get::<_, String>(1))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert!(
            cols.iter().any(|c| c == "aware_users"),
            "columns: {cols:?}"
        );

        // The pre-existing row survives with a NULL awareness column.
        let aware: Option<String> = conn
            .query_row(
                "SELECT aware_users FROM sessions WHERE session_id = 's-1'",
                [],
                |r| r.get(0),
            )
            .expect("row survives the migration");
        assert_eq!(aware, None, "a pre-v12 row reads as no extra readers");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The v12 step is guarded against an already-present column: a database
    /// written by a build that shipped `aware_users` before stamping 12 must
    /// still open, not fail on a duplicate `ADD COLUMN`.
    #[test]
    fn migration_12_is_a_noop_when_the_column_is_already_present() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions ( \
                 session_id TEXT PRIMARY KEY, \
                 agent_id TEXT, \
                 aware_users JSON \
             ); \
             PRAGMA user_version = 11;",
        )
        .expect("seed already-migrated store");

        apply_migrations(&mut conn).expect("open must not fail on the duplicate column");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// v13 is the same shape of step as v12 (a `sessions` column-add) and
    /// needs the same guard: a build that shipped `tool_policy` before
    /// stamping 13 must still open.
    #[test]
    fn migration_13_is_a_noop_when_the_column_is_already_present() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions ( \
                 session_id TEXT PRIMARY KEY, \
                 agent_id TEXT, \
                 tool_policy JSON \
             ); \
             PRAGMA user_version = 12;",
        )
        .expect("seed already-migrated store");

        apply_migrations(&mut conn).expect("open must not fail on the duplicate column");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// A v12 database (the last release before per-session tool narrowing)
    /// gains a NULL `tool_policy` on upgrade — every existing session keeps
    /// inheriting the agent layer, which is exactly the pre-feature
    /// behaviour.
    #[test]
    fn migration_13_leaves_existing_rows_inheriting_the_agent_layer() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions ( \
                 session_id TEXT PRIMARY KEY, \
                 agent_id TEXT, \
                 aware_users JSON \
             ); \
             INSERT INTO sessions (session_id, agent_id) VALUES ('s-1', 'com.test.a'); \
             PRAGMA user_version = 12;",
        )
        .expect("seed v12 store");

        apply_migrations(&mut conn).expect("migrate");

        let policy: Option<String> = conn
            .query_row(
                "SELECT tool_policy FROM sessions WHERE session_id = 's-1'",
                [],
                |r| r.get(0),
            )
            .expect("column exists and the row survives");
        assert_eq!(policy, None, "a pre-v13 row reads as 'inherit the agent layer'");
    }

    /// v14 (plan D2): a v13 database gains a NULL `interveners`. Every
    /// pre-existing session therefore keeps its exact single-writer behaviour —
    /// the migration must not retroactively open anybody into anybody's run.
    #[test]
    fn migration_14_grants_no_intervention_to_pre_existing_sessions() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (                  session_id TEXT PRIMARY KEY,                  agent_id TEXT,                  aware_users JSON,                  tool_policy JSON              );              INSERT INTO sessions (session_id, agent_id) VALUES ('s-1', 'com.test.a');              PRAGMA user_version = 13;",
        )
        .expect("seed v13 store");

        apply_migrations(&mut conn).expect("migrate");

        let interveners: Option<String> = conn
            .query_row(
                "SELECT interveners FROM sessions WHERE session_id = 's-1'",
                [],
                |r| r.get(0),
            )
            .expect("column exists and the row survives");
        assert_eq!(
            interveners, None,
            "a pre-v14 row reads as 'no human may steer this run'"
        );

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// Same guard as v11-v13: SQLite rejects a `WHERE` on `ADD COLUMN`, so a
    /// build that shipped `interveners` before stamping 14 must still open.
    #[test]
    fn migration_14_is_a_noop_when_the_column_is_already_present() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (                  session_id TEXT PRIMARY KEY,                  agent_id TEXT,                  interveners JSON              );              PRAGMA user_version = 13;",
        )
        .expect("seed already-migrated store");

        apply_migrations(&mut conn).expect("open must not fail on the duplicate column");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// The incident this whole convergence pass exists for.
    ///
    /// Two branches both allocated schema version 10: the intent branch used it
    /// for `sessions.intent_key` / `intent_closed`, the response-api branch for
    /// `chat_token_usage.history_tokens`. A workspace stamped 10 by the second
    /// branch opened once under the merged build, which found v10 already
    /// "applied", skipped the intent step, ran v11 and stamped 11 — leaving a
    /// database that claims to be current with no `intent_key` at all.
    ///
    /// From there every PM task dispatch died in the Runtime with
    /// `no such column: intent_key`, and `user_version` gave nobody a reason to
    /// look. The assertion is behavioural, not cosmetic: the exact statement
    /// `SqliteSessionMetaStore::intent_lookup` runs must succeed afterwards.
    #[test]
    fn a_shadowed_version_number_still_heals_the_missing_column() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        // A store stamped at the *current* version that carries the
        // response-api branch's v10 and nothing of the intent branch's v10.
        // (The historical incident was a collision at 10; the invariant this
        // pins is version-independent: a database that claims to be current
        // but is missing a column must be healed by convergence, not by a
        // stamp change.)
        conn.execute_batch(&format!(
            "CREATE TABLE sessions ( \
                 session_id TEXT PRIMARY KEY, \
                 initiator TEXT, \
                 last_active_at INTEGER NOT NULL \
             ); \
             CREATE TABLE chat_token_usage ( \
                 chat_session_id TEXT PRIMARY KEY, \
                 created_at INTEGER NOT NULL, \
                 last_active_at INTEGER NOT NULL, \
                 history_tokens INTEGER \
             ); \
             INSERT INTO sessions (session_id, initiator, last_active_at) \
                 VALUES ('s-1', 'babea202', 0); \
             PRAGMA user_version = {SCHEMA_VERSION};"
        ))
        .expect("seed the incident's store shape");

        apply_migrations(&mut conn).expect("open must heal, not stay broken");

        // The Runtime's routing read — the statement that failed in production.
        // Zero rows is the correct answer here (no binding yet); the point is
        // that the statement runs at all instead of failing on a missing column.
        let mapped: Option<String> = conn
            .query_row(
                "SELECT session_id FROM sessions \
                 WHERE intent_key = ?1 AND intent_closed = 0 \
                 ORDER BY last_active_at DESC LIMIT 1",
                rusqlite::params!["conv:task:p-66e68d1a:t-1b7bdb21"],
                |r| r.get(0),
            )
            .optional()
            .expect("intent routing must work again");
        assert_eq!(mapped, None, "no binding yet — but the query must not fail");

        // A missing column heals without touching the rows that were there.
        let owner: String = conn
            .query_row("SELECT initiator FROM sessions WHERE session_id = 's-1'", [], |r| {
                r.get(0)
            })
            .expect("row intact");
        assert_eq!(owner, "babea202");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .expect("version");
        assert_eq!(version, SCHEMA_VERSION, "the stamp is not rewritten downwards or upwards");
    }

    /// Every fact [`TARGET_SCHEMA`] asserts must hold after opening a baseline
    /// database — otherwise the convergence list is describing something the
    /// chain does not actually build.
    #[test]
    fn target_schema_holds_after_the_full_chain() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(super::SCHEMA_SQL).expect("baseline DDL");
        apply_migrations(&mut conn).expect("migrate");
        for object in super::TARGET_SCHEMA {
            assert!(
                super::is_in_place(&conn, object).expect("presence check"),
                "target schema object missing after the chain: {object:?}"
            );
        }
    }

    /// The same class of damage one level up: a table or index that a shadowed
    /// step never created. Convergence must build it from nothing, and in list
    /// order — `chat_token_usage` before its `history_tokens` column.
    #[test]
    fn a_stamped_database_missing_whole_tables_still_gets_them() {
        let mut conn = rusqlite::Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE sessions (session_id TEXT PRIMARY KEY, initiator TEXT); \
             PRAGMA user_version = 11;",
        )
        .expect("seed a v11 store with the ledger tables gone");

        apply_migrations(&mut conn).expect("open must rebuild what is missing");

        for table in ["conversation_state", "token_stats", "chat_token_usage"] {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .expect("sqlite_master");
            assert_eq!(exists, 1, "missing table: {table}");
        }
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(chat_token_usage)")
            .expect("table_info")
            .query_map([], |r| r.get::<_, String>(1))
            .expect("rows")
            .collect::<rusqlite::Result<_>>()
            .expect("collect");
        assert!(
            cols.iter().any(|c| c == "history_tokens"),
            "columns: {cols:?}"
        );
    }

    /// The root cause of the incident was two branches handing out the *same*
    /// version number, which no per-branch test can see. Pin the invariant that
    /// makes a merge detectable: targets form the contiguous range
    /// `1..=SCHEMA_VERSION`, each exactly once.
    #[test]
    fn migration_targets_are_contiguous_and_match_the_schema_version() {
        let targets: Vec<i64> = super::MIGRATIONS.iter().map(|(t, _)| *t).collect();
        let expected: Vec<i64> = (1..=SCHEMA_VERSION).collect();
        assert_eq!(targets, expected, "migration versions must be dense and unique");
    }

    /// Convergence is only a safety net if it covers what the chain actually
    /// does. Every object a migration creates or adds must therefore be
    /// declared in [`TARGET_SCHEMA`] — otherwise a future version collision
    /// leaves that object silently unrepaired.
    #[test]
    fn every_object_the_chain_creates_is_declared_in_the_target_schema() {
        use super::SchemaObject;

        let mut declared: Vec<&str> = Vec::new();
        for object in super::TARGET_SCHEMA {
            declared.push(match object {
                SchemaObject::Column { name, .. }
                | SchemaObject::Retired { name, .. }
                | SchemaObject::Table { name, .. }
                | SchemaObject::Index { name, .. } => name,
                SchemaObject::Renamed { from, .. } => from,
            });
        }

        fn following_identifier<'a>(sql: &'a str, needle: &str) -> Vec<&'a str> {
            sql.match_indices(needle)
                .map(|(at, _)| {
                    let rest = &sql[at + needle.len()..];
                    let end = rest
                        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                        .unwrap_or(rest.len());
                    &rest[..end]
                })
                .collect()
        }

        for (target, sql) in super::MIGRATIONS {
            for needle in [
                "ADD COLUMN ",
                "CREATE TABLE IF NOT EXISTS ",
                "CREATE INDEX IF NOT EXISTS ",
            ] {
                for name in following_identifier(sql, needle) {
                    assert!(
                        declared.contains(&name),
                        "migration v{target} creates `{name}` but TARGET_SCHEMA does not declare it"
                    );
                }
            }
        }
    }
}
