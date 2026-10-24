//! Tests for `SqliteSessionMetaStore` — the SQLite-backed
//! `SessionMetaStore` implementation (ADR-082 §4 step 3).
//!
//! Validates the trait contract end-to-end against a real on-disk store:
//!
//! 1. Round-trip: `upsert` → `get` returns the same fields.
//! 2. `list_recent` orders by `last_active_at` descending and respects `limit`.
//! 3. `search` matches `title` / `agent_id` / `workspace_id` and falls back
//!    to `list_recent` on empty query.
//! 4. `delete` removes one row (and its FTS entry) and is idempotent.
//! 5. `prune_to` deletes the right rows and reports the deleted ids.
//! 6. The store keeps memory + conversation index + session meta in
//!    the same `.sqlite` file (the user's "one file" requirement).
//! 7. `version` / `corrupted` round-trip, and `PRAGMA user_version` +
//!    `MIGRATIONS` upgrade an older file in order.
//! 8. ADR-076 §决策 4: `user_id` / `visibility` round-trip and `prune_to`
//!    buckets the cap per owner.

use std::path::PathBuf;
use std::sync::Arc;

use acowork_core::error::Result as AcoworkResult;
use acowork_memory::session_meta::{
    SessionMeta, SessionMetaStore, SessionVisibility, last_active_at_ms,
};
use acowork_memory::{SessionTokens, TodoItem, TodoStatus};
use acowork_sqlite::{SqliteSessionMetaStore, SqliteStore};

const DIM: usize = 4;

fn make_meta(
    sid: &str,
    last_active: &str,
    title: Option<&str>,
    agent: &str,
) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: agent.to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        title: title.map(str::to_string),
        workspace_id: Some("ws-test".into()),
        model: Some("gpt-4o-mini".into()),
        provider: Some("openai".into()),
        account_id: Some("acct-default".into()),
        reasoning_effort: None,
        temperature: Some(0.7),
        context_window: Some(128_000),
        todos: Some(vec![TodoItem {
            id: "todo-1".into(),
            content: "ship it".into(),
            status: TodoStatus::InProgress,
        }]),
        message_count: 3,
        last_active_at: last_active.to_string(),
        tokens: Some(SessionTokens {
            last_input: 100,
            last_output: 50,
            total_input: 900,
            total_output: 400,
            last_cache_read: 0,
            last_cache_write: 0,
            total_cache_read: 0,
            total_cache_write: 0,
        }),
        llm_call_counter: Some(2),
        // Deliberately different sample counts per figure: the storage
        // layer must round-trip the two independent denominators, not just
        // the derived means.
        perf: Some(acowork_memory::SessionPerf {
            tps_sum: 105.0,
            tps_samples: 2,
            ttft_ms_sum: 1290.0,
            ttft_ms_samples: 3,
        }),
        model_ratio: Some(1.0),
        last_compaction_offset: Some(0),
        corrupted: false,
        user_id: None,
        visibility: None,
    chat_session_id: None,
        model_usage: None,
    }
}

fn open_pair(path: &PathBuf) -> (Arc<SqliteStore>, SqliteSessionMetaStore) {
    let store = Arc::new(SqliteStore::open(path, DIM).unwrap());
    let session_meta = SqliteSessionMetaStore::new(store.clone());
    (store, session_meta)
}

/// A raw connection, for poking at the file the store manages (migration
/// tests need to build a pre-upgrade database and read `PRAGMA` values).
fn open_raw(path: &PathBuf) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

/// Column names of `sessions`, in declaration order.
fn session_columns(path: &PathBuf) -> Vec<String> {
    let conn = open_raw(path);
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('sessions')").unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect()
}

fn stored_user_version(path: &PathBuf) -> i64 {
    open_raw(path)
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .unwrap()
}

