//! acowork-doc service HTTP reverse proxy (ADR-064 pattern).
//!
//! The Gateway reverse-proxies `/api/doc/*` to the standalone `acowork-doc`
//! process (`127.0.0.1:{doc_port}/*`), reusing [`crate::http::proxy`]'s
//! transparent proxy mode (ADR-033).
//!
//! ## Path mapping
//!
//! | Gateway public | doc internal |
//! |----------------|--------------|
//! | `/api/doc/health` | `/health` |
//! | `/api/doc/api/tree` | `/api/tree` |
//! | `/api/doc/mcp` | `/mcp` |
//!
//! The doc router keeps its internal `/api/*` prefix; this proxy strips only
//! the `/api/doc` Gateway prefix before forwarding.
//!
//! ## Not-ready semantics
//!
//! When the doc process is not started / restarting (`doc_process` is
//! `None`) the proxy returns **503** with `Retry-After: 2` — the Desktop
//! `with503Retry` backs off per that header (same contract as `/api/pm/*`
//! and `/api/global-resources`).
//!
//! ## Identity (design §9)
//!
//! | Path | Policy |
//! |------|--------|
//! | `/api/doc/*` (REST, Desktop) | **Override** `X-Actor` with the trusted identity: the token identity (`AuthContext.effective_user_id`) under `multi_user`, the `human` constant under `local`. Client-claimed `X-Actor` (e.g. forged `agent:xxx`) is dropped. Under `multi_user`, also inject `x-auth-role` (admin/user) for the doc ACL manage gate — client-forged role headers are always dropped |
//! | `/api/doc/mcp` (MCP, Agent) | **Validate** `X-MCP-Actor`: agent_id ∈ Gateway `installed_agents` → pass through (trusted); otherwise **strip** (→ anonymous, read-only tools only, design §9.3). `x-auth-role` never travels here |

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::gateway::state::GatewayState;
use crate::http::auth_middleware::{AuthContext, trusted_rest_actor};
use crate::http::proxy::{is_hop_by_hop_header, runtime_http_client};
use crate::http::routes::AppState;

/// Build the doc reverse-proxy router.
///
/// Mounted on the Gateway root router (`build_router`), matching `/api/doc/*`.
pub fn doc_proxy_routes() -> Router<AppState> {
    Router::new().route("/api/doc/{*rest}", any(doc_proxy_handler))
}

