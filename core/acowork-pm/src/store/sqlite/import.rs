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

use std::path::{Path, PathBuf};

use chrono::Utc;
use rusqlite::params;
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

/// Scan, then import in one transaction. Refuses to run on a non-empty DB or
/// on any blocking finding; the caller reviews [`dry_run`] first.
pub fn import(config: &PmConfig) -> Result<MigrationReport> {
    let (report, batch) = scan(config)?;
    if report.has_blocking_issues() {
        return Err(PmError::Internal(format!(
            "PM import blocked ({} finding(s)); run dry_run first: {}",
            report.blocking.len(),
            report.blocking.join(" | ")
        )));
    }

    let mut conn = schema::open(&config.sqlite_path())?;
    if !schema::is_empty(&conn)? {
        return Err(PmError::Internal(
            "pm.sqlite already holds projects — import is one-shot; delete the DB to re-import"
                .into(),
        ));
    }

    let tx = conn.transaction().map_err(db_err)?;
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
        params![report.source_dir],
    )
    .map_err(db_err)?;

    schema::verify(&tx)?;
    tx.commit().map_err(db_err)?;
    Ok(report)
}

// ── Scan ──────────────────────────────────────────────────────────────────

fn scan(config: &PmConfig) -> Result<(MigrationReport, ImportBatch)> {
    let mut report = MigrationReport {
        source_dir: config.data_dir.display().to_string(),
        ..Default::default()
    };
    let mut batch = ImportBatch::default();
    let projects_dir = config.projects_dir();
    if !projects_dir.exists() {
        return Ok((report, batch));
    }

    let mut project_dirs: Vec<PathBuf> = std::fs::read_dir(&projects_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    project_dirs.sort();

    for dir in project_dirs {
        let name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        // Directory names that are not project ids are sidecars (`.trash/`,
        // `.corrupt/`, scratch dirs), not data. Never block on them.
        let Ok(pid) = name.parse::<ProjectId>() else {
            report.skipped.push(SkippedEntry {
                path: dir.display().to_string(),
                reason: "not a p-* project directory (sidecar)".into(),
            });
            continue;
        };
        let project_json = dir.join("project.json");
        let project: Project = match read_entity(&project_json) {
            Ok(p) => p,
            Err(e) => {
                block(
                    &mut report,
                    &project_json,
                    format!("unreadable project.json: {e}"),
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

        scan_tasks(config, &dir, &pid, &mut report, &mut batch)?;
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
) -> Result<()> {
    let tasks_root = project_dir.join("tasks");
    if !tasks_root.exists() {
        return Ok(());
    }
    // (directory, parent task id, depth)
    let mut stack: Vec<(PathBuf, Option<TaskId>, u8)> = vec![(tasks_root, None, 0)];
    while let Some((dir, parent_id, depth)) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .collect();
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
                    // A corrupt task.json is blocking for a migration: silently
                    // dropping one entity would turn a recoverable tree defect
                    // into permanent data loss in the new authority.
                    block(report, &json_path, format!("unreadable task.json: {e}"));
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

            // Contract 5: the base path for relative attachment storage_path.
            let rel_dir = task_dir
                .strip_prefix(&config.data_dir)
                .unwrap_or(&task_dir)
                .to_string_lossy()
                .replace('\\', "/");

            for att in &task.attachments {
                collect_attachment(report, batch, &task_dir, &tid, &rel_dir, att)?;
            }
            collect_activity(report, batch, &task_dir, &tid)?;

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

fn collect_attachment(
    report: &mut MigrationReport,
    batch: &mut ImportBatch,
    task_dir: &Path,
    tid: &TaskId,
    rel_dir: &str,
    att: &AttachmentMeta,
) -> Result<()> {
    if att.storage_path.contains("..") {
        block(
            report,
            &task_dir.join("task.json"),
            format!(
                "attachment `{}` storage_path escapes the task dir: `{}`",
                att.id, att.storage_path
            ),
        );
        return Ok(());
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
) -> Result<()> {
    let path = task_dir.join("activity.json");
    if !path.exists() {
        return Ok(());
    }
    let rows: Vec<TaskActivity> = match read_entity(&path) {
        Ok(v) => v,
        Err(e) => {
            block(report, &path, format!("unreadable activity.json: {e}"));
            return Ok(());
        }
    };
    for (seq, row) in rows.iter().enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::sqlite::SqlitePmStore;
    use crate::store::tree::{PmMaintenance, PmStore as _, TreePmStore};
    use crate::types::{CreateProject, CreateTask, Dependency, MemberKind, Priority, TaskType};

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
}
