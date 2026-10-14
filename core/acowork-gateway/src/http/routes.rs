//! HTTP route definitions
//!
//! All API routes are defined here. Handlers are split into sub-modules
//! per domain (agents, vault, config, chat, etc.).

use axum::extract::Request;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::auth::AuthMode;
use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use acowork_core::StructuredErrorBody;
use acowork_core::operation::{OperationId, OperationRecord, OperationState};

/// Global body-size cap applied at the root of the Gateway's
/// merged router. See [`api_router`] for why we override axum's 2 MiB
/// default — long story short: every extractor (`Json`, `Bytes`,
/// `String`, `Multipart`, `Form`) shares this cap, so anything
/// larger than 2 MiB was previously rejected by axum itself with an
/// opaque error before our handlers could produce a clean JSON
/// response.
pub(crate) const GLOBAL_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Shared state for HTTP handlers
pub type SharedHttpState = Arc<RwLock<GatewayState>>;

/// Application state available to all HTTP handlers
#[derive(Clone)]
pub struct AppState {
    /// Shared gateway state
    pub gateway_state: SharedHttpState,
    /// HTTP authentication
    pub auth: Arc<HttpAuth>,
    /// Tracing reload handle for dynamic log level changes
    pub log_reload_handle: Option<crate::LogReloadHandle>,
    /// ADR-033: MQTT Gateway client for publishing control commands to Runtime.
    pub mqtt_gateway_client: Option<Arc<crate::mqtt::GatewayMqttClient>>,
    /// ADR-033: MQTT global resources publisher trigger.
    /// HTTP handlers call `.trigger()` after resource changes to republish.
    pub mqtt_publisher_trigger: Option<crate::mqtt::MqttPublisherTrigger>,
    /// Inventory-change signal trigger. HTTP handlers that mutate the
    /// aggregated `installed_agents` table (e.g. `uninstall_agent`) call
    /// `.notify()` so the Gateway republishes `acowork/desktop/inventory`.
    /// Subscribers (Desktop App) refetch `GET /api/agents` on each signal.
    pub inventory_trigger: Option<crate::mqtt::InventoryNotifierTrigger>,
    /// ADR-033: Runtime HTTP port registry for reverse proxy to Runtime localhost HTTP.
    pub runtime_http_registry: Option<crate::http::proxy::SharedRuntimeHttpRegistry>,
    /// ADR-033: Agent registry tracking online/offline status from MQTT.
    pub agent_registry: Option<crate::mqtt::agent_registry::SharedAgentRegistry>,
    /// ADR-055: Node control-plane client (issues agent lifecycle
    /// commands to Node Agents and correlates the NodeEvent replies).
    pub node_control: Option<crate::mqtt::node_control::NodeControlClient>,
    /// ADR-055: Node registry (LWT-driven online state + retained info
    /// snapshots). Used by handlers that route to node-local HTTP
    /// services (e.g. `/api/fs/browse?target={node_id}`, L7-1).
    pub node_registry: Option<crate::mqtt::SharedNodeRegistry>,
    /// ADR-059 §7.2: subsystem readiness registry. Mutation handlers
    /// that depend on a Node's control plane (e.g. `/api/agents/install`)
    /// check `is_ready("node.{node_id}")` before dispatching.
    pub bootstrap_registry: Option<crate::bootstrap::SharedSubsystemReadinessRegistry>,
    /// ADR-059 §6.2: operation store — tracks every accepted mutation
    /// (install / provider write / identity write) from `Accepted` to
    /// a terminal state, keyed by `operation_id`.
    pub operation_store: Option<crate::operation_store::SharedOperationStore>,
    /// Peer-IP allowlist (security backstop, `[security].allowed_node_ips`).
    /// Populated at boot from the Gateway config; empty = allow everyone.
    /// Enforced by the `ip_allowlist_middleware` for requests that carry a
    /// real `ConnectInfo` (i.e. every request that arrived over TCP).
    pub ip_allowlist: crate::security::IpAllowlist,
    /// ADR-076 §决策 12: the resolved deployment auth mode. `Local` =
    /// legacy bearer token, no account system.
    pub auth_mode: AuthMode,
    /// Design doc 24 §6.3: relay tunnel client — runtime enable/disable
    /// and status for the outbound WSS tunnel to an acowork-relay.
    pub relay_client: Option<std::sync::Arc<crate::relay::RelayClient>>,
    /// ADR-087: node ownership table (`{data_dir}/node_owners.json`).
    /// Shared with the MQTT dispatch (enroll writes rows).
    pub node_owners: Option<crate::gateway::ownership::SharedOwnershipStore>,
    /// ADR-087: agent instance ownership table (`{data_dir}/agent_owners.json`).
    pub agent_owners: Option<crate::gateway::ownership::SharedOwnershipStore>,
    /// ADR-087 D7 / review M4: installs dispatched but not yet confirmed
    /// by the node's retained inventory. The owner row is committed when
    /// the instance first appears, dropped when the Node reports failure.
    pub pending_installs: Option<crate::gateway::ownership::SharedPendingInstalls>,
    /// ADR-087 D2: enrollment token store — `POST /api/nodes/enrollment-tokens`
    /// mints tokens bound to the caller. Shared with the MQTT dispatch
    /// (enroll consumes them).
    pub enrollment_tokens: Option<crate::mqtt::SharedEnrollmentTokenStore>,
}