/// Reverse-proxy `/api/doc/{*rest}` → `http://127.0.0.1:{doc_port}/{rest}`.
///
/// Transparent proxy (RFC 7230 §2.3): forwards method, path, query, body and
/// all non-hop-by-hop headers, injecting trusted identity. Returns 503 when
/// the doc process is not ready.
async fn doc_proxy_handler(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    auth: Option<Extension<AuthContext>>,
    body: Bytes,
) -> Response {
    // ADR-076 §决策 10: REST actor is the token identity under `multi_user`,
    // the `human` constant under `local` (§决策 12).
    let actor = trusted_rest_actor(auth.as_ref().map(|Extension(c)| c));
    // Design §9 (doc ACLs): the authenticated role rides along as a trusted
    // header too — the doc service gates permission *management* on it.
    // `None` under `local` (no account system, `human` bypasses anyway).
    let role = auth
        .as_ref()
        .map(|Extension(c)| if c.is_admin() { "admin" } else { "user" });
    // Read the doc process actual port + identity-injection state (the
    // supervisor writes `doc_process` once doc is ready).
    let (port, trusted_headers) = {
        let gw = state.gateway_state.read().await;
        let port = gw.doc_process.as_ref().map(|p| p.port);
        let is_mcp = rest == "mcp" || rest.starts_with("mcp/");
        let trusted = build_trusted_headers(&headers, is_mcp, &gw, &actor, role);
        (port, trusted)
    };
    let Some(port) = port else {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "doc service not ready",
                "message": "The doc service process has not started yet (or is restarting). Retry shortly.",
            })),
        )
            .into_response();
        response
            .headers_mut()
            .insert("Retry-After", axum::http::HeaderValue::from_static("2"));
        return response;
    };

    let target_url = if let Some(q) = query {
        format!("http://127.0.0.1:{}/{}?{}", port, rest, q)
    } else {
        format!("http://127.0.0.1:{}/{}", port, rest)
    };

    tracing::debug!(port, target_url = %target_url, "Reverse-proxying to doc service");

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);

    // Forward the injected trusted headers (RFC 7230 §6.1 strips hop-by-hop).
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
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to doc service");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to doc service",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Build the forwarding headers (identity injection).
///
/// - **REST path** (`is_mcp = false`): drop the client-claimed `X-Actor` and
///   inject `actor` — the token identity under `multi_user`, the `human`
///   constant under `local` (ADR-076 §决策 10 / §决策 12). When `role` is
///   `Some`, also inject `x-auth-role` so the doc service can gate permission
///   *management* on admin (design §9); client-forged `x-auth-role` is always
///   dropped.
/// - **MCP path** (`is_mcp = true`): validate `X-MCP-Actor` — agent_id in
///   Gateway `installed_agents` passes through; otherwise strip (→ anonymous,
///   read-only tools only). `x-auth-role` never travels on this path.
fn build_trusted_headers(
    headers: &HeaderMap,
    is_mcp: bool,
    gw: &GatewayState,
    actor: &str,
    role: Option<&str>,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        // The node credential authenticates the caller at the Gateway's own
        // `auth_middleware` gate — Doc identifies callers via `X-MCP-Actor`
        // / `X-Actor` and has no use for it, so the machine token must not
        // travel past the reverse proxy (least privilege).
        if name == acowork_core::auth::NODE_TOKEN_HEADER {
            continue;
        }
        // Client-forged role claims never reach the doc service — the
        // trusted value (if any) is injected below on the REST path.
        if name == acowork_core::auth::AUTH_ROLE_HEADER {
            continue;
        }
        if is_mcp {
            // MCP path: only pass through trusted X-MCP-Actor.
            if name == "x-mcp-actor" {
                if let Ok(agent_id) = value.to_str()
                    && gw.is_installed(agent_id)
                {
                    out.insert(name, value.clone());
                }
                // Untrusted / missing → not injected (anonymous read-only,
                // design §9.3).
            } else {
                out.insert(name, value.clone());
            }
        } else {
            // REST path: drop the client-claimed X-Actor (trusted value is
            // injected below).
            if name != "x-actor" {
                out.insert(name, value.clone());
            }
        }
    }
    if !is_mcp {
        if let Ok(header) = HeaderValue::from_str(actor) {
            out.insert("x-actor", header);
        }
        if let Some(role) = role
            && let Ok(h) = HeaderValue::from_str(role)
        {
            out.insert(acowork_core::auth::AUTH_ROLE_HEADER, h);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::extract::Path;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::{AppState, build_router};
    use crate::lifecycle::doc_supervisor::DocProcessState;

    /// Build a minimal `AppState` (same as `routes.rs::tests::test_app_state`).
    fn test_app_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-doc-proxy-{}-{}",
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

    /// Start a mock doc server on `127.0.0.1:0` (internal paths keep the
    /// `/api` prefix, as the doc router does). Returns the port.
    async fn start_mock_doc_server() -> u16 {
        let app = axum::Router::new()
            .route(
                "/api/tree",
                get(|| async { axum::Json(serde_json::json!({ "dirs": [] })) }),
            )
            .route(
                "/api/docs/{id}",
                get(|Path(id): Path<String>| async move {
                    axum::Json(serde_json::json!({ "doc_id": id, "content": "# hi" }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock doc server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock doc server runs");
        });
        port
    }

    /// Simulate the supervisor writing `doc_process` after doc is ready.
    async fn set_doc_process(state: &AppState, port: u16) {
        let mut gw = state.gateway_state.write().await;
        gw.doc_process = Some(DocProcessState {
            pid: 0,
            port,
            ready: true,
        });
    }

    /// `multi_user` `AppState` (ADR-076 §决策 12): the account system is on,
    /// so the auth middleware injects an `AuthContext` on authenticated
    /// requests. Mirrors `pm_proxy`'s helper — the two proxies are
    /// isomorphic and must be tested symmetrically.
    fn multi_user_app_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-doc-proxy-mu-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut gw_state = GatewayState::new(&dir.to_string_lossy());
        // ADR-084: the gate verifies with the user service's public key and
        // gates on the resolved mode — no account store involved.
        gw_state.user_verifier = Some(crate::http::test_support::verifier());
        let mut st = AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
        );
        st.auth_mode = crate::auth::AuthMode::MultiUser;
        st
    }

    /// A valid access token for `u-alice` (role `user`) — the identity these
    /// proxy tests author their requests as.
    ///
    /// Was an account seed + `login` before ADR-084; the `state` parameter is
    /// kept so every call site stays as it was.
    fn seed_and_login(_state: &AppState) -> String {
        crate::http::test_support::access_token("u-alice", "user")
    }

    /// `doc_process = None` (not started / restarting): 503 + `Retry-After: 2`.
    #[tokio::test]
    async fn not_ready_returns_503() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/tree")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "GET /api/doc/api/tree with doc_process=None should return 503"
        );
        assert_eq!(
            response
                .headers()
                .get("Retry-After")
                .map(|v| v.to_str().unwrap()),
            Some("2"),
            "503 must carry Retry-After: 2 for with503Retry"
        );
    }

    /// `doc_process = None`: nested sub-path also 503.
    #[tokio::test]
    async fn not_ready_nested_returns_503() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/docs/doc-1")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "nested /api/doc/api/docs/* with doc_process=None should return 503"
        );
    }

    /// `doc_process = Some`: `GET /api/doc/api/tree` transparently forwards.
    #[tokio::test]
    async fn forwards_to_doc_process() {
        let state = test_app_state();
        let port = start_mock_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/tree")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "GET /api/doc/api/tree with doc_process=Some should return 200"
        );

        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).expect("body should be valid JSON");
        assert_eq!(
            parsed["dirs"].as_array().unwrap().len(),
            0,
            "mock doc should return an empty dirs list"
        );
    }

    /// `doc_process = Some`: nested path `/api/doc/api/docs/{id}` forwards.
    #[tokio::test]
    async fn forwards_nested_path() {
        let state = test_app_state();
        let port = start_mock_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/api/docs/doc-42")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "nested /api/doc/api/docs/doc-42 should be forwarded and return 200"
        );

        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).expect("body should be valid JSON");
        assert_eq!(
            parsed["doc_id"],
            serde_json::json!("doc-42"),
            "mock doc should echo the doc id, got: {parsed}"
        );
    }

    /// Mounting the proxy must not break existing Gateway routes (`/health`).
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

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "/health should still respond 200 when the doc proxy is mounted"
        );
    }

    // ── Identity injection (design §9) ─────────────────────────────────

    /// Start a mock doc server whose `/echo-headers` and `/mcp` echo headers.
    async fn start_echo_doc_server() -> u16 {
        let echo = |headers: axum::http::HeaderMap| async move {
            let mut map = serde_json::Map::new();
            for (name, value) in headers.iter() {
                map.insert(
                    name.as_str().to_string(),
                    serde_json::json!(value.to_str().unwrap_or_default()),
                );
            }
            axum::Json(serde_json::Value::Object(map))
        };
        let app = axum::Router::new()
            .route("/echo-headers", get(echo))
            .route("/mcp", get(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo doc server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("echo doc server runs");
        });
        port
    }

    /// Inject an installed Agent into GatewayState (`is_installed` check).
    ///
    /// ADR-073: `instance_id` (UUID) and `agent_id` (reverse-DNS package
    /// id) are **different identities**. `installed_agents` is keyed by
    /// `instance_id` — a package id never matches `is_installed`. The
    /// defaults here mirror a real install: UUID instance + package id.
    async fn add_installed_agent(state: &AppState, instance_id: &str) {
        add_installed_agent_with_pkg(state, instance_id, "com.acowork.architect").await;
    }

    /// `add_installed_agent` with an explicit package id (for tests that
    /// must prove a package id does NOT pass the trust boundary).
    async fn add_installed_agent_with_pkg(state: &AppState, instance_id: &str, package_id: &str) {
        let manifest = acowork_core::AgentManifest::from_toml(&format!(
            r#"
            agent_id = "{package_id}"
            version = "1.0.0"
            name = "Architect"
            description = "test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#
        ))
        .expect("manifest parses");
        let mut gw = state.gateway_state.write().await;
        gw.add_installed(crate::gateway::state::AgentInfo {
            instance_id: instance_id.to_string(),
            agent_id: package_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Architect".to_string(),
            install_path: "/tmp/architect".to_string(),
            manifest,
            node_id: "local".to_string(),
        });
    }

    /// Proxy a GET to the mock echo server and return the echoed headers.
    async fn proxy_echo_headers(
        state: AppState,
        uri: &str,
        extra_headers: &[(&str, &str)],
    ) -> serde_json::Value {
        let router = build_router(state);
        let mut builder = Request::builder().method("GET").uri(uri);
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        let request = builder.body(Body::empty()).expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "proxy to echo server should return 200 for {uri}"
        );
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        serde_json::from_slice(&bytes).expect("echo body should be JSON")
    }

    /// REST path: forged `X-Actor: agent:com.evil` is overridden to `human`.
    #[tokio::test]
    async fn rest_path_overrides_forged_x_actor() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/echo-headers",
            &[("X-Actor", "agent:com.evil")],
        )
        .await;

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("human"),
            "forged X-Actor must be overridden to trusted human actor, got: {echoed}"
        );
    }

    /// ADR-076 §决策 10: under `multi_user` the REST actor is the *token*
    /// identity, not the `human` constant. Mirrors `pm_proxy`'s case.
    #[tokio::test]
    async fn rest_path_injects_token_identity_under_multi_user() {
        let state = multi_user_app_state();
        let token = seed_and_login(&state);
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/echo-headers")
            .header("Authorization", format!("Bearer {token}"))
            .header("X-Actor", "agent:com.evil")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let echoed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("u-alice"),
            "multi_user REST actor must be the token identity, got: {echoed}"
        );
    }

    /// REST path: absent X-Actor → inject `human` (Desktop default operator).
    #[tokio::test]
    async fn rest_path_injects_human_when_absent() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/doc/echo-headers", &[]).await;

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("human"),
            "absent X-Actor must be injected as human, got: {echoed}"
        );
    }

    // ── Role injection for the doc ACL manage gate (design §9) ────────

    /// REST path under `multi_user`: the token role is injected as trusted
    /// `x-auth-role`, and a client-forged `x-auth-role: admin` from a plain
    /// user is replaced (the doc service gates permission *management* on it).
    #[tokio::test]
    async fn rest_path_injects_token_role_and_drops_forged_role() {
        let state = multi_user_app_state();
        let token = seed_and_login(&state); // u-alice, role `user`
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/echo-headers")
            .header("Authorization", format!("Bearer {token}"))
            .header("x-auth-role", "admin")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let echoed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            echoed["x-auth-role"],
            serde_json::json!("user"),
            "forged x-auth-role must be replaced by the trusted token role, got: {echoed}"
        );
    }

    /// REST path under `multi_user` with an *admin* token → `x-auth-role: admin`.
    #[tokio::test]
    async fn rest_path_injects_admin_role() {
        let state = multi_user_app_state();
        let token = crate::http::test_support::access_token("u-root", "admin");
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let router = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/api/doc/echo-headers")
            .header("Authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let echoed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            echoed["x-auth-role"],
            serde_json::json!("admin"),
            "admin token must yield trusted admin role header, got: {echoed}"
        );
    }

    /// `local` mode has no account system: no `x-auth-role` is injected and a
    /// forged one is dropped (the doc service bypasses ACLs for `human` anyway).
    #[tokio::test]
    async fn local_path_never_carries_x_auth_role() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/echo-headers",
            &[("x-auth-role", "admin")],
        )
        .await;

        assert!(
            echoed.get("x-auth-role").is_none(),
            "client-forged x-auth-role must never reach the doc service, got: {echoed}"
        );
    }

    /// MCP path: `x-auth-role` never travels (ACL enforcement is REST-only).
    #[tokio::test]
    async fn mcp_path_strips_x_auth_role() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("x-auth-role", "admin")],
        )
        .await;

        assert!(
            echoed.get("x-auth-role").is_none(),
            "x-auth-role must be stripped on the MCP path, got: {echoed}"
        );
    }

    /// MCP path: trusted agent (installed) `X-MCP-Actor` passes through.
    ///
    /// ADR-073: the value must be the **instance UUID**, which is what the
    /// Gateway catalog `{instance_id}` template substitutes into the doc
    /// MCP headers for every runtime.
    #[tokio::test]
    async fn mcp_path_passes_trusted_actor() {
        let state = test_app_state();
        add_installed_agent(&state, "inst-0001-0000-0000-0000-000000000001").await;
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("X-MCP-Actor", "inst-0001-0000-0000-0000-000000000001")],
        )
        .await;

        assert_eq!(
            echoed["x-mcp-actor"],
            serde_json::json!("inst-0001-0000-0000-0000-000000000001"),
            "trusted X-MCP-Actor (instance UUID) must pass through, got: {echoed}"
        );
    }

    /// ADR-073 trust-boundary invariant: the **package id** (reverse-DNS)
    /// is NOT a valid `X-MCP-Actor`. Even though the package is installed,
    /// `installed_agents` is keyed by instance_id, so the header must be
    /// stripped — otherwise a package-id forgery would impersonate every
    /// installed instance of that package.
    #[tokio::test]
    async fn mcp_path_strips_package_id_even_when_package_installed() {
        let state = test_app_state();
        // The package IS installed (instance UUID registered), but the
        // attacker forges the header with the package id, not the UUID.
        add_installed_agent_with_pkg(
            &state,
            "inst-0001-0000-0000-0000-000000000001",
            "com.acowork.architect",
        )
        .await;
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("X-MCP-Actor", "com.acowork.architect")],
        )
        .await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "package id must NOT pass the X-MCP-Actor trust boundary (ADR-073), got: {echoed}"
        );
    }

    /// MCP path: untrusted agent (not installed) `X-MCP-Actor` is stripped.
    #[tokio::test]
    async fn mcp_path_strips_untrusted_actor() {
        let state = test_app_state();
        add_installed_agent(&state, "inst-0001-0000-0000-0000-000000000001").await;
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("X-MCP-Actor", "inst-9999-0000-0000-0000-000000000099")],
        )
        .await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "untrusted X-MCP-Actor must be stripped (anonymous), got: {echoed}"
        );
    }

    /// ADR-076 least privilege: the node machine credential authenticates
    /// the caller at the Gateway's own `auth_middleware` gate and MUST NOT
    /// be forwarded to the Doc backend — it unlocks every Gateway `/api/*`
    /// route, and Doc identifies callers via `X-MCP-Actor` / `X-Actor`.
    #[tokio::test]
    async fn node_token_is_stripped_before_forwarding() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/doc/mcp",
            &[("X-ACowork-Node-Token", "tok-abc"), ("X-MCP-Actor", "ghost")],
        )
        .await;

        assert!(
            echoed.get("x-acowork-node-token").is_none(),
            "node credential must be stripped at the reverse proxy, got: {echoed}"
        );
    }

    /// MCP path: absent `X-MCP-Actor` stays anonymous (no injection).
    #[tokio::test]
    async fn mcp_path_keeps_anonymous_when_absent() {
        let state = test_app_state();
        let port = start_echo_doc_server().await;
        set_doc_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/doc/mcp", &[]).await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "absent X-MCP-Actor must stay anonymous, got: {echoed}"
        );
        // MCP path must NOT inject X-Actor (REST-only).
        assert!(
            echoed.get("x-actor").is_none(),
            "MCP path must NOT inject X-Actor, got: {echoed}"
        );
    }
}
