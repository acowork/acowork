//! acowork-pm 核心领域类型。
//!
//! ## 类型清单
//!
//! - **强类型 ID**：`ProjectId` / `TaskId` / `AttachmentId`（newtype 包装字符串，避免混用）
//! - **枚举**：`ProjectStatus` / `TaskStatus` / `TaskType` / `ReviewStatus` / `AttachmentKind` / `DependencyKind`
//! - **领域实体**：`Project` / `Task` / `AttachmentMeta`
//! - **输入类型**：`CreateProject` / `CreateTask` / `UpdateTask` / `ReparentTask` / `TaskFilter`
//!
//! ## 设计原则
//!
//! 1. **零冗余字段**：`Task` 不含 `parent_id` / `subtask_ids` / `subtask_count`。
//!    父子关系完全靠物理位置表达（见 [`crate::store`]）。
//! 2. **派生字段仅 API 返回**：`is_blocked` / `blocked_by` 在响应层由 `depends_on` 实时计算，
//!    **不**持久化到 `task.json`。
//! 3. **强类型 ID**：构造时强制格式校验（白名单前缀 + 字符集），运行时不可能出现非法 ID。

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// ────────────────────────────────────────────────────────────────────────────
// 强类型 ID（newtype + 格式校验）
// ────────────────────────────────────────────────────────────────────────────

/// `define_id!` —— 为三种 ID（Project/Task/Attachment）生成同构代码。
///
/// ID 格式：`{prefix}-{uuid8}` 至 `{prefix}-{uuid32}`，字符集 `[a-zA-Z0-9-]`，
/// 长度 3-64。前缀硬编码以与人类可读性 + grep 友好。
macro_rules! define_id {
    ($name:ident, $prefix:literal, $kind:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// 服务端生成新 ID（UUID v4，取前 8 字符 + 前缀）。
            pub fn generate() -> Self {
                let uuid_short = Uuid::new_v4().simple().to_string();
                let short = &uuid_short[..8];
                Self(format!("{}{}", $prefix, short))
            }

            /// 解析已存在的 ID。失败返回 [`crate::error::PmError::InvalidId`]。
            pub fn parse(s: &str) -> Result<Self, $name> {
                if s.len() < 3 || s.len() > 64 || !s.starts_with($prefix) {
                    return Err(Self(s.to_string()));
                }
                let suffix = &s[$prefix.len()..];
                if !suffix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err(Self(s.to_string()));
                }
                Ok(Self(s.to_string()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl std::str::FromStr for $name {
            type Err = crate::error::PmError;
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Self::parse(s).map_err(|_| {
                    crate::error::PmError::InvalidId(format!("{}: {}", $kind, s))
                })
            }
        }
    };
}

define_id!(ProjectId, "p-", "project");
define_id!(TaskId, "t-", "task");
define_id!(AttachmentId, "att-", "attachment");

// ────────────────────────────────────────────────────────────────────────────
// 枚举
// ────────────────────────────────────────────────────────────────────────────

/// 项目状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStatus {
    /// 进行中
    Active,
    /// 已归档（只读，不可新增任务）
    Archived,
    /// 已完成
    Completed,
}

/// 任务状态（看板四列）。
///
/// 状态流转图见 [`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §3.3。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// 待处理（看板第一列）
    Pending,
    /// 进行中（看板第二列，由 Agent claim 进入）
    InProgress,
    /// 已提交待审核（看板第三列，Agent submit 后进入）
    Submitted,
    /// 已完成（看板第四列，review 通过）
    Done,
    /// 已拒绝（review 未通过，可重新提交）
    Rejected,
    /// 已取消
    Cancelled,
}

impl TaskStatus {
    /// 看板列名（用于 UI 分组）。
    pub fn board_column(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Submitted => "submitted",
            TaskStatus::Done => "done",
            TaskStatus::Rejected => "rejected",
            TaskStatus::Cancelled => "cancelled",
        }
    }
}

/// 任务类型。
///
/// - `task`：普通任务
/// - `bug`：缺陷单（自动染色 priority=high）
/// - `feature`：功能请求
/// - `chore`：杂项
/// - `checkpoint`：人类检查点（完成时强制 review）
/// - `milestone`：里程碑标记
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskType {
    #[default]
    Task,
    Bug,
    Feature,
    Chore,
    Checkpoint,
    Milestone,
}

