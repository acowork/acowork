//! acowork-user process supervisor (ADR-084, same pattern as PM / doc).
//!
//! Spawns the standalone `acowork-user` binary, waits for `/health` ready,
//! monitors liveness, and restarts with exponential backoff on crash —
//! identical to the doc supervisor. The reusable building blocks
//! (`RestartHistory`, `backoff_with_jitter`, `supervisor_defaults`) come from
//! `acowork-core::supervisor` (ADR-019).
//!
//! Two things this supervisor does that the PM / doc ones do not, both
//! because the user service is the **owner of the account data**:
//!
//! 1. **Loads the Ed25519 public key** once the service is up
//!    ([`load_verifier`]) and publishes it into
//!    `GatewayState.user_verifier`. The Gateway verifies access tokens
//!    locally with that key and never touches the account store
//!    (ADR-084 §决策 1/2). Until it is loaded the Gateway must **refuse**
//!    authenticated requests rather than treat them as anonymous.
//! 2. **Keeps a `/health` snapshot** ([`UserSnapshot`]) so `restricted_mode`
//!    and `/api/status` answer from memory instead of re-reading
//!    `accounts.json` on every request (ADR-084 §决策 4b).
//!
//! Failure detection (two layers, same as PM / doc):
//!   1. `child.wait()` returning — process crashed or was killed.
//!   2. `/health` failing for > `HEARTBEAT_TIMEOUT` — process alive but stuck.
//!
//! Both trigger the same restart path with exponential backoff (1s → 60s cap)
//! and a 5-attempts/5-min cap. On reaching the cap the supervisor gives up and
//! clears `user_process` — the Gateway keeps running and every user-domain
//! route returns 503.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use tokio::time::sleep;

use acowork_core::auth::TokenVerifier;
use acowork_core::health::supervisor_defaults;
use acowork_core::supervisor::{RestartHistory, backoff_with_jitter};

use crate::auth::AccountSystemState;
use crate::gateway::state::GatewayState;

/// Shared gateway state handle (same as the PM / doc supervisors).
pub type SharedState = Arc<RwLock<GatewayState>>;

/// user process runtime state (written into `GatewayState.user_process`,
/// read by the reverse proxy for the actual port).
#[derive(Debug, Clone)]
pub struct UserProcessState {
    pub pid: u32,
    pub port: u16,
    pub ready: bool,
}

/// The subset of the user service's `/health` details the Gateway consumes
/// (ADR-084 §决策 4b).
///
/// Rides the poll the supervisor already performs, so the restricted-mode
/// gate and `/api/status` need no extra round trip and no I/O on the hot path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSnapshot {
    /// The account system's first-boot state (ADR-114).
    ///
    /// `Uninitialized` = no account store / empty store: the first
    /// administrator has to be created. `NeedsSetup` = accounts exist but
    /// none of them can be logged into yet. `Ready` = at least one
    /// administrator can log in.
    ///
    /// While this is not `Ready` the Gateway answers 403 `setup_required`
    /// to everything but the first-boot setup surface, `/health` and
    /// `/api/status`.
    pub account_state: AccountSystemState,
    /// `[multi_user].registration_open` — whether non-admins may self-register.
    pub registration_open: bool,
    /// Live `user_profiles.json` version. The supervisor compares it against
    /// the cached snapshot on every `/health` poll and pulls when it moved —
    /// the healing path for a missed MQTT signal
    /// ([`crate::lifecycle::user_profile_sync`]).
    pub profile_version: u64,
}

impl Default for UserSnapshot {
    /// A snapshot with no information defaults to the *restricted* state.
    ///
    /// Production never constructs a default snapshot — the supervisor only
    /// publishes one built from a real `/health` reply, and "no snapshot yet"
    /// is represented by `Option::None` (which the gate treats as a startup
    /// window because `user_proxy` has no upstream in that window either).
    /// This `Default` exists for struct-update syntax in tests; keeping it
    /// fail-closed means an accidental production use cannot open the gate.
    fn default() -> Self {
        Self {
            account_state: AccountSystemState::NeedsSetup,
            registration_open: false,
            profile_version: 0,
        }
    }
}

