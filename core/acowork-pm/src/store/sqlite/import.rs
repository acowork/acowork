//! Read-only tree scan and one-shot JSON → SQLite import (ADR-117 D6 step 3).
//!
//! The contract in ADR-117 §3 D6 is implemented here, in order:
//! 1. `dry_run` scans without creating or writing anything and returns a
//!    [`MigrationReport`]; any blocking finding aborts a real import.
//! 2. IDs are re-`parse()`d explicitly. `define_id!` uses
//!    `#[serde(transparent)]`, so a JSON field named `id` is trusted by
//!    serde but is **not** validated — the importer must not trust it.
//!    Directory name, `task.id`, `task.project_id` and the project's own id
//!    must all agree.
//! 3. Physical `children/` nesting is inverted into `parent_id` / `depth`,
//!    with a `max_task_depth` check.
//! 4. Sidecar ownership is explicit: `activity.json` and attachment metadata
//!    are business entities and are imported; `.bak`, `.corrupt/`, `.trash/`,
//!    `icons/`, `*.tmp` and attachment **binaries** are never imported.
//! 5. `AttachmentMeta.storage_path` is relative to the task directory, so the
//!    row stores `task_dir` (relative to `data_dir`) alongside the metadata.
//! 6. Bound parameters, one all-or-nothing transaction, a non-empty-target
//!    idempotency gate, `integrity_check` + `foreign_key_check` before commit,
//!    and a `user_version` stamp.
//! 7. Nothing is deleted from the tree: rollback is a config flip back to
//!    `filetree`, so the source stays a usable read-only archive.
//!
//! The scan is synchronous on purpose: it is a one-shot startup / operator
//! action, not a request path. Keeping it free of `await` makes "dry-run
//! writes nothing" trivially auditable.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;
use serde_json::Value;

use crate::config::PmConfig;
use crate::error::{PmError, Result};
use crate::types::{AttachmentMeta, Project, ProjectId, Task, TaskActivity, TaskId};

use super::db_err;
use super::schema;

/// Everything a dry run found. `blocking` is empty iff a real import may
/// proceed; `skipped` records recoverable omissions that do not block.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MigrationReport {
    pub source_dir: String,
    pub projects: usize,
    pub tasks: usize,
    pub attachments: usize,
    pub activity_rows: usize,
    pub skipped: Vec<SkippedEntry>,
    /// Fields filled by `#[serde(default)]` rather than present in the JSON.
    /// Recorded explicitly so "legitimate default" and "semantic damage"
    /// stay distinguishable during the migration review.
    pub defaults_taken: Vec<String>,
    pub blocking: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkippedEntry {
    pub path: String,
    pub reason: String,
}

impl MigrationReport {
    pub fn has_blocking_issues(&self) -> bool {
        !self.blocking.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "projects={} tasks={} attachments={} activity={} skipped={} defaults={} blocking={}",
            self.projects,
            self.tasks,
            self.attachments,
            self.activity_rows,
            self.skipped.len(),
            self.defaults_taken.len(),
            self.blocking.len()
        )
    }
}

// ── In-memory batch: one row per entity, ready for bound INSERTs ──────────

struct ProjectRow {
    id: String,
    status: String,
    created_at: String,
    updated_at: String,
    props: String,
}

struct TaskRow {
    id: String,
    project_id: String,
    parent_id: Option<String>,
    depth: u8,
    task_dir: String,
    status: String,
    review_status: String,
    priority: String,
    task_type: String,
    title: String,
    assignee: Option<String>,
    start_at: Option<String>,
    due_at: Option<String>,
    dispatched_at: Option<String>,
    created_at: String,
    updated_at: String,
    props: String,
}

struct AttachmentRow {
    id: String,
    task_id: String,
    task_dir: String,
    props: String,
}

struct ActivityRow {
    task_id: String,
    seq: i64,
    props: String,
}

#[derive(Default)]
struct ImportBatch {
    projects: Vec<ProjectRow>,
    tasks: Vec<TaskRow>,
    attachments: Vec<AttachmentRow>,
    activity: Vec<ActivityRow>,
}

/// Read-only scan. Never creates the database file.
pub fn dry_run(config: &PmConfig) -> Result<MigrationReport> {
    let (report, _) = scan(config)?;
    Ok(report)
}

/// Insert a scanned batch into an open transaction. Shared by the one-shot
/// import and the `dual` full rebuild, so the two can never disagree on how a
/// tree entity becomes a row.
fn insert_batch(tx: &Connection, batch: &ImportBatch, source_dir: &str) -> Result<()> {
    for p in &batch.projects {
        tx.execute(
            "INSERT INTO projects (id, status, created_at, updated_at, props)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![p.id, p.status, p.created_at, p.updated_at, p.props],
        )
        .map_err(db_err)?;
    }
    // Parents must exist before children: foreign keys are immediate.
    let mut tasks: Vec<&TaskRow> = batch.tasks.iter().collect();
    tasks.sort_by_key(|t| t.depth);
    for t in tasks {
        tx.execute(
            "INSERT INTO tasks (id, project_id, parent_id, depth, task_dir, status,
                                review_status, priority, task_type, title, assignee,
                                start_at, due_at, dispatched_at, created_at, updated_at, props)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                t.id,
                t.project_id,
                t.parent_id,
                t.depth,
                t.task_dir,
                t.status,
                t.review_status,
                t.priority,
                t.task_type,
                t.title,
                t.assignee,
                t.start_at,
                t.due_at,
                t.dispatched_at,
                t.created_at,
                t.updated_at,
                t.props
            ],
        )
        .map_err(db_err)?;
    }
    for a in &batch.attachments {
        tx.execute(
            "INSERT INTO attachments (id, task_id, task_dir, props) VALUES (?1, ?2, ?3, ?4)",
            params![a.id, a.task_id, a.task_dir, a.props],
        )
        .map_err(db_err)?;
    }
    for a in &batch.activity {
        tx.execute(
            "INSERT INTO activity (task_id, seq, props) VALUES (?1, ?2, ?3)",
            params![a.task_id, a.seq, a.props],
        )
        .map_err(db_err)?;
    }
    let stamp = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('imported_at', ?1)",
        params![stamp],
    )
    .map_err(db_err)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('source_dir', ?1)",
        params![source_dir],
    )
    .map_err(db_err)?;
    Ok(())
}

