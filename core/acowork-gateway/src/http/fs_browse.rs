//! Remote filesystem browsing API
//!
//! Provides a directory listing endpoint for remote Desktop ↔ Gateway scenarios.
//! When the Desktop App connects to a remote Gateway, it cannot use Tauri's
//! native file dialog to browse the remote server's filesystem. This API
//! enables the frontend to browse the server's directory tree remotely.
//!
//! Security considerations:
//! - Only directory listing is allowed (no file content access)
//! - Hidden files/dirs (starting with '.') are skipped
//! - Path traversal (..) is rejected
//! - Absolute paths on Windows that start with a drive letter are accepted
//! - Root-level browsing returns common starting points (home, root, common paths)

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::http::routes::{ApiError, AppState};

/// Query parameters for filesystem browsing
#[derive(Debug, Deserialize, Default)]
pub struct FsBrowseQuery {
    /// Directory path to browse. Empty or "/" = root (returns home + common dirs).
    #[serde(default)]
    pub path: Option<String>,
    /// Target node to browse (ADR-055 L7-1). Empty / "local" = the
    /// Gateway machine's filesystem; any other value = a remote node's
    /// filesystem (reverse-proxied to that node's `/fs/browse`).
    #[serde(default)]
    pub target: Option<String>,
    /// When true, include hidden entries (names starting with '.').
    /// Default is false (matches the historical behavior the Desktop
    /// `RemoteFolderPicker` was written against). Forwarded to the
    /// node-side handler when this call is reverse-proxied.
    #[serde(default)]
    pub show_hidden: Option<bool>,
}

/// A single entry in a directory listing
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseEntry {
    /// File or directory name
    pub name: String,
    /// "file" or "directory"
    #[serde(rename = "type")]
    pub entry_type: String,
    /// Absolute path (for navigation)
    pub path: String,
    /// File size in bytes (None for directories)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Number of direct children (only for directories, for expansion indicator)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children_count: Option<usize>,
}

/// Response for filesystem browsing
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsBrowseResponse {
    /// The path that was browsed (echoed back for UI breadcrumb)
    pub path: String,
    /// Directory entries (directories first, then files, both alphabetical)
    pub entries: Vec<FsBrowseEntry>,
}

/// Count direct children of a directory (excluding hidden ones unless
/// `show_hidden` is true); 0 on any error.
///
/// ponytail: reads the directory at request time, so slow / network drives
/// block the handler. Acceptable here because the sibling listing path does
/// the same enumeration per entry. If this becomes a hotspot, switch to a
/// cached dirent handle or an async stream.
fn count_visible_children(path: &Path, show_hidden: bool) -> usize {
    std::fs::read_dir(path)
        .ok()
        .map(|rd| {
            rd.filter(|e| {
                e.as_ref()
                    .map(|e| show_hidden || !e.file_name().to_string_lossy().starts_with('.'))
                    .unwrap_or(false)
            })
            .count()
        })
        .unwrap_or(0)
}

/// Common root directories to show when browsing "" or empty path.
/// `show_hidden` matches the caller's listing flag so the per-row
/// `childrenCount` reported in the root view stays consistent with what
/// the user sees when they expand a row.
fn root_entries(show_hidden: bool) -> Vec<FsBrowseEntry> {
    let mut entries = Vec::new();

    // User home directory (cross-platform)
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .or_else(|_| {
            std::env::var("HOMEDRIVE")
                .and_then(|d| std::env::var("HOMEPATH").map(|p| format!("{}{}", d, p)))
        })
        .ok();

    if let Some(home_str) = &home {
        let home_path = std::path::Path::new(home_str);
        let name = home_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "Home".to_string());
        entries.push(FsBrowseEntry {
            name,
            entry_type: "directory".to_string(),
            path: home_str.replace('\\', "/"),
            size: None,
            children_count: Some(count_visible_children(home_path, show_hidden)),
        });
    }

    // Temp directory
    #[cfg(unix)]
    {
        let tmp = "/tmp";
        let tmp_path = Path::new(tmp);
        if tmp_path.is_dir() {
            entries.push(FsBrowseEntry {
                name: "tmp".to_string(),
                entry_type: "directory".to_string(),
                path: tmp.to_string(),
                size: None,
                children_count: Some(count_visible_children(tmp_path, show_hidden)),
            });
        }
    }

    // On Unix: add common sibling paths (home and /tmp are above).
    //
    // Don't list `/` itself: this function returns the listing for the
    // `/` path, so `/` is the current path, not a child. Including it
    // made the frontend's tree flatten (Desktop `RemoteFolderPicker`)
    // infinite-loop when the user expanded `/`: the chevron fetch hit
    // this same listing, which contains `/`, which then expanded again
    // to the same listing…
    //
    // `/tmp` is already added by the "Temp directory" block above;
    // listing it again here would emit two entries with the same
    // `path`, which the Desktop frontend rejects as a duplicate React
    // key.
    #[cfg(unix)]
    {
        for (label, path) in [("/var", "/var"), ("/opt", "/opt")] {
            let p = Path::new(path);
            if p.is_dir() {
                entries.push(FsBrowseEntry {
                    name: label.to_string(),
                    entry_type: "directory".to_string(),
                    path: path.to_string(),
                    size: None,
                    children_count: Some(count_visible_children(p, show_hidden)),
                });
            }
        }
    }

    // On Windows: add drive roots
    #[cfg(windows)]
    {
        // List available drive letters
        for letter in 'A'..='Z' {
            let drive = format!("{}:/", letter);
            let drive_path = std::path::Path::new(&drive);
            if drive_path.is_dir() {
                let children_count = count_visible_children(drive_path, show_hidden);
                entries.push(FsBrowseEntry {
                    name: format!("{}:", letter),
                    entry_type: "directory".to_string(),
                    path: drive,
                    size: None,
                    children_count: Some(children_count),
                });
            }
        }
    }

    entries
}