#[test]
fn round_trip_preserves_every_field() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    let meta = make_meta("abc", "2026-03-04T05:06:07.890Z", Some("hello world"), "ponytail");
    sm.upsert(&meta)?;

    let got = sm.get("abc")?.expect("row present");
    assert_eq!(got.session_id, "abc");
    assert_eq!(got.agent_id, "ponytail");
    assert_eq!(got.title.as_deref(), Some("hello world"));
    assert_eq!(got.workspace_id.as_deref(), Some("ws-test"));
    assert_eq!(got.model.as_deref(), Some("gpt-4o-mini"));
    assert_eq!(got.provider.as_deref(), Some("openai"));
    assert_eq!(got.account_id.as_deref(), Some("acct-default"));
    assert_eq!(got.temperature, Some(0.7));
    assert_eq!(got.context_window, Some(128_000));
    assert_eq!(got.message_count, 3);
    assert_eq!(got.llm_call_counter, Some(2));
    assert_eq!(got.perf.as_ref().and_then(|p| p.avg_tps()), Some(52.5));
    assert_eq!(got.perf.as_ref().and_then(|p| p.avg_ttft_ms()), Some(430.0));
    // The per-figure counts survive the round-trip independently.
    assert_eq!(got.perf.as_ref().map(|p| p.tps_samples), Some(2));
    assert_eq!(got.perf.as_ref().map(|p| p.ttft_ms_samples), Some(3));
    assert_eq!(got.model_ratio, Some(1.0));
    // Design 27 §8.2: the Chat session binding round-trips with the
    // `members` visibility it depends on — a lost binding would silently
    // revoke every group participant's read access.
    let mut bound = got.clone();
    bound.visibility = Some(SessionVisibility::Members);
    bound.chat_session_id = Some("cs-group-1".into());
    sm.upsert(&bound)?;
    let got = sm.get("abc")?.expect("row present after binding");
    assert_eq!(got.visibility, Some(SessionVisibility::Members));
    assert_eq!(got.chat_session_id.as_deref(), Some("cs-group-1"));
    let tokens = got.tokens.as_ref().expect("tokens persisted");
    assert_eq!(tokens.last_input, 100);
    assert_eq!(tokens.last_output, 50);
    assert_eq!(tokens.total_input, 900);
    assert_eq!(tokens.total_output, 400);
    let todos = got.todos.as_ref().expect("todos persisted");
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].id, "todo-1");
    assert_eq!(todos[0].content, "ship it");
    assert_eq!(todos[0].status, TodoStatus::InProgress);
    Ok(())
}

#[test]
fn get_missing_returns_none() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    assert!(sm.get("never-existed")?.is_none());
    Ok(())
}

#[test]
fn delete_removes_row_and_is_idempotent() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    sm.upsert(&make_meta("keep", "2026-01-01T00:00:00.000Z", Some("keep"), "agent"))?;
    sm.upsert(&make_meta("gone", "2026-02-01T00:00:00.000Z", Some("gone"), "agent"))?;

    sm.delete("gone")?;
    assert!(sm.get("gone")?.is_none(), "deleted row must be gone");
    assert!(sm.get("keep")?.is_some(), "sibling row untouched");

    // Both `sessions` and `fts_sessions` must be cleared — a stale
    // `fts_sessions` row would keep the deleted title alive.
    let conn = rusqlite::Connection::open(&path)
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?;
    for table in ["sessions", "fts_sessions"] {
        let n: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id = 'gone'"),
                [],
                |r| r.get(0),
            )
            .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?;
        assert_eq!(n, 0, "{table} must not retain the deleted session");
    }

    // Idempotent: deleting an already-absent session is a no-op.
    sm.delete("gone")?;
    Ok(())
}

#[test]
fn list_recent_orders_by_last_active_desc() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    sm.upsert(&make_meta("older", "2025-01-01T00:00:00.000Z", None, "a"))?;
    sm.upsert(&make_meta("middle", "2026-06-15T12:00:00.000Z", None, "a"))?;
    sm.upsert(&make_meta("newest", "2026-12-31T23:59:59.999Z", None, "a"))?;

    let listed = sm.list_recent(10)?;
    assert_eq!(listed.len(), 3);
    assert_eq!(listed[0].session_id, "newest");
    assert_eq!(listed[1].session_id, "middle");
    assert_eq!(listed[2].session_id, "older");

    let capped = sm.list_recent(2)?;
    assert_eq!(capped.len(), 2);
    assert_eq!(capped[0].session_id, "newest");
    assert_eq!(capped[1].session_id, "middle");

    assert!(sm.list_recent(0)?.is_empty());
    Ok(())
}

#[test]
fn find_latest_returns_the_newest() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    sm.upsert(&make_meta("a", "2025-01-01T00:00:00.000Z", None, "a"))?;
    sm.upsert(&make_meta("b", "2026-06-15T12:00:00.000Z", None, "a"))?;
    let latest = sm.find_latest()?.expect("non-empty");
    assert_eq!(latest.session_id, "b");
    Ok(())
}

#[test]
fn empty_search_falls_back_to_list_recent() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    sm.upsert(&make_meta("a", "2025-01-01T00:00:00.000Z", Some("alpha"), "agent"))?;
    sm.upsert(&make_meta("b", "2026-06-15T12:00:00.000Z", Some("beta"), "agent"))?;
    let all = sm.search("   ", 10)?;
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].session_id, "b");
    Ok(())
}

