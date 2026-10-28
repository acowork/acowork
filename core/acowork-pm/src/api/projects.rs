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
        let owner = project.owner().to_string();
        tokio::spawn(async move {
            provisioner.ensure_group(&pid, &title, &participants, &owner).await;
        });
    }
    Ok(Json(project))
}

/// The group chat's participant set: the owner plus whoever the
/// store recorded as members (both faces seed the creator into `members`;
/// `owner_id` — set on an ownership transfer — takes precedence over
/// `created_by` via `Project::owner`). `sync_group` **converges** the set to
/// exactly this list, so the owner must always be in it — a members-only
/// list would evict the human owner from the group chat (and flip the
/// session's `has_human_participant` fact) on the first member add/remove.
pub(crate) fn group_participants(project: &Project) -> Vec<String> {
    let mut participants: Vec<String> = std::iter::once(project.owner().to_string())
        .chain(project.members.iter().map(|m| m.instance_id.clone()))
        .collect();
    participants.sort();
    participants.dedup();
    participants
}

/// Push a project's current group facts — roster, **owner** and **title** —
/// into the chat domain through the Gateway (design 27 §8.3). Every mutation
/// that can change one of them (member add / remove, ownership transfer, a
/// rename) calls this, so the project group's ADR-098 governor (the id allowed
/// to write its `chat.md`) and its displayed name never drift from the
/// project's.
///
/// Best-effort, same rule as provisioning: the PM store is the source of truth,
/// the next call converges, and a failure is logged by the client rather than
/// surfaced to the caller. No-op without a Gateway (standalone / test mode).
pub(crate) fn sync_group_chat(state: &ApiState, project: &Project) {
    let Some((gw_url, token)) = &state.gateway else {
        return;
    };
    let provisioner = crate::gateway_client::GatewayClient::new(gw_url.clone(), token.clone());
    let pid_str = project.id.as_str().to_string();
    let title = project.title.clone();
    // Converged set includes the owner — see `group_participants`.
    let participants = group_participants(project);
    let owner = project.owner().to_string();
    tokio::spawn(async move {
        provisioner
            .sync_group(&pid_str, &title, &participants, &owner)
            .await;
    });
}

// ────────────────────────────────────────────────────────────────────────────
// Governance — who may mutate a project
// ────────────────────────────────────────────────────────────────────────────

/// ADR-076 §决策 12: under `local` the Gateway has no account store, so every
/// REST call carries this constant actor. One machine, one human — there is no
/// cross-account to protect against, so the local actor governs every project,
/// including one an agent created through `pm_create_project` (whose owner id
/// is an instance id no human header can ever match).
const LOCAL_ACTOR: &str = "human";

