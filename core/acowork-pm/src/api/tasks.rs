//! `tasks` handlers（**P1 实现**）。内部路径不带 `/api` 前缀，
//! 公开路径为 `/api/pm/tasks/*`（见 [`routes::pm_router`]）。

use axum::Json;
use axum::extract::{Path, State};

use crate::types::{
    AttachmentId, CreateTask, MemberKind, ProjectId, ReparentTask, ReviewStatus, Task,
    TaskActivity, TaskFilter, TaskId, TaskStatus, UpdateTask,
};

use super::ApiState;

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/:pid/tasks   ·   GET /tasks
// ────────────────────────────────────────────────────────────────────────────

/// `Task` → `TaskResponse`：派生字段（is_blocked / blocked_by / depth /
/// parent_id）由 store 补齐。两个列表口子共用这一份，别再抄第二遍。
async fn respond(
    state: &ApiState,
    tasks: Vec<Task>,
) -> Result<Json<Vec<crate::types::TaskResponse>>, crate::error::PmError> {
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        let tid = task.id.clone();
        let depth = state.store.task_depth(&tid);
        let parent_id = state.store.parent_of(&tid);
        let blocked_by = state.store.compute_blocked_by(&tid).await?;
        out.push(crate::types::TaskResponse {
            task,
            is_blocked: !blocked_by.is_empty(),
            blocked_by,
            depth,
            parent_id,
        });
    }
    Ok(Json(out))
}

#[tracing::instrument(skip(state))]
pub async fn list(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    query: axum::extract::Query<TaskFilter>,
) -> Result<Json<Vec<crate::types::TaskResponse>>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let mut filter = query.0;
    filter.project_id = Some(pid);
    let tasks = state.store.find_tasks(&filter).await?;
    respond(&state, tasks).await
}

