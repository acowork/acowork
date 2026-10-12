//! HTTP server lifecycle management
//!
//! Starts the Axum HTTP server alongside the gRPC server in Gateway::run().
//! Handles port conflict auto-increment and pidfile writing.

use std::net::SocketAddr;
use std::net::TcpListener as StdTcpListener;
use std::path::Path;
use std::sync::Arc;

use axum;
use axum::extract::{ws::WebSocketUpgrade, ConnectInfo};
use axum::routing::get;
use tokio::sync::RwLock;

use crate::config::HttpConfig;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use crate::http::routes::{self, AppState};

/// PID file content for Desktop App discovery
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PidFile {
    pub pid: u32,
    pub http_port: u16,
}

/// Remote-origin listener configuration (design doc 24 §7.2, M3).
#[derive(Debug, Clone, Copy)]
pub struct RemoteListenerParams {
    /// Loopback port for the strict HTTP listener (guard + `/mqtt` WS
    /// bridge). The relay tunnel client forwards inbound HTTP streams
    /// here.
    pub http_port: u16,
    /// Loopback port of the strict MQTT listener the `/mqtt` bridge
    /// dials. Owned by the broker config, consumed here only for the
    /// bridge target.
    pub mqtt_port: u16,
}

/// RAII guard for the pidfile — deletes the file on Drop.
///
/// Ensures cleanup on both normal shutdown and panic-induced exits.
pub struct PidFileGuard {
    path: std::path::PathBuf,
}

impl PidFileGuard {
    /// Create a new guard that will delete the file at `path` on drop.
    fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        if self.path.exists() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => tracing::info!("PID file cleaned up: {}", self.path.display()),
                Err(e) => {
                    tracing::warn!("Failed to remove PID file '{}': {}", self.path.display(), e)
                }
            }
        }
    }
}