/// Only the project's owner (`Project::owner()` = `owner_id ?? created_by`) or
/// a platform admin may mutate the project itself — rename / description /
/// status (PATCH), delete, the member roster (add / remove), and the ownership
/// transfer. Before this the check existed solely on `transfer_owner`, so a
/// plain member could delete a project they were merely invited to while the
/// owner had no more say than a stranger.
///
/// The caller identity is the `x-actor` / `x-actor-admin` pair the Gateway
/// reverse proxy injects from the authenticated token; a client-claimed value
/// is dropped there (`acowork-gateway/src/http/pm_proxy.rs`), so PM can trust
/// them as identity. PM has no account store of its own, and a PM socket
/// reachable by anything other than its own Gateway is outside this design (it
/// binds loopback by default).
fn require_governance(
    headers: &axum::http::HeaderMap,
    project: &Project,
    action: &str,
) -> Result<(), crate::error::PmError> {
    let actor = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    if actor == LOCAL_ACTOR {
        return Ok(());
    }
    let actor_admin = headers
        .get("x-actor-admin")
        .and_then(|v| v.to_str().ok())
        == Some("true");
    if actor == project.owner() || actor_admin {
        return Ok(());
    }
    Err(crate::error::PmError::Forbidden(format!(
        "only the project owner or an admin can {action}"
    )))
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
    headers: axum::http::HeaderMap,
    Json(input): Json<UpdateProject>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let before = state
        .store
        .get_project(&pid)
        .await?
        .ok_or_else(|| crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    require_governance(&headers, &before, "update the project")?;
    // Design 27 §2.1 T2-a: completing a project ends every agent task
    // context under it. `closing_project_contexts` must run before the
    // mutation (afterwards the status is gone).
    let closing = if input.status == Some(crate::types::ProjectStatus::Completed)
        && before.status != crate::types::ProjectStatus::Completed
    {
        Some(pid.clone())
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
    // The project channel is named after the project (the chat domain refuses
    // a user-side rename of a `project_group`), so a rename has to travel to
    // the session row — otherwise the sidebar keeps showing the old title until
    // the next member churn. Same converge-on-push path as the roster/owner.
    if project.title != before.title {
        sync_group_chat(&state, &project);
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
    headers: axum::http::HeaderMap,
    query: axum::extract::Query<DeleteProjectQuery>,
) -> Result<axum::http::StatusCode, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let project = state
        .store
        .get_project(&pid)
        .await?
        .ok_or_else(|| crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    require_governance(&headers, &project, "delete the project")?;
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
    headers: axum::http::HeaderMap,
    Json(input): Json<AddProjectMember>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // Inviting is a governance act, not a membership right: owner or admin
    // only (a member must not be able to widen a project they were invited to).
    let current = state
        .store
        .get_project(&pid)
        .await?
        .ok_or_else(|| crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    require_governance(&headers, &current, "add a member")?;
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
    sync_group_chat(&state, &project);
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
    headers: axum::http::HeaderMap,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    let current = state
        .store
        .get_project(&pid)
        .await?
        .ok_or_else(|| crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    require_governance(&headers, &current, "remove a member")?;
    let project = state
        .store
        .remove_project_member(&pid, &instance_id)
        .await?;
    // Design 27 §8.3: the removed member leaves the project group chat
    // too — participant removal is what revokes their read access to the
    // group's execution sessions (the chat history itself is kept).
    sync_group_chat(&state, &project);
    Ok(Json(project))
}

// ────────────────────────────────────────────────────────────────────────────
// POST /projects/:pid/transfer-owner
// ────────────────────────────────────────────────────────────────────────────

/// `POST /api/pm/projects/:pid/transfer-owner` 请求体。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferOwner {
    /// 新群主的 user_id（必须是当前成员里的人类账号）。
    pub new_owner: String,
}

/// 转让群主。原群主降级为普通成员（store 层），新群主必须是现有
/// 人类成员。只有**当前群主**或 **admin** 能发起：调用者身份取自网
/// 关注入的可信 `X-Actor`，是否 admin 取自可信 `x-actor-admin`（网
/// 关从 token 推导并覆盖客户端伪造值）。成功后走 `sync_group` 收敛
/// 群聊参与者（与加/减成员同一条路径）。
#[tracing::instrument(skip(state, input))]
pub async fn transfer_owner(
    State(state): State<ApiState>,
    Path(pid): Path<String>,
    headers: axum::http::HeaderMap,
    Json(input): Json<TransferOwner>,
) -> Result<Json<Project>, crate::error::PmError> {
    let pid = pid.parse::<ProjectId>()?;
    // 鉴权需要先拿到当前 owner。
    let current = state
        .store
        .get_project(&pid)
        .await?
        .ok_or_else(|| crate::error::PmError::ProjectNotFound(pid.to_string()))?;
    require_governance(&headers, &current, "transfer group ownership")?;

    let project = state.store.transfer_owner(&pid, &input.new_owner).await?;
    // Design 27 §8.3: owner moved + old owner demoted to member → converge
    // the group chat participant set and fire the doorbell (same as add/remove).
    // The new owner rides the same call, so the group's `chat.md` governance
    // moves with the project instead of staying on the demoted former owner.
    sync_group_chat(&state, &project);
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
            owner_id: None,
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

    // ── The governance gate (HTTP surface) ──────────────────────────────

    use crate::api::routes::pm_router;
    use crate::{NoopAgentDirectory, PmConfig};
    use axum::body::Body;
    use axum::http::{header, Method, Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt;

    async fn test_router() -> (axum::Router, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = PmConfig {
            data_dir: tmp.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        };
        let store = Arc::new(crate::TreePmStore::new(cfg.clone()).await.unwrap());
        (pm_router(store, cfg, Arc::new(NoopAgentDirectory), None), tmp)
    }

    /// One call as `actor` (optionally an admin, per the trusted
    /// `x-actor-admin` the Gateway injects) → the status code.
    async fn call(
        router: &axum::Router,
        method: Method,
        uri: &str,
        actor: &str,
        admin: bool,
        body: &str,
    ) -> StatusCode {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-actor", actor);
        if admin {
            builder = builder.header("x-actor-admin", "true");
        }
        let resp = router
            .clone()
            .oneshot(
                builder
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.status()
    }

    async fn create_project(router: &axum::Router, actor: &str, title: &str) -> String {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/projects")
                    .header("x-actor", actor)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"title":"{title}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// The project mutation gate. PM checked the caller only on ownership
    /// transfer, so any authenticated account could rename, delete or
    /// re-roster a project it had merely been invited to — the owner had no
    /// more say than a stranger. Now: owner or admin, on all four surfaces.
    #[tokio::test]
    async fn project_mutations_need_the_owner_or_an_admin() {
        let (router, _tmp) = test_router().await;
        let pid = create_project(&router, "u-owner", "T").await;
        assert_eq!(
            call(
                &router,
                Method::POST,
                &format!("/projects/{pid}/members"),
                "u-owner",
                false,
                r#"{"instance_id":"u-member","kind":"user"}"#,
            )
            .await,
            StatusCode::OK,
            "the owner may invite"
        );

        let mutations = [
            ("PATCH", format!("/projects/{pid}"), r#"{"title":"Hijacked"}"#),
            (
                "POST",
                format!("/projects/{pid}/members"),
                r#"{"instance_id":"u-second","kind":"user"}"#,
            ),
            ("DELETE", format!("/projects/{pid}/members/u-member"), ""),
            ("DELETE", format!("/projects/{pid}"), ""),
        ];
        for (label, uri, body) in mutations {
            let method: Method = label.parse().unwrap();
            assert_eq!(
                call(&router, method.clone(), &uri, "u-member", false, body).await,
                StatusCode::FORBIDDEN,
                "a plain member may not {label} {uri}"
            );
        }

        assert_eq!(
            call(&router, Method::PATCH, &format!("/projects/{pid}"), "u-admin", true, r#"{"title":"Renamed by admin"}"#).await,
            StatusCode::OK,
            "an admin may (the flag is Gateway-injected, never client-claimed)"
        );
        assert_eq!(
            call(&router, Method::PATCH, &format!("/projects/{pid}"), "u-owner", false, r#"{"title":"Renamed by owner"}"#).await,
            StatusCode::OK,
            "the owner may"
        );
    }

    /// `local` mode has no account store: every call carries the constant
    /// actor `"human"` (ADR-076 §决策 12), including on a project an agent
    /// created through `pm_create_project` — whose owner is an instance id no
    /// human header can match. Without this exemption a single-user install
    /// loses every project mutation as soon as an agent files one.
    #[tokio::test]
    async fn the_local_actor_governs_an_agent_created_project() {
        let (router, _tmp) = test_router().await;
        let pid = create_project(&router, "com.acowork.agent-instance", "Filed by an agent").await;
        assert_eq!(
            call(&router, Method::PATCH, &format!("/projects/{pid}"), "human", false, r#"{"title":"Renamed by the local human"}"#).await,
            StatusCode::OK
        );
        assert_eq!(
            call(&router, Method::DELETE, &format!("/projects/{pid}"), "human", false, "").await,
            StatusCode::NO_CONTENT
        );
    }

    /// A rename has to reach the group chat's title — the chat domain refuses
    /// a user-side rename of a `project_group`, so PM's push is the only path.
    /// The handler converges on title change only, so a description edit does
    /// not fire a group refresh.
    #[tokio::test]
    async fn a_rename_reaches_the_group_title_only_when_the_title_changed() {
        let (router, _tmp) = test_router().await;
        let pid = create_project(&router, "u-owner", "Before").await;
        assert_eq!(
            call(&router, Method::PATCH, &format!("/projects/{pid}"), "u-owner", false, r#"{"description":"just a note"}"#).await,
            StatusCode::OK
        );
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/projects/{pid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["title"], "Before", "a description edit must not touch the title");
        assert_eq!(v["description"], "just a note");
    }
}
