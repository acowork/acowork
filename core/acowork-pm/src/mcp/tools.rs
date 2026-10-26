//! MCP 工具分发（**P3 完整实现**）。
//!
//! 所有 `pm_*` 工具在此实现。每个工具：
//!
//! 1. 解析参数（serde，宽松 `default`）
//! 2. 执行身份校验（设计 §9.2 / §9.3）：
//!    - 匿名（无 `X-MCP-Actor`）仅允许只读工具：`pm_list_*` / `pm_get_*`
//!    - 状态变更工具要求身份
//!    - `pm_claim_task` / `pm_submit_task` / `pm_update_task` 要求
//!      调用者 `instance_id` == 任务 `assignee`（**ADR-073**：`agent_instance_id`
//!      是唯一身份 key；`agent_id` 仅显示用），否则 403
//!    - `pm_create_task` 的 `assignee` 必须存在于 Agent 目录（§9.1，
//!      按 instance_id 校验 `GET /api/agents/{instance_id}`）
//! 3. 调用 [`PmStore`] trait 业务方法
//! 4. 返回精简 JSON（复用 REST `TaskResponse` 形状，仅 LLM 关心的字段）
//!
//! 错误统一返回 [`PmError`]；由 [`crate::mcp::mod::jsonrpc_endpoint`] 包装为
//! JSON-RPC error（message 带 `error_code:` 前缀，客户端据此做
//! `[permission]` / `[transient]` 分类）。

use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{PmError, Result};
use crate::mcp::{AgentDirectory, AgentInfo, McpState};
use crate::store::tree::{PmStore, TreePmStore};
use crate::types::{
    CreateProject, CreateTask, Dependency, MemberKind, Priority, ProjectId, ProjectStatus,
    ReparentTask, ReviewStatus, Task, TaskFilter, TaskId, TaskResponse, TaskSort, TaskStatus,
    TaskType, UpdateTask, deserialize_clearable,
};

/// 工具分发入口（由 `POST /mcp` 的 `tools/call` 调用）。
pub async fn dispatch(
    state: &McpState,
    actor: Option<&str>,
    name: &str,
    args: Value,
) -> Result<Value> {
    match name {
        // ── 只读（匿名允许，设计 §9.3）──────────────────────────────
        "pm_list_projects" => pm_list_projects(state, args).await,
        "pm_get_project" => pm_get_project(state, args).await,
        "pm_list_tasks" => pm_list_tasks(state, args).await,
        "pm_get_task" => pm_get_task(state, args).await,

        // ── 需要身份的只读（自查）───────────────────────────────────
        "pm_list_my_tasks" => {
            let a = require_actor(actor)?;
            pm_list_my_tasks(state, a, args).await
        }
        "pm_check_task" => {
            let a = require_actor(actor)?;
            pm_check_task(state, a, args).await
        }

        // ── 状态变更（需要身份）──────────────────────────────────────
        "pm_create_project" => {
            let a = require_actor(actor)?;
            pm_create_project(state, a, args).await
        }
        "pm_create_task" => {
            let a = require_actor(actor)?;
            pm_create_task(state, a, args).await
        }
        "pm_update_task" => {
            let a = require_actor(actor)?;
            pm_update_task(state, a, args).await
        }
        "pm_claim_task" => {
            let a = require_actor(actor)?;
            pm_claim_task(state, a, args).await
        }
        "pm_submit_task" => {
            let a = require_actor(actor)?;
            pm_submit_task(state, a, args).await
        }
        "pm_reparent_task" => {
            let a = require_actor(actor)?;
            pm_reparent_task(state, a, args).await
        }

        other => Err(PmError::BadRequest(format!("unknown tool: {other}"))),
    }
}

// ── 鉴权辅助 ──────────────────────────────────────────────────────────────

/// 要求调用方具备身份（匿名 → 401）。所有状态变更工具必须先过此关。
fn require_actor(actor: Option<&str>) -> Result<&str> {
    actor.ok_or_else(|| {
        PmError::Unauthenticated(
            "this tool requires an authenticated agent (send X-MCP-Actor header)".into(),
        )
    })
}

