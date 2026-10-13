//! Gateway configuration and process management commands.
//!
//! Frontend is the source of truth for gateway mode + URL (persisted in
//! its settingsStore). On startup the frontend MUST call
//! [`set_gateway_config`] to push its persisted values into Rust so that
//! all HTTP commands use the correct base URL and the local spawn is
//! skipped in remote mode. See module docs of `crate::state` for details.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::state::{AppState, BootstrapStateView, GatewayMode};
use acowork_core::defaults;
use serde::{Deserialize, Serialize};
use tauri::Manager;

/// Payload for [`set_gateway_config`].
#[derive(Debug, Deserialize)]
pub struct GatewayConfigInput {
    /// `"local"`, `"remote"` or `"relay"` (anything else falls back to `local`)
    pub mode: String,
    /// User-configured Gateway URL. Used in ALL modes (single-topology:
    /// local mode defaults to `http://127.0.0.1:19876` but accepts any
    /// URL; the mode only controls whether Desktop spawns the Gateway.
    /// Relay mode points at the relay's device domain,
    /// e.g. `https://gw-abc.relay.example.com`).
    #[serde(default)]
    pub url: String,
}

/// Result of a local-gateway boot (probe-or-spawn).
///
/// `ownership` tells the frontend whether the reachable Gateway was
/// spawned by this Desktop App ("owned") or was already running and
/// merely adopted ("foreign"). Ownership drives the exit UX: Desktop
/// only offers to stop an *owned* Gateway on quit; a foreign Gateway
/// is never touched.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub enum GatewayOwnership {
    /// The Gateway was spawned by this Desktop process (and is tracked
    /// in `state.gateway_process`). Desktop may stop it on exit.
    Owned,
    /// A Gateway was already reachable at the configured URL and was
    /// adopted without spawning. Desktop must never stop it.
    Foreign,
}

impl GatewayOwnership {
    pub fn as_str(&self) -> &'static str {
        match self {
            GatewayOwnership::Owned => "owned",
            GatewayOwnership::Foreign => "foreign",
        }
    }
}

/// Returned by [`set_gateway_config`] so the frontend can re-sync after
/// reload / external mutation.
#[derive(Debug, Serialize)]
pub struct GatewayConfigOutput {
    pub mode: String,
    pub base_url: String,
}

/// Boot result returned by [`init_local_gateway`] / [`start_local_gateway`]:
/// the resolved base URL plus the ownership of the running Gateway.
#[derive(Debug, Serialize)]
pub struct GatewayBootResult {
    pub base_url: String,
    /// `"owned"` if Desktop spawned it, `"foreign"` if it was adopted.
    pub ownership: String,
}

/// Push the persisted gateway configuration from the frontend into Rust.
///
/// Mode-independent URL policy: **both** local and remote modes accept
/// the user-configured URL. This is the "single topology" principle —
/// local mode differs from remote only in that the Desktop App *spawns*
/// the Gateway when none is reachable; the Gateway itself, its config,
/// ports and bind addresses are identical in both modes. Local mode's
/// URL defaults to `http://127.0.0.1:19876` in the UI, but the user can
/// point it anywhere (including a LAN IP — in which case other machines
/// may connect to that Gateway exactly as if it were a "remote" one).
///
/// If the mode changes from local→remote/relay while a local Gateway is
/// running, the running local process is stopped to avoid leaving an
/// orphan on the shared port. The reverse (remote/relay→local) does NOT
/// auto-spawn; the frontend must call [`init_local_gateway`] to start a
/// new local instance.
#[tauri::command]
pub async fn set_gateway_config(
    state: tauri::State<'_, AppState>,
    config: GatewayConfigInput,
) -> Result<GatewayConfigOutput, String> {
    let mode = GatewayMode::from_str(&config.mode);
    tracing::info!(
        "[CFG] set_gateway_config: mode={:?}, url={:?}",
        mode,
        config.url
    );

    // Resolve base_url — mode-independent. Empty URL falls back to the
    // shared default (covers legacy local-mode callers that omit it).
    let trimmed = config.url.trim().trim_end_matches('/').to_string();
    let base_url = if trimmed.is_empty() {
        defaults::GATEWAY_HTTP_URL.to_string()
    } else {
        trimmed
    };

    // Update HTTP client base_url
    {
        let mut client = state.gateway.write().await;
        client.set_base_url(base_url.clone());
    }

    // Update mode
    {
        let mut m = state.gateway_mode.write().await;
        *m = mode;
    }

    // If switching away from local (to remote OR relay), stop any
    // locally-spawned Gateway to free the port — neither remote nor
    // relay mode ever uses the child, and an orphan on the shared port
    // would shadow a user-configured remote/relay Gateway.
    //
    // Use the pid-only stop (no sidecar name sweep): switching modes must
    // not kill a Desktop-created ADR-087 node that may be repointing at the
    // new remote/relay Gateway. In remote/relay mode there is no child, so
    // this block never runs and nothing local is touched.
    if mode != GatewayMode::Local {
        let mut proc = state.gateway_process.lock().await;
        if let Some(child) = proc.take() {
            tracing::info!(
                "[CFG] Switching to {}: stopping local Gateway (pid: {:?})",
                mode.as_str(),
                child.id()
            );
            let pid = child.id();
            crate::process_stop::stop_gateway_pid(pid);
            let mut child = child; // Child::wait needs &mut
            let _ = child.wait();
        }
    }

    Ok(GatewayConfigOutput {
        mode: mode.as_str().to_string(),
        base_url,
    })
}

/// Read back the current configuration from Rust. Used by the frontend to
/// detect drift (e.g. after a Rust-side default change).
#[tauri::command]
pub async fn get_gateway_config(
    state: tauri::State<'_, AppState>,
) -> Result<GatewayConfigOutput, String> {
    let mode = *state.gateway_mode.read().await;
    let client = state.gateway.read().await;
    Ok(GatewayConfigOutput {
        mode: mode.as_str().to_string(),
        base_url: client.base_url().to_string(),
    })
}

