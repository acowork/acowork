//! PM 存储契约：`PmStore`（业务方法）与 `PmMaintenance`（维护 / 派生读）。
//!
//! 运行期只有一个实现 `SqlitePmStore`；HTTP / MCP / dispatch 统一依赖
//! `Arc<dyn PmStore>`，因此这两个 trait 是上层与 SQLite 之间唯一的边界。

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::error::Result;
use crate::types::{
    AttachmentId, AttachmentMeta, CreateProject, CreateTask, MemberKind, Project, ProjectId,
    ReparentTask, Task, TaskActivity, TaskFilter, TaskId, UpdateProject, UpdateTask,
};

/// 非业务 CRUD 的后端能力：启动 / 关闭、派发支持、派生读、图标资产与诊断。
#[async_trait]
pub trait PmMaintenance: Send + Sync {
    /// One-shot `start_at` backfill on legacy pending tasks; returns rows changed.
    async fn migrate_null_start_at(&self) -> Result<usize>;

    /// One-shot rewrite of stored dependencies to the single kind the write
    /// path accepts (`blocks`), for databases written before `relates` /
    /// `duplicates` were retired. Returns the number of tasks whose list
    /// changed.
    ///
    /// It is **data hygiene, not enforcement**: the gates no longer read
    /// `kind` (`dispatch_blocked` walks edges), so a database that never runs
    /// this is still correct — it is only untidy. That is what lets the
    /// rewrite be genuinely one-shot: the implementing store records a marker
    /// in `meta` and skips the full `props` scan on later starts. See
    /// `types::DependencyKind`.
    async fn migrate_dependency_kinds(&self) -> Result<usize>;

    /// Flush pending writes before the process exits.
    async fn shutdown(&self) -> Result<()>;

    /// Live storage diagnostics surfaced by `/health`.
    fn storage_diagnostics(&self) -> Value;

    /// Candidate task ids for one dispatch tick, already filtered and capped.
    fn dispatch_due_ids(&self, now: DateTime<Utc>, limit: usize) -> Vec<TaskId>;

    /// Nesting depth used when shaping `TaskResponse`.
    fn task_depth(&self, task_id: &TaskId) -> u8;

    /// Parent id used when shaping `TaskResponse`.
    fn parent_of(&self, task_id: &TaskId) -> Option<TaskId>;

    /// Task count for one project, used by board headers and MCP summaries.
    fn project_task_count(&self, project_id: &ProjectId) -> usize;

    // ── Project assets (icons) ─────────────────────────────────────
    // Binaries stay on the filesystem (ADR-061 decision 5); only their
    // logical names are exposed through the API.

    async fn list_project_icons(&self, project_id: &ProjectId) -> Result<Vec<String>>;
    async fn write_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
        bytes: &[u8],
    ) -> Result<()>;
    async fn read_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
    ) -> Result<Option<(String, Vec<u8>)>>;

    // ── Dispatch support ───────────────────────────────────────────

    fn is_dispatch_blocked(&self, task: &Task) -> bool;
    async fn dispatch_identity(&self, id: &TaskId) -> Result<Option<String>>;
    async fn mark_dispatched(&self, id: &TaskId) -> Result<()>;
    async fn record_dispatch_failure(&self, id: &TaskId, reason: &str) -> Result<u32>;
    /// Re-arm the dispatch of a task whose attempts were given up on: end the
    /// current generation (`dispatch_attempts` / `dispatch_error` /
    /// `dispatch_message_id` cleared), so the scanner sees it again on the next
    /// tick — the same delivery path as a first dispatch, no bypass.
    ///
    /// Lives with the rest of the dispatch generation (`mark_dispatched` /
    /// `record_dispatch_failure`) because it only manipulates that machinery;
    /// it is not a task-lifecycle transition. Only a `pending`,
    /// never-dispatched task has anything to re-arm — anything else returns a
    /// conflict instead of quietly reporting success. The re-arm is recorded
    /// on the activity stream in the same write: a delivered row and its trail
    /// cannot disagree.
    async fn retry_dispatch(&self, id: &TaskId, actor: &str) -> Result<Task>;
}