/// Validate a browse path to prevent traversal attacks
fn validate_path(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);

    // Reject path traversal
    if p.components().any(|c| c == std::path::Component::ParentDir) {
        return Err("Path traversal (..) not allowed".to_string());
    }

    Ok(())
}

/// `GET /api/fs/browse` — browse remote server filesystem directories
///
/// When `path` is empty or "/", returns a list of common root directories
/// (home, drive roots on Windows, / on Unix). For a specific path, returns
/// the directory contents (directories first, then files, both alphabetical).
///
/// ADR-055 L7-1: `?target={node_id}` routes the browse to a remote node's
/// filesystem (reverse-proxied to that node's `/fs/browse`); the default
/// (empty / `local`) browses the Gateway machine.
pub async fn browse_fs(
    State(state): State<AppState>,
    Query(query): Query<FsBrowseQuery>,
) -> Result<Json<FsBrowseResponse>, ApiError> {
    let show_hidden = query.show_hidden.unwrap_or(false);

    // Remote target → reverse-proxy to the node's node-local fs browser.
    let target = query.target.as_deref().unwrap_or("").trim();
    if !target.is_empty() && target != "local" {
        return proxy_fs_browse_to_node(
            &state,
            target,
            query.path.as_deref().unwrap_or(""),
            show_hidden,
        )
        .await;
    }

    let requested_path = query.path.as_deref().unwrap_or("").trim();

    // Root browsing — return common starting points
    if requested_path.is_empty() || requested_path == "/" {
        return Ok(Json(FsBrowseResponse {
            path: requested_path.to_string(),
            entries: root_entries(show_hidden),
        }));
    }

    // Validate path
    validate_path(requested_path).map_err(|e| ApiError::bad_request(&e))?;

    let dir_path = std::path::Path::new(requested_path);

    // Verify it's a directory
    if !dir_path.is_dir() {
        return Err(ApiError::bad_request(&format!(
            "Path is not a directory: {}",
            requested_path
        )));
    }

    // Read directory entries
    let read_dir = match std::fs::read_dir(dir_path) {
        Ok(rd) => rd,
        Err(e) => {
            return Err(ApiError::internal(&format!(
                "Failed to read directory: {}",
                e
            )));
        }
    };

    let mut dirs: Vec<FsBrowseEntry> = Vec::new();
    let mut files: Vec<FsBrowseEntry> = Vec::new();

    // Normalize path for consistent output (strip Windows \\?\ prefix)
    let base_str = dir_path.to_string_lossy();
    let normalized_base = base_str
        .strip_prefix(r"\\?\")
        .unwrap_or(base_str.as_ref());
    let normalized_base = normalized_base.replace('\\', "/");

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();

        // Skip hidden files/dirs (starting with '.') unless the caller
        // opted in via `?show_hidden=true`.
        if !show_hidden && name.starts_with('.') {
            continue;
        }

        let metadata = entry.metadata().ok();
        let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());

        let abs_path = entry.path().to_string_lossy().replace('\\', "/");

        if is_dir {
            dirs.push(FsBrowseEntry {
                name,
                entry_type: "directory".to_string(),
                path: abs_path,
                size: None,
                children_count: Some(count_visible_children(&entry.path(), show_hidden)),
            });
        } else {
            files.push(FsBrowseEntry {
                name,
                entry_type: "file".to_string(),
                path: abs_path,
                size: metadata.as_ref().map(|m| m.len()),
                children_count: None,
            });
        }
    }

    // Sort: directories first, then files — both alphabetical (case-insensitive)
    dirs.sort_by_key(|a| a.name.to_lowercase());
    files.sort_by_key(|a| a.name.to_lowercase());

    let mut entries = dirs;
    entries.append(&mut files);

    Ok(Json(FsBrowseResponse {
        path: normalized_base,
        entries,
    }))
}

