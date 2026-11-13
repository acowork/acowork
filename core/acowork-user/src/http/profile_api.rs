//! User profile HTTP API handlers
//!
//! - GET  /api/users                  — list all user profiles
//! - POST /api/users                  — create a new user profile
//! - PUT  /api/users/{user_id}        — update a user profile
//! - POST /api/users/{user_id}/activate — switch active user

use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Multipart, Path, Query, State},
    http::{StatusCode, header},
    response::Response,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::service::ProfilePatch;
use crate::auth::issuer::now_unix;
use crate::http::account_api::{blocking, sync_profiles};
use crate::auth::context::AuthContext;
use crate::error::ApiError;
use crate::state::AppState;
use crate::types::OperationAck;
use acowork_core::operation::{OperationRecord, OperationState};
use acowork_core::protocol::UserProfile;

/// Build the account-backed users router (ADR-114: the account system is
/// always on). `user_profiles.json` remains only as a derived presentation
/// view owned by [`crate::http::account_api::sync_profiles`].
pub fn users_routes() -> Router<AppState> {
    Router::new()
        .route("/api/users", get(list_users).post(create_user))
        .route("/api/users/{user_id}", put(update_user))
        .route("/api/users/{user_id}/activate", post(activate_user))
}

/// Avatar endpoints — mode-independent (ADR-076 keeps them on the
/// presentation cache in both modes; `account_api` re-derives that cache).
pub fn user_avatar_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/user/avatar-config",
            get(get_user_avatar_config).put(update_user_avatar_config),
        )
        .route("/api/user/avatar-assets", get(list_user_avatar_assets))
        .route(
            "/api/user/avatar-file",
            get(get_user_avatar_file)
                .post(upload_user_avatar_file)
                .delete(delete_user_avatar_file),
        )
}

// ── Request types ──────────────────────────────────────────────────────

/// Request body for creating a new user
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub display_name: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub occupation: Option<String>,
    #[serde(default)]
    pub communication_style: Option<String>,
    #[serde(default)]
    pub custom: std::collections::HashMap<String, String>,
    /// ADR-059 §7.3: the BootstrapState `version` the client read
    /// before writing (optimistic concurrency — stale clients are
    /// rejected with `resource_version_conflict`). Absent → no
    /// precondition.
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// Request body for updating a user profile (all fields optional — merge)
#[derive(Debug, Deserialize, Default)]
pub struct UpdateUserRequest {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub occupation: Option<String>,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub builtin_avatar: Option<String>,
    #[serde(default)]
    pub communication_style: Option<String>,
    #[serde(default)]
    pub custom: Option<std::collections::HashMap<String, String>>,
}

// ── Response types ─────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct UserListResponse {
    pub users: Vec<UserProfile>,
    pub version: u64,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub user: UserProfile,
    pub version: u64,
}

