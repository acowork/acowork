//! `PmStore` business methods for the SQLite backend (ADR-117 D6 step 3).
//!
//! Every mutation is load → mutate → save on the verbatim `props` JSON, with
//! the projected columns refreshed in the same statement. The validation and
//! state-machine helpers are imported from `store::tree` rather than copied:
//! `validate_transition`, `ensure_creation_approved`, `parse_task_id`,
//! `priority_rank` and `seeded_activity` are the single source of truth for
//! both backends, which is the whole point of running the same test suite
//! against them (D6 step 5).
//!
//! Two places where the DB is deliberately *not* a mechanical translation of
//! the tree:
//! - physical `children/` nesting is a `parent_id` + `depth` update, so
//!   `reparent_task` touches columns instead of renaming directories;
//! - DB-created tasks keep their binaries under
//!   `assets/projects/{pid}/tasks/{tid}`, outside the retired tree archive,
//!   so switching to `db` never writes into the recoverable snapshot. This
//!   shares the `{pid}` subtree with `icon/`, letting `delete_project` clean
//!   every binary of a project in one pass. Imported tasks keep their original
//!   `task_dir` until retirement rewrites it.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::Path;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::{PmError, Result};
use crate::types::{
    ActivityKind, AttachmentId, AttachmentMeta, CreateProject, CreateTask, DependencyKind,
    MemberKind, Project, ProjectId, ProjectMember, ProjectStatus, ReparentTask, ReviewStatus, Task,
    TaskActivity, TaskFilter, TaskId, TaskResult, TaskSort, TaskStatus, UpdateProject, UpdateTask,
};

use super::{SqlitePmStore, db_err, load_task_from, update_task_row};
use crate::store::tree::{
    PmStore, ensure_creation_approved, parse_task_id, priority_rank, seeded_activity,
    validate_transition,
};

// ── Activity helpers ────────────────────────────────────────────────

fn activity_rows(conn: &Connection, id: &TaskId) -> Result<Vec<TaskActivity>> {
    let mut stmt = conn
        .prepare("SELECT props FROM activity WHERE task_id = ?1 ORDER BY seq")
        .map_err(db_err)?;
    let rows = stmt
        .query_map(params![id.as_str()], |row| row.get::<_, String>(0))
        .map_err(db_err)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(db_err)?;
    rows.iter()
        .map(|json| Ok(serde_json::from_str(json)?))
        .collect()
}

fn replace_activity_rows(conn: &Connection, id: &TaskId, entries: &[TaskActivity]) -> Result<()> {
    conn.execute(
        "DELETE FROM activity WHERE task_id = ?1",
        params![id.as_str()],
    )
    .map_err(db_err)?;
    for (seq, entry) in entries.iter().enumerate() {
        conn.execute(
            "INSERT INTO activity (task_id, seq, props) VALUES (?1, ?2, ?3)",
            params![id.as_str(), seq as i64, serde_json::to_string(entry)?],
        )
        .map_err(db_err)?;
    }
    Ok(())
}

impl SqlitePmStore {
    /// `activity.json` equivalent: rows ordered by `seq`, seeded from the
    /// task's last `result` when the history is empty (legacy data).
    fn load_activity(&self, id: &TaskId, record: &Task) -> Result<Vec<TaskActivity>> {
        let rows = self.with_conn(|c| activity_rows(c, id))?;
        if rows.is_empty() {
            return Ok(seeded_activity(record));
        }
        Ok(rows)
    }