fn blocking_error(report: &MigrationReport) -> PmError {
    PmError::Internal(format!(
        "PM import blocked ({} finding(s)); run dry_run first: {}",
        report.blocking.len(),
        report.blocking.join(" | ")
    ))
}

/// Scan, then import in one transaction. Refuses to run on a non-empty DB or
/// on any blocking finding; the caller reviews [`dry_run`] first.
pub fn import(config: &PmConfig) -> Result<MigrationReport> {
    let (report, batch) = scan(config)?;
    if report.has_blocking_issues() {
        return Err(blocking_error(&report));
    }

    let mut conn = schema::open(&config.sqlite_path())?;
    if !schema::is_empty(&conn)? {
        return Err(PmError::Internal(
            "pm.sqlite already holds projects — import is one-shot; delete the DB to re-import"
                .into(),
        ));
    }

    let tx = conn.transaction().map_err(db_err)?;
    insert_batch(&tx, &batch, &report.source_dir)?;
    schema::verify(&tx)?;
    tx.commit().map_err(db_err)?;
    Ok(report)
}

/// Replace the whole DB with a fresh scan of the tree in one transaction.
///
/// This is the `dual` backend's projection primitive and the ADR-117 D6
/// "DB is disposable, rebuild from the tree" guarantee. Unlike [`import`] it
/// is idempotent and never gated on emptiness: an empty / stale / corrupt
/// projection is simply overwritten. `dual` calls this after every tree
/// commit and once at startup; a failure is best-effort and must never be
/// propagated into the write result.
pub fn rebuild_into(conn: &mut Connection, config: &PmConfig) -> Result<MigrationReport> {
    assert_dual_projection(config)?;
    // A projection is disposable: mirror exactly what the tree can serve.
    // `import` keeps the stricter Blocking policy because it creates a new
    // authority, where silently dropping an entity is permanent data loss.
    let (report, batch) = scan_with_policy(config, CorruptPolicy::SkipAndRecord)?;
    if report.has_blocking_issues() {
        return Err(blocking_error(&report));
    }
    let tx = conn.transaction().map_err(db_err)?;
    // Clear self-references first so cascading deletes cannot leave a dangling
    // `parent_id` behind, then remove every entity table explicitly.
    tx.execute_batch(
        "UPDATE tasks SET parent_id = NULL;
         DELETE FROM activity;
         DELETE FROM attachments;
         DELETE FROM tasks;
         DELETE FROM projects;
         DELETE FROM meta;",
    )
    .map_err(db_err)?;
    insert_batch(&tx, &batch, &report.source_dir)?;
    schema::verify(&tx)?;
    tx.commit().map_err(db_err)?;
    Ok(report)
}

/// Convenience wrapper for operator / startup callers that do not already
/// hold a connection.
pub fn rebuild(config: &PmConfig) -> Result<MigrationReport> {
    let mut conn = schema::open(&config.sqlite_path())?;
    rebuild_into(&mut conn, config)
}

// ── Scan ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum CorruptPolicy {
    /// One-shot migration into a new authority: dropping an unreadable
    /// entity would turn a recoverable tree defect into permanent data loss.
    Blocking,
    /// Disposable `dual` projection: `TreePmStore` already skips unreadable
    /// files, so the mirror must skip them too. One broken tree file must not
    /// stall every subsequent projection for the rest of the process life.
    SkipAndRecord,
}

fn corrupt_entity(
    report: &mut MigrationReport,
    path: &Path,
    reason: String,
    policy: CorruptPolicy,
) {
    match policy {
        CorruptPolicy::Blocking => block(report, path, reason),
        CorruptPolicy::SkipAndRecord => report.skipped.push(SkippedEntry {
            path: path.display().to_string(),
            reason: format!("{reason} (tree skips this file too; projection mirrors it)"),
        }),
    }
}

/// A tree scan may overwrite `pm.sqlite` only while the tree is authoritative.
/// In `db` mode SQLite is the sole commit point, and a stale tree scan would
/// silently resurrect an older snapshot (ADR-117 §3 D6 iron rule 4).
fn assert_dual_projection(config: &PmConfig) -> Result<()> {
    if config.storage_backend != crate::config::StorageBackend::Dual {
        return Err(PmError::Internal(
            "a tree scan must never overwrite pm.sqlite unless storage_backend=dual \
             (in db mode SQLite is the authority)"
                .into(),
        ));
    }
    Ok(())
}

fn scan(config: &PmConfig) -> Result<(MigrationReport, ImportBatch)> {
    scan_with_policy(config, CorruptPolicy::Blocking)
}

