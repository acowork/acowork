//! SQLite backend for PM (ADR-117): the sole storage authority.
//!
//! `SqlitePmStore` owns a `rusqlite::Connection` behind a `std::sync::Mutex`:
//! PM has a single writer and every statement is sub-millisecond, so a short
//! synchronous critical section is simpler and cheaper than an async SQL
//! stack. The lock is never held across an `await`, so a `#[async_trait]`
//! method cannot stall the executor on I/O it does not actually perform.
//!
//! The schema stores each entity's verbatim JSON in `props` plus projected
//! columns for filtering. Business mutations therefore load → mutate → save a
//! `Task` / `Project`, reusing the shared rule helpers in `store::rules` so
//! validation and state-machine logic have one definition. Attachment binaries
//! and project icons stay on the filesystem (ADR-061 decision 5);
//! `tasks.task_dir` holds the base path those relative asset paths are
//! resolved against.

mod business;
pub mod schema;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::config::PmConfig;
use crate::error::{PmError, Result};
use crate::types::{DependencyKind, Project, ProjectId, ReviewStatus, Task, TaskId, TaskStatus};

use crate::store::contract::PmMaintenance;
use crate::store::rules::{parse_task_id, priority_rank};

/// Convert a `rusqlite` failure into the PM error envelope without adding a
/// new `PmError` variant (which would ripple through `http_status` and
/// `error_code` for no client-visible benefit).
pub(super) fn db_err(err: rusqlite::Error) -> PmError {
    PmError::Internal(format!("sqlite: {err}"))
}

/// Canonical asset root for a DB-owned task:
/// `assets/projects/{pid}/tasks/{tid}`.
///
/// Icons already live at `assets/projects/{pid}/icon/`. Grouping task binaries
/// under the same `{pid}` keeps one project's entire binary footprint in a
/// single subtree, so deleting a project can clean it in one `remove_dir_all`
/// and a project scan can find every asset without touching the DB.
///
/// This is safe because a task's `project_id` never changes: both
/// `create_task_as` and `reparent_task` reject cross-project parents, so no
/// physical directory ever has to move between project subtrees.
pub(super) fn task_asset_rel(project_id: &str, task_id: &str) -> String {
    format!("assets/projects/{project_id}/tasks/{task_id}")
}

/// Only DB-owned asset roots may be physically deleted. The strict shape check
/// (`assets/projects/p-*/tasks/t-*`) keeps the sibling `icon/` directory and
/// any non-canonical path out of the deletable set, so a malformed `task_dir`
/// can never turn a delete into an arbitrary `remove_dir_all`.
pub(super) fn is_db_owned_asset_dir(rel_dir: &str) -> bool {
    let parts: Vec<&str> = rel_dir.split('/').collect();
    parts.len() == 5
        && parts[0] == "assets"
        && parts[1] == "projects"
        && parts[2].starts_with("p-")
        && parts[3] == "tasks"
        && parts[4].starts_with("t-")
}

pub struct SqlitePmStore {
    pub(super) config: PmConfig,
    conn: Arc<Mutex<Connection>>,
    /// Serializes the read-check-write of `claim_task` so the assignee
    /// compare-and-set stays atomic across concurrent callers (G1-b / A5).
    claim_lock: tokio::sync::Mutex<()>,
}

impl SqlitePmStore {
    /// Open (or create) `pm.sqlite` and verify it before serving anything.
    pub fn open(config: PmConfig) -> Result<Self> {
        let conn = schema::open(&config.sqlite_path())?;
        schema::verify(&conn)?;
        Ok(Self {
            config,
            conn: Arc::new(Mutex::new(conn)),
            claim_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn config(&self) -> &PmConfig {
        &self.config
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|e| PmError::Internal(format!("sqlite connection lock poisoned: {e}")))
    }

    fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.lock()?;
        f(&guard)
    }

    /// Transactions need `&mut Connection`. Kept separate from `with_conn` so
    /// the common read path cannot accidentally take a write-capable handle.
    fn with_conn_mut<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut guard = self.lock()?;
        f(&mut guard)
    }

    // ── Entity primitives (used by `business`) ──────────────────────

    pub(super) fn load_task(&self, id: &TaskId) -> Result<Option<Task>> {
        self.with_conn(|c| load_task_from(c, id))
    }