/// Reverse-proxy a filesystem browse to a remote node (ADR-055 L7-1).
///
/// Resolves the node's `http_endpoint` from the NodeRegistry's retained
/// `NodeInfo` snapshot and forwards the `path` to `{endpoint}/fs/browse`,
/// which the node executes against its own filesystem. The node's JSON
/// response is parsed and re-serialized in the same [`FsBrowseResponse`]
/// shape, so the Desktop frontend is agnostic to which machine served it.
async fn proxy_fs_browse_to_node(
    state: &AppState,
    target: &str,
    path: &str,
    show_hidden: bool,
) -> Result<Json<FsBrowseResponse>, ApiError> {
    let endpoint = crate::http::proxy::node_http_endpoint(state, target).await?;

    // Build the node URL conditionally so the default (?show_hidden
    // absent) matches what the node's own FsBrowseQuery treats as
    // "hide hidden", and a non-default request keeps the flag visible
    // to operators inspecting the proxy call.
    let mut url = format!("{}/fs/browse?path={}", endpoint, urlencoding::encode(path));
    if show_hidden {
        url.push_str("&show_hidden=true");
    }

    // Node HTTP is fail-closed on `X-ACowork-Node-Token` once enrolled
    // (ADR-055 Phase 5a) — same attachment `PATCH /api/nodes/{id}` uses.
    // Omitting it yields a permanent 403 `invalid node token` from the
    // phone / remote Node, which Desktop surfaces as this proxy error.
    let token = crate::http::proxy::node_token_for(state, target)
        .await
        .ok_or_else(|| {
            ApiError::service_unavailable(&format!("Node '{target}' has no token yet"))
        })?;

    let client = crate::http::proxy::runtime_http_client();
    let resp = client
        .get(&url)
        .header("X-ACowork-Node-Token", token)
        .send()
        .await
        .map_err(|e| {
            ApiError::service_unavailable(&format!(
                "Failed to reach node '{}' at {}: {}",
                target, endpoint, e
            ))
        })?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(ApiError::internal(&format!(
            "Node '{}' fs/browse returned {}: {}",
            target, status, body
        )));
    }

    let body: FsBrowseResponse = resp.json().await.map_err(|e| {
        ApiError::internal(&format!(
            "Failed to parse node '{}' fs/browse response: {}",
            target, e
        ))
    })?;

    Ok(Json(body))
}

/// Create filesystem browsing routes
pub fn fs_routes() -> Router<AppState> {
    Router::new().route("/api/fs/browse", get(browse_fs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Tiny per-process counter so parallel tests don't collide on the
    // shared process temp dir.
    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn fresh_dir(label: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "acowork-fsbrowse-{}-{}-{}-{}",
            label,
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn count_visible_children_excludes_hidden() {
        let dir = fresh_dir("hidden");
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::write(dir.join("b.txt"), "x").unwrap();
        fs::write(dir.join(".hidden"), "x").unwrap();
        fs::create_dir(dir.join("subdir")).unwrap();

        assert_eq!(count_visible_children(&dir, false), 3, "expected 3 visible entries");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn count_visible_children_missing_dir_is_zero() {
        let p = fresh_dir("missing");
        // Don't create it — count should silently return 0.
        let _ = fs::remove_dir_all(&p);
        assert_eq!(count_visible_children(&p, false), 0);
    }

    #[test]
    fn count_visible_children_includes_hidden_when_opted_in() {
        // Regression for the `?show_hidden=true` plumbing on
        // `count_visible_children`: default keeps the `.`-prefixed
        // entry out, the opt-in flag brings it back in.
        let dir = fresh_dir("show_hidden");
        fs::write(dir.join("a.txt"), "x").unwrap();
        fs::write(dir.join(".hidden"), "x").unwrap();

        assert_eq!(count_visible_children(&dir, false), 1);
        assert_eq!(count_visible_children(&dir, true), 2);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn root_entries_paths_are_unique() {
        // Regression for the macOS crash: `/tmp` was pushed twice (once
        // by the temp-dir block, once by the /var /tmp /opt loop), so
        // the Desktop frontend saw two entries with the same `path` and
        // crashed on a duplicate React key. Every path must be unique.
        let entries = root_entries(false);
        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            assert!(
                seen.insert(entry.path.as_str()),
                "duplicate path in root_entries: {}",
                entry.path
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_entries_excludes_self_path() {
        // Regression for the macOS stack overflow: `/` was listed as
        // one of its own children, so the Desktop's tree flatten walker
        // (RemoteFolderPicker) recursed `/` → children=[…, `/`, …] →
        // `/` → … forever on a single click. The listing for `/` must
        // never contain `/` itself; that path is the current dir, not
        // a child of it.
        for entry in root_entries(false) {
            assert_ne!(
                entry.path, "/",
                "root_entries() must not list `/` as its own child"
            );
        }
    }
}
