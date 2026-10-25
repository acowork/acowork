//! acowork-memory process supervisor (ADR-101, the ADR-064 pattern).
//!
//! Spawns the standalone `acowork-memory` binary, waits for `/health` ready,
//! monitors liveness and restarts with exponential backoff on crash — the same
//! shape as the doc / PM / user supervisors. The reusable building blocks
//! (`RestartHistory`, `backoff_with_jitter`, `supervisor_defaults`) come from
//! `acowork-core::supervisor` (ADR-019).
//!
//! Failure detection (two layers, same as doc):
//!   1. `child.wait()` returning — the process crashed or was killed.
//!   2. `/health` failing for > `HEARTBEAT_TIMEOUT` — alive but stuck.
//!
//! Both take the same restart path (1s → 60s backoff, 5 attempts per 5-minute
//! window). On reaching the cap the supervisor gives up and clears
//! `memory_process`: the Gateway keeps running, `/api/memory/*` returns 503 and
//! the `memory` MCP server drops out of newly built catalogs.
//!
//! ## What is *not* forwarded
//!
//! The doc supervisor passes `--mqtt-host/--mqtt-port/--mqtt-password` because
//! doc publishes tree-change events to the embedded broker. The memory process
//! speaks no MQTT (its Gateway credentials arrive by HTTP pull), so none of
//! those flags are passed — and specifically not the broker token, which would
//! otherwise sit in the process command line where any local user can read it.
//! The HTTP credential it does need travels as `ACOWORK_MEMORY_GATEWAY_TOKEN`
//! (env, not argv) for the same reason — see
//! [`crate::gateway::state::GatewayState::internal_service_token`].

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{RestartHistory, backoff_with_jitter};

use crate::gateway::state::GatewayState;

/// Shared gateway state handle (same as the doc / PM supervisors).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// memory process runtime state (written into `GatewayState.memory_process`,
/// read by the reverse proxy for the actual port).
#[derive(Debug, Clone)]
pub struct MemoryProcessState {
    pub pid: u32,
    pub port: u16,
    pub ready: bool,
}

/// Supervisor config (spawn parameters for acowork-memory).
#[derive(Clone)]
pub struct MemorySupervisorConfig {
    /// `acowork-memory` binary path (`current_exe().parent()` sibling).
    pub memory_bin: PathBuf,
    /// Desired port (default 18084; the service auto-increments on conflict,
    /// the actual port is reported via port_file).
    pub port: u16,
    /// Path where the service writes its actual bound port (under the Gateway
    /// data dir).
    pub port_file: PathBuf,
    /// Log directory (`{gateway.data_dir}/logs`); stderr → `memory.log`.
    pub log_dir: PathBuf,
    /// Gateway `/health` URL (ADR-018: the service self-exits when the
    /// Gateway dies).
    pub gateway_health_url: String,
    /// Gateway HTTP base URL for the global-resources pull (ADR-101 decision
    /// 7), forwarded verbatim as `--gateway-url`.
    pub gateway_url: String,
    /// [`crate::gateway::state::GatewayState::internal_service_token`],
    /// injected as the `ACOWORK_MEMORY_GATEWAY_TOKEN` env var so the service's
    /// pull is authenticated when the Gateway runs in `multi_user` mode.
    /// `None` → nothing injected (the pull then only works under `local`).
    pub gateway_token: Option<String>,
    /// Optional `[memory].data_dir` override forwarded via `--data-dir`.
    /// `None` → the service resolves its own default.
    pub data_dir: Option<PathBuf>,
    /// `[memory].config` — the service's own TOML (distiller, forgetting,
    /// embedding endpoint), forwarded verbatim as `--config`.
    pub config: Option<PathBuf>,
}

/// Spawn the memory supervisor task. Non-fatal: if the service cannot start,
/// the Gateway keeps running and `/api/memory/*` returns 503.
pub fn start_memory_supervisor(cfg: MemorySupervisorConfig, state: SharedState) {
    tokio::spawn(async move {
        run_supervisor(cfg, state).await;
    });
}

async fn run_supervisor(cfg: MemorySupervisorConfig, state: SharedState) {
    let mut history = RestartHistory::new();

    loop {
        spawn_and_monitor(&cfg, &state).await;

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(
                attempts,
                "memory restart limit exceeded; giving up (Gateway keeps running, /api/memory/* returns 503)"
            );
            clear_state(&state).await;
            return;
        }
        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting memory process");
        sleep(backoff).await;
    }
}