#[derive(Debug, Serialize)]
pub struct ActivateResponse {
    pub active_user_id: String,
    pub version: u64,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The service data directory.
///
/// Was a `GatewayConfig` lookup; the service owns its data directory directly
/// now (ADR-084 §3 goal 2), so the accessor only has to unwrap the lock.
async fn get_data_dir(state: &AppState) -> std::path::PathBuf {
    state.shared.read().await.data_dir.clone()
}

// ── Handlers ───────────────────────────────────────────────────────────

/// `GET /api/users` — list all user profiles
pub async fn list_users(State(state): State<AppState>) -> Result<Json<UserListResponse>, ApiError> {
    let gw = state.shared.read().await;
    let list = gw.resource_cache.user_profile_list.clone();
    Ok(Json(UserListResponse {
        users: list.users,
        version: list.version,
    }))
}

/// `POST /api/users` — create a new user profile
///
/// Generates a UUID v4, sets is_active=true (deactivates others),
/// bumps version, saves to disk, and hot-pushes to all running agents.
pub async fn create_user(
    State(state): State<AppState>,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<OperationAck>), ApiError> {
    // ADR-059 §7.3: reject stale writers before touching the profile list.
    crate::state::check_expected_version(&state, req.expected_version).await?;

    let now = now_iso();
    let user_id = Uuid::new_v4().to_string();

    let language = req.language.unwrap_or_else(|| "en-US".to_string());
    let timezone = req.timezone.unwrap_or_else(|| "UTC".to_string());

    let profile = UserProfile {
        user_id,
        display_name: req.display_name,
        language,
        timezone,
        city: req.city,
        country: req.country,
        occupation: req.occupation,
        avatar: None,
        builtin_avatar: None,
        communication_style: req.communication_style,
        custom: req.custom,
        created_at: now.clone(),
        updated_at: now,
        is_active: true,
    };

    // Update state: deactivate all others, add new user
    {
        let mut gw = state.shared.write().await;
        // Deactivate all existing users
        for u in &mut gw.resource_cache.user_profile_list.users {
            u.is_active = false;
        }
        // Add new active user
        gw.resource_cache
            .user_profile_list
            .users
            .push(profile.clone());
    }

    // Bump version, save to disk
    {
        let mut gw = state.shared.write().await;
        crate::profiles::rebuild_and_save_user_profile_cache(&mut gw);
    }

    // ADR-084 M3: profile-change signal to the Gateway goes here.

    let version = {
        let gw = state.shared.read().await;
        gw.resource_cache.user_profile_list.version
    };

    tracing::info!(
        user_id = %profile.user_id,
        display_name = %profile.display_name,
        version = version,
        "User profile created"
    );

    // ADR-059 §6: open a committed operation record so the client can
    // correlate this mutation by `operation_id` and observe the
    // resulting `resource_version`. The side effect (profile list +
    // disk) already completed synchronously.
    let mut record = OperationRecord::new(req.expected_version.unwrap_or(0));
    record.state = OperationState::Committed;
    record.resource_version = Some(version);
    let ack = OperationAck::from_record(&record);

    Ok((StatusCode::CREATED, Json(ack)))
}

/// `PUT /api/users/{user_id}` — update a user profile
///
/// Merges provided fields (None = keep existing), updates `updated_at`,
/// bumps version, saves to disk, and hot-pushes if the active user changed.
pub async fn update_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<UserResponse>, ApiError> {
    let updated_profile = {
        let mut gw = state.shared.write().await;
        let users = &mut gw.resource_cache.user_profile_list.users;
        let idx = users
            .iter()
            .position(|u| u.user_id == user_id)
            .ok_or_else(|| ApiError::not_found(&format!("User not found: {}", user_id)))?;

        let user = &mut users[idx];
        if let Some(name) = req.display_name {
            user.display_name = name;
        }
        if let Some(lang) = req.language {
            user.language = lang;
        }
        if let Some(tz) = req.timezone {
            user.timezone = tz;
        }
        if let Some(city) = req.city {
            user.city = Some(city);
        }
        if let Some(country) = req.country {
            user.country = Some(country);
        }
        if let Some(occ) = req.occupation {
            user.occupation = Some(occ);
        }
        if let Some(avatar) = req.avatar {
            user.avatar = if avatar.is_empty() {
                None
            } else {
                Some(avatar)
            };
        }
        if let Some(builtin) = req.builtin_avatar {
            user.builtin_avatar = if builtin.is_empty() {
                None
            } else {
                Some(builtin)
            };
        }
        if let Some(style) = req.communication_style {
            user.communication_style = Some(style);
        }
        if let Some(custom) = req.custom {
            user.custom = custom;
        }
        user.updated_at = now_iso();
        user.clone()
    };

    // Bump version, save to disk
    {
        let mut gw = state.shared.write().await;
        crate::profiles::rebuild_and_save_user_profile_cache(&mut gw);
    }

    // ADR-084 M3: profile-change signal to the Gateway goes here.

    let version = {
        let gw = state.shared.read().await;
        gw.resource_cache.user_profile_list.version
    };

    tracing::info!(
        user_id = %user_id,
        version = version,
        "User profile updated"
    );

    Ok(Json(UserResponse {
        user: updated_profile,
        version,
    }))
}

/// `POST /api/users/{user_id}/activate` — switch active user
///
/// Deactivates all users, activates the specified one, bumps version,
/// saves to disk, and hot-pushes to all running agents.
pub async fn activate_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<ActivateResponse>, ApiError> {
    // Update state: deactivate all, activate target
    {
        let mut gw = state.shared.write().await;
        let users = &mut gw.resource_cache.user_profile_list.users;

        // Verify user exists
        if !users.iter().any(|u| u.user_id == user_id) {
            return Err(ApiError::not_found(&format!("User not found: {}", user_id)));
        }

        // Deactivate all
        for u in users.iter_mut() {
            u.is_active = false;
        }
        // Activate target
        if let Some(target) = users.iter_mut().find(|u| u.user_id == user_id) {
            target.is_active = true;
            target.updated_at = now_iso();
        }
    }

    // Bump version, save to disk
    {
        let mut gw = state.shared.write().await;
        crate::profiles::rebuild_and_save_user_profile_cache(&mut gw);
    }

    // ADR-084 M3: profile-change signal to the Gateway goes here.

    let version = {
        let gw = state.shared.read().await;
        gw.resource_cache.user_profile_list.version
    };

    tracing::info!(
        active_user_id = %user_id,
        version = version,
        "Active user switched"
    );

    Ok(Json(ActivateResponse {
        active_user_id: user_id,
        version,
    }))
}

// ── User Avatar API ───────────────────────────────────────────────────

/// Response for GET /api/user/avatar-config
#[derive(Debug, Serialize)]
pub struct UserAvatarConfigResponse {
    pub avatar: Option<String>,
    pub builtin_avatar: Option<String>,
}

/// Request for PUT /api/user/avatar-config
#[derive(Debug, Deserialize)]
pub struct UpdateUserAvatarConfigRequest {
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub builtin_avatar: Option<String>,
}

/// Response for GET /api/user/avatar-assets
#[derive(Debug, Serialize)]
pub struct UserAvatarAssetsResponse {
    pub assets: Vec<UserAvatarAssetEntry>,
}

#[derive(Debug, Serialize)]
pub struct UserAvatarAssetEntry {
    pub relative_path: String,
}

/// Query params for avatar-file endpoint
#[derive(Debug, Deserialize)]
pub struct UserAvatarFileQuery {
    pub path: String,
}

/// Check if a filename has an allowed image extension.
fn has_image_extension(path: &str) -> bool {
    let allowed = ["png", "jpg", "jpeg", "gif", "webp", "svg"];
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| allowed.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Compute the next available avatar-XX filename in `dir`.
fn next_avatar_name(dir: &std::path::Path, ext: &str) -> String {
    let mut used = std::collections::BTreeSet::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(cap) = name.strip_prefix("avatar-")
                && let Some(num) = cap.split('.').next()
                && let Ok(n) = num.parse::<u32>()
            {
                used.insert(n);
            }
        }
    }
    let mut n = 1u32;
    while used.contains(&n) {
        n = n.saturating_add(1);
    }
    format!("avatar-{:02}.{}", n, ext)
}

