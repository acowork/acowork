//! Generic app MCP reverse proxy (ADR-116 / design doc 34).
//!
//! Exact route `*/api/apps/{id}/mcp` → upstream `http://127.0.0.1:{port}/mcp`.
//! Deliberately **not** under the ticket-authenticated `/api/apps/{id}/{*rest}`
//! catch-all: Agent Runtime authenticates with node token + `X-MCP-Actor`,
//! the same trust model as `/api/doc/mcp` and `/api/pm/mcp`.
//!
//! Identity policy (mirrors `doc_proxy` MCP path):
//! - Validate `X-MCP-Actor`: instance id ∈ `installed_agents` → pass through;
//!   otherwise strip (anonymous / read-only tools).
//! - Never forward `X-ACowork-Node-Token`, and never forward any member of the
//!   trusted-identity header family (`auth_middleware::is_identity_assertion_header`) —
//!   a client-supplied identity must not reach an app as if the Gateway wrote it.
//! - App disabled or process not ready → `503 + Retry-After: 2`.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::app::registry::resolve_app_port;
use crate::gateway::state::GatewayState;
use crate::http::proxy::{is_hop_by_hop_header, runtime_http_client};
use crate::http::routes::AppState;

pub fn app_mcp_routes() -> Router<AppState> {
    Router::new().route("/api/apps/{id}/mcp", any(app_mcp_handler))
}

async fn app_mcp_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    let (port, mcp_path, trusted) = {
        let gw = state.gateway_state.read().await;
        let Some(manifest) = gw
            .app_registry
            .read()
            .ok()
            .and_then(|r| r.get(&id).cloned())
        else {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "app not found" })),
            )
                .into_response();
        };
        let enabled = gw
            .app_registry
            .read()
            .map(|r| r.is_enabled(&id))
            .unwrap_or(false);
        if !enabled {
            return service_unavailable("app disabled");
        }
        let port = resolve_app_port(&gw, &manifest);
        let trusted = build_mcp_headers(&headers, &gw);
        (port, manifest.mcp.path.clone(), trusted)
    };

    let Some(port) = port else {
        return service_unavailable("app process not ready");
    };

    let path = if mcp_path.starts_with('/') {
        mcp_path
    } else {
        format!("/{mcp_path}")
    };
    let target_url = if let Some(q) = query {
        format!("http://127.0.0.1:{port}{path}?{q}")
    } else {
        format!("http://127.0.0.1:{port}{path}")
    };

    tracing::debug!(app = %id, port, target_url = %target_url, "Reverse-proxying app MCP");

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);
    for (name, value) in trusted.iter() {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }

    match request.send().await {
        Ok(upstream) => {
            let status =
                StatusCode::from_u16(upstream.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = Response::builder().status(status);
            if let Some(headers_mut) = response.headers_mut() {
                for (name, value) in upstream.headers().iter() {
                    if is_hop_by_hop_header(name) {
                        continue;
                    }
                    headers_mut.insert(name, value.clone());
                }
            }
            let bytes = upstream.bytes().await.unwrap_or_default();
            response
                .body(axum::body::Body::from(bytes))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::BAD_GATEWAY,
                        axum::Json(serde_json::json!({ "error": "failed to build proxy response" })),
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, app = %id, "Failed to proxy app MCP");
            service_unavailable("app MCP upstream unreachable")
        }
    }
}

fn service_unavailable(message: &str) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": "app mcp not ready",
            "message": message,
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("Retry-After", HeaderValue::from_static("2"));
    response
}

