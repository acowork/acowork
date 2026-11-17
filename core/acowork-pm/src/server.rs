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
use crate::error::{PmError, Result};
use crate::mcp::{AgentDirectory, NoopAgentDirectory};
use crate::store::sqlite::{SqlitePmStore, import, schema};
use crate::store::tree::PmStore;

/// PM 服务运行实例。
///
/// 独立进程入口（`main.rs`）持有此句柄，用于：
/// - 启动时调用 [`PmService::serve`] 拉起 HTTP server（全量路由）
/// - 关闭时调用 [`PmService::shutdown`] 优雅停机（flush 写、关闭 store）
pub struct PmService {
    pub config: PmConfig,
    /// ADR-117 D6: concrete backend is chosen once at startup by [`build_store`].
    pub store: Arc<dyn PmStore>,
    /// Agent 目录契约（设计 §9.1）：`pm_create_task` 校验 assignee 存在。
    /// 默认 `Noop`（不校验）；Phase 3（ADR-064）将注入 HTTP 实现（查 Gateway
    /// `/api/agents`）。
    pub agent_dir: Arc<dyn AgentDirectory>,
    /// Design 27 Phase 2: Gateway base URL + internal-service token for
    /// project group chat provisioning (`None` = standalone/test mode,
    /// provisioning skipped).
    pub gateway: Option<(String, Option<String>)>,
}

/// ADR-117 D6 (single-backend cutover): SQLite is the only commit point.
///
/// On the first start the JSON tree is imported into `pm.sqlite` in one
/// transaction, its binaries are relocated into `assets/`, and the tree is
/// renamed to `projects.retired`. From then on `pm.sqlite` + `assets/` are the
/// only data source. A blocking scan finding aborts startup instead of
/// creating a half-empty authority.
async fn build_store(config: &PmConfig) -> Result<Arc<dyn PmStore>> {
    ensure_sqlite_authority(config)?;
    Ok(Arc::new(SqlitePmStore::open(config.clone())?) as Arc<dyn PmStore>)
}

