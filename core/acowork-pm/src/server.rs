//! PM 服务 HTTP server 启动入口。
//!
//! ADR-064：PM 作为**独立进程**运行，由 Gateway supervisor 管理生命周期
//! （spawn / monitor / restart）。本模块提供 [`PmService::serve`] 在独立端口
//! serve 全量路由（REST + MCP + `/health`）。
//!
//! 公开路径约定：PM router 内部路径**不带** `/api` 前缀（`/projects`、
//! `/tasks/...`、`/mcp`）；Gateway 反向代理 `/api/pm/*` → `127.0.0.1:{pm_port}/*`
//! 时自动剥离 `/api/pm` 前缀，公开路径统一为 `/api/pm/*`（设计文档 §21）。
//!
//! 设计参考：[`docs/design/zh/21-pm-project-management.md`](../../docs/design/zh/21-pm-project-management.md) §7

use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::PmConfig;
use crate::error::Result;
use crate::mcp::{AgentDirectory, NoopAgentDirectory};
use crate::store::tree::{PmStore, TreePmStore};

/// PM 服务运行实例。
///
/// 独立进程入口（`main.rs`）持有此句柄，用于：
/// - 启动时调用 [`PmService::serve`] 拉起 HTTP server（全量路由）
/// - 关闭时调用 [`PmService::shutdown`] 优雅停机（flush 写、关闭 store）
pub struct PmService {
    pub config: PmConfig,
    pub store: Arc<TreePmStore>,
    /// Agent 目录契约（设计 §9.1）：`pm_create_task` 校验 assignee 存在。
    /// 默认 `Noop`（不校验）；Phase 3（ADR-064）将注入 HTTP 实现（查 Gateway
    /// `/api/agents`）。
    pub agent_dir: Arc<dyn AgentDirectory>,
    /// Design 27 Phase 2: Gateway base URL + internal-service token for
    /// project group chat provisioning (`None` = standalone/test mode,
    /// provisioning skipped).
    pub gateway: Option<(String, Option<String>)>,
}

impl PmService {
    /// 构造服务实例（**不**启动 server，仅初始化 store + 重建索引）。
    ///
    /// 使用宽松 Agent 目录（不做 assignee 存在性校验）。生产环境建议用
    /// [`PmService::with_agent_directory`] 注入真实目录。
    pub async fn new(config: PmConfig) -> Result<Self> {
        Self::build(config, Arc::new(NoopAgentDirectory), None).await
    }

    /// 构造服务实例，并注入 Agent 目录（Gateway 基于其 `installed_agents` 提供）。
    pub async fn with_agent_directory(
        config: PmConfig,
        agent_dir: Arc<dyn AgentDirectory>,
    ) -> Result<Self> {
        Self::build(config, agent_dir, None).await
    }

    /// 构造服务实例，注入 Agent 目录 + Gateway 内部凭证（design 27 Phase 2:
    /// 项目群聊经 `POST /api/intents/project-group` 幂等创建）。
    pub async fn with_gateway(
        config: PmConfig,
        agent_dir: Arc<dyn AgentDirectory>,
        gateway: Option<(String, Option<String>)>,
    ) -> Result<Self> {
        Self::build(config, agent_dir, gateway).await
    }

    async fn build(
        config: PmConfig,
        agent_dir: Arc<dyn AgentDirectory>,
        gateway: Option<(String, Option<String>)>,
    ) -> Result<Self> {
        config.validate()?;

        let store = Arc::new(TreePmStore::new(config.clone()).await?);

        // 启动时重建索引（如果配置开启）
        if config.index_rebuild_on_start {
            store.rebuild_index().await?;
        }

        Ok(Self {
            config,
            store,
            agent_dir,
            gateway,
        })
    }

    /// 构建 axum router（REST + MCP 合并，不含 `/health`）。
    ///
    /// 返回的 router 已包含全部 PM API 路由，**不**启动 listen socket。
    ///
    /// REST `pm_router`（`/projects`、`/tasks/...`）与 MCP JSON-RPC `mcp_router`
    /// （`/mcp`）在此合并。两者均为 `Router<()>`（内部已 `.with_state(...)`
    /// 注入 state）。
    pub fn router(&self) -> axum::Router {
        crate::api::routes::pm_router(
            self.store.clone(),
            self.config.clone(),
            self.agent_dir.clone(),
            self.gateway.clone(),
        )
        .merge(crate::mcp::mcp_router(self.store.clone(), self.agent_dir.clone()))
    }

    /// 在给定地址上 serve 全量路由（REST + MCP + `/health`）。
    ///
    /// ADR-064：PM 独立进程入口。端口冲突时自动递增（默认 18082 起，最多
    /// +20），返回**实际**绑定的地址（供 `--port-file` 上报给 Gateway supervisor）。
    ///
    /// server 在后台 task 中运行；调用方负责在退出前调用 [`PmService::shutdown`]
    /// flush 待写。
    pub async fn serve(self: Arc<Self>, bind: SocketAddr) -> Result<SocketAddr> {
        let host = bind.ip();
        let mut port = bind.port();
        let max_port = port.saturating_add(20);

        loop {
            match tokio::net::TcpListener::bind(SocketAddr::new(host, port)).await {
                Ok(listener) => {
                    let addr = listener.local_addr()?;
                    let router = self
                        .router()
                        .merge(crate::health::health_route(self.config.data_dir.clone()));
                    tracing::info!(addr = %addr, "PM server listening (full router)");
                    tokio::spawn(async move {
                        if let Err(e) = axum::serve(listener, router).await {
                            tracing::error!(error = %e, "PM server exited with error");
                        }
                    });
                    return Ok(addr);
                }
                Err(_) if port < max_port => {
                    tracing::warn!(port, "PM port occupied — trying next");
                    port += 1;
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// @deprecated 使用 [`PmService::serve`] 替代（ADR-064 独立进程）。
    pub async fn start_dev(self: Arc<Self>, bind: SocketAddr) -> Result<SocketAddr> {
        self.serve(bind).await
    }

    /// Design 27 Phase 2 follow-up: backfill the unique group chat for
    /// projects created before per-project group chats existed.
    ///
    /// `ensure_group` is idempotent on `project_id`, so this is safe to run
    /// on every start. A few retries because at PM start the Gateway/user
    /// service may still be warming up (the endpoint answers 503 and
    /// `ensure_group` logs + returns `None`). Best-effort like the
    /// create-time provisioning: a project without its chat still works.
    pub fn spawn_group_chat_backfill(self: &Arc<Self>) {
        let Some((gw_url, token)) = self.gateway.clone() else {
            return; // standalone/test mode — no Gateway to provision through
        };
        let store = self.store.clone();
        tokio::spawn(async move {
            let client = crate::gateway_client::GatewayClient::new(gw_url, token);
            for attempt in 0..3u32 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                }
                let projects = match store.list_projects().await {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(error = %e, attempt, "group chat backfill: project list failed");
                        continue;
                    }
                };
                let mut failed = 0usize;
                for p in projects {
                    let participants = crate::api::projects::group_participants(&p);
                    if client
                        .ensure_group(p.id.as_str(), &p.title, &participants)
                        .await
                        .is_none()
                    {
                        failed += 1;
                    }
                }
                if failed == 0 {
                    tracing::info!("group chat backfill complete (all projects have their group chat)");
                    return;
                }
                tracing::warn!(failed, attempt, "group chat backfill: some projects could not be provisioned");
            }
        });
    }

    /// 优雅停机（flush 待写、关闭 store）。
    pub async fn shutdown(&self) -> Result<()> {
        self.store.shutdown().await
    }
}