/// Mirror the webview's account session into Rust (ADR-076 §决策 3).
///
/// The webview owns the token pair — it is the only party allowed to call
/// `/api/auth/refresh`, because refresh rotation has no grace window and
/// two concurrent rotations would revoke the whole family (ADR-076
/// §10.1 #3). Rust therefore only mirrors the current access token, so that
/// every command-issued Gateway request carries `Authorization: Bearer`,
/// and asks the webview for a newer one when the Gateway answers 401
/// (`gateway-auth-required` event → webview rotates → this command again).
///
/// `None` clears the mirror (logout, or a refresh that ended the session)
/// and releases any request still waiting on a renewal.
#[tauri::command]
pub async fn set_gateway_access_token(
    state: tauri::State<'_, AppState>,
    access_token: Option<String>,
) -> Result<(), String> {
    state.gateway_auth.set_access_token(access_token);
    Ok(())
}

/// Ensure a Gateway is running at the configured URL (local mode) and
/// wait for bootstrap READY.
///
/// This is the ONLY place local Gateway is spawned. It is called from the
/// frontend (SplashScreen init) after `set_gateway_config`, so we know:
///   - The mode is `local` (otherwise this is an error)
///   - `GatewayClient.base_url` is the address the user wants (default
///     `http://127.0.0.1:19876`, but any URL is accepted — single
///     topology, the mode only decides whether we spawn).
///
/// **Probe-then-spawn**: if a Gateway is already reachable at the
/// configured URL (started manually, left over from a previous run, or
/// on a remote host), it is adopted without spawning and reported as
/// `ownership = "foreign"`. Only when nothing answers do we spawn a
/// child (`ownership = "owned"`). This is what makes local mode work
/// against an already-running Gateway — Desktop never fights over the
/// port.
///
/// ADR-059: returns once the Gateway's bootstrap snapshot reaches
/// `phase=READY` (max ~30s) — one wait covers liveness AND subsystem
/// readiness (vault / mqtt / node.local / system_agent / publisher).
/// On timeout a structured `dependency_not_ready` is returned; the old
/// `/health` poll is gone (liveness-only endpoint, §1.3).
#[tauri::command]
pub async fn init_local_gateway(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<GatewayBootResult, DependencyNotReady> {
    {
        let m = state.gateway_mode.read().await;
        if *m != GatewayMode::Local {
            return Err(DependencyNotReady::install_failed(format!(
                "init_local_gateway called in {:?} mode; refusing to spawn local process",
                *m
            )));
        }
    }

    let base_url = state.gateway.read().await.base_url().to_string();

    // Probe-then-spawn: the spawn path itself re-checks reachability under
    // a lock (concurrency-safe); here we just need the outcome.
    let ownership = spawn_gateway(&state, &app_handle)
        .await
        .map_err(DependencyNotReady::install_failed)?;

    // Wait for READY on the configured URL. When ownership is "foreign",
    // the gateway is already up; this wait doubles as the health probe.
    let client = bootstrap_probe_client()?;
    wait_for_bootstrap_ready(&client, &base_url, BOOTSTRAP_TIMEOUT_SECS).await?;

    Ok(GatewayBootResult {
        base_url,
        ownership: ownership.as_str().to_string(),
    })
}

/// Time budget for waiting on the Gateway bootstrap phase (seconds).
/// Shared by every command that gates on bootstrap READY.
const BOOTSTRAP_TIMEOUT_SECS: u64 = 30;

/// HTTP client used to poll `/api/bootstrap` (short per-request timeout;
/// the poll loop in [`wait_for_bootstrap_ready`] retries every 500 ms).
fn bootstrap_probe_client() -> Result<reqwest::Client, DependencyNotReady> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| {
            DependencyNotReady::install_failed(format!("Failed to build HTTP client: {}", e))
        })
}

/// Structured error: a prerequisite of a bootstrap-dependent command was
/// not satisfied.
///
/// Serialised as-is into the Tauri `Err` payload, so the frontend can
/// branch on `error_code` instead of string-matching (ADR-059 §7).
#[derive(Debug, Serialize)]
pub struct DependencyNotReady {
    /// `"dependency_not_ready"` (bootstrap never reached READY) or
    /// `"install_failed"` (internal install error, kept for parity).
    pub error_code: String,
    /// What was not ready, e.g. `"gateway.bootstrap"`. Empty for
    /// `install_failed` (general command failure — no specific subsystem
    /// was the blocker; the caller decides how to phrase it).
    pub dependency: String,
    /// Bootstrap phase observed at failure time (empty for install errors).
    pub phase: String,
    /// Human-readable detail for the log / diagnostics.
    pub detail: String,
    /// Timeout budget that elapsed (seconds).
    pub timeout_secs: u64,
}

impl DependencyNotReady {
    fn not_ready(dependency: &str, phase: &str, detail: String, timeout_secs: u64) -> Self {
        Self {
            error_code: "dependency_not_ready".to_string(),
            dependency: dependency.to_string(),
            phase: phase.to_string(),
            detail,
            timeout_secs,
        }
    }

    fn install_failed(detail: String) -> Self {
        Self {
            error_code: "install_failed".to_string(),
            dependency: String::new(),
            phase: String::new(),
            detail,
            timeout_secs: 0,
        }
    }
}

/// Return the latest Gateway bootstrap snapshot.
///
/// Priority:
/// 1. `AppState.bootstrap_state` — kept fresh by the MQTT bootstrap
///    handler (`acowork/global/bootstrap` retained topic, ADR-059);
/// 2. one-shot `GET /api/bootstrap` when the cache is empty (MQTT not
///    connected yet) — result is cached for subsequent calls.
#[tauri::command]
pub async fn get_bootstrap(
    state: tauri::State<'_, AppState>,
) -> Result<BootstrapStateView, String> {
    // Cache hit: MQTT retained re-delivery already gave us the snapshot.
    if let Some(view) = state.bootstrap_state.read().await.clone() {
        tracing::debug!(
            "[BOOTSTRAP] get_bootstrap cache hit phase={} version={}",
            view.phase,
            view.version
        );
        return Ok(view);
    }

    // Cache miss: pull once over HTTP and cache the result.
    let base_url = state.gateway.read().await.base_url().to_string();
    tracing::debug!("[BOOTSTRAP] get_bootstrap cache miss — HTTP fallback to {}", base_url);
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;
    let resp = client
        .get(format!("{}/api/bootstrap", base_url))
        .send()
        .await
        .map_err(|e| format!("GET /api/bootstrap failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!(
            "GET /api/bootstrap returned HTTP {}",
            resp.status()
        ));
    }
    let view: BootstrapStateView = resp
        .json()
        .await
        .map_err(|e| format!("GET /api/bootstrap parse error: {}", e))?;
    tracing::debug!(
        "[BOOTSTRAP] get_bootstrap HTTP fallback OK phase={} version={}",
        view.phase,
        view.version
    );
    *state.bootstrap_state.write().await = Some(view.clone());
    Ok(view)
}