/// 跨项目任务列表。`?assignee=<instance_id>` 是主用法（单聊右侧的
/// "这个 bot 的任务"面板）；其余 `TaskFilter` 字段原样生效。
///
/// 不带 `project_id` 时必须带一个收窄条件（`TaskFilter::is_narrowed`），与
/// MCP 的 `pm_list_tasks` 同一条规则 —— 否则整库任务的响应体就是一次
/// 几十 MB 的上下文炸弹 + 每个任务一次 `compute_blocked_by` 扫描。
///
/// 不带 `project_id` 时 `find_tasks` 走一遍内存索引（与 `/projects/counts`
/// 同一代价），不会逐个去盘上捞。
#[tracing::instrument(skip(state))]
pub async fn list_all(
    State(state): State<ApiState>,
    query: axum::extract::Query<TaskFilter>,
) -> Result<Json<Vec<crate::types::TaskResponse>>, crate::error::PmError> {
    let filter = query.0;
    if filter.project_id.is_none() && !filter.is_narrowed() {
        return Err(crate::error::PmError::BadRequest(
            TaskFilter::CROSS_PROJECT_NEEDS_NARROWING.to_string(),
        ));
    }
    let tasks = state.store.find_tasks(&filter).await?;
    respond(&state, tasks).await
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects/:pid/tasks
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn create(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<CreateTask>,
) -> Result<Json<Task>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let created_by = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    // ADR-076 §决策 11: REST 面 = 人类操作面 → creator 恒为 User。
    // Dispatch is no longer triggered from here by an event hook (the old
    // Design 27 §5.4 behavior): the single trigger source is
    // `dispatch::spawn_scanner`, scanning for `start_at` due — see that
    // module's docs.
    let task = state
        .store
        .create_task_as(&pid, input, created_by, MemberKind::User)
        .await?;
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

/// 获取任务详情（响应中自动补 `is_blocked` / `blocked_by` / `depth`）。
#[tracing::instrument(skip(state))]
pub async fn get(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<crate::types::TaskResponse>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let task = state
        .store
        .get_task(&tid)
        .await?
        .ok_or(crate::error::PmError::TaskNotFound(tid.to_string()))?;
    let depth = state.store.task_depth(&tid);
    let parent_id = state.store.parent_of(&tid);
    let blocked_by = state.store.compute_blocked_by(&tid).await?;

    Ok(Json(crate::types::TaskResponse {
        task,
        is_blocked: !blocked_by.is_empty(),
        blocked_by,
        depth,
        parent_id,
    }))
}

// ────────────────────────────────────────────────────────────────────────────
// PATCH /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn update(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    Json(input): Json<UpdateTask>,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let before = state.store.get_task(&tid).await?;
    let task = state.store.update_task(&tid, input).await?;
    // Design 27 §2.1 T2-a: a manual PATCH into a terminal status ends the
    // target agent's logical execution context for this task.
    if let Some(before) = before {
        close_task_context_if_terminal(&state.gateway, &before, &task).await;
    }
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

/// 删除任务。
///
/// Query: `?cascade=true` 强制级联删除子树（默认 false，有子任务时返回 409）。
#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    query: axum::extract::Query<DeleteTaskQuery>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    // Design 27 §2.1 T2-a: deletion ends the task's logical execution
    // context like a terminal status would — snapshot the subtree first
    // (the snapshot is taken before the delete removes the rows).
    let closing = state
        .store
        .closing_task_contexts(&tid)
        .await
        .unwrap_or_default();
    state.store.delete_task(&tid, query.cascade).await?;
    close_task_contexts(&state.gateway, closing).await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteTaskQuery {
    #[serde(default)]
    pub cascade: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// PATCH /tasks/:tid/parent
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn reparent(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    Json(input): Json<ReparentTask>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    state.store.reparent_task(&tid, input).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/claim
// ────────────────────────────────────────────────────────────────────────────

/// Agent 认领任务（pending → in_progress）。
///
/// Header `X-Actor` 携带身份（Gateway 反代覆盖为可信值；REST 路径默认
/// `human`，MCP 路径经 `X-MCP-Actor` 注入 `agent_instance_id`，ADR-073）。
/// 返回 409 若被依赖阻塞。
#[tracing::instrument(skip(state))]
pub async fn claim(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let actor = headers.get("x-actor").and_then(|v| v.to_str().ok()).ok_or(
        crate::error::PmError::Internal("missing X-Actor header".to_string()),
    )?;
    // Design 27 §5.4: no dispatch here — the claimer is the assignee itself
    // (it is already awake, doing the claiming). Push dispatch happens at
    // the moment of *assignment* (`create` with an agent assignee, or
    // approval of an agent-created task).
    let task = state.store.claim_task(&tid, actor).await?;
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/dispatch/retry
// ────────────────────────────────────────────────────────────────────────────

/// 重派：把一张被扫描器放弃（`dispatch_attempts >= MAX`）的任务放回候选集。
///
/// 只结束当前派发代次（`dispatch_attempts` / `dispatch_error` /
/// `dispatch_message_id` 清空），任务仍是 `pending`：真正的投递由扫描器下一
/// 个 tick 完成，与首次派发走同一条路径。人类在任务详情面点「重派」，不再需
/// 要先把状态改出去再改回来绕过放弃闸门。
#[tracing::instrument(skip(state))]
pub async fn retry_dispatch(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    // 与 claim / review 同一条约定：写路径必须有操作者（Gateway 反代注入
    // 可信值，Desktop 客户端固定带 `X-Actor`）。重派会留一条活动流，缺身份
    // 就不是"未知人做了件事"，而是调用方接错了线。
    let actor = headers.get("x-actor").and_then(|v| v.to_str().ok()).ok_or(
        crate::error::PmError::Internal("missing X-Actor header".to_string()),
    )?;
    // One store call: the reset and its activity entry share a write, so a
    // 500 here can never mean "re-armed but nobody knows who".
    let task = state.store.retry_dispatch(&tid, actor).await?;
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/submit
// ────────────────────────────────────────────────────────────────────────────

/// Agent 提交结果（in_progress → submitted）。
///
/// Header `X-Actor` 携带身份（见 `claim` 注释；MCP 路径下为 instance_id）。
#[tracing::instrument(skip(state, input))]
pub async fn submit(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<SubmitTaskRequest>,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let actor = headers.get("x-actor").and_then(|v| v.to_str().ok()).ok_or(
        crate::error::PmError::Internal("missing X-Actor header".to_string()),
    )?;
    let task = state
        .store
        .submit_task(&tid, &input.text, input.attachment_ids, vec![], actor)
        .await?;
    Ok(Json(task))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitTaskRequest {
    pub text: String,
    #[serde(default)]
    pub attachment_ids: Vec<AttachmentId>,
}

// ────────────────────────────────────────────────────────────────────────────
// POST /tasks/:tid/review
// ────────────────────────────────────────────────────────────────────────────

/// 人类审核（仅 `type=checkpoint/bug` 等 requires_review=true 的任务）。
#[tracing::instrument(skip(state, input))]
pub async fn review(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<ReviewTaskRequest>,
) -> Result<Json<Task>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let reviewer = headers.get("x-actor").and_then(|v| v.to_str().ok()).ok_or(
        crate::error::PmError::Internal("missing X-Actor header".to_string()),
    )?;
    // review 可由 human 完成（REST 路径默认 X-Actor=human）或 instance 完成（MCP 路径）
    let before = state.store.get_task(&tid).await?;
    let task = state
        .store
        .review_task(&tid, input.approved, reviewer, input._comment.as_deref())
        .await?;
    // Design 27 §5.4 (revised): creation approval does **not** dispatch
    // immediately here — dispatch is triggered solely by
    // `dispatch::spawn_scanner` on `start_at` due (the task stays `pending`
    // after approval and the scanner picks it up on the next tick). A
    // terminal transition still closes the execution context.
    if let Some(before) = before {
        close_task_context_if_terminal(&state.gateway, &before, &task).await;
    }
    Ok(Json(task))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTaskRequest {
    pub approved: bool,
    /// 审核正文（拒绝理由 / 批准附言）。字段名沿用既有客户端契约
    /// （Desktop `reviewTask()`、Mobile），服务端把它落到活动流而不是丢弃。
    #[serde(default)]
    pub _comment: Option<String>,
}

// ────────────────────────────────────────────────────────────────────────────
// GET|POST /tasks/:tid/activity
// ────────────────────────────────────────────────────────────────────────────

/// 任务活动历史（提交 / 审核 / 回复），按时间升序。
#[tracing::instrument(skip(state))]
pub async fn list_activity(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<Vec<TaskActivity>>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let entries = state.store.list_activity(&tid).await?;
    Ok(Json(entries))
}

/// 追加一条回复（人类在任务面上回复 Agent 的提交）。
///
/// 回复**不是**状态变更：不动 `status` / `review_status`，只进活动流，
/// 所以它和审核是两个动作——批准/拒绝走 `/review`，纯回复走这里。
#[tracing::instrument(skip(state, input))]
pub async fn add_activity(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<AddActivityRequest>,
) -> Result<Json<TaskActivity>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let actor = headers.get("x-actor").and_then(|v| v.to_str().ok()).ok_or(
        crate::error::PmError::Internal("missing X-Actor header".to_string()),
    )?;
    let entry = state
        .store
        .add_task_comment(&tid, &input.text, actor)
        .await?;
    Ok(Json(entry))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddActivityRequest {
    pub text: String,
}

// ────────────────────────────────────────────────────────────────────────────
// GET /tasks/:tid/children
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn list_children(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
) -> Result<Json<Vec<Task>>, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    let children = state.store.list_children(&tid).await?;
    Ok(Json(children))
}

/// 待导出辅助：review_status 类型别名（避免 handler 引用未使用项）。
#[allow(dead_code)]
pub type _ReviewStatus = ReviewStatus;

// ────────────────────────────────────────────────────────────────────────────
// Design 27 §5.4 / §2.1 T2-a — group-chat dispatch & context close
// ────────────────────────────────────────────────────────────────────────────

/// Provisioner from the Gateway config; `None` when PM runs without
/// `--gateway-url` (single-component dev) — every dispatch/close hook
/// degrades to a no-op there.
pub(crate) fn provisioner(
    gateway: &Option<(String, Option<String>)>,
) -> Option<crate::gateway_client::GatewayClient> {
    gateway
        .as_ref()
        .map(|(url, token)| crate::gateway_client::GatewayClient::new(url.clone(), token.clone()))
}

/// End the assignee agents' logical execution contexts for `tasks`
/// (§2.1 T2-a). Only agent-assigned tasks have a context to close.
pub(crate) async fn close_task_contexts(
    gateway_infra: &Option<(String, Option<String>)>,
    tasks: impl IntoIterator<Item = (ProjectId, Task)>,
) {
    let Some(gateway) = provisioner(gateway_infra) else {
        return;
    };
    for (pid, task) in tasks {
        let Some(assignee) = task.assignee.as_deref() else {
            continue;
        };
        if assignee == "human" {
            continue;
        }
        gateway
            .close_task_context(pid.as_str(), task.id.as_str(), assignee)
            .await;
    }
}

/// PATCH/review convenience: only a *transition into* a terminal status
/// ends the context (`done` / `rejected` / `cancelled`).
async fn close_task_context_if_terminal(
    gateway: &Option<(String, Option<String>)>,
    before: &Task,
    after: &Task,
) {
    if before.status == after.status {
        return;
    }
    if matches!(
        after.status,
        TaskStatus::Done | TaskStatus::Rejected | TaskStatus::Cancelled
    ) {
        close_task_contexts(gateway, [(after.project_id.clone(), after.clone())]).await;
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PmConfig;
    use crate::mcp::NoopAgentDirectory;
    use crate::store::contract::PmStore;
    use crate::store::sqlite::SqlitePmStore;
    use crate::types::{CreateProject, Priority, TaskType};
    use std::sync::Arc;
    use tempfile::tempdir;

    fn test_config() -> PmConfig {
        let dir = tempdir().unwrap();
        PmConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        }
    }

    /// 两个项目、各一条任务，分别指派给两个 agent —— 跨项目查询只有
    /// `assignee` 能把它们分开。
    async fn seed(store: &SqlitePmStore) {
        for (pid_title, assignee) in [("P-a", "agent-1"), ("P-b", "agent-2")] {
            let p = store
                .create_project(
                    CreateProject {
                        title: pid_title.into(),
                        description: String::new(),
                        metadata: Default::default(),
                    },
                    "human",
                )
                .await
                .unwrap();
            // assignee 必须是项目成员（store 的 `assignee_not_project_member` 规则）。
            store
                .add_project_member(&p.id, assignee, MemberKind::Agent)
                .await
                .unwrap();
            store
                .create_task(
                    &p.id,
                    CreateTask {
                        title: format!("task for {assignee}"),
                        description: String::new(),
                        task_type: TaskType::Task,
                        priority: Priority::Normal,
                        parent_task_id: None,
                        depends_on: vec![],
                        attachment_ids: vec![],
                        assignee: Some(assignee.into()),
                        due_at: None,
                        start_at: None,
                    },
                    "human",
                )
                .await
                .unwrap();
        }
    }

    fn state_with(store: SqlitePmStore) -> ApiState {
        ApiState {
            store: Arc::new(store),
            config: test_config(),
            agent_dir: Arc::new(NoopAgentDirectory),
            gateway: None,
        }
    }

    #[tokio::test]
    async fn cross_project_list_refuses_an_unfiltered_dump() {
        let store = SqlitePmStore::open(test_config()).unwrap();
        seed(&store).await;

        // 没有收窄条件 → 400，而不是把整库任务吐出来（与 MCP 同一条规则）。
        let err = list_all(State(state_with(store)), axum::extract::Query(TaskFilter::default()))
            .await
            .unwrap_err();
        assert!(
            matches!(err, crate::error::PmError::BadRequest(ref m)
                if m.contains("narrowing filter")),
            "unfiltered cross-project list must be refused: {err:?}"
        );
    }

    #[tokio::test]
    async fn assignee_filter_spans_projects() {
        let store = SqlitePmStore::open(test_config()).unwrap();
        seed(&store).await;

        let Json(tasks) = list_all(
            State(state_with(store)),
            axum::extract::Query(TaskFilter {
                assignee: Some("agent-1".into()),
                ..Default::default()
            }),
        )
        .await
        .unwrap();

        // 一个 agent 的任务散落在多个项目里，跨项目一次取回。
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task.assignee.as_deref(), Some("agent-1"));
        assert_eq!(tasks[0].task.title, "task for agent-1");
    }
}
