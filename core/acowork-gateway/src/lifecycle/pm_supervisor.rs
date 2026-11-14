//! PM process supervisor (ADR-064).
//!
//! Spawns the standalone `acowork-pm` binary, waits for `/health` ready,
//! monitors liveness, and restarts with exponential backoff on crash.
//!
//! ## Why `/health` polling instead of SSE
//!
//! The embed supervisor uses SSE `/events` because embed pushes state
//! transitions (model loaded, dimension). PM has no such state events —
//! it is a stateless REST/MCP service — so a `/health` poll is the
//! simplest adequate liveness signal (KISS). The reusable building blocks
//! (`RestartHistory`, `backoff_with_jitter`) come from
//! `acowork-core::supervisor` (ADR-019 extraction).
//!
//! ## Failure detection
//!
//! Two layers:
//!   1. `child.wait()` returning — process crashed or was killed.
//!   2. `/health` failing for > `HEARTBEAT_TIMEOUT` (10s) — process alive
//!      but stuck (deadlock / hang).
//!
//! Both trigger the same restart path with exponential backoff
//! (1s → 60s cap) and a 5-attempts/5-min cap. On reaching the cap the
//! supervisor gives up and clears `pm_process` — the Gateway keeps
//! running and `/api/pm/*` returns 503.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{RestartHistory, backoff_with_jitter};

use crate::gateway::state::GatewayState;

/// Shared gateway state handle (same as the embed supervisor).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// PM 进程运行时状态（写入 `GatewayState.pm_process`，供反代层读取端口）。
#[derive(Debug, Clone)]
pub struct PmProcessState {
    pub pid: u32,
    pub port: u16,
    pub ready: bool,
}

/// Supervisor 配置（spawn PM 所需参数）。
#[derive(Clone)]
pub struct PmSupervisorConfig {
    /// `acowork-pm` 二进制路径（`current_exe().parent()` 同级）。
    pub pm_bin: PathBuf,
    /// 期望端口（默认 18082；PM 冲突自动递增，实际端口经 port_file 上报）。
    pub port: u16,
    /// PM 写入实际绑定端口的文件路径（Gateway 数据目录下）。
    pub port_file: PathBuf,
    /// PM 日志目录（`{gateway.data_dir}/logs`）。
    pub log_dir: PathBuf,
    /// Gateway `/health` URL（ADR-018：PM 失联自退出，避免孤儿进程）。
    pub gateway_health_url: String,
    /// Gateway HTTP base URL（ADR-064 Phase 3）：PM 经 `--gateway-url` 查询
    /// `GET /api/agents` 校验 assignee 存在性。`None` 时 PM 用宽松目录。
    pub gateway_url: Option<String>,
    /// Internal-service HTTP credential，spawn 时经 `ACOWORK_PM_GATEWAY_TOKEN`
    /// env 注入（非 argv —— 进程列表可见）。PM 用它作为 Bearer token 调 Gateway
    /// HTTP；`None` 时 PM 不带 token，multi_user 下会被 401 拒掉。
    pub gateway_token: Option<String>,
}

/// Spawn the PM supervisor task. Non-fatal: if PM cannot start, the
/// Gateway keeps running and `/api/pm/*` returns 503.
pub fn start_pm_supervisor(cfg: PmSupervisorConfig, state: SharedState) {
    tokio::spawn(async move {
        run_supervisor(cfg, state).await;
    });
}

async fn run_supervisor(cfg: PmSupervisorConfig, state: SharedState) {
    let mut history = RestartHistory::new();

    loop {
        spawn_and_monitor(&cfg, &state).await;

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(
                attempts,
                "PM restart limit exceeded; giving up (Gateway keeps running, /api/pm/* returns 503)"
            );
            clear_state(&state).await;
            return;
        }
        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting PM process");
        sleep(backoff).await;
    }
}