/// Wait for the Gateway's bootstrap phase to reach `READY`.
///
/// Polls `GET /api/bootstrap` — the readiness source of truth
/// (ADR-059 §5.1) — every 500 ms until:
/// - `READY` → `Ok(snapshot)` — every required subsystem is up;
/// - `DEGRADED` → `Ok(snapshot)` with a warning — optional subsystems
///   failed but the Gateway still serves its feature surface;
/// - `FAILED` / `SHUTTING_DOWN` → immediate `Err` — do not burn the
///   remaining timeout on a dead Gateway;
/// - the timeout elapses → `Err(DependencyNotReady)`.
///
/// Replaces the old "poll /health + fixed sleep" + `wait_for_node_online`
/// steps: subsystem readiness (vault / mqtt / node.local / system_agent /
/// publisher) is aggregated into the phase, so one wait covers what the
/// two heuristics approximated.
async fn wait_for_bootstrap_ready(
    client: &reqwest::Client,
    gateway_url: &str,
    timeout_secs: u64,
) -> Result<BootstrapStateView, DependencyNotReady> {
    use tokio::time::{sleep, Duration};
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    let mut last: Option<BootstrapStateView> = None;
    while std::time::Instant::now() < deadline {
        match client
            .get(format!("{}/api/bootstrap", gateway_url))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<BootstrapStateView>().await {
                    Ok(view) => match view.phase.as_str() {
                        "READY" => {
                            tracing::info!("[BOOTSTRAP] Gateway READY (v{})", view.version);
                            return Ok(view);
                        }
                        "DEGRADED" => {
                            tracing::warn!(
                                "[BOOTSTRAP] Gateway DEGRADED ({}), proceeding",
                                view.phase_detail
                            );
                            return Ok(view);
                        }
                        "FAILED" | "SHUTTING_DOWN" => {
                            return Err(DependencyNotReady::not_ready(
                                "gateway.bootstrap",
                                &view.phase,
                                format!("Gateway {}: {}", view.phase, view.phase_detail),
                                timeout_secs,
                            ));
                        }
                        _ => {
                            last = Some(view);
                        }
                    },
                    Err(e) => {
                        tracing::debug!("[BOOTSTRAP] /api/bootstrap parse error: {}", e);
                    }
                }
            }
            Ok(resp) => {
                // 404: pre-ADR-059 Gateway without /api/bootstrap — keep
                // polling until the timeout so the error message below is
                // accurate about what happened.
                tracing::debug!(
                    "[BOOTSTRAP] /api/bootstrap returned HTTP {}",
                    resp.status()
                );
            }
            Err(e) => {
                tracing::debug!("[BOOTSTRAP] /api/bootstrap request error: {}", e);
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(DependencyNotReady::not_ready(
        "gateway.bootstrap",
        last.as_ref().map(|v| v.phase.as_str()).unwrap_or("unknown"),
        format!("Gateway did not reach phase READY within {}s", timeout_secs),
        timeout_secs,
    ))
}

/// Start the local Gateway process (used by the Settings page "Start" button).
///
/// Refuses in remote mode. Use [`init_local_gateway`] on first launch
/// instead — this entry point is only for manual restart.
///
/// ADR-059: waits for bootstrap `phase=READY` (same wait as
/// [`init_local_gateway`]); never polls `/health` for readiness.
#[tauri::command]
pub async fn start_local_gateway(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> Result<GatewayBootResult, DependencyNotReady> {
    {
        let m = state.gateway_mode.read().await;
        if *m != GatewayMode::Local {
            return Err(DependencyNotReady::install_failed(format!(
                "start_local_gateway called in {:?} mode; refusing to spawn",
                *m
            )));
        }
    }
    let ownership = spawn_gateway(&state, &app_handle)
        .await
        .map_err(DependencyNotReady::install_failed)?;
    let base_url = state.gateway.read().await.base_url().to_string();
    let client = bootstrap_probe_client()?;
    wait_for_bootstrap_ready(&client, &base_url, BOOTSTRAP_TIMEOUT_SECS).await?;
    Ok(GatewayBootResult {
        base_url,
        ownership: ownership.as_str().to_string(),
    })
}

/// Ensure a Gateway child is running; report who owns it.
/// Used by both `start_local_gateway` command and `init_local_gateway`.
///
/// # Concurrency
///
/// This function is safe to call concurrently from multiple Tauri commands.
/// The entire decision pipeline (`check in-process handle → probe URL →
/// kill stale → spawn → store`) runs under a single `Mutex` critical
/// section, so React StrictMode's dev double-invocation, or any other
/// race, results in **at most one** child Gateway process. Late callers
/// see the freshly-spawned child stored in `state.gateway_process` and
/// return immediately. A URL probe additionally detects Gateway
/// instances already running (started manually, or a leftover from a
/// crashed previous run still holding the port) and adopts them without
/// spawning — reported as [`GatewayOwnership::Foreign`].
pub async fn spawn_gateway(
    state: &AppState,
    app_handle: &tauri::AppHandle,
) -> Result<GatewayOwnership, String> {
    tracing::info!("[BOOT] spawn_gateway entered");

    // ── Single critical section: serialise check → spawn → store ─────
    // Holding the lock across the spawn guarantees that any concurrent
    // caller observes either `None` (and waits) or a valid `Child`
    // (and skips the work entirely).
    let mut proc_guard = state.gateway_process.lock().await;

    // 1) In-process handle check: did we already spawn one?
    if let Some(ref child) = *proc_guard
        && child_output_is_alive(child)
    {
        tracing::info!("[BOOT] Gateway already running (in-process handle), skipping spawn");
        return Ok(GatewayOwnership::Owned);
    }

    // 2) URL probe: is there already a Gateway listening on the
    //    configured URL? Covers a manually-started Gateway, a remote
    //    Gateway the user pointed local mode at, and a stale process
    //    from a previous run that is reachable but was never tracked in
    //    our handle. In every case we adopt rather than fight over the
    //    port (single-topology: the configured URL is authoritative).
    let base_url = state.gateway.read().await.base_url().to_string();
    if is_gateway_reachable(&base_url).await {
        tracing::info!(
            "[BOOT] Gateway already reachable at {} — adopting (foreign), skipping spawn",
            base_url
        );
        return Ok(GatewayOwnership::Foreign);
    }

    // 3) Kill any stale local Gateway from a previous run.
    //    We only get here if no in-process handle AND the URL is
    //    unreachable, so nothing is legitimately holding the port.
    tracing::info!("[BOOT] Checking for stale Gateway processes...");
    kill_stale_gateway_process();
    tracing::info!("[BOOT] Stale Gateway cleanup done");

    // 4) Find binary + spawn the new child.
    let gateway_bin = find_gateway_binary(app_handle.clone())?;
    tracing::info!("[BOOT] Starting local Gateway: {}", gateway_bin.display());

    // Get Tauri resource directory — where bundled assets (lsp_servers.json,
    // lsp_install/, offline_providers.json, etc.) are extracted.
    // Pass it to Gateway via ACOWORK_LSP_CONFIG_DIR env var. The Gateway
    // passes this to the LSP Relay subprocess, which uses it to locate
    // lsp_servers.json and install scripts.
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    tracing::info!("[BOOT] Tauri resource_dir: {}", resource_dir.display());

    let mut command = Command::new(&gateway_bin);
    command
        .current_dir(&resource_dir)
        .env("ACOWORK_GATEWAY_DAEMON", "true")
        .env("ACOWORK_GATEWAY_LOG_LEVEL", "debug")
        .env("RUST_LOG", "debug,h2=info,hyper=info,hyper_util=info,tower=info")
        .env(
            "ACOWORK_LSP_CONFIG_DIR",
            resource_dir.to_string_lossy().to_string(),
        )
        // Pin the advertise host to loopback (ADR-055 D3). The Gateway's
        // HTTP API listens only on `config.http.host` (127.0.0.1 by
        // default), and the local node downloads .agent packages from
        // `http://{advertise_host}:{port}/api/packages/...`. When
        // advertise_host is left unset the Gateway auto-detects a LAN IP
        // (resolve_advertise_host), which a loopback-only listener cannot
        // serve — every install then fails with connection refused and
        // onboarding stalls on "waiting" states. Remote nodes are
        // unaffected: they keep using the gateway's own resolved
        // advertise host when configured.
        .env("ACOWORK_GATEWAY_ADVERTISE_HOST", "127.0.0.1")
        // Pin the BIND addresses to loopback too (CLI > TOML). A config
        // file left over from remote-style experiments (`[http]`/`[mqtt]`
        // host = 0.0.0.0) would otherwise leak a Desktop-spawned Gateway
        // onto the LAN and make it IP-sensitive again. Local mode is
        // loopback-only: remote nodes attach to a remote-mode Gateway,
        // never to one spawned by Desktop.
        .arg("--addr")
        .arg(format!("127.0.0.1:{}", defaults::GATEWAY_HTTP_PORT))
        .arg("--mqtt-addr")
        .arg(format!("127.0.0.1:{}", defaults::GATEWAY_MQTT_PORT));
    // Suppress the pop-up Windows Terminal / conhost window. The Gateway
    // is a console-subsystem binary, so when spawned from a non-console
    // parent (the Tauri WebView2 host), Windows otherwise allocates a new
    // console and the user's screen gets a flashing terminal showing the
    // gateway's startup banner — confusing in a desktop app. Logs are
    // already captured by the Gateway's own file logger
    // (data/logs/gateway.log), and debug output is forwarded to the
    // Tauri dev console in dev mode. CREATE_NO_WINDOW has no effect on
    // macOS / Linux; the cfg gate keeps it platform-correct.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    if let Some(ort_lib_dir) = locate_bundled_ort_lib_dir(&gateway_bin, &resource_dir) {
        inject_ort_env(&mut command, &ort_lib_dir);
        tracing::info!(ort_lib_dir = %ort_lib_dir.display(), "Injected bundled ORT for Gateway");
    }
    let child = command
        .spawn()
        .map_err(|e| format!("Failed to spawn Gateway process: {}", e))?;

    let pid = child.id();
    tracing::info!("[BOOT] Gateway process spawned, pid: {:?}", pid);

    // 5) Store the handle. The next concurrent caller will see this
    //    inside the same lock and return early at step 1.
    //
    // NOTE: no Windows Job Object / KILL_ON_JOB_CLOSE here. Gateway
    // lifecycle is a *policy decision* made at exit time (see
    // `state.gateway_exit_policy` and the tray quit dialog): the user may
    // choose to keep the Gateway running after Desktop quits. Job-object
    // auto-kill would make that impossible, and explicit `taskkill /T /F`
    // in `stop_local_gateway` already provides the same process-tree kill
    // when stopping IS requested. A crashed Desktop may leave an orphaned
    // Gateway behind; it is then adopted (not duplicated) by the next run's
    // probe-then-spawn, so this is safe.
    *proc_guard = Some(child);

    Ok(GatewayOwnership::Owned)
}

/// Stop the locally running Gateway process and all its children.
///
/// On Windows, uses `taskkill /T /F` to kill the process tree, then reaps the
/// sidecars that detach from that tree (LSP relay, Node, the Runtimes the Node
/// hosts, embed, PM, doc, user) by image name — see [`crate::process_stop`].
/// On Unix, sends SIGINT so the Gateway's signal handler cleans up
/// children before exiting.
#[tauri::command]
pub async fn stop_local_gateway(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut proc = state.gateway_process.lock().await;
    if let Some(child) = proc.take() {
        let pid = child.id();
        tracing::info!(pid = pid, "Stopping local Gateway process tree");

        crate::process_stop::stop_gateway_process(pid);

        // Reap the child to avoid zombies
        let mut child = child; // Child::wait needs &mut
        let _ = child.wait();
    }
    *proc = None;
    Ok(())
}

/// Check if the local Gateway process is currently running.
#[tauri::command]
pub async fn get_local_gateway_status(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    let proc = state.gateway_process.lock().await;
    if let Some(ref child) = *proc {
        Ok(child_output_is_alive(child))
    } else {
        Ok(false)
    }
}

// ── Helper functions ────────────────────────────────────────────────────

fn locate_bundled_ort_lib_dir(gateway_bin: &Path, resource_dir: &Path) -> Option<PathBuf> {
    let gateway_dir = gateway_bin.parent();
    let candidates = [gateway_dir, Some(resource_dir)];
    for dir in candidates.into_iter().flatten() {
        if ort_lib_exists(dir) {
            return Some(dir.to_path_buf());
        }
    }
    locate_workspace_ort_lib_dir()
}

fn locate_workspace_ort_lib_dir() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let mut base = PathBuf::from(manifest_dir);
    for _ in 0..3 {
        base = base.parent()?.to_path_buf();
    }
    let ort_base = base.join(".ort");
    let entries = std::fs::read_dir(ort_base).ok()?;
    for entry in entries.flatten() {
        let lib_dir = entry.path().join("lib");
        if ort_lib_exists(&lib_dir) {
            return Some(lib_dir);
        }
    }
    None
}