impl TaskType {
    /// 该类型是否需要强制人类 review（checkpoint 自动启用）。
    pub fn requires_review(&self) -> bool {
        matches!(self, TaskType::Checkpoint | TaskType::Bug)
    }
}

/// 审核状态（仅 [`TaskType::Checkpoint`] / [`TaskType::Bug`] 等需要 review 的任务使用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    /// 不需要 review
    NotRequired,
    /// 待审核
    Pending,
    /// 已通过
    Approved,
    /// 已驳回
    Rejected,
}

/// 附件种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    /// 图片（自动生成缩略图）
    Image,
    /// 其他文件
    File,
}

/// 依赖关系种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// 强依赖：被依赖任务未完成时，本任务不可 claim
    Blocks,
    /// 弱关联：仅展示，不阻塞
    Relates,
    /// 重复：被依赖任务为本任务的副本
    Duplicates,
}

// ────────────────────────────────────────────────────────────────────────────
// 实体：Project
// ────────────────────────────────────────────────────────────────────────────

/// 项目成员身份类型（ADR-076 §决策 11）。
///
/// 人类操作者与 Agent 成员对称：`Agent` → `instance_id`（ADR-073）；
/// `User` → `user_id`（ADR-076）。两者共用 [`ProjectMember::instance_id`]
/// 字段承载（字段名兼容保留），由本枚举区分语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    /// Agent 实例（ADR-073）。旧数据缺 `kind` 时按此解析（零迁移）。
    #[default]
    Agent,
    /// 人类账号（ADR-076）。
    User,
}

impl MemberKind {
    /// 从 actor 值推断身份类型（ADR-076 §决策 10/11）。
    ///
    /// 遗留常量 `"human"` 与 `"unknown"`（`X-Actor` 缺失时的兜底）视为
    /// `User`；其余（UUID instance_id）视为 `Agent`。
    pub fn from_actor(actor: &str) -> Self {
        match actor {
            "human" | "unknown" => Self::User,
            _ => Self::Agent,
        }
    }
}

/// 项目成员（Agent 实例或人类账号，ADR-076 §决策 11）。
///
/// **ADR-073**：`instance_id` 是唯一身份 key（UUID），`agent_id`（包 ID）仅
/// 显示用。成员 = 可被指派为 `task.assignee` 的身份（联动指派不变量：
/// `assignee ∈ ∅ ∪ project.members`，由 store 层强制）。
///
/// **ADR-076 §决策 11**：`kind` 区分 Agent / User，人类操作者成员化——
/// 与 Agent 成员对称，移除 `assignee == "human"` 特例。
///
/// 只存身份 + 加入时间，**不**存显示名/头像快照——展示信息由前端 join
/// agentStore 获取，避免快照过期不一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMember {
    /// Agent 实例 ID（UUID，ADR-073）或 User ID（ADR-076，按 `kind` 解释）。
    pub instance_id: String,
    /// 身份类型。`#[serde(default)]` = `Agent` → 旧 `project.json` 零迁移。
    #[serde(default)]
    pub kind: MemberKind,
    pub added_at: DateTime<Utc>,
}

/// 项目元数据。
///
/// **不含** `tasks` 数组（任务分散存储在 `tasks/` 子目录下）。
/// **不含** `task_ids` 索引（运行时由 [`crate::store::index::TaskIndex`] 维护）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Project {
    pub id: ProjectId,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub status: ProjectStatus,
    /// 创建者身份：Agent 实例 `instance_id`（UUID，ADR-073）、人类账号
    /// `user_id`（ADR-076，REST 面 `X-Actor` 注入）、或 local 模式的哨兵常量
    /// `"human"`。按 [`crate::types::MemberKind::from_actor`] 判别身份。
    pub created_by: String,
    /// 群主（owner）身份。转让群主时**只改此字段**，`created_by`
    /// 保留“谁创建了项目”的审计语义（不变）。为 `null`（旧项目 /
    /// 未转让）时用 [`Project::owner`] 回退到 `created_by`。所有
    /// “把 owner 折入成员/参与者集合”的地方必须走 `owner()`。
    /// `#[serde(default)]` → 旧 `project.json` 零迁移。
    #[serde(default)]
    pub owner_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// 额外键值对（颜色、图标、标签等 UI 偏好）
    #[serde(default)]
    pub metadata: IndexMap<String, serde_json::Value>,
    /// 项目成员（Agent 实例 + 人类账号，ADR-076 §决策 11）。创建者一定在
    /// 列表里（`kind` 标出身份）；`#[serde(default)]` 保证旧 `project.json`
    /// 无此字段时读出空数组（零迁移）。
    #[serde(default)]
    pub members: Vec<ProjectMember>,
}