/// 要求调用方 == 任务 assignee（设计 §9.2）。`pm_claim_task` /
/// `pm_submit_task` / `pm_update_task` 使用。
///
/// **ADR-073**：两侧比较的都是 `agent_instance_id`（UUID）——
/// `actor` 来自 `X-MCP-Actor` header（Gateway 注入 `{instance_id}` 模板，
/// Runtime 替换为 `self.instance_id`），`task.assignee` 在创建时已按
/// instance 维度校验存在性（§9.1）。
fn ensure_assignee(task: &Task, actor: &str) -> Result<()> {
    match &task.assignee {
        Some(a) if a == actor => Ok(()),
        Some(a) => Err(PmError::Forbidden(format!(
            "task {} is assigned to instance `{}`, not `{}`; only the assignee instance can perform this action",
            task.id, a, actor
        ))),
        None => Err(PmError::Forbidden(format!(
            "task {} has no assignee; it must be assigned to instance `{}` before it can be acted on",
            task.id, actor
        ))),
    }
}

// ── 参数解析辅助 ──────────────────────────────────────────────────────────

fn parse_args<T: for<'de> Deserialize<'de>>(name: &str, args: Value) -> Result<T> {
    serde_json::from_value(args)
        .map_err(|e| PmError::BadRequest(format!("invalid arguments for {name}: {e}")))
}

// ── 响应序列化辅助 ────────────────────────────────────────────────────────

/// 保留创建者哨兵值（`types::Project::created_by` / `types::Task::created_by`
/// 的合法取值之一，见 `types.rs` 文档：`human` 或 agent instance UUID）。
/// 非 instance_id，不应发起 Gateway 查询。
const HUMAN_CREATOR: &str = "human";

/// 项目创建者是否为人类账号（ADR-076 §决策 11）。
///
/// 创建者始终自动入 `members` 并带 `kind`；`User` 创建者不在 Agent 目录里，
/// 不应发起 Gateway `/api/agents/{user_id}` 查询（会 404）。
fn creator_is_user(p: &crate::types::Project) -> bool {
    p.members
        .iter()
        .any(|m| m.instance_id == p.created_by && m.kind == MemberKind::User)
}

/// 查 `instance_id` 对应的 Agent 元信息。
///
/// 宽松目录（`NoopAgentDirectory`）返回 `None`；HTTP 目录缓存命中返回
/// `Some`，缓存 miss 时走即时兜底（见 [`AgentDirectory::agent_info`]）。
///
/// `"human"`（保留创建者哨兵）直接短路返回 `None`——否则每次
/// `pm_list_tasks` / `pm_list_projects` 都会对 Gateway 发起一次无谓的
/// `/api/agents/human` 查询（404）。
async fn lookup_agent_meta(
    agent_dir: &dyn AgentDirectory,
    instance_id: Option<&str>,
) -> Option<AgentInfo> {
    let id = instance_id?;
    if id == HUMAN_CREATOR {
        return None;
    }
    agent_dir.agent_info(id).await
}

/// 把 `instance_id` 字符串 + 元信息投影成 `{instance_id, agent_id, name}`。
///
/// `instance_id` 为 `None` 或元信息为 `None`（Agent 已卸载 / Noop 目录）
/// 时返回 `Value::Null`，让 LLM 拿到清晰的 null 而不是空字符串。
fn agent_ref_value(instance_id: Option<&str>, info: Option<&AgentInfo>) -> Value {
    match (instance_id, info) {
        (Some(id), Some(info)) => json!({
            "instance_id": id,
            "agent_id": info.agent_id,
            "name": info.name,
        }),
        _ => Value::Null,
    }
}

