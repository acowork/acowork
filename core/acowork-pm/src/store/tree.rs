//! `TreePmStore`：基于目录树的 PM 存储实现。
//!
//! ## 关键 API
//!
//! - `new(config)` —— 构造 + 创建根目录
//! - `rebuild_index()` —— 从文件系统 walkdir 重建内存索引（启动时 / 崩溃恢复）
//! - `PmStore` trait 方法 —— 全部走 `Arc<TreePmStore>`，handler 通过 axum `State<ApiState>` 访问
//!
//! ## 完整实现分阶段交付
//!
//! - **P0**：trait 定义 + 构造 + 索引骨架
//! - **P1（本文件）**：`rebuild_index` walkdir 实现 + 全部 CRUD / 子树 / reparent /
//!   状态机 / 附件实现
//! - **P3**：依赖图深层运算（`compute_blocked` 已实现；跨项目环检测增强）
//! - **P5+**：可选 SQLite 后端（替换 trait impl）
//!
//! ## 物理布局
//!
//! ```text
//! {data}/acowork-pm/projects/{pid}/
//! ├── project.json
//! └── tasks/
//!     ├── {root_task_id}/
//!     │   ├── task.json
//!     │   ├── attachments/{att_id}/{original.{ext},thumb.jpg}
//!     │   └── children/{child_task_id}/...
//!     └── {another_root_task_id}/
//! ```
//!
//! **核心不变量**（与 [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §3.1 对齐）：
//! - 任务恒为目录（叶子任务也是目录）
//! - 子任务强制放在父任务 `children/` 子目录下
//! - `task.json` **不**冗余 `parent_id` / `subtask_ids`——物理嵌套即权威
//! - 删除 = `rm -rf` 目录树（子树 + 附件原子级清理）
//! - Reparent = `mv` 目录（0 文件写），DFS 防环 + 深度限制

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::RwLock;
use tokio::fs;

use crate::config::PmConfig;
use crate::error::{PmError, Result};
use crate::types::{
    AttachmentId, AttachmentMeta, CreateProject, CreateTask, DependencyKind, MemberKind, Priority,
    Project, ProjectId, ProjectMember, ProjectStatus, ReparentTask, ReviewStatus, Task, TaskFilter,
    TaskId, TaskResult, TaskSort, TaskStatus, UpdateProject, UpdateTask,
};

use super::atomic::{
    atomic_write_json, check_not_reserved, read_json, rename_or_fallback, validate_id_format,
};
use super::index::{TaskEntry, TaskIndex};

// ────────────────────────────────────────────────────────────────────────────
// PmStore trait
// ────────────────────────────────────────────────────────────────────────────

/// PM 存储抽象。
///
/// 业务层（HTTP handlers / MCP tools）只依赖此 trait，**不**直接引用 [`TreePmStore`]。
/// 未来 P5+ 切 SQLite 仅需替换实现，业务代码 0 改动。
#[async_trait]
pub trait PmStore: Send + Sync {
    // ── Project operations ─────────────────────────────────────────

    /// Create a project, deriving the creator's kind from the value
    /// (`human`/`unknown` → [`MemberKind::User`], else `Agent`).
    ///
    /// ponytail: a *convenience* for callers that only hold an actor string
    /// (tests, legacy paths). Production code MUST use [`Self::create_project_as`]
    /// with an explicit kind — a bare `user_id` is a UUID and therefore
    /// indistinguishable from an `instance_id` by value (ADR-076 §决策 11).
    async fn create_project(&self, input: CreateProject, created_by: &str) -> Result<Project> {
        let kind = MemberKind::from_actor(created_by);
        self.create_project_as(input, created_by, kind).await
    }
    /// Create a project with the creator's identity kind stated explicitly.
    async fn create_project_as(
        &self,
        input: CreateProject,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Project>;
    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>>;
    async fn list_projects(&self) -> Result<Vec<Project>>;
    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project>;
    async fn delete_project(&self, id: &ProjectId, cascade: bool) -> Result<()>;

    // ── Member operations ───────────────────────────────────────────

    /// 添加项目成员（Agent 实例或人类账号，ADR-076 §决策 11）。
    /// 重复添加 → [`PmError::MemberAlreadyExists`]。
    async fn add_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
        kind: MemberKind,
    ) -> Result<Project>;
    /// 移除项目成员。成员不存在 → [`PmError::MemberNotFound`]；
    /// 该成员名下仍有未完成任务 → [`PmError::MemberHasOpenTasks`]（显式失败）。
    async fn remove_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
    ) -> Result<Project>;

    /// 转让群主：把 `owner_id` 设为 `new_owner`，并把**原群主降级**为普通
    /// 成员（并入 `members`，不丢访问权/历史）。`new_owner` 必须是当前
    /// 成员里的**人类账号**（`kind: User`）；转让给自己 →
    /// [`PmError::BadRequest`]；项目不存在 → [`PmError::ProjectNotFound`]。
    /// 调用者（`X-Actor` == 原 owner，或 admin）的鉴权在 API 层完成。
    async fn transfer_owner(
        &self,
        project_id: &ProjectId,
        new_owner: &str,
    ) -> Result<Project>;

    // ── Task operations ─────────────────────────────────────────────

    async fn create_task(&self, project_id: &ProjectId, input: CreateTask, created_by: &str)
        -> Result<Task> {
        let kind = MemberKind::from_actor(created_by);
        self.create_task_as(project_id, input, created_by, kind).await
    }
    /// Create a task with the creator's identity kind stated explicitly
    /// (drives `review_status`: `User` → `NotRequired`, `Agent` → `Pending`).
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
    async fn delete_task(&self, id: &TaskId, cascade: bool) -> Result<()>;
    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()>;
    /// 列出任务的直接子任务（按物理 `children/` 目录）。
    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>>;

    // ── Lifecycle operations ────────────────────────────────────────

    async fn claim_task(&self, id: &TaskId, actor: &str) -> Result<Task>;
    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        actor: &str,
    ) -> Result<Task>;
    async fn review_task(&self, id: &TaskId, approved: bool, reviewer: &str) -> Result<Task>;

    // ── Dependency graph ────────────────────────────────────────────

    /// 计算阻塞本任务的依赖（kind=Blocks 且未完成）。
    /// 用于 API 响应附带 `blocked_by` / `is_blocked` 字段。
    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>>;

    // ── Attachment operations ──────────────────────────────────────

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>>;
    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>>;
    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()>;
    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()>;
    /// 写附件物理文件（original + optional thumb）并注册元数据。
    ///
    /// `meta.storage_path` / `meta.thumb_path` 为相对任务目录的路径（handler
    /// 按 `attachments/{att_id}/original.{ext}` / `thumb.jpg` 约定构造）。
    /// 返回持久化后的完整 `meta`。
    async fn write_attachment(
        &self,
        task_id: &TaskId,
        meta: AttachmentMeta,
        bytes: Vec<u8>,
        thumb_bytes: Option<Vec<u8>>,
    ) -> Result<AttachmentMeta>;
    /// 读附件物理文件内容（`thumb=true` 读缩略图，仅图片有）。
    /// 返回 `(meta, bytes)`；附件不存在返回 `None`。
    async fn read_attachment_bytes(
        &self,
        att_id: &AttachmentId,
        thumb: bool,
    ) -> Result<Option<(AttachmentMeta, Vec<u8>)>>;

    /// Design 27 §2.1 T2-a: snapshot the tasks whose logical execution
    /// contexts end with this mutation — the task itself plus its subtree,
    /// each paired with the project id the Gateway key needs. Called before
    /// `delete_task` / `delete_project` (afterwards the data is gone).
    /// Best-effort by contract: the caller only uses the result for
    /// notifications and ignores errors.
    async fn closing_task_contexts(&self, task_id: &TaskId) -> Result<Vec<(ProjectId, Task)>>;

    /// Same question for a whole project (`delete_project`).
    async fn closing_project_contexts(&self, project_id: &ProjectId) -> Result<Vec<Task>>;
}

// ────────────────────────────────────────────────────────────────────────────
// TreePmStore 结构 + 构造
// ────────────────────────────────────────────────────────────────────────────

/// 项目图标允许的扩展名 → content-type。非图片返回 `None`。
fn icon_ext(name: &str) -> Option<&'static str> {
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

/// 目录树存储实现。
///
/// 内部状态：
/// - `projects_dir` = `{data_dir}/projects`
/// - `trash_dir` = `{data_dir}/.trash`
/// - `index` = 内存二级索引（`Arc<RwLock<TaskIndex>>`）
#[derive(Debug)]
pub struct TreePmStore {
    pub(crate) config: PmConfig,
    pub(crate) projects_dir: PathBuf,
    pub(crate) trash_dir: PathBuf,
    pub(crate) index: Arc<RwLock<TaskIndex>>,
}

impl TreePmStore {
    /// 构造存储实例（**不**重建索引——调用方决定）。
    ///
    /// 创建根目录 `projects/` 和 `.trash/`。
    pub async fn new(config: PmConfig) -> Result<Self> {
        let projects_dir = config.projects_dir();
        let trash_dir = config.trash_dir();

        fs::create_dir_all(&projects_dir).await?;
        fs::create_dir_all(&trash_dir).await?;

        Ok(Self {
            config,
            projects_dir,
            trash_dir,
            index: Arc::new(RwLock::new(TaskIndex::new())),
        })
    }

    /// 从文件系统 walkdir 重建内存索引。
    ///
    /// 启动时由 [`crate::server::PmService::new`] 调用（如果配置开启）。
    /// 崩溃后调用幂等——从 `task.json` 全量重建，**无修复逻辑**（物理是权威，无冗余可漂移）。
    pub async fn rebuild_index(&self) -> Result<()> {
        let mut index = TaskIndex::new();
        let mut projects = fs::read_dir(&self.projects_dir).await?;

        while let Some(proj) = projects.next_entry().await? {
            if !proj.file_type().await?.is_dir() {
                continue;
            }
            let name = proj.file_name().to_string_lossy().into_owned();
            if !name.starts_with("p-") {
                continue;
            }
            let project_id = parse_project_id(&name)?;
            let tasks_dir = proj.path().join("tasks");
            if !fs::try_exists(&tasks_dir).await? {
                continue;
            }
            self.walk_tasks(&tasks_dir, &project_id, 0, &mut index)
                .await?;
        }

        let count = index.len();
        *self.index.write() = index;
        tracing::info!(count, "PM index rebuilt");
        Ok(())
    }

