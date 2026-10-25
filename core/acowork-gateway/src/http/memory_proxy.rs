//! acowork-memory service HTTP reverse proxy (ADR-101).
//!
//! The Gateway reverse-proxies the memory service (`127.0.0.1:{memory_port}/*`)
//! on two public surfaces, reusing the transparent-proxy mode (ADR-033) of
//! [`crate::http::proxy`]:
//!
//! | Gateway public | service internal | who calls it |
//! |----------------|------------------|--------------|
//! | `/api/agents/{id}/memory/{*rest}` | `/memory/{rest}?instance_id={id}` | Desktop memory panel |
//! | `/api/memory/memory/{*rest}` (via the short form below) | `/memory/{rest}` | Desktop global search |
//! | `/api/memory/mcp` | `/mcp` | Agent Runtime (MCP tool calls) |
//! | `/api/memory/admin/{*rest}` | `/admin/{rest}` | Agent Runtime (bootstrap) · Node (legacy-store import, ADR-101) |
//!
//! ## Why two surfaces
//!
//! The panel paths are frozen (ADR-101 keeps `memoryStore.ts` byte-identical),
//! so `/api/agents/{id}/memory/*` still carries the agent identity **in the
//! path** and the proxy turns it into the service's `?instance_id=` — the
//! client's own `instance_id` query parameter is dropped, never merged, so a
//! caller can never address a namespace other than the one it was authorized
//! for. `/api/memory/*` is the flat surface the Desktop's global search hits
//! directly; there the id arrives as a query parameter and is checked against
//! the same ownership tables the agent-scoped path goes through.
//!
//! ## Not-ready semantics
//!
//! When the supervisor holds no live process (`memory_process` is `None`) the
//! proxy returns **503** with `Retry-After: 2` — the same contract as
//! `/api/doc/*`, `/api/pm/*` and `/api/user/*`, which the Desktop's
//! `with503Retry` helper backs off on.
//!
//! ## Identity
//!
//! | Path | Policy |
//! |------|--------|
//! | MCP (`/api/memory/mcp`) | **Validate** `X-MCP-Actor`: instance id ∈ Gateway `installed_agents` → pass through; otherwise strip (the service then rejects the call — it has no anonymous namespace) |
//! | Agent-scoped REST | `instance_id` from the path, injected by the proxy. `permission_middleware` already gated the path at `use` (reads) / `manage` (writes) |
//! | Flat REST (`/api/memory/*`) | `?instance_id=` is caller-supplied, so this proxy runs the same `use`/`manage` ACL on it (ADR-087 invariant 4: machine actors carry no [`AuthContext`] and pass through) |

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::gateway::state::GatewayState;
use crate::http::acl::{self, Op, Resource};
use crate::http::auth_middleware::AuthContext;
use crate::http::proxy::{is_hop_by_hop_header, runtime_http_client};
use crate::http::routes::AppState;

/// Build the memory reverse-proxy router.
///
/// Mounted on the Gateway root router (`build_router`). Both surfaces land here
/// instead of the Runtime proxy: the panel paths used to be ten separate
/// `proxy_routes()` entries pointing at `Runtime /memory/*`, and P3 removes
/// those handlers — one catch-all per surface replaces them, with the public
/// paths and response shapes unchanged.
pub fn memory_proxy_routes() -> Router<AppState> {
    Router::new()
        .route("/api/memory/{*rest}", any(flat_memory_proxy_handler))
        .route(
            "/api/agents/{id}/memory/{*rest}",
            any(agent_memory_proxy_handler),
        )
}

/// `GET|POST|PUT|DELETE /api/agents/{id}/memory/{*rest}` →
/// `http://127.0.0.1:{memory_port}/memory/{rest}?instance_id={id}`.
///
/// The public path and the response body are exactly what the Desktop saw when
/// the Runtime answered these (ADR-101: panel contract frozen), so
/// `memoryStore.ts` needs no change beyond the readiness gate added in P4.
async fn agent_memory_proxy_handler(
    State(state): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    // The path is the single coordinate for the namespace: any client-sent
    // `instance_id` is discarded rather than merged.
    let query = with_instance_id(query.as_deref(), &id);
    let target_path = format!("/memory/{rest}");
    forward(
        &state,
        &target_path,
        &query,
        method,
        &headers,
        false,
        body,
    )
    .await
}

