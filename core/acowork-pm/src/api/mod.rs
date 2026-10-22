//! HTTP API 层（axum）。
//!
//! ## 模块划分
//!
//! - [`routes`] —— 集中路由注册（`pm_router`）
//! - [`projects`] —— Projects handlers
//! - [`tasks`] —— Tasks handlers（含子任务 / claim / submit / review / reparent）
//! - [`attachments`] —— Attachments handlers + multipart upload
//!
//! ## 共享状态
//!
//! [`ApiState`] 通过 axum `State` 注入到每个 handler。
//!
//! **ADR-061 简化**：保留 `ApiState` 为 struct（不是 type alias）。
//! - `store: Arc<TreePmStore>` —— handlers 主要依赖
//! - `config: PmConfig` —— 当前为 future-use（handlers 未直接读 config；store 构造时已持有 config 副本）。
//! - **State 类型独立于 Gateway 的 `AppState`** —— [`crate::api::routes::pm_router`]
//!   在内部用 `.with_state(ApiState)` 注入并消耗 state，对外呈现为
//!   `Router<()>`（可 serve）。Gateway 端用 `nest_service("/api", pm_router)`
//!   挂载（axum 0.8 `nest`/`merge` 都要求内外 state 类型一致，PM 的 `()`
//!   与 Gateway 的 `AppState` 不同，故只能用 `nest_service` 接受任意 `Service`）。
//!
//! **为什么不用 type alias**：handlers 内部大量使用多行链式调用
//! `state\n  .store\n  .method()`，type alias 后 `state: Arc<TreePmStore>` 没有
//! `.store` 字段，所有多行链式调用要重构为 `state.method()`。保留 struct 让
//! handlers 维持现状，**零 handler 改动**。

pub mod attachments;
pub mod icon;
pub mod projects;
pub mod routes;
pub mod search;
pub mod tasks;

use std::sync::Arc;

use crate::config::PmConfig;
use crate::mcp::AgentDirectory;
use crate::store::tree::TreePmStore;

/// API 层共享状态（axum `State<ApiState>`）。
///
/// 由 [`crate::api::routes::pm_router`] 在构造时用 `.with_state(state)` 注入到 router。
#[derive(Clone)]
pub struct ApiState {
    pub store: Arc<TreePmStore>,
    pub config: PmConfig,
    /// Agent 目录（设计 §9.1）：`POST /projects/:pid/members` 添加成员时按
    /// instance_id 校验 Agent 存在。宽松实现（[`NoopAgentDirectory`]）跳过校验。
    pub agent_dir: Arc<dyn AgentDirectory>,
    /// Design 27 Phase 2: Gateway base URL + internal-service token for
    /// project group chat provisioning. `None` (no `--gateway-url`) skips
    /// it — same degradation rule as the agent directory.
    pub gateway: Option<(String, Option<String>)>,
}