fn ort_lib_exists(dir: &Path) -> bool {
    let lib_path = if cfg!(windows) {
        dir.join("onnxruntime.dll")
    } else if cfg!(target_os = "macos") {
        dir.join("libonnxruntime.dylib")
    } else {
        dir.join("libonnxruntime.so")
    };
    lib_path.exists()
}

fn inject_ort_env(command: &mut Command, ort_lib_dir: &Path) {
    let dylib_path = if cfg!(windows) {
        ort_lib_dir.join("onnxruntime.dll")
    } else if cfg!(target_os = "macos") {
        ort_lib_dir.join("libonnxruntime.dylib")
    } else {
        ort_lib_dir.join("libonnxruntime.so")
    };
    command.env("ORT_LIB_LOCATION", ort_lib_dir);
    command.env("ORT_DYLIB_PATH", dylib_path);
    command.env("ORT_PREFER_DYNAMIC_LINK", "1");

    #[cfg(windows)]
    {
        let previous = std::env::var("PATH").unwrap_or_default();
        command.env("PATH", format!("{};{}", ort_lib_dir.display(), previous));
    }
    #[cfg(target_os = "macos")]
    {
        let previous = std::env::var("DYLD_LIBRARY_PATH").unwrap_or_default();
        command.env(
            "DYLD_LIBRARY_PATH",
            format!("{}:{}", ort_lib_dir.display(), previous),
        );
    }
    #[cfg(target_os = "linux")]
    {
        let previous = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
        command.env(
            "LD_LIBRARY_PATH",
            format!("{}:{}", ort_lib_dir.display(), previous),
        );
    }
}

