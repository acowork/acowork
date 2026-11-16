//! End-to-end router tests for the hosted-application framework (ADR-116).
//!
//! These exercise the real `build_router` stack — auth middleware, registry,
//! session ticket, reverse proxy and platform API — against a mock app HTTP
//! service on a loopback ephemeral port. No Gateway/Runtime process is
//! started, which keeps the suite fast and side-effect free.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode},
    response::{Html, Json},
    routing::get,
};
use tokio::sync::RwLock;
use tower::ServiceExt;

use crate::gateway::state::GatewayState;
use crate::http::auth::HttpAuth;
use crate::http::routes::{AppState, build_router};
use crate::lifecycle::pm_supervisor::PmProcessState;

const SERVICE_TOKEN: &str = "test-svc-token";

fn test_state(port: u16) -> AppState {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-app-framework-{}-{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut gw = GatewayState::new(&dir.to_string_lossy());
    // Mirror a running Gateway's config snapshot: enable/disable must persist
    // into this temp dir, never into a `./data` folder under the CWD.
    let mut cfg = crate::config::GatewayConfig::default();
    cfg.data_dir = dir.to_string_lossy().to_string();
    gw.config = Some(cfg);
    gw.user_verifier = Some(crate::http::test_support::verifier());
    gw.internal_service_token = Some(SERVICE_TOKEN.to_string());
    gw.pm_process = Some(PmProcessState {
        pid: 0,
        port,
        ready: true,
    });
    AppState::new(Arc::new(RwLock::new(gw)), Arc::new(HttpAuth::new(false)))
}

/// Mock hosted app: its own bundle plus a header-echo REST endpoint.
async fn start_mock_app() -> u16 {
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
    let app = Router::new()
        .route("/ui/", get(|| async { Html("<html>app-owned</html>") }))
        .route("/app-context", get(echo))
        .route("/health", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    port
}

async fn call(
    state: AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, serde_json::Value, String) {
    let router = build_router(state);
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = router
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, json, text)
}

/// Registry exposes both built-in apps with their declared contract.
#[tokio::test]
async fn lists_pm_and_doc_from_registry() {
    let port = start_mock_app().await;
    let (status, body, _) = call(
        test_state(port),
        "GET",
        "/api/apps",
        &[("authorization", "Bearer test-svc-token")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let apps = body["apps"].as_array().expect("apps array");
    let ids: Vec<&str> = apps.iter().map(|a| a["id"].as_str().unwrap()).collect();
    assert!(ids.contains(&"pm") && ids.contains(&"doc"), "ids: {ids:?}");
    let pm = apps.iter().find(|a| a["id"] == "pm").unwrap();
    assert_eq!(pm["mcp_gateway_path"], "/api/pm/mcp");
    assert_eq!(pm["ui_path"], "/ui/");
    assert_eq!(pm["status"], "ready");
    assert_eq!(pm["port"].as_u64(), Some(u64::from(port)));
}

/// The static bundle is reachable without a platform token, and the Gateway
/// injects trusted identity on the authenticated REST channel.
#[tokio::test]
async fn ui_and_api_channels_forward_with_injected_identity() {
    let port = start_mock_app().await;
    let state = test_state(port);

    let (status, _, html) = call(state.clone(), "GET", "/api/apps/pm/ui/", &[]).await;
    assert_eq!(status, StatusCode::OK, "ui: {html}");
    assert!(html.contains("app-owned"));

    let (status, body, _) = call(
        state.clone(),
        "POST",
        "/api/apps/pm/session-ticket",
        &[("authorization", "Bearer test-svc-token")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let ticket = body["ticket"].as_str().expect("ticket").to_string();

    let (status, echoed, _) = call(
        state.clone(),
        "GET",
        "/api/apps/pm/api/app-context",
        &[("x-app-session", &ticket)],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "echo: {echoed}");
    assert_eq!(echoed["x-app-id"], "pm");
    assert_eq!(echoed["x-actor"], "human");
    assert_eq!(echoed["x-actor-admin"], "false");
    assert_eq!(echoed["x-app-platform-url"], "http://127.0.0.1:19876");
    assert!(echoed.get("authorization").is_none(), "bearer must not leak");
    assert!(echoed.get("x-app-session").is_none(), "ticket must not leak");
}

/// A ticket minted for one app cannot be replayed against another, and an
/// unauthenticated app REST call is rejected.
#[tokio::test]
async fn session_is_bound_to_one_app_and_required() {
    let port = start_mock_app().await;
    let state = test_state(port);
    let (_, body, _) = call(
        state.clone(),
        "POST",
        "/api/apps/pm/session-ticket",
        &[("authorization", "Bearer test-svc-token")],
    )
    .await;
    let ticket = body["ticket"].as_str().unwrap().to_string();

    let (status, _, _) = call(
        state.clone(),
        "GET",
        "/api/apps/doc/api/app-context",
        &[("x-app-session", &ticket)],
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "cross-app replay must fail");

    let (status, _, _) = call(state, "GET", "/api/apps/pm/api/app-context", &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "missing session must fail");
}

/// Disabling an app is a platform admission decision, and the control plane
/// stays behind the bearer gate.
#[tokio::test]
async fn disable_changes_admission_and_control_plane_is_gated() {
    let port = start_mock_app().await;
    let state = test_state(port);

    let (status, _, _) = call(state.clone(), "POST", "/api/apps/doc/disable", &[]).await;
    assert_ne!(status, StatusCode::OK, "control plane must require a token");

    let (status, body, _) = call(
        state.clone(),
        "POST",
        "/api/apps/doc/disable",
        &[("authorization", "Bearer test-svc-token")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "disable: {body}");

    let (_, body, _) = call(
        state.clone(),
        "GET",
        "/api/apps",
        &[("authorization", "Bearer test-svc-token")],
    )
    .await;
    let doc = body["apps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "doc")
        .unwrap();
    assert_eq!(doc["enabled"], false);
    assert_eq!(doc["status"], "disabled");
    assert!(doc["port"].is_null(), "disabled app resolves to no port");

    let (status, _, _) = call(state, "GET", "/api/apps/doc/ui/", &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// Declared capabilities are enforced; an undeclared one is default-denied.
#[tokio::test]
async fn platform_api_enforces_declared_capabilities() {
    let port = start_mock_app().await;
    let state = test_state(port);

    let (status, body, _) = call(
        state.clone(),
        "GET",
        "/api/platform/context",
        &[
            ("authorization", "Bearer test-svc-token"),
            ("x-app-id", "pm"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "context: {body}");
    assert_eq!(body["platform"], "acowork");

    // A made-up app id has no manifest → no declared capability → 403.
    let (status, body, _) = call(
        state,
        "GET",
        "/api/platform/agents",
        &[
            ("authorization", "Bearer test-svc-token"),
            ("x-app-id", "ghost"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"], "capability_not_declared");
}