/// PM 业务方法集合。上层只依赖此 trait，不直接引用具体 store。
#[async_trait]
pub trait PmStore: PmMaintenance + Send + Sync {
    // ── Project operations ─────────────────────────────────────────

    /// Convenience for callers holding only an actor string. Production code
    /// MUST use [`Self::create_project_as`] with an explicit kind (ADR-076).
    async fn create_project(&self, input: CreateProject, created_by: &str) -> Result<Project> {
        let kind = MemberKind::from_actor(created_by);
        self.create_project_as(input, created_by, kind).await
    }
    async fn create_project_as(
        &self,
        input: CreateProject,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Project>;
    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>>;
    async fn list_projects(&self) -> Result<Vec<Project>>;
    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project>;
    /// `cascade = false` 且项目仍有任务时返回冲突；`true` 连同任务一起删除。
    async fn delete_project(&self, id: &ProjectId, cascade: bool) -> Result<()>;

    /// Per-project task tallies for board badges, answered in one query.
    async fn project_counts(&self) -> Result<Vec<crate::types::ProjectCounts>>;

    // ── Member operations ───────────────────────────────────────────

    async fn add_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
        kind: MemberKind,
    ) -> Result<Project>;
    async fn remove_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
    ) -> Result<Project>;
    async fn transfer_owner(&self, project_id: &ProjectId, new_owner: &str) -> Result<Project>;

    // ── Task operations ─────────────────────────────────────────────

    async fn create_task(
        &self,
        project_id: &ProjectId,
        input: CreateTask,
        created_by: &str,
    ) -> Result<Task> {
        let kind = MemberKind::from_actor(created_by);
        self.create_task_as(project_id, input, created_by, kind)
            .await
    }
    async fn create_task_as(
        &self,
        project_id: &ProjectId,
        input: CreateTask,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Task>;
    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>>;
    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>>;
    async fn update_task(&self, id: &TaskId, input: UpdateTask) -> Result<Task>;
    /// `cascade = false` 且任务仍有子任务时返回冲突；`true` 连同子树一起删除。
    async fn delete_task(&self, id: &TaskId, cascade: bool) -> Result<()>;
    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()>;
    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>>;

    // ── Lifecycle operations ────────────────────────────────────────

    async fn claim_task(&self, id: &TaskId, actor: &str) -> Result<Task>;
    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        doc_refs: Vec<String>,
        actor: &str,
    ) -> Result<Task>;
    async fn review_task(
        &self,
        id: &TaskId,
        approved: bool,
        reviewer: &str,
        comment: Option<&str>,
    ) -> Result<Task>;
    // ── Activity (submission history) ──────────────────────────────

    async fn list_activity(&self, task_id: &TaskId) -> Result<Vec<TaskActivity>>;
    async fn add_task_comment(
        &self,
        task_id: &TaskId,
        text: &str,
        actor: &str,
    ) -> Result<TaskActivity>;

    // ── Dependency graph ────────────────────────────────────────────

    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>>;

    // ── Attachment operations ──────────────────────────────────────

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>>;
    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>>;
    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()>;
    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()>;
    /// Write the physical file (original + optional thumb) and persist metadata.
    /// `meta.storage_path` / `thumb_path` are relative to the task directory.
    async fn write_attachment(
        &self,
        task_id: &TaskId,
        meta: AttachmentMeta,
        bytes: Vec<u8>,
        thumb_bytes: Option<Vec<u8>>,
    ) -> Result<AttachmentMeta>;
    async fn read_attachment_bytes(
        &self,
        att_id: &AttachmentId,
        thumb: bool,
    ) -> Result<Option<(AttachmentMeta, Vec<u8>)>>;

    /// Snapshot the tasks whose logical execution contexts end with this
    /// mutation. Best-effort by contract; callers only use it for notifications.
    async fn closing_task_contexts(&self, task_id: &TaskId) -> Result<Vec<(ProjectId, Task)>>;
    async fn closing_project_contexts(&self, project_id: &ProjectId) -> Result<Vec<Task>>;
}
