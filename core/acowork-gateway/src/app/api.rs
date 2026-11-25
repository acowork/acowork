//! App discovery and lifecycle-control API (ADR-116).
//!
//! These are platform control-plane routes: they stay behind the Gateway's
//! bearer gate. The app-scoped traffic under `/api/apps/{id}/ui|api|session`
//! is a separate, ticket-authenticated channel owned by `app::proxy`.

use std::path::PathBuf;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
};
use acowork_core::app::MAX_APP_PACKAGE_BYTES;
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
        // Both install routes above answer `202 {job_id}` and do the work in a
        // detached task; these are the observation endpoints.
        .route("/api/apps/install-jobs", get(list_install_jobs))
        .route("/api/apps/install-jobs/{job_id}", get(get_install_job))
        .route("/api/apps/{id}", get(get_app).delete(delete_app))
        .route("/api/apps/{id}/enable", post(enable_app))
        .route("/api/apps/{id}/disable", post(disable_app))
        .route("/api/apps/{id}/session-ticket", post(session_ticket))
        .route("/api/apps/{id}/versions", get(get_versions))
        .route("/api/apps/{id}/rollback", post(rollback))
        // A `.app` is a whole app: the gateway-wide 64 MiB cap rejected every
        // package over that size (vectorcraft is 81 MiB) with a bare 413 the
        // browser reported as "Failed to fetch". The package limit is
        // `MAX_APP_PACKAGE_BYTES` (200 MiB, `app::install` enforces it while
        // streaming to the staging file), so the body cap has to match it —
        // otherwise the route's own limit is unreachable.
        .layer(DefaultBodyLimit::max(MAX_APP_PACKAGE_BYTES as usize))
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
    /// ADR-121 preview channel: `library` | `visual` | `none` | empty.
    pub canvas_kind: String,
    /// Template such as `/?file={workspace_file}` for `visual` apps.
    pub canvas_preview_path: String,
    /// Workspace file extensions this visual app owns (no leading dot).
    pub canvas_extensions: Vec<String>,
    pub nav_label: String,
    pub nav_icon: String,
    pub nav_order: i32,
    pub health_path: String,
    pub permissions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    /// Unix-second install stamp — the Desktop appends new app slots in
    /// install order, so a fresh install lands below the existing icons.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_at: Option<String>,
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
        canvas_kind: manifest.ui.canvas.kind.clone(),
        canvas_preview_path: manifest.ui.canvas.preview_path.clone(),
        canvas_extensions: manifest.ui.canvas.extensions.clone(),
        nav_label: manifest.ui.nav.label.clone(),
        nav_icon: manifest.ui.nav.icon.clone(),
        nav_order: manifest.ui.nav.order,
        health_path: manifest.process.health_path.clone(),
        permissions: manifest.permissions.platform.clone(),
        installed_version: install.as_ref().map(|s| s.current.clone()),
        installed_at: install.as_ref().map(|s| s.installed_at.clone()),
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

/// Query of `POST /api/apps/install` — the local-upload counterpart of
/// `InstallUrlBody::hint_id`.
#[derive(Debug, Deserialize)]
struct InstallHintQuery {
    #[serde(default)]
    hint_id: Option<String>,
}