impl AppState {
    /// Create a new AppState with default models cache
    pub fn new(gateway_state: SharedHttpState, auth: Arc<HttpAuth>) -> Self {
        Self {
            gateway_state,
            auth,
            log_reload_handle: None,
            mqtt_gateway_client: None,
            mqtt_publisher_trigger: None,
            inventory_trigger: None,
            runtime_http_registry: None,
            agent_registry: None,
            node_control: None,
            node_registry: None,
            bootstrap_registry: None,
            operation_store: None,
            ip_allowlist: crate::security::IpAllowlist::default(),
            auth_mode: AuthMode::Local,
            relay_client: None,
            node_owners: None,
            agent_owners: None,
            pending_installs: None,
            enrollment_tokens: None,
        }
    }
}

/// Replace the value of an `access_token` query parameter with `***`
/// before a URI is logged (ADR-089 D2 WS auth).
fn redact_access_token(uri: axum::http::Uri) -> axum::http::Uri {
    let Some(q) = uri.query() else {
        return uri;
    };
    if !q.contains("access_token") {
        return uri;
    }
    let redacted: String = q
        .split('&')
        .map(|pair| {
            if pair.starts_with("access_token=") || pair == "access_token" {
                "access_token=***"
            } else {
                pair
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    // Replace only the query part; path + scheme are untouched.
    let s = uri.to_string();
    let prefix = &s[..s.len() - q.len()];
    format!("{prefix}{redacted}")
        .parse()
        // A redacted copy of a valid URI is always valid; keep the
        // original rather than panic on a pathological input.
        .unwrap_or(uri)
}

/// Log request Origin and response CORS header for every HTTP request.
/// This middleware runs AFTER the CORS layer, so `Access-Control-Allow-Origin`
/// reflects what the browser actually sees.
async fn log_request_origin(req: Request, next: Next) -> axum::response::Response {
    let origin = req
        .headers()
        .get("origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let method = req.method().clone();
    // ADR-089 D2: a WS upgrade may carry `?access_token=` (browser
    // WebSockets cannot set headers) — never write it into the log.
    let uri = redact_access_token(req.uri().clone());
    let msg = format!(
        "HTTP request: origin={} method={} uri={}",
        origin, method, uri
    );
    tracing::info!("{}", msg);
    let response = next.run(req).await;
    let acao = response
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("<none>");
    let msg = format!(
        "HTTP response: status={} access-control-allow-origin={}",
        response.status(),
        acao
    );
    tracing::info!("{}", msg);
    response
}

/// Peer-IP allowlist enforcement (security backstop).
///
/// Reads `AppState::ip_allowlist` (populated at boot from
/// `[security].allowed_node_ips`). Empty list → pass-through (default).
///
/// The peer IP comes from Axum's `ConnectInfo<SocketAddr>` extension,
/// which is only attached when the router is served via
/// `into_make_service_with_connect_info::<SocketAddr>()` (see
/// `crate::http::server::start_http_server`). Requests that arrive
/// **without** ConnectInfo (in-process tests, internal callers that
/// invoke the router directly) are trusted and pass through — they are
/// already on a trusted code path; the allowlist exists to gate
/// *network* peers, and every network peer goes through the TCP
/// listener that supplies ConnectInfo.
///
/// A blocked peer receives `403 Forbidden` with a JSON body and a WARN
/// log line (IP + URI), so an operator can detect scan / probe traffic.
pub(crate) async fn ip_allowlist_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let allowlist = &state.ip_allowlist;
    if allowlist.is_empty() {
        return next.run(req).await;
    }
    // Peek at the request for the log line without consuming it.
    let uri = req.uri().to_string();
    match req
        .extensions()
        .get::<axum::extract::connect_info::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0)
    {
        // Real network connection: enforce the allowlist.
        Some(peer) if allowlist.allows_socket(peer) => next.run(req).await,
        Some(peer) => {
            tracing::warn!(
                peer = %peer.ip(),
                uri = %uri,
                "blocked by security.allowed_node_ips (HTTP)"
            );
            let body = axum::Json(serde_json::json!({
                "error": "forbidden",
                "detail": "peer IP not allowed by gateway security policy",
            }));
            (StatusCode::FORBIDDEN, body).into_response()
        }
        // No ConnectInfo — trusted in-process path.
        None => next.run(req).await,
    }
}

/// Build the HTTP router with all routes.
///
/// ADR-064: PM 不再内嵌（`nest_service` 已删除），`/api/pm/*` 由
/// [`crate::http::pm_proxy::pm_proxy_routes`] 反向代理到独立进程。
pub fn build_router(state: AppState) -> Router {
    // Peer-IP allowlist middleware needs the state again after
    // `.with_state(state)` moves it — clone up front.
    let allowlist_state = state.clone();
    // Same reason for the auth middleware (ADR-076 §决策 3).
    let auth_layer_state = state.clone();

    // CORS — permissive for all deployments.
    //
    // `CorsLayer::permissive()` alone — deliberately WITHOUT
    // `allow_credentials(true)`: permissive() answers `*` for origin /
    // method / header, and the CORS spec forbids combining `*` with
    // `Access-Control-Allow-Credentials: true`. tower-http asserts at
    // layer-build time and panics on that combination (this exact bug
    // crashed the HTTP server once — see ensure_usable_cors_rules).
    //
    // Credentials are not needed: the Gateway never sends `Set-Cookie`
    // and the desktop frontend fetches with the default
    // `credentials: 'same-origin'`, so no cross-origin credentials are
    // ever transmitted. The Gateway binds to loopback by default
    // (`[http].host = 127.0.0.1`), so any origin that can reach it is
    // already on the user's machine — 0 risk locally. For remote
    // deployments (`[http].host = 0.0.0.0` or a LAN address), CSRF is
    // prevented by the bearer-token middleware, not by CORS.
    //
    // Dev mode (Vite on :5173, Tauri custom protocol on macOS/Windows) is
    // always cross-origin against the Gateway (:19876); a hardcoded
    // allowlist breaks the moment the WebView resolves `localhost` to a
    // different IP literal than the one hardcoded.
    let cors = tower_http::cors::CorsLayer::permissive();

    Router::new()
        .route("/health", get(health_check))
        .route("/api/status", get(system_status))
        // ADR-059 Phase 1.3: readiness projection (liveness lives at
        // `/health`). `GET /api/bootstrap` exposes the same aggregated
        // snapshot as the retained MQTT topic `acowork/global/bootstrap`.
        .merge(crate::http::bootstrap_api::bootstrap_routes())
        // ADR-059 Phase 5.4: vault lock/unlock — the relock path must
        // demote the vault subsystem (phase drops below READY) and the
        // unlock path must restore it, reusing the cold-start unlock
        // sequence (see vault_api.rs).
        .merge(crate::http::vault_api::vault_routes())
        .merge(crate::http::agents::agent_routes())
        .merge(crate::http::provider_api::provider_routes())
        .merge(crate::http::config_api::config_routes())
        .merge(crate::http::cron_api::cron_routes())
        // Intent delivery entry point (tech-debt plan D1/D3/D4/D8) —
        // the Gateway's only Intent ingress; Runtimes POST here instead
        // of publishing to the control topic.
        .merge(crate::http::intent_api::intent_routes())
        .merge(crate::http::models_api::models_routes())
        .merge(crate::http::memory_api::memory_routes())
        .merge(crate::http::skills_api::skills_routes())
        .merge(crate::http::workspaces::workspace_routes())
        .merge(crate::http::publish_api::publish_routes())
        .merge(crate::http::mcp_catalog_api::mcp_catalog_routes())
        .merge(crate::http::nodes_api::nodes_routes())
        // ADR-089 D2: LSP relay reverse proxy (HTTP + WebSocket).
        .merge(crate::http::lsp_proxy::lsp_proxy_routes())
        // ADR-091 §5.7: LSP *registry* admin proxy (`can_manage` on writes,
        // and deliberately not gated on the relay being up).
        .merge(crate::http::lsp_admin_proxy::lsp_admin_routes())
        // Plan §3.3 P2-2: Gateway-perspective service diagnostics
        // (`GET /api/services/diagnose`) — one-fetch aggregate of
        // gateway / mqtt / embed / pm / doc / nodes for the Desktop
        // diagnostic panel. Remote Desktops reach this over the same
        // HTTP surface as everything else (P2-1).
        .merge(crate::http::services_api::services_routes())
        // ADR-084: the whole user domain (accounts, profiles, avatars,
        // user-to-user chat) now lives in the `acowork-user` process. The
        // Gateway prose nothing here — `user_proxy` forwards the public
        // paths verbatim, so the Desktop sees the same URLs it always did.
        // Not-ready → 503 + `Retry-After`, same contract as `/api/pm`/`/api/doc`.
        .merge(crate::http::user_proxy::user_proxy_routes())
        .merge(crate::http::embedding_api::embedding_routes())
        .merge(crate::embedding_providers::embedding_providers_routes())
        .merge(crate::http::fs_browse::fs_routes())
        .merge(crate::http::global_resources_api::global_resources_routes())
        .merge(crate::http::proxy::proxy_routes())
        // ADR-064: PM 独立进程反向代理（`/api/pm/*` → `127.0.0.1:{pm_port}/*`）。
        // PM 未就绪时返回 503（带 Retry-After），Desktop `with503Retry` 退避重试。
        .merge(crate::http::pm_proxy::pm_proxy_routes())
        // acowork-doc 独立进程反向代理（`/api/doc/*` → `127.0.0.1:{doc_port}/*`）。
        // 未就绪时返回 503（带 Retry-After），与 PM 同一契约。
        .merge(crate::http::doc_proxy::doc_proxy_routes())
        .merge(crate::http::debug_mqtt::debug_mqtt_routes())
        .merge(crate::http::settings_api::settings_routes())
        // Design doc 24 §6.3: relay tunnel management.
        .merge(crate::http::relay_api::relay_routes())
        .with_state(state)
        // Global body-size cap. See `GLOBAL_BODY_LIMIT` for why we
        // override axum's 2 MiB default at the root of the gateway
        // router. Per-route service-layer limits (e.g. Runtime's
        // `MAX_UPLOAD_BYTES` for attachments, gateway's
        // `MAX_FILE_SIZE` for static file reads) remain the source of
        // truth for user-facing caps — this layer only ensures those
        // limits produce a clean error body instead of an opaque
        // extractor parse failure.
        .layer(DefaultBodyLimit::max(GLOBAL_BODY_LIMIT))
        // ADR-087: authorization policy layer. axum runs layers
        // bottom-to-top, so adding it *before* `auth_middleware` makes it
        // the inner layer — it sees the `AuthContext` the auth gate
        // injects, and every `/api/agents/{id}/**` route is checked
        // against the ownership tables (default-deny for unregistered
        // routes). No-op under `AUTH_MODE=local` (ADR-087 D8).
        .layer(middleware::from_fn_with_state(
            auth_layer_state.clone(),
            crate::http::permission::permission_middleware,
        ))
        // ADR-076 §决策 3: bearer-token gate. Placed inside CORS (so a
        // 401 still carries `Access-Control-Allow-Origin`) and outside
        // every route (so no handler can be added without passing it).
        .layer(middleware::from_fn_with_state(
            auth_layer_state.clone(),
            crate::http::auth_middleware::auth_middleware,
        ))
        // ADR-076 §决策 12 v2: first-boot restricted-mode gate. axum runs
        // layers bottom-to-top, so adding it *after* `auth_middleware`
        // makes it the outer one — a restricted-mode request is answered
        // 403 `setup_required` before the token check can turn it into a
        // 401 `missing bearer token`. The two errors must not be
        // confusable, and 403 is the more informative answer here.
        .layer(middleware::from_fn_with_state(
            auth_layer_state.clone(),
            crate::http::restricted_mode::restricted_mode_middleware,
        ))
        .layer(middleware::from_fn(log_request_origin))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .layer(cors)
        // Peer-IP allowlist — outermost layer so it gates *every*
        // route (including /health and unknown paths). Empty list =
        // pass-through. See `ip_allowlist_middleware`.
        .layer(middleware::from_fn_with_state(
            allowlist_state,
            ip_allowlist_middleware,
        ))
}

// ── Health check (liveness-only) ─────────────────────────────────────

/// `GET /health` — liveness probe (no auth required).
///
/// ADR-059 Phase 1.3: this endpoint is STRICTLY liveness — it answers
/// as soon as the HTTP server is up and says nothing about subsystem
/// readiness (no checks, no ready flags). Consumers that need to know
/// whether the Gateway is ready to accept dependent work must use
/// `GET /api/bootstrap` and wait for `phase = READY`.
#[derive(Debug, Serialize)]
pub struct LivenessResponse {
    pub status: String,
    pub version: String,
    /// HTTP port this server listens on — the process identity half of
    /// the liveness contract.
    pub port: u16,
}

/// `GET /health` — liveness check (no auth required)
pub async fn health_check(State(state): State<AppState>) -> Json<LivenessResponse> {
    let gw = state.gateway_state.read().await;
    let port = gw
        .config
        .as_ref()
        .map(|c| c.http.port)
        .unwrap_or_else(crate::config::default_http_port);
    Json(LivenessResponse {
        status: "ok".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        port,
    })
}

// ── System status ─────────────────────────────────────────────────────

/// System status response
#[derive(Serialize)]
pub struct SystemStatusResponse {
    pub version: String,
    pub agents_installed: usize,
    pub agents_running: usize,
    pub uptime_secs: u64,
    /// ADR-055 D3 §6.3: MQTT broker port for Desktop discovery.
    /// Lets the Desktop derive the broker port dynamically instead of
    /// assuming the default 19875 (L3-6 residual gap — ADR-058 W4 fixed
    /// the host derivation; this closes the port half).
    pub mqtt_port: u16,
    /// ADR-055 Phase 5a: MQTT username for the Desktop's broker
    /// connection, present only when `mqtt.auth_enabled` is on.
    /// Informational at this tier — CONNECT identity is keyed by
    /// client_id (`user:{name}:desktop:{id}`), not username.
    pub mqtt_username: Option<String>,
    /// ADR-055 Phase 5a: MQTT password for the Desktop's broker
    /// connection — the HttpAuth bearer token, the same value the
    /// broker CONNECT check accepts for `user:*:desktop:*` clients.
    /// Present only when `mqtt.auth_enabled` is on.
    pub mqtt_password: Option<String>,
    /// ADR-076 §决策 12: the resolved deployment auth mode
    /// (`"local"` | `"multi_user"`). The Desktop reads this at boot to
    /// decide whether to gate the UI behind a login (`multi_user`) or
    /// skip the account system entirely (`local`). `/api/status` is on
    /// the auth middleware's public whitelist, so this is reachable
    /// before any login.
    pub auth_mode: &'static str,
    /// ADR-076 §决策 6: whether a non-admin may create accounts
    /// (`[multi_user].registration_open`). Always `false` unless the account
    /// system is actually running — the Desktop offers the invite
    /// affordance to non-admins only when this is set.
    pub registration_open: bool,
    /// ADR-076 §决策 12 v2: true iff the Gateway is in first-boot
    /// restricted mode (a passwordless `admin` account exists). The
    /// Desktop reads this to decide whether to render the
    /// "Gateway is not ready" gate instead of the login screen. Always
    /// `false` in `local` mode or after setup is finished.
    #[serde(default)]
    pub requires_setup: bool,
}

/// `GET /api/status` — system status
pub async fn system_status(State(state): State<AppState>) -> Json<SystemStatusResponse> {
    let gw = state.gateway_state.read().await;

    // ADR-033: Prefer AgentRegistry (MQTT LWT-based) for agent online count.
    // It reflects the broker's protocol-level view via Will Message, which is
    // more accurate than the Gateway's process-level running_agents count.
    let agents_running = if let Some(ref reg) = state.agent_registry {
        reg.read().await.online_count()
    } else {
        gw.running_agents.len()
    };

    // ADR-055 Phase 5a: MQTT credentials for the Desktop
    // (`connect_mqtt` consumes these when present). Exposed only when
    // MQTT auth is on; the password is the HttpAuth bearer token — the
    // same value the broker CONNECT handler accepts for `user:*:desktop:*`.
    let mqtt_auth_enabled = gw
        .mqtt_broker_auth
        .as_ref()
        .map(|a| a.auth_enabled)
        .unwrap_or(false);
    let (mqtt_username, mqtt_password) = if mqtt_auth_enabled {
        (
            Some("desktop".to_string()),
            state.auth.token().map(str::to_string),
        )
    } else {
        (None, None)
    };

    Json(SystemStatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        agents_installed: gw.installed_agents.len(),
        agents_running,
        uptime_secs: (chrono::Utc::now() - gw.started_at).num_seconds().max(0) as u64,
        mqtt_port: gw
            .config
            .as_ref()
            .map(|c| c.mqtt.port)
            .unwrap_or_else(crate::config::default_mqtt_port),
        mqtt_username,
        mqtt_password,
        auth_mode: state.auth_mode.as_str(),
        // ADR-084 §决策 4b: both flags come from the user service's
        // `/health` snapshot, so `/api/status` no longer needs the account
        // store (and no longer disagrees with the service's own view).
        registration_open: gw
            .user_snapshot
            .as_ref()
            .is_some_and(|s| s.registration_open),
        requires_setup: gw.user_snapshot.as_ref().is_some_and(|s| s.requires_setup),
    })
}

// ── Error response helpers ────────────────────────────────────────────

/// ADR-059 §7.3/§7.4 — unified mutation ack returned by every
/// operation-bearing write API (`POST /api/providers`, `POST
/// /api/users`, `POST /api/agents/install`, `POST /api/mcp-catalog`).
///
/// The ack only carries protocol-level fields (OCP §5.4.4): the
/// operation id, its current state, the post-commit resource version,
/// and the structured terminal error when the operation failed. No
/// capability names / subsystem generations / process ids.
#[derive(Debug, Clone, Serialize)]
pub struct OperationAck {
    pub operation_id: OperationId,
    pub state: OperationState,
    pub resource_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_error: Option<StructuredErrorBody>,
    /// ADR-073: the instance identity created by this operation
    /// (install / clone only). Lets the Desktop render the new
    /// instance immediately instead of polling the retained
    /// inventory. `None` for operations that do not create an
    /// instance (start/stop/upgrade/…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
}

impl OperationAck {
    /// Build an ack from a tracked record (state + resource version
    /// + terminal error mirrored from the record).
    pub fn from_record(record: &OperationRecord) -> Self {
        Self {
            operation_id: record.operation_id.clone(),
            state: record.state,
            resource_version: record.resource_version,
            terminal_error: record.terminal_error.clone(),
            instance_id: None,
        }
    }
}

/// ADR-059 §7.3 — validate the mutation's `expected_version` against
/// the Gateway's CURRENT bootstrap snapshot version.
///
/// The client reads `GET /api/bootstrap` before writing and echoes the
/// snapshot's `version` back; a mismatch means the client's view is
/// stale (typically a Gateway restart — the new instance's version
/// counter restarts at 1, so any old-instance `expected_version` is
/// rejected instead of being accepted against a fresh process).
///
/// Returns `Ok(())` on match, or a structured `409
/// resource_version_conflict` carrying both version numbers.
pub async fn check_expected_version(
    state: &AppState,
    expected_version: Option<u64>,
) -> Result<(), ApiError> {
    let Some(expected) = expected_version else {
        // No precondition — the mutation proceeds optimistically.
        return Ok(());
    };
    let current = state
        .gateway_state
        .read()
        .await
        .bootstrap
        .orchestrator
        .as_ref()
        .map(|o| o.snapshot().version)
        .unwrap_or(0);
    if current != expected {
        return Err(ApiError::conflict_structured(
            StructuredErrorBody::resource_version_conflict(current, expected),
        ));
    }
    Ok(())
}

/// Standard API error response
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiError {
    pub error: String,
    pub code: u16,
    /// ADR-059 §6.3: structured protocol error body for mutation
    /// APIs. Absent for plain HTTP-layer errors (existing clients are
    /// unaffected); present for `dependency_not_ready` /
    /// `resource_version_conflict` etc. so clients can retry without
    /// parsing human-readable text.
    ///
    /// Boxed so the `Err` variant of every handler signature stays small:
    /// `StructuredErrorBody` carries ~10 `Option<...>` fields and is
    /// ~232 bytes inline. Boxing shrinks `ApiError` from ~248 B to ~40 B,
    /// which keeps `cargo clippy -D warnings` (`result_large_err`) quiet
    /// on every `Result<Json<X>, ApiError>` handler signature. Wire
    /// format is identical — `Box<T>: Serialize` derefs to `T` for
    /// `serde_json`, and `#[serde(skip_serializing_if = "Option::is_none")]`
    /// still hides absent values.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<Box<StructuredErrorBody>>,
}

/// Handler errors return [`ApiError`] directly (no `(StatusCode,
/// Json<ApiError>)` tuple wrapper). [`ApiError::code`] carries the
/// intended HTTP status as a u16, which this impl decodes back into
/// an `http::StatusCode` for axum's response pipeline.
///
/// Keeping the `Err` variant small (≈ 80 bytes: String + u16 +
/// Option<StructuredErrorBody>) instead of a ~256-byte tuple
/// `(StatusCode, Json<ApiError>)` satisfies `cargo clippy -D warnings`
/// (`result_large_err`) without forcing every handler to thread a
/// `Box<...>` through `?`.
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // `code` is the canonical u16 representation of the intended
        // status. Defensively fall back to 500 if a future helper ever
        // forgets to populate it, so the client still gets a valid
        // HTTP response rather than axum panicking on a status-code
        // conversion.
        let status = StatusCode::from_u16(self.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        // ADR-087 D5.3: every permission denial leaves the Gateway with the
        // same machine-readable shape as `acl::render` /
        // `permission::forbidden` — `code: "not_authorized"`, never the
        // numeric status — so the Desktop maps one code, not one per
        // emitter. The human-readable detail moves to `message`.
        if status == StatusCode::FORBIDDEN {
            return (
                status,
                Json(serde_json::json!({
                    "error": "forbidden",
                    "code": "not_authorized",
                    "message": self.error,
                })),
            )
                .into_response();
        }
        (status, Json(self)).into_response()
    }
}