impl UserSnapshot {
    /// Whether the first-boot gate is closed.
    pub fn requires_setup(&self) -> bool {
        self.account_state.requires_setup()
    }
}

/// Supervisor config (spawn parameters for acowork-user).
#[derive(Clone)]
pub struct UserSupervisorConfig {
    /// `acowork-user` binary path (`current_exe().parent()` sibling).
    pub user_bin: PathBuf,
    /// Desired port (default 18083; the service auto-increments on conflict,
    /// the actual port is reported via port_file).
    pub port: u16,
    /// Path where the service writes its actual bound port.
    pub port_file: PathBuf,
    /// Log directory (`{gateway.data_dir}/logs`); the service's stderr →
    /// `user.log`.
    pub log_dir: PathBuf,
    /// Gateway `/health` URL (ADR-018: the service self-exits when the
    /// Gateway dies).
    pub gateway_health_url: String,
    /// Optional `[user].data_dir` override forwarded via `--data-dir`.
    ///
    /// `None` (the usual case) forwards nothing and lets the service resolve
    /// its own default (ADR-084 §决策 5). The Gateway must not compute that
    /// default itself: doing so drifted from the service's under `--home`,
    /// and the mismatch silently turned every token into a rejected one.
    /// Where the service actually landed is read back from `/health`.
    pub data_dir: Option<PathBuf>,
    /// `[user].config` — the service's own TOML (bootstrap admin, password
    /// policy, registration flag), forwarded verbatim.
    pub config: Option<PathBuf>,
    /// Broker port forwarded via `--mqtt-port` so the service can publish its
    /// profile-change signal to the embedded broker (always loopback: the
    /// service is spawned on this host, next to the broker — the host is
    /// fixed to 127.0.0.1).
    pub mqtt_port: u16,
    /// Broker CONNECT password forwarded via `--mqtt-password` when
    /// `mqtt.auth_enabled` is on: the Gateway's internal publisher token,
    /// which the broker admits `user:service` with (ADR-084 §决策 4b).
    /// `None` (auth off, the default) forwards nothing.
    pub mqtt_password: Option<String>,
}

/// Spawn the user supervisor task. Non-fatal: if the service cannot start,
/// the Gateway keeps running and the user-domain routes return 503.
pub fn start_user_supervisor(cfg: UserSupervisorConfig, state: SharedState) {
    tokio::spawn(async move {
        run_supervisor(cfg, state).await;
    });
}

async fn run_supervisor(cfg: UserSupervisorConfig, state: SharedState) {
    let mut history = RestartHistory::new();

    loop {
        spawn_and_monitor(&cfg, &state).await;

        let attempts = history.record(supervisor_defaults::RESTART_WINDOW);
        if attempts as u32 > supervisor_defaults::MAX_RESTART_ATTEMPTS {
            tracing::error!(
                attempts,
                "user service restart limit exceeded; giving up (Gateway keeps running, \
                 user-domain routes return 503)"
            );
            clear_state(&state).await;
            return;
        }
        let backoff = backoff_with_jitter(
            attempts as u32,
            supervisor_defaults::RESTART_BACKOFF_MIN,
            supervisor_defaults::RESTART_BACKOFF_MAX,
        );
        tracing::info!(attempt = attempts, ?backoff, "Restarting user service");
        sleep(backoff).await;
    }
}