/// Find the Gateway binary next to the current executable.
pub fn find_gateway_binary(app_handle: tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    find_bundled_binary(&app_handle, "acowork-gateway")
}

/// Locate a bundled sibling binary (`acowork-gateway`, `acowork-node`, …)
/// next to the Desktop executable or in the resource directory; in dev
/// builds falls back to the workspace `target/{profile}` dirs.
/// (ADR-087: extracted from `find_gateway_binary` for `create_local_node`.)
pub fn find_bundled_binary(
    app_handle: &tauri::AppHandle,
    stem: &str,
) -> Result<std::path::PathBuf, String> {
    // In development, binaries live next to current_exe in target/release/
    // or target/debug/. In production (bundled), they're extracted next
    // to the Desktop app.
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("Failed to get current exe path: {}", e))?
        .parent()
        .ok_or("Current exe has no parent directory")?
        .to_path_buf();

    // Also check the Tauri resource directory (for bundled builds)
    let resource_dir = app_handle.path().resource_dir().unwrap_or(exe_dir.clone());

    let names = [format!("{stem}.exe"), stem.to_string()];
    for dir in [&exe_dir, &resource_dir] {
        for name in &names {
            let path = dir.join(name);
            if path.exists() {
                return Ok(path);
            }
        }
    }

    // Also try the workspace target/ directory for dev convenience.
    // Check release first, then debug.
    if let Ok(manifest_dir) =
        std::env::var("CARGO_MANIFEST_DIR").or_else(|_| std::env::var("TAURI_DEV_DIR"))
    {
        let manifest_path = std::path::PathBuf::from(&manifest_dir);
        // manifest_dir = .../apps/acowork-desktop/src-tauri
        // Go up 3 levels to workspace root: .../apps/acowork-desktop/src-tauri -> .../
        let mut base = manifest_path.clone();
        for _ in 0..3 {
            if let Some(parent) = base.parent() {
                base = parent.to_path_buf();
            } else {
                break;
            }
        }

        for profile in &["release", "debug"] {
            let target_dir = base.join("target").join(profile);
            for name in &names {
                let path = target_dir.join(name);
                if path.exists() {
                    return Ok(path);
                }
            }
        }
    }

    Err(format!(
        "Binary `{stem}` not found. Searched: {:?}",
        [&exe_dir, &resource_dir]
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
    ))
}

