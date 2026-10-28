//! PM service HTTP reverse proxy (ADR-064).
//!
//! Gateway 不再内嵌 PM（删除 `nest_service` 挂载），改为把 `/api/pm/*`
//! **反向代理**到独立进程 `acowork-pm`（`127.0.0.1:{pm_port}/*`）。复用
//! [`crate::http::proxy`] 的透明反代模式（ADR-033）。
//!
//! ## 路径映射
//!
//! | Gateway 公开 | PM 内部 |
//! |-------------|---------|
//! | `/api/pm/projects` | `/projects` |
//! | `/api/pm/tasks/{tid}` | `/tasks/{tid}` |
//! | `/api/pm/mcp` | `/mcp` |
//!
//! PM router 内部路径**不带** `/api` 前缀；本代理剥离 `/api/pm` 前缀后转发。
//!
//! ## 未就绪语义
//!
//! PM 进程未启动 / 正在重启（`pm_process` 为 `None`）时返回 **503**，并带
//! `Retry-After: 2`——Desktop 的 `with503Retry` 会按该 header 退避重试
//! （与 `/api/global-resources` 的 503 契约一致）。
//!
//! ## 身份（ADR-064 Phase 3）
//!
//! 反代时注入**可信身份**，修复客户端伪造漏洞：
//!
//! | 路径 | 策略 |
//! |------|------|
//! | `/api/pm/*`（REST，Desktop） | **覆盖** `X-Actor` 为可信身份。`multi_user` 下 = token 身份（`AuthContext.effective_user_id`）；`local` 下回退常量 `human`。客户端自报的 `X-Actor`（如伪造 `agent:xxx`）被丢弃 |
//! | `/api/pm/mcp`（MCP，Agent） | **校验** `X-MCP-Actor`：agent_id ∈ Gateway `installed_agents` → 透传（可信）；否则**剥离**（→ 匿名，仅只读工具，设计 §9.3）。客户端自报的 `X-Actor` 同样**剥离**（不注入替代值） |
//!
//! 安全语义：REST 面只允许人类操作（`created_by`/reviewer = 真实 user_id）；
//! Agent 身份只能经 MCP `X-MCP-Actor` 表达，且必须通过 Gateway Agent 目录校验。

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

/// Build the PM reverse-proxy router.
///
/// 挂载在 Gateway 根 router 上（`build_router`），匹配 `/api/pm/*`。
pub fn pm_proxy_routes() -> Router<AppState> {
    Router::new().route("/api/pm/{*rest}", any(pm_proxy_handler))
}