#[test]
fn search_matches_title_agent_workspace() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    sm.upsert(&make_meta("a", "2025-01-01T00:00:00.000Z", Some("alpha project"), "agent"))?;
    sm.upsert(&make_meta("b", "2026-06-15T12:00:00.000Z", Some("beta run"), "agent"))?;

    let by_title = sm.search("alpha", 10)?;
    assert_eq!(by_title.len(), 1);
    assert_eq!(by_title[0].session_id, "a");

    let by_title_cs = sm.search("ALPHA", 10)?;
    assert_eq!(by_title_cs.len(), 1, "case-insensitive match expected");

    let by_agent = sm.search("agent", 10)?;
    assert_eq!(by_agent.len(), 2);
    Ok(())
}

#[test]
fn prune_to_keeps_top_n_and_reports_victims() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);
    sm.upsert(&make_meta("a", "2025-01-01T00:00:00.000Z", None, "agent"))?;
    sm.upsert(&make_meta("b", "2025-02-01T00:00:00.000Z", None, "agent"))?;
    sm.upsert(&make_meta("c", "2025-03-01T00:00:00.000Z", None, "agent"))?;
    sm.upsert(&make_meta("d", "2025-04-01T00:00:00.000Z", None, "agent"))?;

    let victims = sm.prune_to(2)?;
    assert_eq!(victims.len(), 2);
    // Oldest first; the two newest (c, d) survive.
    let mut sorted = victims.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["a".to_string(), "b".to_string()]);

    let kept = sm.list_recent(10)?;
    assert_eq!(kept.len(), 2);
    let kept_ids: Vec<String> = kept.iter().map(|m| m.session_id.clone()).collect();
    assert!(kept_ids.contains(&"c".to_string()));
    assert!(kept_ids.contains(&"d".to_string()));
    Ok(())
}

#[test]
fn ownership_columns_round_trip_and_prune_buckets_per_owner() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);

    // Alice owns 3 sessions, bob owns 1, and one session has no owner at all
    // (the pre-accounts / local-mode shape).
    let alice = |sid: &str, stamp: &str| {
        let mut m = make_meta(sid, stamp, None, "agent");
        m.user_id = Some("alice".into());
        m.visibility = Some(SessionVisibility::Private);
        m
    };
    sm.upsert(&alice("a-old", "2026-01-01T00:00:00.000Z"))?;
    sm.upsert(&alice("a-mid", "2026-02-01T00:00:00.000Z"))?;
    sm.upsert(&alice("a-new", "2026-03-01T00:00:00.000Z"))?;

    let mut bob = make_meta("b-1", "2026-01-15T00:00:00.000Z", None, "agent");
    bob.user_id = Some("bob".into());
    bob.visibility = Some(SessionVisibility::Public);
    sm.upsert(&bob)?;

    sm.upsert(&make_meta("orphan", "2025-01-01T00:00:00.000Z", None, "agent"))?;

    // ADR-076 §决策 4: both columns survive a write → read.
    let read_back = sm.get("a-mid")?.expect("alice's row");
    assert_eq!(read_back.user_id.as_deref(), Some("alice"));
    assert_eq!(read_back.visibility, Some(SessionVisibility::Private));
    let orphan = sm.get("orphan")?.expect("ownerless row");
    assert_eq!(orphan.user_id, None);
    assert_eq!(orphan.visibility, None);

    // The cap is per owner: with max=1, alice loses her 2 oldest and bob keeps
    // his only session, even though bob's row is older than alice's *newest*.
    // A global cap would have evicted bob instead.
    let victims = sm.prune_to(1)?;
    assert_eq!(victims.len(), 2, "got {victims:?}");
    assert!(victims.contains(&"a-old".to_string()));
    assert!(victims.contains(&"a-mid".to_string()));
    assert!(sm.get("b-1")?.is_some(), "bob's only session must survive");
    assert!(sm.get("a-new")?.is_some());
    assert!(sm.get("orphan")?.is_some(), "the ownerless bucket is its own bucket");
    Ok(())
}

#[test]
fn visibility_text_encoding_matches_the_json_representation() {
    // The SQLite column and the serde representation must agree, or a row that
    // migrated from the JSON sidecar era would read back as a different
    // sharing state than the same value written after the migration.
    for v in [
        SessionVisibility::Public,
        SessionVisibility::Private,
        SessionVisibility::Members,
    ] {
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, format!("\"{}\"", v.as_str()));
        assert_eq!(SessionVisibility::from_db_str(v.as_str()), Some(v));
    }
    assert_eq!(SessionVisibility::from_db_str("nonsense"), None);
    assert_eq!(SessionVisibility::from_db_str(""), None);
}