/// Accept an uploaded `.app` and answer `202 {job_id}`.
///
/// The upload itself stays in-request (it is a loopback copy and the Desktop
/// already streams the file); everything after it — unpack, signature check,
/// manifest, binaries, spawn — is the slow part and now runs as the same kind
/// of job a gallery install uses, so both paths report progress identically.
async fn install_upload(
    State(state): State<AppState>,
    Query(query): Query<InstallHintQuery>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(dir) = data_dir_async(&state).await else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "gateway data_dir unavailable" })),
        )
            .into_response();
    };
    if let Some(job_id) = existing_job(&state, &query.hint_id) {
        return job_accepted(&state, &job_id);
    }
    let staging = match write_upload_with_limit(&dir, body.as_ref()) {
        Ok(p) => p,
        Err(e) => return install_err_response(e),
    };
    let total = body.len() as u64;
    let job_id = state.app_install_jobs.create(query.hint_id.clone());
    let task_state = state.clone();
    let jobs = state.app_install_jobs.clone();
    let spawned = job_id.clone();
    tokio::spawn(async move {
        jobs.update(&spawned, |j| {
            j.phase = crate::app::install_jobs::InstallPhase::Install;
            j.total_bytes = total;
            j.received_bytes = total;
        });
        let allow_unsigned = allow_unsigned(&task_state).await;
        let install_dir = dir.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            let opts = InstallOptions {
                allow_unsigned,
                origin: "upload".into(),
            };
            let outcome = install_app_package(&install_dir, &staging, &opts);
            let _ = std::fs::remove_file(&staging);
            outcome
        })
        .await;
        match outcome {
            Ok(Ok(o)) => {
                jobs.update(&spawned, |j| {
                    j.phase = crate::app::install_jobs::InstallPhase::Start;
                    j.app_id = Some(o.app_id.clone());
                });
                reload_and_maybe_start(&task_state, &o.app_id).await;
                jobs.finish_ok(&spawned, &o.app_id, &o.installed_version);
            }
            Ok(Err(e)) => jobs.finish_err(&spawned, e.code, &e.message),
            Err(e) => jobs.finish_err(
                &spawned,
                "package_malformed",
                &format!("install task: {e}"),
            ),
        }
    });
    job_accepted(&state, &job_id)
}

/// Body of `POST /api/apps/install-url`.
#[derive(Debug, Deserialize)]
struct InstallUrlBody {
    url: String,
    sha256: String,
    /// Card id the Desktop is installing (see [`InstallJob::hint_id`]).
    /// Display + single-flight key only; never used as the installed id.
    #[serde(default)]
    hint_id: Option<String>,
}

/// `InstallJob` fields the Desktop quotes in its own copy.
fn job_accepted(state: &AppState, job_id: &str) -> axum::response::Response {
    match state.app_install_jobs.get(job_id) {
        Some(job) => (StatusCode::ACCEPTED, Json(job)).into_response(),
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "install job vanished" })),
        )
            .into_response(),
    }
}

/// The job that already covers this card, or `None`. Single-flight: a second
/// click (or a re-click after switching panels) must not start a second
/// 17–77 MB download.
fn existing_job(state: &AppState, hint_id: &Option<String>) -> Option<String> {
    hint_id
        .as_deref()
        .and_then(|hint| state.app_install_jobs.running_for_hint(hint))
        .map(|job| job.job_id)
}

/// Accept an install from a gallery / Release URL and answer immediately.
///
/// It used to be a single blocking request: the Desktop held a 20–60 s POST
/// open with no progress to show, and the client's view-local spinner died the
/// moment the user switched panels. The response is now `202` + a job id;
/// `GET /api/apps/install-jobs/{job_id}` carries phase and byte progress.
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
    if let Some(job_id) = existing_job(&state, &body.hint_id) {
        return job_accepted(&state, &job_id);
    }
    let job_id = state.app_install_jobs.create(body.hint_id.clone());
    let task_state = state.clone();
    let jobs = state.app_install_jobs.clone();
    let (url, sha256) = (body.url, body.sha256);
    let spawned = job_id.clone();
    tokio::spawn(async move {
        match download_and_install(&task_state, &jobs, &spawned, &url, &sha256).await {
            Ok((app_id, version)) => jobs.finish_ok(&spawned, &app_id, &version),
            Err(e) => jobs.finish_err(&spawned, e.code, &e.message),
        }
    });
    job_accepted(&state, &job_id)
}

/// `GET /api/apps/install-jobs` — every known job, running first. The Desktop
/// refetches this on mount, which is what makes a running install survive
/// navigation away from the Apps panel.
async fn list_install_jobs(State(state): State<AppState>) -> impl IntoResponse {
    let jobs = state.app_install_jobs.list();
    Json(serde_json::json!({ "jobs": jobs }))
}

/// `GET /api/apps/install-jobs/{job_id}` — phase, byte progress and outcome.
async fn get_install_job(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> impl IntoResponse {
    match state.app_install_jobs.get(&job_id) {
        Some(job) => Json(job).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install job not found" })),
        )
            .into_response(),
    }
}

