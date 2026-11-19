//! App discovery and lifecycle-control API (ADR-116).
//!
//! These are platform control-plane routes: they stay behind the Gateway's
//! bearer gate. The app-scoped traffic under `/api/apps/{id}/ui|api|session`
//! is a separate, ticket-authenticated channel owned by `app::proxy`.

use std::path::PathBuf;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use crate::app::install::{
    InstallOptions, install_app_package, list_versions, rollback_app, uninstall_app,
    write_upload_with_limit,
};
use crate::app::registry::{app_mcp_url, app_status, resolve_app_port};
use crate::app::ticket::{SESSION_TTL_SECS, mint_session_ticket};
use crate::http::auth_middleware::{AuthContext, trusted_rest_actor};
use crate::http::routes::AppState;

pub fn app_routes() -> Router<AppState> {
    Router::new()
        .route("/api/apps", get(list_apps))
        .route("/api/apps/install", post(install_upload))
        .route("/api/apps/install-url", post(install_from_url))
        .route("/api/apps/{id}", get(get_app).delete(delete_app))
        .route("/api/apps/{id}/enable", post(enable_app))
        .route("/api/apps/{id}/disable", post(disable_app))
        .route("/api/apps/{id}/session-ticket", post(session_ticket))
        .route("/api/apps/{id}/versions", get(get_versions))
        .route("/api/apps/{id}/rollback", post(rollback))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
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
    let install = gw
        .config
        .as_ref()
        .and_then(|c| {
            acowork_core::app::AppRegistry::read_install_state(
                std::path::Path::new(&c.data_dir),
                &manifest.id,
            )
        });
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
        installed_version: install.as_ref().map(|s| s.current.clone()),
        fingerprint: install.as_ref().and_then(|s| s.fingerprint.clone()),
        origin: install.as_ref().map(|s| s.origin.clone()),
    }
}

async fn data_dir_async(state: &AppState) -> Option<PathBuf> {
    let gw = state.gateway_state.read().await;
    gw.config.as_ref().map(|c| PathBuf::from(&c.data_dir))
}

async fn allow_unsigned(state: &AppState) -> bool {
    let gw = state.gateway_state.read().await;
    gw.config
        .as_ref()
        .map(|c| c.apps.allow_unsigned || c.dev_mode)
        .unwrap_or(false)
}

fn install_err_response(err: crate::app::install::InstallError) -> axum::response::Response {
    let status = StatusCode::from_u16(err.http).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        Json(serde_json::json!({ "code": err.code, "error": err.message })),
    )
        .into_response()
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