/// One-shot startup import + archive retirement. Idempotent: a non-empty v1
/// database is left alone, while retirement re-runs until the tree has really
/// been renamed (a crash between the two steps is recoverable).
///
/// The probe uses a read-only connection so deciding "do we need to import?"
/// cannot create WAL sidecars, DDL or a `user_version` stamp on a database we
/// might refuse. `import()` runs its own blocking scan and a single transaction.
fn ensure_sqlite_authority(config: &PmConfig) -> Result<()> {
    let path = config.sqlite_path();
    let needs_import = if !path.is_file() {
        true
    } else {
        let conn = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| PmError::Internal(format!("sqlite: {e}")))?;
        let version = schema::user_version(&conn)?;
        if version == 0 {
            // A zero-byte / unversioned file. `import()` calls `schema::open()`,
            // which refuses to adopt a DB that already has PM tables.
            true
        } else if version != schema::SCHEMA_VERSION {
            return Err(PmError::Internal(format!(
                "pm.sqlite schema v{version} is not understood by this binary (v{}); \
                 refusing to start over an unmigrated database",
                schema::SCHEMA_VERSION
            )));
        } else {
            schema::is_empty(&conn)?
        }
    };
    if needs_import {
        let report = import::dry_run(config)?;
        if report.has_blocking_issues() {
            return Err(PmError::Internal(format!(
                "startup import blocked ({} finding(s)): {}",
                report.blocking.len(),
                report.blocking.join(" | ")
            )));
        }
        if report.projects > 0 {
            let report = import::import(config)?;
            tracing::info!(
                projects = report.projects,
                tasks = report.tasks,
                activity_rows = report.activity_rows,
                "one-shot JSON -> SQLite import complete"
            );
        }
    }
    // Always run, even when the rows were imported by an earlier start: a
    // crash between the import and this step must not leave two data sources.
    import::retire_tree_archive(config)?;
    Ok(())
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

        let store = build_store(&config).await?;

        // 启动时重建索引（如果配置开启）
        if config.index_rebuild_on_start {
            store.rebuild_index().await?;
        }

        // 一次性回填 `start_at`：改动前建的任务该字段全是 null，而新规则
        // 「没有开始时间 = 不派发」会把它们整体冻住。回填成 `now`（旧缺省值
        // 的同一个含义），存量待办照常流动。幂等，第二次跑无事可做。
        match store.migrate_null_start_at().await {
            Ok(n) if n > 0 => {
                tracing::info!(count = n, "PM backfilled start_at on legacy tasks")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "PM start_at backfill failed"),
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
        .merge(crate::mcp::mcp_router(
            self.store.clone(),
            self.agent_dir.clone(),
            self.gateway.clone(),
        ))
        // ADR-116: the app's own front end, reached through
        // `/api/apps/pm/ui/*`. PM owns this bundle, not the Desktop.
        .merge(crate::ui::ui_router())
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
                    let router = self.router().merge(crate::health::health_route(
                        self.config.data_dir.clone(),
                        self.store.corrupt_registry(),
                    ));
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
    /// on every start. Retries with backoff because at PM start the
    /// Gateway/user service may still be warming up (the endpoint answers 503
    /// and `ensure_group` logs + returns `None`) — on a cold start the user
    /// service can take well over 30s to become ready, so the window is
    /// generous. Best-effort like the create-time provisioning: a project
    /// without its chat still works.
    pub fn spawn_group_chat_backfill(self: &Arc<Self>) {
        let Some((gw_url, token)) = self.gateway.clone() else {
            return; // standalone/test mode — no Gateway to provision through
        };
        let store = self.store.clone();
        tokio::spawn(async move {
            let client = crate::gateway_client::GatewayClient::new(gw_url, token);
            // Retries at ~0/10/20/40/70/110s — cumulative ~2min warm-up window.
            let backoffs = [0u64, 10, 20, 30, 40];
            for (attempt, &delay) in backoffs.iter().enumerate() {
                if delay > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
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
                    // The owner travels too: `ensure_group` converges it onto
                    // an existing row, so a project group created before the
                    // chat domain carried an owner (or one whose ownership
                    // moved while the Gateway was down) heals here — that id
                    // is what governs the group's `chat.md` (ADR-098).
                    let owner = p.owner().to_string();
                    if client
                        .ensure_group(p.id.as_str(), &p.title, &participants, &owner)
                        .await
                        .is_none()
                    {
                        failed += 1;
                    }
                }
                if failed == 0 {
                    tracing::info!(
                        "group chat backfill complete (all projects have their group chat)"
                    );
                    return;
                }
                tracing::warn!(
                    failed,
                    attempt,
                    "group chat backfill: some projects could not be provisioned"
                );
            }
        });
    }

    /// Start-time-driven task dispatch scanner (see [`crate::dispatch`]).
    /// No-op without a Gateway (standalone/test mode), same degradation
    /// rule as provisioning. The scanner stops between ticks once
    /// `shutdown` is requested.
    pub fn spawn_dispatch_scanner(
        self: &Arc<Self>,
        interval: std::time::Duration,
        per_tick: usize,
        shutdown: Arc<acowork_core::shutdown::Shutdown>,
    ) {
        let Some(gateway) = self.gateway.clone() else {
            return;
        };
        crate::dispatch::spawn_scanner(self.store.clone(), gateway, interval, per_tick, shutdown);
    }

    /// 优雅停机（flush 待写、关闭 store）。
    pub async fn shutdown(&self) -> Result<()> {
        self.store.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tree::TreePmStore;
    use crate::types::TaskId;

    fn test_config(data_dir: &std::path::Path) -> PmConfig {
        PmConfig {
            data_dir: data_dir.to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn db_backend_is_object_safe_and_serves_an_empty_fresh_install() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = test_config(tmp.path());
        let store = build_store(&cfg).await.unwrap();
        assert!(store.list_projects().await.unwrap().is_empty());
        assert_eq!(store.task_depth(&TaskId("t-nonexistent".into())), 0);
        assert!(store.corrupt_registry().read().is_empty());
    }

    /// The whole point of the single-backend cutover: an existing JSON tree is
    /// imported on the first start with no operator command, and a second
    /// start is a no-op. The tree is renamed (never deleted) so the snapshot
    /// stays recoverable.
    #[tokio::test]
    async fn startup_imports_the_existing_tree_automatically() {
        use crate::types::CreateProject;

        let tmp = tempfile::tempdir().unwrap();
        let tree_cfg = test_config(tmp.path());
        let tree = TreePmStore::new(tree_cfg).await.unwrap();
        tree.create_project(
            CreateProject {
                title: "migrated from the tree".into(),
                description: String::new(),
                metadata: Default::default(),
            },
            "human",
        )
        .await
        .unwrap();
        let tree_before = count_tree_files(&tmp.path().join("projects"));

        let cfg = test_config(tmp.path());
        let store = build_store(&cfg)
            .await
            .expect("first start must import the tree automatically");
        let projects = store.list_projects().await.unwrap();
        assert_eq!(projects.len(), 1, "imported project must be visible");
        assert_eq!(projects[0].title, "migrated from the tree");
        assert!(cfg.sqlite_path().exists(), "import must create pm.sqlite");
        assert_eq!(
            count_tree_files(&tmp.path().join("projects.retired")),
            tree_before,
            "retirement must preserve every tree file"
        );
        assert!(
            !tmp.path().join("projects").exists(),
            "projects/ must no longer be a live data source"
        );

        let again = build_store(&cfg)
            .await
            .expect("second start must be idempotent");
        assert_eq!(again.list_projects().await.unwrap().len(), 1);
    }

    fn count_tree_files(root: &std::path::Path) -> usize {
        fn walk(path: &std::path::Path, n: &mut usize) {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        walk(&p, n);
                    } else {
                        *n += 1;
                    }
                }
            }
        }
        let mut n = 0;
        walk(root, &mut n);
        n
    }

    /// Zero-config start: `PmConfig::default()` (with only data_dir pointed at
    /// a temp dir) must build the SQLite authority without any operator step.
    #[tokio::test]
    async fn default_config_builds_the_sqlite_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = PmConfig {
            data_dir: tmp.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        };
        let store = build_store(&cfg).await.unwrap();
        assert!(store.list_projects().await.unwrap().is_empty());
        assert_eq!(store.task_depth(&TaskId("t-nonexistent".into())), 0);
        assert!(store.corrupt_registry().read().is_empty());
        assert!(cfg.sqlite_path().exists());
    }
}