fn scan_with_policy(
    config: &PmConfig,
    policy: CorruptPolicy,
) -> Result<(MigrationReport, ImportBatch)> {
    let mut report = MigrationReport {
        source_dir: config.data_dir.display().to_string(),
        ..Default::default()
    };
    let mut batch = ImportBatch::default();
    let projects_dir = config.projects_dir();
    if !projects_dir.exists() {
        return Ok((report, batch));
    }

    // `filter_map(|e| e.ok())` used to swallow an unreadable directory entry.
    // For a one-shot authority migration that can silently omit a whole
    // project, so I/O errors now propagate instead of being dropped.
    let mut project_dirs: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&projects_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            project_dirs.push(entry.path());
        }
    }
    project_dirs.sort();

    for dir in project_dirs {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if !name.starts_with("p-") {
            report.skipped.push(SkippedEntry {
                path: dir.display().to_string(),
                reason: "not a p-* project directory (sidecar)".into(),
            });
            continue;
        }
        // Looks like a project but fails the id grammar: block rather than
        // treating it as a sidecar, otherwise it silently disappears from the
        // new authority while dry-run still reports zero blocking findings.
        let Ok(pid) = name.parse::<ProjectId>() else {
            block(
                &mut report,
                &dir,
                format!(
                    "project directory `{name}` looks like a project but fails ProjectId::parse"
                ),
            );
            continue;
        };
        let project_json = dir.join("project.json");
        if !project_json.exists() {
            // A db-mode asset-only directory can land here. Only a directory
            // that actually holds tasks is a damaged project.
            let has_tasks = std::fs::read_dir(dir.join("tasks"))
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);
            if has_tasks {
                block(
                    &mut report,
                    &dir,
                    "project directory has tasks/ but no project.json".into(),
                );
            } else {
                report.skipped.push(SkippedEntry {
                    path: dir.display().to_string(),
                    reason: "project dir without project.json (asset-only or unfinished)".into(),
                });
            }
            continue;
        }
        let project: Project = match read_entity(&project_json) {
            Ok(p) => p,
            Err(e) => {
                corrupt_entity(
                    &mut report,
                    &project_json,
                    format!("unreadable project.json: {e}"),
                    policy,
                );
                continue;
            }
        };
        // Contract 2: serde never validates a transparent id, so parse it.
        if project.id.as_str() != name || project.id.as_str().parse::<ProjectId>().is_err() {
            block(
                &mut report,
                &project_json,
                format!(
                    "project id `{}` does not match directory `{name}` / fails parse",
                    project.id
                ),
            );
            continue;
        }
        record_defaults(
            &mut report,
            &project_json,
            &["description", "owner_id", "metadata", "members"],
        )?;

        batch.projects.push(ProjectRow {
            id: pid.to_string(),
            status: enum_text(&project.status)?,
            created_at: project.created_at.to_rfc3339(),
            updated_at: project.updated_at.to_rfc3339(),
            props: serde_json::to_string(&project)?,
        });
        report.projects += 1;

        scan_tasks(config, &dir, &pid, &mut report, &mut batch, policy)?;
    }
    Ok((report, batch))
}

/// Iterative DFS over `tasks/` and nested `children/` directories.
fn scan_tasks(
    config: &PmConfig,
    project_dir: &Path,
    pid: &ProjectId,
    report: &mut MigrationReport,
    batch: &mut ImportBatch,
    policy: CorruptPolicy,
) -> Result<()> {
    let tasks_root = project_dir.join("tasks");
    if !tasks_root.exists() {
        return Ok(());
    }
    // (directory, parent task id, depth)
    let mut stack: Vec<(PathBuf, Option<TaskId>, u8)> = vec![(tasks_root, None, 0)];
    while let Some((dir, parent_id, depth)) = stack.pop() {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                entries.push(entry.path());
            }
        }
        entries.sort();
        for task_dir in entries {
            let name = task_dir
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string();
            if !name.starts_with("t-") {
                // `attachments/`, `icons/`, `.bak`, `.corrupt/` … never data.
                report.skipped.push(SkippedEntry {
                    path: task_dir.display().to_string(),
                    reason: "sidecar directory, not a task".into(),
                });
                continue;
            }
            let Ok(tid) = name.parse::<TaskId>() else {
                block(
                    report,
                    &task_dir,
                    format!("task directory `{name}` fails TaskId::parse"),
                );
                continue;
            };
            if depth > config.max_task_depth {
                block(
                    report,
                    &task_dir,
                    format!(
                        "depth {depth} exceeds max_task_depth {}",
                        config.max_task_depth
                    ),
                );
                continue;
            }
            let json_path = task_dir.join("task.json");
            let task: Task = match read_entity(&json_path) {
                Ok(t) => t,
                Err(e) => {
                    corrupt_entity(
                        report,
                        &json_path,
                        format!("unreadable task.json: {e}"),
                        policy,
                    );
                    continue;
                }
            };
            if task.id.as_str() != name || task.id.as_str().parse::<TaskId>().is_err() {
                block(
                    report,
                    &json_path,
                    format!("task id `{}` does not match directory `{name}`", task.id),
                );
                continue;
            }
            if task.project_id.as_str() != pid.as_str() {
                block(
                    report,
                    &json_path,
                    format!(
                        "task.project_id `{}` does not match project `{pid}`",
                        task.project_id
                    ),
                );
                continue;
            }
            record_defaults(
                report,
                &json_path,
                &[
                    "description",
                    "review_status",
                    "priority",
                    "assignee",
                    "start_at",
                    "attachments",
                ],
            )?;

            // Nested ids are just as untrusted as the directory names:
            // `define_id!` deserializes any string without calling `parse()`.
            if task
                .depends_on
                .iter()
                .any(|d| d.task_id.as_str().parse::<TaskId>().is_err())
                || task.result.as_ref().is_some_and(|r| {
                    r.attachment_ids
                        .iter()
                        .any(|a| a.as_str().parse::<crate::types::AttachmentId>().is_err())
                })
            {
                block(
                    report,
                    &json_path,
                    "task contains an invalid nested id (depends_on / result.attachment_ids)"
                        .into(),
                );
                continue;
            }

            // Contract 5: the base path for relative attachment storage_path.
            let rel_dir = match task_dir.strip_prefix(&config.data_dir) {
                Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
                Err(_) => {
                    block(
                        report,
                        &task_dir,
                        format!(
                            "task dir {} is not under data_dir {}",
                            task_dir.display(),
                            config.data_dir.display()
                        ),
                    );
                    continue;
                }
            };

            for att in &task.attachments {
                collect_attachment(report, batch, &task_dir, &tid, &rel_dir, att)?;
            }
            collect_activity(report, batch, &task_dir, &tid, &task, policy)?;

            batch.tasks.push(TaskRow {
                id: tid.to_string(),
                project_id: pid.to_string(),
                parent_id: parent_id.as_ref().map(|p| p.to_string()),
                depth,
                task_dir: rel_dir.clone(),
                status: enum_text(&task.status)?,
                review_status: enum_text(&task.review_status)?,
                priority: enum_text(&task.priority)?,
                task_type: enum_text(&task.task_type)?,
                title: task.title.clone(),
                assignee: task.assignee.clone(),
                start_at: task.start_at.map(|d| d.to_rfc3339()),
                due_at: task.due_at.map(|d| d.to_rfc3339()),
                dispatched_at: task.dispatched_at.map(|d| d.to_rfc3339()),
                created_at: task.created_at.to_rfc3339(),
                updated_at: task.updated_at.to_rfc3339(),
                props: serde_json::to_string(&task)?,
            });
            report.tasks += 1;

            let children = task_dir.join("children");
            if children.exists() {
                stack.push((children, Some(tid.clone()), depth + 1));
            }
        }
    }
    Ok(())
}

