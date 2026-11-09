//! Node reverse proxy (`:19900`) — ADR-055 §6.4 / §6.17.
//!
//! Routes `/agents/{id}/*` to the machine-local Runtime loopback port
//! (`127.0.0.1:{http_port}/*`) with hop-by-hop header stripping
//! (§6.17), so the Gateway reaches every Runtime on this node through a
//! single port + single auth boundary. Runtime `http_endpoint`
//! registration then points at this proxy instead of the Runtime's
//! loopback address (advertise injection chain §6.3).
//!
//! Two-hop semantics (§6.17):
//! - **Error origin**: a failed upstream fetch returns `502` with
//!   `X-Error-Origin: runtime`; an unknown/not-running agent returns
//!   `503` with `X-Error-Origin: node`. The Gateway adds
//!   `X-Error-Origin: node` when *this* proxy is unreachable, so the
//!   failure layer is always identifiable.
//! - **Error mapping**: Runtime responses pass through verbatim (status
//!   + body); the proxy never fabricates a Runtime business error.
//! - **Streaming**: the proxy is buffered like the Gateway's
//!   `proxy_to_runtime_with_method` (chat streaming goes over MQTT, not
//!   HTTP — ADR-035); SSE/WebSocket streaming lands with the LSP
//!   sidecar in Phase 4.
//! - **Auth** (Phase 5a, §6.8): inbound requests MUST carry the
//!   Gateway-issued `node_token` in `X-ACowork-Node-Token` or they get
//!   `403` with `X-Error-Origin: node`. A node that has **not** enrolled
//!   yet holds no token and therefore refuses everything: the Gateway
//!   learns this endpoint from the enrollment handshake, so
//!   pre-enrollment traffic can only be a stray client on the network.
//!   `proxy_bind` defaults to `0.0.0.0`, which makes fail-closed the
//!   difference between a closed and an open LAN port on `/fs/browse`
//!   and the package write routes.
//!
//! **Node keeps the `{instance_id} → loopback port` mapping private**
//! (§6.4): the port comes from [`crate::state::AgentSlot::http_port`],
//! allocated by the Node at spawn time. ADR-073: the `{id}` in
//! `/agents/{id}/*` is the runtime instance id (UUIDv4), NOT the
//! package id.

use std::time::Duration;

use axum::{
    body::Body,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderName, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
    Router,
};
use reqwest::header::HeaderValue;

use crate::state::NodeHttpState;

mod relay;

/// Build the node reverse-proxy router.
///
/// `state` is the node process table (read-only here) providing the
/// `{instance_id} → http_port` mapping.
pub fn router(state: NodeHttpState) -> Router {
    Router::new()
        // ADR-055 §6.7: node liveness endpoint — the LSP relay's
        // self-exit watchdog (`--gateway-health-url`) probes this, so
        // the relay dies with its parent Node.
        .route("/health", axum::routing::get(health))
        .route("/agents/{id}/{*rest}", any(proxy_agent))
        // ADR-089 D2: Gateway → loopback LSP relay bridge (HTTP + WS).
        .route("/relay/{*rest}", any(relay::proxy_relay))
        .with_state(state)
}

/// Liveness probe for the Node itself (ADR-055 §6.7 — the LSP relay
/// watchdog target).
///
/// Returns 200 `{"status":"ok"}` whenever the node HTTP server is
/// serving AND the shared process-table lock is acquirable within
/// 500 ms; returns 503 `{"status":"stalled"}` when a business task
/// holds the write lock across an await (the 2026-08 reaper deadlock
/// class). Probing the lock is what turns "accept loop alive" into
/// "business logic can run": during that incident `/health` stayed
/// 200 (3f688e04 hindsight) while every `/agents/*` request stalled,
/// so neither the relay watchdog nor the node HTTP watchdog could
/// see the hang. With the lock probe, both self-heal triggers fire
/// on a genuine stall.
async fn health(State(state): State<NodeHttpState>) -> Response {
    // 2 s timeout: covers slow-but-valid state-mutating operations
    // (install/uninstall/clone/upgrade/upgrade_publish in control/mod.rs
    // acquire the write lock for the full sync filesystem round-trip
    // and can exceed 500 ms on large packages or slow disks). Anything
    // truly stalled longer than this returns 503 and triggers the
    // relay / node HTTP watchdogs to self-heal.
    match tokio::time::timeout(Duration::from_millis(2000), state.node.read()).await {
        Ok(_guard) => (
            StatusCode::OK,
            axum::Json(serde_json::json!({ "status": "ok" })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "status": "stalled" })),
        )
            .into_response(),
    }
}