/// Reverse-proxy `/api/pm/{*rest}` → `http://127.0.0.1:{pm_port}/{rest}`。
///
/// 透明反代（RFC 7230 §2.3）：转发方法、路径、query、body 与全部非
/// hop-by-hop header，并按 ADR-064 Phase 3 注入可信身份。PM 未就绪时返回 503。
async fn pm_proxy_handler(
    State(state): State<AppState>,
    Path(rest): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    auth: Option<Extension<AuthContext>>,
    body: Bytes,
) -> Response {
    // ADR-076 §决策 10: under `multi_user` the REST actor is the token
    // identity; under `local` there is no `AuthContext` (the middleware is
    // a no-op) and we fall back to the `human` constant (§决策 12).
    let actor = trusted_rest_actor(auth.as_ref().map(|Extension(c)| c));
    // Whether the caller holds the admin role — the trusted signal the PM
    // `transfer-owner` gate reads (PM has no account store of its own). Taken
    // from the authenticated `AuthContext`, never the client (there is no
    // `AuthContext` under `local`, so it is `false` there).
    let actor_admin = auth.as_ref().map(|Extension(c)| c.is_admin()).unwrap_or(false);
    // 读取 PM 进程实际绑定端口 + 身份注入所需状态（supervisor 在 PM ready 后写入）。
    let (port, trusted_headers) = {
        let gw = state.gateway_state.read().await;
        let port = gw.pm_process.as_ref().map(|p| p.port);
        let is_mcp = rest == "mcp" || rest.starts_with("mcp/");
        let trusted = build_trusted_headers(&headers, is_mcp, &gw, &actor, actor_admin);
        (port, trusted)
    };
    let Some(port) = port else {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "PM service not ready",
                "message": "The PM service process has not started yet (or is restarting). Retry shortly.",
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

    tracing::debug!(port, target_url = %target_url, "Reverse-proxying to PM service");

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);

    // 转发注入后的可信 header（RFC 7230 §6.1 剥离 hop-by-hop）。
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
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to PM service");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to PM service",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Construct the forwarding headers (ADR-064 Phase 3 identity injection).
///
/// - **REST path** (`is_mcp = false`): drop the client-claimed `X-Actor` and
///   inject `actor` — the token identity under `multi_user`, the `human`
///   constant under `local` (ADR-076 §决策 10 / §决策 12).
/// - **MCP path** (`is_mcp = true`): validate `X-MCP-Actor` — agent_id in
///   Gateway `installed_agents` passes through; otherwise strip (→ anonymous,
///   read-only tools only). A client-claimed `X-Actor` is dropped here too (PM
///   reads it as the caller identity), and nothing is injected in its place.
fn build_trusted_headers(
    headers: &HeaderMap,
    is_mcp: bool,
    gw: &GatewayState,
    actor: &str,
    actor_admin: bool,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        // The node credential authenticates the caller at the Gateway's own
        // `auth_middleware` gate — PM identifies callers via `X-MCP-Actor`
        // / `X-Actor` and has no use for it, so the machine token must not
        // travel past the reverse proxy (least privilege).
        if name == acowork_core::auth::NODE_TOKEN_HEADER {
            continue;
        }
        // The admin flag is injected below from the authenticated identity
        // only; a client-claimed `x-actor-admin` never travels to PM (REST or
        // MCP), so it cannot spoof the `transfer-owner` authorization gate.
        if name == "x-actor-admin" {
            continue;
        }
        if is_mcp {
            // MCP 路径：只透传可信 X-MCP-Actor。
            if name == "x-mcp-actor" {
                if let Ok(agent_id) = value.to_str()
                    && gw.is_installed(agent_id)
                {
                    out.insert(name, value.clone());
                }
                // 不可信 / 缺失 → 不注入（匿名只读，设计 §9.3）
            } else if name != "x-actor" {
                // PM reads `x-actor` as the caller identity (its project
                // governance gate, including the `human` local-actor bypass).
                // The MCP face has no REST actor, so a client-claimed one is
                // dropped here exactly as on the REST branch below — otherwise
                // `x-actor: human` on this path forges the local actor.
                out.insert(name, value.clone());
            }
        } else {
            // REST 路径：丢弃客户端自报 X-Actor（下面注入可信值）。
            if name != "x-actor" {
                out.insert(name, value.clone());
            }
        }
    }
    if !is_mcp {
        if let Ok(header) = HeaderValue::from_str(actor) {
            out.insert("x-actor", header);
        }
        out.insert(
            "x-actor-admin",
            HeaderValue::from_static(if actor_admin { "true" } else { "false" }),
        );
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
    use crate::lifecycle::pm_supervisor::PmProcessState;

    /// 构造最小 `AppState`（与 `routes.rs::tests::test_app_state` 等价）。
    fn test_app_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-pm-proxy-{}-{}",
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

    /// `multi_user` `AppState` (ADR-076 §决策 12): the account system is on,
    /// so the auth middleware injects an `AuthContext` on authenticated
    /// requests. Returns the state plus the data dir for seeding.
    fn multi_user_app_state() -> AppState {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-pm-proxy-mu-{}-{}",
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

    /// 在 `127.0.0.1:0` 起一个 mock PM 服务（路径不带 `/api` 前缀），返回端口。
    async fn start_mock_pm_server() -> u16 {
        let app = axum::Router::new()
            .route(
                "/projects",
                get(|| async { axum::Json(serde_json::json!([])) }),
            )
            .route(
                "/tasks/{tid}",
                get(|Path(tid): Path<String>| async move {
                    axum::Json(serde_json::json!({ "id": tid }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock PM server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock PM server runs");
        });
        port
    }

    /// 模拟 supervisor 在 PM ready 后写入 `pm_process`。
    async fn set_pm_process(state: &AppState, port: u16) {
        let mut gw = state.gateway_state.write().await;
        gw.pm_process = Some(PmProcessState {
            pid: 0,
            port,
            ready: true,
        });
    }

    /// `pm_process = None`（PM 未启动 / 正在重启）：返回 503 + `Retry-After: 2`。
    #[tokio::test]
    async fn not_ready_returns_503() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/pm/projects")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "GET /api/pm/projects with pm_process=None should return 503"
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

    /// `pm_process = None`：嵌套子路径同样 503。
    #[tokio::test]
    async fn not_ready_nested_returns_503() {
        let state = test_app_state();
        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/pm/tasks/anything")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "nested /api/pm/tasks/* with pm_process=None should return 503"
        );
    }

    /// `pm_process = Some`：`GET /api/pm/projects` 透明转发到 mock PM，返回 200 + 空数组。
    #[tokio::test]
    async fn forwards_to_pm_process() {
        let state = test_app_state();
        let port = start_mock_pm_server().await;
        set_pm_process(&state, port).await;

        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/pm/projects")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "GET /api/pm/projects with pm_process=Some should return 200"
        );

        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).expect("body should be valid JSON");
        assert!(parsed.is_array(), "expected JSON array, got: {parsed}");
        assert_eq!(
            parsed.as_array().unwrap().len(),
            0,
            "mock PM should return zero projects"
        );
    }

    /// `pm_process = Some`：嵌套路径 `/api/pm/tasks/{tid}` 正确转发（剥离前缀）。
    #[tokio::test]
    async fn forwards_nested_path() {
        let state = test_app_state();
        let port = start_mock_pm_server().await;
        set_pm_process(&state, port).await;

        let router = build_router(state);

        let request = Request::builder()
            .method("GET")
            .uri("/api/pm/tasks/task-42")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "nested /api/pm/tasks/task-42 should be forwarded and return 200"
        );

        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read response body");
        let body_str = String::from_utf8_lossy(&bytes).to_string();
        let parsed: serde_json::Value =
            serde_json::from_str(&body_str).expect("body should be valid JSON");
        assert_eq!(
            parsed["id"],
            serde_json::json!("task-42"),
            "mock PM should echo the task id, got: {parsed}"
        );
    }

    /// 反代挂载不破坏 Gateway 原有路由（`/health` 仍 200）。
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
            "/health should still respond 200 when PM proxy is mounted"
        );
    }

    // ── ADR-064 Phase 3: 身份注入 ─────────────────────────────────────

    /// 起一个 mock PM 服务，`/echo-headers` 与 `/mcp` 回显收到的 header（验证注入）。
    async fn start_echo_pm_server() -> u16 {
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
            .expect("bind echo PM server");
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("echo PM server runs");
        });
        port
    }

    /// 往 GatewayState 注入一个已安装 Agent（`is_installed` 校验用）。
    async fn add_installed_agent(state: &AppState, agent_id: &str) {
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
            instance_id: agent_id.to_string(),
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Architect".to_string(),
            install_path: "/tmp/architect".to_string(),
            manifest,
            node_id: "local".to_string(),
        });
    }

    /// 经反代请求 mock PM 的 `/echo-headers`，返回回显的 header JSON。
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

    /// REST 路径：客户端伪造 `X-Actor: agent:com.evil` → Gateway 覆盖为 `human`。
    #[tokio::test]
    async fn rest_path_overrides_forged_x_actor() {
        let state = test_app_state();
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/pm/echo-headers",
            &[("X-Actor", "agent:com.evil")],
        )
        .await;

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("human"),
            "forged X-Actor must be overridden to trusted human actor, got: {echoed}"
        );
    }

    /// REST 路径：无 X-Actor 时注入 `human`（Desktop 默认操作者）。
    #[tokio::test]
    async fn rest_path_injects_human_when_absent() {
        let state = test_app_state();
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/pm/echo-headers", &[]).await;

        assert_eq!(
            echoed["x-actor"],
            serde_json::json!("human"),
            "absent X-Actor must be injected as human, got: {echoed}"
        );
    }

    /// ADR-076 §决策 10: under `multi_user` the REST actor is the *token*
    /// identity, not the `human` constant. A forged `X-Actor` is still
    /// dropped, and the injected value is the authenticated `user_id`.
    #[tokio::test]
    async fn rest_path_injects_token_identity_under_multi_user() {
        let state = multi_user_app_state();
        let token = seed_and_login(&state);
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let router = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/api/pm/echo-headers")
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

    /// REST 路径：客户端伪造 `x-actor-admin: true` 被丢弃，改由可信身份
    /// 推导（`local` 无 `AuthContext` → `false`）——否则任意调用方都能骗
    /// 过 PM 的 `transfer-owner` 鉴权门。
    #[tokio::test]
    async fn rest_path_overrides_forged_admin_flag() {
        let state = test_app_state();
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed =
            proxy_echo_headers(state, "/api/pm/echo-headers", &[("x-actor-admin", "true")]).await;

        assert_eq!(
            echoed["x-actor-admin"],
            serde_json::json!("false"),
            "forged x-actor-admin must be overridden by the trusted value (false under local), got: {echoed}"
        );
    }

    /// ADR-064 Phase 3: under `multi_user` an admin token injects the trusted
    /// `x-actor-admin: true`, and a client-claimed `false` is dropped.
    #[tokio::test]
    async fn rest_path_injects_admin_flag_for_admin_token() {
        let state = multi_user_app_state();
        let token = crate::http::test_support::access_token("u-root", "admin");
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let router = build_router(state);
        let request = Request::builder()
            .method("GET")
            .uri("/api/pm/echo-headers")
            .header("Authorization", format!("Bearer {token}"))
            .header("x-actor-admin", "false")
            .body(Body::empty())
            .expect("build request");
        let response = router.oneshot(request).await.expect("router responds");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let echoed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(
            echoed["x-actor-admin"],
            serde_json::json!("true"),
            "admin token must inject x-actor-admin=true and drop the forged value, got: {echoed}"
        );
    }

    /// MCP 路径：可信 agent（已安装）的 `X-MCP-Actor` 透传。
    #[tokio::test]
    async fn mcp_path_passes_trusted_actor() {
        let state = test_app_state();
        add_installed_agent(&state, "com.acowork.architect").await;
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/pm/mcp",
            &[("X-MCP-Actor", "com.acowork.architect")],
        )
        .await;

        assert_eq!(
            echoed["x-mcp-actor"],
            serde_json::json!("com.acowork.architect"),
            "trusted X-MCP-Actor must pass through, got: {echoed}"
        );
    }

    /// MCP 路径：不可信 agent（未安装）的 `X-MCP-Actor` 被剥离（→ 匿名只读）。
    #[tokio::test]
    async fn mcp_path_strips_untrusted_actor() {
        let state = test_app_state();
        add_installed_agent(&state, "com.acowork.architect").await;
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed =
            proxy_echo_headers(state, "/api/pm/mcp", &[("X-MCP-Actor", "com.evil.ghost")]).await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "untrusted X-MCP-Actor must be stripped (anonymous), got: {echoed}"
        );
    }

    /// MCP 路径：客户端自报的 `X-Actor` 被剥离。PM 拿它当调用方身份（项目
    /// 治理门 + `human` 本地 actor 豁免），而 MCP 面没有 REST 身份，所以
    /// 客户端声称 `human` 不能替它拿到本地 actor 的权限。
    #[tokio::test]
    async fn mcp_path_strips_a_client_claimed_x_actor() {
        let state = test_app_state();
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/pm/mcp", &[("x-actor", "human")]).await;

        assert!(
            echoed.get("x-actor").is_none(),
            "a client-claimed X-Actor must not travel on the MCP path, got: {echoed}"
        );
    }

    /// ADR-076 least privilege: the node machine credential authenticates
    /// the caller at the Gateway's own `auth_middleware` gate and MUST NOT
    /// be forwarded to the PM backend — it unlocks every Gateway `/api/*`
    /// route, and PM identifies callers via `X-MCP-Actor` / `X-Actor`.
    #[tokio::test]
    async fn node_token_is_stripped_before_forwarding() {
        let state = test_app_state();
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed = proxy_echo_headers(
            state,
            "/api/pm/mcp",
            &[("X-ACowork-Node-Token", "tok-abc"), ("X-MCP-Actor", "ghost")],
        )
        .await;

        assert!(
            echoed.get("x-acowork-node-token").is_none(),
            "node credential must be stripped at the reverse proxy, got: {echoed}"
        );
    }

    /// MCP 路径：无 `X-MCP-Actor` 保持匿名（不注入）。
    #[tokio::test]
    async fn mcp_path_keeps_anonymous_when_absent() {        let state = test_app_state();
        let port = start_echo_pm_server().await;
        set_pm_process(&state, port).await;

        let echoed = proxy_echo_headers(state, "/api/pm/mcp", &[]).await;

        assert!(
            echoed.get("x-mcp-actor").is_none(),
            "absent X-MCP-Actor must stay anonymous, got: {echoed}"
        );
        // MCP 路径不注入 X-Actor（REST 专属）。
        assert!(
            echoed.get("x-actor").is_none(),
            "MCP path must NOT inject X-Actor, got: {echoed}"
        );
    }
}