    /// 迭代扫描 `tasks/`（或 `children/`）目录，填充索引。
    ///
    /// 用显式栈做 DFS（避免递归 `async fn` 的 E0733 无限大小 future）；
    /// 项目 ID 整树不变，栈仅携带目录路径与深度。
    async fn walk_tasks(
        &self,
        root_dir: &Path,
        project_id: &ProjectId,
        root_depth: u8,
        index: &mut TaskIndex,
    ) -> Result<()> {
        let mut stack: Vec<(PathBuf, u8)> = vec![(root_dir.to_path_buf(), root_depth)];
        while let Some((dir, depth)) = stack.pop() {
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                if !entry.file_type().await?.is_dir() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.starts_with("t-") {
                    continue;
                }
                let task_id = parse_task_id(&name)?;
                let task_path = entry.path();
                let json_path = task_path.join("task.json");
                if !fs::try_exists(&json_path).await? {
                    tracing::warn!(task = %name, "task dir missing task.json, skipping");
                    continue;
                }
                let task: Task = read_json(&json_path).await?;

                index.insert(
                    task_id.clone(),
                    TaskEntry {
                        project_id: project_id.clone(),
                        status: task.status,
                        assignee: task.assignee.clone(),
                        depth,
                        dir_path: task_path.clone(),
                    },
                );
                for att in &task.attachments {
                    index.register_attachment(att.id.clone(), task_id.clone());
                }
                for dep in &task.depends_on {
                    if dep.kind == DependencyKind::Blocks {
                        index.add_dependency(task_id.clone(), dep.task_id.clone());
                    }
                }

                // 子任务目录入栈
                let children_dir = task_path.join("children");
                if fs::try_exists(&children_dir).await? {
                    stack.push((children_dir, depth + 1));
                }
            }
        }
        Ok(())
    }

    /// 迭代收集目录下所有直接子任务 ID（不含自身）。
    ///
    /// 显式栈 DFS，避免递归 `async fn` 的 E0733。
    async fn collect_subtree_ids(&self, root_dir: &Path, out: &mut Vec<TaskId>) -> Result<()> {
        let mut stack: Vec<PathBuf> = vec![root_dir.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let children = dir.join("children");
            if !fs::try_exists(&children).await? {
                continue;
            }
            let mut entries = fs::read_dir(&children).await?;
            while let Some(e) = entries.next_entry().await? {
                if !e.file_type().await?.is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("t-") {
                    continue;
                }
                let tid = parse_task_id(&name)?;
                out.push(tid.clone());
                stack.push(e.path());
            }
        }
        Ok(())
    }

    /// 迭代收集被移动子树的索引更新项（**不持锁**）。
    ///
    /// 返回 `(task_id, 新 dir_path, 新 depth)` 列表；调用方在物理 mv 之后
    /// 一次性加锁批量应用，避免 parking_lot guard 跨 `.await`（非 Send）。
    /// 显式栈 DFS，避免递归 `async fn` 的 E0733。
    async fn collect_subtree_updates(
        &self,
        root_id: &TaskId,
        new_root_dir: &Path,
        new_root_depth: u8,
        out: &mut Vec<(TaskId, PathBuf, u8)>,
    ) -> Result<()> {
        let mut stack: Vec<(TaskId, PathBuf, u8)> =
            vec![(root_id.clone(), new_root_dir.to_path_buf(), new_root_depth)];
        while let Some((rid, rdir, rdepth)) = stack.pop() {
            out.push((rid.clone(), rdir.clone(), rdepth));
            let children = rdir.join("children");
            if !fs::try_exists(&children).await? {
                continue;
            }
            let mut entries = fs::read_dir(&children).await?;
            while let Some(e) = entries.next_entry().await? {
                if !e.file_type().await?.is_dir() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                if !name.starts_with("t-") {
                    continue;
                }
                let cid = parse_task_id(&name)?;
                stack.push((cid, e.path(), rdepth + 1));
            }
        }
        Ok(())
    }

    /// 优雅停机（当前无 pending write，保留接口供未来 flush）。
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("TreePmStore shutdown complete");
        Ok(())
    }

    /// 计算任务的物理路径（不含 `task.json` 后缀）——**仅根任务**。
    ///
    /// 嵌套任务必须经索引 `TaskEntry::dir_path` 定位（O(1)）。
    pub(crate) fn task_dir(&self, project_id: &ProjectId, task_id: &TaskId) -> PathBuf {
        self.projects_dir
            .join(project_id.as_str())
            .join("tasks")
            .join(task_id.as_str())
    }

    /// 计算项目目录。
    pub(crate) fn project_dir(&self, project_id: &ProjectId) -> PathBuf {
        self.projects_dir.join(project_id.as_str())
    }

    /// 计算项目 `project.json` 路径。
    pub(crate) fn project_json_path(&self, project_id: &ProjectId) -> PathBuf {
        self.project_dir(project_id).join("project.json")
    }

    /// 计算项目图标的物理目录 `{project_dir}/icon`。
    ///
    /// 图标是项目级小图片资产（自定义上传落这里），与任务附件
    /// 分开——附件是 task 级、随任务删除。布局：
    /// `{data_dir}/projects/{pid}/icon/icon-NN.{ext}`
    pub(crate) fn project_icon_dir(&self, project_id: &ProjectId) -> PathBuf {
        self.project_dir(project_id).join("icon")
    }

    /// 写入一个项目图标文件（`name` 由 handler 生成、已净化，不含路径分隔）。
    pub async fn write_project_icon(&self, project_id: &ProjectId, name: &str, bytes: &[u8]) -> Result<()> {
        let dir = self.project_icon_dir(project_id);
        fs::create_dir_all(&dir).await?;
        fs::write(dir.join(name), bytes).await?;
        Ok(())
    }

    /// 列出项目图标文件名（`icon-*` 且图片扩展名），升序。目录不存在 → 空。
    pub async fn list_project_icons(&self, project_id: &ProjectId) -> Result<Vec<String>> {
        let dir = self.project_icon_dir(project_id);
        let mut out = Vec::new();
        let mut entries = match fs::read_dir(&dir).await {
            Ok(e) => e,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(err) => return Err(PmError::Io(err)),
        };
        while let Some(e) = entries.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("icon-") && icon_ext(&name).is_some() {
                out.push(name);
            }
        }
        out.sort();
        Ok(out)
    }

    /// 读取一个项目图标字节（`(content_type, bytes)`）。不存在 → `None`。
    ///
    /// 调用方（handler）必须先校验 `name` 不含路径分隔 / `..`，这里
    /// 只按净化后的文件名直接拼接目录，不做二次遍历防护。
    pub async fn read_project_icon(
        &self,
        project_id: &ProjectId,
        name: &str,
    ) -> Result<Option<(String, Vec<u8>)>> {
        let path = self.project_icon_dir(project_id).join(name);
        match fs::read(&path).await {
            Ok(bytes) => Ok(icon_ext(name).map(|ct| (ct.to_string(), bytes))),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(PmError::Io(err)),
        }
    }

    /// 当前索引任务数（供健康检查）。
    pub fn indexed_task_count(&self) -> usize {
        self.index.read().len()
    }

    /// 某项目下的任务总数（二级索引，O(1)）。供项目概览（看板头 / MCP 摘要）。
    pub fn project_task_count(&self, project_id: &ProjectId) -> usize {
        self.index
            .read()
            .by_project
            .get(project_id)
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// 内部辅助：从索引读取任务条目。
    pub fn index_entry(&self, task_id: &TaskId) -> Option<TaskEntry> {
        self.index.read().by_id.get(task_id).cloned()
    }

    /// 内部辅助：读取项目 `project.json`（不存在 → [`PmError::ProjectNotFound`]）。
    pub(crate) async fn read_project(&self, project_id: &ProjectId) -> Result<Project> {
        let path = self.project_json_path(project_id);
        if !fs::try_exists(&path).await? {
            return Err(PmError::ProjectNotFound(project_id.to_string()));
        }
        read_json(&path).await
    }

    /// 推导任务的父任务 ID（由物理目录位置推断，根任务返回 `None`）。
    ///
    /// `dir_path` 结构：
    /// - 根任务：`.../projects/{pid}/tasks/{tid}`
    /// - 子任务：`.../projects/{pid}/tasks/{parent}/children/{tid}`
    ///
    /// **P2 新增**：供列表接口返回 `parent_id` 派生字段，前端一次拉取
    /// 即可重建看板树（无需逐任务调用 `/tasks/:tid/children`）。
    pub fn parent_of(&self, task_id: &TaskId) -> Option<TaskId> {
        let entry = self.index.read().by_id.get(task_id)?.clone();
        let parent = entry.dir_path.parent()?;
        // 子任务父目录名为 "children"，其上一级目录名即父任务 id
        if parent.file_name()?.to_string_lossy() == "children" {
            let grand = parent.parent()?;
            let name = grand.file_name()?.to_string_lossy().into_owned();
            TaskId::parse(&name).ok()
        } else {
            // 根任务：父目录名是 "tasks"
            None
        }
    }

    /// 内部辅助：联动指派校验 —— `assignee`（非空）必须是项目成员。
    ///
    /// 所有写入 `task.assignee` 的路径（create / update / claim）共用，
    /// REST 与 MCP 两条入口统一由 store 层强制。
    pub(crate) fn ensure_assignee_is_member(
        &self,
        project: &Project,
        assignee: &str,
    ) -> Result<()> {
        if project.members.iter().any(|m| m.instance_id == assignee) {
            Ok(())
        } else {
            Err(PmError::AssigneeNotProjectMember {
                assignee: assignee.to_string(),
                project_id: project.id.to_string(),
            })
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ID 解析辅助
// ────────────────────────────────────────────────────────────────────────────

fn parse_project_id(s: &str) -> Result<ProjectId> {
    validate_id_format(s, "p-", "project")?;
    ProjectId::parse(s).map_err(|_| PmError::InvalidId(format!("project: {}", s)))
}

fn parse_task_id(s: &str) -> Result<TaskId> {
    validate_id_format(s, "t-", "task")?;
    check_not_reserved(s)?;
    TaskId::parse(s).map_err(|_| PmError::InvalidId(format!("task: {}", s)))
}

/// 状态流转合法校验（对齐设计文档 §4 状态机图）。
fn validate_transition(from: TaskStatus, to: TaskStatus, task_id: &TaskId) -> Result<()> {
    let allowed = match from {
        // `pending → rejected`：人类驳回一个 Agent 刚建、从未开工的任务
        // （创建审批的 reject 分支，区别于结果审核的 `submitted → rejected`）。
        TaskStatus::Pending => matches!(
            to,
            TaskStatus::InProgress | TaskStatus::Rejected | TaskStatus::Cancelled
        ),
        TaskStatus::InProgress => matches!(
            to,
            TaskStatus::Pending | TaskStatus::Submitted | TaskStatus::Cancelled
        ),
        TaskStatus::Submitted => matches!(to, TaskStatus::Done | TaskStatus::Rejected),
        TaskStatus::Done => matches!(to, TaskStatus::InProgress),
        TaskStatus::Rejected => {
            matches!(to, TaskStatus::Pending | TaskStatus::InProgress | TaskStatus::Cancelled)
        }
        TaskStatus::Cancelled => false,
    };
    if !allowed {
        return Err(PmError::InvalidStateTransition {
            task_id: task_id.to_string(),
            from: from.board_column().to_string(),
            to: to.board_column().to_string(),
        });
    }
    Ok(())
}

/// **创建审批闸门**：Agent 创建的任务（`review_status=pending`）未经人类
/// 批准前，不得进入 `in_progress`。
///
/// 挂在**所有**进入 `in_progress` 的写路径上——`claim_task` 与
/// `update_task(status=in_progress)`——因为只堵一条等于没堵：Agent 换个
/// 工具就绕过去了（设计 §4 "需人类 `review` 通过才进入正式流转"）。
///
/// 只卡 `Pending` 一种：`Rejected` 仍放行，那是设计 D-1 明确的
/// "驳回后 Agent 可重新自领重做"路径。
///
/// ponytail: 人类操作面同样受此闸门约束（REST `PATCH status`）。这是有意的
/// ——审批是一个显式的、可审计的写入动作（`review_status=approved` + 记
/// `updated_at`），比在闸门里开一个人类特权后门更简单也更可查。代价：人类
/// 想强推一个 Agent 建的垃圾任务，得先点一次"批准"再改状态。
fn ensure_creation_approved(task: &Task) -> Result<()> {
    if task.review_status == ReviewStatus::Pending && task.status == TaskStatus::Pending {
        return Err(PmError::ReviewNotApproved {
            task_id: task.id.to_string(),
        });
    }
    Ok(())
}

fn priority_rank(p: Priority) -> u8 {
    match p {
        Priority::Urgent => 0,
        Priority::High => 1,
        Priority::Normal => 2,
        Priority::Low => 3,
    }
}

// ────────────────────────────────────────────────────────────────────────────
// PmStore impl for TreePmStore
// ────────────────────────────────────────────────────────────────────────────

#[async_trait]
impl PmStore for TreePmStore {
    // ── Projects ───────────────────────────────────────────────────

    async fn create_project_as(
        &self,
        input: CreateProject,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Project> {
        let id = ProjectId::generate();
        let now = Utc::now();
        // 创建者自动成为成员（ADR-076 §决策 11）：否则创建者建项目后无法
        // 给自己指派任务（联动指派要求 assignee ∈ members），形成死锁。
        // 人类与 Agent 创建者都入 members，按 `kind` 区分身份（无特例）。
        let members = vec![ProjectMember {
            instance_id: created_by.to_string(),
            kind: created_by_kind,
            added_at: now,
        }];
        let project = Project {
            id: id.clone(),
            title: input.title,
            description: input.description,
            status: ProjectStatus::Active,
            created_by: created_by.to_string(),
            owner_id: None,
            created_at: now,
            updated_at: now,
            metadata: input.metadata,
            members,
        };
        let dir = self.project_dir(&id);
        fs::create_dir_all(dir.join("tasks")).await?;
        atomic_write_json(&self.project_json_path(&id), &project).await?;
        tracing::info!(project_id = %id, "created project");
        Ok(project)
    }

    async fn get_project(&self, id: &ProjectId) -> Result<Option<Project>> {
        let path = self.project_json_path(id);
        if !fs::try_exists(&path).await? {
            return Ok(None);
        }
        let project = read_json(&path).await?;
        Ok(Some(project))
    }

    async fn list_projects(&self) -> Result<Vec<Project>> {
        let mut result = Vec::new();
        let mut entries = fs::read_dir(&self.projects_dir).await?;
        while let Some(e) = entries.next_entry().await? {
            if !e.file_type().await?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("p-") {
                continue;
            }
            let json_path = e.path().join("project.json");
            if !fs::try_exists(&json_path).await? {
                continue;
            }
            match read_json::<Project>(&json_path).await {
                Ok(p) => result.push(p),
                Err(err) => {
                    tracing::warn!(project = %name, error = %err, "skipping unreadable project.json");
                }
            }
        }
        result.sort_by_key(|p| p.created_at);
        Ok(result)
    }

    async fn update_project(&self, id: &ProjectId, input: UpdateProject) -> Result<Project> {
        let path = self.project_json_path(id);
        if !fs::try_exists(&path).await? {
            return Err(PmError::ProjectNotFound(id.to_string()));
        }
        let mut project: Project = read_json(&path).await?;
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
        atomic_write_json(&path, &project).await?;
        Ok(project)
    }

    async fn delete_project(&self, id: &ProjectId, _cascade: bool) -> Result<()> {
        let dir = self.project_dir(id);
        if !fs::try_exists(&dir).await? {
            return Err(PmError::ProjectNotFound(id.to_string()));
        }
        let ts = Utc::now().format("%Y%m%dT%H%M%SZ");
        let trash_path = self.trash_dir.join(format!("{}.archived-{}", id, ts));
        rename_or_fallback(&dir, &trash_path).await?;
        tracing::info!(project_id = %id, trash = %trash_path.display(), "project archived");

        // 清理该项目全部任务索引（含附件 / 依赖引用）
        let ids: Vec<TaskId> = {
            let index = self.index.read();
            index
                .by_id
                .iter()
                .filter(|(_, e)| &e.project_id == id)
                .map(|(tid, _)| tid.clone())
                .collect()
        };
        let mut index = self.index.write();
        for tid in ids {
            index.remove(&tid);
            index.remove_dependent_refs(&tid);
            index.remove_blocker(&tid);
            index.remove_task_attachments(&tid);
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
        let path = self.project_json_path(project_id);
        let mut project: Project = if fs::try_exists(&path).await? {
            read_json(&path).await?
        } else {
            return Err(PmError::ProjectNotFound(project_id.to_string()));
        };
        if project.members.iter().any(|m| m.instance_id == instance_id) {
            return Err(PmError::MemberAlreadyExists(instance_id.to_string()));
        }
        project.members.push(ProjectMember {
            instance_id: instance_id.to_string(),
            kind,
            added_at: Utc::now(),
        });
        project.updated_at = Utc::now();
        atomic_write_json(&path, &project).await?;
        tracing::info!(project_id = %project_id, instance_id, "project member added");
        Ok(project)
    }

    async fn remove_project_member(
        &self,
        project_id: &ProjectId,
        instance_id: &str,
    ) -> Result<Project> {
        let path = self.project_json_path(project_id);
        let mut project: Project = if fs::try_exists(&path).await? {
            read_json(&path).await?
        } else {
            return Err(PmError::ProjectNotFound(project_id.to_string()));
        };
        if !project.members.iter().any(|m| m.instance_id == instance_id) {
            return Err(PmError::MemberNotFound(instance_id.to_string()));
        }

        // 显式失败：成员名下仍有未完成任务（pending/in_progress/submitted/rejected）
        // 时拒绝移除——任务要先转走或完成，避免悬挂 assignee。
        let has_open = {
            let index = self.index.read();
            index.by_assignee.get(instance_id).is_some_and(|ids| {
                ids.iter().any(|tid| {
                    index.by_id.get(tid).is_some_and(|e| {
                        e.project_id == *project_id
                            && matches!(
                                e.status,
                                TaskStatus::Pending
                                    | TaskStatus::InProgress
                                    | TaskStatus::Submitted
                                    | TaskStatus::Rejected
                            )
                    })
                })
            })
        };
        if has_open {
            return Err(PmError::MemberHasOpenTasks {
                instance_id: instance_id.to_string(),
                project_id: project_id.to_string(),
            });
        }

        project.members.retain(|m| m.instance_id != instance_id);
        project.updated_at = Utc::now();
        atomic_write_json(&path, &project).await?;
        tracing::info!(project_id = %project_id, instance_id, "project member removed");
        Ok(project)
    }

    async fn transfer_owner(
        &self,
        project_id: &ProjectId,
        new_owner: &str,
    ) -> Result<Project> {
        let path = self.project_json_path(project_id);
        let mut project: Project = if fs::try_exists(&path).await? {
            read_json(&path).await?
        } else {
            return Err(PmError::ProjectNotFound(project_id.to_string()));
        };

        let old_owner = project.owner().to_string();
        if old_owner == new_owner {
            return Err(PmError::BadRequest(
                "the new owner is already the group owner".to_string(),
            ));
        }
        // Transfer target must be an existing human member. Agents cannot own
        // a project group (no inbox / account), and the target must already be
        // in the project so the transfer never invents membership.
        let target_is_human_member = project
            .members
            .iter()
            .any(|m| m.instance_id == new_owner && m.kind == MemberKind::User);
        if !target_is_human_member {
            return Err(PmError::BadRequest(
                "the new owner must be an existing human member".to_string(),
            ));
        }

        // Demote the old owner to a normal member so they keep access and
        // history (and can be removed later through the ordinary flow). A
        // REST-created project whose creator was already removed from
        // `members` re-enters here; if the old owner is still a member,
        // leave them be.
        if !project.members.iter().any(|m| m.instance_id == old_owner) {
            project.members.push(ProjectMember {
                instance_id: old_owner,
                kind: MemberKind::User,
                added_at: Utc::now(),
            });
        }
        project.owner_id = Some(new_owner.to_string());
        project.updated_at = Utc::now();
        atomic_write_json(&path, &project).await?;
        tracing::info!(project_id = %project_id, new_owner, "project group ownership transferred");
        Ok(project)
    }

    // ── Tasks ──────────────────────────────────────────────────────

    async fn create_task_as(
        &self,
        project_id: &ProjectId,
        input: CreateTask,
        created_by: &str,
        created_by_kind: MemberKind,
    ) -> Result<Task> {
        // 校验项目存在 + 联动指派（assignee 非空必须是项目成员）
        let project = self.read_project(project_id).await?;
        if let Some(assignee) = &input.assignee {
            self.ensure_assignee_is_member(&project, assignee)?;
        }

        let task_id = TaskId::generate();
        let now = Utc::now();

        // 确定目标目录 + 深度
        let (dir, depth) = if let Some(parent_id) = &input.parent_task_id {
            parse_task_id(parent_id.as_str())?;
            let parent = self
                .index
                .read()
                .by_id
                .get(parent_id)
                .cloned()
                .ok_or_else(|| PmError::TaskNotFound(parent_id.to_string()))?;
            if &parent.project_id != project_id {
                return Err(PmError::InvalidId(format!(
                    "parent {} is not in project {}",
                    parent_id, project_id
                )));
            }
            let new_depth = parent.depth + 1;
            if new_depth > self.config.max_task_depth {
                return Err(PmError::MaxDepthExceeded {
                    depth: new_depth,
                    max: self.config.max_task_depth,
                });
            }
            // 子任务数量限制（每任务直接子任务 ≤ 1000）
            let children_dir = parent.dir_path.join("children");
            if fs::try_exists(&children_dir).await? {
                let mut count = 0usize;
                let mut rd = fs::read_dir(&children_dir).await?;
                while let Some(_e) = rd.next_entry().await? {
                    count += 1;
                    if count >= 1000 {
                        return Err(PmError::TooManyChildren(parent_id.to_string()));
                    }
                }
            }
            fs::create_dir_all(&children_dir).await?;
            (children_dir.join(task_id.as_str()), new_depth)
        } else {
            let dir = self.task_dir(project_id, &task_id);
            fs::create_dir_all(&dir).await?;
            (dir, 0)
        };

        fs::create_dir_all(dir.join("attachments")).await?;

        // 依赖自环预检（浅层；深层环检测 P3）
        for dep in &input.depends_on {
            parse_task_id(dep.task_id.as_str())?;
            if dep.task_id == task_id {
                return Err(PmError::DependencyCycle(task_id.to_string()));
            }
        }

        // 人类创建 → 直接生效（NotRequired）；Agent 创建 → 待审核（Pending）
        // ADR-076 §决策 11: 按身份类型判定，而非 `"human"` 字面量特例。
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
            depends_on: input.depends_on,
            attachments: vec![],
            result: None,
            created_by: created_by.to_string(),
            created_at: now,
            updated_at: now,
            claimed_at: None,
            submitted_at: None,
        };
        atomic_write_json(&dir.join("task.json"), &task).await?;

        // 更新索引
        let mut index = self.index.write();
        index.insert(
            task_id.clone(),
            TaskEntry {
                project_id: project_id.clone(),
                status: task.status,
                assignee: task.assignee.clone(),
                depth,
                dir_path: dir,
            },
        );
        for dep in &task.depends_on {
            if dep.kind == DependencyKind::Blocks {
                index.add_dependency(task_id.clone(), dep.task_id.clone());
            }
        }
        tracing::info!(task_id = %task_id, project_id = %project_id, depth, "created task");
        Ok(task)
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<Task>> {
        let entry = match self.index.read().by_id.get(id) {
            Some(e) => e.clone(),
            None => return Ok(None),
        };
        let path = entry.dir_path.join("task.json");
        if !fs::try_exists(&path).await? {
            return Ok(None);
        }
        let task = read_json(&path).await?;
        Ok(Some(task))
    }

    async fn find_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>> {
        let candidates: Vec<TaskId> = {
            let index = self.index.read();
            index
                .by_id
                .iter()
                .filter(|(_, e)| {
                    if let Some(pid) = &filter.project_id
                        && &e.project_id != pid
                    {
                        return false;
                    }
                    if let Some(st) = filter.status
                        && e.status != st
                    {
                        return false;
                    }
                    if let Some(a) = &filter.assignee
                        && e.assignee.as_deref() != Some(a.as_str())
                    {
                        return false;
                    }
                    true
                })
                .map(|(id, _)| id.clone())
                .collect()
        };

        let mut result = Vec::new();
        for id in candidates {
            if filter.only_blocked {
                let blocked = self.compute_blocked_by(&id).await?;
                if blocked.is_empty() {
                    continue;
                }
            }
            if let Some(t) = self.get_task(&id).await? {
                result.push(t);
            }
        }

        match filter.sort.unwrap_or_default() {
            TaskSort::CreatedAt => result.sort_by_key(|t| t.created_at),
            TaskSort::UpdatedAt => result.sort_by_key(|t| t.updated_at),
            TaskSort::DueAt => {
                result.sort_by_key(|t| t.due_at.unwrap_or(chrono::DateTime::<Utc>::MAX_UTC))
            }
            TaskSort::Priority => result.sort_by_key(|t| priority_rank(t.priority)),
        }
        Ok(result)
    }

    async fn update_task(&self, id: &TaskId, input: UpdateTask) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

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
            // 审批闸门：绕过 claim_task 直接 PATCH status=in_progress 的路径
            // 同样受管（见 ensure_creation_approved）
            if s == TaskStatus::InProgress {
                ensure_creation_approved(&task)?;
            }
            task.status = s;
        }
        if let Some(p) = input.priority {
            task.priority = p;
        }
        if input.assignee.is_some() {
            // 联动指派：新 assignee（非空）必须是项目成员
            if let Some(new_assignee) = input.assignee.clone().flatten() {
                let project = self.read_project(&entry.project_id).await?;
                self.ensure_assignee_is_member(&project, &new_assignee)?;
            }
            task.assignee = input.assignee.clone().flatten();
        }
        if input.due_at.is_some() {
            task.due_at = input.due_at.flatten();
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
        atomic_write_json(&path, &task).await?;

        // 重建索引条目（状态 / 责任人变化）+ 重建依赖图
        let mut index = self.index.write();
        index.remove(id);
        index.remove_dependent_refs(id);
        index.insert(
            id.clone(),
            TaskEntry {
                project_id: entry.project_id,
                status: task.status,
                assignee: task.assignee.clone(),
                depth: entry.depth,
                dir_path: entry.dir_path,
            },
        );
        for dep in &task.depends_on {
            if dep.kind == DependencyKind::Blocks {
                index.add_dependency(id.clone(), dep.task_id.clone());
            }
        }
        Ok(task)
    }

    async fn delete_task(&self, id: &TaskId, _cascade: bool) -> Result<()> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;

        // 收集子树任务（物理嵌套，级联删除）
        let mut to_remove = vec![id.clone()];
        self.collect_subtree_ids(&entry.dir_path, &mut to_remove).await?;

        // rm -rf 任务目录（子树 + 附件原子级清理）
        fs::remove_dir_all(&entry.dir_path).await?;

        let mut index = self.index.write();
        for tid in to_remove {
            index.remove(&tid);
            index.remove_dependent_refs(&tid);
            index.remove_blocker(&tid);
            index.remove_task_attachments(&tid);
        }
        tracing::info!(task_id = %id, "deleted task (with subtree)");
        Ok(())
    }

    async fn reparent_task(&self, id: &TaskId, input: ReparentTask) -> Result<()> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let old_dir = entry.dir_path.clone();

        // 计算目标目录 + 新深度
        let (target_dir, new_depth) = match &input.new_parent {
            None => {
                // 提升为项目根任务
                let dir = self.task_dir(&entry.project_id, id);
                fs::create_dir_all(dir.parent().expect("tasks dir")).await?;
                (dir, 0)
            }
            Some(parent_id) => {
                parse_task_id(parent_id.as_str())?;
                let parent = self
                    .index
                    .read()
                    .by_id
                    .get(parent_id)
                    .cloned()
                    .ok_or_else(|| PmError::TaskNotFound(parent_id.to_string()))?;
                if parent.project_id != entry.project_id {
                    return Err(PmError::InvalidId(format!(
                        "parent {} is not in project {}",
                        parent_id, entry.project_id
                    )));
                }
                // DFS 防环：目标父不能位于被移动子树内部
                if parent.dir_path.starts_with(&old_dir) {
                    return Err(PmError::CycleDetected {
                        task_id: id.to_string(),
                        parent_id: parent_id.to_string(),
                    });
                }
                let new_depth = parent.depth + 1;
                if new_depth > self.config.max_task_depth {
                    return Err(PmError::MaxDepthExceeded {
                        depth: new_depth,
                        max: self.config.max_task_depth,
                    });
                }
                let children = parent.dir_path.join("children");
                fs::create_dir_all(&children).await?;
                (children.join(id.as_str()), new_depth)
            }
        };

        if target_dir == old_dir {
            return Ok(());
        }

        // mv 目录（0 文件写）
        rename_or_fallback(&old_dir, &target_dir).await?;

        // 更新子树索引（dir_path + depth）
        //
        // 先收集更新项（**不持锁**，纯 fs 遍历），再一次性加锁批量应用——
        // 避免 parking_lot guard 跨 `.await`（非 Send）。
        let mut updates = Vec::new();
        self.collect_subtree_updates(id, &target_dir, new_depth, &mut updates)
            .await?;
        {
            let mut index = self.index.write();
            for (tid, dir, depth) in updates {
                if let Some(e) = index.by_id.get_mut(&tid) {
                    e.dir_path = dir;
                    e.depth = depth;
                }
            }
        }
        tracing::info!(task_id = %id, new_dir = %target_dir.display(), "reparented task");
        Ok(())
    }

    async fn list_children(&self, parent: &TaskId) -> Result<Vec<Task>> {
        let entry = self
            .index
            .read()
            .by_id
            .get(parent)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(parent.to_string()))?;
        let children = entry.dir_path.join("children");
        let mut result = Vec::new();
        if !fs::try_exists(&children).await? {
            return Ok(result);
        }
        let mut entries = fs::read_dir(&children).await?;
        while let Some(e) = entries.next_entry().await? {
            if !e.file_type().await?.is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with("t-") {
                continue;
            }
            let cid = parse_task_id(&name)?;
            if let Some(t) = self.get_task(&cid).await? {
                result.push(t);
            }
        }
        result.sort_by_key(|t| t.created_at);
        Ok(result)
    }

    // ── Lifecycle ──────────────────────────────────────────────────

    async fn claim_task(&self, id: &TaskId, actor: &str) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        validate_transition(task.status, TaskStatus::InProgress, id)?;
        // 审批闸门：Agent 创建但人类尚未批准 → 不可自领（见 ensure_creation_approved）
        ensure_creation_approved(&task)?;

        // 依赖未满足 → 409
        let blocked = self.compute_blocked_by(id).await?;
        if let Some(b) = blocked.first() {
            return Err(PmError::DependencyNotSatisfied {
                task_id: id.to_string(),
                blocker: b.to_string(),
            });
        }

        task.status = TaskStatus::InProgress;
        task.claimed_at = Some(Utc::now());
        // P3: Agent 自领后即成为责任人（修复 P1 遗留——claim 不更新 assignee）。
        // MCP 层已校验调用者 instance_id == assignee instance_id（设计 §9.2 / ADR-073）。
        // 联动指派：claim 会把 actor 写为 assignee，actor 必须是项目成员。
        let project = self.read_project(&entry.project_id).await?;
        self.ensure_assignee_is_member(&project, actor)?;
        task.assignee = Some(actor.to_string());
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;
        self.refresh_entry(&entry, id, task.status, task.assignee.clone());
        Ok(task)
    }

    async fn submit_task(
        &self,
        id: &TaskId,
        text: &str,
        attachment_ids: Vec<AttachmentId>,
        actor: &str,
    ) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        validate_transition(task.status, TaskStatus::Submitted, id)?;

        let now = Utc::now();
        task.status = TaskStatus::Submitted;
        task.submitted_at = Some(now);
        task.result = Some(TaskResult {
            text: text.to_string(),
            attachment_ids,
            // ADR-073: submitted_by 是 agent_instance_id（X-MCP-Actor 注入）
            submitted_by: actor.to_string(),
            submitted_at: now,
        });
        task.updated_at = now;
        atomic_write_json(&path, &task).await?;
        self.refresh_entry(&entry, id, task.status, task.assignee.clone());
        Ok(task)
    }

    /// 人类审核。**两种审核语义**，由任务的 `status` 区分：
    ///
    /// | 任务形态 | `status` | approve | reject |
    /// |---------|---------|---------|--------|
    /// | Agent 刚创建、未开工（`review_status=pending`） | `pending` | 只翻 `review_status=approved`，**`status` 不动** | `status=rejected` + `review_status=rejected` |
    /// | Agent 已提交结果等验收 | `submitted` | `status=done` + `review_status=approved` | `status=rejected` + `review_status=rejected` |
    ///
    /// 两种审核都写 `review_status`，因为 MCP `pm_check_task` 用它回答
    /// "我建的任务批了吗"（manifest 工具语义），而人类操作面的 `status`
    /// 决定卡片落到哪一列。
    ///
    /// 保持 `status=pending` 而非 `done` 很重要：审批只是**授权** Agent
    /// 开工，不代表工作完成——把它跳到 `done` 会让看板出现一个"从未
    /// claim、从未 submit 就已完成"的任务，也堵死了 Agent 自己的
    /// `pm_claim_task` 路径。
    async fn review_task(&self, id: &TaskId, approved: bool, _reviewer: &str) -> Result<Task> {
        let entry = self
            .index
            .read()
            .by_id
            .get(id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        // 创建审批（status=pending）只动 review_status；结果审核才动 status。
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
            task.status = to;
        }
        task.review_status = if approved {
            ReviewStatus::Approved
        } else {
            ReviewStatus::Rejected
        };
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;
        self.refresh_entry(&entry, id, task.status, task.assignee.clone());
        Ok(task)
    }

    async fn closing_task_contexts(&self, task_id: &TaskId) -> Result<Vec<(ProjectId, Task)>> {
        let entry = match self.index.read().by_id.get(task_id) {
            Some(e) => e.clone(),
            None => return Ok(Vec::new()),
        };
        let mut ids = vec![task_id.clone()];
        self.collect_subtree_ids(&entry.dir_path, &mut ids).await?;
        let mut out = Vec::with_capacity(ids.len());
        for tid in ids {
            if let Some(t) = self.get_task(&tid).await? {
                out.push((entry.project_id.clone(), t));
            }
        }
        Ok(out)
    }

    async fn closing_project_contexts(&self, project_id: &ProjectId) -> Result<Vec<Task>> {
        let filter = crate::types::TaskFilter {
            project_id: Some(project_id.clone()),
            ..Default::default()
        };
        self.find_tasks(&filter).await
    }

    async fn compute_blocked_by(&self, task_id: &TaskId) -> Result<Vec<TaskId>> {
        let task = self
            .get_task(task_id)
            .await?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;

        let mut blocked = Vec::new();
        for dep in &task.depends_on {
            if dep.kind != DependencyKind::Blocks {
                continue;
            }
            let status = self.index.read().by_id.get(&dep.task_id).map(|e| e.status);
            match status {
                // 被依赖任务已完成 / 取消 → 不再阻塞
                Some(TaskStatus::Done) | Some(TaskStatus::Cancelled) => {}
                // 未完成或不存在（悬挂引用）→ 阻塞
                _ => blocked.push(dep.task_id.clone()),
            }
        }
        Ok(blocked)
    }

    // ── Attachments ────────────────────────────────────────────────

    async fn list_attachments(&self, task_id: &TaskId) -> Result<Vec<AttachmentMeta>> {
        let task = self
            .get_task(task_id)
            .await?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        Ok(task.attachments)
    }

    async fn get_attachment(&self, att_id: &AttachmentId) -> Result<Option<AttachmentMeta>> {
        let task_id = match self.index.read().by_attachment.get(att_id) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let task = self
            .get_task(&task_id)
            .await?
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        Ok(task.attachments.into_iter().find(|m| &m.id == att_id))
    }

    async fn register_attachment(&self, task_id: &TaskId, meta: AttachmentMeta) -> Result<()> {
        let entry = self
            .index
            .read()
            .by_id
            .get(task_id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        if task.attachments.len() >= self.config.max_attachments_per_task {
            return Err(PmError::TooManyAttachments(task_id.to_string()));
        }
        if task.attachments.iter().any(|m| m.id == meta.id) {
            return Err(PmError::Internal(format!(
                "attachment {} already registered",
                meta.id
            )));
        }

        task.attachments.push(meta.clone());
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;
        self.index
            .write()
            .register_attachment(meta.id, task_id.clone());
        Ok(())
    }

    async fn delete_attachment(&self, att_id: &AttachmentId) -> Result<()> {
        let task_id = self
            .index
            .read()
            .by_attachment
            .get(att_id)
            .cloned()
            .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))?;
        let entry = self
            .index
            .read()
            .by_id
            .get(&task_id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let path = entry.dir_path.join("task.json");
        let mut task: Task = read_json(&path).await?;

        let meta = task
            .attachments
            .iter()
            .find(|m| &m.id == att_id)
            .cloned()
            .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))?;

        task.attachments.retain(|m| &m.id != att_id);
        task.updated_at = Utc::now();
        atomic_write_json(&path, &task).await?;

        // 清理物理文件（attachments/{att_id}/ 目录）
        if let Some(parent) = Path::new(&meta.storage_path).parent() {
            let dir = entry.dir_path.join(parent);
            let _ = fs::remove_dir_all(&dir).await;
        }

        self.index.write().unregister_attachment(att_id);
        Ok(())
    }

    async fn write_attachment(
        &self,
        task_id: &TaskId,
        meta: AttachmentMeta,
        bytes: Vec<u8>,
        thumb_bytes: Option<Vec<u8>>,
    ) -> Result<AttachmentMeta> {
        let entry = self
            .index
            .read()
            .by_id
            .get(task_id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let task_dir = entry.dir_path;

        // 物理目录：{task_dir}/attachments/{att_id}/
        let att_dir = task_dir
            .join("attachments")
            .join(meta.id.as_str());
        fs::create_dir_all(&att_dir).await?;

        // original.{ext} —— storage_path 形如 "attachments/{att_id}/original.{ext}"
        let original_name = Path::new(&meta.storage_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("original.bin");
        fs::write(att_dir.join(original_name), &bytes).await?;

        // thumb.jpg —— 仅图片（thumb_path 形如 "attachments/{att_id}/thumb.jpg"）
        if let (Some(thumb_bytes), Some(thumb_path)) = (thumb_bytes, &meta.thumb_path) {
            let thumb_name = Path::new(thumb_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("thumb.jpg");
            fs::write(att_dir.join(thumb_name), thumb_bytes).await?;
        }

        // 注册元数据（数量上限校验在 register_attachment 内）
        self.register_attachment(task_id, meta.clone()).await?;
        Ok(meta)
    }

    async fn read_attachment_bytes(
        &self,
        att_id: &AttachmentId,
        thumb: bool,
    ) -> Result<Option<(AttachmentMeta, Vec<u8>)>> {
        let task_id = match self.index.read().by_attachment.get(att_id) {
            Some(t) => t.clone(),
            None => return Ok(None),
        };
        let meta = self
            .get_attachment(att_id)
            .await?
            .ok_or_else(|| PmError::AttachmentNotFound(att_id.to_string()))?;

        let rel_path = if thumb {
            meta.thumb_path.clone().ok_or_else(|| {
                PmError::AttachmentNotFound(format!("thumb for {}", att_id))
            })?
        } else {
            meta.storage_path.clone()
        };

        let entry = self
            .index
            .read()
            .by_id
            .get(&task_id)
            .cloned()
            .ok_or_else(|| PmError::TaskNotFound(task_id.to_string()))?;
        let file_path = entry.dir_path.join(&rel_path);
        let bytes = fs::read(&file_path).await?;
        Ok(Some((meta, bytes)))
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 内部辅助：索引条目刷新（状态 / 责任人变化后重建 by_status / by_assignee）
// ────────────────────────────────────────────────────────────────────────────

impl TreePmStore {
    fn refresh_entry(
        &self,
        entry: &TaskEntry,
        id: &TaskId,
        status: TaskStatus,
        assignee: Option<String>,
    ) {
        let mut index = self.index.write();
        index.remove(id);
        index.insert(
            id.clone(),
            TaskEntry {
                project_id: entry.project_id.clone(),
                status,
                assignee,
                depth: entry.depth,
                dir_path: entry.dir_path.clone(),
            },
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// 单元测试
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CreateTask, Priority, TaskType};
    use tempfile::tempdir;

    fn test_config() -> PmConfig {
        let dir = tempdir().unwrap();
        PmConfig {
            data_dir: dir.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        }
    }

    fn create_task_input(title: &str) -> CreateTask {
        CreateTask {
            title: title.to_string(),
            description: String::new(),
            task_type: TaskType::Task,
            priority: Priority::Normal,
            parent_task_id: None,
            depends_on: vec![],
            attachment_ids: vec![],
            assignee: None,
            due_at: None,
        }
    }

    /// 联动指派测试辅助：把 instance 加为项目成员。
    async fn add_member(store: &TreePmStore, pid: &ProjectId, instance: &str) {
        store
            .add_project_member(pid, instance, MemberKind::Agent)
            .await
            .unwrap();
    }

    /// 人类批准一个 Agent 创建的任务（`review_status: pending → approved`）。
    /// 测试里凡是"Agent 建了任务然后要 claim"的都必须先过这一关，否则撞审批闸门。
    async fn approve_creation(store: &TreePmStore, tid: &TaskId) {
        store.review_task(tid, true, "human").await.unwrap();
    }

    #[tokio::test]
    async fn project_crud_roundtrip() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "Demo".into(),
                    description: "d".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(p.title, "Demo");
        assert_eq!(store.get_project(&p.id).await.unwrap().unwrap().id, p.id);
        assert_eq!(store.list_projects().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn create_root_and_child_task() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();

        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        assert_eq!(store.index_entry(&root.id).unwrap().depth, 0);

        let child = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(root.id.clone()),
                    ..create_task_input("child")
                },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 1);
        assert_eq!(store.list_children(&root.id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_task_removes_subtree() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        let child = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(root.id.clone()),
                    ..create_task_input("child")
                },
                "human",
            )
            .await
            .unwrap();

        store.delete_task(&root.id, true).await.unwrap();
        assert!(store.get_task(&root.id).await.unwrap().is_none());
        assert!(store.get_task(&child.id).await.unwrap().is_none());
        assert_eq!(store.indexed_task_count(), 0);
    }

    #[tokio::test]
    async fn reparent_detects_cycle() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let a = store
            .create_task(&p.id, create_task_input("a"), "human")
            .await
            .unwrap();
        let b = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(a.id.clone()),
                    ..create_task_input("b")
                },
                "human",
            )
            .await
            .unwrap();

        // 把 a 移到 b 之下 → 环
        let err = store
            .reparent_task(&a.id, ReparentTask { new_parent: Some(b.id.clone()) })
            .await
            .unwrap_err();
        assert!(matches!(err, PmError::CycleDetected { .. }));
    }

    #[tokio::test]
    async fn lifecycle_claim_submit_review() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        // ADR-073: actor 是 agent_instance_id（UUID）
        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        // 联动指派：claim 会把 actor 写为 assignee，actor 必须是项目成员
        add_member(&store, &p.id, instance).await;
        let t = store
            .create_task(&p.id, create_task_input("t"), instance)
            .await
            .unwrap();
        assert_eq!(t.review_status, ReviewStatus::Pending);

        // 审批闸门：未批准 → claim 409
        let err = store.claim_task(&t.id, instance).await.unwrap_err();
        assert!(
            matches!(err, PmError::ReviewNotApproved { .. }),
            "unapproved agent task must not be claimable, got {err:?}"
        );
        // 同样挡住绕过 claim 的 PATCH status 路径
        let err = store
            .update_task(
                &t.id,
                UpdateTask {
                    status: Some(TaskStatus::InProgress),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, PmError::ReviewNotApproved { .. }),
            "update_task must not bypass the approval gate, got {err:?}"
        );

        // 人类批准创建 → review_status=approved，**status 仍为 pending**（只是授权开工）
        let approved = store.review_task(&t.id, true, "human").await.unwrap();
        assert_eq!(approved.review_status, ReviewStatus::Approved);
        assert_eq!(
            approved.status,
            TaskStatus::Pending,
            "creation approval must not jump the task to done/in_progress"
        );

        let claimed = store.claim_task(&t.id, instance).await.unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);

        let submitted = store
            .submit_task(&t.id, "done", vec![], instance)
            .await
            .unwrap();
        assert_eq!(submitted.status, TaskStatus::Submitted);

        // 结果审核：这次才动 status
        let reviewed = store.review_task(&t.id, true, "human").await.unwrap();
        assert_eq!(reviewed.status, TaskStatus::Done);
        assert_eq!(reviewed.review_status, ReviewStatus::Approved);
    }

    /// 人类**驳回**一个 Agent 新建的（从未开工的）任务 → `status=rejected`
    /// 且 `review_status=rejected`；Agent 之后仍可按设计 D-1 重新自领重做。
    #[tokio::test]
    async fn reject_creation_blocks_then_allows_reclaim() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        add_member(&store, &p.id, instance).await;
        let t = store
            .create_task(&p.id, create_task_input("junk"), instance)
            .await
            .unwrap();

        let rejected = store.review_task(&t.id, false, "human").await.unwrap();
        assert_eq!(rejected.status, TaskStatus::Rejected);
        assert_eq!(rejected.review_status, ReviewStatus::Rejected);

        // 设计 D-1：驳回后仍可自领重做（闸门只卡 review_status=pending）
        let reclaimed = store.claim_task(&t.id, instance).await.unwrap();
        assert_eq!(reclaimed.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn rebuild_index_recovers_all_tasks() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject {
                    title: "P".into(),
                    description: "".into(),
                    metadata: Default::default(),
                },
                "human",
            )
            .await
            .unwrap();
        let a = store
            .create_task(&p.id, create_task_input("a"), "human")
            .await
            .unwrap();
        let b = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(a.id.clone()),
                    ..create_task_input("b")
                },
                "human",
            )
            .await
            .unwrap();

        // 模拟崩溃：重建索引
        store.rebuild_index().await.unwrap();
        assert_eq!(store.indexed_task_count(), 2);
        assert_eq!(store.get_task(&b.id).await.unwrap().unwrap().id, b.id);
        assert_eq!(store.index_entry(&b.id).unwrap().depth, 1);
    }

    // ── 负路径覆盖 (ADR-061 P1 补测) ─────────────────────────────
    //
    // 这组测试专门验证设计文档 §21 中的**契约不变式**在反例下也成立。
    // 与正路径(成功流)分开,失败时能直接定位是哪条契约被破坏。

    /// 负路径 1:依赖图可计算阻塞关系。
    ///
    /// - T1 依赖 T0(kind=Blocks)
    /// - T0 仍为 Pending(未完成)→ T1 **被阻塞**
    /// - T0 review(Done)→ T1 **不再被阻塞**
    ///
    /// 注:`submit_task` 不检查依赖(P1 软契约,行为由 find_tasks only_blocked 体现),
    /// 所以这里用 `compute_blocked_by` 验证阻塞检测正确。
    #[tokio::test]
    async fn dependency_blocks_until_done() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let t0 = store
            .create_task(&p.id, create_task_input("blocker"), "human")
            .await
            .unwrap();
        let t1 = store
            .create_task(
                &p.id,
                CreateTask {
                    depends_on: vec![crate::types::Dependency {
                        task_id: t0.id.clone(),
                        kind: DependencyKind::Blocks,
                    }],
                    ..create_task_input("blocked")
                },
                "human",
            )
            .await
            .unwrap();

        // T0 Pending → T1 被 T0 阻塞
        let blocked = store.compute_blocked_by(&t1.id).await.unwrap();
        assert_eq!(blocked, vec![t0.id.clone()], "T1 must be blocked by T0 while T0 is open");

        // 完成 T0 → T1 不再被阻塞
        // 联动指派：claim 会把 actor 写为 assignee，actor 必须是项目成员
        add_member(&store, &p.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await;
        let claimed = store.claim_task(&t0.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await.unwrap();
        assert_eq!(claimed.status, TaskStatus::InProgress);
        let submitted = store
            .submit_task(&t0.id, "done", vec![], "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d")
            .await
            .unwrap();
        assert_eq!(submitted.status, TaskStatus::Submitted);
        let reviewed = store.review_task(&t0.id, true, "human").await.unwrap();
        assert_eq!(reviewed.status, TaskStatus::Done);

        let blocked_after = store.compute_blocked_by(&t1.id).await.unwrap();
        assert!(blocked_after.is_empty(), "T1 must NOT be blocked after T0 is Done");
    }

    /// 负路径 2:深度超限被拒绝(`max_task_depth` 配置生效)。
    ///
    /// 实现契约:`if new_depth > max_task_depth` 严格大于,所以
    /// - max=1 允许 depth 0(根)和 depth 1(子):1 > 1 = false
    /// - max=1 拒绝 depth 2(孙):2 > 1 = true
    /// 这里设 max=1,先确认 child(depth=1)合法,再确认 grandchild(depth=2)被拒。
    #[tokio::test]
    async fn create_task_rejects_over_max_depth() {
        let mut cfg = test_config();
        cfg.max_task_depth = 1;
        let store = TreePmStore::new(cfg).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();

        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        assert_eq!(store.index_entry(&root.id).unwrap().depth, 0);

        // child(depth=1) 在 max=1 下合法(1 > 1 = false)
        let child = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(root.id.clone()),
                    ..create_task_input("child")
                },
                "human",
            )
            .await
            .expect("child (depth=1) should be allowed when max=1");
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 1);

        // grandchild(depth=2 > max=1) 必须被拒绝
        let err = store
            .create_task(
                &p.id,
                CreateTask {
                    parent_task_id: Some(child.id.clone()),
                    ..create_task_input("grandchild")
                },
                "human",
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, PmError::MaxDepthExceeded { depth: 2, max: 1 }),
            "expected MaxDepthExceeded {{ depth: 2, max: 1 }}, got {err:?}"
        );
    }

    /// 负路径 3:UpdateTask.assignee = Some(None) 清空责任人,Some(Some(s)) 设置。
    ///
    /// 类型签名 `assignee: Option<Option<String>>` 表达三态:
    /// - `None` → 不修改
    /// - `Some(None)` → 清空为 null
    /// - `Some(Some(s))` → 设为 s
    ///
    /// 注:`claim_task` 在 P1 阶段**不**自动写 assignee 字段(只设 claimed_at),
    /// 所以这里用 update_task 直接设,覆盖三态契约即可。
    #[tokio::test]
    async fn update_task_can_clear_assignee() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        // 联动指派：update 设置 assignee 需要成员身份
        add_member(&store, &p.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await;
        let t = store
            .create_task(&p.id, create_task_input("t"), "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d")
            .await
            .unwrap();
        assert!(t.assignee.is_none(), "fresh task has no assignee");

        // Some(Some(s)) → 设置
        let set = store
            .update_task(
                &t.id,
                UpdateTask {
                    assignee: Some(Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(set.assignee.as_deref(), Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"));
        assert_eq!(store.index_entry(&t.id).unwrap().assignee.as_deref(), Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d"));

        // Some(None) → 清空
        let cleared = store
            .update_task(
                &t.id,
                UpdateTask {
                    assignee: Some(None),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(cleared.assignee.is_none(), "assignee must be cleared to None");
        assert_eq!(store.index_entry(&t.id).unwrap().assignee, None);

        // None → 不修改(已为 None,验证 round-trip)
        let untouched = store
            .update_task(
                &t.id,
                UpdateTask {
                    title: Some("renamed".to_string()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(untouched.assignee.is_none(), "None assignee must NOT be touched when UpdateTask.assignee=None");
        assert_eq!(untouched.title, "renamed");
    }

    /// 负路径 4:delete_task 的 `cascade` 参数在 P1 阶段被忽略(总是级联删除子树)。
    ///
    /// **承认 P1 行为**:接口保留 `cascade` 参数(为 P2+ SQL back-end 预留语义),
    /// 当前实现总是 rm -rf 整个子树。这是一个 P1 阶段已知限制。
    ///
    /// 测试目的:**锁定当前行为**,未来切 SQL 时这个测试会失败,作为迁移提醒。
    #[tokio::test]
    async fn delete_task_ignores_cascade_flag_in_p1() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        let child = store
            .create_task(
                &p.id,
                CreateTask { parent_task_id: Some(root.id.clone()), ..create_task_input("child") },
                "human",
            )
            .await
            .unwrap();

        // P1 阶段 cascade=false 仍级联删除子树
        store.delete_task(&root.id, false).await.unwrap();

        assert!(store.get_task(&root.id).await.unwrap().is_none());
        assert!(
            store.get_task(&child.id).await.unwrap().is_none(),
            "child must be removed even with cascade=false in P1 (acknowledged limitation)"
        );
        assert_eq!(store.indexed_task_count(), 0);
    }

    /// 负路径 5:reparent_task(new_parent=None) 把子任务提升为项目根任务。
    #[tokio::test]
    async fn reparent_to_root_promotes_task() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let root = store
            .create_task(&p.id, create_task_input("root"), "human")
            .await
            .unwrap();
        let child = store
            .create_task(
                &p.id,
                CreateTask { parent_task_id: Some(root.id.clone()), ..create_task_input("child") },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 1);

        // 提升为根
        store
            .reparent_task(
                &child.id,
                ReparentTask { new_parent: None },
            )
            .await
            .unwrap();

        // depth=0,不再属于任何 root 的子
        assert_eq!(store.index_entry(&child.id).unwrap().depth, 0);
        assert!(store.list_children(&root.id).await.unwrap().is_empty());
        // 任务本身仍存在
        assert!(store.get_task(&child.id).await.unwrap().is_some());
    }

    /// 负路径 6:find_tasks 多字段过滤(status + assignee + project_id 同时生效)。
    ///
    /// 构造 4 个任务,在不同 project / 不同 status / 不同 assignee 下,
    /// 验证所有过滤条件 AND 生效。
    #[tokio::test]
    async fn find_tasks_combined_filter() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p1 = store
            .create_project(
                CreateProject { title: "P1".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let p2 = store
            .create_project(
                CreateProject { title: "P2".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();

        // 联动指派：claim/assignee 需要成员身份
        add_member(&store, &p1.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await;
        add_member(&store, &p1.id, "agent-y").await;
        add_member(&store, &p2.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await;

        let t_p1_inprog_x = store
            .create_task(&p1.id, create_task_input("p1-inprog-x"), "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d")
            .await
            .unwrap();
        // claim_task 在 P1 不写 assignee,显式 update_task 设之(覆盖三态契约)
        approve_creation(&store, &t_p1_inprog_x.id).await;
        store.claim_task(&t_p1_inprog_x.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await.unwrap();
        store
            .update_task(
                &t_p1_inprog_x.id,
                UpdateTask {
                    assignee: Some(Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let t_p1_pending_y = store
            .create_task(&p1.id, create_task_input("p1-pending-y"), "agent-y")
            .await
            .unwrap();
        store
            .update_task(
                &t_p1_pending_y.id,
                UpdateTask {
                    assignee: Some(Some("agent-y".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let t_p2_inprog_x = store
            .create_task(&p2.id, create_task_input("p2-inprog-x"), "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d")
            .await
            .unwrap();
        approve_creation(&store, &t_p2_inprog_x.id).await;
        store.claim_task(&t_p2_inprog_x.id, "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d").await.unwrap();
        store
            .update_task(
                &t_p2_inprog_x.id,
                UpdateTask {
                    assignee: Some(Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d".to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        // 同时过滤 P1 + InProgress + instance → 仅 t_p1_inprog_x
        let filter = TaskFilter {
            project_id: Some(p1.id.clone()),
            status: Some(TaskStatus::InProgress),
            assignee: Some("3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d".to_string()),
            ..Default::default()
        };
        let results = store.find_tasks(&filter).await.unwrap();
        assert_eq!(results.len(), 1, "only P1+InProgress+instance matches");
        assert_eq!(results[0].id, t_p1_inprog_x.id);

        // 仅 project_id=P1 → 2 个
        let p1_only = store
            .find_tasks(&TaskFilter { project_id: Some(p1.id.clone()), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(p1_only.len(), 2);

        // 无过滤 → 全部 3 个
        let all = store.find_tasks(&TaskFilter::default()).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    /// 负路径 7:delete_attachment 反向清理(task.json + 索引 + 物理文件)。
    ///
    /// 验证三个清理动作都发生:
    /// 1. `task.attachments` 数组移除该 attachment
    /// 2. `index.by_attachment` 反注册
    /// 3. `{task_dir}/attachments/{att_id}/` 物理目录被删除
    #[tokio::test]
    async fn delete_attachment_cleans_index_and_disk() {
        use crate::types::{AttachmentId, AttachmentKind, AttachmentMeta};
        use std::path::Path;
        use tempfile::tempdir;

        // 用具名临时目录(便于断言物理路径)
        let dir = tempdir().unwrap();
        let cfg = PmConfig {
            data_dir: dir.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        };
        let store = TreePmStore::new(cfg).await.unwrap();

        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let t = store
            .create_task(&p.id, create_task_input("t"), "human")
            .await
            .unwrap();

        // 构造附件元数据(storage_path 相对 task_dir)
        let att_id = AttachmentId::parse("att-12345678").unwrap();
        let meta = AttachmentMeta {
            id: att_id.clone(),
            filename: "test.png".into(),
            kind: AttachmentKind::Image,
            content_type: "image/png".into(),
            size: 1024,
            sha256: "deadbeef".into(),
            storage_path: format!("attachments/{att_id}/original.png"),
            thumb_path: None,
            width: None,
            height: None,
            uploaded_by: "human".into(),
            uploaded_at: chrono::Utc::now(),
        };

        store.register_attachment(&t.id, meta.clone()).await.unwrap();

        // 准备物理文件:写一个 dummy 文件到 attachments/{att_id}/original.png
        let entry = store.index_entry(&t.id).unwrap();
        let att_dir = entry.dir_path.join("attachments").join(att_id.as_str());
        std::fs::create_dir_all(&att_dir).unwrap();
        let file_path = att_dir.join("original.png");
        std::fs::write(&file_path, b"fake-png-bytes").unwrap();
        assert!(file_path.exists(), "precondition: attachment file must exist on disk");

        // 反向索引已注册
        assert!(store.get_attachment(&att_id).await.unwrap().is_some());

        // 执行删除
        store.delete_attachment(&att_id).await.unwrap();

        // 1) task.json 不再含此 attachment
        let task_after = store.get_task(&t.id).await.unwrap().unwrap();
        assert!(
            task_after.attachments.iter().all(|a| a.id != att_id),
            "task.attachments must no longer contain {att_id}"
        );

        // 2) 反向索引已清
        assert!(
            store.get_attachment(&att_id).await.unwrap().is_none(),
            "by_attachment index must no longer resolve {att_id}"
        );

        // 3) 物理目录被删除
        assert!(
            !att_dir.exists(),
            "physical attachment directory must be removed, but still exists at {}",
            att_dir.display()
        );
        // 验证父目录仍存在(只删除 attachments/{att_id}/ 子目录,不动 attachments/)
        let parent_attachments = entry.dir_path.join("attachments");
        assert!(parent_attachments.exists(), "attachments/ parent must remain");
        // Path::new(&meta.storage_path).parent() = "attachments/{att_id}",删除范围正确
        let _ = Path::new(&meta.storage_path); // 仅为路径 API 编译检查
    }

    // ── 项目成员 + 联动指派 ──────────────────────────────────────────

    /// agent / human 创建项目 → 创建者都自动成为成员（ADR-076 §决策 11），
    /// 按 `kind` 区分身份。
    #[tokio::test]
    async fn creator_is_auto_member_for_agent_and_human() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let inst = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";

        let p_agent = store
            .create_project(
                CreateProject { title: "by-agent".into(), description: "".into(), metadata: Default::default() },
                inst,
            )
            .await
            .unwrap();
        assert_eq!(p_agent.members.len(), 1);
        assert_eq!(p_agent.members[0].instance_id, inst);
        assert_eq!(p_agent.members[0].kind, MemberKind::Agent);

        // ADR-076 §决策 11: 人类创建者也入 members（成员化，无特例）。
        let p_human = store
            .create_project(
                CreateProject { title: "by-human".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(p_human.members.len(), 1, "human creator is now a member");
        assert_eq!(p_human.members[0].instance_id, "human");
        assert_eq!(p_human.members[0].kind, MemberKind::User);
    }

    /// ADR-076 §决策 11: `create_*_as` 的显式 `kind` 驱动 review_status 与
    /// 成员 kind —— 真实 user_id（UUID）不会被误判为 Agent。
    #[tokio::test]
    async fn explicit_kind_drives_review_status_and_member_kind() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let user_id = "9b1c0f2e-1111-4a2b-8c3d-000000000001"; // 形似 UUID 的 user_id
        let instance = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";

        // User 创建项目（显式 User）→ 成员 kind=User；其任务 review_status=NotRequired。
        let p = store
            .create_project_as(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                user_id,
                MemberKind::User,
            )
            .await
            .unwrap();
        assert_eq!(p.members[0].kind, MemberKind::User);

        let t_user = store
            .create_task_as(&p.id, create_task_input("by-user"), user_id, MemberKind::User)
            .await
            .unwrap();
        assert_eq!(
            t_user.review_status,
            ReviewStatus::NotRequired,
            "User-created task needs no review"
        );

        // Agent 创建任务（显式 Agent）→ review_status=Pending。
        let t_agent = store
            .create_task_as(&p.id, create_task_input("by-agent"), instance, MemberKind::Agent)
            .await
            .unwrap();
        assert_eq!(
            t_agent.review_status,
            ReviewStatus::Pending,
            "Agent-created task needs review"
        );
    }

    /// 成员增删往返 + 重复添加 409 + 移除不存在成员 404。
    #[tokio::test]
    async fn member_add_remove_roundtrip() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let inst = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";

        let updated = store
            .add_project_member(&p.id, inst, MemberKind::Agent)
            .await
            .unwrap();
        // ADR-076 §决策 11: 创建者（human）已是成员，故加 inst 后为 2 人。
        assert_eq!(updated.members.len(), 2);
        assert!(updated.members.iter().any(|m| m.instance_id == inst));
        // 持久化后重新读取
        let reloaded = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(reloaded.members.len(), 2, "member must persist to project.json");

        // 重复添加 → MemberAlreadyExists
        let err = store
            .add_project_member(&p.id, inst, MemberKind::Agent)
            .await
            .unwrap_err();
        assert!(matches!(err, PmError::MemberAlreadyExists(_)));

        // 移除 → inst 消失，仅剩创建者
        let updated = store.remove_project_member(&p.id, inst).await.unwrap();
        assert_eq!(updated.members.len(), 1);
        assert!(!updated.members.iter().any(|m| m.instance_id == inst));

        // 移除不存在的成员 → MemberNotFound
        let err = store.remove_project_member(&p.id, inst).await.unwrap_err();
        assert!(matches!(err, PmError::MemberNotFound(_)));

        // 项目不存在 → ProjectNotFound
        let ghost = crate::types::ProjectId("p-00000000".into());
        let err = store
            .add_project_member(&ghost, inst, MemberKind::Agent)
            .await
            .unwrap_err();
        assert!(matches!(err, PmError::ProjectNotFound(_)));
    }

    /// 转让群主：`owner_id` 生效、`owner()` 跟随、老群主降级但保留
    /// 为成员；向自己 / 非成员 / Agent 成员转让均被拒（BadRequest）。
    #[tokio::test]
    async fn transfer_owner_demotes_old_owner_and_moves_owner_id() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        // New owner must be an existing human member.
        let alice = "a11ce111-7e4b-4d2a-b6f1-1a91b07e4c2d";
        store.add_project_member(&p.id, alice, MemberKind::User).await.unwrap();

        let updated = store.transfer_owner(&p.id, alice).await.unwrap();
        assert_eq!(updated.owner_id.as_deref(), Some(alice));
        assert_eq!(updated.owner(), alice, "owner() must follow owner_id");
        // Creator ("human") is demoted but stays a member (keeps access/history).
        assert!(updated.members.iter().any(|m| m.instance_id == "human"));
        assert!(updated.members.iter().any(|m| m.instance_id == alice));
        // Participant set includes both the new owner and the demoted old one.
        let parts = crate::api::projects::group_participants(&updated);
        assert!(parts.contains(&alice.to_string()), "{parts:?}");
        assert!(parts.contains(&"human".to_string()), "{parts:?}");

        // Persisted to project.json.
        let reloaded = store.get_project(&p.id).await.unwrap().unwrap();
        assert_eq!(reloaded.owner_id.as_deref(), Some(alice));

        // Transfer to the current owner → BadRequest.
        let err = store.transfer_owner(&p.id, alice).await.unwrap_err();
        assert!(matches!(err, PmError::BadRequest(_)));

        // Transfer to a non-member → BadRequest.
        let outsider = "00000000-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let err = store.transfer_owner(&p.id, outsider).await.unwrap_err();
        assert!(matches!(err, PmError::BadRequest(_)));

        // Transfer to an Agent member → BadRequest (agents cannot own).
        let bot = "b0tb0000-7e4b-4d2a-b6f1-1a91b07e4c2d";
        store.add_project_member(&p.id, bot, MemberKind::Agent).await.unwrap();
        let err = store.transfer_owner(&p.id, bot).await.unwrap_err();
        assert!(matches!(err, PmError::BadRequest(_)));
    }

    /// 联动指派：create/update/claim 的 assignee（非空）必须是项目成员。
    #[tokio::test]
    async fn assignee_must_be_project_member() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let member = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        let outsider = "5d2e1100-7e4b-4d2a-b6f1-1a91b07e4c2d";
        add_member(&store, &p.id, member).await;

        // create_task: 非成员 assignee → 400
        let err = store
            .create_task(
                &p.id,
                CreateTask {
                    assignee: Some(outsider.to_string()),
                    ..create_task_input("t")
                },
                "human",
            )
            .await
            .unwrap_err();
        assert!(matches!(err, PmError::AssigneeNotProjectMember { .. }));

        // create_task: 成员 assignee → 成功
        let t = store
            .create_task(
                &p.id,
                CreateTask {
                    assignee: Some(member.to_string()),
                    ..create_task_input("t-ok")
                },
                "human",
            )
            .await
            .unwrap();
        assert_eq!(t.assignee.as_deref(), Some(member));

        // update_task: 非成员 assignee → 400
        let err = store
            .update_task(
                &t.id,
                UpdateTask {
                    assignee: Some(Some(outsider.to_string())),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, PmError::AssigneeNotProjectMember { .. }));

        // update_task: 清空 assignee → 允许（非空才校验）
        let cleared = store
            .update_task(&t.id, UpdateTask { assignee: Some(None), ..Default::default() })
            .await
            .unwrap();
        assert!(cleared.assignee.is_none());

        // claim: 非成员 actor → 400（claim 会把 actor 写为 assignee）
        let t2 = store
            .create_task(
                &p.id,
                CreateTask {
                    assignee: Some(member.to_string()),
                    ..create_task_input("t2")
                },
                "human",
            )
            .await
            .unwrap();
        let err = store.claim_task(&t2.id, outsider).await.unwrap_err();
        assert!(matches!(err, PmError::AssigneeNotProjectMember { .. }));
        // 任务状态未被改变
        let after = store.get_task(&t2.id).await.unwrap().unwrap();
        assert_eq!(after.status, TaskStatus::Pending);
    }

    /// 移除成员时若其名下仍有未完成任务 → 409；任务 done 后可移除。
    #[tokio::test]
    async fn remove_member_with_open_tasks_conflicts() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();
        let inst = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
        add_member(&store, &p.id, inst).await;

        // 有 pending 任务 → 移除 409
        let t = store
            .create_task(
                &p.id,
                CreateTask {
                    assignee: Some(inst.to_string()),
                    ..create_task_input("open")
                },
                "human",
            )
            .await
            .unwrap();
        let err = store.remove_project_member(&p.id, inst).await.unwrap_err();
        assert!(matches!(err, PmError::MemberHasOpenTasks { .. }));

        // 任务走完生命周期（claim → submit → review done）后可移除
        store.claim_task(&t.id, inst).await.unwrap();
        store.submit_task(&t.id, "done", vec![], inst).await.unwrap();
        store.review_task(&t.id, true, "human").await.unwrap();
        let updated = store.remove_project_member(&p.id, inst).await.unwrap();
        // ADR-076 §决策 11: 移除 inst 后仅剩创建者（human）成员。
        assert!(
            !updated.members.iter().any(|m| m.instance_id == inst),
            "member removable after tasks done"
        );
    }

    /// 旧 `project.json` 无 `members` 字段 → 读出空数组（零迁移）。
    #[tokio::test]
    async fn old_project_json_without_members_defaults_empty() {
        let store = TreePmStore::new(test_config()).await.unwrap();
        let p = store
            .create_project(
                CreateProject { title: "P".into(), description: "".into(), metadata: Default::default() },
                "human",
            )
            .await
            .unwrap();

        // 手工改写 project.json 为"旧格式"（无 members 字段）
        let path = store.project_json_path(&p.id);
        let legacy = serde_json::json!({
            "id": p.id.as_str(),
            "title": p.title,
            "description": p.description,
            "status": "active",
            "created_by": "human",
            "created_at": p.created_at,
            "updated_at": p.updated_at,
            "metadata": {}
        });
        tokio::fs::write(&path, serde_json::to_string(&legacy).unwrap())
            .await
            .unwrap();

        let reloaded = store.get_project(&p.id).await.unwrap().unwrap();
        assert!(reloaded.members.is_empty(), "legacy project.json must deserialize with empty members");
    }
}