/// Download, verify and install one package, keeping `job_id` updated at every
/// step. Runs detached from the request that accepted it.
async fn download_and_install(
    state: &AppState,
    jobs: &crate::app::install_jobs::SharedInstallJobs,
    job_id: &str,
    url: &str,
    sha256: &str,
) -> Result<(String, String), crate::app::install::InstallError> {
    use crate::app::install::InstallError;
    use crate::app::install_jobs::InstallPhase;
    use sha2::{Digest, Sha256};

    let Some(dir) = data_dir_async(state).await else {
        return Err(InstallError::new(
            "gateway_unavailable",
            503,
            "gateway data_dir unavailable",
        ));
    };
    // No overall deadline tight enough to kill a legitimately slow download
    // (a 77 MB package through a debug build is minutes): the job is no longer
    // a request, so hanging is a visible stall in the byte counter rather than
    // a hung UI. The connect timeout still fails a dead host quickly.
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(900))
        .build()
        .map_err(|e| InstallError::new("package_malformed", 500, format!("http client: {e}")))?;
    let mut resp = client.get(url).send().await.map_err(|e| {
        InstallError::new("package_malformed", 400, format!("download failed: {e}"))
    })?;
    if !resp.status().is_success() {
        return Err(InstallError::new(
            "package_malformed",
            400,
            format!("download HTTP {}", resp.status()),
        ));
    }

    // The package is buffered whole: `install_app_package` streams it again
    // into its own staging copy, so a temp file here would only add a second
    // 77 MB write. Ceiling: peak RSS grows by one package (ADR-116 limit is
    // `MAX_APP_PACKAGE_BYTES`, 200 MB).
    let total = resp.content_length().unwrap_or(0);
    jobs.update(job_id, |j| {
        j.phase = InstallPhase::Download;
        j.total_bytes = total;
    });
    let mut received: u64 = 0;
    let mut body = Vec::with_capacity(total.min(MAX_APP_PACKAGE_BYTES) as usize);
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        InstallError::new("package_malformed", 400, format!("download body: {e}"))
    })? {
        received += chunk.len() as u64;
        if received > MAX_APP_PACKAGE_BYTES {
            return Err(InstallError::new(
                "package_too_large",
                413,
                format!(
                    "package exceeds {} MB limit",
                    MAX_APP_PACKAGE_BYTES / (1024 * 1024)
                ),
            ));
        }
        body.extend_from_slice(&chunk);
        jobs.update(job_id, |j| j.received_bytes = received);
    }

    jobs.update(job_id, |j| j.phase = InstallPhase::Verify);
    let digest = format!("{:x}", Sha256::digest(&body));
    if !digest.eq_ignore_ascii_case(sha256.trim()) {
        return Err(InstallError::new(
            "package_tampered",
            400,
            "downloaded bytes do not match sha256",
        ));
    }

    jobs.update(job_id, |j| j.phase = InstallPhase::Install);
    let allow_unsigned = allow_unsigned(state).await;
    let install_dir = dir.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let staging = write_upload_with_limit(&install_dir, body.as_slice())?;
        let opts = InstallOptions {
            allow_unsigned,
            origin: "url".into(),
        };
        let outcome = install_app_package(&install_dir, &staging, &opts);
        let _ = std::fs::remove_file(&staging);
        outcome
    })
    .await
    .map_err(|e| {
        InstallError::new("package_malformed", 500, format!("install task: {e}"))
    })??;

    jobs.update(job_id, |j| {
        j.phase = InstallPhase::Start;
        j.app_id = Some(outcome.app_id.clone());
    });
    reload_and_maybe_start(state, &outcome.app_id).await;
    Ok((outcome.app_id, outcome.installed_version))
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
    let pid = crate::app::supervisor::stop_app(&id, &state.gateway_state).await;
    if let Some(pid) = pid {
        crate::app::supervisor::wait_process_gone(pid, std::time::Duration::from_secs(5)).await;
    }
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