async fn reload_and_maybe_start(state: &AppState, app_id: &str) {
    let Some(dir) = data_dir_async(state).await else {
        return;
    };
    {
        let gw = state.gateway_state.read().await;
        if let Ok(mut reg) = gw.app_registry.write() {
            reg.reload(&dir);
        }
    }
    let (manifest, tokens) = {
        let gw = state.gateway_state.read().await;
        let manifest = gw
            .app_registry
            .read()
            .ok()
            .and_then(|r| r.get(app_id).cloned());
        (
            manifest,
            (gw.internal_service_token.clone(), gw.http_port),
        )
    };
    let Some(manifest) = manifest else {
        return;
    };
    if manifest.process.runtime != acowork_core::app::AppRuntime::Generic {
        republish_catalog(state);
        return;
    }
    let enabled = {
        let gw = state.gateway_state.read().await;
        gw.app_registry
            .read()
            .map(|r| r.is_enabled(&manifest.id))
            .unwrap_or(false)
    };
    if !enabled {
        crate::app::supervisor::stop_app(&manifest.id, &state.gateway_state).await;
        republish_catalog(state);
        return;
    }
    let (token, http_port) = tokens;
    let gateway_url = format!("http://127.0.0.1:{http_port}");
    let health = format!("{gateway_url}/health");
    if let Err(e) = crate::app::supervisor::restart_app(
        &dir,
        &manifest,
        state.gateway_state.clone(),
        gateway_url,
        health,
        token,
        None,
    )
    .await
    {
        tracing::error!(app = %manifest.id, error = %e, "installed app failed to start");
    }
    republish_catalog(state);
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

async fn install_upload(
    State(state): State<AppState>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(dir) = data_dir_async(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gateway data_dir unavailable" })),
        )
            .into_response();
    };
    let staging = match write_upload_with_limit(&dir, body.as_ref()) {
        Ok(p) => p,
        Err(e) => return install_err_response(e),
    };
    let opts = InstallOptions {
        allow_unsigned: allow_unsigned(&state).await,
        origin: "upload".into(),
    };
    let outcome = match install_app_package(&dir, &staging, &opts) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&staging);
            return install_err_response(e);
        }
    };
    let _ = std::fs::remove_file(&staging);
    reload_and_maybe_start(&state, &outcome.app_id).await;
    let gw = state.gateway_state.read().await;
    let dto = gw
        .app_registry
        .read()
        .ok()
        .and_then(|r| r.get(&outcome.app_id).cloned())
        .map(|m| to_dto(&m, &gw));
    Json(serde_json::json!({
        "installed_version": outcome.installed_version,
        "fingerprint": outcome.fingerprint,
        "origin": outcome.origin,
        "app": dto,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
struct InstallUrlBody {
    url: String,
    sha256: String,
}

async fn install_from_url(
    State(state): State<AppState>,
    Json(body): Json<InstallUrlBody>,
) -> impl IntoResponse {
    if body.sha256.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "code": "package_malformed",
                "error": "sha256 is required for install-url"
            })),
        )
            .into_response();
    }
    let Some(dir) = data_dir_async(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gateway data_dir unavailable" })),
        )
            .into_response();
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let bytes = match client.get(&body.url).send().await {
        Ok(resp) if resp.status().is_success() => match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "code": "package_malformed",
                        "error": format!("download body: {e}")
                    })),
                )
                    .into_response();
            }
        },
        Ok(resp) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "code": "package_malformed",
                    "error": format!("download HTTP {}", resp.status())
                })),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "code": "package_malformed",
                    "error": format!("download failed: {e}")
                })),
            )
                .into_response();
        }
    };
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if !digest.eq_ignore_ascii_case(body.sha256.trim()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "code": "package_tampered",
                "error": "downloaded bytes do not match sha256"
            })),
        )
            .into_response();
    }
    let staging = match write_upload_with_limit(&dir, bytes.as_ref()) {
        Ok(p) => p,
        Err(e) => return install_err_response(e),
    };
    let opts = InstallOptions {
        allow_unsigned: allow_unsigned(&state).await,
        origin: "url".into(),
    };
    let outcome = match install_app_package(&dir, &staging, &opts) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&staging);
            return install_err_response(e);
        }
    };
    let _ = std::fs::remove_file(&staging);
    reload_and_maybe_start(&state, &outcome.app_id).await;
    Json(serde_json::json!({
        "installed_version": outcome.installed_version,
        "fingerprint": outcome.fingerprint,
        "origin": outcome.origin,
        "app_id": outcome.app_id,
    }))
    .into_response()
}

async fn get_versions(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(dir) = data_dir_async(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gateway data_dir unavailable" })),
        )
            .into_response();
    };
    match list_versions(&dir, &id) {
        Ok((current, versions)) => Json(serde_json::json!({
            "current": current,
            "versions": versions,
        }))
        .into_response(),
        Err(e) => install_err_response(e),
    }
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    version: String,
}

async fn rollback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RollbackBody>,
) -> impl IntoResponse {
    let Some(dir) = data_dir_async(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gateway data_dir unavailable" })),
        )
            .into_response();
    };
    match rollback_app(&dir, &id, &body.version) {
        Ok(st) => {
            reload_and_maybe_start(&state, &id).await;
            Json(serde_json::json!({
                "id": id,
                "current": st.current,
            }))
            .into_response()
        }
        Err(e) => install_err_response(e),
    }
}

#[derive(Debug, Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    purge_data: bool,
}

async fn delete_app(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> impl IntoResponse {
    let Some(dir) = data_dir_async(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gateway data_dir unavailable" })),
        )
            .into_response();
    };
    crate::app::supervisor::stop_app(&id, &state.gateway_state).await;
    if let Err(e) = uninstall_app(&dir, &id, q.purge_data) {
        return install_err_response(e);
    }
    {
        let gw = state.gateway_state.read().await;
        if let Ok(mut reg) = gw.app_registry.write() {
            reg.reload(&dir);
        }
    }
    republish_catalog(&state);
    StatusCode::NO_CONTENT.into_response()
}
