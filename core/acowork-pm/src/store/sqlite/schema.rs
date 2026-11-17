//! SQLite schema for the PM authority (ADR-117 D6 step 3).
//!
//! The layout follows the ADR-082 pattern that already works in
//! `acowork-memory`: `props` holds the verbatim JSON serialization of the
//! domain struct, while a handful of projected columns carry the fields that
//! filters, ordering and dispatch scans need. Keeping the JSON verbatim means
//! adding a field to `Task` never requires a schema migration.
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
/// WAL keeps readers (e.g. an operator inspecting the live DB) from blocking
/// the writer. `foreign_keys` is a per-connection pragma and must be set on
/// every open, not just at creation time.
///
/// The version gate runs **before** any DDL. A database this binary cannot
/// understand must be left byte-for-byte untouched: converging `IF NOT EXISTS`
/// tables or stamping `user_version` on it would destroy the evidence an
/// operator needs to decide whether to restore, migrate or rebuild it.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path).map_err(db_err)?;

    // Read-only gate first: `PRAGMA journal_mode = WAL` and any DDL are
    // writes, so a database we are going to refuse must be inspected *before*
    // either runs. Otherwise a version-skewed authority would be modified on
    // the very startup that rejects it.
    let current = user_version(&conn)?;
    if current == 0 {
        // `user_version = 0` means "fresh" *only* when there is no schema at
        // all. An unversioned DB from an older build already has these tables
        // and must not be silently adopted as v1 — its column layout may be
        // different in ways `IF NOT EXISTS` cannot detect.
        if has_pm_tables(&conn)? {
            return Err(PmError::Internal(format!(
                "pm.sqlite has PM tables but user_version=0 (unversioned legacy database); \
                 this binary understands v{SCHEMA_VERSION} only — refusing to adopt it silently"
            )));
        }
    } else if current != SCHEMA_VERSION {
        return Err(PmError::Internal(format!(
            "pm.sqlite schema v{current} is not understood by this binary (v{SCHEMA_VERSION}); \
             refusing to open a database we cannot migrate"
        )));
    }

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;\nPRAGMA synchronous = NORMAL;\nPRAGMA foreign_keys = ON;",
    )
    .map_err(db_err)?;
    conn.execute_batch(SCHEMA_SQL).map_err(db_err)?;
    if current == 0 {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(db_err)?;
    }
    Ok(conn)
}

/// Whether any PM-owned table already exists (regardless of `user_version`).
fn has_pm_tables(conn: &Connection) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('meta', 'projects', 'tasks', 'attachments', 'activity')",
            [],
            |row| row.get(0),
        )
        .map_err(db_err)?;
    Ok(n > 0)
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
///
/// `integrity_check` returns **one row per problem** plus a final `ok` row;
/// `query_row` would read only the first and mis-report a multi-error DB.
pub fn verify(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA integrity_check").map_err(db_err)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    drop(stmt);
    let problems: Vec<&str> = rows
        .iter()
        .map(String::as_str)
        .filter(|r| *r != "ok")
        .collect();
    if !problems.is_empty() {
        let shown = problems
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join("; ");
        return Err(PmError::Internal(format!(
            "pm.sqlite integrity_check failed with {} problem(s): {shown}{}",
            problems.len(),
            if problems.len() > 5 {
                " (truncated)"
            } else {
                ""
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::OpenFlags;

    fn read_only(path: &Path) -> Connection {
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    /// A refused version-skewed DB must be left completely untouched — not
    /// even `journal_mode` is allowed to change on the startup that rejects
    /// it, because in `db` mode that file is the authority.
    #[test]
    fn open_refuses_a_version_skewed_db_without_converging_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pm.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = open(&path).unwrap_err().to_string();
        assert!(err.contains("not understood"), "error was: {err}");

        let conn = read_only(&path);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION + 1);
        assert!(
            table_names(&conn).is_empty(),
            "a refused open must not add tables"
        );
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_ne!(mode.to_ascii_lowercase(), "wal", "journal mode was changed");
    }

    /// `user_version = 0` with PM tables already present is an unversioned
    /// legacy schema, not a fresh database. Stamping v1 onto it would let
    /// `IF NOT EXISTS` hide a column-layout mismatch.
    #[test]
    fn open_refuses_an_unversioned_db_that_already_has_pm_tables() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("pm.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE projects (id TEXT PRIMARY KEY);")
                .unwrap();
        }
        let err = open(&path).unwrap_err().to_string();
        assert!(err.contains("user_version=0"), "error was: {err}");

        let conn = read_only(&path);
        assert_eq!(user_version(&conn).unwrap(), 0);
        assert_eq!(table_names(&conn), vec!["projects".to_string()]);
    }

    #[test]
    fn fresh_db_is_fully_created_and_passes_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/pm.sqlite");
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(table_names(&conn).len(), 5);
        verify(&conn).unwrap();
        assert!(is_empty(&conn).unwrap());
        // Opening an already-versioned DB is idempotent.
        drop(conn);
        let conn = open(&path).unwrap();
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
        verify(&conn).unwrap();
    }
}