/// Spawn the service, wait for ready, then monitor until it dies or gets stuck.
async fn spawn_and_monitor(cfg: &UserSupervisorConfig, state: &SharedState) {
    // Remove a stale port file so the previous run's port is not mistaken for
    // this run's readiness.
    let _ = std::fs::remove_file(&cfg.port_file);

    let (child, pid) = match spawn_user(cfg).await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, "Failed to spawn user service");
            return;
        }
    };

    // Startup grace: wait for the port file + `/health` ready.
    let port = match wait_for_ready(cfg, pid).await {
        Some(p) => p,
        None => {
            tracing::warn!("user service did not become ready within startup grace");
            let _ = crate::lifecycle::process::kill_agent_process(pid).await;
            return;
        }
    };

    // One `/health` read serves both needs: the gate's snapshot and the
    // data directory the service resolved for itself (which is the only
    // place its public key can be).
    let details = match fetch_details(port).await {
        Some(d) => d,
        None => {
            tracing::warn!("user service answered, then failed /health; restarting");
            let _ = crate::lifecycle::process::kill_agent_process(pid).await;
            return;
        }
    };
    let snapshot = UserSnapshot {
        account_state: parse_account_state(details.get("account_state")),
        registration_open: details
            .get("registration_open")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        profile_version: details
            .get("user_profile_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    };

    // Publish the public key before advertising readiness: the proxy routes
    // to `port` the moment `user_process` is set, and a request that arrives
    // with a verifiable token but no verifier would be rejected.
    //
    // ADR-114: the key is loaded **unconditionally**. The account system is
    // always present, so the service always writes a key — there is no mode
    // in which "no verifier" is the correct steady state.
    let verifier = match details.get("data_dir").and_then(|v| v.as_str()) {
        Some(dir) => load_verifier(std::path::Path::new(dir)).await,
        None => {
            // Not fatal, but loud: the service is up and the Gateway still
            // cannot verify anything it signs.
            tracing::error!(
                "user service /health reports no data_dir; cannot locate its signing key"
            );
            None
        }
    };

    {
        let mut gw = state.write().await;
        gw.user_process = Some(UserProcessState {
            pid,
            port,
            ready: true,
        });
        if let Some(v) = verifier {
            gw.user_verifier = Some(Arc::new(v));
        }
        gw.user_snapshot = Some(snapshot);
    }
    tracing::info!(pid, port, "user service ready");

    // ADR-084 §决策 4b: prime the profile snapshot. The Gateway's cache starts
    // empty, so without this the retained `last_user_profile` a Runtime
    // subscribes to would stay blank until the next mutation. Must run after
    // the state write above (it reads `user_process` to find the port).
    crate::lifecycle::user_profile_sync::refresh(state).await;

    // Monitor loop: `child.wait()` for exit, `/health` poll for stuck.
    let mut child = child;
    let mut last_healthy = Instant::now();
    loop {
        tokio::select! {
            _ = sleep(Duration::from_secs(2)) => {
                match fetch_snapshot(port).await {
                    Some(s) => {
                        last_healthy = Instant::now();
                        let version_moved = {
                            let mut gw = state.write().await;
                            // Both the gate state and the profile version ride
                            // this poll: no extra round trip, no I/O on the
                            // request path (ADR-084 §决策 4b).
                            let moved = gw.resource_cache.user_profile_list.version
                                != s.profile_version;
                            gw.user_snapshot = Some(s);
                            moved
                        };
                        if version_moved {
                            // The MQTT signal is the fast path; this is what
                            // makes correctness independent of the broker.
                            crate::lifecycle::user_profile_sync::refresh(state).await;
                        }
                    }
                    None if last_healthy.elapsed() > supervisor_defaults::HEARTBEAT_TIMEOUT => {
                        tracing::warn!(
                            elapsed_secs = last_healthy.elapsed().as_secs(),
                            pid,
                            port,
                            "user service /health failing for too long — killing and restarting"
                        );
                        let _ = crate::lifecycle::process::kill_agent_process(pid).await;
                        clear_state(state).await;
                        return;
                    }
                    None => {}
                }
            }
            _ = child.wait() => {
                tracing::warn!(pid, "user service exited");
                clear_state(state).await;
                return;
            }
        }
    }
}

/// Wait up to `STARTUP_GRACE` for the service to write its port file and
/// answer `/health`. Returns the actual bound port, or `None` on timeout/death.
async fn wait_for_ready(cfg: &UserSupervisorConfig, pid: u32) -> Option<u16> {
    let deadline = Instant::now() + supervisor_defaults::STARTUP_GRACE;
    loop {
        if let Ok(port_str) = tokio::fs::read_to_string(&cfg.port_file).await
            && let Ok(port) = port_str.trim().parse::<u16>()
            && check_health(port).await
        {
            return Some(port);
        }
        // Process died during boot?
        if !crate::lifecycle::process::check_health(pid).await {
            tracing::warn!(pid, "user service died during startup grace");
            return None;
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                "user service did not become ready within {:?}",
                supervisor_defaults::STARTUP_GRACE
            );
            return None;
        }
        sleep(supervisor_defaults::STARTUP_POLL).await;
    }
}