/// Resolve the avatar asset directory for a request.
///
/// Under `multi_user` each account owns `assets/avatars/{user_id}/` —
/// uploads land there and deletes are confined to it (ADR-076 §5.5);
/// under `local` there is a single user and the legacy shared `assets/`
/// root is kept. Returns the directory plus the relative path prefix
/// used to build the stored `avatar` field.
fn avatar_target_dir(
    data_dir: &std::path::Path,
    auth: Option<&AuthContext>,
) -> Result<(std::path::PathBuf, String), ApiError> {
    let Some(ctx) = auth else {
        return Ok((data_dir.join("assets"), "assets".to_string()));
    };
    // The user id becomes a path component. It comes from our own signed
    // token, but a path boundary stays a path boundary — reject anything
    // that could escape the namespace instead of trusting the claim.
    if !is_safe_user_id(&ctx.user_id) {
        return Err(ApiError::bad_request("invalid user id"));
    }
    let prefix = format!("assets/avatars/{}", ctx.user_id);
    Ok((data_dir.join(&prefix), prefix))
}

/// A user id is used as a directory name; keep it to the token-minted
/// shape (alnum / `-` / `_`) so it can never traverse out of the
/// namespace.
fn is_safe_user_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `GET /api/user/avatar-config` — get the caller's avatar config.
///
/// Under `multi_user` the authority is the caller's `UserAccount`
/// (ADR-076 §5.5); under `local` it is the single active profile.
pub async fn get_user_avatar_config(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
) -> Result<Json<UserAvatarConfigResponse>, ApiError> {
    let svc = state.auth_service.clone();
    if let Some(ctx) = auth.map(|Extension(c)| c) {
        let uid = ctx.user_id;
        let account = blocking(move || svc.account(&uid)).await?;
        return Ok(Json(UserAvatarConfigResponse {
            avatar: account.avatar,
            builtin_avatar: account.builtin_avatar,
        }));
    }

    let gw = state.shared.read().await;
    let active = gw
        .resource_cache
        .user_profile_list
        .users
        .iter()
        .find(|u| u.is_active)
        .cloned();
    match active {
        Some(user) => Ok(Json(UserAvatarConfigResponse {
            avatar: user.avatar,
            builtin_avatar: user.builtin_avatar,
        })),
        None => Ok(Json(UserAvatarConfigResponse {
            avatar: None,
            builtin_avatar: None,
        })),
    }
}

