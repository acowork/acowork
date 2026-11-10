//! Static file serving for workspace-backed assets (ADR-055 L2-7).
//!
//! The HTML preview iframe needs raw bytes (HTML / CSS / image binaries);
//! a JSON envelope would break every `<img>`, `<link>`, and `<script>` in
//! the rendered document. Under ADR-055 the Gateway no longer touches the
//! workspace filesystem (ADR-034 rule 3 — "Gateway does not access Agent
//! Runtime local files"), so these routes are now a **thin reverse proxy**
//! to the Runtime's `GET /workspaces/raw/{path}` endpoint, which serves
//! the raw bytes with the correct `Content-Type`.
//!
//! The Runtime is the authoritative owner of workspace config (it writes
//! `<work_dir>/config/agent_workspaces.json`), so `workspace_id`
//! resolution and the path-traversal guard both live on the Runtime side
//! (`RuntimeWorkspaceQueryService::resolve_within`). The Gateway merely
//! forwards the `workspace_id` and file path verbatim.

use axum::{
    Router,
    extract::{Path, State},
    http::HeaderMap,
    routing::get,
};

use crate::http::proxy::proxy_to_runtime_with_method;
use crate::http::routes::AppState;

/// Percent-encode each slash-delimited segment of a path so it can be
/// appended verbatim to the Runtime raw-endpoint URL (workspace filenames
/// may contain spaces / non-ASCII). Preserves `/` separators.
fn encode_path_segments(path: &str) -> String {
    path.split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// `GET /ws-files/{agent_id}/{*path}` — serve an agent-home file as a
/// static asset (legacy alias, reverse-proxied to the Runtime raw endpoint).
pub async fn serve_ws_file(
    State(state): State<AppState>,
    Path((agent_id, file_rel_path)): Path<(String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    let path = format!("/workspaces/raw/{}", encode_path_segments(&file_rel_path));
    proxy_to_runtime_with_method(
        &state,
        &agent_id,
        &path,
        "",
        reqwest::Method::GET,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy one workspace file's raw bytes to the caller.
///
/// The **single implementation** behind every static-asset entry point
/// (ADR-112 §决策 3). Callers differ only in how they authenticated the
/// request — a bearer header on the legacy `/workspace-files/…` route, a
/// capability ticket on `/workspace-preview/…` — so the byte path itself
/// (path encoding, `workspace_id` forwarding, Runtime-side traversal guard)
/// must not be written twice.
pub async fn proxy_workspace_file(
    state: &AppState,
    agent_id: &str,
    workspace_id: &str,
    file_rel_path: &str,
    headers: &HeaderMap,
) -> axum::response::Response {
    let path = format!("/workspaces/raw/{}", encode_path_segments(file_rel_path));
    let query = format!("workspace_id={}", urlencoding::encode(workspace_id));
    proxy_to_runtime_with_method(
        state,
        agent_id,
        &path,
        &query,
        reqwest::Method::GET,
        None,
        headers,
    )
    .await
}

/// `GET /workspace-files/{agent_id}/{workspace_id}/{*path}` — serve a
/// workspace file as a static asset (reverse-proxied to the Runtime raw
/// endpoint; `workspace_id` is forwarded for Runtime-side resolution).
///
/// Bearer-gated: this route is **not** in `is_public_path`. Browsers cannot
/// attach a token to a sub-resource load, so workspace HTML is served through
/// [`crate::http::workspace_preview`] instead (ADR-112); this route stays for
/// fetch-based callers (the Desktop's blob previews, Rust-side downloads).
pub async fn serve_workspace_ws_file(
    State(state): State<AppState>,
    Path((agent_id, workspace_id, file_rel_path)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    proxy_workspace_file(&state, &agent_id, &workspace_id, &file_rel_path, &headers).await
}

/// Static-asset routes for the workspace browser's HTML preview iframe.
///
/// Both routes are pure reverse proxies (ADR-055 L2-7): they forward the
/// `workspace_id` + file path to the Runtime, which resolves the workspace
/// root and streams the raw bytes back. No filesystem access happens on
/// the Gateway side.
pub fn workspace_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/workspace-files/{agent_id}/{workspace_id}/{*path}",
            get(serve_workspace_ws_file),
        )
        .route(
            "/ws-files/{agent_id}/{*path}",
            get(serve_ws_file),
        )
}