/// 任务 → 精简 JSON（复用 REST `TaskResponse` 形状，含派生字段 + Agent 元信息）。
///
/// **MCP 专属**附加字段（不在 REST `TaskResponse` 里）：
/// - `assignee_meta`：assignee 的 `{instance_id, agent_id, name}`，LLM 据此反查
///   显示名 / 派任务时取 `instance_id`
/// - `created_by_meta`：创建者同上（`"human"` 时为 `null`）
///
/// `assignee` / `created_by` 原字段保留为 `String`（前端 join agentStore 显示）。
///
/// 单条场景（get / create / claim / submit / update）直接查 meta；列表场景
/// 请走 [`tasks_to_values`]（先批量去重查询，再复用 [`task_to_value_with_meta`]）。
async fn task_to_value(
    store: &Arc<TreePmStore>,
    agent_dir: &dyn AgentDirectory,
    task: Task,
) -> Result<Value> {
    // 单条任务至多 2 个 agent 字段（assignee + created_by），`tokio::join!`
    // 并发查即可，无需批量层。
    let (assignee_meta, created_by_meta) = tokio::join!(
        lookup_agent_meta(agent_dir, task.assignee.as_deref()),
        lookup_agent_meta(agent_dir, Some(&task.created_by)),
    );
    task_to_value_with_meta(store, task, assignee_meta.as_ref(), created_by_meta.as_ref()).await
}

/// [`task_to_value`] 的查表变体：`assignee_meta` / `created_by_meta` 已由
/// 调用方（如 [`tasks_to_values`] 的批量查询）解析好，此处只做派生字段
/// 计算 + 序列化 + 附加，不再访问 `agent_dir`。
async fn task_to_value_with_meta(
    store: &Arc<TreePmStore>,
    task: Task,
    assignee_meta: Option<&AgentInfo>,
    created_by_meta: Option<&AgentInfo>,
) -> Result<Value> {
    let tid = task.id.clone();
    let depth = store.index_entry(&tid).map(|e| e.depth).unwrap_or(0);
    let parent_id = store.parent_of(&tid);
    let blocked_by = store.compute_blocked_by(&tid).await?;

    // 在 `task` move 进 `TaskResponse` 前取出原始字符串，避免从序列化结果
    // 反读（`v["assignee"]` / `v["created_by"]`）带来的格式耦合。
    let assignee = task.assignee.clone();
    let created_by = task.created_by.clone();

    let resp = TaskResponse {
        task,
        is_blocked: !blocked_by.is_empty(),
        blocked_by,
        depth,
        parent_id,
    };
    let mut v = serde_json::to_value(resp).map_err(PmError::from)?;
    v["assignee_meta"] = agent_ref_value(assignee.as_deref(), assignee_meta);
    v["created_by_meta"] = agent_ref_value(Some(&created_by), created_by_meta);
    Ok(v)
}

/// 批量查 `instance_id` 元信息（过滤哨兵 + 去重 + 限流并发）。
///
/// `list_*` 接口一次返回 N 条任务 / 项目，每条至多 2 个 agent 字段
/// （assignee + created_by）。不去重会产生 N×2 次 HTTP 调用；本函数
/// 去重后按 [`MAX_META_LOOKUP_CONCURRENCY`] 限流并发，避免冷缓存下
/// 一次大列表把 Gateway `/api/agents/{id}` 打爆（ADR-055 多机部署时
/// Gateway 在远端，无上限并发会放大延迟与压力）。
///
/// `"human"`（保留创建者哨兵）与空串直接跳过，不走 Gateway 查询——
/// 人类创建的项目/任务越多，省下的无谓 HTTP 越多。
const MAX_META_LOOKUP_CONCURRENCY: usize = 8;