impl ApiError {
    pub fn not_found(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 404,
            structured: None,
        }
    }

    pub fn bad_request(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 400,
            structured: None,
        }
    }

    /// ADR-056: unprocessable entity — the request is well-formed but the
    /// referenced entity does not exist (e.g. `default_compact_model`
    /// pointing at an unknown provider_id / model_id). Mirrors the 422
    /// contract in ADR-056 §4.1.
    pub fn unprocessable_entity(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 422,
            structured: None,
        }
    }

    pub fn internal(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 500,
            structured: None,
        }
    }

    pub fn unauthorized(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 401,
            structured: None,
        }
    }

    /// 413 — the body is too large for the endpoint's own ceiling (e.g. an
    /// attachment over ADR-076 §决策 9's image/document limits).
    pub fn payload_too_large(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 413,
            structured: None,
        }
    }

    /// 403 — authenticated but not permitted (e.g. a non-admin calling an
    /// admin-only account-management route).
    pub fn forbidden(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 403,
            structured: None,
        }
    }

    pub fn service_unavailable(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 503,
            structured: None,
        }
    }

    /// 504 Gateway Timeout — the request depended on a downstream
    /// round-trip (e.g. a Node control-plane command) that exceeded its
    /// deadline. Distinct from 500 so clients can surface "the node did
    /// not answer in time" without treating it as a server crash.
    pub fn gateway_timeout(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 504,
            structured: None,
        }
    }

    /// ADR-059 §2.3: conflict — the request depends on a resource that
    /// is not ready yet (e.g. installing onto a Node whose control
    /// plane has not announced `NodeReady`). The client should retry
    /// once `GET /api/bootstrap` reports `phase = READY`.
    pub fn conflict(msg: &str) -> Self {
        Self {
            error: msg.to_string(),
            code: 409,
            structured: None,
        }
    }

    /// ADR-059 §6.3: structured conflict — HTTP 409 whose body carries
    /// a protocol-level [`StructuredErrorBody`] (e.g.
    /// `resource_version_conflict` with both version numbers).
    pub fn conflict_structured(body: StructuredErrorBody) -> Self {
        Self {
            error: format!("{:?}", body.code),
            code: 409,
            structured: Some(Box::new(body)),
        }
    }

    /// Generic structured error constructor (2026-09-07 incident
    /// follow-up). The closed `StructuredErrorCode` set in
    /// `acowork-core::error_codes` covers the whole API surface, but
    /// some endpoints (e.g. `start_agent`) want to attach a code to a
    /// status code that doesn't have a dedicated helper (`503`,
    /// `422`, …). This constructor lets the handler pick both
    /// independently.
    ///
    /// The convention is:
    ///   - `code` = intended HTTP status (`u16`).
    ///   - `error` = short human-readable line, **safe for end users**
    ///     (do not leak internal paths or PII).
    ///   - `structured` = machine-readable protocol body. Callers pick
    ///     a `StructuredErrorCode` that classifies the failure for
    ///     client-side retry / rendering decisions.
    pub fn structured(status: StatusCode, message: &str, body: StructuredErrorBody) -> Self {
        Self {
            error: message.to_string(),
            code: status.as_u16(),
            structured: Some(Box::new(body)),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn redact_access_token_only_touches_that_param() {
        let uri: axum::http::Uri = "/api/nodes/n-1/lsp/lsp/rust?workspace_root=%2Ftmp&access_token=SECRET"
            .parse()
            .unwrap();
        let red = super::redact_access_token(uri).to_string();
        assert_eq!(
            red,
            "/api/nodes/n-1/lsp/lsp/rust?workspace_root=%2Ftmp&access_token=***"
        );
        // No token -> untouched; other params keep their order/value.
        let plain: axum::http::Uri = "/api/nodes?x=1".parse().unwrap();
        assert_eq!(super::redact_access_token(plain).to_string(), "/api/nodes?x=1");
    }

    use super::*;
    use axum::body::Body;
    use tower::ServiceExt;

    fn test_app_state() -> AppState {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-http-routes-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
        )
    }

    #[tokio::test]
    async fn test_health_check() {
        let state = test_app_state();
        let resp = health_check(State(state)).await;
        // Liveness-only contract (ADR-059 Phase 1.3): always "ok", no
        // checks map, no readiness claims.
        assert_eq!(resp.status, "ok");
        assert!(!resp.version.is_empty());
        assert!(resp.port > 0);
    }

    /// Peer-IP allowlist middleware: empty list = pass-through.
    #[tokio::test]
    async fn allowlist_empty_passes_everything() {
        let mut state = test_app_state();
        state.ip_allowlist = crate::security::IpAllowlist::default(); // empty
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Peer-IP allowlist middleware: allowed peer passes.
    #[tokio::test]
    async fn allowlist_allows_permitted_peer() {
        let mut state = test_app_state();
        let entry = crate::security::AllowlistEntry::parse("192.168.1.20").unwrap();
        state.ip_allowlist = crate::security::IpAllowlist::new(vec![entry]);
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        let mut request = request;
        let peer: std::net::SocketAddr = "192.168.1.20:54321".parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::connect_info::ConnectInfo(peer));
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Peer-IP allowlist middleware: blocked peer gets 403 — even on /health.
    #[tokio::test]
    async fn allowlist_blocks_disallowed_peer_with_403() {
        let mut state = test_app_state();
        let entry = crate::security::AllowlistEntry::parse("192.168.1.20").unwrap();
        state.ip_allowlist = crate::security::IpAllowlist::new(vec![entry]);
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        let mut request = request;
        let peer: std::net::SocketAddr = "203.0.113.9:54321".parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::connect_info::ConnectInfo(peer));
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    /// Peer-IP allowlist middleware: loopback is always allowed even when
    /// the list is restrictive.
    #[tokio::test]
    async fn allowlist_always_allows_loopback() {
        let mut state = test_app_state();
        let entry = crate::security::AllowlistEntry::parse("192.168.1.20").unwrap();
        state.ip_allowlist = crate::security::IpAllowlist::new(vec![entry]);
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        let mut request = request;
        let peer: std::net::SocketAddr = "127.0.0.1:54321".parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::connect_info::ConnectInfo(peer));
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Peer-IP allowlist middleware: in-process requests without
    /// ConnectInfo (internal callers, direct router tests) pass through —
    /// the allowlist gates *network* peers only.
    #[tokio::test]
    async fn allowlist_passes_request_without_connect_info() {
        let mut state = test_app_state();
        let entry = crate::security::AllowlistEntry::parse("192.168.1.20").unwrap();
        state.ip_allowlist = crate::security::IpAllowlist::new(vec![entry]);
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        // NOTE: no ConnectInfo extension inserted.
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_system_status() {
        let state = test_app_state();
        let resp = system_status(State(state)).await;
        assert_eq!(resp.agents_installed, 0);
        assert_eq!(resp.agents_running, 0);
        // MQTT auth off by default → no credentials are exposed.
        assert_eq!(resp.mqtt_username, None);
        assert_eq!(resp.mqtt_password, None);
        // ADR-076 §决策 12: the resolved mode is exposed for the Desktop's
        // login gate; the default (loopback) deployment is `local`.
        assert_eq!(resp.auth_mode, "local");
        // No account system → registration can never be open.
        assert!(!resp.registration_open);
    }

    #[tokio::test]
    async fn test_system_status_exposes_mqtt_credentials_when_auth_enabled() {
        use std::sync::Mutex;

        // MQTT auth enabled + HttpAuth enabled (the broker CONNECT
        // check for `user:*:desktop:*` compares against the HttpAuth
        // bearer token, so the Desktop gets it as its MQTT password).
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-http-status-auth-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        {
            let mut gw = gw_state.write().await;
            gw.mqtt_broker_auth = Some(crate::mqtt::broker::BrokerAuth {
                auth_enabled: true,
                enrollment_tokens: Arc::new(Mutex::new(Default::default())),
                node_tokens: Arc::new(Mutex::new(Default::default())),
                publisher_token: "publisher-token".to_string(),
                http_token: None,
            });
        }
        let state = AppState::new(gw_state, Arc::new(HttpAuth::new(true)));
        let resp = system_status(State(state)).await;
        assert_eq!(resp.mqtt_username.as_deref(), Some("desktop"));
        let password = resp
            .mqtt_password
            .as_deref()
            .expect("password exposed when auth on");
        assert_eq!(password.len(), 64, "256-bit hex token");

        // MQTT auth enabled but HttpAuth off → no password to hand out.
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        {
            let mut gw = gw_state.write().await;
            gw.mqtt_broker_auth = Some(crate::mqtt::broker::BrokerAuth {
                auth_enabled: true,
                enrollment_tokens: Arc::new(Mutex::new(Default::default())),
                node_tokens: Arc::new(Mutex::new(Default::default())),
                publisher_token: "publisher-token".to_string(),
                http_token: None,
            });
        }
        let state = AppState::new(gw_state, Arc::new(HttpAuth::new(false)));
        let resp = system_status(State(state)).await;
        assert_eq!(resp.mqtt_username.as_deref(), Some("desktop"));
        assert_eq!(resp.mqtt_password, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_router() {
        let state = test_app_state();
        let _router = build_router(state);
    }

    /// ADR-059 §6: the mutation ack carries exactly the protocol
    /// fields (`operation_id` / `state` / `resource_version` /
    /// `terminal_error`) — never internal capability names, subsystem
    /// generations or process ids (OCP boundary, §5.4.4).
    #[test]
    fn operation_ack_carries_only_protocol_fields() {
        let mut record = OperationRecord::new(3);
        record.state = OperationState::Committed;
        record.resource_version = Some(8);

        let json = serde_json::to_value(OperationAck::from_record(&record)).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 3, "terminal_error omitted when None: {obj:?}");
        assert_eq!(obj["state"], "committed");
        assert_eq!(obj["resource_version"], 8);
        assert_eq!(obj["operation_id"], record.operation_id.as_str());

        // A failed ack carries the structured terminal error instead.
        let mut failed = OperationRecord::new(3);
        failed.state = OperationState::Failed;
        failed.terminal_error = Some(StructuredErrorBody::dependency_not_ready(
            Some("BOOTING".to_string()),
            None,
            500,
        ));
        let json = serde_json::to_value(OperationAck::from_record(&failed)).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["state"], "failed");
        assert_eq!(obj["terminal_error"]["code"], "dependency_not_ready");
        assert_eq!(obj["terminal_error"]["current_phase"], "BOOTING");
        // OCP: no internal fields leak into the ack's terminal_error.
        assert!(obj["terminal_error"].get("operation_id").is_none());
        assert!(obj["terminal_error"].get("current_version").is_none());
    }

    #[test]
    fn test_build_router_with_operation_store() {
        let mut state = test_app_state();
        state.operation_store = Some(crate::operation_store::OperationStore::new_shared());
        let _router = build_router(state);
    }

    // ── 2026-09-07 incident follow-up: structured error layer ─────────

    /// `ApiError::structured` is the generic constructor for endpoints
    /// that need an HTTP status (`503` / `422` / `504`) without a
    /// dedicated helper. Pin the wire shape so a future refactor of
    /// the JSON encoding cannot silently lose the structured body —
    /// the Desktop's retry / toast logic depends on `structured.code`.
    #[test]
    fn api_error_structured_carries_status_message_and_body() {
        use acowork_core::error_codes::{RetryHint, StructuredErrorCode};
        use axum::http::StatusCode;
        let body = acowork_core::error_codes::StructuredErrorBody {
            code: StructuredErrorCode::HandshakeTimeout,
            phase_detail: Some("start_node_command_timeout agent=foo".to_string()),
            retry_hint: Some(RetryHint {
                retry_after_ms: Some(1500),
                retry_count: 5,
            }),
            ..Default::default()
        };
        let err = ApiError::structured(StatusCode::GATEWAY_TIMEOUT, "node did not answer", body);
        // HTTP layer: status code lifted to top-level `code`.
        assert_eq!(err.code, 504);
        assert_eq!(err.error, "node did not answer");
        // Protocol layer: structured body preserved untouched.
        let (
            structured_code,
            structured_phase_detail,
            structured_retry_after_ms,
            structured_retry_count,
        ) = {
            let s = err
                .structured
                .as_ref()
                .expect("structured must be Some when constructed via ::structured()");
            (
                s.code,
                s.phase_detail.clone(),
                s.retry_hint.as_ref().and_then(|r| r.retry_after_ms),
                s.retry_hint.as_ref().map(|r| r.retry_count),
            )
        };
        assert_eq!(structured_code, StructuredErrorCode::HandshakeTimeout);
        assert_eq!(
            structured_phase_detail.as_deref(),
            Some("start_node_command_timeout agent=foo")
        );
        assert_eq!(structured_retry_after_ms, Some(1500));
        assert_eq!(structured_retry_count, Some(5));
        // Wire encoding: status + human + nested structured all present.
        let json = serde_json::to_value(&err).unwrap();
        assert_eq!(json["code"], 504);
        assert_eq!(json["error"], "node did not answer");
        assert_eq!(json["structured"]["code"], "handshake_timeout");
        assert_eq!(json["structured"]["retry_hint"]["retry_count"], 5);
    }

    /// `ApiError::structured` is the only path that carries
    /// `structured: Some(_)` for HTTP 503 / 422 / 504 — the legacy
    /// `internal` / `bad_request` / `service_unavailable` helpers
    /// intentionally leave it `None`. Pin this contract so a future
    /// helper consolidation does not accidentally drop the body on
    /// the wire (the Desktop distinguishes 503-with-body from
    /// 503-without-body via this field).
    #[test]
    fn legacy_api_error_helpers_have_no_structured_body() {
        use axum::http::StatusCode;
        // 500 / 400 / 404 / 503 / 504 / 401 / 409 / 422 — none of
        // these carry a structured body (the closed
        // `StructuredErrorCode` set has its own dedicated constructors
        // where a body is meaningful).
        for err in [
            ApiError::internal("boom"),
            ApiError::bad_request("nope"),
            ApiError::not_found("missing"),
            ApiError::unauthorized("denied"),
            ApiError::service_unavailable("down"),
            ApiError::gateway_timeout("late"),
            ApiError::conflict("not ready"),
            ApiError::unprocessable_entity("invalid"),
        ] {
            assert!(
                err.structured.is_none(),
                "legacy helper must NOT auto-fill a structured body; got {:?} for {} {}",
                err.structured,
                err.code,
                err.error
            );
        }
        // And the dedicated structured helper does.
        let structured_err = ApiError::structured(
            StatusCode::SERVICE_UNAVAILABLE,
            "mqtt offline",
            acowork_core::error_codes::StructuredErrorBody::default(),
        );
        assert!(structured_err.structured.is_some());
    }
}