    pub(super) fn load_project(&self, id: &ProjectId) -> Result<Option<Project>> {
        self.with_conn(|c| load_project_from(c, id))
    }

    /// Persist a task and keep every projected column in lockstep with `props`.
    /// A zero-row update means the caller worked on a task that is gone.
    pub(super) fn save_task(&self, task: &Task) -> Result<()> {
        self.with_conn(|conn| update_task_row(conn, task))
    }

    pub(super) fn save_project(&self, project: &Project) -> Result<()> {
        self.with_conn(|conn| {
            let affected = conn
                .execute(
                    "UPDATE projects SET status = ?2, updated_at = ?3, props = ?4 WHERE id = ?1",
                    params![
                        project.id.as_str(),
                        enum_text(&project.status)?,
                        project.updated_at.to_rfc3339(),
                        serde_json::to_string(project)?,
                    ],
                )
                .map_err(db_err)?;
            if affected == 0 {
                return Err(PmError::ProjectNotFound(project.id.to_string()));
            }
            Ok(())
        })
    }

    pub(super) fn insert_project(&self, project: &Project) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT INTO projects (id, status, created_at, updated_at, props)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    project.id.as_str(),
                    enum_text(&project.status)?,
                    project.created_at.to_rfc3339(),
                    project.updated_at.to_rfc3339(),
                    serde_json::to_string(project)?,
                ],
            )
            .map_err(db_err)?;
            Ok(())
        })
    }

    pub(super) fn insert_task(
        &self,
        task: &Task,
        parent_id: Option<&TaskId>,
        depth: u8,
        task_dir: &str,
    ) -> Result<()> {
        self.with_conn(|conn| insert_task_row(conn, task, parent_id, depth, task_dir))
    }

    pub(super) fn all_tasks(&self, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<Task>> {
        self.with_conn(|conn| {
            let mut stmt = conn.prepare(sql).map_err(db_err)?;
            let rows = stmt
                .query_map(args, |row| row.get::<_, String>(0))
                .map_err(db_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)?;
            rows.iter()
                .map(|json| Ok(serde_json::from_str(json)?))
                .collect()
        })
    }

    /// All tasks, verbatim. Used by filters whose predicates live in Rust so
    /// the tree and DB backends answer them with exactly the same rules.
    pub(super) fn every_task(&self) -> Result<Vec<Task>> {
        self.all_tasks("SELECT props FROM tasks", &[])
    }

    /// Tasks paired with the `parent_id` column. `find_tasks` needs the
    /// parent as a derived read; doing it in one query avoids a lookup per row.
    pub(super) fn every_task_with_parent(&self) -> Result<Vec<(Task, Option<TaskId>)>> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT props, parent_id FROM tasks")
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
                })
                .map_err(db_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)?;
            rows.iter()
                .map(|(json, parent)| {
                    Ok((
                        serde_json::from_str(json)?,
                        parent.as_deref().and_then(|p| parse_task_id(p).ok()),
                    ))
                })
                .collect()
        })
    }

    pub(super) fn task_meta(&self, id: &TaskId) -> Result<Option<(Option<TaskId>, u8, String)>> {
        self.with_conn(|conn| {
            let row: Option<(Option<String>, i64, String)> = conn
                .query_row(
                    "SELECT parent_id, depth, task_dir FROM tasks WHERE id = ?1",
                    params![id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(db_err)?;
            Ok(row.map(|(parent, depth, dir)| {
                (
                    parent.and_then(|p| parse_task_id(&p).ok()),
                    depth as u8,
                    dir,
                )
            }))
        })
    }

    /// Stable asset root for a task, grouped under the owning project so icons
    /// and task binaries share one subtree.
    pub(super) fn new_task_asset_dir(&self, project_id: &ProjectId, id: &TaskId) -> String {
        task_asset_rel(project_id.as_str(), id.as_str())
    }

    /// Row ids of `id` plus its whole descendant subtree, parents first.
    /// `visited` guards against a corrupted `parent_id` cycle: without it a
    /// single bad row would hang delete / reparent / dispatch indefinitely.
    pub(super) fn subtree_ids(&self, id: &TaskId) -> Result<Vec<TaskId>> {
        self.with_conn(|conn| {
            let mut out = Vec::new();
            let mut visited = std::collections::HashSet::new();
            let mut frontier = vec![id.as_str().to_string()];
            while let Some(current) = frontier.pop() {
                if !visited.insert(current.clone()) {
                    continue;
                }
                if let Ok(tid) = parse_task_id(&current) {
                    out.push(tid);
                }
                let mut stmt = conn
                    .prepare("SELECT id FROM tasks WHERE parent_id = ?1")
                    .map_err(db_err)?;
                let children = stmt
                    .query_map(params![current], |row| row.get::<_, String>(0))
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                frontier.extend(children);
            }
            Ok(out)
        })
    }

    /// Every DB-owned asset root referenced by these tasks, from **both**
    /// `tasks.task_dir` and `attachments.task_dir`; either table may carry the
    /// only reference to a directory, so reading just one would leave orphaned
    /// binaries behind after a subtree delete.
    pub(super) fn db_owned_asset_dirs(&self, ids: &[TaskId]) -> Result<Vec<String>> {
        self.with_conn(|conn| {
            let mut out = std::collections::BTreeSet::new();
            let mut stmt = conn
                .prepare(
                    "SELECT task_dir FROM tasks WHERE id = ?1
                     UNION SELECT task_dir FROM attachments WHERE task_id = ?1",
                )
                .map_err(db_err)?;
            for id in ids {
                let rows = stmt
                    .query_map(params![id.as_str()], |row| row.get::<_, String>(0))
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                for rel in rows {
                    if is_db_owned_asset_dir(&rel) {
                        out.insert(rel);
                    }
                }
            }
            Ok(out.into_iter().collect())
        })
    }

    pub(super) fn delete_task_rows(&self, ids: &[TaskId]) -> Result<()> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            for id in ids {
                tx.execute("DELETE FROM tasks WHERE id = ?1", params![id.as_str()])
                    .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    pub(super) fn delete_project_row(&self, id: &ProjectId) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute("DELETE FROM projects WHERE id = ?1", params![id.as_str()])
                .map_err(db_err)?;
            Ok(())
        })
    }

    /// Icon root: `assets/projects/{pid}/icon/` — the one physical location
    /// for a project's icons.
    fn icon_dir(&self, project_id: &ProjectId) -> PathBuf {
        self.config
            .data_dir
            .join("assets/projects")
            .join(project_id.as_str())
            .join("icon")
    }
}

