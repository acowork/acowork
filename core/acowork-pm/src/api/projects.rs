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
        let provisioner = crate::gateway_client::GatewayClient::new(
            gw_url.clone(),
            token.clone(),
        );
        let pid = project.id.as_str().to_string();
        let title = project.title.clone();
        let participants = group_participants(&project);
        tokio::spawn(async move {
            provisioner.ensure_group(&pid, &title, &participants).await;
        });
    }
    Ok(Json(project))
}

/// The project group chat's participant set: the owner plus whoever the
/// store recorded as members (MCP face seeds the creator there; REST face
/// leaves the list at the owner). `sync_group` **converges** the set to
/// exactly this list, so the owner must always be in it — a members-only
/// list would evict the human owner from the group chat (and flip the
/// session's `has_human_participant` fact) on the first member add/remove.
pub(crate) fn group_participants(project: &Project) -> Vec<String> {
    let mut participants: Vec<String> = std::iter::once(project.created_by.clone())
        .chain(project.members.iter().map(|m| m.instance_id.clone()))
        .collect();
    participants.sort();
    participants.dedup();
    participants
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

/// 添加项目成员（Agent 实例或人类账号，ADR-076 §决策 11）。
///
/// `kind: "agent"`（缺省）时按 `AgentDirectory::agent_exists`
/// （设计 §9.1 / ADR-073，instance_id 维度）校验 Agent 存在；
/// `kind: "user"` 跳过该校验（PM 服务无用户目录，user_id 由前端从
/// 已认证的账号目录取得）。重复添加 → 409 `member_already_exists`。
#[tracing::instrument(skip(state, input))]
pub async fn add_member(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    Json(input): Json<AddProjectMember>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // 校验 Agent 存在（宽松目录跳过）。Agent 成员必须是真实存在的实例；
    // 人类成员只受“同一 instance_id 不能重复”约束（store 层）。
    if input.kind == MemberKind::Agent
        && !state.agent_dir.agent_exists(&input.instance_id).await
    {
        return Err(crate::error::PmError::BadRequest(format!(
            "agent instance not found in agent directory: {}",
            input.instance_id
        )));
    }
    let project = state
        .store
        .add_project_member(&pid, &input.instance_id, input.kind)
        .await?;
    // Design 27 Phase 2 + §8.3: a new project member joins the project
    // group chat too. `sync_group` converges the participant set to the
    // project's current members — the old `ensure_group` call was idempotent
    // on `project_id` but never actually added the newcomer.
    if let Some((gw_url, token)) = &state.gateway {
        let provisioner = crate::gateway_client::GatewayClient::new(
            gw_url.clone(),
            token.clone(),
        );
        let pid_str = pid.as_str().to_string();
        // Converged set includes the owner — see `group_participants`.
        let participants = group_participants(&project);
        tokio::spawn(async move {
            provisioner.sync_group(&pid_str, &participants).await;
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
    // Design 27 §8.3: the removed member leaves the project group chat
    // too — participant removal is what revokes their read access to the
    // group's execution sessions (the chat history itself is kept).
    if let Some((gw_url, token)) = &state.gateway {
        let provisioner = crate::gateway_client::GatewayClient::new(
            gw_url.clone(),
            token.clone(),
        );
        let pid_str = pid.as_str().to_string();
        // Converged set includes the owner — see `group_participants`.
        let participants = group_participants(&project);
        tokio::spawn(async move {
            provisioner.sync_group(&pid_str, &participants).await;
        });
    }
    Ok(Json(project))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ProjectMember, ProjectStatus};

    fn project(created_by: &str, members: &[&str]) -> Project {
        Project {
            id: "p-test0000".parse().unwrap(),
            title: "T".into(),
            description: String::new(),
            status: ProjectStatus::Active,
            created_by: created_by.into(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            metadata: Default::default(),
            members: members
                .iter()
                .map(|id| ProjectMember {
                    instance_id: (*id).into(),
                    kind: crate::types::MemberKind::Agent,
                    added_at: chrono::Utc::now(),
                })
                .collect(),
        }
    }

    #[test]
    fn group_participants_always_include_the_owner() {
        // Regression: `sync_group` converges the participant set to
        // exactly what PM sends — a members-only list evicted the human
        // owner on the first member add.
        let p = project("u-owner", &["ag1", "ag2"]);
        let got = group_participants(&p);
        assert!(got.contains(&"u-owner".to_string()), "{got:?}");
        assert!(got.contains(&"ag1".to_string()) && got.contains(&"ag2".to_string()));
        // Creator seeded as a member (MCP face) must not appear twice.
        let p = project("u-owner", &["u-owner", "ag1"]);
        let got = group_participants(&p);
        assert_eq!(got.iter().filter(|x| *x == "u-owner").count(), 1);
        // No members yet: the owner alone is the set.
        assert_eq!(group_participants(&project("u-owner", &[])), vec!["u-owner".to_string()]);
    }
}
