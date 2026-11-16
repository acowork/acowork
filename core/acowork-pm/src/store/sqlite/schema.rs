//! SQLite schema for the PM `db` / `dual` backends (ADR-117 D6 step 3).
//!
//! The layout follows the ADR-082 pattern that already works in
//! `acowork-memory`: `props` holds the verbatim JSON serialization of the
//! domain struct, while a handful of projected columns carry the fields that
//! filters, ordering and dispatch scans need. Keeping the JSON verbatim means
//! adding a field to `Task` never requires a schema migration, and the dual
//! backend can reconcile a projection by comparing `props` byte-for-byte.
//!
//! Three deliberate departures from the tree model (ADR-061 inversion):
//! - physical `children/` nesting becomes `parent_id` + `depth`;
//! - `task_dir` stores the task directory **relative to `data_dir`**, because
//!   `AttachmentMeta.storage_path` is relative to that directory (ADR-117
//!   §3 D6 contract 5);
//! - attachment binaries stay on the filesystem; only their metadata rows move.
//!
//! The DDL is idempotent (`IF NOT EXISTS` throughout), but opening an existing
//! database never downgrades or silently migrates its `user_version` — a newer
//! schema than this binary understands is an explicit error.

use std::path::Path;

use rusqlite::Connection;

use crate::error::{PmError, Result};

use super::db_err;

/// Bump when adding a migration step. Until migrations exist, any version
/// other than 0 (fresh) or this one is refused.
pub const SCHEMA_VERSION: u32 = 1;

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS projects (
    id         TEXT PRIMARY KEY,
    status     TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    props      JSON NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
    id             TEXT PRIMARY KEY,
    project_id     TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    parent_id      TEXT REFERENCES tasks(id) ON DELETE SET NULL,
    depth          INTEGER NOT NULL DEFAULT 0,
    -- Relative to `data_dir`; the base for AttachmentMeta.storage_path.
    task_dir       TEXT NOT NULL,
    status         TEXT NOT NULL,
    review_status  TEXT NOT NULL,
    priority       TEXT NOT NULL,
    task_type      TEXT NOT NULL,
    title          TEXT NOT NULL,
    assignee       TEXT,
    start_at       TEXT,
    due_at         TEXT,
    dispatched_at  TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    props          JSON NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project_id);
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);
CREATE INDEX IF NOT EXISTS idx_tasks_start_at ON tasks(start_at);

CREATE TABLE IF NOT EXISTS attachments (
    id       TEXT PRIMARY KEY,
    task_id  TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    task_dir TEXT NOT NULL,
    props    JSON NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_attachments_task ON attachments(task_id);

CREATE TABLE IF NOT EXISTS activity (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    seq     INTEGER NOT NULL,
    props   JSON NOT NULL,
    PRIMARY KEY (task_id, seq)
);
"#;

/// Open (or create) the PM database and converge the schema.
///
/// WAL is enabled so a read-only reconciliation pass during a `dual` rollout
/// does not block the writer. `foreign_keys` is a per-connection pragma and
/// must be set on every open, not just at creation time.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).map_err(db_err)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA foreign_keys = ON;",
    )
    .map_err(db_err)?;
    conn.execute_batch(SCHEMA_SQL).map_err(db_err)?;

    let current = user_version(&conn)?;
    if current == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err)?;
    } else if current != SCHEMA_VERSION {
        return Err(PmError::Internal(format!(
            "pm.sqlite schema v{current} is not understood by this binary (v{SCHEMA_VERSION}); \
             refusing to open a database we cannot migrate"
        )));
    }
    Ok(conn)
}

pub fn user_version(conn: &Connection) -> Result<u32> {
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(db_err)
}

/// Idempotency gate: a database that already holds projects is not an import
/// target. Merging two histories is a different, much harder problem.
pub fn is_empty(conn: &Connection) -> Result<bool> {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM projects", [], |row| row.get(0))
        .map_err(db_err)?;
    Ok(n == 0)
}

/// `PRAGMA integrity_check` + `PRAGMA foreign_key_check`, run inside the
/// import transaction before commit so a half-imported database is impossible.
pub fn verify(conn: &Connection) -> Result<()> {
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(db_err)?;
    if result != "ok" {
        return Err(PmError::Internal(format!(
            "pm.sqlite integrity_check failed: {result}"
        )));
    }
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").map_err(db_err)?;
    let violations = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    if !violations.is_empty() {
        return Err(PmError::Internal(format!(
            "pm.sqlite foreign_key_check failed for {} row(s), first table: {}",
            violations.len(),
            violations[0]
        )));
    }
    Ok(())
}