/// Load the Ed25519 public key the service writes at startup (ADR-084 §决策 2).
///
/// `user_data_dir` is the directory the service *reports* it is using, not
/// the one the Gateway would have picked — see
/// [`UserSupervisorConfig::data_dir`].
///
/// Retries: the file appears a moment after `/health` first answers (the
/// service generates the key during boot, before binding). A gateway that
/// never gets it cannot authenticate anyone, so this is worth waiting for —
/// but it is not fatal, because a restart may succeed where this attempt
/// raced.
async fn load_verifier(user_data_dir: &std::path::Path) -> Option<TokenVerifier> {
    let path = user_data_dir.join("auth").join("ed25519.pub");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match TokenVerifier::from_public_key_file(&path) {
            Ok(v) => {
                tracing::info!(path = %path.display(), "loaded user service signing key");
                return Some(v);
            }
            Err(e) if Instant::now() < deadline => {
                tracing::debug!(error = %e, path = %path.display(), "public key not readable yet");
                sleep(Duration::from_millis(100)).await;
            }
            Err(e) => {
                // Loud: without this key every authenticated request 503s.
                tracing::error!(
                    error = %e,
                    path = %path.display(),
                    "failed to load the user service public key — authenticated requests \
                     will be refused until it is readable (ADR-084 §决策 2)"
                );
                return None;
            }
        }
    }
}

