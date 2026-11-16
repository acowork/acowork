//! App discovery and lifecycle-control API (ADR-116).
//!
//! These are platform control-plane routes: they stay behind the Gateway's
//! bearer gate. The app-scoped traffic under `/api/apps/{id}/ui|api|session`
//! is a separate, ticket-authenticated channel owned by `app::proxy`.

use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::Serialize;

use crate::app::registry::{app_mcp_url, app_status, resolve_app_port};
use crate::app::ticket::{SESSION_TTL_SECS, mint_session_ticket};
use crate::http::auth_middleware::{AuthContext, trusted_rest_actor};
use crate::http::routes::AppState;

pub fn app_routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps", get(list_apps))
        .route("/api/apps/{id}", get(get_app))
        .route("/api/apps/{id}/enable", post(enable_app))
        .route("/api/apps/{id}/disable", post(disable_app))
        .route("/api/apps/{id}/session-ticket", post(session_ticket))
}

/// The Desktop-facing projection of one registered app.
#[derive(Debug, Clone, Serialize)]
pub struct AppDto {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub icon: String,
    pub builtin: bool,
    pub enabled: bool,
    pub status: String,
    pub port: Option<u16>,
    pub runtime: String,
    pub mcp_path: String,
    pub mcp_gateway_path: String,
    pub mcp_url: Option<String>,
    pub ui_path: String,
    pub ui_mode: String,
    pub nav_label: String,
    pub nav_icon: String,
    pub nav_order: i32,
    pub health_path: String,
    pub permissions: Vec<String>,
}

fn to_dto(
    manifest: &acowork_core::app::AppManifest,
    gw: &crate::gateway::state::GatewayState,
) -> AppDto {
    let enabled = gw
        .app_registry
        .read()
        .map(|r| r.is_enabled(&manifest.id))
        .unwrap_or(false);
    let runtime = match manifest.process.runtime {
        acowork_core::app::AppRuntime::Legacy => "legacy",
        acowork_core::app::AppRuntime::Generic => "generic",
    };
    let ui_mode = match manifest.ui.mode {
        acowork_core::app::AppUiMode::Webview => "webview",
        acowork_core::app::AppUiMode::Native => "native",
    };
    let port = resolve_app_port(gw, manifest);
    AppDto {
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        description: manifest.description.clone(),
        icon: manifest.icon.clone(),
        builtin: manifest.builtin,
        enabled,
        status: app_status(gw, manifest).to_string(),
        port,
        runtime: runtime.to_string(),
        mcp_path: manifest.mcp.path.clone(),
        mcp_gateway_path: manifest.mcp.gateway_path.clone(),
        mcp_url: app_mcp_url(gw, manifest),
        ui_path: manifest.ui.path.clone(),
        ui_mode: ui_mode.to_string(),
        nav_label: manifest.ui.nav.label.clone(),
        nav_icon: manifest.ui.nav.icon.clone(),
        nav_order: manifest.ui.nav.order,
        health_path: manifest.process.health_path.clone(),
        permissions: manifest.permissions.platform.clone(),
    }
}

async fn list_apps(State(state): State<AppState>) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;
    let manifests: Vec<_> = gw
        .app_registry
        .read()
        .map(|r| r.all().cloned().collect())
        .unwrap_or_default();
    let dtos: Vec<AppDto> = manifests.iter().map(|m| to_dto(m, &gw)).collect();
    Json(serde_json::json!({ "apps": dtos }))
}

async fn get_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let gw = state.gateway_state.read().await;
    let manifest = gw.app_registry.read().ok().and_then(|r| r.get(&id).cloned());
    match manifest {
        Some(m) => Json(serde_json::to_value(to_dto(&m, &gw)).unwrap()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "app not found" })),
        )
            .into_response(),
    }
}

/// Republish `acowork/global/mcps` so Runtimes see the new catalog without
/// a restart. This is the ADR-090 hot-push path.
fn republish_catalog(state: &AppState) {
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }
}

async fn set_enabled(
    state: AppState,
    id: String,
    enabled: bool,
) -> impl IntoResponse {
    let changed = {
        let gw = state.gateway_state.read().await;
        let mut registry = match gw.app_registry.write() {
            Ok(r) => r,
            Err(_) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "app registry lock poisoned" })),
                )
                    .into_response();
            }
        };
        registry.set_enabled(&id, enabled)
    };
    if !changed {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "app not found" })),
        )
            .into_response();
    }
    let (dir, disabled) = {
        let gw = state.gateway_state.read().await;
        // A running Gateway always has a config snapshot; it is the same source
        // `build_available_mcps` uses. Without one (tests / embedded
        // construction) there is no durable location: keep the in-memory
        // change and skip persistence rather than dropping a `./data`
        // directory into the process's current working directory.
        let dir = gw.config.as_ref().map(|c| PathBuf::from(&c.data_dir));
        let disabled = gw
            .app_registry
            .read()
            .map(|r| r.disabled().clone())
            .unwrap_or_default();
        (dir, disabled)
    };
    if let Some(dir) = dir {
        if let Err(e) = acowork_core::app::AppRegistry::save_disabled(&dir, &disabled) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    } else {
        tracing::warn!(
            app = %id,
            "app enable state changed without a config snapshot; not persisted"
        );
    }
    republish_catalog(&state);
    tracing::info!(app = %id, enabled, "app enable state changed");
    Json(serde_json::json!({ "id": id, "enabled": enabled })).into_response()
}

async fn enable_app(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    set_enabled(state, id, true).await
}

async fn disable_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    set_enabled(state, id, false).await
}

/// Mint the short-lived capability ticket a Desktop iframe uses to obtain its
/// scoped session cookie. Only the authenticated main window can call this.
async fn session_ticket(
    State(state): State<AppState>,
    Path(id): Path<String>,
    auth: Option<Extension<AuthContext>>,
) -> impl IntoResponse {
    let known = state
        .gateway_state
        .read()
        .await
        .app_registry
        .read()
        .map(|r| r.is_enabled(&id))
        .unwrap_or(false);
    if !known {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "app not found or disabled" })),
        )
            .into_response();
    }
    let actor = trusted_rest_actor(auth.as_ref().map(|Extension(c)| c));
    let expires_at = crate::http::workspace_preview::now_unix() + SESSION_TTL_SECS;
    let ticket = mint_session_ticket(&state.app_session_secret, &id, &actor, expires_at);
    Json(serde_json::json!({
        "ticket": ticket,
        "expires_at": expires_at,
        "ui_url": format!("/api/apps/{}/ui/", id),
    }))
    .into_response()
}