async fn batch_lookup_agent_meta(
    agent_dir: &dyn AgentDirectory,
    ids: Vec<String>,
) -> std::collections::HashMap<String, AgentInfo> {
    let unique: std::collections::HashSet<String> = ids
        .into_iter()
        .filter(|id| !id.is_empty() && id != HUMAN_CREATOR)
        .collect();
    if unique.is_empty() {
        return std::collections::HashMap::new();
    }
    let futures = unique.into_iter().map(|id| {
        let agent_dir: &dyn AgentDirectory = agent_dir;
        async move { (id.clone(), agent_dir.agent_info(&id).await) }
    });
    use futures_util::StreamExt;
    let resolved: Vec<_> = futures_util::stream::iter(futures)
        .buffer_unordered(MAX_META_LOOKUP_CONCURRENCY)
        .collect()
        .await;
    resolved
        .into_iter()
        .filter_map(|(id, info)| info.map(|i| (id, i)))
        .collect()
}

/// 任务列表 → JSON 值列表（应用 limit + 先批量查 meta 再组装）。
///
/// 关键点：**先**把全部任务的 assignee + created_by 收集、去重、一次性查
/// `agent_dir`（单次列表至多一次批量查询），**再**逐条序列化查表组装。
/// 避免每条任务独立查 `agent_info`，在冷缓存下退化成 N×2 次 HTTP。
async fn tasks_to_values(
    store: &Arc<TreePmStore>,
    agent_dir: &dyn AgentDirectory,
    tasks: Vec<Task>,
    limit: usize,
) -> Result<Vec<Value>> {
    let tasks: Vec<Task> = tasks.into_iter().take(limit).collect();
    // 收集全部 agent 字段 id：assignee（Option）+ created_by（必填）。
    // `flat_map` 把 Option 迭代器 + 单元素链平铺成 `Iterator<Item = String>`。
    let ids: Vec<String> = tasks
        .iter()
        .flat_map(|t| {
            t.assignee
                .iter()
                .chain(std::iter::once(&t.created_by))
                .cloned()
        })
        .collect();
    let meta = batch_lookup_agent_meta(agent_dir, ids).await;

    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        let assignee_meta = task.assignee.as_deref().and_then(|id| meta.get(id));
        let created_by_meta = meta.get(&task.created_by);
        out.push(
            task_to_value_with_meta(store, task, assignee_meta, created_by_meta).await?,
        );
    }
    Ok(out)
}


/// 项目 → 精简 JSON。
///
/// 成员元信息（`agent_id` / `name`）从 `agent_dir` 反查后单独附加，
/// 不混入此函数 — `pm_list_projects` 列表展示时不带成员（避免一次拉全量
/// agent 目录塞回列表响应，撑爆 LLM 上下文）。
///
/// `created_by_meta`（MCP 专属）见 [`task_to_value`]。
fn project_to_value(
    p: crate::types::Project,
    task_count: usize,
    created_by_meta: Option<&AgentInfo>,
) -> Value {
    json!({
        "id": p.id,
        "title": p.title,
        "description": p.description,
        "status": p.status,
        "created_by": p.created_by,
        "created_by_meta": agent_ref_value(Some(&p.created_by), created_by_meta),
        "created_at": p.created_at,
        "updated_at": p.updated_at,
        "task_count": task_count,
    })
}

// ── 只读工具 ──────────────────────────────────────────────────────────────

/// `pm_list_projects` — 项目列表（摘要 + 任务数）。
async fn pm_list_projects(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize, Default)]
    struct Args {
        #[serde(default)]
        include_archived: bool,
    }
    let a: Args = parse_args("pm_list_projects", args)?;

    let projects = state.store.list_projects().await?;
    // 先过滤可见项目，再一次性批量查 created_by 的 agent 元信息
    // （去重 + 限流并发；"human" 哨兵在 batch 内跳过，User 创建者跳过）。
    let visible: Vec<_> = projects
        .into_iter()
        .filter(|p| {
            a.include_archived
                || !matches!(p.status, ProjectStatus::Archived | ProjectStatus::Completed)
        })
        .collect();
    let created_by_ids: Vec<String> = visible
        .iter()
        .filter(|p| !creator_is_user(p))
        .map(|p| p.created_by.clone())
        .collect();
    let meta = batch_lookup_agent_meta(state.agent_dir.as_ref(), created_by_ids).await;

    let out: Vec<Value> = visible
        .into_iter()
        .map(|p| {
            let count = state.store.project_task_count(&p.id);
            let cb_meta = meta.get(&p.created_by);
            project_to_value(p, count, cb_meta)
        })
        .collect();
    Ok(Value::Array(out))
}

