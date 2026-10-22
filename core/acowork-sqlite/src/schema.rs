//! SQLite schema for the memory store (ADR-082 D1).
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
//! `user_version` is the schema-version gate; every open runs
//! [`apply_migrations`] which only executes the steps whose target version
//! is strictly greater than the stored one.

use acowork_memory::labels;

/// DDL applied on every [`crate::SqliteStore::open`]. `IF NOT EXISTS` throughout.
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
/// 6 = `model_usage` breakdown column + `token_stats` aggregate table.
pub const SCHEMA_VERSION: i64 = 6;

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
/// ponytail: idempotency belongs inside the SQL itself. SQLite has no
/// `ALTER TABLE ADD COLUMN IF NOT EXISTS`, so a step that adds a column
/// guards with `WHERE NOT EXISTS (... pragma_table_info ...)`. The same
/// step is then a no-op against a fresh database (which got the column
/// from [`SCHEMA_SQL`]) and a real change against an older one (which did
/// not). [`SCHEMA_SQL`] does not stamp `user_version`; that stamp is the
/// last statement of the migration transaction.
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
    // - `last_compaction_summary`: write-time derived state, so the T2
    //   handoff read is O(1) instead of a full JSONL scan. Nullable, no
    //   default: pre-existing rows read as NULL, which the runtime heals
    //   with one legacy scan + write-back on first touch.
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
];

/// Apply every [`MIGRATIONS`] step whose target version is strictly greater
/// than the database's current `PRAGMA user_version`. Safe to call on every
/// open — a database already at [`SCHEMA_VERSION`] is a no-op (no
/// transaction opened, no statement executed). All work runs in a single
/// transaction so a crash mid-upgrade leaves the database at its previous
/// version, never half-migrated.
///
/// Stamp: `PRAGMA user_version = SCHEMA_VERSION` is the last statement of
/// the transaction. This is the only path that writes `user_version`; the
/// DDL string and the open code never touch it.
pub(crate) fn apply_migrations(conn: &mut rusqlite::Connection) -> rusqlite::Result<()> {
    let stored: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0);
    if stored >= SCHEMA_VERSION {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for (target, sql) in MIGRATIONS {
        if stored < *target {
            tx.execute_batch(sql)?;
        }
    }
    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()
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
    } else if label == crate::conversation::LABEL {
        Some("fts_conversation")
    } else {
        None
    }
}