/// Probe `/health` and parse the Gateway-facing snapshot (ADR-084 §决策 4b).
///
/// Doubles as the liveness probe: `None` means either unreachable or a
/// non-success status, both of which the caller treats as "unhealthy".
async fn fetch_snapshot(port: u16) -> Option<UserSnapshot> {
    let details = fetch_details(port).await?;
    Some(UserSnapshot {
        account_state: parse_account_state(details.get("account_state")),
        registration_open: details
            .get("registration_open")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        profile_version: details
            .get("user_profile_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    })
}

/// Parse the account system's first-boot state out of the service's
/// `/health` details (ADR-114).
///
/// A missing or unrecognised value **fails closed** into
/// [`AccountSystemState::NeedsSetup`]. The old fail-open `Ready` was justified
/// by "the auth gate still requires a valid token, and a token cannot exist
/// before an administrator exists" — ADR-114 invalidated that reasoning:
/// `POST /api/auth/setup` is a token-less path that *creates* both the
/// administrator and its first token. A `/health` reply without this field is
/// a broken contract, not a deployment we should silently trust; closing the
/// gate is loud and recoverable, while opening it can hand the first admin
/// account to whoever reaches the endpoint.
///
/// "No `/health` reply at all" is a different case and is handled elsewhere:
/// `fetch_details()` returns `None`, no snapshot is published, and the gate's
/// `Option::None` startup-window default applies.
fn parse_account_state(raw: Option<&serde_json::Value>) -> AccountSystemState {
    match raw.and_then(|v| v.as_str()) {
        Some("uninitialized") => AccountSystemState::Uninitialized,
        Some("needs_setup") => AccountSystemState::NeedsSetup,
        Some("ready") => AccountSystemState::Ready,
        other => {
            tracing::warn!(
                ?other,
                "user service /health did not report a valid account_state; \
                 keeping the first-boot gate closed until it does"
            );
            AccountSystemState::NeedsSetup
        }
    }
}

/// `GET /health` → the `details` object.
async fn fetch_details(port: u16) -> Option<serde_json::Value> {
    let url = format!("http://127.0.0.1:{}/health", port);
    let resp = http_client()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    body.get("details").cloned()
}

/// Liveness-only probe (used during the startup grace, where a body parse
/// would be noise).
async fn check_health(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{}/health", port);
    match http_client()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// Shared HTTP client for supervisor probes (and the profile-snapshot pull).
pub(crate) fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build user supervisor HTTP client")
    })
}

/// Clear the user-service state (service gone / gave up).
///
/// `user_verifier` goes with it: keeping a verifier while the process is gone
/// would let the Gateway accept tokens it can no longer invalidate (a
/// password change revokes families only inside the service), so both are
/// dropped and the gate fails closed with 503.
async fn clear_state(state: &SharedState) {
    let mut gw = state.write().await;
    gw.user_process = None;
    gw.user_verifier = None;
    gw.user_snapshot = None;
}

/// Spawn the `acowork-user` process with supervisor-managed args.
async fn spawn_user(cfg: &UserSupervisorConfig) -> Result<(tokio::process::Child, u32), String> {
    if !cfg.user_bin.exists() {
        return Err(format!(
            "acowork-user binary not found at {:?}",
            cfg.user_bin
        ));
    }

    // Create log dir + open log file (truncate on each start).
    std::fs::create_dir_all(&cfg.log_dir)
        .map_err(|e| format!("Failed to create log dir {:?}: {}", cfg.log_dir, e))?;
    let log_path = cfg.log_dir.join("user.log");
    let log_file = std::fs::File::create(&log_path)
        .map_err(|e| format!("Failed to create user log file {:?}: {}", log_path, e))?;
    tracing::info!(path = %log_path.display(), "user service logging to file");

    let mut cmd = tokio::process::Command::new(&cfg.user_bin);
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
        // connect target (a wildcard bind host like 0.0.0.0 is not a valid
        // TCP dial address on Windows).
        .arg("--mqtt-host")
        .arg("127.0.0.1")
        .arg("--mqtt-port")
        .arg(cfg.mqtt_port.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(log_file));

    // On Unix, create a new process group so a Gateway shutdown does not
    // cascade a SIGHUP to the service (it self-exits via the ADR-018
    // watchdog). `Command::process_group` is an inherent method (stable since
    // 1.64), so no `std::os::unix::process::CommandExt` import is needed.
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }

    // ADR-084 §决策 4b: with broker auth on the broker refuses a
    // credential-less `user:service`; forward the internal token.
    if let Some(password) = &cfg.mqtt_password {
        cmd.arg("--mqtt-password").arg(password);
    }
    if let Some(dir) = &cfg.data_dir {
        cmd.arg("--data-dir").arg(dir);
    }
    if let Some(path) = &cfg.config {
        cmd.arg("--config").arg(path);
    }

    let child = cmd.spawn().map_err(|e| {
        format!(
            "Failed to spawn acowork-user (binary: {:?}): {}",
            cfg.user_bin, e
        )
    })?;
    let pid = child.id().unwrap_or(0);
    tracing::info!(pid, port = cfg.port, "user service spawned");
    Ok((child, pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn health_snapshot_states_parse_to_the_wire_spelling() {
        let cases = [
            ("uninitialized", AccountSystemState::Uninitialized),
            ("needs_setup", AccountSystemState::NeedsSetup),
            ("ready", AccountSystemState::Ready),
        ];
        for (raw, expected) in cases {
            assert_eq!(parse_account_state(Some(&json!(raw))), expected, "{raw}");
        }
    }

    /// Regression (ADR-114 review): fail-open `Ready` on a broken `/health`
    /// contract could unlock the token-less setup endpoint. Unknown and
    /// missing values must close the gate instead.
    #[test]
    fn a_missing_or_unknown_health_state_fails_closed() {
        for raw in [None, Some(json!("multi_user")), Some(json!(7)), Some(json!(null))] {
            let state = parse_account_state(raw.as_ref());
            assert_eq!(state, AccountSystemState::NeedsSetup, "raw = {raw:?}");
            assert!(state.requires_setup());
        }
        assert!(UserSnapshot::default().requires_setup());
    }
}