/// Read this machine's node identity (`{home}/identity.json`).
///
/// Returns `(node_id, enrolled)`: `node_id` is the stable routing key
/// (None when the file does not exist yet — the machine has never run a
/// node), `enrolled` is true when the identity carries a Gateway-issued
/// `node_token` (the machine has been bound to an account).
fn read_local_node_identity(home: &Path) -> (Option<String>, bool) {
    let Some(v) = std::fs::read(home.join("identity.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return (None, false);
    };
    let node_id = v
        .get("node_id")
        .and_then(|t| t.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let enrolled = v
        .get("node_token")
        .and_then(|t| t.as_str())
        .is_some_and(|s| !s.is_empty());
    (node_id, enrolled)
}

/// Is a local node on THIS machine already running?
///
/// Fast path: a running node answers `/health` on the local proxy port.
/// Fallback (fixes the old `ponytail` gap): a node whose proxy failed to
/// bind (port taken by another process) is invisible to the probe but is
/// still registered online with the Gateway — reconcile the local
/// `identity.json` node_id against `GET /api/nodes` before declaring it
/// dead, so we never spawn a second process fighting over the same
/// identity (two processes re-registering the same node_id kick each
/// other off MQTT).
async fn local_node_running(
    client: &crate::gateway_client::GatewayClient,
    base_url: &str,
    local_node_id: Option<&str>,
) -> bool {
    let local_node_probe = format!("http://127.0.0.1:{}", acowork_core::node::NODE_PROXY_PORT);
    if is_gateway_reachable(&local_node_probe).await {
        return true;
    }
    let Some(id) = local_node_id else {
        return false;
    };
    let url = format!("{}/api/nodes", base_url.trim_end_matches('/'));
    let resp = match client
        .send(|| Ok(client.request(reqwest::Method::GET, &url)))
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!("local_node_running: GET /api/nodes failed: {e}");
            return false;
        }
    };
    let Ok(nodes) = resp.json::<serde_json::Value>().await else {
        return false;
    };
    nodes.as_array().is_some_and(|arr| {
        arr.iter().any(|n| {
            n.get("node_id").and_then(|v| v.as_str()) == Some(id)
                && n.get("online").and_then(|v| v.as_bool()).unwrap_or(false)
        })
    })
}

/// ADR-087: "Create local Node" — spawn an `acowork-node` Agent on this
/// machine, owned by the signed-in user.
///
/// The enrollment token is minted through the Gateway HTTP API (bound to
/// the caller's account) and handed to the child via argv — it never
/// surfaces in the UI, config files, or logs. Idempotent: a machine that
/// is already enrolled (`identity.json` carries a `node_token`) skips the
/// token step; the spawn just reconnects.
///
/// `silent` marks the Desktop's auto-start path (remote mode, resume an
/// existing enrollment after launch). A silent call on a NOT-yet-enrolled
/// machine returns `skipped_not_enrolled` instead of minting a token:
/// first-time enrollment binds an account-level resource and leaves a
/// persistent daemon on the machine — that stays an explicit user action
/// (the AgentList banner / menu button).
#[tauri::command]
pub async fn create_local_node(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    silent: Option<bool>,
) -> Result<serde_json::Value, String> {
    let node_bin = find_bundled_binary(&app_handle, "acowork-node")?;
    let home = acowork_core::node::default_node_home();
    let (local_node_id, enrolled) = read_local_node_identity(&home);

    let client = state.gateway.read().await;
    let base_url = client.base_url().to_string();

    // Dedup: never spawn a second node for the same identity.
    if local_node_running(&client, &base_url, local_node_id.as_deref()).await {
        tracing::info!("ADR-087: create_local_node — a local node is already running, skipping spawn");
        return Ok(serde_json::json!({ "already_running": true }));
    }

    if silent.unwrap_or(false) && !enrolled {
        tracing::info!("ADR-087: create_local_node(silent) — machine not enrolled yet, leaving first-run to the UI");
        return Ok(serde_json::json!({ "skipped_not_enrolled": true }));
    }

    let mqtt_host = reqwest::Url::parse(&base_url)
        .map_err(|e| format!("bad gateway base_url {base_url}: {e}"))?
        .host_str()
        .ok_or_else(|| "gateway base_url has no host".to_string())?
        .to_string();
    let status = client
        .system_status()
        .await
        .map_err(|e| format!("GET /api/status failed: {e}"))?;

    let token = if enrolled {
        None
    } else {
        let url = format!("{}/api/nodes/enrollment-tokens", base_url.trim_end_matches('/'));
        let resp = client
            .send(|| {
                Ok(client
                    .request(reqwest::Method::POST, &url)
                    .json(&serde_json::json!({ "ttl_seconds": 3600 })))
            })
            .await
            .map_err(|e| format!("enrollment-token request failed: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("enrollment-tokens: HTTP {}", resp.status()));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("enrollment-tokens response: {e}"))?;
        Some(
            body.get("token")
                .and_then(|t| t.as_str())
                .map(|s| s.to_string())
                .ok_or_else(|| "enrollment-tokens response has no `token`".to_string())?,
        )
    };

    let mut command = std::process::Command::new(&node_bin);
    command
        .arg("start")
        .arg("--gateway")
        .arg(format!("{mqtt_host}:{}", status.mqtt_port))
        .arg("--home")
        .arg(&home);
    if let Some(t) = token.as_deref() {
        command.arg("--token").arg(t);
    }
    // Same console-window suppression as the Gateway spawn: the node is a
    // console binary; a flashing terminal from a desktop app is noise.
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = command
        .spawn()
        .map_err(|e| format!("spawn acowork-node: {e}"))?;
    let pid = child.id();
    // Detached: the node outlives this command (and the Desktop process);
    // nobody reaps it, so forget the handle.
    std::mem::forget(child);
    tracing::info!(pid, enrolled, "ADR-087: local node started from Desktop");
    Ok(serde_json::json!({ "pid": pid, "was_enrolled": enrolled }))
}

/// ADR-087 follow-up: read-only local-node state for the Desktop startup
/// hook and the AgentList banner.
///
/// `enrolled` — this machine has an identity bound to an account
/// (`identity.json` carries a `node_token`). `running` — a node process
/// for this machine's identity is alive (proxy `/health` or Gateway
/// node-list reconciliation). Deliberately does NOT touch the bundled
/// binary: the banner needs the state before any spawn decision.
#[tauri::command]
pub async fn local_node_status(
    state: tauri::State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let home = acowork_core::node::default_node_home();
    let (local_node_id, enrolled) = read_local_node_identity(&home);
    let client = state.gateway.read().await;
    let base_url = client.base_url().to_string();
    let running = local_node_running(&client, &base_url, local_node_id.as_deref()).await;
    Ok(serde_json::json!({ "enrolled": enrolled, "running": running }))
}

/// One-shot health probe used inside `spawn_gateway` to detect a Gateway
/// already listening on the default port (e.g. orphaned by a previous
/// crash). Returns `true` as soon as `/health` responds once; returns
/// `false` on any error or timeout. Intentionally short timeout to keep
/// the spawn path snappy.
async fn is_gateway_reachable(base_url: &str) -> bool {
    let health_url = format!("{}/health", base_url);
    let probe = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build();
    let Ok(client) = probe else {
        tracing::warn!("is_gateway_reachable: failed to build probe client");
        return false;
    };
    match client.get(&health_url).send().await {
        Ok(resp) if resp.status().is_success() => true,
        Ok(resp) => {
            tracing::warn!(
                "is_gateway_reachable: {} returned HTTP {}",
                base_url,
                resp.status()
            );
            false
        }
        Err(e) => {
            tracing::warn!("is_gateway_reachable: {} probe send error: {e:?}", base_url);
            false
        }
    }
}

/// Inspect the default Node proxy port and return only listeners whose
/// executable is `acowork-node`. Other processes on the port are never killed.
#[tauri::command]
pub fn inspect_local_node_process() -> Result<Vec<u32>, String> {
    node_pids_listening_on(acowork_core::node::NODE_PROXY_PORT)
}

/// Stop Node listeners after frontend confirmation. Re-resolve and validate
/// immediately before termination so a changed port owner is not killed.
#[tauri::command]
pub fn stop_local_node_process() -> Result<Vec<u32>, String> {
    let pids = inspect_local_node_process()?;
    for pid in &pids {
        #[cfg(target_os = "windows")]
        let result = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
        #[cfg(not(target_os = "windows"))]
        let result = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output();
        match result {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return Err(format!("failed to stop acowork-node pid {pid}: {}", String::from_utf8_lossy(&output.stderr).trim()));
            }
            Err(error) => return Err(format!("failed to stop acowork-node pid {pid}: {error}")),
        }
    }
    Ok(pids)
}

/// Resolve listeners on the Node proxy port and retain only `acowork-node`.
fn node_pids_listening_on(port: u16) -> Result<Vec<u32>, String> {
    #[cfg(target_os = "windows")]
    let pids = {
        let output = std::process::Command::new("netstat")
            .args(["-ano"])
            .output()
            .map_err(|e| format!("netstat failed while checking Node port {port}: {e}"))?;
        if !output.status.success() {
            return Err(format!("netstat failed while checking Node port {port}"));
        }
        parse_netstat_listeners(&String::from_utf8_lossy(&output.stdout), port)
    };
    #[cfg(not(target_os = "windows"))]
    let pids = {
        let output = std::process::Command::new("lsof")
            .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
            .output()
            .map_err(|e| format!("lsof failed while checking Node port {port}: {e}"))?;
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.trim().parse::<u32>().ok())
            .collect::<Vec<_>>()
    };

    let mut nodes = Vec::new();
    for pid in pids {
        #[cfg(target_os = "windows")]
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output();
        #[cfg(not(target_os = "windows"))]
        let output = std::process::Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output();
        let output = output.map_err(|e| format!("failed to inspect process {pid}: {e}"))?;
        if !output.status.success() {
            return Err(format!("failed to inspect process {pid}"));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let name = stdout
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .split(',')
            .next()
            .unwrap_or_default()
            .trim_matches('"')
            .rsplit(['\\', '/'])
            .next()
            .unwrap_or_default();
        if name.eq_ignore_ascii_case("acowork-node")
            || name.eq_ignore_ascii_case("acowork-node.exe")
        {
            nodes.push(pid);
        } else {
            return Err(format!(
                "port {port} is occupied by PID {pid} ({name}), not acowork-node; refusing to stop it"
            ));
        }
    }
    nodes.sort_unstable();
    nodes.dedup();
    Ok(nodes)
}

/// Parse `netstat -ano` output, matching the local-address port exactly.
fn parse_netstat_listeners(output: &str, port: u16) -> Vec<u32> {
    output
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 5 || fields[0] != "TCP" || fields[3] != "LISTENING" {
                return None;
            }
            let local_port = fields[1].rsplit_once(':')?.1.parse::<u16>().ok()?;
            if local_port != port {
                return None;
            }
            fields[4].parse().ok()
        })
        .collect()
}

