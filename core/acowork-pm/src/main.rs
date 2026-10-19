//! acowork-pm — 独立 PM 服务进程（ADR-064）。
//!
//! 由 Gateway supervisor 管理生命周期（spawn / monitor / restart），
//! 独立监听端口，serve 全量路由（REST + MCP + `/health`）。
//!
//! 数据目录独立解析为 `$HOME/.acowork/acowork-pm/`（与 `acowork-gateway/`、
//! `acowork-node/` 平级），不再嵌套在 Gateway 数据目录下。
//!
//! 对外契约不变：Desktop 走 `{gw}/api/pm/*`、远程 Agent 走
//! `http://{advertise_host}:{gw_http_port}/api/pm/mcp`——两端均经 Gateway
//! 反向代理，无感知。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use acowork_pm::{HttpAgentDirectory, PmConfig, PmService};

/// CLI 参数。
#[derive(Parser)]
#[command(
    name = "acowork-pm",
    about = "ACowork Project & Task management service (standalone process, ADR-064)"
)]
struct Cli {
    /// HTTP 监听地址
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// HTTP 监听端口（默认 18082；冲突自动递增，最多 +20）
    #[arg(long)]
    port: Option<u16>,

    /// 数据目录覆盖（默认 `$HOME/.acowork/acowork-pm`）
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// 把实际绑定的端口写入此文件（供 Gateway supervisor 读取）
    #[arg(long)]
    port_file: Option<PathBuf>,

    /// Gateway 健康 URL（ADR-018）：Gateway 失联超时后自退出，避免孤儿进程
    #[arg(long)]
    gateway_health_url: Option<String>,

    /// Gateway 健康探测间隔（毫秒，默认 10000 = 10s）
    #[arg(long, default_value = "10000")]
    gateway_health_interval_ms: u64,

    /// Gateway 失联自退出超时（毫秒，默认 300000 = 5min）
    #[arg(long, default_value = "300000")]
    gateway_health_timeout_ms: u64,

    /// Gateway HTTP base URL（ADR-064 Phase 3）：AgentDirectory 查询
    /// `GET /api/agents` 校验 assignee 存在性。缺省时用宽松目录（不校验）。
    #[arg(long)]
    gateway_url: Option<String>,

    /// Agent 目录周期刷新间隔（秒，默认 60）。仅 `--gateway-url` 提供时生效。
    #[arg(long, default_value = "60")]
    agent_sync_interval_secs: u64,

    /// 日志级别
    #[arg(long, default_value = "info")]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // 子进程日志（Gateway 把 stderr 重定向到 pm.log）
    acowork_core::logging::init_subprocess_logging(&cli.log_level);
    acowork_core::logging::install_panic_hook();

    let mut config = PmConfig::default();
    if let Some(dir) = cli.data_dir {
        config.data_dir = dir;
    }
    if let Some(port) = cli.port {
        config.port = port;
    }
    if let Err(e) = config.validate() {
        tracing::error!(error = %e, "Invalid PM config");
        std::process::exit(1);
    }

    if !config.enabled {
        tracing::warn!("PM disabled via config (enabled=false) — exiting");
        std::process::exit(0);
    }

    // ADR-018: Gateway 健康看门狗（Gateway 崩溃后 PM 自退出）
    if let Some(ref url) = cli.gateway_health_url {
        spawn_gateway_health_watchdog(
            url.clone(),
            Duration::from_millis(cli.gateway_health_interval_ms),
            Duration::from_millis(cli.gateway_health_timeout_ms),
        );
        tracing::info!(
            url = %url,
            interval_ms = cli.gateway_health_interval_ms,
            timeout_ms = cli.gateway_health_timeout_ms,
            "Gateway health watchdog started (ADR-018)"
        );
    }

