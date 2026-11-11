//! acowork-doc process supervisor (ADR-064 pattern).
//!
//! Spawns the standalone `acowork-doc` binary, waits for `/health` ready,
//! monitors liveness, and restarts with exponential backoff on crash —
//! identical to the PM supervisor. The reusable building blocks
//! (`RestartHistory`, `backoff_with_jitter`, `supervisor_defaults`) come
//! from `acowork-core::supervisor` (ADR-019).
//!
//! Failure detection (two layers, same as PM):
//!   1. `child.wait()` returning — process crashed or was killed.
//!   2. `/health` failing for > `HEARTBEAT_TIMEOUT` — process alive but stuck.
//!
//! Both trigger the same restart path with exponential backoff (1s → 60s
//! cap) and a 5-attempts/5-min cap. On reaching the cap the supervisor
//! gives up and clears `doc_process` — the Gateway keeps running and
//! `/api/doc/*` returns 503.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{RestartHistory, backoff_with_jitter};

use crate::gateway::state::GatewayState;

/// Shared gateway state handle (same as the PM / embed supervisors).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// doc process runtime state (written into `GatewayState.doc_process`,
/// read by the reverse proxy for the actual port).
#[derive(Debug, Clone)]
pub struct DocProcessState {
    pub pid: u32,
    pub port: u16,
    pub ready: bool,
}

/// Supervisor config (spawn parameters for acowork-doc).
#[derive(Clone)]
pub struct DocSupervisorConfig {
    /// `acowork-doc` binary path (`current_exe().parent()` sibling).
    pub doc_bin: PathBuf,
    /// Desired port (default 18081; doc auto-increments on conflict, the
    /// actual port is reported via port_file).
    pub port: u16,
    /// Path where doc writes its actual bound port (under Gateway data dir).
    pub port_file: PathBuf,
    /// Log directory (`{gateway.data_dir}/logs`); doc stderr → `doc.log`.
    pub log_dir: PathBuf,
    /// Gateway `/health` URL (ADR-018: doc self-exits when the Gateway dies).
    pub gateway_health_url: String,
    /// Optional data-dir override forwarded via `--data-dir`.
    pub data_dir: Option<PathBuf>,
    /// Optional update-request TTL override forwarded via
    /// `--request-ttl-hours`.
    pub request_ttl_hours: Option<u32>,
    /// Broker port forwarded via `--mqtt-port` so the doc process can
    /// publish `acowork/doc/tree/changed` to the embedded broker
    /// (always loopback: doc is spawned on this host, next to the
    /// broker — host is fixed to 127.0.0.1).
    pub mqtt_port: u16,
    /// Broker CONNECT password forwarded via `--mqtt-password` when
    /// `mqtt.auth_enabled` is on: the Gateway's internal publisher token,
    /// which the broker admits `doc:service` with (ADR-084 §决策 4b).
    /// `None` (auth off, the default) forwards nothing.
    pub mqtt_password: Option<String>,
    /// Gateway HTTP base URL forwarded via `--gateway-url` so the doc
    /// process can push review receipts (plan §7 G5). `None` disables the
    /// push (standalone runs).
    pub gateway_url: Option<String>,
    /// Internal-service Bearer token passed via env
    /// `ACOWORK_DOC_GATEWAY_TOKEN` (never argv — same channel as PM's
    /// `ACOWORK_PM_GATEWAY_TOKEN`).
    pub gateway_token: Option<String>,
}

/// Spawn the doc supervisor task. Non-fatal: if doc cannot start, the
/// Gateway keeps running and `/api/doc/*` returns 503.
pub fn start_doc_supervisor(cfg: DocSupervisorConfig, state: SharedState) {
    tokio::spawn(async move {
        run_supervisor(cfg, state).await;
    });
}

async fn run_supervisor(cfg: DocSupervisorConfig, state: SharedState) {
    let mut history = RestartHistory::new();

    loop {
        spawn_and_monitor(&cfg, &state).await;

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(
                attempts,
                "doc restart limit exceeded; giving up (Gateway keeps running, /api/doc/* returns 503)"
            );
            clear_state(&state).await;
            return;
        }
        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting doc process");
        sleep(backoff).await;
    }
}

