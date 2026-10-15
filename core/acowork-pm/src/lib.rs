//! acowork-pm - Project & Task management service.
//!
//! 提供项目/任务/附件的 CRUD、依赖图、父子树、看板视图等能力，对外暴露：
//!
//! - **REST API**（axum）—— Gateway 反向代理到本服务（公开路径 `/api/pm/projects/*`、`/api/pm/tasks/*`、`/api/pm/attachments/*`，内部路径不带 `/api` 前缀）
//! - **MCP tools**（HTTP Server）—— Agent 通过 MCP 协议调用 `pm_*` 工具
//!
//! ## 存储模型：目录树 + 物理嵌套即权威
//!
//! 一个项目 = 一棵完整目录树：
//!
//! ```text
//! {data}/acowork-pm/projects/{pid}/
//! ├── project.json
//! └── tasks/
//!     ├── {root_task_id}/
//!     │   ├── task.json
//!     │   ├── attachments/{att_id}/{original.{ext},thumb.jpg}
//!     │   └── children/{child_task_id}/...
//!     └── {another_root_task_id}/
//! ```
//!
//! **核心不变量**：
//!
//! - 任务恒为目录（不用文件形态），避免二态分支
//! - 子任务强制放在父任务的 `children/` 子目录下
//! - `children/` 按需创建（首个子任务时 `mkdir`）
//! - 删除任务 = `rm -rf` 目录树，原子级清理子树 + 附件
//! - Reparent = `mv` 目录，**0 文件写**
//! - **`task.json` 不存 `parent_id` / `subtask_ids` / `subtask_count`** —— 父子关系完全靠物理位置表达
//!
//! 物理结构损坏可由 `walkdir` 重建索引，**无修复逻辑**（因为没有冗余字段可漂移）。
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
pub mod error;
pub mod group_chat;
pub mod health;
pub mod mcp;
pub mod server;
pub mod store;
pub mod types;

// ───────────────────────────────────��────────────────────────────────────────
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
pub use store::index::TaskIndex;
pub use store::tree::{PmStore, TreePmStore};

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