/// `PUT /api/user/avatar-config` — update the caller's avatar config.
///
/// Under `multi_user` this writes through `accounts.json` (the authority)
/// for the authenticated caller: the derived `user_profiles.json` is
/// rebuilt from it on the next account mutation, so writing the view
/// directly would lose the change (ADR-076 §5.5).
pub async fn update_user_avatar_config(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Json(req): Json<UpdateUserAvatarConfigRequest>,
) -> Result<Json<UserAvatarConfigResponse>, ApiError> {
    let svc = state.auth_service.clone();
    if let Some(ctx) = auth.clone().map(|Extension(c)| c) {
        let patch = ProfilePatch {
            avatar: req.avatar,
            builtin_avatar: req.builtin_avatar,
            ..Default::default()
        };
        let uid = ctx.user_id;
        let account = blocking(move || svc.update_account(&uid, patch, now_unix())).await?;
        sync_profiles(&state).await;
        return Ok(Json(UserAvatarConfigResponse {
            avatar: account.avatar,
            builtin_avatar: account.builtin_avatar,
        }));
    }

    {
        let mut gw = state.shared.write().await;
        let users = &mut gw.resource_cache.user_profile_list.users;
        let idx = users
            .iter()
            .position(|u| u.is_active)
            .ok_or_else(|| ApiError::not_found("No active user found"))?;

        let user = &mut users[idx];
        if let Some(av) = req.avatar {
            user.avatar = if av.is_empty() { None } else { Some(av) };
        }
        if let Some(bi) = req.builtin_avatar {
            user.builtin_avatar = if bi.is_empty() { None } else { Some(bi) };
        }
        user.updated_at = now_iso();
        user.is_active
    };

    // Bump version, save to disk
    {
        let mut gw = state.shared.write().await;
        crate::profiles::rebuild_and_save_user_profile_cache(&mut gw);
    }

    // ADR-084 M3: profile-change signal to the Gateway goes here.

    // Return updated config
    get_user_avatar_config(State(state), auth).await
}

/// `GET /api/user/avatar-assets` — list the caller's avatar files.
///
/// Under `multi_user` this is the caller's own namespace; under `local`
/// the shared `assets/` root.
pub async fn list_user_avatar_assets(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
) -> Result<Json<UserAvatarAssetsResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await;
    let (assets_dir, rel_prefix) =
        avatar_target_dir(&data_dir, auth.as_ref().map(|Extension(c)| c))?;
    let mut entries = Vec::new();

    if let Ok(dir) = std::fs::read_dir(&assets_dir) {
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().to_string();
            if name.starts_with("avatar-") && has_image_extension(&name) {
                entries.push(UserAvatarAssetEntry {
                    relative_path: format!("{rel_prefix}/{name}"),
                });
            }
        }
    }

    entries.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(Json(UserAvatarAssetsResponse { assets: entries }))
}