/// Forward `/agents/{id}/{*rest}` to `http://127.0.0.1:{http_port}/{rest}`.
///
/// Method, path suffix, querystring, body and headers are forwarded
/// verbatim (minus hop-by-hop headers). The Runtime is the authority
/// for the resource — the proxy does not second-guess status/body.
async fn proxy_agent(
    State(state): State<NodeHttpState>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: axum::body::Bytes,
) -> Response {
    // ADR-055 Phase 5a §6.8: a Gateway-issued node_token turns this
    // proxy into an auth boundary.
    if let Some(denied) = authorize(&state, &headers, &id).await {
        return denied;
    }

    // Resolve the agent's loopback HTTP port from the node process table.
    let http_port = {
        let node = state.node.read().await;
        node.agents.get(&id).map(|slot| slot.http_port)
    };

    let Some(http_port) = http_port else {
        // ADR-055 §6.17: Node-known error (Runtime not started) → 503,
        // origin = node. Do NOT fabricate a Runtime business error.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(HeaderName::from_static("x-error-origin"), HeaderValue::from_static("node"))],
            axum::Json(serde_json::json!({
                "error": "agent not running on this node",
                "id": id,
            })),
        )
            .into_response();
    };

    let target_url = if let Some(q) = query {
        format!("http://127.0.0.1:{}/{}?{}", http_port, rest, q)
    } else {
        format!("http://127.0.0.1:{}/{}", http_port, rest)
    };

    tracing::debug!(instance_id = %id, http_port, target_url = %target_url, "Node reverse-proxying to Runtime loopback");

    let client = runtime_http_client();
    let mut request = client.request(method.clone(), &target_url);

    // Forward all non-hop-by-hop headers (RFC 7230 §6.1).
    for (name, value) in &headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
    }

    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            // `PUT avatar-config` is the one Runtime write the retained
            // inventory mirrors (`identity` — name + avatar, read back
            // from the instance's `agent_config.json`). Refresh it: the
            // inventory is the only source for a stopped agent, and every
            // human-facing surface reads it.
            if status.is_success() && refreshes_inventory(&method, &rest) {
                crate::package::republish_installed_info(&state, &id).await;
            }
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut builder = Response::builder().status(status);
            *builder.headers_mut().unwrap() = resp_headers;
            builder.body(Body::from(body)).unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "Failed to build proxy response")
                    .into_response()
            })
        }
        Err(e) => {
            tracing::warn!(instance_id = %id, url = %target_url, error = %e, "Failed to proxy to Runtime loopback");
            (
                StatusCode::BAD_GATEWAY,
                [(
                    HeaderName::from_static("x-error-origin"),
                    HeaderValue::from_static("runtime"),
                )],
                axum::Json(serde_json::json!({
                    "error": "failed to connect to Runtime HTTP server",
                    "id": id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Does a successful Runtime write on this proxied request change the
/// instance identity that the retained inventory mirrors?
///
/// The inventory's `identity` (name + avatar) is a projection of the
/// instance's `agent_config.json`, and `PUT /avatar-config` is that file's
/// only writer. Every such write must therefore refresh the retained entry:
/// the inventory is what `GET /api/agents` renders from, so a missed
/// refresh leaves the sidebar and every chat transcript painting the
/// pre-write avatar until the Node restarts.
///
/// ## Match the path's last segment, not the whole suffix
///
/// `rest` is what arrived *after* the node-local `/agents/{id}` route
/// prefix — and that prefix is not necessarily the only one in front of the
/// Runtime path. The Gateway addresses a Runtime through the endpoint the
/// Node advertised (`http://{node}:19900/agents/{id}`) and then appends the
/// Runtime's own `/agents/{id}/avatar-config`, so the suffix reaches this
/// proxy as `agents/{id}/avatar-config`. An equality check against
/// `"avatar-config"` never matched that shape: the write landed in
/// `agent_config.json` (the Runtime's `GET /avatar-config` and the settings
/// panel agreed) while the identity inventory — and therefore the list
/// view — kept the old avatar forever.
///
/// Keeping the comparison on the segment makes the hook independent of how
/// many prefixes a caller composes, which is the caller's business, not
/// this one's. Which paths rewrite identity is still this function's
/// business, and it stays a whitelist of one: a write that does not touch
/// `agent_config.json` must not cost an inventory republish.
fn refreshes_inventory(method: &Method, rest: &str) -> bool {
    *method == Method::PUT && rest.rsplit('/').next() == Some("avatar-config")
}

/// ADR-055 Phase 5a §6.8: validate the inbound `X-ACowork-Node-Token`.
///
/// Every route that exposes node-local state is an auth boundary:
/// inbound requests MUST present the Gateway-issued `node_token`
/// (constant-time compare).
///
/// **Fail closed.** A node without a token has not enrolled, and the
/// Gateway cannot address it (it learns the endpoint from the
/// enrollment handshake), so no legitimate caller exists. Refusing is
/// what keeps a freshly started node from serving `/fs/browse` and the
/// package write routes to anyone who can reach `proxy_bind` (default
/// `0.0.0.0:19900`).
///
/// Returns `None` when the request is authorized, or the `403` response
/// to send back otherwise. Shared by the Runtime reverse proxy, the
/// read-only asset service and the package routes so every hop has one
/// policy, not three.
pub(crate) async fn authorize(
    state: &NodeHttpState,
    headers: &HeaderMap,
    id: &str,
) -> Option<Response> {
    let Some(expected) = state.identity.read().await.node_token.clone() else {
        tracing::warn!(
            instance_id = %id,
            "Node rejected request: this node has not enrolled yet (no token)"
        );
        return Some(forbidden(id, "node has no enrollment token yet"));
    };
    let provided = headers
        .get("X-ACowork-Node-Token")
        .and_then(|v| v.to_str().ok());
    if provided.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes())) {
        return None;
    }
    tracing::warn!(
        instance_id = %id,
        "Node rejected request: missing/invalid X-ACowork-Node-Token"
    );
    Some(forbidden(id, "invalid node token"))
}

/// The `403` every auth failure answers with.
fn forbidden(id: &str, reason: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(
            HeaderName::from_static("x-error-origin"),
            HeaderValue::from_static("node"),
        )],
        axum::Json(serde_json::json!({
            "error": reason,
            "id": id,
        })),
    )
        .into_response()
}

/// Strip hop-by-hop headers (RFC 7230 §6.1) — same list as the
/// Gateway's `proxy.rs` so both hops apply identical semantics.
fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Constant-time byte equality — same discipline as the Gateway's
/// `EnrollmentTokenStore::validate_token`: never leaks prefix/length
/// mismatch timing to a remote attacker probing the header value.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }
    result == 0
}