/// A relative path made only of normal components. `contains("..")` alone is
/// not enough: `PathBuf::join` replaces the base entirely when the argument
/// is absolute (`/etc/hosts`, `C:\...`), so a leading root must be rejected
/// too. The same check is applied again at the DB read point.
pub(crate) fn safe_rel(path: &str) -> bool {
    let p = Path::new(path);
    !path.is_empty()
        && !p.is_absolute()
        && p.components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

fn collect_attachment(
    report: &mut MigrationReport,
    batch: &mut ImportBatch,
    task_dir: &Path,
    tid: &TaskId,
    rel_dir: &str,
    att: &AttachmentMeta,
) -> Result<()> {
    for (field, value) in [
        ("storage_path", Some(att.storage_path.as_str())),
        ("thumb_path", att.thumb_path.as_deref()),
    ] {
        if let Some(value) = value
            && !safe_rel(value)
        {
            block(
                report,
                &task_dir.join("task.json"),
                format!(
                    "attachment `{}` {field} escapes the task dir: `{value}`",
                    att.id
                ),
            );
            return Ok(());
        }
    }
    if att
        .id
        .as_str()
        .parse::<crate::types::AttachmentId>()
        .is_err()
    {
        block(
            report,
            &task_dir.join("task.json"),
            format!("attachment id `{}` fails parse", att.id),
        );
        return Ok(());
    }
    batch.attachments.push(AttachmentRow {
        id: att.id.to_string(),
        task_id: tid.to_string(),
        task_dir: rel_dir.to_string(),
        props: serde_json::to_string(att)?,
    });
    report.attachments += 1;
    Ok(())
}

fn collect_activity(
    report: &mut MigrationReport,
    batch: &mut ImportBatch,
    task_dir: &Path,
    tid: &TaskId,
    task: &Task,
    policy: CorruptPolicy,
) -> Result<()> {
    let path = task_dir.join("activity.json");
    // `TreePmStore::load_activity` seeds a Submitted row from `task.result`
    // when a pre-history task has no `activity.json` on disk. The importer
    // must mirror that *business read path* exactly: otherwise those tasks
    // lose their history after a `db` cutover, while `reconcile` compares
    // 0 rows against 0 rows and reports a false green light.
    let rows: Vec<TaskActivity> = if !path.exists() {
        crate::store::tree::seeded_activity(task)
    } else {
        match read_entity(&path) {
            Ok(v) => v,
            Err(e) => {
                corrupt_entity(
                    report,
                    &path,
                    format!("unreadable activity.json: {e}"),
                    policy,
                );
                return Ok(());
            }
        }
    };
    for (seq, row) in rows.iter().enumerate() {
        // `#[serde(transparent)]` trusts every id inside JSON, so re-parse
        // the nested ones as well as the directory / entity ids.
        if row.id.as_str().parse::<crate::types::ActivityId>().is_err()
            || row
                .attachment_ids
                .iter()
                .any(|a| a.as_str().parse::<crate::types::AttachmentId>().is_err())
        {
            block(
                report,
                &path,
                format!("activity row {seq} contains an invalid nested id"),
            );
            return Ok(());
        }
        batch.activity.push(ActivityRow {
            task_id: tid.to_string(),
            seq: seq as i64,
            props: serde_json::to_string(row)?,
        });
        report.activity_rows += 1;
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn read_entity<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = std::fs::read(path)?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Record which `#[serde(default)]` keys were absent from the source JSON.
fn record_defaults(report: &mut MigrationReport, path: &Path, keys: &[&str]) -> Result<()> {
    let bytes = std::fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes)?;
    if let Value::Object(map) = &value {
        for key in keys {
            if !map.contains_key(*key) {
                report
                    .defaults_taken
                    .push(format!("{}#{key}", path.display()));
            }
        }
    }
    Ok(())
}

fn enum_text<T: Serialize>(value: &T) -> Result<String> {
    match serde_json::to_value(value)? {
        Value::String(s) => Ok(s),
        other => Ok(other.to_string()),
    }
}

fn block(report: &mut MigrationReport, path: &Path, reason: String) {
    report.skipped.push(SkippedEntry {
        path: path.display().to_string(),
        reason: reason.clone(),
    });
    report
        .blocking
        .push(format!("{}: {reason}", path.display()));
}

// ── Offline reconciliation (ADR-117 D6 step 6) ──────────────────────────

/// Result of comparing the authoritative tree against a `dual` projection.
/// `props` is compared verbatim, so a field-level drift (not just a missing
/// row) is visible without decoding either side.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ReconcileReport {
    pub tree_projects: usize,
    pub db_projects: usize,
    pub tree_tasks: usize,
    pub db_tasks: usize,
    pub tree_attachments: usize,
    pub db_attachments: usize,
    pub tree_activity: usize,
    pub db_activity: usize,
    /// Entity keys present in the tree but absent from the DB.
    pub missing_in_db: Vec<String>,
    /// Entity keys present in the DB but no longer in the tree (a stale row).
    pub extra_in_db: Vec<String>,
    /// Entity keys present in both whose `props` JSON differs.
    pub props_mismatch: Vec<String>,
    /// Entity keys whose projected columns differ. `db` mode reads
    /// `parent_id` / `depth` / `status` / ... instead of decoding `props`,
    /// so a columns-only drift would otherwise pass the cutover gate unseen.
    pub column_mismatch: Vec<String>,
    pub blocking: Vec<String>,
}

impl ReconcileReport {
    /// True only when every entity, every `props` blob and every projected
    /// column agrees.
    pub fn is_consistent(&self) -> bool {
        self.missing_in_db.is_empty()
            && self.extra_in_db.is_empty()
            && self.props_mismatch.is_empty()
            && self.column_mismatch.is_empty()
            && self.blocking.is_empty()
            && self.tree_projects == self.db_projects
            && self.tree_tasks == self.db_tasks
            && self.tree_attachments == self.db_attachments
            && self.tree_activity == self.db_activity
    }

    pub fn summary(&self) -> String {
        format!(
            "consistent={} projects={}/{} tasks={}/{} attachments={}/{} activity={}/{} missing={} extra={} mismatch={} col_mismatch={}",
            self.is_consistent(),
            self.tree_projects,
            self.db_projects,
            self.tree_tasks,
            self.db_tasks,
            self.tree_attachments,
            self.db_attachments,
            self.tree_activity,
            self.db_activity,
            self.missing_in_db.len(),
            self.extra_in_db.len(),
            self.props_mismatch.len(),
            self.column_mismatch.len()
        )
    }
}

#[derive(Debug, Clone, Default)]
struct SnapshotValue {
    props: String,
    /// Projected columns that the `db` backend reads instead of `props`.
    columns: String,
}

/// Both sides must build this string from the same field order, otherwise a
/// purely cosmetic difference would show up as a permanent red light.
#[allow(clippy::too_many_arguments)]
fn task_columns(
    project_id: &str,
    parent_id: Option<&str>,
    depth: i64,
    task_dir: &str,
    status: &str,
    review_status: &str,
    priority: &str,
    task_type: &str,
    title: &str,
    assignee: Option<&str>,
    start_at: Option<&str>,
    due_at: Option<&str>,
    dispatched_at: Option<&str>,
    created_at: &str,
    updated_at: &str,
) -> String {
    format!(
        "{project_id}|{}|{depth}|{task_dir}|{status}|{review_status}|{priority}|{task_type}|{title}|{}|{}|{}|{}|{created_at}|{updated_at}",
        parent_id.unwrap_or(""),
        assignee.unwrap_or(""),
        start_at.unwrap_or(""),
        due_at.unwrap_or(""),
        dispatched_at.unwrap_or(""),
    )
}

fn tree_snapshot(batch: &ImportBatch) -> HashMap<String, SnapshotValue> {
    let mut out = HashMap::new();
    for p in &batch.projects {
        out.insert(
            format!("project:{}", p.id),
            SnapshotValue {
                props: p.props.clone(),
                columns: format!("{}|{}|{}", p.status, p.created_at, p.updated_at),
            },
        );
    }
    for t in &batch.tasks {
        out.insert(
            format!("task:{}", t.id),
            SnapshotValue {
                props: t.props.clone(),
                columns: task_columns(
                    &t.project_id,
                    t.parent_id.as_deref(),
                    t.depth as i64,
                    &t.task_dir,
                    &t.status,
                    &t.review_status,
                    &t.priority,
                    &t.task_type,
                    &t.title,
                    t.assignee.as_deref(),
                    t.start_at.as_deref(),
                    t.due_at.as_deref(),
                    t.dispatched_at.as_deref(),
                    &t.created_at,
                    &t.updated_at,
                ),
            },
        );
    }
    for a in &batch.attachments {
        out.insert(
            format!("attachment:{}", a.id),
            SnapshotValue {
                props: a.props.clone(),
                columns: format!("{}|{}", a.task_id, a.task_dir),
            },
        );
    }
    for a in &batch.activity {
        out.insert(
            format!("activity:{}:{}", a.task_id, a.seq),
            SnapshotValue {
                props: a.props.clone(),
                columns: format!("{}|{}", a.task_id, a.seq),
            },
        );
    }
    out
}

fn db_snapshot(conn: &Connection) -> Result<HashMap<String, SnapshotValue>> {
    let mut out = HashMap::new();
    {
        let mut stmt = conn
            .prepare("SELECT id, props, status, created_at, updated_at FROM projects")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    format!(
                        "{}|{}|{}",
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?
                    ),
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        for (id, props, columns) in rows {
            out.insert(format!("project:{id}"), SnapshotValue { props, columns });
        }
    }
    {
        let mut stmt = conn
            .prepare(
                "SELECT id, props, project_id, parent_id, depth, task_dir, status,
                        review_status, priority, task_type, title, assignee,
                        start_at, due_at, dispatched_at, created_at, updated_at
                 FROM tasks",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                let parent_id: Option<String> = row.get(3)?;
                let assignee: Option<String> = row.get(11)?;
                let start_at: Option<String> = row.get(12)?;
                let due_at: Option<String> = row.get(13)?;
                let dispatched_at: Option<String> = row.get(14)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    task_columns(
                        &row.get::<_, String>(2)?,
                        parent_id.as_deref(),
                        row.get::<_, i64>(4)?,
                        &row.get::<_, String>(5)?,
                        &row.get::<_, String>(6)?,
                        &row.get::<_, String>(7)?,
                        &row.get::<_, String>(8)?,
                        &row.get::<_, String>(9)?,
                        &row.get::<_, String>(10)?,
                        assignee.as_deref(),
                        start_at.as_deref(),
                        due_at.as_deref(),
                        dispatched_at.as_deref(),
                        &row.get::<_, String>(15)?,
                        &row.get::<_, String>(16)?,
                    ),
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        for (id, props, columns) in rows {
            out.insert(format!("task:{id}"), SnapshotValue { props, columns });
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT id, props, task_id, task_dir FROM attachments")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    format!("{}|{}", row.get::<_, String>(2)?, row.get::<_, String>(3)?),
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        for (id, props, columns) in rows {
            out.insert(format!("attachment:{id}"), SnapshotValue { props, columns });
        }
    }
    {
        let mut stmt = conn
            .prepare("SELECT task_id, seq, props FROM activity")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(db_err)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(db_err)?;
        for (task_id, seq, props) in rows {
            out.insert(
                format!("activity:{task_id}:{seq}"),
                SnapshotValue {
                    props,
                    columns: format!("{task_id}|{seq}"),
                },
            );
        }
    }
    Ok(out)
}

/// Read-only comparison of the tree and the DB projection. Never writes —
/// this is the operator's gate before switching `dual` → `db` (ADR-117 D6
/// step 6). Counts come from the live tables, not from the report, so a
/// duplicated row is caught even when the key set happens to match.
///
/// Deliberately does **not** call [`schema::open`]: that helper creates the
/// directory, applies DDL and bumps `user_version`, which would silently
/// conjure an empty `pm.sqlite` on a tree-only install. A gate that can
/// create its own evidence is not a gate.
pub fn reconcile(config: &PmConfig) -> Result<ReconcileReport> {
    let (report, batch) = scan(config)?;
    let mut out = ReconcileReport {
        tree_projects: report.projects,
        tree_tasks: report.tasks,
        tree_attachments: report.attachments,
        tree_activity: report.activity_rows,
        blocking: report.blocking,
        ..Default::default()
    };
    let path = config.sqlite_path();
    if !path.is_file() {
        out.blocking.push(format!(
            "no projection to reconcile: {} does not exist",
            path.display()
        ));
        return Ok(out);
    }
    let conn = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(db_err)?;
    let version = schema::user_version(&conn)?;
    if version != schema::SCHEMA_VERSION {
        out.blocking.push(format!(
            "pm.sqlite schema v{version} does not match this binary (v{})",
            schema::SCHEMA_VERSION
        ));
        return Ok(out);
    }
    if let Err(e) = schema::verify(&conn) {
        out.blocking.push(e.to_string());
        return Ok(out);
    }
    let count = |sql: &str| -> Result<usize> {
        let n: i64 = conn.query_row(sql, [], |row| row.get(0)).map_err(db_err)?;
        Ok(n as usize)
    };
    out.db_projects = count("SELECT COUNT(*) FROM projects")?;
    out.db_tasks = count("SELECT COUNT(*) FROM tasks")?;
    out.db_attachments = count("SELECT COUNT(*) FROM attachments")?;
    out.db_activity = count("SELECT COUNT(*) FROM activity")?;

    let tree = tree_snapshot(&batch);
    let db = db_snapshot(&conn)?;
    let tree_keys: BTreeSet<String> = tree.keys().cloned().collect();
    let db_keys: BTreeSet<String> = db.keys().cloned().collect();
    out.missing_in_db = tree_keys.difference(&db_keys).cloned().collect();
    out.extra_in_db = db_keys.difference(&tree_keys).cloned().collect();
    out.props_mismatch = tree_keys
        .intersection(&db_keys)
        .filter(|k| tree[k.as_str()].props != db[k.as_str()].props)
        .cloned()
        .collect();
    out.column_mismatch = tree_keys
        .intersection(&db_keys)
        .filter(|k| tree[k.as_str()].columns != db[k.as_str()].columns)
        .cloned()
        .collect();
    Ok(out)
}

/// Retire the JSON tree after a successful import (ADR-117 step ⑧, automated).
///
/// Two kinds of binary are still referenced by rows that came from the tree:
/// project icons under `projects/{pid}/icon/` and task attachment binaries
/// under `projects/{pid}/tasks/{tid}/attachments/`. Both are copied into
/// DB-owned `assets/`, the `task_dir` columns are rewritten, and `projects/`
/// is renamed so no runtime path can read the archive again. Nothing is ever
/// deleted — the renamed directory stays as a recoverable snapshot.
///
/// Idempotent and resumable: a process that committed the import but died
/// before finishing this runs it again on the next start.
pub fn retire_tree_archive(config: &PmConfig) -> Result<()> {
    let projects = config.projects_dir();
    if !projects.is_dir() {
        return Ok(());
    }
    let retired = config.data_dir.join("projects.retired");
    if retired.exists() {
        return Err(PmError::Internal(format!(
            "cannot retire {}: {} already exists",
            projects.display(),
            retired.display()
        )));
    }

    // The icon API namespace (`projects/{pid}/icon/...` in metadata.icon.path)
    // is a logical URL prefix, not a filesystem path: `read_project_icon()`
    // looks in `assets/projects/{pid}/icon` first, so no metadata rewrite is
    // needed and already-rendered front-end URLs keep working.
    copy_project_icons(config, &projects)?;

    let mut conn = schema::open(&config.sqlite_path())?;
    let tx = conn.transaction().map_err(db_err)?;
    relocate_task_assets(config, &tx)?;
    tx.commit().map_err(db_err)?;

    let remaining: i64 = conn
        .query_row(
            "SELECT (SELECT COUNT(*) FROM tasks WHERE task_dir LIKE 'projects/%') \
                  + (SELECT COUNT(*) FROM attachments WHERE task_dir LIKE 'projects/%')",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if remaining != 0 {
        return Err(PmError::Internal(format!(
            "{remaining} row(s) still point into projects/; archive left in place"
        )));
    }
    drop(conn);

    std::fs::rename(&projects, &retired)?;
    tracing::info!(
        path = %retired.display(),
        "JSON tree retired; pm.sqlite + assets/ are now the only data source"
    );
    Ok(())
}

fn copy_project_icons(config: &PmConfig, projects: &Path) -> Result<()> {
    for entry in std::fs::read_dir(projects)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let pid = entry.file_name().to_string_lossy().into_owned();
        let Ok(files) = std::fs::read_dir(entry.path().join("icon")) else {
            continue;
        };
        for file in files.flatten() {
            let name = file.file_name().to_string_lossy().into_owned();
            if !name.starts_with("icon-") {
                continue;
            }
            let dst_dir = config
                .data_dir
                .join("assets/projects")
                .join(&pid)
                .join("icon");
            std::fs::create_dir_all(&dst_dir)?;
            let dst = dst_dir.join(&name);
            if !dst.exists() {
                std::fs::copy(file.path(), &dst)?;
            }
        }
    }
    Ok(())
}

fn relocate_task_assets(config: &PmConfig, tx: &Connection) -> Result<()> {
    let mut stmt = tx
        .prepare("SELECT id, task_dir FROM tasks WHERE task_dir LIKE 'projects/%'")
        .map_err(db_err)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    drop(stmt);

    for (tid, rel_dir) in rows {
        // `storage_path` is relative to `task_dir`, so moving the whole
        // `attachments/` subtree and rewriting that one column is enough.
        let src = config.data_dir.join(&rel_dir).join("attachments");
        let new_rel = format!("assets/tasks/{tid}");
        if src.is_dir() {
            copy_dir_all(&src, &config.data_dir.join(&new_rel).join("attachments"))?;
        }
        tx.execute(
            "UPDATE tasks SET task_dir = ?1 WHERE id = ?2",
            params![new_rel, tid],
        )
        .map_err(db_err)?;
        tx.execute(
            "UPDATE attachments SET task_dir = ?1 WHERE task_id = ?2",
            params![new_rel, tid],
        )
        .map_err(db_err)?;
    }
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else if !to.exists() {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::SqlitePmStore;
    use crate::store::tree::{PmMaintenance, PmStore as _, TreePmStore};
    use crate::types::{
        AttachmentId, AttachmentKind, AttachmentMeta, CreateProject, CreateTask, Dependency,
        MemberKind, Priority, TaskType,
    };

    fn task_input(title: &str, parent: Option<TaskId>) -> CreateTask {
        CreateTask {
            title: title.to_string(),
            description: String::new(),
            task_type: TaskType::Task,
            priority: Priority::Normal,
            parent_task_id: parent,
            depends_on: Vec::<Dependency>::new(),
            attachment_ids: Vec::new(),
            assignee: None,
            due_at: None,
            start_at: None,
        }
    }

    /// Seed a real tree through `TreePmStore` so the fixture exercises the
    /// exact on-disk layout the importer must understand (including nested
    /// `children/` and per-task `attachments/` sidecars).
    async fn seed_tree(dir: &Path) -> (PmConfig, ProjectId, TaskId, TaskId) {
        let config = PmConfig {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        };
        let store = TreePmStore::new(config.clone()).await.unwrap();
        let project = store
            .create_project_as(
                CreateProject {
                    title: "migration fixture".into(),
                    description: String::new(),
                    metadata: Default::default(),
                },
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let root = store
            .create_task_as(
                &project.id,
                task_input("root", None),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let child = store
            .create_task_as(
                &project.id,
                task_input("child", Some(root.id.clone())),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        (config, project.id, root.id, child.id)
    }

    #[tokio::test]
    async fn dry_run_is_read_only_and_reports_the_tree_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, _, _, _) = seed_tree(tmp.path()).await;

        let report = dry_run(&config).unwrap();
        assert!(!report.has_blocking_issues(), "{}", report.summary());
        assert_eq!(report.projects, 1);
        assert_eq!(report.tasks, 2);
        assert_eq!(report.attachments, 0);
        // A dry run must not even create the database file.
        assert!(!config.sqlite_path().exists());
    }

    /// Retiring the archive must leave `pm.sqlite` + `assets/` as the only
    /// data source: icons and attachment binaries are physically relocated,
    /// `task_dir` columns are rewritten, and `projects/` is renamed (not
    /// deleted). Re-running on the next start is a no-op.
    #[tokio::test]
    async fn retire_tree_archive_relocates_binaries_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, pid, root_id, _) = seed_tree(tmp.path()).await;
        let store = TreePmStore::new(config.clone()).await.unwrap();
        // `TreePmStore::new` deliberately does not walk the tree; the caller
        // decides when to rebuild. Without this the lookup below sees no task.
        store.rebuild_index().await.unwrap();
        let att_id = AttachmentId::generate();
        let meta = AttachmentMeta {
            id: att_id.clone(),
            filename: "notes.txt".into(),
            kind: AttachmentKind::File,
            content_type: "text/plain".into(),
            size: 5,
            sha256: "deadbeef".into(),
            storage_path: format!("attachments/{}/original.txt", att_id.as_str()),
            thumb_path: None,
            width: None,
            height: None,
            uploaded_by: "human".into(),
            uploaded_at: Utc::now(),
        };
        store
            .write_attachment(&root_id, meta, b"hello".to_vec(), None)
            .await
            .unwrap();

        let icon_dir = config.projects_dir().join(pid.as_str()).join("icon");
        std::fs::create_dir_all(&icon_dir).unwrap();
        std::fs::write(icon_dir.join("icon-01.png"), b"png").unwrap();

        let report = import(&config).unwrap();
        assert_eq!(report.attachments, 1);
        retire_tree_archive(&config).unwrap();

        assert!(!config.projects_dir().is_dir(), "projects/ must be retired");
        let retired = config.data_dir.join("projects.retired");
        assert!(retired.is_dir(), "the archive must stay recoverable");
        assert_eq!(
            std::fs::read(retired.join(pid.as_str()).join("icon/icon-01.png")).unwrap(),
            b"png".to_vec()
        );
        assert_eq!(
            std::fs::read(
                config
                    .data_dir
                    .join("assets/projects")
                    .join(pid.as_str())
                    .join("icon/icon-01.png")
            )
            .unwrap(),
            b"png".to_vec()
        );
        assert_eq!(
            std::fs::read(
                config
                    .data_dir
                    .join("assets/tasks")
                    .join(root_id.as_str())
                    .join("attachments")
                    .join(att_id.as_str())
                    .join("original.txt")
            )
            .unwrap(),
            b"hello".to_vec()
        );

        let conn = schema::open(&config.sqlite_path()).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM tasks WHERE task_dir LIKE 'projects/%') \
                      + (SELECT COUNT(*) FROM attachments WHERE task_dir LIKE 'projects/%')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "no row may still point into the retired tree");
        drop(conn);

        // A crash between import and retirement is recoverable on next start.
        retire_tree_archive(&config).unwrap();
    }

    #[tokio::test]
    async fn import_round_trips_entities_parenting_and_is_one_shot() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, pid, root_id, child_id) = seed_tree(tmp.path()).await;
        // Sidecars that must never become entities.
        let root_dir = config
            .projects_dir()
            .join(pid.as_str())
            .join("tasks")
            .join(root_id.as_str());
        std::fs::write(root_dir.join("task.json.bak"), b"{\"stale\":true}").unwrap();
        std::fs::create_dir_all(root_dir.join(".corrupt")).unwrap();

        let report = import(&config).unwrap();
        assert!(!report.has_blocking_issues(), "{}", report.summary());
        assert_eq!(report.projects, 1);
        assert_eq!(report.tasks, 2);

        let db = SqlitePmStore::open(config.clone()).unwrap();
        assert!(db.get_project(&pid).await.unwrap().is_some());
        assert_eq!(db.get_task(&root_id).await.unwrap().unwrap().title, "root");
        assert_eq!(
            db.get_task(&child_id).await.unwrap().unwrap().title,
            "child"
        );
        // Contract 3: physical nesting became parent_id + depth.
        assert_eq!(db.task_depth(&root_id), 0);
        assert_eq!(db.task_depth(&child_id), 1);
        assert_eq!(db.parent_of(&child_id).as_ref(), Some(&root_id));
        assert_eq!(db.project_task_count(&pid), 2);
        assert_eq!(db.project_counts().await.unwrap()[0].total, 2);
        // Contract 4: the .bak sidecar never became a row.
        assert!(db.list_attachments(&root_id).await.unwrap().is_empty());

        // Contract 6: a second import is refused instead of merging histories.
        let err = import(&config).unwrap_err().to_string();
        assert!(err.contains("already holds projects"), "error was: {err}");
    }

    #[tokio::test]
    async fn new_attachments_on_imported_tasks_never_touch_the_tree_archive() {
        use crate::types::{AttachmentId, AttachmentKind, AttachmentMeta};

        let tmp = tempfile::tempdir().unwrap();
        let (config, pid, root_id, _) = seed_tree(tmp.path()).await;
        import(&config).unwrap();
        let archive_dir = config
            .projects_dir()
            .join(pid.as_str())
            .join("tasks")
            .join(root_id.as_str());
        let before: Vec<String> = std::fs::read_dir(&archive_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();

        let db = SqlitePmStore::open(config.clone()).unwrap();
        let id = AttachmentId::generate();
        let meta = AttachmentMeta {
            id: id.clone(),
            filename: "fresh.txt".into(),
            kind: AttachmentKind::File,
            content_type: "text/plain".into(),
            size: 5,
            sha256: "cafebabe".into(),
            storage_path: format!("attachments/{}/original.txt", id.as_str()),
            thumb_path: None,
            width: None,
            height: None,
            uploaded_by: "human".into(),
            uploaded_at: Utc::now(),
        };
        db.write_attachment(&root_id, meta, b"fresh".to_vec(), None)
            .await
            .unwrap();
        let (_, bytes) = db.read_attachment_bytes(&id, false).await.unwrap().unwrap();
        assert_eq!(bytes, b"fresh");

        let after: Vec<String> = std::fs::read_dir(&archive_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(before, after, "the tree archive must stay read-only");
        assert!(config.data_dir.join("assets/tasks").exists());
    }

    #[tokio::test]
    async fn blocking_findings_abort_before_any_db_write() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, pid, root_id, _) = seed_tree(tmp.path()).await;
        let task_json = config
            .projects_dir()
            .join(pid.as_str())
            .join("tasks")
            .join(root_id.as_str())
            .join("task.json");
        // `#[serde(transparent)]` trusts the JSON id — the importer must not.
        let mut value: Value =
            serde_json::from_str(&std::fs::read_to_string(&task_json).unwrap()).unwrap();
        value["id"] = Value::String("t-forged".into());
        std::fs::write(&task_json, serde_json::to_vec(&value).unwrap()).unwrap();

        let report = dry_run(&config).unwrap();
        assert!(report.has_blocking_issues(), "{}", report.summary());
        assert!(report.blocking.iter().any(|b| b.contains("does not match")));
        let err = import(&config).unwrap_err().to_string();
        assert!(err.contains("PM import blocked"), "error was: {err}");
        assert!(!config.sqlite_path().exists());
    }

    #[tokio::test]
    async fn an_unreadable_task_json_blocks_instead_of_silently_dropping_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, pid, root_id, _) = seed_tree(tmp.path()).await;
        let task_json = config
            .projects_dir()
            .join(pid.as_str())
            .join("tasks")
            .join(root_id.as_str())
            .join("task.json");
        std::fs::write(&task_json, b"{ not json").unwrap();

        let report = dry_run(&config).unwrap();
        assert!(report.has_blocking_issues(), "{}", report.summary());
        assert!(
            report
                .blocking
                .iter()
                .any(|b| b.contains("unreadable task.json"))
        );
        assert!(!config.sqlite_path().exists());
    }

    /// A cutover gate must not create the evidence it is supposed to inspect:
    /// `schema::open()` would conjure an empty `pm.sqlite` (plus DDL and WAL
    /// sidecars) on a tree-only install.
    #[tokio::test]
    async fn reconcile_never_creates_a_db_on_a_tree_only_install() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, _, _, _) = seed_tree(tmp.path()).await;
        assert!(!config.sqlite_path().exists());

        let report = reconcile(&config).unwrap();
        assert!(!report.is_consistent(), "{}", report.summary());
        assert!(
            report.blocking.iter().any(|b| b.contains("does not exist")),
            "{}",
            report.summary()
        );
        assert!(report.tree_tasks > 0);
        assert!(
            !config.sqlite_path().exists(),
            "reconcile must be strictly read-only"
        );
    }

    #[tokio::test]
    async fn reconcile_is_consistent_after_the_one_shot_import() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, _, _, _) = seed_tree(tmp.path()).await;
        import(&config).unwrap();
        let report = reconcile(&config).unwrap();
        assert!(report.is_consistent(), "{}", report.summary());
    }

    #[tokio::test]
    async fn reconcile_refuses_a_version_skewed_db() {
        let tmp = tempfile::tempdir().unwrap();
        let (config, _, _, _) = seed_tree(tmp.path()).await;
        import(&config).unwrap();
        {
            let conn = Connection::open(config.sqlite_path()).unwrap();
            conn.pragma_update(None, "user_version", schema::SCHEMA_VERSION + 1)
                .unwrap();
        }
        let report = reconcile(&config).unwrap();
        assert!(!report.is_consistent(), "{}", report.summary());
        assert!(
            report.blocking.iter().any(|b| b.contains("schema v")),
            "{}",
            report.summary()
        );
    }
}