/// Check if a child process output indicates it is alive.
fn child_output_is_alive(child: &std::process::Child) -> bool {
    // Child::id() returns the PID assigned at spawn; if the process
    // was never spawned or has been reaped, id() still returns the
    // original PID. For our purposes, we treat a non-zero PID as
    // "was successfully spawned".
    child.id() > 0
}

/// Resolve the PIDs of processes listening on a TCP port (Windows
/// `netstat -ano`, the only built-in way to map port → PID).
#[cfg(target_os = "windows")]
fn pids_listening_on(port: u16) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("netstat").args(["-ano"]).output() else {
        tracing::warn!("netstat failed — cannot resolve PID for port {}", port);
        return Vec::new();
    };
    let needle = format!(":{} ", port);
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            if !line.contains("LISTENING") || !line.contains(&needle) {
                return None;
            }
            line.split_whitespace().last()?.parse().ok()
        })
        .collect()
}

/// Kill any stale local Gateway process left from a previous Desktop App run.
///
/// This is only safe to call when we KNOW we're in local mode (and therefore
/// are about to spawn a new child Gateway that needs the default port).
/// All call sites go through [`spawn_gateway`], which is itself only reachable
/// from `init_local_gateway` / `start_local_gateway`, both of which check
/// `gateway_mode == Local` first.
fn kill_stale_gateway_process() {
    #[cfg(target_os = "windows")]
    {
        // Kill only the process(es) actually holding the Gateway HTTP
        // port — never `taskkill /IM acowork-gateway.exe`, which would
        // also kill a healthy Gateway started outside Tauri that we are
        // about to reuse (the reachability probe already returned false,
        // so anything on the port here is genuinely stale).
        let stale_pids = pids_listening_on(defaults::GATEWAY_HTTP_PORT);
        if stale_pids.is_empty() {
            return;
        }
        tracing::info!(
            pids = ?stale_pids,
            "Killing stale Gateway process(es) holding port {}",
            defaults::GATEWAY_HTTP_PORT
        );
        for pid in stale_pids {
            let output = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
            match output {
                Ok(o) if o.status.success() => {
                    tracing::info!("Killed stale Gateway process (pid {}) from previous run", pid);
                }
                _ => {
                    tracing::warn!("Failed to kill stale Gateway process (pid {})", pid);
                }
            }
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = std::process::Command::new("pkill")
            .args(["-f", "acowork-gateway"])
            .output();
        match output {
            Ok(o) if o.status.success() => {
                tracing::info!("Killed stale Gateway process from previous run");
            }
            _ => {
                // No stale process found
            }
        }
    }
}

// ============================================================================
// MQTT broker debug controls (ADR-XXX)
// ============================================================================
//
// These commands proxy to the Gateway's debug HTTP endpoints
// (`POST /api/debug/mqtt/{shutdown,start}`) so the frontend status bar
// can exercise the MQTT reconnect path without DevTools. They are
// idempotent and only do anything when the local Gateway is running
// in the standard configuration.
// ============================================================================

#[derive(Debug, Serialize)]
pub struct MqttDebugResponse {
    pub ok: bool,
    pub message: String,
}

async fn call_mqtt_debug_endpoint(
    state: tauri::State<'_, AppState>,
    action: &str,
) -> Result<MqttDebugResponse, String> {
    // Routed through `GatewayClient` so the request carries the account
    // credentials and the transparent 401 replay (ADR-076 §决策 3) — this
    // debug endpoint is not a public Gateway path.
    let gw = state.gateway.read().await;
    let url = format!("{}/api/debug/mqtt/{}", gw.base_url(), action);
    let resp = gw
        .send(|| Ok(gw.request(reqwest::Method::POST, &url)))
        .await
        .map_err(|e| format!("Gateway unreachable at {}: {}", url, e))?;
    drop(gw);

    let status = resp.status();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("Failed to read Gateway response body: {}", e))?;
    tracing::info!(
        action,
        %status,
        content_type = %content_type,
        body_len = body_bytes.len(),
        body_preview = %String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(256)]),
        "MQTT debug endpoint raw response"
    );
    // Try to parse as JSON first. If that fails (e.g. axum returned a
    // text/plain error page from a panic), fall back to treating the
    // body as a plain-text error message so the user still sees what
    // went wrong.
    let body: serde_json::Value = match serde_json::from_slice(&body_bytes) {
        Ok(v) => v,
        Err(parse_err) => {
            let text = String::from_utf8_lossy(&body_bytes);
            return Err(format!(
                "Gateway returned non-JSON response (status={}, content-type={:?}, body_len={}): {} | body: {}",
                status,
                content_type,
                body_bytes.len(),
                parse_err,
                if text.len() > 200 { &text[..200] } else { &text },
            ));
        }
    };

    let ok = status.is_success()
        && body.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let message = body
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("(no message)")
        .to_string();

    tracing::info!(action, ok, %status, "MQTT debug endpoint returned: {}", message);
    Ok(MqttDebugResponse { ok, message })
}