fn build_mcp_headers(headers: &HeaderMap, gw: &GatewayState) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        if name == acowork_core::auth::NODE_TOKEN_HEADER {
            continue;
        }
        // The whole trusted-identity family, not just the role: this proxy
        // forwards to an app's MCP upstream, and the app authenticates agents
        // through `X-MCP-Actor` alone. Everything else in the family arrived
        // from the client as a claim about itself (ADR-076 §决策 4), so the
        // session-scope pair must not ride along either.
        if crate::http::auth_middleware::is_identity_assertion_header(name.as_str()) {
            continue;
        }
        if name == "x-mcp-actor" {
            if let Ok(agent_id) = value.to_str()
                && gw.is_installed(agent_id)
            {
                out.insert(name, value.clone());
            }
            continue;
        }
        // Strip client-forged app/session identity on the MCP path; the
        // upstream app authenticates agents via X-MCP-Actor only.
        // X-MCP-Workspace-Root (ADR-121) is NOT identity — it is the
        // Actor's session workspace path and must be forwarded.
        if matches!(
            name.as_str(),
            "x-actor"
                | "x-actor-admin"
                | "x-app-id"
                | "x-app-user"
                | "x-app-session"
                | "x-app-session-verified"
                | "authorization"
                | "cookie"
        ) {
            continue;
        }
        out.insert(name, value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, Request, StatusCode};
    use axum::response::Json;
    use axum::routing::get;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::app::registry::AppProcessState;
    use crate::gateway::state::{AgentInfo, GatewayState};
    use crate::http::auth::HttpAuth;
    use crate::http::routes::{AppState, build_router};

    const SERVICE_TOKEN: &str = "test-svc-token";
    const AGENT: &str = "inst-0001-0000-0000-0000-000000000001";

    fn test_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-app-mcp-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut gw = GatewayState::new(&dir.to_string_lossy());
        let cfg = crate::config::GatewayConfig {
            data_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        };
        gw.config = Some(cfg);
        gw.user_verifier = Some(crate::http::test_support::verifier());
        gw.internal_service_token = Some(SERVICE_TOKEN.to_string());
        AppState::new(Arc::new(RwLock::new(gw)), Arc::new(HttpAuth::new(false)))
    }

    async fn register_clone(state: &AppState, port: Option<u16>, enabled: bool) {
        let toml = r#"
id = "doc-clone"
version = "1.0.0"
name = "DOC Clone"
icon = "file-text"
builtin = false

[process]
runtime = "generic"
binary = "acowork-doc"
port_hint = 18181
health_path = "/health"

[mcp]
path = "/mcp"
auto_inject = true
gateway_path = "/api/apps/doc-clone/mcp"

[ui]
path = "/ui/"

[ui.nav]
label = "Clone"
"#;
        let manifest = acowork_core::app::AppManifest::from_toml(toml).unwrap();
        let mut gw = state.gateway_state.write().await;
        {
            let mut reg = gw.app_registry.write().unwrap();
            let mut apps: std::collections::BTreeMap<_, _> =
                reg.all().map(|m| (m.id.clone(), m.clone())).collect();
            apps.insert(manifest.id.clone(), manifest.clone());
            let mut disabled = reg.disabled().clone();
            if !enabled {
                disabled.insert("doc-clone".into());
            } else {
                disabled.remove("doc-clone");
            }
            *reg = acowork_core::app::AppRegistry::from_parts(apps, disabled);
        }
        if let Some(port) = port {
            gw.app_processes.insert(
                "doc-clone".into(),
                AppProcessState {
                    pid: 0,
                    port,
                    ready: true,
                },
            );
        } else {
            gw.app_processes.remove("doc-clone");
        }
    }

    async fn add_agent(state: &AppState, id: &str) {
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.acowork.test"
            version = "1.0.0"
            name = "Test"
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
        gw.add_installed(AgentInfo {
            instance_id: id.to_string(),
            agent_id: "com.acowork.test".into(),
            name: "Test".into(),
            version: "1.0.0".into(),
            install_path: "/tmp/test".into(),
            manifest,
            node_id: "local".into(),
        });
    }

    async fn start_echo_mcp() -> u16 {
        let echo = |headers: HeaderMap| async move {
            let mut map = serde_json::Map::new();
            for (name, value) in headers.iter() {
                map.insert(
                    name.as_str().to_string(),
                    serde_json::json!(value.to_str().unwrap_or_default()),
                );
            }
            Json(serde_json::Value::Object(map))
        };
        let app = Router::new().route("/mcp", get(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        port
    }

    async fn call(
        state: AppState,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (StatusCode, serde_json::Value) {
        let router = build_router(state);
        let mut builder = Request::builder().method("GET").uri(uri);
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        // Node token / bearer so auth middleware lets MCP through.
        builder = builder.header("Authorization", format!("Bearer {SERVICE_TOKEN}"));
        let response = router
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    #[tokio::test]
    async fn trusted_actor_passes_through() {
        let state = test_state();
        add_agent(&state, AGENT).await;
        let port = start_echo_mcp().await;
        register_clone(&state, Some(port), true).await;
        let (status, body) = call(
            state,
            "/api/apps/doc-clone/mcp",
            &[("X-MCP-Actor", AGENT)],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["x-mcp-actor"], serde_json::json!(AGENT));
    }

    #[tokio::test]
    async fn workspace_root_header_is_forwarded() {
        let state = test_state();
        add_agent(&state, AGENT).await;
        let port = start_echo_mcp().await;
        register_clone(&state, Some(port), true).await;
        let root = "/tmp/agent-ws";
        let (status, body) = call(
            state,
            "/api/apps/doc-clone/mcp",
            &[
                ("X-MCP-Actor", AGENT),
                ("X-MCP-Workspace-Root", root),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["x-mcp-workspace-root"], serde_json::json!(root));
        assert_eq!(body["x-mcp-actor"], serde_json::json!(AGENT));
    }

    #[tokio::test]
    async fn untrusted_actor_is_stripped() {
        let state = test_state();
        add_agent(&state, AGENT).await;
        let port = start_echo_mcp().await;
        register_clone(&state, Some(port), true).await;
        let (status, body) = call(
            state,
            "/api/apps/doc-clone/mcp",
            &[("X-MCP-Actor", "inst-9999-0000-0000-0000-000000000099")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("x-mcp-actor").is_none());
    }

    /// ADR-076 §决策 4: nobody but the Gateway may assert an identity, and this
    /// proxy is the last hop before a third-party app. The role was stripped
    /// before this test existed; the session-scope pair was forwarded by
    /// omission, which is exactly the bug a per-site list produces.
    #[tokio::test]
    async fn the_whole_identity_family_is_stripped() {
        let state = test_state();
        add_agent(&state, AGENT).await;
        let port = start_echo_mcp().await;
        register_clone(&state, Some(port), true).await;
        let (status, body) = call(
            state,
            "/api/apps/doc-clone/mcp",
            &[
                ("x-user-id", "u-alice"),
                ("x-auth-role", "admin"),
                ("x-auth-user", "u-alice"),
                ("x-auth-as-user", "u-bob"),
                ("x-acowork-scope-admin", "a-1"),
                ("x-acowork-chat-scope", "chat-1"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        for name in [
            "x-user-id",
            "x-auth-role",
            "x-auth-user",
            "x-auth-as-user",
            "x-acowork-scope-admin",
            "x-acowork-chat-scope",
        ] {
            assert!(
                body.get(name).is_none(),
                "{name} reached the app as if the Gateway had written it"
            );
        }
    }

    /// The node token is a machine credential, not an identity claim, and is
    /// stripped by name rather than by family — pinned so the family predicate
    /// cannot be "simplified" into covering it and silently letting a client
    /// present one.
    #[tokio::test]
    async fn the_node_token_never_reaches_an_app() {
        let state = test_state();
        add_agent(&state, AGENT).await;
        let port = start_echo_mcp().await;
        register_clone(&state, Some(port), true).await;
        let (status, body) = call(
            state,
            "/api/apps/doc-clone/mcp",
            &[("X-ACowork-Node-Token", "node-secret")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.get("x-acowork-node-token").is_none());
    }

    #[tokio::test]
    async fn disabled_app_returns_503() {
        let state = test_state();
        let port = start_echo_mcp().await;
        register_clone(&state, Some(port), false).await;
        let (status, body) = call(state, "/api/apps/doc-clone/mcp", &[]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "app mcp not ready");
    }

    #[tokio::test]
    async fn not_ready_returns_503() {
        let state = test_state();
        register_clone(&state, None, true).await;
        let (status, _) = call(state, "/api/apps/doc-clone/mcp", &[]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }
}