/// `pm_get_project` — 项目详情（含任务数分拆 + 成员元信息）。
///
/// 成员元信息（`agent_id` / `name`）从 `agent_dir` 反查，让 LLM 拿到项目
/// 成员列表后能用 agent name 反查到 `instance_id` 用于 `pm_create_task` 指派。
/// `NoopAgentDirectory`（宽松模式）下所有 `agent_id` / `name` 字段为 `null`。
async fn pm_get_project(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        project_id: ProjectId,
    }
    let a: Args = parse_args("pm_get_project", args)?;

    let p = state
        .store
        .get_project(&a.project_id)
        .await?
        .ok_or_else(|| PmError::ProjectNotFound(a.project_id.to_string()))?;
    let count = state.store.project_task_count(&a.project_id);

    // 批量查：created_by + Agent 类 members（去重 + 限流并发；`"human"` 哨兵
    // 与 User 成员跳过——User 成员是 ADR-076 人类账号，不在 Agent 目录里）。
    let mut ids: Vec<String> = Vec::with_capacity(1 + p.members.len());
    ids.push(p.created_by.clone());
    ids.extend(
        p.members
            .iter()
            .filter(|m| m.kind == MemberKind::Agent)
            .map(|m| m.instance_id.clone()),
    );
    let meta = batch_lookup_agent_meta(state.agent_dir.as_ref(), ids).await;
    let cb_meta = meta.get(&p.created_by);

    let mut v = project_to_value(p.clone(), count, cb_meta);
    let mut members = Vec::with_capacity(p.members.len());
    for m in &p.members {
        // User 成员（ADR-076）不在 Agent 目录里 → 元信息恒为 None。
        let info = (m.kind == MemberKind::Agent)
            .then(|| meta.get(&m.instance_id))
            .flatten();
        // 投影规则：
        // - `agent_id` / `name` / `role` 来自 AgentDirectory（缓存命中 / 即时兜底）
        // - 任一字段 AgentDirectory 拿不到 → `null`（与 `NoopAgentDirectory`
        //   宽松模式语义一致），**不**省略（省略会让老调用方 schema 漂移
        //   解析失败；保持显式 null = "未声明 / 不可用" 是契约稳定的最小面）
        // - `added_at` 来自 ProjectMember 自身
        members.push(json!({
            "instance_id": m.instance_id,
            "kind": m.kind,
            "agent_id": info.map(|i| &i.agent_id),
            "name": info.map(|i| &i.name),
            "role": info.and_then(|i| i.role.as_ref()),
            "added_at": m.added_at,
        }));
    }
    v["members"] = Value::Array(members);
    Ok(v)
}

/// `pm_list_tasks` — 项目内任务列表（支持过滤 + limit）。
async fn pm_list_tasks(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        project_id: ProjectId,
        #[serde(default)]
        status: Option<TaskStatus>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        only_blocked: bool,
        #[serde(default = "default_limit")]
        limit: usize,
    }
    fn default_limit() -> usize {
        20
    }
    let a: Args = parse_args("pm_list_tasks", args)?;

    let filter = TaskFilter {
        project_id: Some(a.project_id),
        status: a.status,
        assignee: a.assignee,
        only_blocked: a.only_blocked,
        sort: Some(TaskSort::CreatedAt),
    };
    let tasks = state.store.find_tasks(&filter).await?;
    let agent_dir: &dyn AgentDirectory = state.agent_dir.as_ref();
    let out = tasks_to_values(&state.store, agent_dir, tasks, a.limit.max(1)).await?;
    Ok(Value::Array(out))
}

/// `pm_get_task` — 任务详情（含 is_blocked / blocked_by / depth / parent_id）。
async fn pm_get_task(state: &McpState, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
    }
    let a: Args = parse_args("pm_get_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    task_to_value(&state.store, state.agent_dir.as_ref(), task).await
}

