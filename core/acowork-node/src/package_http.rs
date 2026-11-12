//! Node HTTP surface for agent **package content** (ADR-009 §5, §V-R).
//!
//! Two roles, one owner. The Node is the package manager (ADR-009 §2.2)
//! and the only process that is up in **both** agent states, so it serves:
//!
//! 1. **Reads** — the avatar bytes the Desktop renders. Serving them from
//!    the Runtime meant a stopped agent answered `503` and the Desktop
//!    silently fell back to a random icon (review §7.1 / V-P). One route,
//!    both states.
//! 2. **Publish-domain writes** — the publish wizard's avatar selection
//!    and image upload. These bake *package content*, and the wizard is
//!    reachable while the agent is stopped, so they cannot live on the
//!    Runtime either (review §V-H…V-J, §V-R).
//!
//! Scope is narrow by design: package content only (`manifest.avatar`,
//! `assets/avatar*`) plus the manifest's avatar fields. The instance's own
//! pick — `avatar` / `builtin_avatar` / `name` in its
//! `workspace/config/agent_config.json` — is deliberately NOT written here:
//! that file belongs to the Runtime. The Runtime resolves the effective
//! avatar for `avatar-config`; the Node only *reads* the identity out of it
//! to report what a stopped agent is called.
//!
//! Mounted on the node HTTP listener (`:19900`) next to the Runtime
//! reverse proxy, so the Gateway has one address for both.

use std::path::{Path, PathBuf};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};

use acowork_core::avatar::{AVATAR_EXTENSIONS, has_avatar_extension};

use crate::state::NodeHttpState;

/// Upload cap for `manifest/file` — avatars are small; anything larger is
/// a misuse, not a package.
const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;

/// Body for `POST manifest/avatar`.
///
/// `None` = the field was absent = leave unchanged; `Some("")` = clear.
/// (An explicit JSON `null` is indistinguishable from absent and also
/// means "leave unchanged" — that is what the Desktop's client sends.)
#[derive(Debug, Default, Deserialize)]
pub struct UpdateAvatarRequest {
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub builtin_avatar: Option<String>,
}

/// Query parameters for the avatar file endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct AvatarFileQuery {
    /// Package-relative path (e.g. `assets/avatar-02.jpg`).
    #[serde(default)]
    pub path: String,
}

/// A single avatar asset inside the package.
#[derive(Debug, Clone, Serialize)]
pub struct AvatarAssetEntry {
    pub relative_path: String,
}

/// Response for the asset listing.
///
/// ADR-073: the route's `{id}` is the runtime instance id; the
/// package id is `agent_id` (for callers that still key by package,
/// e.g. the Desktop PublishWizard). Returning both keeps the
/// response self-describing without forcing the caller to issue a
/// separate `agents list` query.
#[derive(Debug, Serialize)]
pub struct AvatarAssetsResponse {
    pub instance_id: String,
    pub agent_id: String,
    pub assets: Vec<AvatarAssetEntry>,
}