/// `POST /api/debug/mqtt/shutdown` — gracefully stop the broker.
#[tauri::command]
pub async fn debug_mqtt_shutdown(
    state: tauri::State<'_, AppState>,
) -> Result<MqttDebugResponse, String> {
    call_mqtt_debug_endpoint(state, "shutdown").await
}

/// `POST /api/debug/mqtt/start` — restart the broker.
#[tauri::command]
pub async fn debug_mqtt_start(
    state: tauri::State<'_, AppState>,
) -> Result<MqttDebugResponse, String> {
    call_mqtt_debug_endpoint(state, "start").await
}

#[cfg(test)]
mod local_node_identity_tests {
    use super::{parse_netstat_listeners, read_local_node_identity};
    use std::path::PathBuf;

    /// Create an isolated temp dir for one case; the dir name doubles as
    /// the uniqueness guard (no tempfile dev-dependency in this crate).
    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acowork-localnode-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn netstat_listener_parse_matches_exact_port_and_state() {
        let output = "  TCP    0.0.0.0:19900   0.0.0.0:0   LISTENING   1234\n  TCP    0.0.0.0:119900  0.0.0.0:0   LISTENING   5678\n  TCP    127.0.0.1:19900 127.0.0.1:1 ESTABLISHED 9999\n  TCP    [::]:19900      [::]:0      LISTENING   42\n";
        assert_eq!(parse_netstat_listeners(output, 19900), vec![1234, 42]);
    }

    #[test]
    fn missing_identity_is_unenrolled_without_id() {
        let dir = scratch_dir("missing");
        let (node_id, enrolled) = read_local_node_identity(&dir);
        assert_eq!(node_id, None);
        assert!(!enrolled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unparsable_identity_degrades_to_missing() {
        let dir = scratch_dir("garbage");
        std::fs::write(dir.join("identity.json"), b"not json").unwrap();
        let (node_id, enrolled) = read_local_node_identity(&dir);
        assert_eq!(node_id, None);
        assert!(!enrolled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identity_without_token_is_created_not_enrolled() {
        let dir = scratch_dir("notoken");
        std::fs::write(
            dir.join("identity.json"),
            br#"{"node_id":"abc-123","node_name":"box","gateway_managed":false,"created_at":"2026-01-01T00:00:00Z","enrollment":"created"}"#,
        )
        .unwrap();
        let (node_id, enrolled) = read_local_node_identity(&dir);
        assert_eq!(node_id.as_deref(), Some("abc-123"));
        assert!(!enrolled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn identity_with_token_is_enrolled() {
        let dir = scratch_dir("token");
        std::fs::write(
            dir.join("identity.json"),
            br#"{"node_id":"abc-123","node_name":"box","gateway_managed":false,"node_token":"tok","created_at":"2026-01-01T00:00:00Z","enrollment":"enrolled"}"#,
        )
        .unwrap();
        let (node_id, enrolled) = read_local_node_identity(&dir);
        assert_eq!(node_id.as_deref(), Some("abc-123"));
        assert!(enrolled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn empty_token_string_counts_as_not_enrolled() {
        let dir = scratch_dir("emptytoken");
        std::fs::write(
            dir.join("identity.json"),
            br#"{"node_id":"abc-123","node_name":"box","gateway_managed":false,"node_token":"","created_at":"2026-01-01T00:00:00Z"}"#,
        )
        .unwrap();
        let (_, enrolled) = read_local_node_identity(&dir);
        assert!(!enrolled);
        std::fs::remove_dir_all(&dir).ok();
    }
}