/// `ANY /api/memory/{*rest}` → the service's flat surface.
///
/// Path mapping is namespaced rather than a blind prefix strip, because the
/// service keeps its panel routes under `/memory/*` while the Gateway prefix is
/// already `/api/memory`: `/api/memory/search` → `/memory/search`,
/// `/api/memory/mcp` → `/mcp`, `/api/memory/admin/bootstrap` →
/// `/admin/bootstrap`.
async fn flat_memory_proxy_handler(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    auth: Option<Extension<AuthContext>>,
    body: Bytes,
) -> Response {
    let is_mcp = rest == "mcp" || rest.starts_with("mcp/");
    let target_path = if is_mcp || rest.starts_with("admin/") {
        format!("/{rest}")
    } else {
        format!("/memory/{rest}")
    };

    // The MCP surface authenticates itself through `X-MCP-Actor` (validated
    // against the install table below); only the REST surface needs an
    // instance namespace up front, and the service answers 400 without one.
    if !is_mcp {
        let instance_id = match query_param(query.as_deref(), "instance_id") {
            Some(id) => id.to_string(),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({
                        "error": "instance_id is required",
                        "message": "memory data is namespaced per agent instance; \
                                    pass ?instance_id=<agent instance id>",
                    })),
                )
                    .into_response();
            }
        };
        // Same tier the agent-scoped path is gated at by `permission_middleware`
        // (`classify`: memory reads = use, writes = manage). `decide` is a
        // no-op for a machine actor (no `AuthContext`) and under `local`.
        let op = if matches!(method, Method::GET | Method::HEAD) {
            Op::Use
        } else {
            Op::Manage
        };
        let resource = Resource::Agent(instance_id);
        if let Err(deny) = acl::decide(&state, auth.as_ref().map(|Extension(c)| c), op, &resource) {
            return acl::render(deny);
        }
    }

    forward(
        &state,
        &target_path,
        query.as_deref().unwrap_or(""),
        method,
        &headers,
        is_mcp,
        body,
    )
    .await
}

/// Forward one request to the memory service, or 503 when it is not up.
///
/// Transparent proxy (RFC 7230 §2.3): method, path, query, body and every
/// non-hop-by-hop header travel; only the identity headers are rewritten
/// ([`build_trusted_headers`]).
async fn forward(
    state: &AppState,
    target_path: &str,
    query: &str,
    method: Method,
    headers: &HeaderMap,
    is_mcp: bool,
    body: Bytes,
) -> Response {
    let (port, trusted_headers) = {
        let gw = state.gateway_state.read().await;
        let port = gw.memory_process.as_ref().map(|p| p.port);
        (port, build_trusted_headers(headers, is_mcp, &gw))
    };
    let Some(port) = port else {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "memory service not ready",
                "message": "The memory service process has not started yet (or is restarting). Retry shortly.",
            })),
        )
            .into_response();
        response
            .headers_mut()
            .insert("Retry-After", HeaderValue::from_static("2"));
        return response;
    };

    let target_url = if query.is_empty() {
        format!("http://127.0.0.1:{port}{target_path}")
    } else {
        format!("http://127.0.0.1:{port}{target_path}?{query}")
    };

    tracing::debug!(port, target_url = %target_url, "Reverse-proxying to memory service");

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);
    for (name, value) in trusted_headers.iter() {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
            response_builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to memory service");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to memory service",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Forward an internal Gateway request to one instance's memory namespace.
///
/// Used by the dimension-migration task ([`crate::http::embedding_api`]), which
/// starts a re-embed and polls its progress. It used to reach those endpoints
/// through the Runtime; the service owns them now, so the call goes over this
/// proxy with `instance_id` injected exactly like the panel path does.
pub async fn proxy_to_memory_service(
    state: &AppState,
    instance_id: &str,
    target_path: &str,
    method: Method,
    body: Option<Vec<u8>>,
    headers: &HeaderMap,
) -> Response {
    let query = with_instance_id(None, instance_id);
    let body = Bytes::from(body.unwrap_or_default());
    forward(state, target_path, &query, method, headers, false, body).await
}

/// Strip the hop-by-hop headers and the machine credentials, and gate
/// `X-MCP-Actor` on the install table for MCP calls.
///
/// `X-ACowork-Node-Token` never leaves the Gateway (least privilege, same rule
/// as [`crate::http::doc_proxy`]): the node credential authenticates the caller
/// at the Gateway's own `auth_middleware`, and the memory service identifies
/// callers only by the instance namespace it is handed.
fn build_trusted_headers(headers: &HeaderMap, is_mcp: bool, gw: &GatewayState) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        if name == acowork_core::auth::NODE_TOKEN_HEADER {
            continue;
        }
        if is_mcp && name == "x-mcp-actor" {
            // Untrusted / missing → not injected; the service then refuses the
            // call instead of falling back to some default namespace.
            if let Ok(actor) = value.to_str() && gw.is_installed(actor) {
                out.insert(name, value.clone());
            }
            continue;
        }
        out.insert(name, value.clone());
    }
    out
}

