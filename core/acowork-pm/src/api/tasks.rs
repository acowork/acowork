//! `tasks` handlers（**P1 实现**）。内部路径不带 `/api` 前缀，
//! 公开路径为 `/api/pm/tasks/*`（见 [`routes::pm_router`]）。

use axum::extract::{Path, State};
use axum::Json;

use crate::types::{
    AttachmentId, CreateTask, MemberKind, ProjectId, ReparentTask, ReviewStatus, Task, TaskFilter,
    TaskId, TaskStatus, UpdateTask,
};

use super::ApiState;
use crate::store::tree::PmStore;

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/:pid/tasks
// ────────────────────────────────────────────────────────────────────────────

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
    let mut out = Vec::with_capacity(tasks.len());
    for task in tasks {
        let tid = task.id.clone();
        let depth = state.store.index_entry(&tid).map(|e| e.depth).unwrap_or(0);
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
    // Design 27 §5.4: a human creating a task *for an agent* is an act of
    // assignment — the assignee gets a push dispatch into the project group
    // chat. (Approval of agent-created tasks dispatches from `review`; the
    // MCP face creates with `review_status=pending` and no dispatch.)
    let task = state
        .store
        .create_task_as(&pid, input, created_by, MemberKind::User)
        .await?;
    if task.review_status == ReviewStatus::NotRequired {
        dispatch_task_via_gateway(&state, &task).await;
    }
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
    let depth = state.store.index_entry(&tid).map(|e| e.depth).unwrap_or(0);
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
        close_task_context_if_terminal(&state, &before, &task).await;
    }
    Ok(Json(task))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /tasks/:tid
// ────────────────────────────────────────────────────────────────────────────

/// 删除任务。
///
/// Query: `?cascade=true` 强制级联删除子树（默认 false，子任务被提升为顶层）。
#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(tid): Path<String>,
    query: axum::extract::Query<DeleteTaskQuery>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let tid = tid.parse::<TaskId>()?;
    // Design 27 §2.1 T2-a: deletion ends the task's logical execution
    // context like a terminal status would — snapshot the subtree first
    // (`delete_task` cascades regardless of the flag).
    let closing = state.store.closing_task_contexts(&tid).await.unwrap_or_default();
    state.store.delete_task(&tid, query.cascade).await?;
    close_task_contexts(&state, closing).await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteTaskQuery {
    #[serde(default)]
    pub cascade: bool,
    /// `?promote_children=true` —— 子任务提升为父项目的顶层（默认 false，cascade 时忽略）。
    #[serde(default)]
    pub promote_children: bool,
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
    let actor = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal("missing X-Actor header".to_string()))?;
    // Design 27 §5.4: no dispatch here — the claimer is the assignee itself
    // (it is already awake, doing the claiming). Push dispatch happens at
    // the moment of *assignment* (`create` with an agent assignee, or
    // approval of an agent-created task).
    let task = state.store.claim_task(&tid, actor).await?;
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
    let actor = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal("missing X-Actor header".to_string()))?;
    let task = state
        .store
        .submit_task(&tid, &input.text, input.attachment_ids, actor)
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
    let reviewer = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .ok_or(crate::error::PmError::Internal("missing X-Actor header".to_string()))?;
    // review 可由 human 完成（REST 路径默认 X-Actor=human）或 instance 完成（MCP 路径）
    let before = state.store.get_task(&tid).await?;
    let task = state.store.review_task(&tid, input.approved, reviewer).await?;
    // Design 27 §5.4 / §2.1 T2-a: approving an agent-created task (still
    // `pending`, awaiting creation approval) is the human's act of handing
    // the work over — dispatch it. Anything that lands the task on a
    // terminal status (result approval → done, any reject → rejected,
    // creation rejects included) ends the assignee's logical execution
    // context.
    if task.status == TaskStatus::Pending {
        let was_awaiting_approval = before
            .as_ref()
            .is_some_and(|b| b.review_status == ReviewStatus::Pending);
        if input.approved && was_awaiting_approval {
            dispatch_task_via_gateway(&state, &task).await;
        }
    } else if let Some(before) = before {
        close_task_context_if_terminal(&state, &before, &task).await;
    }
    Ok(Json(task))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTaskRequest {
    pub approved: bool,
    #[serde(default)]
    pub _comment: Option<String>,
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
fn provisioner(state: &ApiState) -> Option<crate::gateway_client::GatewayClient> {
    state
        .gateway
        .as_ref()
        .map(|(url, token)| crate::gateway_client::GatewayClient::new(url.clone(), token.clone()))
}

/// Push-dispatch a task as a task-keyed `intent_request` into the project
/// group chat (§5.4: assignment is dispatch). No-op without a Gateway
/// config or without an agent assignee; failures are logged inside the
/// provisioner and never fail the PM mutation.
pub(crate) async fn dispatch_task_via_gateway(state: &ApiState, task: &Task) {
    let Some(gateway) = provisioner(state) else { return };
    let Some(assignee) = task.assignee.as_deref() else { return };
    if assignee == "human" {
        return;
    }
    let message = if task.description.trim().is_empty() {
        task.title.clone()
    } else {
        format!("{}\n{}", task.title, task.description.trim())
    };
    gateway
        .dispatch_task(
            task.project_id.as_str(),
            task.id.as_str(),
            assignee,
            &task.created_by,
            &task.title,
            &message,
        )
        .await;
}

/// End the assignee agents' logical execution contexts for `tasks`
/// (§2.1 T2-a). Only agent-assigned tasks have a context to close.
pub(crate) async fn close_task_contexts(
    state: &ApiState,
    tasks: impl IntoIterator<Item = (ProjectId, Task)>,
) {
    let Some(gateway) = provisioner(state) else { return };
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
async fn close_task_context_if_terminal(state: &ApiState, before: &Task, after: &Task) {
    if before.status == after.status {
        return;
    }
    if matches!(
        after.status,
        TaskStatus::Done | TaskStatus::Rejected | TaskStatus::Cancelled
    ) {
        close_task_contexts(state, [(after.project_id.clone(), after.clone())]).await;
    }
}
