//! Configuration-driven app process supervisor (ADR-116).
//!
//! Generic apps are declared entirely by `app.toml`; there is no per-app Rust
//! supervisor. Liveness uses the same reusable building blocks as the ADR-064
//! supervisors (`acowork-core::supervisor`), so restart behaviour is
//! identical across built-in and installed apps.
//!
//! Failure is non-fatal: a broken app never takes the Gateway down. Its
//! routes answer `503 + Retry-After: 2` until it becomes ready again.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{RwLock, watch};
use tokio::time::sleep;

use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{RestartHistory, backoff_with_jitter};

use crate::app::registry::{resolve_app_binary, AppProcessState};
use crate::gateway::state::GatewayState;

pub type SharedState = Arc<RwLock<GatewayState>>;

/// Stop signals for running generic supervisors (design doc 34 §6.10).
static APP_STOP: Mutex<Option<HashMap<String, watch::Sender<bool>>>> = Mutex::new(None);

fn stop_map() -> std::sync::MutexGuard<'static, Option<HashMap<String, watch::Sender<bool>>>> {
    APP_STOP.lock().unwrap_or_else(|e| e.into_inner())
}

/// Everything the generic supervisor needs to launch one app.
#[derive(Clone)]
pub struct GenericAppConfig {
    pub app_id: String,
    pub binary: PathBuf,
    pub port_hint: u16,
    pub port_file: PathBuf,
    pub log_dir: PathBuf,
    pub health_path: String,
    pub gateway_health_url: String,
    pub gateway_url: String,
    pub data_dir: PathBuf,
    /// Extra CLI arguments; `{gateway_url}` and `{data_dir}` are expanded.
    pub extra_args: Vec<String>,
    /// Extra environment variables; `{internal_service_token}`, `{gateway_url}`
    /// and `{data_dir}` are expanded. Secrets travel through env, never argv.
    pub env: BTreeMap<String, String>,
    pub internal_service_token: Option<String>,
    pub publisher_token: Option<String>,
}

/// Spawn the supervisor task for one generic app.
pub fn start_generic_app_supervisor(cfg: GenericAppConfig, state: SharedState) {
    let (tx, rx) = watch::channel(false);
    {
        let mut guard = stop_map();
        let map = guard.get_or_insert_with(HashMap::new);
        // Replacing an existing supervisor: signal the old one to stop.
        if let Some(old) = map.insert(cfg.app_id.clone(), tx) {
            let _ = old.send(true);
        }
    }
    tokio::spawn(async move {
        run_supervisor(cfg, state, rx).await;
    });
}

/// Stop a running generic supervisor and clear its process table entry.
///
/// Returns the PID that was signalled, if any, so uninstall can wait until
/// Windows releases the directory handle on `apps/{id}/versions/.../bin`.
pub async fn stop_app(app_id: &str, state: &SharedState) -> Option<u32> {
    if let Some(map) = stop_map().as_mut() {
        if let Some(tx) = map.remove(app_id) {
            let _ = tx.send(true);
        }
    }
    let pid = {
        let mut gw = state.write().await;
        gw.app_processes.remove(app_id).map(|p| p.pid)
    };
    if let Some(pid) = pid {
        let _ = crate::lifecycle::process::kill_agent_process(pid).await;
    }
    pid
}

/// Poll until `pid` is gone, or `timeout` elapses. Best-effort: uninstall
/// still proceeds if the process table lags.
pub async fn wait_process_gone(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !crate::lifecycle::process::check_health(pid).await {
            return;
        }
        sleep(Duration::from_millis(50)).await;
    }
    tracing::warn!(pid, "app process still listed after kill; continuing uninstall");
}

/// Resolve binary + start a generic app declared in the registry.
/// Returns an error string when the binary cannot be resolved; the caller
/// should surface `status=error` without rolling back an install.
pub fn start_app(
    data_dir: &Path,
    manifest: &acowork_core::app::AppManifest,
    state: SharedState,
    gateway_url: String,
    gateway_health_url: String,
    internal_service_token: Option<String>,
    publisher_token: Option<String>,
) -> Result<(), String> {
    if manifest.process.runtime != acowork_core::app::AppRuntime::Generic {
        return Err("start_app only applies to generic runtime apps".into());
    }
    let binary = resolve_app_binary(data_dir, manifest)?;
    let app_data = data_dir
        .join(acowork_core::app::APP_DATA_DIR_NAME)
        .join(&manifest.id);
    std::fs::create_dir_all(&app_data)
        .map_err(|e| format!("create app-data: {e}"))?;
    let cfg = config_from_manifest(
        manifest,
        binary,
        data_dir.to_path_buf(),
        data_dir.join("logs"),
        gateway_url,
        gateway_health_url,
        internal_service_token,
        publisher_token,
    );
    start_generic_app_supervisor(cfg, state);
    Ok(())
}