/// `GET /api/user/avatar-file?path=<relative>` — serve avatar file bytes.
pub async fn get_user_avatar_file(
    State(state): State<AppState>,
    Query(query): Query<UserAvatarFileQuery>,
) -> Result<Response<Body>, ApiError> {
    // Path traversal guard: only allow "assets/..." paths
    let relative = query.path.trim_start_matches('/');
    if !relative.starts_with("assets/") {
        return Err(ApiError::bad_request("Invalid path: must be under assets/"));
    }
    if relative.contains("..") {
        return Err(ApiError::bad_request(
            "Invalid path: path traversal detected",
        ));
    }

    let data_dir = get_data_dir(&state).await;
    let canonical = data_dir.join(relative);

    if !has_image_extension(&query.path) {
        return Err(ApiError::bad_request(
            "Invalid file extension: only png, jpg, jpeg, gif, webp, svg are allowed",
        ));
    }

    let bytes = std::fs::read(&canonical)
        .map_err(|e| ApiError::not_found(&format!("Failed to read avatar file: {}", e)))?;

    let content_type = match std::path::Path::new(&query.path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "public, max-age=300")
        .body(Body::from(bytes))
        .unwrap())
}

/// `POST /api/user/avatar-file` — upload a new avatar file.
///
/// Returns the generated relative path — `assets/avatars/{uid}/avatar-01.png`
/// under `multi_user` (the caller's own namespace), `assets/avatar-01.png`
/// under `local`.
pub async fn upload_user_avatar_file(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    mut multipart: Multipart,
) -> Result<Json<UserAvatarAssetEntry>, ApiError> {
    let data_dir = get_data_dir(&state).await;
    let (assets_dir, rel_prefix) =
        avatar_target_dir(&data_dir, auth.as_ref().map(|Extension(c)| c))?;
    std::fs::create_dir_all(&assets_dir)
        .map_err(|e| ApiError::internal(&format!("Failed to create assets dir: {}", e)))?;

    let mut bytes: Option<Vec<u8>> = None;
    let mut ext = String::from("png");

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(&format!("Failed to read multipart field: {}", e)))?
    {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let file_name = field.file_name().unwrap_or("avatar.png").to_string();
            if let Some(e) = std::path::Path::new(&file_name)
                .extension()
                .and_then(|e| e.to_str())
            {
                ext = e.to_lowercase();
            }
            let data = field
                .bytes()
                .await
                .map_err(|e| ApiError::bad_request(&format!("Failed to read file bytes: {}", e)))?;
            bytes = Some(data.to_vec());
        }
    }

    let bytes = bytes.ok_or_else(|| ApiError::bad_request("No file uploaded"))?;

    // Validate extension
    if !has_image_extension(&format!("x.{}", ext)) {
        return Err(ApiError::bad_request(&format!(
            "Invalid file extension: {}; only png, jpg, jpeg, gif, webp, svg are allowed",
            ext
        )));
    }

    let name = next_avatar_name(&assets_dir, &ext);
    let target = assets_dir.join(&name);
    std::fs::write(&target, &bytes)
        .map_err(|e| ApiError::internal(&format!("Failed to write avatar file: {}", e)))?;

    let relative_path = format!("{rel_prefix}/{name}");
    tracing::info!(path = %relative_path, "User avatar file uploaded");
    Ok(Json(UserAvatarAssetEntry { relative_path }))
}