    /// Append one entry and persist the whole (small) history. One transaction
    /// — unlike the tree's two-file write, there is no window where the
    /// comment exists without a consistent task row.
    fn append_activity(&self, id: &TaskId, activity: TaskActivity) -> Result<()> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let record =
                load_task_from(&tx, id)?.ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
            let mut history = activity_rows(&tx, id)?;
            if history.is_empty() {
                history = seeded_activity(&record);
            }
            history.push(activity);
            replace_activity_rows(&tx, id, &history)?;
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    /// The tree's `commit_task_activity` writes history first and rolls it
    /// back manually if the record write fails. SQLite gets the same atomicity
    /// from one transaction: either both land or neither does.
    fn commit_task_activity(
        &self,
        id: &TaskId,
        record: &Task,
        before: &Task,
        activity: TaskActivity,
    ) -> Result<()> {
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let mut history = activity_rows(&tx, id)?;
            if history.is_empty() {
                history = seeded_activity(before);
            }
            history.push(activity);
            replace_activity_rows(&tx, id, &history)?;
            update_task_row(&tx, record)?;
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    fn attachment_row(&self, id: &AttachmentId) -> Result<Option<(AttachmentMeta, String)>> {
        self.with_conn(|conn| {
            let row: Option<(String, String)> = conn
                .query_row(
                    "SELECT props, task_dir FROM attachments WHERE id = ?1",
                    params![id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(db_err)?;
            row.map(|(props, dir)| Ok((serde_json::from_str(&props)?, dir)))
                .transpose()
        })
    }

    fn asset_base(&self, rel_dir: &str) -> std::path::PathBuf {
        let path = Path::new(rel_dir);
        if path.is_absolute() {
            // `rel_dir` comes from a DB row. A tampered absolute value used
            // to make `join` discard `data_dir` entirely, so reads and writes
            // could escape to any path on the machine. Confine it instead.
            tracing::warn!(dir = %rel_dir, "absolute task_dir in DB — confining to data_dir/_confined");
            self.config.data_dir.join("_confined")
        } else {
            self.config.data_dir.join(path)
        }
    }

    /// Only DB-owned asset roots may be physically deleted. Imported tasks
    /// point into the retired tree archive, which is never touched here.
    fn is_db_owned_asset_dir(rel_dir: &str) -> bool {
        super::is_db_owned_asset_dir(rel_dir)
    }

    /// `dir_override` lets `write_attachment` redirect newly uploaded
    /// binaries for *imported* tasks into a DB-owned root, so a `db` rollout
    /// never writes into the tree archive it must be able to roll back to.
    fn register_attachment_in(
        &self,
        task_id: &TaskId,
        meta: AttachmentMeta,
        dir_override: Option<String>,
    ) -> Result<()> {
        // Validate paths before they ever reach a row: both the importer and
        // the HTTP upload path converge here, and a relative path with `..`
        // or an absolute path would otherwise be trusted at read time.
        if !super::import::safe_rel(&meta.storage_path)
            || meta
                .thumb_path
                .as_deref()
                .is_some_and(|p| !super::import::safe_rel(p))
        {
            return Err(PmError::PathTraversal(format!("attachment {}", meta.id)));
        }
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let mut task = load_task_from(&tx, task_id)?
                .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
            if task.attachments.len() >= self.config.max_attachments_per_task {
                return Err(PmError::TooManyAttachments(task_id.to_string()));
            }
            if task.attachments.iter().any(|m| m.id == meta.id) {
                return Err(PmError::Internal(format!(
                    "attachment {} already registered",
                    meta.id
                )));
            }
            let rel_dir = match dir_override {
                Some(dir) => dir,
                None => tx
                    .query_row(
                        "SELECT task_dir FROM tasks WHERE id = ?1",
                        params![task_id.as_str()],
                        |row| row.get(0),
                    )
                    .map_err(db_err)?,
            };
            task.attachments.push(meta.clone());
            task.updated_at = Utc::now();
            update_task_row(&tx, &task)?;
            tx.execute(
                "INSERT INTO attachments (id, task_id, task_dir, props) VALUES (?1, ?2, ?3, ?4)",
                params![
                    meta.id.as_str(),
                    task_id.as_str(),
                    rel_dir,
                    serde_json::to_string(&meta)?
                ],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }
}

#[async_trait]
impl PmStore for SqlitePmStore {
    // ── Projects ───────────────────────────────────────────────────

    async fn create_project_as(
        &self,
        input: CreateProject,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Project> {
        let now = Utc::now();
        let project = Project {
            id: ProjectId::generate(),
            title: input.title,
            description: input.description,
            status: ProjectStatus::Active,
            created_by: created_by.to_string(),
            owner_id: None,
            created_at: now,
            updated_at: now,
            metadata: input.metadata,
            // Same invariant as the tree: the creator is a member from the
            // start, otherwise they cannot assign themselves a task.
            members: vec![ProjectMember {
                instance_id: created_by.to_string(),
                kind: created_by_kind,
                added_at: now,
            }],
        };
        self.insert_project(&project)?;
        Ok(project)
    }

    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>> {
        self.load_project(id)
    }

    async fn list_projects(&self) -> Result<Vec<Project>> {
        let mut result = self.with_conn(|conn| {
            let mut stmt = conn.prepare("SELECT props FROM projects").map_err(db_err)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(db_err)?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(db_err)?;
            rows.iter()
                .map(|json| Ok(serde_json::from_str(json)?))
                .collect::<Result<Vec<Project>>>()
        })?;
        result.sort_by_key(|p| p.created_at);
        Ok(result)
    }

    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project> {
        let mut project = self
            .load_project(id)?
            .ok_or_else(|| PmError::ProjectNotFound(id.to_string()))?;
        if let Some(t) = input.title {
            project.title = t;
        }
        if let Some(d) = input.description {
            project.description = d;
        }
        if let Some(s) = input.status {
            project.status = s;
        }
        if let Some(m) = input.metadata {
            project.metadata = m;
        }
        project.updated_at = Utc::now();
        self.save_project(&project)?;
        Ok(project)
    }

    async fn project_counts(&self) -> Result<Vec<crate::types::ProjectCounts>> {
        let mut tallies: HashMap<ProjectId, (usize, usize)> = HashMap::new();
        for task in self.every_task()? {
            let slot = tallies.entry(task.project_id.clone()).or_insert((0, 0));
            slot.0 += 1;
            if task.status == TaskStatus::Submitted {
                slot.1 += 1;
            }
        }
        let mut out: Vec<crate::types::ProjectCounts> = tallies
            .into_iter()
            .map(
                |(project_id, (total, submitted))| crate::types::ProjectCounts {
                    project_id,
                    total,
                    submitted,
                },
            )
            .collect();
        out.sort_by(|a, b| a.project_id.as_str().cmp(b.project_id.as_str()));
        Ok(out)
    }

    async fn delete_project(&self, id: &ProjectId, _cascade: bool) -> Result<()> {
        if self.load_project(id)?.is_none() {
            return Err(PmError::ProjectNotFound(id.to_string()));
        }
        // `tasks.project_id` is ON DELETE CASCADE; attachments and activity
        // cascade from tasks. The retired tree snapshot is never touched.
        self.delete_project_row(id)?;
        // ADR-117 asset layout: icons and every task binary of a project live
        // under one `{pid}` subtree, so one best-effort removal leaves no
        // orphans. DB rows are already committed — a filesystem failure only
        // leaves stale bytes, never an inconsistent store.
        let root = self
            .config
            .data_dir
            .join("assets/projects")
            .join(id.as_str());
        if root.is_dir()
            && let Err(e) = std::fs::remove_dir_all(&root)
        {
            tracing::warn!(path = %root.display(), error = %e, "could not remove project assets");
        }
        Ok(())
    }

    // ── Members ─────────────────────────────────────────────────────

    async fn add_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
        kind: MemberKind,
    ) -> Result<Project> {
        let mut project = self
            .load_project(project_id)?
            .ok_or_else(|| PmError::ProjectNotFound(project_id.to_string()))?;
        if project.members.iter().any(|m| m.instance_id == instance_id) {
            return Err(PmError::MemberAlreadyExists(instance_id.to_string()));
        }
        project.members.push(ProjectMember {
            instance_id: instance_id.to_string(),
            kind,
            added_at: Utc::now(),
        });
        project.updated_at = Utc::now();
        self.save_project(&project)?;
        Ok(project)
    }

    async fn remove_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
    ) -> Result<Project> {
        let mut project = self
            .load_project(project_id)?
            .ok_or_else(|| PmError::ProjectNotFound(project_id.to_string()))?;
        if !project.members.iter().any(|m| m.instance_id == instance_id) {
            return Err(PmError::MemberNotFound(instance_id.to_string()));
        }
        let has_open = self.every_task()?.iter().any(|t| {
            t.project_id == *project_id
                && t.assignee.as_deref() == Some(instance_id)
                && matches!(
                    t.status,
                    TaskStatus::Pending
                        | TaskStatus::InProgress
                        | TaskStatus::Submitted
                        | TaskStatus::Rejected
                )
        });
        if has_open {
            return Err(PmError::MemberHasOpenTasks {
                instance_id: instance_id.to_string(),
                project_id: project_id.to_string(),
            });
        }
        project.members.retain(|m| m.instance_id != instance_id);
        project.updated_at = Utc::now();
        self.save_project(&project)?;
        Ok(project)
    }

    async fn transfer_owner(&self, project_id: &ProjectId, new_owner: &str) -> Result<Project> {
        let mut project = self
            .load_project(project_id)?
            .ok_or_else(|| PmError::ProjectNotFound(project_id.to_string()))?;
        let old_owner = project.owner().to_string();
        if old_owner == new_owner {
            return Err(PmError::BadRequest(
                "the new owner is already the group owner".to_string(),
            ));
        }
        let target_is_human_member = project
            .members
            .iter()
            .any(|m| m.instance_id == new_owner && m.kind == MemberKind::User);
        if !target_is_human_member {
            return Err(PmError::BadRequest(
                "the new owner must be an existing human member".to_string(),
            ));
        }
        if !project.members.iter().any(|m| m.instance_id == old_owner) {
            project.members.push(ProjectMember {
                instance_id: old_owner,
                kind: MemberKind::User,
                added_at: Utc::now(),
            });
        }
        project.owner_id = Some(new_owner.to_string());
        project.updated_at = Utc::now();
        self.save_project(&project)?;
        Ok(project)
    }

    // ── Tasks ───────────────────────────────────────────────────────

    async fn create_task_as(
        &self,
        project_id: &ProjectId,
        input: CreateTask,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Task> {
        let project = self
            .load_project(project_id)?
            .ok_or_else(|| PmError::ProjectNotFound(project_id.to_string()))?;
        // ADR-106 D15: the task's first delivery is a chat message authored
        // by `created_by`, so the author must be able to speak in the group.
        if created_by != project.owner()
            && !project.members.iter().any(|m| m.instance_id == created_by)
        {
            return Err(PmError::AuthorNotProjectMember {
                author: created_by.to_string(),
                project_id: project_id.to_string(),
            });
        }
        if let Some(assignee) = &input.assignee
            && !project.has_member(assignee)
        {
            return Err(PmError::AssigneeNotProjectMember {
                assignee: assignee.clone(),
                project_id: project_id.to_string(),
            });
        }

        let task_id = TaskId::generate();
        let now = Utc::now();
        let (parent, depth) = if let Some(parent_id) = &input.parent_task_id {
            parse_task_id(parent_id.as_str())?;
            let parent_task = self
                .load_task(parent_id)?
                .ok_or_else(|| PmError::TaskNotFound(parent_id.to_string()))?;
            if &parent_task.project_id != project_id {
                return Err(PmError::InvalidId(format!(
                    "parent {} is not in project {}",
                    parent_id, project_id
                )));
            }
            let parent_depth = self.task_meta(parent_id)?.map(|(_, d, _)| d).unwrap_or(0);
            let new_depth = parent_depth + 1;
            if new_depth > self.config.max_task_depth {
                return Err(PmError::MaxDepthExceeded {
                    depth: new_depth,
                    max: self.config.max_task_depth,
                });
            }
            let children: i64 = self.with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM tasks WHERE parent_id = ?1",
                    params![parent_id.as_str()],
                    |row| row.get(0),
                )
                .map_err(db_err)
            })?;
            if children >= 1000 {
                return Err(PmError::TooManyChildren(parent_id.to_string()));
            }
            (Some(parent_id.clone()), new_depth)
        } else {
            (None, 0)
        };

        for dep in &input.depends_on {
            parse_task_id(dep.task_id.as_str())?;
            if dep.task_id == task_id {
                return Err(PmError::DependencyCycle(task_id.to_string()));
            }
        }

        let review_status = if created_by_kind == MemberKind::User {
            ReviewStatus::NotRequired
        } else {
            ReviewStatus::Pending
        };
        let task = Task {
            id: task_id.clone(),
            project_id: project_id.clone(),
            title: input.title,
            description: input.description,
            task_type: input.task_type,
            status: TaskStatus::Pending,
            review_status,
            priority: input.priority,
            assignee: input.assignee,
            due_at: input.due_at,
            start_at: Some(input.start_at.unwrap_or(now)),
            dispatched_at: None,
            dispatch_message_id: None,
            dispatch_attempts: 0,
            dispatch_error: None,
            depends_on: input.depends_on,
            attachments: vec![],
            result: None,
            created_by: created_by.to_string(),
            created_at: now,
            updated_at: now,
            claimed_at: None,
            submitted_at: None,
        };
        let rel_dir = self.new_task_asset_dir(project_id, &task_id);
        let asset_root = self.asset_base(&rel_dir);
        std::fs::create_dir_all(asset_root.join("attachments"))?;
        if let Err(e) = self.insert_task(&task, parent.as_ref(), depth, &rel_dir) {
            // No DB row was committed. Roll back the just-created asset tree
            // so a failed create does not leave an orphan directory behind.
            let _ = std::fs::remove_dir_all(&asset_root);
            return Err(e);
        }
        Ok(task)
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>> {
        self.load_task(id)
    }

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>> {
        let mut result: Vec<Task> = self
            .every_task_with_parent()?
            .into_iter()
            .filter(|(task, parent)| {
                if let Some(pid) = &filter.project_id
                    && &task.project_id != pid
                {
                    return false;
                }
                if let Some(st) = filter.status
                    && task.status != st
                {
                    return false;
                }
                if let Some(a) = &filter.assignee
                    && task.assignee.as_deref() != Some(a.as_str())
                {
                    return false;
                }
                if let Some(rs) = filter.review_status
                    && task.review_status != rs
                {
                    return false;
                }
                if let Some(tt) = filter.task_type
                    && task.task_type != tt
                {
                    return false;
                }
                if let Some(q) = &filter.query
                    && !task.title.to_lowercase().contains(&q.to_lowercase())
                {
                    return false;
                }
                if let Some(want) = &filter.parent_task_id
                    && parent.as_ref() != Some(want)
                {
                    return false;
                }
                if filter.only_roots && parent.is_some() {
                    return false;
                }
                if let Some(since) = filter.updated_since
                    && task.updated_at < since
                {
                    return false;
                }
                true
            })
            .map(|(task, _)| task)
            .collect();

        if filter.only_blocked {
            let mut kept = Vec::new();
            for task in result {
                if !self.compute_blocked_by(&task.id).await?.is_empty() {
                    kept.push(task);
                }
            }
            result = kept;
        }

        match filter.sort.unwrap_or_default() {
            TaskSort::CreatedAt => result.sort_by_key(|t| Reverse(t.created_at)),
            TaskSort::UpdatedAt => result.sort_by_key(|t| Reverse(t.updated_at)),
            TaskSort::DueAt => result.sort_by_key(|t| t.due_at.unwrap_or(DateTime::<Utc>::MAX_UTC)),
            TaskSort::Priority => result.sort_by_key(|t| priority_rank(t.priority)),
        }
        Ok(result)
    }

    async fn update_task(&self, id: &TaskId, input: UpdateTask) -> Result<Task> {
        let mut task = self
            .load_task(id)?
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        if let Some(t) = input.title {
            task.title = t;
        }
        if let Some(d) = input.description {
            task.description = d;
        }
        if let Some(t) = input.task_type {
            task.task_type = t;
        }
        if let Some(s) = input.status {
            validate_transition(task.status, s, id)?;
            if s == TaskStatus::InProgress {
                ensure_creation_approved(&task)?;
            }
            if s != task.status {
                task.clear_dispatch_generation();
            }
            task.status = s;
        }
        if let Some(p) = input.priority {
            task.priority = p;
        }
        if input.assignee.is_some() {
            if let Some(new_assignee) = input.assignee.clone().flatten() {
                let project = self
                    .load_project(&task.project_id)?
                    .ok_or_else(|| PmError::ProjectNotFound(task.project_id.to_string()))?;
                if !project.has_member(&new_assignee) {
                    return Err(PmError::AssigneeNotProjectMember {
                        assignee: new_assignee,
                        project_id: task.project_id.to_string(),
                    });
                }
            }
            if input.assignee.clone().flatten().as_deref() != task.assignee.as_deref()
                && task.status == TaskStatus::Pending
            {
                task.clear_dispatch_generation();
            }
            task.assignee = input.assignee.clone().flatten();
        }
        if input.due_at.is_some() {
            task.due_at = input.due_at.flatten();
        }
        if input.start_at.is_some() {
            task.start_at = input.start_at.flatten();
        }
        if let Some(deps) = input.depends_on {
            for dep in &deps {
                parse_task_id(dep.task_id.as_str())?;
                if dep.task_id == *id {
                    return Err(PmError::DependencyCycle(id.to_string()));
                }
            }
            task.depends_on = deps;
        }
        task.updated_at = Utc::now();
        self.save_task(&task)?;
        Ok(task)
    }

    async fn delete_task(&self, id: &TaskId, _cascade: bool) -> Result<()> {
        if self.load_task(id)?.is_none() {
            return Err(PmError::TaskNotFound(id.to_string()));
        }
        let ids = self.subtree_ids(id)?;
        // Snapshot the DB-owned asset roots *before* the rows disappear, but
        // only unlink them **after** the DB transaction commits. Deleting the
        // binaries first would leave live metadata pointing at files that no
        // longer exist whenever the row delete fails — irreversible data loss
        // for an operation the caller was told had failed.
        let doomed = self.db_owned_asset_dirs(&ids)?;
        self.delete_task_rows(&ids)?;
        for rel in doomed {
            // Best-effort garbage collection: a failed unlink leaves an
            // orphan directory, never a broken reference.
            let _ = std::fs::remove_dir_all(self.asset_base(&rel));
        }
        Ok(())
    }

    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()> {
        let (old_parent, old_depth, _) = self
            .task_meta(id)?
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let (new_parent, new_depth) = match &input.new_parent {
            None => (None, 0),
            Some(parent_id) => {
                parse_task_id(parent_id.as_str())?;
                let parent_task = self
                    .load_task(parent_id)?
                    .ok_or_else(|| PmError::TaskNotFound(parent_id.to_string()))?;
                let task = self
                    .load_task(id)?
                    .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
                if parent_task.project_id != task.project_id {
                    return Err(PmError::InvalidId(format!(
                        "parent {} is not in project {}",
                        parent_id, task.project_id
                    )));
                }
                // DFS-equivalent cycle check: the new parent must not be
                // inside the subtree being moved.
                if self.subtree_ids(id)?.iter().any(|t| t == parent_id) {
                    return Err(PmError::CycleDetected {
                        task_id: id.to_string(),
                        parent_id: parent_id.to_string(),
                    });
                }
                let pdepth = self.task_meta(parent_id)?.map(|(_, d, _)| d).unwrap_or(0);
                let depth = pdepth + 1;
                if depth > self.config.max_task_depth {
                    return Err(PmError::MaxDepthExceeded {
                        depth,
                        max: self.config.max_task_depth,
                    });
                }
                (Some(parent_id.clone()), depth)
            }
        };
        if old_parent == new_parent && old_depth == new_depth {
            return Ok(());
        }
        let delta = new_depth as i64 - old_depth as i64;
        let ids = self.subtree_ids(id)?;
        // The moved root fitting under `max_task_depth` is not enough: every
        // descendant shifts by the same delta, so the deepest one decides.
        // Mirror `TreePmStore::reparent_task`, which checks the whole subtree
        // before moving any directory.
        let deepest = self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT depth FROM tasks WHERE id = ?1")
                .map_err(db_err)?;
            let mut deepest = old_depth as i64;
            for tid in &ids {
                let d: Option<i64> = stmt
                    .query_row(params![tid.as_str()], |row| row.get(0))
                    .optional()
                    .map_err(db_err)?;
                if let Some(d) = d {
                    deepest = deepest.max(d);
                }
            }
            Ok(deepest)
        })?;
        let resulting_deepest = deepest + delta;
        if resulting_deepest > self.config.max_task_depth as i64 {
            return Err(PmError::MaxDepthExceeded {
                depth: resulting_deepest.max(0) as u8,
                max: self.config.max_task_depth,
            });
        }
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            for tid in &ids {
                if tid == id {
                    tx.execute(
                        "UPDATE tasks SET parent_id = ?2, depth = ?3 WHERE id = ?1",
                        params![
                            id.as_str(),
                            new_parent.as_ref().map(|p| p.as_str()),
                            new_depth
                        ],
                    )
                    .map_err(db_err)?;
                } else {
                    // Clamp at zero: a legacy row with an inconsistent
                    // `depth` plus a negative delta would otherwise store a
                    // negative value that `task_meta` casts to `u8` and wraps
                    // into a bogus huge depth.
                    tx.execute(
                        "UPDATE tasks SET depth = MAX(0, depth + ?2) WHERE id = ?1",
                        params![tid.as_str(), delta],
                    )
                    .map_err(db_err)?;
                }
            }
            tx.commit().map_err(db_err)?;
            Ok(())
        })
    }

    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>> {
        if self.load_task(parent)?.is_none() {
            return Err(PmError::TaskNotFound(parent.to_string()));
        }
        let pid = parent.as_str();
        let mut result = self.all_tasks("SELECT props FROM tasks WHERE parent_id = ?1", &[&pid])?;
        result.sort_by_key(|t| t.created_at);
        Ok(result)
    }

    // ── Lifecycle ───────────────────────────────────────────────────

    async fn claim_task(&self, id: &TaskId, actor: &str) -> Result<Task> {
        let _claim = self.claim_lock.lock().await;
        let mut task = self
            .load_task(id)?
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        if let Some(current) = &task.assignee
            && current != actor
        {
            return Err(PmError::Forbidden(format!(
                "task {} is already assigned to instance `{current}`, not `{actor}`; only the assignee can claim an assigned task",
                task.id
            )));
        }
        validate_transition(task.status, TaskStatus::InProgress, id)?;
        ensure_creation_approved(&task)?;
        let blocked = self.compute_blocked_by(id).await?;
        if let Some(b) = blocked.first() {
            return Err(PmError::DependencyNotSatisfied {
                task_id: id.to_string(),
                blocker: b.to_string(),
            });
        }
        let project = self
            .load_project(&task.project_id)?
            .ok_or_else(|| PmError::ProjectNotFound(task.project_id.to_string()))?;
        if !project.has_member(actor) {
            return Err(PmError::AssigneeNotProjectMember {
                assignee: actor.to_string(),
                project_id: task.project_id.to_string(),
            });
        }
        if task.assignee.is_none() {
            task.assignee = Some(actor.to_string());
        }
        task.status = TaskStatus::InProgress;
        task.claimed_at = Some(Utc::now());
        task.clear_dispatch_generation();
        task.updated_at = Utc::now();
        self.save_task(&task)?;
        Ok(task)
    }

    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        doc_refs: Vec<String>,
        actor: &str,
    ) -> Result<Task> {
        let mut task = self
            .load_task(id)?
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        validate_transition(task.status, TaskStatus::Submitted, id)?;
        let before = task.clone();
        let now = Utc::now();
        let result = TaskResult {
            text: text.to_string(),
            attachment_ids,
            doc_refs,
            submitted_by: actor.to_string(),
            submitted_at: now,
        };
        task.status = TaskStatus::Submitted;
        task.submitted_at = Some(now);
        task.updated_at = now;
        let activity = TaskActivity::submitted(&result);
        task.result = Some(result);
        self.commit_task_activity(id, &task, &before, activity)?;
        Ok(task)
    }

    async fn review_task(
        &self,
        id: &TaskId,
        approved: bool,
        reviewer: &str,
        comment: Option<&str>,
    ) -> Result<Task> {
        let mut task = self
            .load_task(id)?
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let before = task.clone();
        let to = if approved {
            if task.status == TaskStatus::Pending {
                None
            } else {
                Some(TaskStatus::Done)
            }
        } else {
            Some(TaskStatus::Rejected)
        };
        if let Some(to) = to {
            validate_transition(task.status, to, id)?;
            if to != task.status {
                task.clear_dispatch_generation();
            }
            task.status = to;
        }
        let new_review = if approved {
            ReviewStatus::Approved
        } else {
            ReviewStatus::Rejected
        };
        if before.status == task.status && before.review_status == new_review {
            return Ok(task);
        }
        task.review_status = new_review;
        task.updated_at = Utc::now();
        let activity = TaskActivity::new(
            if approved {
                ActivityKind::Approved
            } else {
                ActivityKind::Rejected
            },
            reviewer,
            comment
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or_default(),
        );
        self.commit_task_activity(id, &task, &before, activity)?;
        Ok(task)
    }

    // ── Activity ────────────────────────────────────────────────────

    async fn list_activity(&self, task_id: &TaskId) -> Result<Vec<TaskActivity>> {
        let record = self
            .load_task(task_id)?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        self.load_activity(task_id, &record)
    }

    async fn add_task_comment(
        &self,
        task_id: &TaskId,
        text: &str,
        actor: &str,
    ) -> Result<TaskActivity> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err(PmError::BadRequest(
                "comment text must not be empty".to_string(),
            ));
        }
        if self.load_task(task_id)?.is_none() {
            return Err(PmError::TaskNotFound(task_id.to_string()));
        }
        let activity = TaskActivity::new(ActivityKind::Comment, actor, trimmed);
        self.append_activity(task_id, activity.clone())?;
        Ok(activity)
    }

    // ── Dependency graph ────────────────────────────────────────────

    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>> {
        let task = self
            .load_task(task_id)?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let mut blocked = Vec::new();
        for dep in task
            .depends_on
            .iter()
            .filter(|d| d.kind == DependencyKind::Blocks)
        {
            let satisfied = matches!(
                self.load_task(&dep.task_id)?.map(|t| t.status),
                Some(TaskStatus::Done) | Some(TaskStatus::Cancelled)
            );
            if !satisfied {
                blocked.push(dep.task_id.clone());
            }
        }
        Ok(blocked)
    }

    // ── Attachments ─────────────────────────────────────────────────

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>> {
        let task = self
            .load_task(task_id)?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        Ok(task.attachments)
    }

    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>> {
        Ok(self.attachment_row(att_id)?.map(|(meta, _)| meta))
    }

    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()> {
        self.register_attachment_in(task_id, meta, None)
    }

    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()> {
        let (meta, rel_dir) = self
            .attachment_row(att_id)?
            .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))?;
        let task_id: TaskId = self.with_conn(|conn| {
            let s: Option<String> = conn
                .query_row(
                    "SELECT task_id FROM attachments WHERE id = ?1",
                    params![att_id.as_str()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(db_err)?;
            s.and_then(|v| parse_task_id(&v).ok())
                .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))
        })?;
        self.with_conn_mut(|conn| {
            let tx = conn.transaction().map_err(db_err)?;
            let mut task = load_task_from(&tx, &task_id)?
                .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
            if !task.attachments.iter().any(|m| m.id == *att_id) {
                return Err(PmError::AttachmentNotFound(att_id.to_string()));
            }
            task.attachments.retain(|m| m.id != *att_id);
            task.updated_at = Utc::now();
            update_task_row(&tx, &task)?;
            tx.execute(
                "DELETE FROM attachments WHERE id = ?1",
                params![att_id.as_str()],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
            Ok(())
        })?;
        if Self::is_db_owned_asset_dir(&rel_dir)
            && let Some(parent) = Path::new(&meta.storage_path).parent()
        {
            let _ = std::fs::remove_dir_all(self.asset_base(&rel_dir).join(parent));
        }
        Ok(())
    }

    async fn write_attachment(
        &self,
        task_id: &TaskId,
        meta: AttachmentMeta,
        bytes: Vec<u8>,
        thumb_bytes: Option<Vec<u8>>,
    ) -> Result<AttachmentMeta> {
        let (_, _, current_rel) = self
            .task_meta(task_id)?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        // Imported tasks point into the retired tree archive. New binaries must
        // not mutate that snapshot, so they get a project-scoped DB-owned root.
        let rel_dir = if Self::is_db_owned_asset_dir(&current_rel) {
            current_rel
        } else {
            let task = self
                .load_task(task_id)?
                .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
            self.new_task_asset_dir(&task.project_id, task_id)
        };
        // Validate before touching the filesystem. `register_attachment_in`
        // repeats this at commit time, but doing it here first means an
        // invalid relative path cannot leave orphan binaries behind.
        if !super::import::safe_rel(&meta.storage_path)
            || meta
                .thumb_path
                .as_deref()
                .is_some_and(|p| !super::import::safe_rel(p))
        {
            return Err(PmError::PathTraversal(format!("attachment {}", meta.id)));
        }
        let base = self.asset_base(&rel_dir);
        let att_dir = base.join("attachments").join(meta.id.as_str());
        std::fs::create_dir_all(&att_dir)?;
        let original_name = Path::new(&meta.storage_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("original.bin");
        std::fs::write(att_dir.join(original_name), &bytes)?;
        if let (Some(bytes), Some(thumb_path)) = (thumb_bytes, &meta.thumb_path) {
            let thumb_name = Path::new(thumb_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("thumb.jpg");
            std::fs::write(att_dir.join(thumb_name), bytes)?;
        }
        if let Err(e) = self.register_attachment_in(task_id, meta.clone(), Some(rel_dir)) {
            // FS-first means a rejected registration (too many attachments,
            // missing task, DB locked) would otherwise leak the binaries.
            let _ = std::fs::remove_dir_all(&att_dir);
            return Err(e);
        }
        Ok(meta)
    }

    async fn read_attachment_bytes(
        &self,
        att_id: &AttachmentId,
        thumb: bool,
    ) -> Result<Option<(AttachmentMeta, Vec<u8>)>> {
        let Some((meta, rel_dir)) = self.attachment_row(att_id)? else {
            return Ok(None);
        };
        let rel_path = if thumb {
            meta.thumb_path
                .clone()
                .ok_or_else(|| PmError::AttachmentNotFound(format!("thumb for {}", att_id)))?
        } else {
            meta.storage_path.clone()
        };
        if !super::import::safe_rel(&rel_path) {
            return Err(PmError::PathTraversal(rel_path));
        }
        let base = self.asset_base(&rel_dir);
        let full = base.join(&rel_path);
        // Even with a component-level whitelist, resolve symlinks and make
        // sure the final file is still inside the task's asset root.
        let canonical_base = base.canonicalize()?;
        let canonical_file = match full.canonicalize() {
            Ok(p) if p.starts_with(&canonical_base) => p,
            Ok(_) => return Err(PmError::PathTraversal(rel_path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(PmError::AttachmentNotFound(att_id.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        let bytes = std::fs::read(canonical_file)?;
        Ok(Some((meta, bytes)))
    }

    // ── Closing contexts (Gateway notification snapshots) ───────────

    async fn closing_task_contexts(&self, task_id: &TaskId) -> Result<Vec<(ProjectId, Task)>> {
        let mut out = Vec::new();
        for tid in self.subtree_ids(task_id)? {
            if let Some(task) = self.load_task(&tid)? {
                out.push((task.project_id.clone(), task));
            }
        }
        Ok(out)
    }

    async fn closing_project_contexts(&self, project_id: &ProjectId) -> Result<Vec<Task>> {
        self.find_tasks(&TaskFilter {
            project_id: Some(project_id.clone()),
            ..Default::default()
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PmConfig;
    use crate::store::tree::PmMaintenance;
    use crate::types::{
        AttachmentId, AttachmentKind, CreateProject, CreateTask, Dependency, Priority, TaskType,
    };

    fn new_store(dir: &Path) -> SqlitePmStore {
        SqlitePmStore::open(PmConfig {
            data_dir: dir.to_path_buf(),
            ..Default::default()
        })
        .unwrap()
    }

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

    fn attachment_meta() -> AttachmentMeta {
        let id = AttachmentId::generate();
        AttachmentMeta {
            id: id.clone(),
            filename: "notes.txt".into(),
            kind: AttachmentKind::File,
            content_type: "text/plain".into(),
            size: 5,
            sha256: "deadbeef".into(),
            storage_path: format!("attachments/{}/original.txt", id.as_str()),
            thumb_path: None,
            width: None,
            height: None,
            uploaded_by: "human".into(),
            uploaded_at: Utc::now(),
        }
    }

    /// `dispatch_identity` must replay the persisted generation id with no
    /// write at all — matching `TreePmStore`. Writing on every scanner tick
    /// bumps `updated_at` forever and, under `dual`, triggers a full-tree
    /// projection every minute for a task that has not changed.
    #[tokio::test]
    async fn dispatch_identity_replays_without_rewriting_the_row() {
        let tmp = tempfile::tempdir().unwrap();
        let db = new_store(tmp.path());
        let project = db
            .create_project_as(
                CreateProject {
                    title: "dispatch parity".into(),
                    description: String::new(),
                    metadata: Default::default(),
                },
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let task = db
            .create_task_as(
                &project.id,
                task_input("due task", None),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();

        let first = db
            .dispatch_identity(&task.id)
            .await
            .unwrap()
            .expect("first attempt mints an identity");
        let persisted = db.load_task(&task.id).unwrap().unwrap();
        assert_eq!(
            persisted.dispatch_message_id.as_deref(),
            Some(first.as_str())
        );
        let updated_at = persisted.updated_at;

        let second = db.dispatch_identity(&task.id).await.unwrap().unwrap();
        assert_eq!(first, second, "a retry must replay the same identity");
        let after = db.load_task(&task.id).unwrap().unwrap();
        assert_eq!(
            after.updated_at, updated_at,
            "a replay must not rewrite the task row"
        );
    }

    /// Same subtree-depth rule as `TreePmStore::reparent_task`, expressed on
    /// projected columns rather than directories.
    #[tokio::test]
    async fn reparent_rejects_when_the_subtree_would_exceed_max_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let db = SqlitePmStore::open(PmConfig {
            data_dir: tmp.path().to_path_buf(),
            max_task_depth: 2,
            ..Default::default()
        })
        .unwrap();
        let project = db
            .create_project_as(
                CreateProject {
                    title: "depth".into(),
                    description: String::new(),
                    metadata: Default::default(),
                },
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        async fn make(
            db: &SqlitePmStore,
            pid: &ProjectId,
            title: &str,
            parent: Option<TaskId>,
        ) -> Task {
            db.create_task_as(pid, task_input(title, parent), "human", MemberKind::User)
                .await
                .unwrap()
        }
        let target_root = make(&db, &project.id, "target root", None).await;
        let target_child = make(
            &db,
            &project.id,
            "target child",
            Some(target_root.id.clone()),
        )
        .await;
        let mover = make(&db, &project.id, "mover", None).await;
        let mover_child = make(&db, &project.id, "mover child", Some(mover.id.clone())).await;

        let err = db
            .reparent_task(
                &mover.id,
                ReparentTask {
                    new_parent: Some(target_child.id.clone()),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, PmError::MaxDepthExceeded { .. }),
            "error was: {err:?}"
        );
        assert_eq!(db.task_depth(&mover.id), 0);
        assert_eq!(db.task_depth(&mover_child.id), 1);
        assert_eq!(db.parent_of(&mover.id), None);
    }

    /// An invalid attachment path must be rejected before any file is
    /// written, and a rejected registration must not leave orphan binaries.
    #[tokio::test]
    async fn attachment_writes_leave_no_orphans_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let db = new_store(tmp.path());
        let project = db
            .create_project_as(
                CreateProject {
                    title: "attachments".into(),
                    description: String::new(),
                    metadata: Default::default(),
                },
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let task = db
            .create_task_as(
                &project.id,
                task_input("root", None),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();

        let mut bad = attachment_meta();
        bad.storage_path = "../../escaped.txt".into();
        let err = db
            .write_attachment(&task.id, bad.clone(), b"x".to_vec(), None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, PmError::PathTraversal(_)),
            "error was: {err:?}"
        );

        let asset_root = db.asset_base(
            &db.task_meta(&task.id)
                .unwrap()
                .map(|(_, _, dir)| dir)
                .unwrap(),
        );
        assert!(
            !asset_root
                .join("attachments")
                .join(bad.id.as_str())
                .exists(),
            "invalid attachment left an orphan directory"
        );
        assert!(db.list_attachments(&task.id).await.unwrap().is_empty());
    }

    /// One end-to-end pass over the whole business surface. The point is not
    /// to re-test the tree's rules but to prove that a `db` store can actually
    /// serve every kind of mutation the HTTP / MCP layers issue.
    #[tokio::test]
    async fn full_business_flow_on_a_fresh_db() {
        let tmp = tempfile::tempdir().unwrap();
        let db = new_store(tmp.path());

        let project = db
            .create_project_as(
                CreateProject {
                    title: "db project".into(),
                    description: String::new(),
                    metadata: Default::default(),
                },
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let pid = project.id.clone();

        let root = db
            .create_task_as(&pid, task_input("root", None), "human", MemberKind::User)
            .await
            .unwrap();
        let child = db
            .create_task_as(
                &pid,
                task_input("child", Some(root.id.clone())),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        assert_eq!(db.task_depth(&root.id), 0);
        assert_eq!(db.task_depth(&child.id), 1);
        assert_eq!(db.parent_of(&child.id).as_ref(), Some(&root.id));

        let roots = db
            .find_tasks(&TaskFilter {
                project_id: Some(pid.clone()),
                only_roots: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].id, root.id);

        // Moving the root under its own descendant must be refused.
        let err = db
            .reparent_task(
                &root.id,
                ReparentTask {
                    new_parent: Some(child.id.clone()),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(err.error_code(), "cycle_detected");

        // Promote then re-nest the child; both are pure column updates.
        db.reparent_task(&child.id, ReparentTask { new_parent: None })
            .await
            .unwrap();
        assert_eq!(db.task_depth(&child.id), 0);
        assert_eq!(db.parent_of(&child.id), None);
        db.reparent_task(
            &child.id,
            ReparentTask {
                new_parent: Some(root.id.clone()),
            },
        )
        .await
        .unwrap();
        assert_eq!(db.parent_of(&child.id).as_ref(), Some(&root.id));

        // Lifecycle: claim → submit → approve, with history in the DB.
        let claimed = db.claim_task(&root.id, "human").await.unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        db.submit_task(&root.id, "all done", vec![], vec![], "human")
            .await
            .unwrap();
        let reviewed = db
            .review_task(&root.id, true, "human", Some("LGTM"))
            .await
            .unwrap();
        assert_eq!(reviewed.status, TaskStatus::Done);
        assert_eq!(reviewed.review_status, ReviewStatus::Approved);
        let history = db.list_activity(&root.id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].kind, ActivityKind::Submitted);
        assert_eq!(history[1].kind, ActivityKind::Approved);
        db.add_task_comment(&root.id, "  thanks  ", "human")
            .await
            .unwrap();
        assert_eq!(db.list_activity(&root.id).await.unwrap().len(), 3);
        assert_eq!(db.project_counts().await.unwrap()[0].submitted, 0);

        // Attachment metadata lives in the DB; binaries stay on disk.
        let meta = attachment_meta();
        let att_id = meta.id.clone();
        db.write_attachment(&child.id, meta, b"hello".to_vec(), None)
            .await
            .unwrap();
        assert_eq!(db.list_attachments(&child.id).await.unwrap().len(), 1);
        assert_eq!(
            db.get_attachment(&att_id).await.unwrap().unwrap().filename,
            "notes.txt"
        );
        let (_, bytes) = db
            .read_attachment_bytes(&att_id, false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, b"hello");
        db.delete_attachment(&att_id).await.unwrap();
        assert!(db.get_attachment(&att_id).await.unwrap().is_none());

        // Deleting a root cascades through the subtree.
        let contexts = db.closing_task_contexts(&root.id).await.unwrap();
        assert_eq!(contexts.len(), 2);
        db.delete_task(&root.id, false).await.unwrap();
        assert!(db.get_task(&child.id).await.unwrap().is_none());
        assert!(db.project_counts().await.unwrap().is_empty());

        db.delete_project(&pid, false).await.unwrap();
        let err = db.delete_project(&pid, false).await.unwrap_err();
        assert_eq!(err.error_code(), "project_not_found");
    }

    #[tokio::test]
    async fn dispatch_scan_does_not_deadlock_when_dependencies_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let db = new_store(tmp.path());
        let project = db
            .create_project_as(
                CreateProject {
                    title: "dispatch".into(),
                    description: String::new(),
                    metadata: Default::default(),
                },
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        db.add_project_member(&project.id, "agent-1", MemberKind::Agent)
            .await
            .unwrap();
        let blocker = db
            .create_task_as(
                &project.id,
                task_input("blocker", None),
                "human",
                MemberKind::User,
            )
            .await
            .unwrap();
        let mut input = task_input("blocked", None);
        input.assignee = Some("agent-1".into());
        input.depends_on = vec![crate::types::Dependency {
            task_id: blocker.id.clone(),
            kind: DependencyKind::Blocks,
        }];
        db.create_task_as(&project.id, input, "human", MemberKind::User)
            .await
            .unwrap();

        // The old implementation re-entered the connection Mutex from inside
        // `dispatch_due_ids` and hung here; it now returns an empty tick.
        let due = db.dispatch_due_ids(Utc::now(), 10);
        assert!(due.is_empty());
        db.review_task(&blocker.id, true, "human", None)
            .await
            .unwrap();
        // Still blocked: the prerequisite is Done only after a submit+approve.
        assert!(db.dispatch_due_ids(Utc::now(), 10).is_empty());
    }
}