impl Project {
    /// The resolved group owner id — `owner_id` when set (post-transfer),
    /// otherwise the creator `created_by`. The owner is a permanent group
    /// member; every surface that folds it into the member/participant set
    /// reads through this so transfer is reflected everywhere at once.
    pub fn owner(&self) -> &str {
        self.owner_id.as_deref().unwrap_or(&self.created_by)
    }
}

/// `POST /api/pm/projects/:pid/members` 请求体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddProjectMember {
    /// Agent 实例 ID（UUID，ADR-073）或 User ID（ADR-076，按 `kind` 解释）。
    /// `kind: "agent"` 时添加前由 API 层按
    /// `AgentDirectory::agent_exists` 校验存在性。
    pub instance_id: String,
    /// 身份类型。`#[serde(default)]` = `Agent` → 旧客户端不发 `kind` 时
    /// 行为与既有契约完全一致（零破坏）。
    #[serde(default)]
    pub kind: MemberKind,
}

/// `POST /api/pm/projects` 请求体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateProject {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub metadata: IndexMap<String, serde_json::Value>,
}

/// `PATCH /api/pm/projects/:pid` 请求体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateProject {
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<ProjectStatus>,
    pub metadata: Option<IndexMap<String, serde_json::Value>>,
}

// ────────────────────────────────────────────────────────────────────────────
// 实体：Task
// ────────────────────────────────────────────────────────────────────────────

/// 任务实体（持久化在 `{project_dir}/tasks/.../{task_id}/task.json`）。
///
/// **关键**：不含 `parent_id` / `subtask_ids` / `subtask_count`，因为物理嵌套即权威。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: TaskId,
    pub project_id: ProjectId,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    pub status: TaskStatus,
    #[serde(default = "default_review")]
    pub review_status: ReviewStatus,
    #[serde(default = "default_priority")]
    pub priority: Priority,
    /// 责任人：`human` 或 `agent_instance_id`（UUID，ADR-073 §1.3 不变量 1）。
    ///
    /// 解析为显示名时，由调用方按 `instance_id` 查 agent directory（`meta.display_name ?? meta.name`）。
    #[serde(default)]
    pub assignee: Option<String>,
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    /// 前置依赖（跨树/跨项目皆可）。
    ///
    /// **派生字段** `is_blocked` / `blocked_by` 在 API 响应层计算，**不**写入此结构。
    #[serde(default)]
    pub depends_on: Vec<Dependency>,
    /// 附件元数据列表（**二进制不入此结构**，实际文件在 `attachments/{att_id}/` 下）。
    #[serde(default)]
    pub attachments: Vec<AttachmentMeta>,
    /// Agent 提交的结果（submit 时写入）。
    #[serde(default)]
    pub result: Option<TaskResult>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub claimed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub submitted_at: Option<DateTime<Utc>>,
}

fn default_review() -> ReviewStatus {
    ReviewStatus::NotRequired
}

fn default_priority() -> Priority {
    Priority::Normal
}

/// 任务优先级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
    Urgent,
}

/// 依赖声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub task_id: TaskId,
    pub kind: DependencyKind,
}

/// Agent 提交结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub text: String,
    #[serde(default)]
    pub attachment_ids: Vec<AttachmentId>,
    /// 提交者：`agent_instance_id`（UUID，ADR-073），由 `X-MCP-Actor` 注入。
    pub submitted_by: String,
    pub submitted_at: DateTime<Utc>,
}