/// Spawn PM, wait for ready, then monitor until it dies or gets stuck.
async fn spawn_and_monitor(cfg: &PmSupervisorConfig, state: &SharedState) {
    // 删除陈旧 port file，避免上一次运行的端口被误判为本次 ready。
    let _ = std::fs::remove_file(&cfg.port_file);

    let (child, pid) = match spawn_pm(cfg).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to spawn PM process");
            return;
        }
    };

    // Startup grace: wait for port file + /health ready.
    let port = match wait_for_ready(cfg, pid).await {
        Some(p) => p,
        None => {
            tracing::warn!("PM did not become ready within startup grace");
            let _ = crate::lifecycle::process::kill_agent_process(pid).await;
            return;
        }
    };

    // Update shared state so the reverse proxy can route to PM.
    {
        let mut gw = state.write().await;
        gw.pm_process = Some(PmProcessState {
            pid,
            port,
            ready: true,
        });
    }
    tracing::info!(pid, port, "PM process ready");

    // Monitor loop: `child.wait()` for exit, `/health` poll for stuck.
    let mut child = child;
    let mut last_healthy = Instant::now();
    loop {
        tokio::select! {
            _ = sleep(Duration::from_secs(2)) => {
                if check_pm_health(port).await {
                    last_healthy = Instant::now();
                } else if last_healthy.elapsed() > supervisor_defaults::HEARTBEAT_TIMEOUT {
                    tracing::warn!(
                        elapsed_secs = last_healthy.elapsed().as_secs(),
                        pid,
                        port,
                        "PM /health failing for too long — killing and restarting"
                    );
                    let _ = crate::lifecycle::process::kill_agent_process(pid).await;
                    clear_state(state).await;
                    return;
                }
            }
            _ = child.wait() => {
                tracing::warn!(pid, "PM process exited");
                clear_state(state).await;
                return;
            }
        }
    }
}

/// Wait up to `STARTUP_GRACE` for PM to write its port file and answer
/// `/health`. Returns the actual bound port, or `None` on timeout/death.
async fn wait_for_ready(cfg: &PmSupervisorConfig, pid: u32) -> Option<u16> {
    let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
    loop {
        if let Ok(port_str) = tokio::fs::read_to_string(&cfg.port_file).await
            && let Ok(port) = port_str.trim().parse::<u16>()
            && check_pm_health(port).await
        {
            return Some(port);
        }
        // Process died during boot?
        if !crate::lifecycle::process::check_health(pid).await {
            tracing::warn!(pid, "PM died during startup grace");
            return None;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "PM did not become ready within {:?}",
                supervisor_defaults::STARTUP_GRACE
            );
            return None;
        }
        sleep(supervisor_defaults::STARTUP_POLL).await;
    }
}

/// Spawn the `acowork-pm` process with supervisor-managed args.
async fn spawn_pm(cfg: &PmSupervisorConfig) -> Result<(tokio::process::Child, u32), String> {
    if !cfg.pm_bin.exists() {
        return Err(format!("acowork-pm binary not found at {:?}", cfg.pm_bin));
    }

    // Create log dir + open log file (truncate on each start).
    std::fs::create_dir_all(&cfg.log_dir)
        .map_err(|e| format!("Failed to create log dir {:?}: {}", cfg.log_dir, e))?;
    let log_path = cfg.log_dir.join("pm.log");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("Failed to create pm log file {:?}: {}", log_path, e))?;
    tracing::info!(path = %log_path.display(), "PM process logging to file");

    let mut cmd = tokio::process::Command::new(&cfg.pm_bin);
    cmd.arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--port-file")
        .arg(&cfg.port_file)
        .arg("--gateway-health-url")
        .arg(&cfg.gateway_health_url)
        .arg("--log-level")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));

    // ADR-064 Phase 3: 传 Gateway base URL 供 PM 查询 `/api/agents`（AgentDirectory）。
    if let Some(gw_url) = &cfg.gateway_url {
        cmd.arg("--gateway-url").arg(gw_url);
    }

    // 内部服务调用 Gateway HTTP 的凭证。走 env 而非 argv（`main.rs` 读
    // `ACOWORK_PM_GATEWAY_TOKEN` 早已预留好这条通道，之前一直没人写入，
    // 于是 multi_user 下 PM 的目录查询恒 401）。失败非致命：PM 会退化成
    // 无 token 刷新，Gateway 侧照常服务。
    if let Some(token) = &cfg.gateway_token {
        cmd.env("ACOWORK_PM_GATEWAY_TOKEN", token);
    }

    // On Unix, create a new process group so a Gateway shutdown does not
    // cascade a SIGHUP to PM (PM self-exits via the ADR-018 watchdog).
    // `tokio::process::Command` exposes `process_group` directly, so the
    // `std::os::unix::process::CommandExt` trait import is unnecessary here.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn acowork-pm (binary: {:?}): {}", cfg.pm_bin, e))?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(pid, port = cfg.port, "PM process spawned");
    Ok((child, pid))
}

/// Probe `GET /health` on the PM port.
async fn check_pm_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);
    let client = http_client();
    match client
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Shared HTTP client for supervisor probes.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            // Loopback hop to a supervised sidecar. An ambient HTTP_PROXY
            // must not answer for it: a proxy's reply would make a dead
            // service look healthy (or a healthy one dead).
            .no_proxy()
            .build()
            .expect("Failed to build PM supervisor HTTP client")
    })
}

/// Clear `pm_process` from shared state (PM gone / gave up).
async fn clear_state(state: &SharedState) {
    let mut gw = state.write().await;
    gw.pm_process = None;
}