/// `pm_list_my_tasks` — Agent 自查：指派给当前调用者的任务。
async fn pm_list_my_tasks(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize, Default)]
    struct Args {
        #[serde(default)]
        status: Option<TaskStatus>,
        #[serde(default = "default_limit")]
        limit: usize,
    }
    fn default_limit() -> usize {
        20
    }
    let a: Args = parse_args("pm_list_my_tasks", args)?;

    let filter = TaskFilter {
        assignee: Some(actor.to_string()),
        status: a.status,
        ..Default::default()
    };
    let tasks = state.store.find_tasks(&filter).await?;
    let agent_dir: &dyn AgentDirectory = state.agent_dir.as_ref();
    let out = tasks_to_values(&state.store, agent_dir, tasks, a.limit.max(1)).await?;
    Ok(Value::Array(out))
}

/// `pm_check_task` — 查询自己创建的任务是否被批准（含审核状态）。
async fn pm_check_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
    }
    let a: Args = parse_args("pm_check_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    // 仅允许创建者查询（设计 §6：`Agent 查询自己创建的任务是否被批准`）。
    if task.created_by != actor {
        return Err(PmError::Forbidden(format!(
            "task {} was created by `{}`, not `{}`; only the creator can check its review status",
            task.id, task.created_by, actor
        )));
    }
    let created_by_info = lookup_agent_meta(state.agent_dir.as_ref(), Some(&task.created_by)).await;
    Ok(json!({
        "id": task.id,
        "project_id": task.project_id,
        "title": task.title,
        "status": task.status,
        "review_status": task.review_status,
        "approved": matches!(task.review_status, ReviewStatus::Approved),
        "created_by": task.created_by,
        "created_by_meta": agent_ref_value(Some(&task.created_by), created_by_info.as_ref()),
        "created_at": task.created_at,
        "updated_at": task.updated_at,
    }))
}

// ── 状态变更工具 ──────────────────────────────────────────────────────────

/// `pm_create_project` — 创建项目。
async fn pm_create_project(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        title: String,
        #[serde(default)]
        description: String,
    }
    let a: Args = parse_args("pm_create_project", args)?;

    let input = CreateProject {
        title: a.title,
        description: a.description,
        metadata: Default::default(),
    };
    let p = state
        .store
        .create_project_as(input, actor, MemberKind::Agent)
        .await?;
    // 创建者始终入 members 并带 `kind`：User 创建者（真实 user_id）不在 Agent
    // 目录里 → 跳过查询，meta 为 null（ADR-076 §决策 11）。
    let cb_meta = if creator_is_user(&p) {
        None
    } else {
        lookup_agent_meta(state.agent_dir.as_ref(), Some(&p.created_by)).await
    };
    Ok(project_to_value(p, 0, cb_meta.as_ref()))
}

/// `pm_create_task` — 创建任务（Agent 创建 → `review_status=pending`，待人类审核）。
///
/// `assignee` 若提供则必须存在于 Agent 目录（§9.1）；不要求是调用者本人（§9.2）。
async fn pm_create_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        project_id: ProjectId,
        title: String,
        #[serde(default)]
        description: String,
        #[serde(rename = "type", default)]
        task_type: TaskType,
        #[serde(default)]
        priority: Priority,
        #[serde(default)]
        parent_task_id: Option<TaskId>,
        #[serde(default)]
        depends_on: Vec<Dependency>,
        #[serde(default)]
        assignee: Option<String>,
        #[serde(default)]
        due_at: Option<chrono::DateTime<chrono::Utc>>,
        /// 开始时间：到期才由 PM 派发扫描器推送 intent；缺省 = 立即。
        #[serde(default)]
        start_at: Option<chrono::DateTime<chrono::Utc>>,
    }
    let a: Args = parse_args("pm_create_task", args)?;

    // 设计 §9.1：assignee 必须存在（按 instance_id 校验，ADR-073）
    if let Some(assignee) = &a.assignee
        && !state.agent_dir.agent_exists(assignee).await
    {
        return Err(PmError::BadRequest(format!(
            "assignee instance not found in agent directory: {assignee}"
        )));
    }

    let input = CreateTask {
        title: a.title,
        description: a.description,
        task_type: a.task_type,
        priority: a.priority,
        parent_task_id: a.parent_task_id,
        depends_on: a.depends_on,
        attachment_ids: vec![],
        assignee: a.assignee,
        due_at: a.due_at,
        start_at: a.start_at,
    };
    let task = state
        .store
        .create_task_as(&a.project_id, input, actor, MemberKind::Agent)
        .await?;
    task_to_value(&state.store, state.agent_dir.as_ref(), task).await
}