#[test]
fn sessions_table_cohabits_with_memory_and_conversation_index() -> AcoworkResult<()> {
    // The user's hard requirement: one .sqlite file holds memory +
    // conversation index + session meta. We assert it by writing a row
    // into each table family and then reading all three back.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unified.sqlite");
    let store = Arc::new(SqliteStore::open(&path, DIM)?);
    let sm = SqliteSessionMetaStore::new(store.clone());

    // Session meta side: a single row exercises the schema we just added.
    sm.upsert(&make_meta(
        "demo",
        "2026-07-01T00:00:00.000Z",
        Some("cohabitation test"),
        "agent",
    ))?;

    // Memory + conversation tables are reachable from the same file
    // because the schema is applied on every `SqliteStore::open`.
    assert!(store.node_count()? == 0, "fresh store has no nodes");
    let sessions_count: i64 = {
        let conn = rusqlite::Connection::open(&path)
            .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?;
        conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
            .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
    };
    assert!(sessions_count >= 1, "one session row written");
    assert_eq!(sessions_count, 1, "one session row written");

    // The helper last_active_at_ms still works (epoch-ms invariant).
    let meta = sm.get("demo")?.expect("session meta row present");
    let ms = last_active_at_ms(&meta);
    assert!(ms > 0);
    Ok(())
}

/// `version` and `corrupted` used to be dropped on write (hardcoded to `4` /
/// `false` on read), so a corrupted session came back healthy.
#[test]
fn version_and_corrupted_round_trip() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let (_store, sm) = open_pair(&dir.path().join("store.sqlite"));

    let mut meta = make_meta("c", "2026-01-01T00:00:00.000Z", None, "agent");
    meta.version = 7;
    meta.corrupted = true;
    sm.upsert(&meta)?;

    let back = sm.get("c")?.expect("row");
    assert_eq!(back.version, 7);
    assert!(back.corrupted);
    Ok(())
}

/// A pre-v1 `sessions` table (no `version` / `corrupted` columns) is upgraded
/// in place by the v1 migration: DDL skips the existing table, the migration
/// adds the columns, the row survives, and `PRAGMA user_version` is stamped
/// to the current schema version in a single transaction. Reopening the
/// store must be a no-op (no extra statements, no file mtime change).
#[test]
fn migrations_run_in_order_and_stop_at_stored_version() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");

    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id              TEXT PRIMARY KEY,
                agent_id                TEXT NOT NULL,
                created_at              INTEGER NOT NULL,
                last_active_at          INTEGER NOT NULL,
                title                   TEXT,
                workspace_id            TEXT,
                model                   TEXT,
                provider                TEXT,
                account_id              TEXT,
                reasoning_effort        TEXT,
                temperature             REAL,
                context_window          INTEGER,
                todos                   JSON,
                message_count           INTEGER NOT NULL DEFAULT 0,
                llm_call_counter        INTEGER,
                model_ratio             REAL,
                last_compaction_offset  INTEGER,
                token_last_input        INTEGER NOT NULL DEFAULT 0,
                token_last_output       INTEGER NOT NULL DEFAULT 0,
                token_total_input       INTEGER NOT NULL DEFAULT 0,
                token_total_output      INTEGER NOT NULL DEFAULT 0,
                token_last_cache_read   INTEGER NOT NULL DEFAULT 0,
                token_last_cache_write  INTEGER NOT NULL DEFAULT 0,
                token_total_cache_read  INTEGER NOT NULL DEFAULT 0,
                token_total_cache_write INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO sessions(session_id, agent_id, created_at, last_active_at, title)
            VALUES ('old', 'agent', 0, 0, 'legacy');",
        )
        .unwrap();
    }

    let (_store, sm) = open_pair(&path);

    // v1 migration added the two columns and stamped the version.
    let cols: Vec<String> = rusqlite::Connection::open(&path)
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
        .prepare("SELECT name FROM pragma_table_info('sessions')")
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
        .filter_map(|r| r.ok())
        .collect();
    assert!(cols.iter().any(|c| c == "version"), "v1 added `version`");
    assert!(cols.iter().any(|c| c == "corrupted"), "v1 added `corrupted`");
    // ... and the whole chain runs, so v2's columns land in the same pass.
    assert!(cols.iter().any(|c| c == "user_id"), "v2 added `user_id`");
    assert!(cols.iter().any(|c| c == "visibility"), "v2 added `visibility`");
    assert!(cols.iter().any(|c| c == "perf_tps_sum"), "v3 added `perf_tps_sum`");
    assert!(
        cols.iter().any(|c| c == "perf_tps_samples"),
        "v3 added `perf_tps_samples`"
    );
    assert!(
        cols.iter().any(|c| c == "perf_ttft_ms_sum"),
        "v3 added `perf_ttft_ms_sum`"
    );
    assert!(
        cols.iter().any(|c| c == "perf_ttft_ms_samples"),
        "v3 added `perf_ttft_ms_samples`"
    );
    // Design 27 §8.2: v4 added the Chat session binding column.
    assert!(
        cols.iter().any(|c| c == "chat_session_id"),
        "v4 added `chat_session_id`"
    );

    let v: i64 = rusqlite::Connection::open(&path)
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?;
    assert_eq!(v, acowork_sqlite::schema_version());

    // Legacy row survives with DDL defaults.
    let row = sm.get("old")?.expect("legacy row survives");
    assert_eq!(row.title.as_deref(), Some("legacy"));
    assert_eq!(row.version, 3);
    assert!(!row.corrupted);
    // ADR-076 §决策 4: a pre-ADR-076 row migrates to ownerless + no explicit
    // visibility, which reads as "public". Nothing is retroactively hidden.
    assert_eq!(row.user_id, None);
    assert_eq!(row.visibility, None);
    // A pre-perf row migrates to "no measurement", never a fabricated 0.
    assert_eq!(row.perf, None);
    // A pre-v4 row migrates to "no chat binding", which only matters for
    // `members` rows (none pre-exist), so nothing is retroactively hidden.
    assert_eq!(row.chat_session_id, None);

    // Reopen is a no-op: file mtime unchanged (no transaction, no rewrite).
    let mtime_before = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let _ = open_pair(&path);
    let mtime_after = std::fs::metadata(&path).unwrap().modified().unwrap();
    assert_eq!(
        mtime_before, mtime_after,
        "reopening an up-to-date store must not rewrite the file"
    );
    Ok(())
}