/// Restart = stop then start (used after install / rollback).
pub async fn restart_app(
    data_dir: &Path,
    manifest: &acowork_core::app::AppManifest,
    state: SharedState,
    gateway_url: String,
    gateway_health_url: String,
    internal_service_token: Option<String>,
    publisher_token: Option<String>,
) -> Result<(), String> {
    stop_app(&manifest.id, &state).await;
    start_app(
        data_dir,
        manifest,
        state,
        gateway_url,
        gateway_health_url,
        internal_service_token,
        publisher_token,
    )
}

async fn run_supervisor(
    cfg: GenericAppConfig,
    state: SharedState,
    mut stop: watch::Receiver<bool>,
) {
    let mut history = RestartHistory::new();
    loop {
        if *stop.borrow() {
            clear_state(&cfg.app_id, &state).await;
            return;
        }
        tokio::select! {
            _ = spawn_and_monitor(&cfg, &state) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    // Best-effort kill of the current process.
                    if let Some(proc) = {
                        let mut gw = state.write().await;
                        gw.app_processes.remove(&cfg.app_id)
                    } {
                        let _ = crate::lifecycle::process::kill_agent_process(proc.pid).await;
                    }
                    return;
                }
            }
        }
        if *stop.borrow() {
            return;
        }
        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(
                app = %cfg.app_id,
                attempts,
                "app restart limit exceeded; the Gateway keeps running and its routes return 503"
            );
            clear_state(&cfg.app_id, &state).await;
            return;
        }
        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(app = %cfg.app_id, attempt = attempts, ?backoff, "restarting app");
        tokio::select! {
            _ = sleep(backoff) => {}
            _ = stop.changed() => {
                if *stop.borrow() {
                    clear_state(&cfg.app_id, &state).await;
                    return;
                }
            }
        }
    }
}

fn expand(value: &str, cfg: &GenericAppConfig) -> String {
    let token = cfg.internal_service_token.clone().unwrap_or_default();
    let publisher = cfg.publisher_token.clone().unwrap_or_default();
    value
        .replace("{gateway_url}", &cfg.gateway_url)
        .replace("{data_dir}", &cfg.data_dir.to_string_lossy())
        .replace("{internal_service_token}", &token)
        .replace("{publisher_token}", &publisher)
        .replace("{app_id}", &cfg.app_id)
}

async fn spawn_and_monitor(cfg: &GenericAppConfig, state: &SharedState) {
    let _ = std::fs::remove_file(&cfg.port_file);
    let (mut child, pid) = match spawn_app(cfg).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(app = %cfg.app_id, error = %e, "failed to spawn app");
            return;
        }
    };

    let port = match wait_for_ready(cfg, pid).await {
        Some(p) => p,
        None => {
            tracing::warn!(app = %cfg.app_id, pid, "app did not become ready within startup grace");
            let _ = crate::lifecycle::process::kill_agent_process(pid).await;
            return;
        }
    };

    {
        let mut gw = state.write().await;
        gw.app_processes.insert(
            cfg.app_id.clone(),
            AppProcessState {
                pid,
                port,
                ready: true,
            },
        );
    }
    tracing::info!(app = %cfg.app_id, pid, port, "app process ready");

    let mut last_healthy = Instant::now();
    loop {
        tokio::select! {
            _ = sleep(Duration::from_secs(2)) => {
                if check_health(port, &cfg.health_path).await {
                    last_healthy = Instant::now();
                } else if last_healthy.elapsed() > supervisor_defaults::HEARTBEAT_TIMEOUT {
                    tracing::warn!(app = %cfg.app_id, pid, "app health failing — killing and restarting");
                    let _ = crate::lifecycle::process::kill_agent_process(pid).await;
                    clear_state(&cfg.app_id, state).await;
                    return;
                }
            }
            _ = child.wait() => {
                tracing::warn!(app = %cfg.app_id, pid, "app process exited");
                clear_state(&cfg.app_id, state).await;
                return;
            }
        }
    }
}