fn load_task_from(conn: &Connection, id: &TaskId) -> Result<Option<Task>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT props FROM tasks WHERE id = ?1",
            params![id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?;
    json.map(|j| Ok(serde_json::from_str(&j)?)).transpose()
}

fn load_project_from(conn: &Connection, id: &ProjectId) -> Result<Option<Project>> {
    let json: Option<String> = conn
        .query_row(
            "SELECT props FROM projects WHERE id = ?1",
            params![id.as_str()],
            |row| row.get(0),
        )
        .optional()
        .map_err(db_err)?;
    json.map(|j| Ok(serde_json::from_str(&j)?)).transpose()
}

/// Works on both `&Connection` and `&Transaction` (which derefs to it), so
/// activity commits can update the record and its history in one transaction.
fn update_task_row(conn: &Connection, task: &Task) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE tasks SET status = ?2, review_status = ?3, priority = ?4,
                              task_type = ?5, title = ?6, assignee = ?7,
                              start_at = ?8, due_at = ?9, dispatched_at = ?10,
                              updated_at = ?11, props = ?12
             WHERE id = ?1",
            params![
                task.id.as_str(),
                enum_text(&task.status)?,
                enum_text(&task.review_status)?,
                enum_text(&task.priority)?,
                enum_text(&task.task_type)?,
                task.title,
                task.assignee,
                task.start_at.map(|d| d.to_rfc3339()),
                task.due_at.map(|d| d.to_rfc3339()),
                task.dispatched_at.map(|d| d.to_rfc3339()),
                task.updated_at.to_rfc3339(),
                serde_json::to_string(task)?,
            ],
        )
        .map_err(db_err)?;
    if affected == 0 {
        return Err(PmError::TaskNotFound(task.id.to_string()));
    }
    Ok(())
}