/// `POST /api/pm/projects/:pid/tasks` 请求体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateTask {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type", default = "default_task_type")]
    pub task_type: TaskType,
    #[serde(default = "default_priority")]
    pub priority: Priority,
    /// 父任务 ID（**不**接受 `parent_id` 字段——只能通过显式端点设置）。
    #[serde(default)]
    pub parent_task_id: Option<TaskId>,
    #[serde(default)]
    pub depends_on: Vec<Dependency>,
    /// 可选：在创建时上传的附件 ID 列表（附件需先调用 multipart 上传获取 ID）。
    #[serde(default)]
    pub attachment_ids: Vec<AttachmentId>,
    /// 指派 Agent（设计 PM-04 / §6 `pm_create_task` 的 `assignee` 参数）。
    ///
    /// **ADR-073**：`assignee` 存 `agent_instance_id`（UUID），不是 `agent_id`（包 ID）。
    /// **联动指派**：非空 assignee 必须是该项目成员（`project.members`），
    /// 由 store 层统一强制（REST + MCP 共用同一写路径）。
    #[serde(default)]
    pub assignee: Option<String>,
    /// 截止时间（可选；`pm_create_task` 的 `due` 参数）。
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
}

fn default_task_type() -> TaskType {
    TaskType::Task
}

/// 反序列化"可清空"的三态可选字段（`Option<Option<T>>`）：
///
/// | JSON | 反序列化结果 | 语义 |
/// |------|------------|------|
/// | 字段缺失 | `None` | 不修改 |
/// | `null` | `Some(None)` | 清空为 null |
/// | 值 `v` | `Some(Some(v))` | 设为 v |
///
/// **为什么需要自定义反序列化器**：serde 默认把 JSON `null` 解析为 `Option<T>`
/// 的外层 `None`，与字段缺失无法区分，导致 `UpdateTask.assignee` / `due_at`
/// 的"清空"分支永远无法通过 wire 触发（P1 遗留缺陷）。此 helper 通过
/// `deserialize_option` 的 `visit_none` 分支显式返回 `Some(None)` 修复之。
///
/// 用于 `#[serde(default, deserialize_with = "deserialize_clearable")]`。
pub fn deserialize_clearable<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    struct ClearableVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: serde::de::DeserializeOwned> serde::de::Visitor<'de>
        for ClearableVisitor<T>
    {
        type Value = Option<Option<T>>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a nullable value (null clears the field)")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Some(None))
        }

        fn visit_some<D2>(self, d: D2) -> Result<Self::Value, D2::Error>
        where
            D2: serde::Deserializer<'de>,
        {
            T::deserialize(d).map(|t| Some(Some(t)))
        }
    }

    d.deserialize_option(ClearableVisitor(std::marker::PhantomData))
}

/// `PATCH /api/pm/tasks/:tid` 请求体（**所有字段皆可选**，未提供则不修改）。
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateTask {
    pub title: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub task_type: Option<TaskType>,
    pub status: Option<TaskStatus>,
    pub priority: Option<Priority>,
    #[serde(default, deserialize_with = "deserialize_clearable")]
    pub assignee: Option<Option<String>>, // `null` 表示清空
    #[serde(default, deserialize_with = "deserialize_clearable")]
    pub due_at: Option<Option<DateTime<Utc>>>,
    pub depends_on: Option<Vec<Dependency>>,
}

/// `PATCH /api/pm/tasks/:tid/parent` 请求体。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReparentTask {
    /// 新的父任务 ID。`None` 表示提升为项目根任务。
    pub new_parent: Option<TaskId>,
}

// ────────────────────────────────────────────────────────────────────────────
// 实体：Attachment
// ────────────────────────────────────────────────────────────────────────────

/// 附件元数据（持久化在 `task.json` 的 `attachments` 数组中）。
///
/// **二进制文件不放在此结构**，实际文件位于 `{task_dir}/attachments/{id}/original.{ext}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentMeta {
    pub id: AttachmentId,
    pub filename: String,
    pub kind: AttachmentKind,
    pub content_type: String,
    pub size: u64,
    pub sha256: String,
    /// 相对 `task.json` 所在目录的路径（用于回放/恢复）。
    pub storage_path: String,
    /// 仅图片有缩略图。
    #[serde(default)]
    pub thumb_path: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    pub uploaded_by: String,
    pub uploaded_at: DateTime<Utc>,
}

// ────────────────────────────────────────────────────────────────────────────
// 查询过滤器
// ────────────────────────────────────────────────────────────────────────────

