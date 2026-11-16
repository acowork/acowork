//! App → Platform API (ADR-116).
//!
//! The deliberately narrow surface an app's *backend* may call. It is
//! read-oriented and default-deny: every operation maps to a capability the
//! app declared in `app.toml`, and an undeclared operation is rejected. The
//! platform does NOT expose a host SDK that lets an app mutate agent config,
//! stop a Runtime, rewrite the MCP catalog or read another app's data — that
//! is the boundary between an application and a plug-in.
//!
//! An app reaches this API with the internal service token its supervisor
//! injected plus an `X-App-Id` header. The app's webview never sees a platform
//! token; it talks to its own backend, which forwards platform reads.

use axum::{
    Json, Router,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post},
};

use crate::http::auth_middleware::AuthContext;
use crate::http::routes::AppState;

pub const CAP_IDENTITY_READ: &str = "identity.read";
pub const CAP_CONTEXT_READ: &str = "context.read";
pub const CAP_AGENTS_READ: &str = "agents.read";
pub const CAP_NOTIFY: &str = "notify";

pub fn platform_routes() -> Router<AppState> {
    Router::new()
        .route("/api/platform/identity", get(identity))
        .route("/api/platform/context", get(context))
        .route("/api/platform/agents", get(agents))
        .route("/api/platform/notify", post(notify))
}

/// Resolve the calling app and check one declared capability.
///
/// Returns `None` when the caller is a normal user (no app id) — which is fine,
/// the Desktop may read these endpoints too — or `Some(error_response)` when
/// an app asked for something it did not declare.
fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    capability: &str,
) -> Option<axum::response::Response> {
    let app_id = headers
        .get("x-app-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if app_id.is_empty() {
        return None;
    }
    let allowed = state
        .gateway_state
        .try_read()
        .ok()
        .and_then(|gw| {
            gw.app_registry
                .read()
                .ok()
                .and_then(|r| r.get(&app_id).cloned())
        })
        .map(|m| m.permissions.platform.iter().any(|p| p == capability))
        .unwrap_or(false);
    if allowed {
        None
    } else {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "error": "capability_not_declared",
                    "app_id": app_id,
                    "capability": capability,
                })),
            )
                .into_response(),
        )
    }
}

async fn identity(
    State(state): State<AppState>,
    headers: HeaderMap,
    auth: Option<Extension<AuthContext>>,
) -> impl IntoResponse {
    if let Some(err) = authorize(&state, &headers, CAP_IDENTITY_READ) {
        return err;
    }
    let user_id = auth
        .as_ref()
        .map(|Extension(c)| c.effective_user_id().to_string())
        .or_else(|| {
            headers
                .get("x-actor")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "human".to_string());
    let role = if auth.as_ref().is_some_and(|Extension(c)| c.is_admin()) {
        "admin"
    } else {
        "user"
    };
    Json(serde_json::json!({
        "user_id": user_id,
        "role": role,
        "app_id": headers.get("x-app-id").and_then(|v| v.to_str().ok()),
    }))
    .into_response()
}

async fn context(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(err) = authorize(&state, &headers, CAP_CONTEXT_READ) {
        return err;
    }
    let gw = state.gateway_state.read().await;
    let (http_port, mqtt_port) = gw
        .config
        .as_ref()
        .map(|c| (c.http.port, c.mqtt.port))
        .unwrap_or((gw.http_port, 19875));
    Json(serde_json::json!({
        "platform": "acowork",
        "instance_id": gw.instance_id,
        "advertise_host": gw.advertise_host,
        "http_port": http_port,
        "mqtt_port": mqtt_port,
        "started_at": gw.started_at.to_rfc3339(),
    }))
    .into_response()
}

async fn agents(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(err) = authorize(&state, &headers, CAP_AGENTS_READ) {
        return err;
    }
    let gw = state.gateway_state.read().await;
    let list: Vec<_> = gw
        .installed_agents
        .values()
        .map(|a| {
            serde_json::json!({
                "instance_id": a.instance_id,
                "agent_id": a.agent_id,
                "name": a.name,
            })
        })
        .collect();
    Json(serde_json::json!({ "agents": list })).into_response()
}

async fn notify(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if let Some(err) = authorize(&state, &headers, CAP_NOTIFY) {
        return err;
    }
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .unwrap_or(serde_json::Value::Null);
    let level = payload
        .get("level")
        .and_then(|v| v.as_str())
        .unwrap_or("info");
    let message = payload
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // A notification is a platform-side hint, never a command. Recording it
    // in the structured log keeps the surface side-effect free.
    tracing::info!(app = ?headers.get(header::HOST), level, message, "app notification");
    Json(serde_json::json!({ "accepted": true })).into_response()
}