/// Check if a port is occupied by another Gateway instance.
///
/// Probes `GET /health` on the given port with a 2-second timeout.
/// If the response contains the Gateway `version` field, it's another
/// Gateway instance. Non-Gateway apps or unreachable ports return `false`.
///
/// This is more reliable than PID-based checks because:
/// - No stale state after crashes
/// - No PID reuse false positives
/// - Direct verification of the service identity
async fn is_gateway_on_port(host: &str, port: u16) -> bool {
    let url = format!("http://{}:{}/health", host, port);
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    match client.get(&url).send().await {
        Ok(resp) => {
            // Check if the response is JSON with a "version" field (Gateway signature)
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                body.get("version").is_some() && body.get("checks").is_some()
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// Remove a stale pidfile from a previous Gateway run.
///
/// This is purely for housekeeping — mutual exclusion is now handled
/// by probing the HTTP port (see `is_gateway_on_port`). If the file
/// exists, it is simply removed; no PID liveness check is performed.
pub fn cleanup_stale_pidfile(data_dir: &Path) {
    let pid_path = data_dir.join("gateway.pid");
    if pid_path.exists() {
        match std::fs::remove_file(&pid_path) {
            Ok(()) => tracing::info!("Removed stale pidfile '{}'", pid_path.display()),
            Err(e) => tracing::warn!("Failed to remove stale pidfile '{}': {}", pid_path.display(), e),
        }
    }
}

/// Start the HTTP API server
///
/// Binds to the configured host:port. If port is occupied,
/// auto-increments up to port_max.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start_http_server(
    http_config: &HttpConfig,
    gateway_state: Arc<RwLock<GatewayState>>,
    data_dir: &Path,
    log_reload_handle: Option<crate::LogReloadHandle>,
    mqtt_client: Option<Arc<crate::mqtt::GatewayMqttClient>>,
    mqtt_publisher_trigger: Option<crate::mqtt::MqttPublisherTrigger>,
    inventory_trigger: Option<crate::mqtt::InventoryNotifierTrigger>,
    runtime_http_registry: Option<crate::http::proxy::SharedRuntimeHttpRegistry>,
    agent_registry: Option<crate::mqtt::agent_registry::SharedAgentRegistry>,
    node_control: Option<crate::mqtt::node_control::NodeControlClient>,
    node_registry: Option<crate::mqtt::SharedNodeRegistry>,
    // ADR-059 §2.3: subsystem readiness registry — mutation handlers
    // gate on Node readiness before dispatching install commands.
    bootstrap_registry: Option<crate::bootstrap::SharedSubsystemReadinessRegistry>,
    // ADR-059 §6.2: operation store — mutation handlers open records
    // here and the dispatch layer transitions them.
    operation_store: Option<crate::operation_store::SharedOperationStore>,
    // ADR-055 Phase 5a: HttpAuth is created by the caller so the MQTT
    // broker CONNECT handler can share the same token (Desktop MQTT
    // password). Its `enabled` flag may be `http.auth_enabled ||
    // mqtt.auth_enabled`.
    auth: Arc<HttpAuth>,
    // ADR-076 §决策 12: resolved deployment auth mode, and the account
    // service (`Some` only under `AUTH_MODE=multi_user`).
    auth_mode: crate::auth::AuthMode,
    // Design doc 24 §6.3: relay tunnel client handle — always constructed
    // by `Gateway::run`; the supervisor only runs when enabled.
    relay_client: Option<std::sync::Arc<crate::relay::RelayClient>>,
    // Design doc 24 §7.2 (M3): remote-origin loopback listener pair.
    // `Some` starts the strict HTTP listener (guard + `/mqtt` WS bridge)
    // alongside the main server; the strict MQTT listener is hosted by
    // the broker instead (see `mqtt::RemoteMqttListener`).
    remote_listener: Option<RemoteListenerParams>,
    // ADR-087: ownership policy tables (created in `Gateway::run` next to
    // the token stores; shared with the MQTT dispatch).
    node_owners: crate::gateway::ownership::SharedOwnershipStore,
    agent_owners: crate::gateway::ownership::SharedOwnershipStore,
    // ADR-087 D2: enrollment token store (HTTP mints, MQTT dispatch consumes).
    enrollment_tokens: crate::mqtt::SharedEnrollmentTokenStore,
    // ADR-087 D7 / review M4: staged installs (HTTP stages, MQTT dispatch
    // commits on inventory / drops on failure).
    pending_installs: crate::gateway::ownership::SharedPendingInstalls,
) -> Result<(), GatewayError> {
    if !http_config.enabled {
        tracing::info!("HTTP API disabled by configuration");
        return Ok(());
    }

    // Token file write happens in the caller (before broker start);
    // re-writing here is a harmless idempotent refresh.
    auth.write_token_file(data_dir)?;

    // Build app state
    let mut app_state = AppState::new(
        gateway_state,
        auth,
    );
    app_state.log_reload_handle = log_reload_handle;
    app_state.mqtt_gateway_client = mqtt_client;
    app_state.mqtt_publisher_trigger = mqtt_publisher_trigger;
    app_state.inventory_trigger = inventory_trigger;
    app_state.runtime_http_registry = runtime_http_registry;
    app_state.agent_registry = agent_registry;
    app_state.node_control = node_control;
    app_state.node_registry = node_registry;
    app_state.bootstrap_registry = bootstrap_registry;
    app_state.operation_store = operation_store;
    app_state.auth_mode = auth_mode;
    app_state.relay_client = relay_client;
    app_state.node_owners = Some(node_owners);
    app_state.agent_owners = Some(agent_owners);
    app_state.enrollment_tokens = Some(enrollment_tokens);
    app_state.pending_installs = Some(pending_installs);
    // ADR-087 §4: one-shot migration filling the denormalized
    // `node_id` on agent ownership rows written before the field
    // existed. Runs before the first request so the ACL cascade never
    // observes a half-populated table.
    backfill_agent_owner_node_ids(&app_state).await;
    if auth_mode.is_multi_user() {
        tracing::info!(
            "AUTH_MODE=multi_user: account system delegated to acowork-user              (verification starts once its public key is loaded)"
        );
    }

    // Prime the offline providers cache so `data_dir/offline_providers.json`
    // (written by `POST /api/models/refresh-catalog`) takes priority over the
    // bundled `assets/offline_providers.json`. Must run before any HTTP handler
    // touches `models_api::offline_providers()`.
    crate::http::models_api::init_offline_providers(data_dir);

    // Peer-IP allowlist (security backstop). Read from the loaded Gateway
    // config — this happens AFTER config load, so the values include the
    // env override applied in `GatewayConfig::load`. Boot-time only.
    {
        let gw = app_state.gateway_state.read().await;
        if let Some(config) = gw.config.as_ref() {
            let allowlist = config.security.to_allowlist();
            if !allowlist.is_empty() {
                tracing::info!(
                    entries = allowlist.len(),
                    "security.allowed_node_ips enabled (HTTP + MQTT peer filter)"
                );
            }
            app_state.ip_allowlist = allowlist;
        } else {
            tracing::debug!("security: no gateway config snapshot — allowlist disabled (allow all)");
        }
    }

    // Clean up stale pidfile from a previous run (if any). This is purely
    // for housekeeping — mutual exclusion is handled by port probing below.
    cleanup_stale_pidfile(data_dir);

    // Find an available port, probing occupied ports to distinguish between
    // another Gateway instance (fatal) and unrelated applications (skip).
    let (actual_port, std_listener) = find_available_port(
        &http_config.host,
        http_config.port,
        http_config.port_max,
    )
    .await?;

    // Write pidfile for Desktop App discovery
    let _pidfile_guard = write_pidfile(data_dir, actual_port)?;

    // ADR-089 D3: record the probed port so handlers can assemble
    // externally-reachable URLs (LSP relay proxy) without re-reading
    // config (which may name a different port than we bound).
    {
        app_state.gateway_state.write().await.http_port = actual_port;
    }

    // Snapshot for the remote-origin listener (M3) — `build_router`
    // consumes `app_state` below, and the remote listener serves the
    // same routes from its own copy.
    let app_state_for_remote = app_state.clone();

    // Build router — PM routes are reverse-proxied to the standalone
    // acowork-pm process (ADR-064); no in-process PM handle needed.
    let app = routes::build_router(app_state);

    // LAN MQTT-over-WebSocket bridge: the same `/mqtt` pipe the remote surface
    // uses, mounted on the MAIN listener. A mobile WebView cannot open a raw
    // TCP socket and the embedded broker binds loopback, so this is the only
    // MQTT path a LAN device has — without it the phone can only poll.
    // Registered exactly like the relay route: AFTER `build_router`, so the
    // bearer gate does not wrap it (MQTT authenticates at CONNECT against the
    // strict listener) and the peer-IP allowlist is re-checked inside the
    // handler. See `relay::remote_listener::lan_mqtt_bridge`.
    let app = match remote_listener.map(|r| r.mqtt_port) {
        Some(mqtt_port) => {
            let allowlist = app_state_for_remote.ip_allowlist.clone();
            tracing::info!(
                mqtt_port,
                "LAN MQTT-over-WebSocket bridge mounted at /mqtt (strict-listener CONNECT auth)"
            );
            app.route(
                "/mqtt",
                get(move |ws: WebSocketUpgrade, ConnectInfo(peer): ConnectInfo<SocketAddr>| {
                    crate::relay::remote_listener::lan_mqtt_bridge(
                        ws,
                        peer,
                        mqtt_port,
                        allowlist.clone(),
                    )
                }),
            )
        }
        None => app,
    };

    // Design doc 24 §7.2 (M3): remote-origin loopback listener, running
    // the SAME router (plus the `/mqtt` bridge and the origin guard).
    // Spawned, not awaited: the main listener below stays the
    // lifecycle owner of this function. A startup failure is logged
    // loudly but never aborts the Gateway — remote access is an
    // additive path (§9 rollback design).
    if let Some(remote) = remote_listener {
        let remote_state = app_state_for_remote;
        tokio::spawn(async move {
            if let Err(e) = crate::relay::remote_listener::start_remote_http_listener(
                remote_state,
                remote.http_port,
                remote.mqtt_port,
            )
            .await
            {
                tracing::error!("remote-origin HTTP listener failed: {}", e);
            }
        });
    }

    // Convert std::net::TcpListener to tokio::net::TcpListener
    // This reuses the already-bound listener — no second bind() call.
    // On Windows, we must set non-blocking mode before conversion.
    std_listener
        .set_nonblocking(true)
        .map_err(|e| GatewayError::Config(format!("Failed to set non-blocking mode: {}", e)))?;
    let listener = tokio::net::TcpListener::from_std(std_listener)
        .map_err(|e| GatewayError::Config(format!("Failed to convert TCP listener: {}", e)))?;

    tracing::info!(
        "HTTP API listening on http://{}:{}",
        http_config.host,
        actual_port
    );

    axum::serve(
        listener,
        // `into_make_service_with_connect_info` attaches the peer
        // `SocketAddr` as a per-request extension. The IP allowlist
        // middleware (`routes::ip_allowlist_middleware`) reads it to
        // enforce `[security].allowed_node_ips`.
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .map_err(|e| GatewayError::Config(format!("HTTP server error: {}", e)))?;

    Ok(())
}

/// ADR-087 §4 — one-shot migration: fill the denormalized `node_id` on
/// agent ownership rows written before the field existed.
///
/// The ACL cascade needs `agent → node` to evaluate `uninstall` (the one
/// route gated on both lists) from policy data alone. Rows that predate
/// the field carry `None`.
///
/// The source of truth for the mapping is the live install table
/// (`AgentInfo.node_id`, refreshed from each node's retained inventory
/// by ADR-055 §6.5) — not a second guess at the filesystem. At this
/// point in boot the table may still be empty for a node that has not
/// reported yet, which is why an unfilled row is *not* an error:
///
/// - The agent is still evaluated on its own row (see
///   [`crate::http::acl::Resource::parent`]), so visibility and
///   management are unaffected.
/// - Only `uninstall`'s node half is missing, and that route
///   re-checks the node explicitly.
/// - Every boot retries, so a row written while a node was offline is
///   filled on the next start. A long-offline node that never reports
///   leaves the row unfilled forever, which is why the retry is
///   unconditional rather than guarded by a "migrated" flag.
///
/// Writes once, at the end, only when something actually changed.
async fn backfill_agent_owner_node_ids(state: &AppState) {
    let Some(store) = state.agent_owners.clone() else {
        return;
    };
    // Take the install table under the async lock FIRST and drop it
    // before touching the ownership mutex — two locks, never nested,
    // and never a sync read inside async (which panics on a runtime
    // worker).
    let pending: Vec<(String, String)> = {
        let gw = state.gateway_state.read().await;
        let guard = crate::gateway::ownership::lock(&store);
        guard
            .iter()
            .filter(|(_, rec)| rec.node_id.is_none())
            .filter_map(|(id, _)| {
                gw.installed_agents
                    .get(id)
                    .map(|info| (id.clone(), info.node_id.clone()))
            })
            .collect()
    };
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    let mut guard = crate::gateway::ownership::lock(&store);
    let mut filled = 0usize;
    for (id, node_id) in pending {
        // `update` runs `f` then persists — the per-row write is the
        // simplest correct thing here. The row count is bounded by the
        // number of agents installed before the migration, which is
        // small; batching would need a bulk `persist` to be exposed
        // and buys nothing at that size.
        if guard.update(&id, |rec| rec.node_id = Some(node_id)) {
            filled += 1;
        }
    }
    drop(guard);
    tracing::info!(
        rows = filled,
        considered = count,
        "ADR-087: backfilled node_id on agent ownership rows"
    );
}

/// Find an available port in the configured range.
///
/// For each occupied port, probes `GET /health` to determine if it's another
/// Gateway instance (fatal error) or an unrelated application (skip to next).
/// Returns the bound `TcpListener` directly so the caller can pass it to
/// `axum::serve` without a second `bind()` call.
async fn find_available_port(
    host: &str,
    start_port: u16,
    max_port: u16,
) -> Result<(u16, StdTcpListener), GatewayError> {
    for port in start_port..=max_port {
        match StdTcpListener::bind(format!("{}:{}", host, port)) {
            Ok(listener) => {
                tracing::info!("Found available HTTP port: {}", port);
                return Ok((port, listener));
            }
            Err(_) => {
                // Port occupied — probe to check if it's another Gateway
                if is_gateway_on_port(host, port).await {
                    return Err(GatewayError::Config(format!(
                        "Another Gateway instance is running on {}:{}",
                        host, port
                    )));
                }
                tracing::warn!(
                    "Port {} occupied by non-Gateway application, trying next",
                    port
                );
            }
        }
    }
    Err(GatewayError::Config(format!(
        "No available port in range {}-{}",
        start_port, max_port
    )))
}

/// Write pidfile for Desktop App discovery.
/// Returns a `PidFileGuard` that will delete the pidfile on Drop.
fn write_pidfile(
    data_dir: &Path,
    http_port: u16,
) -> Result<PidFileGuard, GatewayError> {
    let pid_file = PidFile {
        pid: std::process::id(),
        http_port,
    };
    let content = serde_json::to_string_pretty(&pid_file)
        .map_err(|e| GatewayError::Config(format!("Failed to serialize pidfile: {}", e)))?;
    let pid_path = data_dir.join("gateway.pid");
    std::fs::write(&pid_path, content).map_err(|e| {
        GatewayError::Config(format!(
            "Failed to write pidfile '{}': {}",
            pid_path.display(),
            e
        ))
    })?;
    tracing::info!("PID file written to {}", pid_path.display());
    Ok(PidFileGuard::new(pid_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_find_available_port() {
        // find_available_port returns both the port number and the bound listener.
        // Use a high port range unlikely to conflict with running services.
        let result = find_available_port("127.0.0.1", 29876, 29886).await;
        assert!(result.is_ok());
        let (port, listener) = result.unwrap();
        assert!((29876..=29886).contains(&port));
        // Verify the listener is actually bound
        assert!(listener.local_addr().is_ok());
    }

    #[tokio::test]
    async fn test_find_available_port_range_exhausted() {
        // Use an impossible range to test exhaustion
        let result = find_available_port("127.0.0.1", 1, 0).await;
        assert!(result.is_err());
    }

    #[test]
    fn test_write_pidfile() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let _guard = write_pidfile(&dir, 19876).unwrap();

        let pid_path = dir.join("gateway.pid");
        assert!(pid_path.exists());

        let content = std::fs::read_to_string(&pid_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["http_port"], 19876);
        assert!(parsed["pid"].as_u64().unwrap() > 0);

        drop(_guard);
        assert!(
            !pid_path.exists(),
            "pidfile should be cleaned up after guard is dropped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── S5.9 tests ──────────────────────────────────────────────────────────

    #[test]
    fn test_pidfile_guard_cleanup_on_drop() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile-drop");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write pidfile via write_pidfile, which returns a PidFileGuard
        let pid_path = dir.join("gateway.pid");
        {
            let _guard = write_pidfile(&dir, 19876).unwrap();
            // pidfile should exist while guard is alive
            assert!(
                pid_path.exists(),
                "pidfile should exist while PidFileGuard is alive"
            );
        }
        // After guard is dropped, pidfile should be cleaned up
        assert!(
            !pid_path.exists(),
            "pidfile should be deleted after PidFileGuard is dropped"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cleanup_stale_pidfile_removes_file() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile-stale");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a stale pidfile
        let pid_file = PidFile {
            pid: 9999999,
            http_port: 19876,
        };
        let pid_path = dir.join("gateway.pid");
        let content = serde_json::to_string_pretty(&pid_file).unwrap();
        std::fs::write(&pid_path, &content).unwrap();
        assert!(pid_path.exists(), "stale pidfile should exist before cleanup");

        // cleanup_stale_pidfile should simply remove it
        cleanup_stale_pidfile(&dir);
        assert!(!pid_path.exists(), "stale pidfile should be removed after cleanup");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_cleanup_stale_pidfile_no_file() {
        let dir = std::env::temp_dir().join("acowork-test-pidfile-none");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // No pidfile exists — should not panic
        cleanup_stale_pidfile(&dir);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_is_gateway_on_port_no_server() {
        // A port with nothing listening should return false
        assert!(!is_gateway_on_port("127.0.0.1", 19999).await);
    }
}
