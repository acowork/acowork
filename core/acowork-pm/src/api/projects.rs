//! `projects` handlers（**P1 实现**）。内部路径不带 `/api` 前缀，
//! 公开路径为 `/api/pm/projects/*`（见 [`routes::pm_router`]）。

use axum::extract::{Path, State};
use axum::Json;

use crate::types::{AddProjectMember, CreateProject, MemberKind, Project, ProjectId, UpdateProject};

use super::ApiState;
use crate::store::tree::PmStore;

// ────────────────────────────────────────────────────────────────────────────
// GET /projects
// ────────────────────────────────────────────────────────────────────────────

/// 列出所有项目。
///
/// 支持未来扩展：`?status=active&include_archived=true`。
#[tracing::instrument(skip(state))]
pub async fn list(State(state): State<ApiState>) -> Result<Json<Vec<Project>>, crate::error::PmError> {
    let projects = state.store.list_projects().await?;
    Ok(Json(projects))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects
// ────────────────────────────────────────────────────────────────────────────

/// 创建项目。
///
/// `created_by` 来自 HTTP header `X-Actor`（Gateway 注入当前用户/Agent ID）。
#[tracing::instrument(skip(state, input))]
pub async fn create(
    State(state): State<ApiState>,
    headers: axum::http::HeaderMap,
    Json(input): Json<CreateProject>,
) -> Result<Json<Project>, crate::error::PmError> {
    let created_by = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    // ADR-076 §决策 11: REST 面 = 人类操作面，actor 恒为 User（Gateway
    // 注入 token user_id；local 模式注入常量 "human"）。
    let project = state
        .store
        .create_project_as(input, created_by, MemberKind::User)
        .await?;
    // Design 27 Phase 2: every project gets exactly one group chat,
    // ensured idempotently in the chat domain through the Gateway.
    // Best-effort — a missing chat must not fail project creation.
    if let Some((gw_url, token)) = &state.gateway {
        let provisioner = crate::group_chat::GroupChatProvisioner::new(
            gw_url.clone(),
            token.clone(),
        );
        let pid = project.id.as_str().to_string();
        let title = project.title.clone();
        // Participants: the owner plus whoever the store recorded as
        // members (MCP face seeds the creator there; REST face leaves
        // the list at the owner).
        let mut participants: Vec<String> =
            std::iter::once(project.created_by.clone())
                .chain(project.members.iter().map(|m| m.instance_id.clone()))
                .collect();
        participants.sort();
        participants.dedup();
        tokio::spawn(async move {
            provisioner.ensure_group(&pid, &title, &participants).await;
        });
    }
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// GET /projects/:pid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state))]
pub async fn get(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let project = state
        .store
        .get_project(&pid)
        .await?
        .ok_or(crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// PATCH /projects/:pid
// ────────────────────────────────────────────────────────────────────────────

#[tracing::instrument(skip(state, input))]
pub async fn update(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    Json(input): Json<UpdateProject>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // Design 27 §2.1 T2-a: completing a project ends every agent task
    // context under it. `closing_project_contexts` must run before the
    // mutation (afterwards the status is gone).
    let closing = if input.status == Some(crate::types::ProjectStatus::Completed) {
        state.store.get_project(&pid).await.ok().flatten().and_then(|p| {
            if p.status != crate::types::ProjectStatus::Completed {
                Some(pid.clone())
            } else {
                None
            }
        })
    } else {
        None
    };
    let project = state.store.update_project(&pid, input).await?;
    if let Some(pid) = closing
        && let Ok(tasks) = state.store.closing_project_contexts(&pid).await
    {
        super::tasks::close_task_contexts(
            &state,
            tasks.into_iter().map(|t| (pid.clone(), t)),
        )
        .await;
    }
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /projects/:pid
// ────────────────────────────────────────────────────────────────────────────

/// 删除项目。
///
/// Query: `?cascade=true` 强制级联删除所有任务（默认 false，返回 409 若仍有任务）。
#[tracing::instrument(skip(state))]
pub async fn delete(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    query: axum::extract::Query<DeleteProjectQuery>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // Design 27 §2.1 T2-a: deleting the project ends all its agent task
    // contexts (snapshot before the cascade removes the data).
    let closing = state
        .store
        .closing_project_contexts(&pid)
        .await
        .unwrap_or_default();
    state.store.delete_project(&pid, query.cascade).await?;
    super::tasks::close_task_contexts(
        &state,
        closing.into_iter().map(|t| (pid.clone(), t)),
    )
    .await;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Deserialize)]
pub struct DeleteProjectQuery {
    #[serde(default)]
    pub cascade: bool,
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects/:pid/members
// ────────────────────────────────────────────────────────────────────────────

/// 添加项目成员（Agent 实例）。
///
/// 按 `AgentDirectory::agent_exists`（设计 §9.1 / ADR-073，instance_id 维度）
/// 校验 Agent 存在；重复添加 → 409 `member_already_exists`。
#[tracing::instrument(skip(state, input))]
pub async fn add_member(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    Json(input): Json<AddProjectMember>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // 校验 Agent 存在（宽松目录跳过）。成员必须是真实存在的 Agent 实例。
    if !state.agent_dir.agent_exists(&input.instance_id).await {
        return Err(crate::error::PmError::BadRequest(format!(
            "agent instance not found in agent directory: {}",
            input.instance_id
        )));
    }
    let project = state
        .store
        .add_project_member(&pid, &input.instance_id)
        .await?;
    // Design 27 Phase 2: a new project member joins the project group
    // chat too (idempotent — the chat store dedups on re-add).
    if let Some((gw_url, token)) = &state.gateway {
        let provisioner = crate::group_chat::GroupChatProvisioner::new(
            gw_url.clone(),
            token.clone(),
        );
        let pid_str = pid.as_str().to_string();
        let title = project.title.clone();
        let participants: Vec<String> = project
            .members
            .iter()
            .map(|m| m.instance_id.clone())
            .collect();
        tokio::spawn(async move {
            provisioner
                .ensure_group(&pid_str, &title, &participants)
                .await;
        });
    }
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// DELETE /projects/:pid/members/:instance_id
// ────────────────────────────────────────────────────────────────────────────

/// 移除项目成员。
///
/// 成员名下仍有未完成任务 → 409 `member_has_open_tasks`（显式失败，
/// 任务要先转走或完成）。成员不存在 → 404 `member_not_found`。
#[tracing::instrument(skip(state))]
pub async fn remove_member(
    State(state): State<ApiState>,
    Path((pid, instance_id)): Path<(String, String)>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let project = state
        .store
        .remove_project_member(&pid, &instance_id)
        .await?;
    Ok(Json(project))
}