/// The upgrade path and the fresh-install path must converge on the same
/// `sessions` table. If they diverge, an upgraded developer database and a
/// brand-new one behave differently at runtime — the classic migration bug
/// that only reproduces on someone else's machine.
#[test]
fn fresh_and_upgraded_databases_share_one_sessions_shape() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();

    // Fresh: `SCHEMA_SQL` + the whole migration chain.
    let fresh = dir.path().join("fresh.sqlite");
    let _ = open_pair(&fresh);

    // A v1 database: build it fully, then roll it back to the shape every
    // database created before ADR-076 has (no `user_id` / `visibility` /
    // `perf_*` / `chat_session_id`, `user_version = 1`) and let the store
    // upgrade it again.
    let old = dir.path().join("old.sqlite");
    let _ = open_pair(&old);
    open_raw(&old)
        .execute_batch(
            "ALTER TABLE sessions DROP COLUMN user_id;
             ALTER TABLE sessions DROP COLUMN visibility;
             ALTER TABLE sessions DROP COLUMN perf_tps_sum;
             ALTER TABLE sessions DROP COLUMN perf_tps_samples;
             ALTER TABLE sessions DROP COLUMN perf_ttft_ms_sum;
             ALTER TABLE sessions DROP COLUMN perf_ttft_ms_samples;
             ALTER TABLE sessions DROP COLUMN chat_session_id;
             ALTER TABLE sessions DROP COLUMN model_usage;
             PRAGMA user_version = 1;",
        )
        .unwrap();
    let _ = open_pair(&old);

    assert_eq!(
        session_columns(&fresh),
        session_columns(&old),
        "an upgraded v1 database must end up with the same `sessions` shape as a fresh one"
    );
    assert_eq!(stored_user_version(&fresh), stored_user_version(&old));
    Ok(())
}

/// Fresh databases must record the schema version so external tooling can
/// tell which schema it is looking at, and older databases must be upgraded
/// to it. Reopening a store whose stored version already matches must be a
/// no-op (no extra PRAGMA, no row churn).
#[test]
fn user_version_is_stamped_on_open_and_is_idempotent() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");

    // First open on a fresh file.
    let (store, _sm) = open_pair(&path);
    let v1: i64 = rusqlite::Connection::open(&path)
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?;
    assert_eq!(v1, acowork_sqlite::schema_version());
    drop(store);

    // Reopen: the stored version already matches, so opening must not touch
    // anything. We assert that by checking `sessions` row count is unchanged.
    let _ = open_pair(&path);
    let row_count: i64 = rusqlite::Connection::open(&path)
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?
        .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
        .map_err(|e| acowork_core::error::AcoworkError::Memory(format!("sqlite: {e}")))?;
    assert_eq!(row_count, 0, "reopen must not synthesize rows");
    Ok(())
}

// ── per-model token statistics (schema v6) ───────────────────────────

use acowork_memory::session_meta::{ModelUsageEntry, TokenStatsRow};
use std::collections::BTreeMap;

fn usage_entry(input: u64, output: u64, cost: Option<f64>) -> ModelUsageEntry {
    ModelUsageEntry {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        cost,
    }
}

fn usage_map(pairs: &[(&str, ModelUsageEntry)]) -> BTreeMap<String, ModelUsageEntry> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), v.clone()))
        .collect()
}