/// `pm_update_task` — 更新任务（仅 assignee 本人，设计 §9.2）。
async fn pm_update_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(rename = "type", default)]
        task_type: Option<TaskType>,
        #[serde(default)]
        status: Option<TaskStatus>,
        #[serde(default)]
        priority: Option<Priority>,
        #[serde(default, deserialize_with = "deserialize_clearable")]
        assignee: Option<Option<String>>,
        #[serde(default, deserialize_with = "deserialize_clearable")]
        due_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
        #[serde(default, deserialize_with = "deserialize_clearable")]
        start_at: Option<Option<chrono::DateTime<chrono::Utc>>>,
        #[serde(default)]
        depends_on: Option<Vec<Dependency>>,
    }
    let a: Args = parse_args("pm_update_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    ensure_assignee(&task, actor)?;

    let input = UpdateTask {
        title: a.title,
        description: a.description,
        task_type: a.task_type,
        status: a.status,
        priority: a.priority,
        assignee: a.assignee,
        due_at: a.due_at,
        start_at: a.start_at,
        depends_on: a.depends_on,
    };
    let task = state.store.update_task(&a.task_id, input).await?;
    task_to_value(&state.store, state.agent_dir.as_ref(), task).await
}

/// `pm_claim_task` — 自领（pending → in_progress），仅限 assignee；依赖未满足 409。
async fn pm_claim_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
    }
    let a: Args = parse_args("pm_claim_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    ensure_assignee(&task, actor)?;

    let task = state.store.claim_task(&a.task_id, actor).await?;
    task_to_value(&state.store, state.agent_dir.as_ref(), task).await
}

/// `pm_submit_task` — 提交结果（in_progress → submitted），仅限 assignee。
async fn pm_submit_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
        text: String,
        #[serde(default)]
        attachment_ids: Vec<crate::types::AttachmentId>,
    }
    let a: Args = parse_args("pm_submit_task", args)?;

    let task = state
        .store
        .get_task(&a.task_id)
        .await?
        .ok_or_else(|| PmError::TaskNotFound(a.task_id.to_string()))?;
    ensure_assignee(&task, actor)?;

    let task = state
        .store
        .submit_task(&a.task_id, &a.text, a.attachment_ids, actor)
        .await?;
    task_to_value(&state.store, state.agent_dir.as_ref(), task).await
}

/// `pm_reparent_task` — 移动任务到新父下（new_parent=null 提升为根），DFS 防环。
///
/// 需要身份（§9.3 匿名仅只读）；设计 §9.2 未要求 assignee 匹配，但要求已认证。
async fn pm_reparent_task(state: &McpState, actor: &str, args: Value) -> Result<Value> {
    #[derive(Deserialize)]
    struct Args {
        task_id: TaskId,
        #[serde(default)]
        new_parent: Option<TaskId>,
    }
    let a: Args = parse_args("pm_reparent_task", args)?;

    // actor 仅用于满足"已认证"前提（见上方 doc）；任务存在性/防环由 store 校验
    let _actor = actor;

    let input = ReparentTask {
        new_parent: a.new_parent,
    };
    state.store.reparent_task(&a.task_id, input).await?;
    Ok(json!({ "ok": true, "task_id": a.task_id }))
}
