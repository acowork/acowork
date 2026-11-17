//! acowork-pm - Project & Task management service.
//!
//! 提供项目/任务/附件的 CRUD、依赖图、父子树、看板视图等能力，对外暴露：
//!
//! - **REST API**（axum）—— Gateway 反向代理到本服务（公开路径 `/api/pm/projects/*`、`/api/pm/tasks/*`、`/api/pm/attachments/*`，内部路径不带 `/api` 前缀）
//! - **MCP tools**（HTTP Server）—— Agent 通过 MCP 协议调用 `pm_*` 工具
//!
//! ## 存储模型：SQLite 权威（ADR-117）
//!
//! `pm.sqlite` 是唯一提交点：实体以原始 JSON 存在 `props`，同时投影
//! `parent_id` / `depth` / `status` / `task_dir` 等列供查询。附件与项目图标
//! 二进制仍在文件系统，按 `assets/projects/{pid}/` 分组。
//!
//! 空数据目录首次启动会直接建立空的 v1 库；不再读取任何 JSON 树，
//! 也没有第二提交点、灰度配置或运行时后端切换。
//!
//! ## 跨服务集成
//!
//! | 调用方 | 协议 | 入口 |
//! |--------|------|------|
//! | Gateway HTTP API | REST over axum（反代） | `acowork-gateway::http::pm_proxy` |
//! | Agent (本地/远程) | MCP HTTP | `acowork-pm::mcp::tools` |
//! | Desktop UI | REST + multipart upload | 同 Gateway 路径 |
//!
//! ADR-064：PM 作为**独立进程**运行（`acowork-pm` 二进制），由 Gateway
//! supervisor 管理生命周期，Gateway 反向代理 `/api/pm/*` → `127.0.0.1:{pm_port}/*`。
//!
//! ## 设计引用
//!
//! - 服务端设计：[`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md)
//! - Desktop UX：[`docs/design/zh/22-pm-desktop-ui.md`](../../docs/design/zh/22-pm-desktop-ui.md)
//! - 开发计划：[`docs/plan/zh/pm-dev-plan.md`](../../docs/plan/zh/pm-dev-plan.md)
//! - ADR：[`docs/adr/zh/ADR-061-pm-storage-tree.md`](../../docs/adr/zh/ADR-061-pm-storage-tree.md)

pub mod api;
pub mod config;
pub mod dispatch;
pub mod error;
pub mod gateway_client;
pub mod health;
pub mod mcp;
pub mod server;
pub mod store;
pub mod types;
pub mod ui;

// ────────────────────────────────────────────────────────────────────────────
// Re-exports: core types (crate-level public API)
// ────────────────────────────────────────────────────────────────────────────

// Config
pub use config::PmConfig;

// Errors
pub use error::{PmError, Result};

// Domain types
pub use types::{
    AttachmentId, AttachmentKind, AttachmentMeta, DependencyKind, MemberKind, Project, ProjectId,
    ProjectStatus, ReviewStatus, Task, TaskId, TaskStatus, TaskType,
};

// Storage
pub use store::contract::{PmMaintenance, PmStore};
pub use store::sqlite::{SqlitePmStore, schema as pm_sqlite_schema};

// API
pub use api::routes::pm_router;

// Service handle (ADR-061 / ADR-064): constructed by the standalone
// `acowork-pm` process entry (`main.rs`) and used to serve the full
// router (REST + MCP + `/health`). The PM service stays a stateless
// library crate; the Gateway no longer compiles it (ADR-064).
pub use server::PmService;

// MCP
pub use mcp::agent_dir::HttpAgentDirectory;
pub use mcp::manifest::PM_TOOL_MANIFEST;
pub use mcp::mcp_router;
pub use mcp::{AgentDirectory, McpState, NoopAgentDirectory};