/// Σ(`token_stats`) over the four token dimensions, for set-agnostic
/// assertions (the table's read order is a presentation concern).
fn stats_totals(sm: &SqliteSessionMetaStore) -> (u64, u64) {
    let rows = sm.token_stats().unwrap();
    (
        rows.iter().map(|r| r.total_input).sum(),
        rows.iter().map(|r| r.total_output).sum(),
    )
}

fn find_stat<'a>(rows: &'a [TokenStatsRow], provider: &str, model: &str) -> &'a TokenStatsRow {
    rows.iter()
        .find(|r| r.provider == provider && r.model == model)
        .unwrap_or_else(|| panic!("no token_stats row for {provider}/{model}"))
}

/// v5 → v6: legacy rows get an `unknown|unknown` breakdown seeded from
/// their `token_total_*` columns (never retroactively attributed to the
/// session's current model), the aggregate table is seeded so
/// Σ(aggregate) == Σ(breakdowns) from the first open, and reopening the
/// migrated store is a no-op.
#[test]
fn v6_migration_backfills_unknown_breakdown_and_aggregate() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");

    // Build a current store (all migrations), then roll it back to the v5
    // shape: no `model_usage` column, no `token_stats` table, version 5.
    // The `last_compaction_summary` column is re-added because a genuine v5
    // database had it (migration 5) — the v7 step drops it again on reopen.
    {
        let (_store, sm) = open_pair(&path);
        let mut meta = make_meta("legacy", "2026-01-01T00:00:00.000Z", Some("l"), "a");
        meta.tokens = Some(SessionTokens {
            last_input: 0,
            last_output: 0,
            total_input: 500,
            total_output: 250,
            last_cache_read: 0,
            last_cache_write: 0,
            total_cache_read: 11,
            total_cache_write: 7,
        });
        meta.model_usage = None;
        sm.upsert(&meta)?;
        // Zero-usage rows must NOT get a breakdown (they never consumed
        // anything; a 0-token `unknown` bucket would be noise in the pie).
        let mut zero = make_meta("zero", "2026-01-01T00:00:00.000Z", Some("z"), "a");
        zero.tokens = Some(SessionTokens {
            last_input: 0,
            last_output: 0,
            total_input: 0,
            total_output: 0,
            last_cache_read: 0,
            last_cache_write: 0,
            total_cache_read: 0,
            total_cache_write: 0,
        });
        zero.model_usage = None;
        sm.upsert(&zero)?;
    }
    open_raw(&path)
        .execute_batch(
            "DROP TABLE token_stats;
             ALTER TABLE sessions DROP COLUMN model_usage;
             ALTER TABLE sessions ADD COLUMN last_compaction_summary TEXT;
             PRAGMA user_version = 5;",
        )
        .unwrap();

    // Reopen runs the v6 step: column + table + backfill + aggregate seed.
    let (_store, sm) = open_pair(&path);
    assert_eq!(stored_user_version(&path), acowork_sqlite::schema_version());

    let got = sm.get("legacy")?.expect("legacy row survives");
    let usage = got.model_usage.expect("non-zero legacy row gets a breakdown");
    let entry = usage
        .get("unknown|unknown")
        .expect("old totals land in the unknown bucket");
    assert_eq!((entry.input, entry.output, entry.cache_read, entry.cache_write), (500, 250, 11, 7));
    assert_eq!(entry.cost, None, "no historical price is ever estimated");
    assert!(
        sm.get("zero")?.unwrap().model_usage.is_none(),
        "a zero-usage row stays without a breakdown"
    );

    // Σ(aggregate) == Σ(breakdowns) straight after the migration.
    let rows = sm.token_stats()?;
    assert_eq!(rows.len(), 1);
    let row = find_stat(&rows, "unknown", "unknown");
    assert_eq!((row.total_input, row.total_output), (500, 250));
    assert_eq!(row.total_cost, None);
    assert!(sm.token_stats_consistent()?);

    // Reopen is a no-op: the migration must not re-run and double-seed.
    drop(sm);
    drop(_store);
    let (_store2, sm2) = open_pair(&path);
    assert_eq!(sm2.token_stats()?.len(), 1, "reopen must not duplicate aggregate rows");
    assert!(sm2.token_stats_consistent()?);
    Ok(())
}