    // ADR-064 Phase 3: AgentDirectory HTTP 化。
    //
    // `--gateway-url` 提供时注入 `HttpAgentDirectory`（查询 Gateway `/api/agents`
    // 校验 assignee 存在性，启动拉全量 + 周期刷新 + 即时兜底）；缺省时用
    // 宽松目录（`NoopAgentDirectory`，不校验——测试/独立调试场景）。
    let gateway_info: Option<(String, Option<String>)> = cli.gateway_url.clone().map(|u| {
        (
            u,
            std::env::var("ACOWORK_PM_GATEWAY_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
        )
    });
    let agent_dir: Arc<dyn acowork_pm::AgentDirectory> = if let Some((gw_url, auth_token)) =
        gateway_info.clone()
    {
        // Gateway auth 开启时经 env 传入 Bearer token（避免 CLI 参数暴露在进程列表）。
        let dir = Arc::new(HttpAgentDirectory::new(
            gw_url,
            auth_token,
            Duration::from_secs(cli.agent_sync_interval_secs),
        ));
        dir.start();
        tracing::info!(
            sync_interval_secs = cli.agent_sync_interval_secs,
            "Agent directory (HTTP) started — assignee existence validated against Gateway"
        );
        dir
    } else {
        tracing::warn!("No --gateway-url — using permissive AgentDirectory (assignee not validated)");
        Arc::new(acowork_pm::NoopAgentDirectory)
    };

    let service = match PmService::with_gateway(config.clone(), agent_dir, gateway_info).await {
        Ok(svc) => Arc::new(svc),
        Err(e) => {
            tracing::error!(error = %e, "Failed to initialize PM store");
            std::process::exit(1);
        }
    };

    let bind = format!("{}:{}", cli.host, config.port);
    let bind_addr: std::net::SocketAddr = match bind.parse() {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(bind = %bind, error = %e, "Invalid bind address");
            std::process::exit(1);
        }
    };

    let addr = match service.clone().serve(bind_addr).await {
        Ok(a) => a,
        Err(e) => {
            tracing::error!(error = %e, "Failed to start PM server");
            std::process::exit(1);
        }
    };
    tracing::info!(
        addr = %addr,
        data_dir = %config.data_dir.display(),
        "PM service listening (ADR-064 standalone)"
    );

    // Design 27 Phase 2 follow-up: old projects (created before per-project
    // group chats existed) get their group chat backfilled, idempotently.
    service.spawn_group_chat_backfill();

    // 把实际绑定端口写入 port_file（Gateway supervisor 读取）
    if let Some(path) = &cli.port_file {
        if let Err(e) = std::fs::write(path, addr.port().to_string()) {
            tracing::error!(path = %path.display(), error = %e, "Failed to write port file");
            std::process::exit(1);
        }
        tracing::info!(path = %path.display(), port = addr.port(), "Port file written");
    }

    // 优雅停机：等待信号 → flush store
    let shutdown = acowork_core::shutdown::Shutdown::new();
    acowork_core::shutdown::install_signal_handlers(shutdown.clone());
    while !shutdown.is_shutting_down() {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tracing::info!("Graceful shutdown initiated");
    if let Err(e) = service.shutdown().await {
        tracing::error!(error = %e, "PM shutdown error");
    }
    tracing::info!("PM service shut down");
}

/// ADR-018: 周期探测 Gateway `/health`，失联超时后自退出。
///
/// 与 LSP relay 的 `spawn_gateway_health_watchdog` 同模式：Gateway 崩溃
/// （panic / SIGKILL）时无法清理子进程，子进程必须自行检测父进程死亡并退出。
fn spawn_gateway_health_watchdog(health_url: String, interval: Duration, timeout: Duration) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build HTTP client for Gateway health probe");

        let mut last_success = std::time::Instant::now();
        loop {
            tokio::time::sleep(interval).await;

            let healthy = match client.get(&health_url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };

            if healthy {
                last_success = std::time::Instant::now();
            } else if last_success.elapsed() >= timeout {
                tracing::error!(
                    elapsed_secs = last_success.elapsed().as_secs(),
                    timeout_secs = timeout.as_secs(),
                    "Gateway unreachable — self-exiting (ADR-018)"
                );
                std::process::exit(0);
            }
        }
    });
}