/// Shared reqwest client with connection pooling (ADR-055 §6.17:
/// keep-alive, no per-request rebuild).
fn runtime_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build node reverse-proxy HTTP client")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::config::NodeConfig;
    use crate::identity::{EnrollmentState, NodeIdentity};
    use crate::state::{NodeState, NodeHttpState};

    fn http_state(node_token: Option<&str>) -> NodeHttpState {
        NodeHttpState {
            node: Arc::new(RwLock::new(NodeState::new(16))),
            config: NodeConfig::default(),
            identity: Arc::new(RwLock::new(NodeIdentity {
                node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
                node_name: "node-1".to_string(),
                gateway_managed: false,
                node_token: node_token.map(str::to_string),
                gateway_addr: None,
                enrollment: EnrollmentState::Enrolled,
                created_at: chrono::Utc::now(),
                enrolled_at: None,
            })),
            // Not under test here: an empty registry and a relay handle on an
            // unused port.
            lsp: crate::lsp_registry::for_tests(std::path::Path::new("."), None),
            lsp_relay: crate::state::LspRelayControl::detached("127.0.0.1", 1),
        }
    }

    fn proxy_request(token: Option<&str>) -> axum::http::Request<Body> {
        let mut builder = axum::http::Request::builder()
            .uri("/agents/com.test.foo/status")
            .method("GET");
        if let Some(t) = token {
            builder = builder.header("X-ACowork-Node-Token", t);
        }
        builder.body(Body::empty()).unwrap()
    }

    /// The identity-write hook must fire for the path shape the Gateway
    /// actually sends, not only for the one it was written against.
    ///
    /// Regression: `rest == "avatar-config"` never matched a request that
    /// came through the Gateway, because the advertised endpoint already
    /// carries `/agents/{id}` and the Runtime path is appended to it
    /// (observed URL:
    /// `http://127.0.0.1:19900/agents/{id}/agents/{id}/avatar-config`).
    /// The republish was dead code, so a newly picked avatar never reached
    /// the inventory and the sidebar/transcript kept the old one.
    #[test]
    fn refreshes_inventory_matches_every_avatar_config_path_shape() {
        const IID: &str = "bd68e6a7-7dd4-4ca7-82a2-e65745c4d198";
        // What the Gateway sends (endpoint prefix + Runtime path).
        let via_gateway = format!("agents/{IID}/avatar-config");
        assert!(refreshes_inventory(&Method::PUT, &via_gateway));
        // What a node-addressed call (`proxy_to_node`) sends.
        assert!(refreshes_inventory(&Method::PUT, "avatar-config"));
    }

    /// The hook stays a whitelist of one: only `agent_config.json`'s writer
    /// costs an inventory republish, and only on a write.
    #[test]
    fn refreshes_inventory_ignores_other_paths_and_reads() {
        const IID: &str = "bd68e6a7-7dd4-4ca7-82a2-e65745c4d198";
        assert!(!refreshes_inventory(&Method::GET, "avatar-config"));
        assert!(!refreshes_inventory(&Method::GET, &format!("agents/{IID}/avatar-config")));
        assert!(!refreshes_inventory(&Method::PUT, "avatar-file"));
        assert!(!refreshes_inventory(&Method::PUT, &format!("agents/{IID}/manifest/avatar")));
        assert!(!refreshes_inventory(&Method::PUT, &format!("agents/{IID}/chat")));
        // A path that merely ends in the word must not qualify.
        assert!(!refreshes_inventory(&Method::PUT, "not-avatar-config"));
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        assert!(is_hop_by_hop_header(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("content-length")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("host")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("content-type")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("authorization")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("x-trace-id")));
    }

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[tokio::test]
    async fn enrolled_proxy_rejects_missing_token() {
        let app = router(http_state(Some("secret-token")));
        let resp = app.oneshot(proxy_request(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            resp.headers().get("x-error-origin").unwrap(),
            HeaderValue::from_static("node")
        );
    }

    #[tokio::test]
    async fn enrolled_proxy_rejects_wrong_token() {
        let app = router(http_state(Some("secret-token")));
        let resp = app
            .oneshot(proxy_request(Some("wrong-token")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn enrolled_proxy_accepts_matching_token() {
        let app = router(http_state(Some("secret-token")));
        // Token passes the gate; the unknown agent then yields 503,
        // proving the auth check let the request through to routing.
        let resp = app
            .oneshot(proxy_request(Some("secret-token")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// ADR-089 D2: `/relay/*` gate + port resolution, end to end.
    ///
    /// No relay is running in the unit-test process, so the two
    /// observable behaviours are: a bad token is refused at the auth
    /// boundary (never reaching the forward), and a good token with no
    /// relay process yields the 503 `relay_not_ready` shape the
    /// Gateway surfaces.
    #[tokio::test]
    async fn relay_route_enforces_token_and_reports_missing_relay() {
        let app = router(http_state(Some("secret-token")));
        let req = axum::http::Request::builder()
            .uri("/relay/api/lsp/servers")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let req = axum::http::Request::builder()
            .uri("/relay/api/lsp/servers")
            .method("GET")
            .header("X-ACowork-Node-Token", "secret-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["code"], "relay_not_ready");
    }

    #[tokio::test]
    async fn unenrolled_proxy_is_fail_closed() {
        // No token exists yet → refuse. Serving this port (default bind
        // `0.0.0.0:19900`) before enrollment would expose the Runtime
        // proxy, `/fs/browse` and the package write routes to the LAN.
        let app = router(http_state(None));
        let resp = app.oneshot(proxy_request(None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    fn health_request() -> axum::http::Request<Body> {
        axum::http::Request::builder()
            .uri("/health")
            .method("GET")
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn health_answers_200_when_state_lock_is_free() {
        let app = router(http_state(None));
        let resp = app.oneshot(health_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_answers_503_when_state_write_lock_is_held() {
        // Simulate the 2026-08 reaper deadlock: a write guard held
        // across an await stalls every reader, and the health probe
        // must report 503 ("stalled") instead of a healthy 200 so the
        // LSP relay watchdog and the node HTTP watchdog can trigger
        // recovery. The 500 ms probe timeout bounds the test.
        let state = http_state(None);
        // Hold the write lock through the probe (borrow a local Arc so
        // `state` can still move into the router).
        let held = state.node.clone();
        let _guard = held.write().await;
        let app = router(state);
        let resp = app.oneshot(health_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