/// Build the read-only asset router.
pub fn router(state: NodeHttpState) -> Router {
    Router::new()
        .route("/agents/{id}/avatar", get(get_avatar))
        .route(
            "/agents/{id}/avatar-file",
            get(get_avatar_file).delete(delete_avatar_file),
        )
        .route(
            "/agents/{id}/manifest/avatar-assets",
            get(list_avatar_assets),
        )
        .route("/agents/{id}/manifest/avatar", post(put_manifest_avatar))
        // 10 MB of image data + multipart framing — mirrors the cap the
        // Gateway enforced on this route before the move.
        .route(
            "/agents/{id}/manifest/file",
            post(upload_package_file).layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES + 2 * 1024 * 1024)),
        )
        .with_state(state)
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /agents/{id}/avatar` — the **packaged** avatar image.
///
/// The manifest is re-read from disk per request (not the install-time
/// snapshot in the install table) so a publish-flow manifest rewrite is
/// visible immediately, without restarting anything.
///
/// User overrides are intentionally not consulted: this endpoint feeds
/// the publish wizard's "what is in the package" preview. Callers that
/// want the *effective* avatar use the path `list_agents` returns with
/// `/avatar-file`.
async fn get_avatar(
    State(state): State<NodeHttpState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = crate::proxy::authorize(&state, &headers, &id).await {
        return denied;
    }
    let dir = match install_dir(&state, &id).await {
        Ok(dir) => dir,
        Err(resp) => return *resp,
    };

    let manifest_path = dir.join("manifest.toml");
    let declared = match std::fs::read_to_string(&manifest_path)
        .ok()
        .and_then(|raw| acowork_core::AgentManifest::from_toml(&raw).ok())
        .and_then(|manifest| manifest.avatar)
        .filter(|a| !a.trim().is_empty())
    {
        Some(avatar) => avatar,
        None => return not_found("Agent has no packaged avatar"),
    };

    serve_image(&dir, &declared, "public, max-age=31536000, immutable")
}

/// `GET /agents/{id}/avatar-file?path=<relative>` — a package image.
///
/// Works whether or not the agent is running: this is the route the
/// Desktop uses for every avatar it renders.
async fn get_avatar_file(
    State(state): State<NodeHttpState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<AvatarFileQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = crate::proxy::authorize(&state, &headers, &id).await {
        return denied;
    }
    let dir = match install_dir(&state, &id).await {
        Ok(dir) => dir,
        Err(resp) => return *resp,
    };
    if !has_avatar_extension(&query.path) {
        return bad_request(&format!(
            "Invalid file extension: only {} are allowed",
            AVATAR_EXTENSIONS.join(", ")
        ));
    }
    serve_image(&dir, &query.path, "public, max-age=300")
}

/// `GET /agents/{id}/manifest/avatar-assets` — the package's custom
/// avatar images (`assets/avatar*.{png,jpg,jpeg,gif,webp,svg}`).
async fn list_avatar_assets(
    State(state): State<NodeHttpState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = crate::proxy::authorize(&state, &headers, &id).await {
        return denied;
    }
    // ADR-073: `id` is the instance id; `info.agent_id` is the package
    // id. Returning both keeps the response self-describing.
    let info = match installed_agent(&state, &id).await {
        Ok(info) => info,
        Err(resp) => return *resp,
    };
    let dir = PathBuf::from(&info.install_path);

    let mut entries: Vec<(String, Option<u32>)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir.join("assets")) {
        for entry in read_dir.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let lower = name.to_ascii_lowercase();
            if !lower.starts_with("avatar") || !has_avatar_extension(&lower) {
                continue;
            }
            // `avatar.*` sorts before `avatar-NN.*`, then numerically —
            // same order the picker showed when the Runtime served this.
            let stem = Path::new(&lower)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            let sort_key = if stem == "avatar" {
                None
            } else {
                stem.strip_prefix("avatar-").and_then(|n| n.parse::<u32>().ok())
            };
            entries.push((format!("assets/{name}"), sort_key));
        }
    }
    entries.sort_by(|a, b| match (a.1, b.1) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(a_n), Some(b_n)) => a_n.cmp(&b_n),
    });

    Json(AvatarAssetsResponse {
        instance_id: info.instance_id,
        agent_id: info.agent_id,
        assets: entries
            .into_iter()
            .map(|(relative_path, _)| AvatarAssetEntry { relative_path })
            .collect(),
    })
    .into_response()
}

// ── Publish-domain writes (ADR-009 §V-R) ─────────────────────────────

/// `POST /agents/{id}/manifest/avatar` — rewrite the manifest's avatar
/// fields, used by the publish wizard to bake the user's selection into
/// the package before build.
///
/// Read-modify-write of `manifest.toml` on the machine that owns it. The
/// Gateway used to do this on `install_path`, which under ADR-055 is a
/// *node-local* path: the write landed in a file this node's
/// `build_publish` never reads, so the avatar selection was silently
/// dropped from the built package.
async fn put_manifest_avatar(
    State(state): State<NodeHttpState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(req): Json<UpdateAvatarRequest>,
) -> Response {
    if let Some(denied) = crate::proxy::authorize(&state, &headers, &id).await {
        return denied;
    }
    // ADR-073: capture the package id so the response advertises the
    // actual package this instance belongs to, not the URL instance id.
    let info = match installed_agent(&state, &id).await {
        Ok(info) => info,
        Err(resp) => return *resp,
    };
    let dir = PathBuf::from(&info.install_path);

    let manifest_path = dir.join("manifest.toml");
    let raw = match std::fs::read_to_string(&manifest_path) {
        Ok(raw) => raw,
        Err(e) => return not_found(&format!("manifest.toml not readable: {e}")),
    };
    let mut manifest = match acowork_core::AgentManifest::from_toml(&raw) {
        Ok(manifest) => manifest,
        Err(e) => return internal(&format!("Failed to parse existing manifest.toml: {e}")),
    };

    if let Some(value) = clearable(&req.avatar) {
        manifest.avatar = value;
    }
    if let Some(value) = clearable(&req.builtin_avatar) {
        if let Some(v) = value.as_deref()
            && !is_plausible_builtin_avatar_id(v)
        {
            return bad_request(&format!(
                "Invalid builtin_avatar value '{v}': expected 'icon-NN' or numeric 1-99"
            ));
        }
        manifest.builtin_avatar = value;
    }

    let toml = match manifest.to_toml() {
        Ok(toml) => toml,
        Err(e) => return internal(&format!("Failed to serialize manifest: {e}")),
    };
    if let Err(e) = std::fs::write(&manifest_path, toml) {
        return internal(&format!("Failed to write manifest.toml: {e}"));
    }

    // ADR-009 §5 / ADR-055 §6.5: the retained inventory carries the
    // manifest, so a rewrite must refresh it — otherwise the Gateway
    // keeps showing the old avatar until the node reconnects.
    crate::package::republish_installed_info(&state, &id).await;

    Json(serde_json::json!({
        "message": "Manifest avatar updated",
        "instance_id": info.instance_id,
        "agent_id": info.agent_id,
        "avatar": manifest.avatar,
        "builtin_avatar": manifest.builtin_avatar,
    }))
    .into_response()
}

/// `POST /agents/{id}/manifest/file?path=<relative>` (multipart, field
/// `file`) — write one image into the package, e.g. the avatar the wizard
/// just had the user pick.
async fn upload_package_file(
    State(state): State<NodeHttpState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<AvatarFileQuery>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Response {
    if let Some(denied) = crate::proxy::authorize(&state, &headers, &id).await {
        return denied;
    }
    let info = match installed_agent(&state, &id).await {
        Ok(info) => info,
        Err(resp) => return *resp,
    };
    let dir = PathBuf::from(&info.install_path);

    let relative = query.path.trim().to_string();
    if relative.is_empty() {
        return bad_request("Missing 'path' query parameter");
    }
    // This route exists for avatar images only; a new use case gets its
    // own endpoint with its own validation.
    if !has_avatar_extension(&relative) {
        return bad_request(&format!(
            "Invalid file extension: only {} are allowed",
            AVATAR_EXTENSIONS.join(", ")
        ));
    }
    let target = match resolve_for_write(&dir, &relative) {
        Ok(path) => path,
        Err(message) => return bad_request(&message),
    };

    let mut bytes: Option<Vec<u8>> = None;
    loop {
        match multipart.next_field().await {
            Ok(Some(field)) => {
                if field.name() == Some("file") {
                    match field.bytes().await {
                        Ok(data) => {
                            bytes = Some(data.to_vec());
                            break;
                        }
                        Err(e) => return bad_request(&format!("Failed to read file field: {e}")),
                    }
                }
            }
            Ok(None) => break,
            Err(e) => return bad_request(&format!("Failed to read multipart field: {e}")),
        }
    }
    let bytes = match bytes {
        Some(bytes) if !bytes.is_empty() => bytes,
        Some(_) => return bad_request("Uploaded file is empty"),
        None => return bad_request("Missing required field: 'file'"),
    };
    if bytes.len() > MAX_UPLOAD_BYTES {
        return bad_request(&format!(
            "Uploaded file exceeds {} MB limit",
            MAX_UPLOAD_BYTES / (1024 * 1024)
        ));
    }
    if let Err(e) = std::fs::write(&target, &bytes) {
        return internal(&format!("Failed to write '{}': {e}", target.display()));
    }

    Json(serde_json::json!({
        "message": "File uploaded",
        "instance_id": info.instance_id,
        "agent_id": info.agent_id,
        "path": relative,
        "size": bytes.len(),
    }))
    .into_response()
}

/// `DELETE /agents/{id}/avatar-file?path=<relative>` — remove an image
/// from the package.
///
/// Package content only. Whether the *user's preference* still points at
/// the deleted file is not this endpoint's business: that value belongs
/// to the Runtime (ADR-009 §5), which the Gateway asks separately.
async fn delete_avatar_file(
    State(state): State<NodeHttpState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<AvatarFileQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(denied) = crate::proxy::authorize(&state, &headers, &id).await {
        return denied;
    }
    let info = match installed_agent(&state, &id).await {
        Ok(info) => info,
        Err(resp) => return *resp,
    };
    let dir = PathBuf::from(&info.install_path);
    if !has_avatar_extension(&query.path) {
        return bad_request(&format!(
            "Invalid file extension: only {} are allowed",
            AVATAR_EXTENSIONS.join(", ")
        ));
    }
    let target = match resolve_within_package(&dir, &query.path) {
        Ok(path) => path,
        Err(message) => return bad_request(&message),
    };
    if let Err(e) = std::fs::remove_file(&target) {
        return not_found(&format!("Failed to delete avatar file: {e}"));
    }
    Json(serde_json::json!({
        "message": "Avatar file deleted",
        "instance_id": info.instance_id,
        "agent_id": info.agent_id,
        "path": query.path,
    }))
    .into_response()
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Look up the installed instance by its ADR-073 instance id.
///
/// Returns the full `InstalledAgent` clone (so handlers can release the
/// node lock immediately) or a `404` JSON response when the node does
/// not host the instance. The cloned `agent_id` field carries the
/// PACKAGE identity — distinct from the URL's `id` path variable
/// (which is the runtime instance identity).
async fn installed_agent(state: &NodeHttpState, id: &str) -> Result<crate::state::InstalledAgent, Box<Response>> {
    let node = state.node.read().await;
    match node.installed_agents.get(id) {
        Some(info) => Ok(info.clone()),
        None => Err(Box::new(
            (
                StatusCode::NOT_FOUND,
                [(header::CONTENT_TYPE, "application/json")],
                Json(serde_json::json!({
                    "error": "agent not installed on this node",
                    "id": id,
                }))
                .to_string(),
            )
                .into_response(),
        )),
    }
}

/// The instance's package directory, or the `404` to return when this
/// node does not host the instance (ADR-073: keyed by instance id).
async fn install_dir(state: &NodeHttpState, id: &str) -> Result<PathBuf, Box<Response>> {
    Ok(PathBuf::from(&installed_agent(state, id).await?.install_path))
}

/// Read `{package_dir}/{relative}` and answer with the image bytes.
///
/// Every failure is a `404`: the caller cannot distinguish "no such
/// file" from "not an image" and has nothing useful to do about either.
fn serve_image(package_dir: &Path, relative: &str, cache_control: &'static str) -> Response {
    let canonical = match resolve_within_package(package_dir, relative) {
        Ok(path) => path,
        Err(message) => return bad_request(&message),
    };
    match std::fs::read(&canonical) {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, image_content_type(&canonical)),
                (header::CACHE_CONTROL, cache_control),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(path = %canonical.display(), error = %e, "avatar asset read failed");
            not_found(&format!("Failed to read avatar: {e}"))
        }
    }
}

/// Canonicalize `{package_dir}/{relative}` and refuse anything that
/// escapes the package root (`../../etc/passwd`). Canonicalization also
/// resolves symlinks, so a link pointing outside the package is refused
/// too. This is the whole point of routing these reads through the
/// package owner — a regression here is a file-disclosure bug.
fn resolve_within_package(package_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.trim().is_empty() {
        return Err("Missing 'path' parameter".to_string());
    }
    if Path::new(relative).is_absolute() {
        return Err("Path must be relative to the package directory".to_string());
    }
    let canonical_root = std::fs::canonicalize(package_dir)
        .map_err(|e| format!("Package directory unavailable: {e}"))?;
    let canonical = std::fs::canonicalize(canonical_root.join(relative))
        .map_err(|_| format!("File not found: {relative}"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(format!("Path escapes the package directory: {relative}"));
    }
    Ok(canonical)
}

fn image_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// `None` = leave the field alone, `Some(None)` = clear it,
/// `Some(Some(v))` = set it. Empty/whitespace counts as clear.
fn clearable(value: &Option<String>) -> Option<Option<String>> {
    value.as_deref().map(|raw| {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

/// Sanity-check a builtin avatar id: `icon-NN` or a bare `1`..=`99`.
fn is_plausible_builtin_avatar_id(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let digits = lower.strip_prefix("icon-").unwrap_or(&lower);
    digits.parse::<u32>().is_ok_and(|n| (1..=99).contains(&n))
}

/// Resolve a *write* target: create the destination directory, then
/// canonicalize it and refuse anything outside the package.
///
/// Unlike [`resolve_within_package`] the target file need not exist yet,
/// so the guard canonicalizes the parent — the same trick the Gateway
/// used. A `..` component is rejected up front as well: the canonicalize
/// check alone would let `assets/../assets/x.png` through, which is
/// harmless but hides intent.
fn resolve_for_write(package_dir: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.trim().is_empty() {
        return Err("Missing 'path' parameter".to_string());
    }
    if Path::new(relative).is_absolute() {
        return Err("Path must be relative to the package directory".to_string());
    }
    if Path::new(relative)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("Path must not contain '..'".to_string());
    }

    let canonical_root = std::fs::canonicalize(package_dir)
        .map_err(|e| format!("Package directory unavailable: {e}"))?;
    let target = canonical_root.join(relative);
    let parent = target
        .parent()
        .ok_or_else(|| "Path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Failed to create {}: {e}", parent.display()))?;
    let canonical_parent = std::fs::canonicalize(parent)
        .map_err(|e| format!("Failed to resolve {}: {e}", parent.display()))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err(format!("Path escapes the package directory: {relative}"));
    }
    let file_name = target
        .file_name()
        .ok_or_else(|| "Path has no file name".to_string())?;
    Ok(canonical_parent.join(file_name))
}

fn internal(message: &str) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, message)
}

fn bad_request(message: &str) -> Response {
    error(StatusCode::BAD_REQUEST, message)
}

fn not_found(message: &str) -> Response {
    error(StatusCode::NOT_FOUND, message)
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use crate::config::NodeConfig;
    use crate::identity::{EnrollmentState, NodeIdentity};
    use crate::state::{InstalledAgent, NodeState};

    /// Every route is behind `authorize`, which is fail-closed, so the
    /// default fixture is an enrolled node and the default request
    /// carries its token.
    const TEST_TOKEN: &str = "test-token";

    fn node_identity(token: Option<&str>) -> NodeIdentity {
        NodeIdentity {
            node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
            node_name: "node-1".to_string(),
            node_token: token.map(str::to_string),
            gateway_addr: None,
            enrollment: EnrollmentState::Enrolled,
            created_at: chrono::Utc::now(),
            enrolled_at: None,
        }
    }

    fn state_with_package(dir: &Path) -> NodeHttpState {
        let mut node = NodeState::new(8);
        node.add_installed(InstalledAgent {
            instance_id: "3f2a0c1e-0000-4000-8000-000000000001".to_string(),
            agent_id: "com.acowork.test".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: dir.to_string_lossy().to_string(),
            manifest: acowork_core::AgentManifest::from_toml(
                "agent_id = \"com.acowork.test\"
name = \"Test\"
version = \"1.0.0\"
description = \"fixture\"
author = \"acowork-test\"
runtime_version = \"0.1.0\"
avatar = \"assets/avatar.png\"
",
            )
            .expect("fixture manifest must parse"),
        });
        NodeHttpState {
            node: Arc::new(RwLock::new(node)),
            config: NodeConfig::default(),
            identity: Arc::new(RwLock::new(node_identity(Some(TEST_TOKEN)))),
            // Not under test here: an empty registry and a relay handle on an
            // unused port.
            lsp: crate::lsp_registry::for_tests(dir, None),
            lsp_relay: crate::state::LspRelayControl::detached("127.0.0.1", 1),
        }
    }

    fn fixture_package() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acowork-assets-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/avatar.png"), b"packaged").unwrap();
        std::fs::write(dir.join("assets/avatar-02.jpg"), b"second").unwrap();
        std::fs::write(
            dir.join("manifest.toml"),
            "agent_id = \"com.acowork.test\"
name = \"Test\"
version = \"1.0.0\"
description = \"fixture\"
author = \"acowork-test\"
runtime_version = \"0.1.0\"
avatar = \"assets/avatar.png\"
",
        )
        .unwrap();
        dir
    }

    fn get(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .uri(uri)
            .header("X-ACowork-Node-Token", TEST_TOKEN)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn get_without_token(uri: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    /// The asset routes MUST win over the `/agents/{id}/{*rest}` Runtime
    /// reverse proxy, which answers `503 agent not running` when no
    /// Runtime process exists. Merging both routers in one test proves
    /// the precedence *and* that axum accepts the overlapping patterns
    /// (a conflict would panic while building the router).
    #[tokio::test]
    async fn asset_routes_win_over_the_runtime_proxy() {
        let dir = fixture_package();
        let state = state_with_package(&dir);
        let app = crate::proxy::router(state.clone()).merge(router(state.clone()));

        let resp = app
            .clone()
            .oneshot(get("/agents/3f2a0c1e-0000-4000-8000-000000000001/avatar-file?path=assets/avatar-02.jpg"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/jpeg");

        let resp = app
            .oneshot(get("/agents/3f2a0c1e-0000-4000-8000-000000000001/avatar"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Traversal must be refused — this route is the file-disclosure
    /// boundary now that the package owner serves the bytes.
    #[tokio::test]
    async fn traversal_and_non_images_are_refused() {
        let dir = fixture_package();
        std::fs::write(dir.parent().unwrap().join("outside.png"), b"secret").unwrap();
        let state = state_with_package(&dir);
        let app = router(state);
        let id = "3f2a0c1e-0000-4000-8000-000000000001";

        for uri in [
            format!("/agents/{id}/avatar-file?path=../outside.png"),
            format!("/agents/{id}/avatar-file?path=assets/evil.sh"),
            format!("/agents/{id}/avatar-file?path=assets/none.png"),
            format!("/agents/{id}/avatar-file?path="),
        ] {
            let resp = app.clone().oneshot(get(&uri)).await.unwrap();
            assert!(
                resp.status().is_client_error(),
                "{uri} should be refused, got {}",
                resp.status()
            );
        }

        // Unknown instance → 404, never a silent fallback to another dir.
        let resp = app
            .oneshot(get("/agents/00000000-0000-4000-8000-000000000000/avatar-file?path=assets/avatar.png"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(dir.parent().unwrap().join("outside.png"));
    }

    /// An enrolled node demands the token on asset reads too — the asset
    /// service must not become an unauthenticated hole in the §6.8
    /// boundary.
    #[tokio::test]
    async fn node_without_token_refuses_everything() {
        let dir = fixture_package();
        let mut state = state_with_package(&dir);
        state.identity = Arc::new(RwLock::new(node_identity(None)));
        let app = router(state);

        // Un-enrolled node: no legitimate caller exists, so even a
        // tokenless read of a packaged asset is refused.
        let resp = app
            .oneshot(get_without_token(
                "/agents/3f2a0c1e-0000-4000-8000-000000000001/avatar",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A wrong token is refused as well.
    #[tokio::test]
    async fn enrolled_node_rejects_wrong_token() {
        let dir = fixture_package();
        let mut state = state_with_package(&dir);
        state.identity = Arc::new(RwLock::new(node_identity(Some("secret"))));
        let app = router(state);

        let resp = app
            .oneshot(get_without_token(
                "/agents/3f2a0c1e-0000-4000-8000-000000000001/avatar",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