/// Spawn the service, wait for ready, then monitor until it dies or stalls.
async fn spawn_and_monitor(cfg: &MemorySupervisorConfig, state: &SharedState) {
    // Remove a stale port file so the previous run's port is not mistaken
    // for this run's readiness.
    let _ = std::fs::remove_file(&cfg.port_file);

    let (child, pid) = match spawn_memory(cfg).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to spawn memory process");
            return;
        }
    };

    // Startup grace: wait for the port file + /health ready.
    let port = match wait_for_ready(cfg, pid).await {
        Some(p) => p,
        None => {
            tracing::warn!("memory did not become ready within startup grace");
            let _ = crate::lifecycle::process::kill_agent_process(pid).await;
            return;
        }
    };

    // Update shared state so the reverse proxy can route to the service.
    {
        let mut gw = state.write().await;
        gw.memory_process = Some(MemoryProcessState {
            pid,
            port,
            ready: true,
        });
    }
    tracing::info!(pid, port, "memory process ready");

    // Monitor loop: `child.wait()` for exit, `/health` poll for stuck.
    let mut child = child;
    let mut last_healthy = Instant::now();
    loop {
        tokio::select! {
            _ = sleep(Duration::from_secs(2)) => {
                if check_memory_health(port).await {
                    last_healthy = Instant::now();
                } else if last_healthy.elapsed() > supervisor_defaults::HEARTBEAT_TIMEOUT {
                    tracing::warn!(
                        elapsed_secs = last_healthy.elapsed().as_secs(),
                        pid,
                        port,
                        "memory /health failing for too long — killing and restarting"
                    );
                    let _ = crate::lifecycle::process::kill_agent_process(pid).await;
                    clear_state(state).await;
                    return;
                }
            }
            _ = child.wait() => {
                tracing::warn!(pid, "memory process exited");
                clear_state(state).await;
                return;
            }
        }
    }
}

/// Wait up to `STARTUP_GRACE` for the service to write its port file and answer
/// `/health`. Returns the actual bound port, or `None` on timeout/death.
async fn wait_for_ready(cfg: &MemorySupervisorConfig, pid: u32) -> Option<u16> {
    let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
    loop {
        if let Ok(port_str) = tokio::fs::read_to_string(&cfg.port_file).await
            && let Ok(port) = port_str.trim().parse::<u16>()
            && check_memory_health(port).await
        {
            return Some(port);
        }
        // Process died during boot?
        if !crate::lifecycle::process::check_health(pid).await {
            tracing::warn!(pid, "memory died during startup grace");
            return None;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "memory did not become ready within {:?}",
                supervisor_defaults::STARTUP_GRACE
            );
            return None;
        }
        sleep(supervisor_defaults::STARTUP_POLL).await;
    }
}

/// Spawn the `acowork-memory` process with supervisor-managed args.
async fn spawn_memory(cfg: &MemorySupervisorConfig) -> Result<(tokio::process::Child, u32), String> {
    if !cfg.memory_bin.exists() {
        return Err(format!(
            "acowork-memory binary not found at {:?}",
            cfg.memory_bin
        ));
    }

    // Create log dir + open log file (truncate on each start).
    std::fs::create_dir_all(&cfg.log_dir)
        .map_err(|e| format!("Failed to create log dir {:?}: {}", cfg.log_dir, e))?;
    let log_path = cfg.log_dir.join("memory.log");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("Failed to create memory log file {:?}: {}", log_path, e))?;
    tracing::info!(path = %log_path.display(), "memory process logging to file");

    let mut cmd = tokio::process::Command::new(&cfg.memory_bin);
    cmd.arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(cfg.port.to_string())
        .arg("--port-file")
        .arg(&cfg.port_file)
        .arg("--gateway-health-url")
        .arg(&cfg.gateway_health_url)
        .arg("--gateway-url")
        .arg(&cfg.gateway_url)
        .arg("--log-level")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));

    // Optional overrides from the Gateway `[memory]` section.
    if let Some(dir) = &cfg.data_dir {
        cmd.arg("--data-dir").arg(dir);
    }
    if let Some(path) = &cfg.config {
        cmd.arg("--config").arg(path);
    }
    // The HTTP credential is env-only: a command-line argument is readable by
    // anyone on the machine (`Get-CimInstance Win32_Process` / `ps -e`), which
    // is exactly how the broker token leaked in the earlier design.
    if let Some(token) = &cfg.gateway_token {
        cmd.env("ACOWORK_MEMORY_GATEWAY_TOKEN", token);
    }

    // On Unix, create a new process group so a Gateway shutdown does not
    // cascade a SIGHUP to the service (it self-exits via the ADR-018 watchdog).
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let child = cmd.spawn().map_err(|e| {
        format!(
            "Failed to spawn acowork-memory (binary: {:?}): {}",
            cfg.memory_bin, e
        )
    })?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(pid, port = cfg.port, "memory process spawned");
    Ok((child, pid))
}

/// Probe `GET /health` on the memory port.
async fn check_memory_health(port: u16) -> bool {
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
            .build()
            .expect("Failed to build memory supervisor HTTP client")
    })
}

/// Clear `memory_process` from shared state (service gone / gave up).
async fn clear_state(state: &SharedState) {
    let mut gw = state.write().await;
    gw.memory_process = None;
}