/// Spawn doc, wait for ready, then monitor until it dies or gets stuck.
async fn spawn_and_monitor(cfg: &DocSupervisorConfig, state: &SharedState) {
    // Remove a stale port file so the previous run's port is not mistaken
    // for this run's readiness.
    let _ = std::fs::remove_file(&cfg.port_file);

    let (child, pid) = match spawn_doc(cfg).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to spawn doc process");
            return;
        }
    };

    // Startup grace: wait for port file + /health ready.
    let port = match wait_for_ready(cfg, pid).await {
        Some(p) => p,
        None => {
            tracing::warn!("doc did not become ready within startup grace");
            let _ = crate::lifecycle::process::kill_agent_process(pid).await;
            return;
        }
    };

    // Update shared state so the reverse proxy can route to doc.
    {
        let mut gw = state.write().await;
        gw.doc_process = Some(DocProcessState {
            pid,
            port,
            ready: true,
        });
    }
    tracing::info!(pid, port, "doc process ready");

    // Monitor loop: `child.wait()` for exit, `/health` poll for stuck.
    let mut child = child;
    let mut last_healthy = Instant::now();
    loop {
        tokio::select! {
            _ = sleep(Duration::from_secs(2)) => {
                if check_doc_health(port).await {
                    last_healthy = Instant::now();
                } else if last_healthy.elapsed() > supervisor_defaults::HEARTBEAT_TIMEOUT {
                    tracing::warn!(
                        elapsed_secs = last_healthy.elapsed().as_secs(),
                        pid,
                        port,
                        "doc /health failing for too long — killing and restarting"
                    );
                    let _ = crate::lifecycle::process::kill_agent_process(pid).await;
                    clear_state(state).await;
                    return;
                }
            }
            _ = child.wait() => {
                tracing::warn!(pid, "doc process exited");
                clear_state(state).await;
                return;
            }
        }
    }
}

/// Wait up to `STARTUP_GRACE` for doc to write its port file and answer
/// `/health`. Returns the actual bound port, or `None` on timeout/death.
async fn wait_for_ready(cfg: &DocSupervisorConfig, pid: u32) -> Option<u16> {
    let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
    loop {
        if let Ok(port_str) = tokio::fs::read_to_string(&cfg.port_file).await
            && let Ok(port) = port_str.trim().parse::<u16>()
            && check_doc_health(port).await
        {
            return Some(port);
        }
        // Process died during boot?
        if !crate::lifecycle::process::check_health(pid).await {
            tracing::warn!(pid, "doc died during startup grace");
            return None;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "doc did not become ready within {:?}",
                supervisor_defaults::STARTUP_GRACE
            );
            return None;
        }
        sleep(supervisor_defaults::STARTUP_POLL).await;
    }
}

/// Spawn the `acowork-doc` process with supervisor-managed args.
async fn spawn_doc(cfg: &DocSupervisorConfig) -> Result<(tokio::process::Child, u32), String> {
    if !cfg.doc_bin.exists() {
        return Err(format!("acowork-doc binary not found at {:?}", cfg.doc_bin));
    }

    // Create log dir + open log file (truncate on each start).
    std::fs::create_dir_all(&cfg.log_dir)
        .map_err(|e| format!("Failed to create log dir {:?}: {}", cfg.log_dir, e))?;
    let log_path = cfg.log_dir.join("doc.log");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("Failed to create doc log file {:?}: {}", log_path, e))?;
    tracing::info!(path = %log_path.display(), "doc process logging to file");

    let mut cmd = tokio::process::Command::new(&cfg.doc_bin);
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
        // Embedded broker is on this host; loopback is always the right
        // connect target (a wildcard bind host like 0.0.0.0 is not a
        // valid TCP dial address on Windows).
        .arg("--mqtt-host")
        .arg("127.0.0.1")
        .arg("--mqtt-port")
        .arg(cfg.mqtt_port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));

    // ADR-084 §决策 4b: with broker auth on the broker refuses a
    // credential-less `doc:service`; forward the internal token.
    if let Some(password) = &cfg.mqtt_password {
        cmd.arg("--mqtt-password").arg(password);
    }

    // Optional overrides from the Gateway `[doc]` section.
    if let Some(dir) = &cfg.data_dir {
        cmd.arg("--data-dir").arg(dir);
    }
    if let Some(ttl) = cfg.request_ttl_hours {
        cmd.arg("--request-ttl-hours").arg(ttl.to_string());
    }

    // Plan §7 G5: the review receipt push needs the Gateway HTTP address.
    // The token goes through env, not argv (secrets never on the command
    // line — same rule as the PM supervisor's `ACOWORK_PM_GATEWAY_TOKEN`).
    // Failure to deliver a receipt is the doc side's warn-log, never a
    // review error: no retry machinery here by design.
    if let Some(gw_url) = &cfg.gateway_url {
        cmd.arg("--gateway-url").arg(gw_url);
    }
    if let Some(token) = &cfg.gateway_token {
        cmd.env("ACOWORK_DOC_GATEWAY_TOKEN", token);
    }

    // On Unix, create a new process group so a Gateway shutdown does not
    // cascade a SIGHUP to doc (doc self-exits via the ADR-018 watchdog).
    // `Command::process_group` is an inherent method (stable since 1.64),
    // so no `std::os::unix::process::CommandExt` import is needed.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn acowork-doc (binary: {:?}): {}", cfg.doc_bin, e))?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(pid, port = cfg.port, "doc process spawned");
    Ok((child, pid))
}

/// Probe `GET /health` on the doc port.
async fn check_doc_health(port: u16) -> bool {
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
            .expect("Failed to build doc supervisor HTTP client")
    })
}

/// Clear `doc_process` from shared state (doc gone / gave up).
async fn clear_state(state: &SharedState) {
    let mut gw = state.write().await;
    gw.doc_process = None;
}