/// `DELETE /api/user/avatar-file?path=<relative>` — delete an avatar file.
///
/// If the deleted file was the caller's current avatar (multi_user) or the
/// active user's (local), that field is cleared.
pub async fn delete_user_avatar_file(
    State(state): State<AppState>,
    auth: Option<Extension<AuthContext>>,
    Query(query): Query<UserAvatarFileQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Path traversal guard
    let relative = query.path.trim_start_matches('/');
    if !relative.starts_with("assets/") || relative.contains("..") {
        return Err(ApiError::bad_request("Invalid path"));
    }

    // multi_user: avatar files are per-user — only the owner may unlink.
    // The guard runs *before* the unlink: refusing after the delete would
    // still have destroyed the file. An admin removing someone else's file
    // goes through `PUT /api/users/{id}` first (un-reference, then delete).
    {
        let ctx = auth
            .as_ref()
            .map(|Extension(c)| c)
            .ok_or_else(|| ApiError::unauthorized("authentication required"))?;
        let own = format!("assets/avatars/{}/", ctx.user_id);
        if !relative.starts_with(&own) {
            return Err(ApiError::forbidden(
                "avatar files are per-user; only the owner may delete them",
            ));
        }
    }

    let data_dir = get_data_dir(&state).await;
    let canonical = data_dir.join(relative);
    std::fs::remove_file(&canonical)
        .map_err(|e| ApiError::internal(&format!("Failed to delete avatar file: {}", e)))?;

    // multi_user: clear the caller's own field through the authority.
    let svc = state.auth_service.clone();
    if let Some(ctx) = auth.map(|Extension(c)| c) {
        let uid = ctx.user_id;
        let path = query.path.clone();
        let cleared = blocking(move || {
            if svc.account(&uid)?.avatar.as_deref() != Some(path.as_str()) {
                return Ok(false);
            }
            let patch = ProfilePatch {
                avatar: Some(String::new()),
                ..Default::default()
            };
            svc.update_account(&uid, patch, now_unix()).map(|_| true)
        })
        .await?;
        if cleared {
            sync_profiles(&state).await;
        }
        return Ok(Json(serde_json::json!({
            "message": "Avatar file deleted",
            "path": query.path,
        })));
    }

    // If it was the active user's avatar, clear it
    {
        let mut gw = state.shared.write().await;
        let users = &mut gw.resource_cache.user_profile_list.users;
        if let Some(user) = users.iter_mut().find(|u| u.is_active)
            && user.avatar.as_deref() == Some(query.path.as_str())
        {
            user.avatar = None;
            user.updated_at = now_iso();
        }
    }

    // Bump version, save to disk
    {
        let mut gw = state.shared.write().await;
        crate::profiles::rebuild_and_save_user_profile_cache(&mut gw);
    }

    tracing::info!(path = %query.path, "User avatar file deleted");
    Ok(Json(serde_json::json!({
        "message": "Avatar file deleted",
        "path": query.path,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_users_routes_builds() {
        let _router = users_routes();
    }

    /// ADR-076 §5.5: avatar assets are per-user under `multi_user` and
    /// stay in the legacy shared root under `local`; a user id that could
    /// escape its namespace is refused at the boundary.
    #[test]
    fn avatar_target_dir_is_per_user_under_multi_user_and_shared_under_local() {
        let data_dir = std::env::temp_dir().join("acowork-avatar-dir-test");
        let ctx = |uid: &str| AuthContext {
            user_id: uid.to_string(),
            role: acowork_core::account::Role::User,
            as_user: None,
        };

        let (dir, prefix) = avatar_target_dir(&data_dir, Some(&ctx("u-bob"))).unwrap();
        assert_eq!(dir, data_dir.join("assets/avatars/u-bob"));
        assert_eq!(prefix, "assets/avatars/u-bob");

        let (dir, prefix) = avatar_target_dir(&data_dir, None).unwrap();
        assert_eq!(dir, data_dir.join("assets"));
        assert_eq!(prefix, "assets");

        assert!(avatar_target_dir(&data_dir, Some(&ctx("../u-evil"))).is_err());
        assert!(avatar_target_dir(&data_dir, Some(&ctx("u/bob"))).is_err());
        assert!(avatar_target_dir(&data_dir, Some(&ctx(""))).is_err());
    }

    #[test]
    fn next_avatar_name_scans_the_target_directory() {
        let dir = std::env::temp_dir().join("acowork-avatar-next-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(next_avatar_name(&dir, "png"), "avatar-01.png");
        std::fs::write(dir.join("avatar-01.png"), b"x").unwrap();
        assert_eq!(next_avatar_name(&dir, "png"), "avatar-02.png");
    }
}