/// `GET /api/pm/tasks?...` 查询参数。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskFilter {
    pub project_id: Option<ProjectId>,
    pub status: Option<TaskStatus>,
    pub assignee: Option<String>,
    /// 设为 `true` 仅返回被依赖阻塞的任务。
    #[serde(default)]
    pub only_blocked: bool,
    /// 排序字段（默认 `created_at`）。
    #[serde(default)]
    pub sort: Option<TaskSort>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskSort {
    #[default]
    CreatedAt,
    UpdatedAt,
    DueAt,
    Priority,
}

// ────────────────────────────────────────────────────────────────────────────
// API 响应包装（响应层补充派生字段）
// ────────────────────────────────────────────────────────────────────────────

/// 任务完整响应（包含派生字段）。
///
/// 序列化时附带 `is_blocked` / `blocked_by` / `depth` / `parent_id`，
/// 便于前端展示与重建看板树，但不写入 `task.json`。
#[derive(Debug, Clone, Serialize)]
pub struct TaskResponse {
    #[serde(flatten)]
    pub task: Task,
    /// 当前是否被依赖阻塞。
    #[serde(skip_serializing_if = "is_false")]
    pub is_blocked: bool,
    /// 阻塞本任务的任务 ID 列表（仅 kind=Blocks 的依赖）。
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<TaskId>,
    /// 物理深度（根=0）。
    pub depth: u8,
    /// 父任务 ID（由物理目录位置推导，根任务为 `null`）。
    ///
    /// **P2 新增**：列表接口返回此字段，前端可一次拉取重建看板树，
    /// 无需逐个调用 `/tasks/:tid/children`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<TaskId>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

// ────────────────────────────────────────────────────────────────────────────
// 单元测试
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_generation_has_prefix() {
        let pid = ProjectId::generate();
        assert!(pid.as_str().starts_with("p-"));
        assert!(pid.as_str().len() >= 10);

        let tid = TaskId::generate();
        assert!(tid.as_str().starts_with("t-"));

        let aid = AttachmentId::generate();
        assert!(aid.as_str().starts_with("att-"));
    }

    /// ADR-076 §决策 11：`POST /members` 请求体的 `kind` 字段——旧客户端
    /// 不发时缺省为 Agent（零破坏），发 `"user"` 时解析为人类账号。
    #[test]
    fn add_project_member_kind_defaults_to_agent() {
        let legacy: AddProjectMember =
            serde_json::from_str(r#"{"instance_id":"21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c"}"#)
                .expect("legacy body without kind");
        assert_eq!(legacy.kind, MemberKind::Agent);

        let with_user: AddProjectMember = serde_json::from_str(
            r#"{"instance_id":"21a727ee-d2ac-4d7a-9bb1-a846cb34cf3c","kind":"user"}"#,
        )
        .expect("body with kind=user");
        assert_eq!(with_user.kind, MemberKind::User);
    }

    #[test]
    fn id_parse_validates_format() {
        assert!(ProjectId::parse("p-abc123").is_ok());
        assert!(ProjectId::parse("p-").is_err()); // too short
        assert!(ProjectId::parse("p-abc/def").is_err()); // invalid char
        assert!(ProjectId::parse("x-abc").is_err()); // wrong prefix
        assert!(ProjectId::parse(&format!("p-{}", "x".repeat(70))).is_err()); // too long
    }

    #[test]
    fn task_roundtrip_json_excludes_derived_fields() {
        let task = Task {
            id: TaskId::generate(),
            project_id: ProjectId::generate(),
            title: "test".to_string(),
            description: "".to_string(),
            task_type: TaskType::Task,
            status: TaskStatus::Pending,
            review_status: ReviewStatus::NotRequired,
            priority: Priority::Normal,
            assignee: None,
            due_at: None,
            depends_on: vec![],
            attachments: vec![],
            result: None,
            created_by: "human".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            claimed_at: None,
            submitted_at: None,
        };
        let json = serde_json::to_string(&task).unwrap();
        let parsed: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(task.id, parsed.id);
        // 确认关键字段不存在（冗余字段验证）
        assert!(!json.contains("parent_id"));
        assert!(!json.contains("subtask_ids"));
        assert!(!json.contains("subtask_count"));
    }

    #[test]
    fn checkpoint_requires_review() {
        assert!(TaskType::Checkpoint.requires_review());
        assert!(TaskType::Bug.requires_review());
        assert!(!TaskType::Task.requires_review());
        assert!(!TaskType::Milestone.requires_review());
    }
}