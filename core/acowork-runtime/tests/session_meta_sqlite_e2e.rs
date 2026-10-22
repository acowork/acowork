//! End-to-end wiring test for ADR-082 §4 step 3: the runtime's session-meta
//! free functions must serve from the SQLite store, and that store must be
//! reachable before the memory backend is installed.
//!
//! Lives in its own integration-test binary because
//! `conversation::install_session_meta_backend` registers a process-wide store
//! keyed by the workspace `.sqlite` path — another test in the same binary
//! sharing that path would alias it.

use std::sync::Arc;

use acowork_core::error::AcoworkError;
use acowork_memory::SessionMetaStore;
use acowork_memory::session_meta::SessionMeta;
use acowork_sqlite::{SqliteSessionMetaStore, SqliteStore};

use acowork_runtime::conversation::{
    delete_session_meta, find_latest_session, install_session_meta_backend, read_session_meta,
    scan_sessions_from_meta, session_meta_exists, write_session_meta,
};

const DIM: usize = 8;

fn make_meta(sid: &str, last_active: &str, title: &str) -> SessionMeta {
    SessionMeta {
        version: 4,
        session_id: sid.to_string(),
        agent_id: "test-agent".to_string(),
        created_at: "2026-01-01T00:00:00.000Z".to_string(),
        user_id: None,
        visibility: None,
        title: Some(title.to_string()),
        workspace_id: None,
        model: Some("test-model".into()),
        provider: Some("test-provider".into()),
        account_id: None,
        reasoning_effort: None,
        temperature: None,
        context_window: None,
        todos: None,
        message_count: 1,
        last_active_at: last_active.to_string(),
        tokens: None,
        llm_call_counter: None,
        perf: None,
        model_ratio: None,
        last_compaction_offset: None,
        last_compaction_summary: None,
        corrupted: false,
    chat_session_id: None,
        model_usage: None,
    }
}

#[test]
fn session_meta_is_served_from_the_memory_file() -> Result<(), AcoworkError> {
    let ws = tempfile::tempdir().unwrap();

    // Open the SQLite store in the memory dir — the same file the runtime
    // uses by default (one file for memory + session meta + conversation
    // vectors).
    let store = Arc::new(SqliteStore::open(
        ws.path().join("memory").join("private.sqlite"),
        DIM,
    )?);
    let sm = Arc::new(SqliteSessionMetaStore::new(store));

    // Two existing sessions, as a booted install has.
    for (sid, stamp, title) in [
        ("s1", "2025-01-01T00:00:00.000Z", "first session"),
        ("s2", "2025-02-01T00:00:00.000Z", "second session"),
    ] {
        sm.upsert(&make_meta(sid, stamp, title))?;
    }

    install_session_meta_backend(&ws.path().join("conversations"), sm.clone());

    // Reads come from SQLite.
    let got = read_session_meta(&ws.path().join("conversations"), "s1").unwrap();
    assert_eq!(got.title.as_deref(), Some("first session"));
    assert_eq!(got.model.as_deref(), Some("test-model"));

    // A brand-new write lands in SQLite only — no JSON sidecar, no `meta/`
    // directory.
    let s3 = make_meta("s3", "2026-06-01T00:00:00.000Z", "third session");
    write_session_meta(&ws.path().join("conversations"), &s3).unwrap();
    assert!(
        !ws.path().join("conversations").join("meta").exists(),
        "the SQLite backend must not create a meta/ directory"
    );
    let round = read_session_meta(&ws.path().join("conversations"), "s3").unwrap();
    assert_eq!(round.title.as_deref(), Some("third session"));

    // The list path sees all three rows, newest first.
    let all = scan_sessions_from_meta(&ws.path().join("conversations"));
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].0, "s3", "newest session first");

    // ── Lifecycle probe + delete (ADR-082 §4 step 3 regression) ──────────
    //
    // `session_meta_exists` is what `get_lifecycle_state` / `open` consult.
    // Probing the legacy sidecar path reported a live session as `NotFound`,
    // which is the bug this test locks down.
    let conv_dir = ws.path().join("conversations");
    assert!(
        session_meta_exists(&conv_dir, "s3"),
        "sqlite-backed session must be visible to the lifecycle probe"
    );
    assert!(!session_meta_exists(&conv_dir, "does-not-exist"));

    // Delete drops the row, so the probe flips to absent, reads fail, and
    // the session leaves the list — i.e. it stays out of `/sessions`.
    delete_session_meta(&conv_dir, "s3").unwrap();
    assert!(!session_meta_exists(&conv_dir, "s3"));
    assert!(read_session_meta(&conv_dir, "s3").is_err());
    assert_eq!(scan_sessions_from_meta(&conv_dir).len(), 2);
    // Idempotent: deleting an absent session is a no-op, not an error.
    delete_session_meta(&conv_dir, "s3").unwrap();
    Ok(())
}

/// The store must be reachable *before* `install_session_meta_backend`:
/// `find_latest_session` runs during session_init, ahead of the memory backend.
/// With nothing registered, the first read opens the workspace's
/// `memory/private.sqlite` itself.
#[test]
fn on_demand_open_reads_the_workspace_store_before_install() -> Result<(), AcoworkError> {
    let ws = tempfile::tempdir().unwrap();
    let db_path = ws.path().join("memory").join("private.sqlite");

    // Populate the file through a store that is never registered, then drop
    // it — the process's first session-meta touch looks exactly like this: a
    // populated file, no backend installed.
    {
        let store = Arc::new(SqliteStore::open(&db_path, DIM)?);
        let sm = SqliteSessionMetaStore::new(store);
        sm.upsert(&make_meta("old1", "2025-01-01T00:00:00.000Z", "old1"))?;
        sm.upsert(&make_meta("old2", "2025-03-01T00:00:00.000Z", "old2"))?;
    }
    assert!(db_path.exists());

    let conv_dir = ws.path().join("conversations");
    assert_eq!(
        find_latest_session(&conv_dir).as_deref(),
        Some("old2"),
        "the on-demand open must read the workspace store"
    );
    assert_eq!(scan_sessions_from_meta(&conv_dir).len(), 2);
    assert!(session_meta_exists(&conv_dir, "old1"));
    Ok(())
}
