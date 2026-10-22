//! PM API 路由集中注册（[`pm_router`]）。
//!
//! ## Path 前缀约定
//!
//! **PM router 内部路径不带 `/api` 前缀**——由 Gateway 端
//! `nest_service("/api/pm", ...)` 挂载时自动加上，公开路径统一为
//! **`/api/pm/*`**（设计文档 §21：`{gw}/api/pm/...`，命名空间隔离，
//! 不与 Gateway 现有 `/api/*` 路由冲突）：
//!
//! | PM router 内部 | 公开 URL |
//! |---------------|----------|
//! | `GET /projects` | `GET /api/pm/projects` |
//! | `GET /tasks/{tid}` | `GET /api/pm/tasks/{tid}` |
//! | `GET /attachments/{aid}` | `GET /api/pm/attachments/{aid}` |
//!
//! 这种分离让 PM 既可以：
//! - 通过 Gateway nest 暴露（生产）→ 公开 `/api/pm/*` 路径
//! - 独立 dev server（开发）→ 占位 router 只 serve `/health`（P0 阶段）
//!
//! ## State 类型
//!
//! `Router`（即 `Router<()>`）—— PM 在内部用 `.with_state(ApiState)` 把 state
//! 注入并消耗掉，对外呈现为无 state 需求、可 serve 的 router。Gateway 端用
//! `nest_service("/api/pm", ...)` 挂载（axum 0.8 `nest`/`merge` 都要求内外 router
//! state 类型一致，PM 的 `()` 与 Gateway 的 `AppState` 不同，故只能用
//! `nest_service` 接受任意 `Service`）。

use std::sync::Arc;

use axum::routing::{delete, get, patch, post};
use axum::Router;

use crate::config::PmConfig;
use crate::mcp::AgentDirectory;
use crate::store::tree::TreePmStore;

use super::{attachments, icon, projects, search, tasks, ApiState};

/// 构建 PM API 路由树（内部路径**不带** `/api` 前缀）。
///
/// 返回 `Router`（`Router<()>`）—— 内部已 `.with_state(ApiState)` 注入 state，
/// 由 Gateway 端 `nest_service("/api/pm", ...)` 挂载。
///
/// `config` 当前未在 handlers 中直接使用（store 构造时已持有 config 副本），
/// 但保留在 [`ApiState`] 中供未来 P1+ 扩展使用（如 handlers 读取
/// `config.max_attachment_size` 做上传校验）。
pub fn pm_router(
    store: Arc<TreePmStore>,
    config: PmConfig,
    agent_dir: Arc<dyn AgentDirectory>,
    gateway: Option<(String, Option<String>)>,
) -> Router {
    let state = ApiState {
        store,
        config,
        agent_dir,
        gateway,
    };

    Router::new()
        // ── Projects ─────────────────────────────────────────────
        .route(
            "/projects",
            get(projects::list).post(projects::create),
        )
        .route(
            "/projects/{pid}",
            get(projects::get)
                .patch(projects::update)
                .delete(projects::delete),
        )
        .route(
            "/projects/{pid}/members",
            post(projects::add_member),
        )
        .route(
            "/projects/{pid}/members/{instance_id}",
            delete(projects::remove_member),
        )
        .route(
            "/projects/{pid}/transfer-owner",
            post(projects::transfer_owner),
        )
        // ── Project icons (custom upload) ──────────────────────────
        .route("/projects/{pid}/icon", post(icon::upload))
        .route("/projects/{pid}/icon-assets", get(icon::list_assets))
        .route("/projects/{pid}/icon-file", get(icon::serve))
        // ── Tasks ────────────────────────────────────────────────
        .route(
            "/projects/{pid}/tasks",
            get(tasks::list).post(tasks::create),
        )
        .route(
            "/tasks/{tid}",
            get(tasks::get)
                .patch(tasks::update)
                .delete(tasks::delete),
        )
        .route("/tasks/{tid}/parent", patch(tasks::reparent))
        .route("/tasks/{tid}/claim", post(tasks::claim))
        .route("/tasks/{tid}/submit", post(tasks::submit))
        .route("/tasks/{tid}/review", post(tasks::review))
        .route("/tasks/{tid}/children", get(tasks::list_children))
        // ── Attachments ──────────────────────────────────────────
        .route(
            "/tasks/{tid}/attachments",
            get(attachments::list).post(attachments::upload),
        )
        .route(
            "/attachments/{aid}",
            get(attachments::download).delete(attachments::delete),
        )
        // ── Search (ADR-081 P0-2) ────────────────────────────────
        .route("/search", get(search::search))
        .with_state(state)
}