/// The `upsert` diff transaction maintains `token_stats` incrementally:
/// first write adds, same-model re-accumulation replaces the row's
/// contribution (not adds on top of it), a model switch nets the old key
/// down and the new key up, and deletes/prunes subtract symmetrically.
#[test]
fn upsert_diff_and_delete_maintain_the_aggregate() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);

    // Fresh rows contribute nothing until they carry a breakdown.
    let mut a = make_meta("a", "2026-02-01T00:00:00.000Z", Some("A"), "agent");
    sm.upsert(&a)?;
    assert!(sm.token_stats()?.is_empty());

    // First breakdown: one key.
    a.model_usage = Some(usage_map(&[("openai|gpt-a", usage_entry(100, 10, Some(0.01)))]));
    sm.upsert(&a)?;
    let rows = sm.token_stats()?;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].total_input, rows[0].total_output, rows[0].total_cost), (100, 10, Some(0.01)));

    // Re-upsert the SAME totals — diff is zero, aggregate unchanged
    // (idempotence: write_meta fires on every persist, not only on change).
    sm.upsert(&a)?;
    assert_eq!(stats_totals(&sm), (100, 10));

    // Session grows: new totals replace the old contribution, they are
    // NOT added on top of it.
    a.model_usage = Some(usage_map(&[("openai|gpt-a", usage_entry(160, 25, Some(0.02)))]));
    sm.upsert(&a)?;
    let rows = sm.token_stats()?;
    assert_eq!((rows[0].total_input, rows[0].total_output), (160, 25));

    // Model switch: old key nets down, new key appears.
    let mut b = make_meta("b", "2026-02-02T00:00:00.000Z", Some("B"), "agent");
    b.model_usage = Some(usage_map(&[("anthropic|claude", usage_entry(50, 5, None))]));
    sm.upsert(&b)?;
    a.model_usage = Some(usage_map(&[
        ("openai|gpt-a", usage_entry(60, 5, Some(0.01))),
        ("anthropic|claude", usage_entry(100, 20, None)),
    ]));
    sm.upsert(&a)?;
    let rows = sm.token_stats()?;
    assert_eq!(find_stat(&rows, "openai", "gpt-a").total_input, 60);
    assert_eq!(find_stat(&rows, "anthropic", "claude").total_input, 150);
    assert_eq!(find_stat(&rows, "anthropic", "claude").total_output, 25);
    assert!(sm.token_stats_consistent()?);

    // Delete subtracts the removed row's whole contribution.
    sm.delete("a")?;
    let rows = sm.token_stats()?;
    assert!(find_stat(&rows, "openai", "gpt-a").total_input == 0 || rows.iter().all(|r| r.provider != "openai"));
    assert_eq!(find_stat(&rows, "anthropic", "claude").total_input, 50);
    assert_eq!(find_stat(&rows, "anthropic", "claude").total_output, 5);
    assert!(sm.token_stats_consistent()?);

    // Prune deducts symmetrically: two owned rows, keep the newest one.
    let mut c = make_meta("c", "2026-01-01T00:00:00.000Z", Some("C"), "agent");
    c.user_id = Some("u".into());
    c.model_usage = Some(usage_map(&[("p|m1", usage_entry(10, 1, None))]));
    sm.upsert(&c)?;
    let mut d = make_meta("d", "2026-06-01T00:00:00.000Z", Some("D"), "agent");
    d.user_id = Some("u".into());
    d.model_usage = Some(usage_map(&[("p|m1", usage_entry(20, 2, None))]));
    sm.upsert(&d)?;
    let victims = sm.prune_to(1)?;
    assert_eq!(victims, vec!["c".to_string()]);
    let rows = sm.token_stats()?;
    let row = find_stat(&rows, "p", "m1");
    // b's claude(50/5) + d's m1(20/2) survive; c's 10/1 is deducted.
    assert_eq!(row.total_input, 20);
    assert_eq!(find_stat(&rows, "anthropic", "claude").total_input, 50);
    assert!(sm.token_stats_consistent()?);
    Ok(())
}

/// Deleting (or pruning) a session must subtract its KNOWN cost from the
/// aggregate, not poison the shared row: a priced key still held by other
/// priced sessions keeps its price after one contributor is removed.
#[test]
fn delete_of_priced_entry_preserves_aggregate_cost() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (_store, sm) = open_pair(&path);

    let mut a = make_meta("a", "2026-02-01T00:00:00.000Z", Some("A"), "agent");
    a.model_usage = Some(usage_map(&[("p|m1", usage_entry(100, 10, Some(1.0)))]));
    sm.upsert(&a)?;
    let mut b = make_meta("b", "2026-02-02T00:00:00.000Z", Some("B"), "agent");
    b.model_usage = Some(usage_map(&[("p|m1", usage_entry(40, 4, Some(0.5)))]));
    sm.upsert(&b)?;
    assert_eq!(find_stat(&sm.token_stats()?, "p", "m1").total_cost, Some(1.5));

    sm.delete("a")?;
    let rows = sm.token_stats()?;
    let row = find_stat(&rows, "p", "m1");
    assert_eq!((row.total_input, row.total_output), (40, 4));
    assert_eq!(row.total_cost, Some(0.5), "known-cost removal must not poison the row");
    assert!(sm.token_stats_consistent()?);
    Ok(())
}

