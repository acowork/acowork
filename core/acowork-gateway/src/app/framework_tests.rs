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

/// A package larger than the Gateway's 64 MiB router cap still reaches the
/// installer: the install route's body limit is the package limit
/// (`MAX_APP_PACKAGE_BYTES`). Regression for vectorcraft (81 MiB), which the
/// router refused with a bare 413 the browser reported as "Failed to fetch".
#[tokio::test]
async fn a_package_over_the_router_body_cap_reaches_the_installer() {
    let port = start_mock_app().await;
    let body = Body::from(vec![0u8; 65 * 1024 * 1024]);
    let request = Request::builder()
        .method("POST")
        .uri("/api/apps/install")
        .header("authorization", format!("Bearer {SERVICE_TOKEN}"))
        .header("content-type", "application/octet-stream")
        .body(body)
        .unwrap();
    let response = build_router(test_state(port))
        .oneshot(request)
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "the 64 MiB router cap refused a package the installer accepts"
    );
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

/// The app proxy's channel table: `ui` and `fonts` are public bytes, `api` is the app's own
/// REST, and nothing else gets through. A font request is what makes a `.pptx` paint without
/// tofu, so it must not fall into "unknown app channel".
#[test]
fn app_proxy_channel_for_ui_fonts_api_and_junk() {
    use crate::app::proxy::channel_for;

    assert_eq!(channel_for("ui", "/ui").map(|(c, p)| (c, p)), Some(("ui", "/ui/".to_string())));
    assert_eq!(
        channel_for("ui/app.css", "/ui").map(|(c, p)| (c, p)),
        Some(("ui", "/ui/app.css".to_string()))
    );
    assert_eq!(
        channel_for("fonts/宋体", "/ui").map(|(c, p)| (c, p)),
        Some(("ui", "fonts/宋体".to_string()))
    );
    assert_eq!(channel_for("fonts", "/ui").map(|(c, _)| c), Some("ui"));
    assert_eq!(
        channel_for("api/projects", "/ui").map(|(c, p)| (c, p)),
        Some(("api", "projects".to_string()))
    );
    assert!(channel_for("secrets", "/ui").is_none());
    assert!(channel_for("apifoo", "/ui").is_none());
}

/// `call` with a request body (the install routes carry JSON / raw bytes).
async fn call_with_body(
    state: AppState,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Body,
) -> (StatusCode, serde_json::Value, String) {
    let router = build_router(state);
    let mut builder = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let response = router.oneshot(builder.body(body).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 256 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let json = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
    (status, json, text)
}

/// An install no longer holds the request open. `POST /api/apps/install-url`
/// answers `202 {job_id, phase, …}` immediately and the work continues in a
/// detached job, which `GET /api/apps/install-jobs[/{id}]` reports.
///
/// Regression for the UX that motivated this: a 17–77 MB download blocked the
/// request for a minute with nothing to show, and the Desktop's view-local
/// spinner vanished as soon as the user switched panels.
#[tokio::test]
async fn install_url_answers_202_and_the_job_reports_its_outcome() {
    let port = start_mock_app().await;
    let state = test_state(port);
    let auth = [("authorization", "Bearer test-svc-token")];
    // Port 1 on loopback refuses instantly: the download step fails, which is
    // the cheapest way to drive a job to a terminal state without a package.
    let body = Body::from(
        serde_json::json!({
            "url": "http://127.0.0.1:1/nope.app",
            "sha256": "0".repeat(64),
            "hint_id": "gridcraft",
        })
        .to_string(),
    );
    let (status, ack, text) = call_with_body(
        state.clone(),
        "POST",
        "/api/apps/install-url",
        &[("authorization", "Bearer test-svc-token"), ("content-type", "application/json")],
        body,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "ack: {text}");
    let job_id = ack["job_id"].as_str().expect("job id").to_string();
    assert_eq!(ack["hint_id"].as_str(), Some("gridcraft"));
    assert_eq!(ack["phase"].as_str(), Some("download"));

    // A second submit for the same card is answered with the running job, not
    // with a second download.
    let (status, again, _) = call_with_body(
        state.clone(),
        "POST",
        "/api/apps/install-url",
        &[("authorization", "Bearer test-svc-token"), ("content-type", "application/json")],
        Body::from(
            serde_json::json!({
                "url": "http://127.0.0.1:1/nope.app",
                "sha256": "0".repeat(64),
                "hint_id": "gridcraft",
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(again["job_id"].as_str(), Some(job_id.as_str()));

    // The detached task settles on its own; poll like the Desktop does.
    let mut job = serde_json::Value::Null;
    for _ in 0..200 {
        let (status, body, _) = call(
            state.clone(),
            "GET",
            &format!("/api/apps/install-jobs/{job_id}"),
            &auth,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        job = body;
        if job["phase"] == "failed" || job["phase"] == "done" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(job["phase"].as_str(), Some("failed"), "job: {job}");
    assert_eq!(job["error_code"].as_str(), Some("package_malformed"));
    assert!(
        job["error"].as_str().unwrap_or_default().contains("download"),
        "the failing step must keep its detail: {job}"
    );
    assert!(job["finished_at"].as_u64().is_some());

    // The list form the Desktop refetches on mount knows the same job.
    let (status, list, _) = call(state.clone(), "GET", "/api/apps/install-jobs", &auth).await;
    assert_eq!(status, StatusCode::OK);
    let jobs = list["jobs"].as_array().expect("jobs array");
    assert!(jobs.iter().any(|j| j["job_id"] == job_id.as_str()));
}