/// Rebuild a query string that carries exactly one `instance_id` — the trusted
/// one. Client-supplied duplicates are dropped, not appended to.
fn with_instance_id(query: Option<&str>, instance_id: &str) -> String {
    let mut parts: Vec<String> = query
        .unwrap_or("")
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| key_of(pair) != "instance_id")
        .map(|pair| (*pair).to_string())
        .collect();
    parts.push(format!("instance_id={instance_id}"));
    parts.join("&")
}

/// The name half of a `k=v` query pair (or of a bare `k`).
fn key_of(pair: &str) -> &str {
    pair.split('=').next().unwrap_or(pair)
}

/// The value of a query pair, if the caller sent one.
fn query_param<'a>(query: Option<&'a str>, name: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        (key == name && !value.is_empty()).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use axum::routing::get;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use super::*;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::{AppState, build_router};
    use crate::lifecycle::memory_supervisor::MemoryProcessState;

    fn test_app_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-memory-proxy-{}-{}",
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

    /// Start a stand-in memory service on `127.0.0.1:0` that echoes the request
    /// line it received (path + query) and the headers it was given.
    async fn start_echo_memory_server() -> u16 {
        let echo = |uri: axum::http::Uri, headers: axum::http::HeaderMap| async move {
            let mut map = serde_json::Map::new();
            map.insert("path".to_string(), serde_json::json!(uri.path()));
            map.insert(
                "query".to_string(),
                serde_json::json!(uri.query().unwrap_or_default()),
            );
            for (name, value) in headers.iter() {
                map.insert(
                    name.as_str().to_string(),
                    serde_json::json!(value.to_str().unwrap_or_default()),
                );
            }
            axum::Json(serde_json::Value::Object(map))
        };
        let app = axum::Router::new()
            .route("/memory/{*rest}", get(echo).post(echo).put(echo).delete(echo))
            .route("/mcp", get(echo).post(echo))
            .route("/admin/{*rest}", get(echo).post(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo memory server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("echo memory runs");
        });
        port
    }

    async fn set_memory_process(state: &AppState, port: u16) {
        let mut gw = state.gateway_state.write().await;
        gw.memory_process = Some(MemoryProcessState {
            pid: 0,
            port,
            ready: true,
        });
    }

    async fn read_json(response: Response) -> serde_json::Value {
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("body should be valid JSON")
    }

    #[test]
    fn with_instance_id_replaces_a_client_supplied_one() {
        assert_eq!(
            with_instance_id(Some("page=2&instance_id=spoofed&size=5"), "real-id"),
            "page=2&size=5&instance_id=real-id"
        );
        assert_eq!(with_instance_id(None, "real-id"), "instance_id=real-id");
        assert_eq!(
            with_instance_id(Some("instance_id"), "real-id"),
            "instance_id=real-id",
            "a bare `instance_id` flag must not survive either"
        );
    }

    #[test]
    fn query_param_reads_values_only() {
        assert_eq!(query_param(Some("a=1&instance_id=x&b=2"), "instance_id"), Some("x"));
        assert_eq!(query_param(Some("instance_id="), "instance_id"), None);
        assert_eq!(query_param(None, "instance_id"), None);
    }

    /// `memory_process = None`: 503 + `Retry-After: 2`, panel path included.
    #[tokio::test]
    async fn not_ready_returns_503_with_retry_after() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/agents/inst-1/memory/nodes")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get("Retry-After").map(|v| v.to_str().unwrap()),
            Some("2"),
            "503 must carry Retry-After: 2 for with503Retry"
        );
    }

    /// The panel path forwards to `/memory/*` and carries the path id as
    /// `instance_id`, keeping every other query parameter.
    #[tokio::test]
    async fn agent_scoped_path_injects_instance_id() {
        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/agents/inst-42/memory/nodes?type=Knowledge&page=2")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        assert_eq!(body["path"], "/memory/nodes");
        assert!(
            body["query"]
                .as_str()
                .unwrap()
                .contains("instance_id=inst-42"),
            "instance_id must be injected, got: {body}"
        );
        assert!(
            body["query"].as_str().unwrap().contains("page=2"),
            "existing query params must survive, got: {body}"
        );
    }

    /// A client-forged `?instance_id=` on the panel path is dropped — the path
    /// is the only coordinate for the namespace.
    #[tokio::test]
    async fn agent_scoped_path_ignores_forged_instance_id() {
        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/agents/inst-42/memory/stats?instance_id=victim")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        let query = body["query"].as_str().unwrap();
        assert!(query.contains("instance_id=inst-42"), "{body}");
        assert!(!query.contains("victim"), "forged id must be dropped: {body}");
    }

    /// The flat surface maps `/api/memory/search` to `/memory/search` and
    /// passes the caller's `instance_id` through.
    #[tokio::test]
    async fn flat_surface_maps_search_into_memory_namespace() {
        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/memory/search?q=hi&instance_id=inst-1")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        assert_eq!(body["path"], "/memory/search");
        assert_eq!(body["query"], "q=hi&instance_id=inst-1");
    }

    /// Without a namespace the flat surface must not reach the service at all.
    #[tokio::test]
    async fn flat_surface_requires_instance_id() {
        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/memory/search?q=hi")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// `/api/memory/mcp` is forwarded as the service's `/mcp` route, and a
    /// trusted `X-MCP-Actor` travels while the machine token does not.
    #[tokio::test]
    async fn mcp_path_passes_trusted_actor_and_strips_node_token() {
        use acowork_core::auth::{NODE_TOKEN_HEADER, NODE_TOKEN_TEMPLATE};

        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        add_installed_agent(&state, "inst-1").await;
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/memory/mcp")
            .header("x-mcp-actor", "inst-1")
            .header(NODE_TOKEN_HEADER, NODE_TOKEN_TEMPLATE)
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        assert_eq!(body["path"], "/mcp");
        assert_eq!(body["x-mcp-actor"], "inst-1");
        assert!(
            body.get(NODE_TOKEN_HEADER).is_none(),
            "the machine credential must not cross the reverse proxy: {body}"
        );
    }

    /// An `X-MCP-Actor` that is not an installed instance is stripped, so the
    /// service never sees a namespace the Gateway did not vouch for.
    #[tokio::test]
    async fn mcp_path_strips_untrusted_actor() {
        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/memory/mcp")
            .header("x-mcp-actor", "not-installed")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        assert!(
            body.get("x-mcp-actor").is_none(),
            "untrusted actor must be stripped: {body}"
        );
    }

    /// `/api/memory/admin/bootstrap` is the Runtime's first-start write entry —
    /// it maps onto the service's `/admin/bootstrap`, not `/memory/admin/…`.
    #[tokio::test]
    async fn admin_path_maps_to_service_admin_namespace() {
        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/memory/admin/bootstrap?instance_id=inst-1")
            .header("content-type", "application/json")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        assert_eq!(body["path"], "/admin/bootstrap");
    }

    /// The contract the Node's one-shot migration pass writes against
    /// (ADR-101 decision 10): a raw SQLite body, a `?instance_id=` namespace,
    /// and no machine credential past the reverse proxy. A machine actor has no
    /// `AuthContext`, so the write-tier ACL must let it through.
    #[tokio::test]
    async fn node_legacy_store_import_forwards_raw_body_and_drops_machine_token() {
        use acowork_core::auth::{NODE_TOKEN_HEADER, NODE_TOKEN_TEMPLATE};

        let state = test_app_state();
        let port = start_echo_memory_server().await;
        set_memory_process(&state, port).await;
        let router = build_router(state);

        let request = Request::builder()
            .method("POST")
            .uri("/api/memory/admin/import?instance_id=inst-9")
            .header("content-type", "application/x-sqlite3")
            .header(NODE_TOKEN_HEADER, NODE_TOKEN_TEMPLATE)
            .body(Body::from(b"SQLite format 3\0".to_vec()))
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
        let body = read_json(response).await;
        assert_eq!(body["path"], "/admin/import");
        assert_eq!(body["query"], "instance_id=inst-9");
        assert_eq!(body["content-type"], "application/x-sqlite3");
        assert!(
            body.get(NODE_TOKEN_HEADER).is_none(),
            "the node credential is between the Node and the Gateway only: {body}"
        );
    }

    /// Mounting the proxy must not break existing Gateway routes.
    #[tokio::test]
    async fn preserves_gateway_routes() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Register an installed instance (the `is_installed` table key is the
    /// instance identity, ADR-073).
    async fn add_installed_agent(state: &AppState, instance_id: &str) {
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.acowork.architect"
            version = "1.0.0"
            name = "Architect"
            description = "test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#,
        )
        .expect("manifest parses");
        let mut gw = state.gateway_state.write().await;
        gw.add_installed(crate::gateway::state::AgentInfo {
            instance_id: instance_id.to_string(),
            agent_id: "com.acowork.architect".to_string(),
            version: "1.0.0".to_string(),
            name: "Architect".to_string(),
            install_path: "/tmp/architect".to_string(),
            manifest,
            node_id: "local".to_string(),
        });
    }
}