async fn wait_for_ready(cfg: &GenericAppConfig, pid: u32) -> Option<u16> {
    let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
    loop {
        if let Ok(text) = tokio::fs::read_to_string(&cfg.port_file).await {
            if let Ok(port) = text.trim().parse::<u16>() {
                if check_health(port, &cfg.health_path).await {
                    return Some(port);
                }
            }
        }
        if !crate::lifecycle::process::check_health(pid).await {
            return None;
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(supervisor_defaults::STARTUP_POLL).await;
    }
}

async fn spawn_app(cfg: &GenericAppConfig) -> Result<(tokio::process::Child, u32), String> {
    if !cfg.binary.exists() {
        return Err(format!("app binary not found at {:?}", cfg.binary));
    }
    std::fs::create_dir_all(&cfg.log_dir)
        .map_err(|e| format!("failed to create log dir {:?}: {e}", cfg.log_dir))?;
    let log_path = cfg.log_dir.join(format!("{}.log", cfg.app_id));
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("failed to create app log {:?}: {e}", log_path))?;

    let mut cmd = tokio::process::Command::new(&cfg.binary);
    cmd.arg("--host")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(cfg.port_hint.to_string())
        .arg("--port-file")
        .arg(&cfg.port_file)
        .arg("--gateway-health-url")
        .arg(&cfg.gateway_health_url)
        .arg("--log-level")
        .arg("info")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));
    for arg in &cfg.extra_args {
        cmd.arg(expand(arg, cfg));
    }
    for (name, value) in &cfg.env {
        cmd.env(name, expand(value, cfg));
    }
    // Every generic app gets its own data directory. The Gateway never reads
    // it — this is the app's private filesystem, not a platform store.
    cmd.env("ACOWORK_APP_DATA_DIR", &cfg.data_dir);
    cmd.env("ACOWORK_APP_ID", &cfg.app_id);

    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn app {}: {e}", cfg.app_id))?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(app = %cfg.app_id, pid, port = cfg.port_hint, "app process spawned");
    Ok((child, pid))
}

async fn check_health(port: u16, path: &str) -> bool {
    let url = format!("http://127.0.0.1:{port}{path}");
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .expect("failed to build app supervisor HTTP client")
    });
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

async fn clear_state(app_id: &str, state: &SharedState) {
    let mut gw = state.write().await;
    gw.app_processes.remove(app_id);
}

/// Build the supervisor config from a declaration. Kept separate from the
/// spawn loop so it can be unit-tested without launching a process.
pub fn config_from_manifest(
    manifest: &acowork_core::app::AppManifest,
    binary: PathBuf,
    data_dir: PathBuf,
    log_dir: PathBuf,
    gateway_url: String,
    gateway_health_url: String,
    internal_service_token: Option<String>,
    publisher_token: Option<String>,
) -> GenericAppConfig {
    let port_file = data_dir.join(
        manifest
            .process
            .port_file
            .clone()
            .unwrap_or_else(|| format!("{}.port", manifest.id)),
    );
    GenericAppConfig {
        app_id: manifest.id.clone(),
        binary,
        port_hint: manifest.process.port_hint,
        port_file,
        log_dir,
        health_path: manifest.process.health_path.clone(),
        gateway_health_url,
        gateway_url,
        // Design doc 34 §6.4: private data lives under app-data/{id}, never
        // under apps/{id} (that tree holds the install ledger + package).
        data_dir: data_dir
            .join(acowork_core::app::APP_DATA_DIR_NAME)
            .join(&manifest.id),
        extra_args: manifest.process.extra_args.clone(),
        env: manifest.process.env.clone(),
        internal_service_token,
        publisher_token,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::app::PM_APP_TOML;

    #[test]
    fn config_expands_placeholders_without_leaking_secrets_into_argv() {
        let manifest = acowork_core::app::AppManifest::from_toml(PM_APP_TOML).unwrap();
        let mut manifest = manifest;
        manifest.process.extra_args = vec!["--gateway-url".into(), "{gateway_url}".into()];
        manifest
            .process
            .env
            .insert("APP_TOKEN".into(), "{internal_service_token}".into());
        let cfg = config_from_manifest(
            &manifest,
            PathBuf::from("bin"),
            PathBuf::from("/data"),
            PathBuf::from("/data/logs"),
            "http://127.0.0.1:19876".into(),
            "http://127.0.0.1:19876/health".into(),
            Some("secret-token".into()),
            None,
        );
        assert_eq!(cfg.port_file, PathBuf::from("/data/pm.port"));
        assert_eq!(cfg.data_dir, PathBuf::from("/data/app-data/pm"));
        assert_eq!(expand("{gateway_url}", &cfg), "http://127.0.0.1:19876");
        assert_eq!(
            expand("{internal_service_token}", &cfg),
            "secret-token"
        );
        assert!(
            cfg.extra_args.iter().all(|a| !a.contains("secret-token")),
            "credentials must never appear in argv"
        );
    }
}