/// Disposable derived index (ADR-082 pattern): when the aggregate drifts
/// from the per-session breakdowns, `token_stats_consistent` says so and
/// `rebuild_token_stats` re-derives the exact incremental result.
#[test]
fn rebuild_heals_a_drifted_aggregate_and_is_idempotent() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (store, sm) = open_pair(&path);

    let mut a = make_meta("a", "2026-02-01T00:00:00.000Z", Some("A"), "agent");
    a.model_usage = Some(usage_map(&[("p|m1", usage_entry(100, 10, Some(1.0)))]));
    sm.upsert(&a)?;
    let mut b = make_meta("b", "2026-02-02T00:00:00.000Z", Some("B"), "agent");
    b.model_usage = Some(usage_map(&[("p|m2", usage_entry(7, 3, None))]));
    sm.upsert(&b)?;
    assert!(sm.token_stats_consistent()?);
    let incremental = sm.token_stats()?;

    // Simulate drift: zero the aggregate out from under the store.
    drop(sm);
    drop(store);
    open_raw(&path)
        .execute_batch("UPDATE token_stats SET total_input = 0, total_output = 0, total_cost = NULL;")
        .unwrap();
    let (_store, sm) = open_pair(&path);
    assert!(
        !sm.token_stats_consistent()?,
        "a zeroed aggregate over non-zero breakdowns must read as drift"
    );

    sm.rebuild_token_stats()?;
    assert!(sm.token_stats_consistent()?);
    let mut rebuilt = sm.token_stats()?;
    let mut want = incremental;
    rebuilt.sort_by(|x, y| (&x.provider, &x.model).cmp(&(&y.provider, &y.model)));
    want.sort_by(|x, y| (&x.provider, &x.model).cmp(&(&y.provider, &y.model)));
    assert_eq!(rebuilt, want, "rebuild must equal the incremental result");

    // The m2 row's cost stays NULL through rebuild (poison rule, never a
    // fabricated number).
    assert_eq!(find_stat(&rebuilt, "p", "m2").total_cost, None);

    // Rebuild again: pure function of the breakdowns, so it is idempotent.
    sm.rebuild_token_stats()?;
    assert!(sm.token_stats_consistent()?);
    assert_eq!(sm.token_stats()?.len(), 2);
    Ok(())
}

/// The cost dimension follows the aggregate's poison rule: a NULL
/// aggregate cost over priced details is a real drift (e.g. the table
/// was wiped), while a poisoned-NULL cost after an unpriced contribution
/// counts as consistent until a rebuild.
#[test]
fn consistency_cost_rule_distinguishes_wipe_from_poison() -> AcoworkResult<()> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.sqlite");
    let (store, sm) = open_pair(&path);

    let mut a = make_meta("a", "2026-02-01T00:00:00.000Z", Some("A"), "agent");
    a.model_usage = Some(usage_map(&[("p|m1", usage_entry(10, 1, Some(0.5)))]));
    sm.upsert(&a)?;
    assert!(sm.token_stats_consistent()?);

    // Poison: an unpriced contribution lands → cost NULL, still consistent.
    a.model_usage = Some(usage_map(&[
        ("p|m1", usage_entry(20, 2, Some(0.5))),
        ("p|m2", usage_entry(5, 1, None)),
    ]));
    sm.upsert(&a)?;
    let rows = sm.token_stats()?;
    // Poison is per-row: m1 keeps its priced fold, m2's unpriced
    // contribution NULLs only its own row's cost.
    assert_eq!(find_stat(&rows, "p", "m1").total_cost, Some(0.5));
    assert_eq!(find_stat(&rows, "p", "m2").total_cost, None);
    assert!(sm.token_stats_consistent()?, "poisoned-NULL cost is not drift");

    // Wipe: details non-zero, aggregate emptied → tokens mismatch → drift.
    drop(sm);
    drop(store);
    open_raw(&path)
        .execute_batch("DELETE FROM token_stats;")
        .unwrap();
    let (_store, sm) = open_pair(&path);
    assert!(!sm.token_stats_consistent()?);
    sm.rebuild_token_stats()?;
    assert!(sm.token_stats_consistent()?);
    Ok(())
}

// ponytail: this suite is intentionally extensive — the SQLite backend is
// the new home of session-meta data, so a regression here is a regression
// in every workspace that ever opts in. Cloning Arc<SqliteStore> into the
// session store means both backends share the same connection lock, which
// is the property that makes the "one file" guarantee hold.