fn insert_task_row(
    conn: &Connection,
    task: &Task,
    parent_id: Option<&TaskId>,
    depth: u8,
    task_dir: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO tasks (id, project_id, parent_id, depth, task_dir, status,
                            review_status, priority, task_type, title, assignee,
                            start_at, due_at, dispatched_at, created_at, updated_at, props)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            task.id.as_str(),
            task.project_id.as_str(),
            parent_id.map(|p| p.as_str()),
            depth,
            task_dir,
            enum_text(&task.status)?,
            enum_text(&task.review_status)?,
            enum_text(&task.priority)?,
            enum_text(&task.task_type)?,
            task.title,
            task.assignee,
            task.start_at.map(|d| d.to_rfc3339()),
            task.due_at.map(|d| d.to_rfc3339()),
            task.dispatched_at.map(|d| d.to_rfc3339()),
            task.created_at.to_rfc3339(),
            task.updated_at.to_rfc3339(),
            serde_json::to_string(task)?,
        ],
    )
    .map_err(db_err)?;
    Ok(())
}

fn enum_text<T: serde::Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        serde_json::Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

fn icon_content_type(name: &str) -> Option<&'static str> {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("svg") => Some("image/svg+xml"),
        _ => None,
    }
}

/// Dependency gate over an already-loaded task. A dangling blocker blocks,
/// Done / Cancelled satisfies the dependency-block rule.
/// Takes the connection explicitly so the dispatch scan can stay inside one
/// critical section instead of re-entering the connection `Mutex`.
fn dispatch_blocked(conn: &Connection, task: &Task) -> Result<bool> {
    for dep in task
        .depends_on
        .iter()
        .filter(|d| d.kind == DependencyKind::Blocks)
    {
        let blocker = load_task_from(conn, &dep.task_id)?;
        if !matches!(
            blocker.map(|b| b.status),
            Some(TaskStatus::Done) | Some(TaskStatus::Cancelled)
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[async_trait]
impl PmMaintenance for SqlitePmStore {
    /// Backfill `start_at` on legacy pending rows, updating `props` and the
    /// projected column together. Idempotent by construction.
    async fn migrate_null_start_at(&self) -> Result<usize> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let mut stmt = tx
                .prepare(
                    "SELECT id, props FROM tasks WHERE status = 'pending' AND start_at IS NULL",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(db_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)?;
            drop(stmt);
            let now = Utc::now();
            for (id, json) in &rows {
                let mut task: Task = serde_json::from_str(json)?;
                task.start_at = Some(now);
                tx.execute(
                    "UPDATE tasks SET start_at = ?2, props = ?3 WHERE id = ?1",
                    params![id, now.to_rfc3339(), serde_json::to_string(&task)?],
                )
                .map_err(db_err)?;
            }
            tx.commit().map_err(db_err)?;
            Ok(rows.len())
        })
    }

    async fn shutdown(&self) -> Result<()> {
        self.with_conn(|conn| {
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .map_err(db_err)?;
            Ok(())
        })
    }

    /// Same candidate rules as the tree scanner, answered from projected
    /// columns + the verbatim `props`. Everything happens inside one lock
    /// acquisition: calling `self.is_dispatch_blocked()` here would re-enter
    /// the non-reentrant `Mutex` and deadlock.
    fn dispatch_due_ids(&self, now: DateTime<Utc>, limit: usize) -> Vec<TaskId> {
        self.with_conn(|conn| {
            let mut due: Vec<Task> = {
                let mut stmt = conn
                    .prepare(
                        "SELECT props FROM tasks
                         WHERE status = 'pending' AND dispatched_at IS NULL
                           AND start_at IS NOT NULL AND start_at <= ?1
                           AND assignee IS NOT NULL AND assignee <> 'human'",
                    )
                    .map_err(db_err)?;
                let rows = stmt
                    .query_map(params![now.to_rfc3339()], |row| row.get::<_, String>(0))
                    .map_err(db_err)?
                    .collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(db_err)?;
                rows.iter()
                    .map(|json| Ok(serde_json::from_str(json)?))
                    .collect::<Result<Vec<_>>>()?
            };
            due.retain(|task| {
                task.review_status != ReviewStatus::Pending
                    && task.dispatch_attempts < crate::dispatch::MAX_DISPATCH_ATTEMPTS
                    && !dispatch_blocked(conn, task).unwrap_or(true)
            });
            due.sort_by_key(|t| {
                (
                    priority_rank(t.priority),
                    t.start_at.unwrap_or(DateTime::<Utc>::MIN_UTC),
                )
            });
            due.truncate(limit);
            Ok(due.into_iter().map(|t| t.id).collect())
        })
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "sqlite dispatch scan failed — empty tick");
            Vec::new()
        })
    }

    fn task_depth(&self, task_id: &TaskId) -> u8 {
        self.with_conn(|conn| {
            let depth: Option<u8> = conn
                .query_row(
                    "SELECT depth FROM tasks WHERE id = ?1",
                    params![task_id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            Ok(depth.unwrap_or(0))
        })
        .unwrap_or(0)
    }

    fn parent_of(&self, task_id: &TaskId) -> Option<TaskId> {
        self.task_meta(task_id)
            .ok()
            .flatten()
            .and_then(|(parent, _, _)| parent)
    }

    fn storage_diagnostics(&self) -> Value {
        let conn = match self.conn.lock() {
            Ok(guard) => guard,
            Err(e) => {
                return json!({ "error": format!("sqlite connection lock poisoned: {e}") });
            }
        };
        let schema_version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap_or(-1);
        let integrity: String = conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap_or_else(|_| "unavailable".to_string());

        let mut rows = serde_json::Map::new();
        let mut decode_failures = 0u64;
        for table in ["projects", "tasks", "attachments", "activity"] {
            let count: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or(-1);
            rows.insert(table.to_string(), json!(count));
            // A corrupt `props` blob is the SQLite equivalent of an
            // unparseable task.json: surface it instead of returning a
            // permanently-empty health counter.
            let mut stmt = match conn.prepare(&format!("SELECT props FROM {table}")) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let blobs = match stmt.query_map([], |r| r.get::<_, String>(0)) {
                Ok(b) => b,
                Err(_) => continue,
            };
            for blob in blobs {
                match blob {
                    Ok(s) if serde_json::from_str::<Value>(&s).is_ok() => {}
                    _ => decode_failures += 1,
                }
            }
        }
        json!({
            "schema_version": schema_version,
            "integrity": integrity,
            "rows": rows,
            "props_decode_failures": decode_failures,
        })
    }

    fn project_task_count(&self, project_id: &ProjectId) -> usize {
        self.with_conn(|conn| {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM tasks WHERE project_id = ?1",
                    params![project_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(db_err)?;
            Ok(n as usize)
        })
        .unwrap_or(0)
    }

    async fn list_project_icons(&self, project_id: &ProjectId) -> Result<Vec<String>> {
        let mut out = std::collections::BTreeSet::new();
        let entries = match std::fs::read_dir(self.icon_dir(project_id)) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("icon-") && icon_content_type(&name).is_some() {
                out.insert(name);
            }
        }
        Ok(out.into_iter().collect())
    }

    async fn write_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
        bytes: &[u8],
    ) -> Result<()> {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(PmError::PathTraversal(name.to_string()));
        }
        let dir = self.icon_dir(project_id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(name), bytes)?;
        Ok(())
    }

    async fn read_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err(PmError::PathTraversal(name.to_string()));
        }
        match std::fs::read(self.icon_dir(project_id).join(name)) {
            Ok(bytes) => Ok(icon_content_type(name).map(|ct| (ct.to_string(), bytes))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn is_dispatch_blocked(&self, task: &Task) -> bool {
        // Fail closed: an unreadable blocker blocks, exactly like a dangling
        // dependency reference.
        self.with_conn(|conn| dispatch_blocked(conn, task))
            .unwrap_or(true)
    }

    async fn dispatch_identity(&self, id: &TaskId) -> Result<Option<String>> {
        let Some(mut task) = self.load_task(id)? else {
            return Ok(None);
        };
        // A persisted generation identity is returned verbatim with NO write.
        // persisted generation identity is returned verbatim with NO write.
        // Re-saving it on every scanner tick would bump `updated_at` forever
        // for a task that has not changed.
        if let Some(existing) = task.dispatch_message_id.clone() {
            return Ok(Some(existing));
        }
        if task.status != TaskStatus::Pending || task.dispatched_at.is_some() {
            return Ok(None);
        }
        let minted = format!("pm-{}", uuid::Uuid::new_v4());
        task.dispatch_message_id = Some(minted.clone());
        task.updated_at = Utc::now();
        self.save_task(&task)?;
        Ok(Some(minted))
    }

    async fn mark_dispatched(&self, id: &TaskId) -> Result<()> {
        let Some(mut task) = self.load_task(id)? else {
            return Ok(());
        };
        if task.status != TaskStatus::Pending || task.dispatched_at.is_some() {
            return Ok(());
        }
        let now = Utc::now();
        task.dispatched_at = Some(now);
        task.updated_at = now;
        task.dispatch_attempts = 0;
        task.dispatch_error = None;
        self.save_task(&task)
    }

    async fn record_dispatch_failure(&self, id: &TaskId, reason: &str) -> Result<u32> {
        let Some(mut task) = self.load_task(id)? else {
            return Ok(0);
        };
        if task.status != TaskStatus::Pending || task.dispatched_at.is_some() {
            return Ok(task.dispatch_attempts);
        }
        task.dispatch_attempts = task.dispatch_attempts.saturating_add(1);
        task.dispatch_error = Some(reason.to_string());
        self.save_task(&task)?;
        Ok(task.dispatch_attempts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(dir: &Path) -> PmConfig {
        PmConfig {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn opening_a_fresh_db_is_idempotent_and_verified() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        {
            let store = SqlitePmStore::open(cfg.clone()).unwrap();
            assert_eq!(
                schema::user_version(&store.conn.lock().unwrap()).unwrap(),
                1
            );
            assert_eq!(
                store.storage_diagnostics()["rows"]["projects"],
                0,
                "a fresh database must be empty"
            );
            store.shutdown().await.unwrap();
        }
        // Reopening converges without downgrading or duplicating tables.
        let store = SqlitePmStore::open(cfg).unwrap();
        schema::verify(&store.conn.lock().unwrap()).unwrap();
        let diag = store.storage_diagnostics();
        assert_eq!(diag["props_decode_failures"], 0);
        assert_eq!(diag["integrity"], "ok");
    }

    #[tokio::test]
    async fn a_newer_schema_is_refused_instead_of_migrated_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        {
            let store = SqlitePmStore::open(cfg.clone()).unwrap();
            store
                .conn
                .lock()
                .unwrap()
                .pragma_update(None, "user_version", 99)
                .unwrap();
        }
        let err = match SqlitePmStore::open(cfg) {
            Ok(_) => panic!("a newer schema must be refused"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("not understood"), "error was: {err}");
    }

    /// Icons live in the DB-owned `assets/projects/{pid}/icon/` root; no
    /// legacy directory is ever created.
    #[tokio::test]
    async fn icons_live_in_the_project_asset_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let store = SqlitePmStore::open(cfg.clone()).unwrap();
        let pid = ProjectId("p-icons".into());

        store
            .write_project_icon(&pid, "icon-new.png", b"fresh")
            .await
            .unwrap();
        assert!(
            store.icon_dir(&pid).join("icon-new.png").exists(),
            "new icon must land in the DB-owned asset root"
        );
        assert!(
            !store.config.data_dir.join("projects").exists(),
            "no legacy JSON tree directory must be created"
        );

        let listed = store.list_project_icons(&pid).await.unwrap();
        assert_eq!(listed, vec!["icon-new.png"]);

        let (ct, bytes) = store
            .read_project_icon(&pid, "icon-new.png")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ct, "image/png");
        assert_eq!(bytes, b"fresh");

        // Files that are not icons are not listed, and a traversal name is
        // rejected on both write and read.
        std::fs::write(store.icon_dir(&pid).join("notes.txt"), b"x").unwrap();
        assert_eq!(store.list_project_icons(&pid).await.unwrap().len(), 1);
        assert!(
            store
                .read_project_icon(&pid, "../project.json")
                .await
                .is_err()
        );
        assert!(
            store
                .write_project_icon(&pid, "..\\escape.png", b"x")
                .await
                .is_err()
        );
    }
}
