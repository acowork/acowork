//! RuntimeWorkspaceQueryService — implements [`WorkspaceQueryService`].
//!
//! ADR-040: the only implementation of the workspace query trait. Holds
//! the agent `work_dir` (read once at boot) and resolves `workspace_id`
//! to its absolute path via `agent_workspaces.json` so HTTP handlers
//! never see the filesystem layout directly.
//!
//! The five operations cover:
//! - `list_workspaces` — return the agent's configured workspace dirs
//! - `list_tree`       — directory listing (used by FileTree component)
//! - `read_file`       — UTF-8 file content read (editor open)
//! - `find_files`      — fuzzy filename search (Ctrl+P-style palette)
//! - `search_files`    — ripgrep-style content search (Ctrl+Shift+F)
//!
//! `find_files` + `search_files` are offloaded to `spawn_blocking`
//! because they walk the whole workspace tree — keeping them on the
//! async runtime would block LLM streaming on large workspaces.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use base64::Engine;
use chrono::DateTime;
use ignore::WalkBuilder;
use regex::Regex;

use crate::usecases::workspace_query::{
    FindFilesParams, FindMatchDto, FindResponse, ListTreeParams, RawFileDto, ReadFileParams,
    SearchFilesParams, SearchMatchDto, SearchResponse, TreeEntryDto, TreeResponse, WorkspaceError,
    WorkspaceFileDto, WorkspaceQueryService, WorkspacesListResponse,
};

// ── Constants (kept in sync with gateway workspaces.rs) ────────────────────

const MAX_FILENAME_SCAN: usize = 50_000;
const DEFAULT_FILENAME_LIMIT: usize = 50;
const MAX_FILENAME_LIMIT: usize = 200;
const DEFAULT_MAX_SEARCH_RESULTS: usize = 200;
const ABSOLUTE_MAX_SEARCH_RESULTS: usize = 1000;
const SEARCH_BAILOUT_BYTES: u64 = 1_048_576; // 1 MiB per file

/// VSCode's default `files.exclude` — the only dotfiles/dirs hidden from
/// the workspace tree. Everything else (`.cargo`, `.vscode`, `.gitignore`,
/// ...) is shown, mirroring the Explorer's default behaviour: hidden
/// entries are visible unless they are VCS metadata or OS junk files.
const VSCODE_DEFAULT_EXCLUDES: &[&str] = &[".git", ".svn", ".hg", "CVS", ".DS_Store", "Thumbs.db"];

/// True when `name` matches one of VSCode's default `files.exclude`
/// entries. Applied at every directory level (equivalent to `**/.git`).
fn is_vscode_default_excluded(name: &str) -> bool {
    VSCODE_DEFAULT_EXCLUDES.contains(&name)
}
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "tif", "mp3", "mp4", "avi", "mov",
    "wav", "flac", "ogg", "webm", "mkv", "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst", "o",
    "obj", "a", "so", "dylib", "dll", "exe", "pdb", "lib", "class", "wasm", "bc", "ll", "pyc",
    "pyo", "rlib", "rmeta", "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "bin", "dat", "db",
    "sqlite", "sqlite3", "pack",
    "idx",
    // NOTE: SVG (`svg`) is intentionally NOT listed here. SVG is XML text and
    // the editor needs the raw markup as `content` so it can be edited in
    // Monaco; base64-wrapping it made the source unreadable. The desktop
    // preview branch handles SVG via `data:image/svg+xml;charset=utf-8,...`
    // which works directly on the text payload.
];

// ── Impl ───────────────────────────────────────────────────────────────────

pub struct RuntimeWorkspaceQueryService {
    work_dir: PathBuf,
    package_dir: Option<PathBuf>,
    agent_id: String,
}

impl RuntimeWorkspaceQueryService {
    pub fn new(work_dir: PathBuf, agent_id: String) -> Self {
        Self {
            work_dir,
            package_dir: None,
            agent_id,
        }
    }

    pub fn new_with_package_dir(work_dir: PathBuf, agent_id: String, package_dir: PathBuf) -> Self {
        Self {
            work_dir,
            package_dir: Some(package_dir),
            agent_id,
        }
    }

    fn resolve_requested_path(
        &self,
        workspace_id: Option<&str>,
        path: &str,
    ) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
        if crate::usecases::package_workspace::is_package_workspace(workspace_id) {
            let package_dir = self.package_dir.as_deref().ok_or_else(|| {
                WorkspaceError::WorkspaceNotFound("__agent_package__".to_string())
            })?;
            return crate::usecases::package_workspace::resolve_within(package_dir, path);
        }
        self.resolve_within(workspace_id, path)
    }

    /// Resolve a workspace root absolute path from `agent_workspaces.json`.
    /// Returns the agent home directory when `workspace_id` is None /
    /// empty / `__agent_home__`. Unknown workspace IDs surface as
    /// `WorkspaceError::WorkspaceNotFound`.
    fn resolve_root(&self, workspace_id: Option<&str>) -> Result<PathBuf, WorkspaceError> {
        let ws_id = match workspace_id {
            Some(id) if !id.is_empty() && id != "__agent_home__" => id,
            _ => return Ok(self.work_dir.clone()),
        };

        let config_path = self.work_dir.join("config").join("agent_workspaces.json");
        let content = std::fs::read_to_string(&config_path).map_err(|_| {
            // Config missing is treated as "no additional workspaces",
            // matching the fallback semantics of `list_tree`.
            // Only an explicitly-unknown id surfaces as WorkspaceNotFound.
            WorkspaceError::WorkspaceNotFound(ws_id.to_string())
        })?;

        let val: serde_json::Value =
            serde_json::from_str(&content).map_err(WorkspaceError::Json)?;

        if let Some(dirs) = val.get("additional_dirs").and_then(|v| v.as_array()) {
            for dir in dirs {
                if let Some(path) = dir
                    .get("id")
                    .and_then(|v| v.as_str())
                    .filter(|id| *id == ws_id)
                    .and_then(|_| dir.get("path").and_then(|v| v.as_str()))
                {
                    return Ok(PathBuf::from(path));
                }
            }
        }

        Err(WorkspaceError::WorkspaceNotFound(ws_id.to_string()))
    }

    /// Same as `resolve_root` but additionally canonicalises the
    /// `requested_path` (with deepest-existing-ancestor fallback) and
    /// verifies it stays inside the workspace root. Mirrors the
    /// `resolve_within_workspace` helper previously in server.rs.
    fn resolve_within(
        &self,
        workspace_id: Option<&str>,
        requested_path: &str,
    ) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
        let workspace_root = self.resolve_root(workspace_id)?;

        let abs_path = if requested_path.is_empty() {
            workspace_root.clone()
        } else {
            workspace_root.join(requested_path)
        };

        let canonical_root = std::fs::canonicalize(&workspace_root).map_err(|e| {
            WorkspaceError::Io(std::io::Error::other(format!(
                "workspace root not accessible: {}",
                e
            )))
        })?;

        let check_path = deepest_existing_ancestor(&abs_path);
        let canonical_check = std::fs::canonicalize(&check_path).unwrap_or(check_path);

        if !canonical_check.starts_with(&canonical_root) {
            return Err(WorkspaceError::InvalidPath(
                "path traversal detected".to_string(),
            ));
        }

        let rel_path = abs_path
            .strip_prefix(&workspace_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        Ok((canonical_root, abs_path, rel_path))
    }

    /// Best-effort MIME type from file extension. Falls back to
    /// `application/octet-stream` for unknown extensions.
    ///
    /// Covers both the text formats read by the editor and the binary
    /// image/PDF formats returned base64-encoded to the desktop image
    /// preview branch (`mimeType.startsWith("image/")`).
    fn mime_type_for(rel_path: &str) -> &'static str {
        let ext = Path::new(rel_path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match ext.as_str() {
            "txt" | "log" | "csv" | "tsv" | "ini" => "text/plain",
            "md" | "markdown" => "text/markdown",
            "json" => "application/json",
            "yml" | "yaml" => "application/yaml",
            "xml" => "application/xml",
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "js" | "mjs" | "cjs" => "application/javascript",
            "ts" | "tsx" => "application/typescript",
            "rs" => "text/x-rust",
            "py" => "text/x-python",
            "go" => "text/x-go",
            "java" => "text/x-java",
            "kt" | "kts" => "text/x-kotlin",
            "swift" => "text/x-swift",
            "c" | "h" => "text/x-c",
            "cpp" | "cxx" | "cc" | "hpp" => "text/x-cpp",
            "sh" | "bash" => "application/x-sh",
            "ps1" => "application/x-powershell",
            "toml" => "application/toml",
            "svg" => "image/svg+xml",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "ico" => "image/x-icon",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        }
    }

    fn is_binary_path(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| BINARY_EXTENSIONS.contains(&e.to_lowercase().as_str()))
            .unwrap_or(false)
    }

    fn modified_rfc3339(meta: &std::fs::Metadata) -> Option<String> {
        meta.modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
            .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0))
            .map(|dt| dt.to_rfc3339())
    }

    // ── Filename scoring (mirrors gateway `score_match`) ───────────────

    fn score_match(name: &str, rel_path: &str, q_lower: &str, q_segments: &[&str]) -> Option<u32> {
        if q_segments.is_empty() {
            return None;
        }
        let name_lower = name.to_lowercase();
        if name_lower == *q_lower {
            return Some(1000);
        }
        if name_lower.starts_with(q_lower) {
            return Some(800);
        }
        if q_segments.iter().all(|seg| match_word_boundary(name, seg)) {
            return Some(600);
        }
        if q_segments.iter().all(|seg| name_lower.contains(seg)) {
            return Some(400);
        }
        let path_lower = rel_path.to_lowercase();
        if q_segments.iter().all(|seg| path_lower.contains(seg)) {
            return Some(200);
        }
        None
    }
}

/// Walk up `path` until we find an existing directory.
fn deepest_existing_ancestor(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    while !current.exists() {
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => return path.to_path_buf(),
        }
    }
    current
}

/// Camel/snake/kebab-case word-boundary match — see gateway `match_word_boundary`.
fn match_word_boundary(name: &str, seg: &str) -> bool {
    if seg.is_empty() {
        return true;
    }
    let name_bytes = name.as_bytes();
    let name_lower = name.to_lowercase();
    let mut start = 0usize;
    while let Some(pos) = name_lower[start..].find(seg) {
        let abs = start + pos;
        let at_word_start =
            abs == 0 || matches!(name_bytes[abs - 1], b'.' | b'_' | b' ' | b'-' | b'/');
        let camel_boundary = abs > 0
            && name_bytes[abs - 1].is_ascii_lowercase()
            && name_bytes[abs].is_ascii_uppercase();
        if at_word_start || camel_boundary {
            return true;
        }
        start = abs + seg.len();
    }
    false
}

// ── Trait impl ─────────────────────────────────────────────────────────────

impl RuntimeWorkspaceQueryService {
    fn list_package_tree(&self, requested_path: &str) -> Result<TreeResponse, WorkspaceError> {
        let package_dir = self
            .package_dir
            .as_deref()
            .ok_or_else(|| WorkspaceError::WorkspaceNotFound("__agent_package__".to_string()))?;
        let (canonical_root, abs_path, rel_path) =
            crate::usecases::package_workspace::resolve_within(package_dir, requested_path)?;
        if requested_path.is_empty() {
            let entries = ["prompts", "skills"]
                .iter()
                .filter_map(|name| {
                    let (_, path, _) =
                        crate::usecases::package_workspace::resolve_within(package_dir, name)
                            .ok()?;
                    let metadata = std::fs::metadata(&path).ok()?;
                    metadata.is_dir().then(|| TreeEntryDto {
                        name: (*name).to_string(),
                        entry_type: "directory".to_string(),
                        size: None,
                        modified: Self::modified_rfc3339(&metadata),
                        children_count: std::fs::read_dir(&path).ok().map(|rd| rd.count()),
                    })
                })
                .collect();
            return Ok(TreeResponse {
                root: canonical_to_root_string(&canonical_root),
                path: String::new(),
                entries,
            });
        }
        let read_dir = std::fs::read_dir(&abs_path).map_err(|e| {
            WorkspaceError::Io(std::io::Error::other(format!(
                "Failed to read directory: {e}"
            )))
        })?;
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_vscode_default_excluded(&name)
                || entry
                    .file_type()
                    .is_ok_and(|file_type| file_type.is_symlink())
            {
                continue;
            }
            let metadata = entry.metadata().ok();
            if metadata.as_ref().is_some_and(|m| m.is_dir()) {
                dirs.push(TreeEntryDto {
                    name,
                    entry_type: "directory".to_string(),
                    size: None,
                    modified: metadata.as_ref().and_then(Self::modified_rfc3339),
                    children_count: std::fs::read_dir(entry.path()).ok().map(|rd| rd.count()),
                });
            } else {
                files.push(TreeEntryDto {
                    name,
                    entry_type: "file".to_string(),
                    size: metadata.as_ref().map(|m| m.len()),
                    modified: metadata.as_ref().and_then(Self::modified_rfc3339),
                    children_count: None,
                });
            }
        }
        dirs.sort_by_key(|e| e.name.to_lowercase());
        files.sort_by_key(|e| e.name.to_lowercase());
        dirs.extend(files);
        Ok(TreeResponse {
            root: canonical_to_root_string(&canonical_root),
            path: rel_path,
            entries: dirs,
        })
    }
}

#[async_trait]
impl WorkspaceQueryService for RuntimeWorkspaceQueryService {
    async fn list_workspaces(&self) -> Result<WorkspacesListResponse, WorkspaceError> {
        let config_path = self.work_dir.join("config").join("agent_workspaces.json");

        let workspaces = if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(val) => val
                        .get("additional_dirs")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default(),
                    Err(_) => vec![],
                },
                Err(_) => vec![],
            }
        } else {
            vec![]
        };

        Ok(WorkspacesListResponse {
            agent_id: self.agent_id.clone(),
            workspaces,
        })
    }

    async fn list_tree(&self, params: &ListTreeParams) -> Result<TreeResponse, WorkspaceError> {
        let requested_path = params.path.as_deref().unwrap_or("");
        if crate::usecases::package_workspace::is_package_workspace(params.workspace_id.as_deref())
        {
            return self.list_package_tree(requested_path);
        }
        let workspace_root = self.resolve_root(params.workspace_id.as_deref())?;

        // Mirror the original server.rs list_tree: canonicalize the
        // workspace root for the response + path-traversal guard.
        let abs_path = if requested_path.is_empty() {
            workspace_root.clone()
        } else {
            workspace_root.join(requested_path)
        };
        let canonical_root =
            std::fs::canonicalize(&workspace_root).unwrap_or_else(|_| workspace_root.clone());
        let canonical_abs = std::fs::canonicalize(&abs_path).unwrap_or_else(|_| abs_path.clone());

        if !canonical_abs.starts_with(&canonical_root) {
            return Err(WorkspaceError::InvalidPath(
                "path traversal detected".to_string(),
            ));
        }

        let rel_path = canonical_abs
            .strip_prefix(&canonical_root)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();

        let read_dir = std::fs::read_dir(&canonical_abs).map_err(|e| {
            WorkspaceError::Io(std::io::Error::other(format!(
                "Failed to read directory: {}",
                e
            )))
        })?;

        // Strip Windows `\\?\` extended-length path prefix and normalise
        // separators to forward slashes. Forwarding the verbatim path
        // (e.g. `\\?\D:\foo`) verbatim would surface as `//?/D:/foo` to
        // the client; once it reaches `monaco.Uri.file` / `vscode-uri`,
        // the `?` is URL-encoded to `%3F` and the resulting
        // `file://%3F/d%3A/...` URI is rejected by rust-analyzer
        // ("invalid international domain name").
        let root_str = canonical_to_root_string(&canonical_root);
        let mut dirs: Vec<TreeEntryDto> = Vec::new();
        let mut files: Vec<TreeEntryDto> = Vec::new();

        for entry in read_dir {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if is_vscode_default_excluded(&name) {
                continue;
            }
            let metadata = entry.metadata().ok();
            let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());

            if is_dir {
                let children_count = std::fs::read_dir(entry.path())
                    .ok()
                    .map(|rd| {
                        rd.filter(|e| {
                            e.as_ref()
                                .map(|e| {
                                    let fname = e.file_name();
                                    let n = fname.to_string_lossy();
                                    !is_vscode_default_excluded(&n)
                                })
                                .unwrap_or(false)
                        })
                        .count()
                    })
                    .unwrap_or(0);
                dirs.push(TreeEntryDto {
                    name,
                    entry_type: "directory".to_string(),
                    size: None,
                    modified: metadata.and_then(|m| Self::modified_rfc3339(&m)),
                    children_count: Some(children_count),
                });
            } else {
                files.push(TreeEntryDto {
                    name,
                    entry_type: "file".to_string(),
                    size: metadata.as_ref().map(|m| m.len()),
                    modified: metadata.and_then(|m| Self::modified_rfc3339(&m)),
                    children_count: None,
                });
            }
        }

        dirs.sort_by_key(|a| a.name.to_lowercase());
        files.sort_by_key(|a| a.name.to_lowercase());
        let mut entries = dirs;
        entries.append(&mut files);

        Ok(TreeResponse {
            root: root_str,
            path: rel_path,
            entries,
        })
    }

    async fn read_file(&self, params: &ReadFileParams) -> Result<WorkspaceFileDto, WorkspaceError> {
        let (_root, abs_path, rel_path) =
            self.resolve_requested_path(params.workspace_id.as_deref(), &params.path)?;

        let meta = std::fs::metadata(&abs_path).map_err(|_| {
            WorkspaceError::NotFound(format!("failed to read metadata for: {}", rel_path))
        })?;

        let is_binary = Self::is_binary_path(&abs_path);

        let content = if is_binary {
            let bytes = std::fs::read(&abs_path).map_err(|_| {
                WorkspaceError::NotFound(format!("failed to read file: {}", rel_path))
            })?;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        } else {
            std::fs::read_to_string(&abs_path).map_err(|_| {
                WorkspaceError::InvalidUtf8(format!("file is not valid UTF-8: {}", rel_path))
            })?
        };

        Ok(WorkspaceFileDto {
            content,
            size: meta.len(),
            mime_type: Self::mime_type_for(&rel_path).to_string(),
            is_file: meta.is_file(),
            is_dir: meta.is_dir(),
            modified: Self::modified_rfc3339(&meta),
            path: rel_path,
        })
    }

    async fn read_file_raw(&self, params: &ReadFileParams) -> Result<RawFileDto, WorkspaceError> {
        // Same `resolve_within` guard as `read_file` — canonicalise +
        // containment check (path-traversal defence) before touching disk.
        let (_root, abs_path, rel_path) =
            self.resolve_requested_path(params.workspace_id.as_deref(), &params.path)?;

        let meta = std::fs::metadata(&abs_path).map_err(|_| {
            WorkspaceError::NotFound(format!("failed to read metadata for: {}", rel_path))
        })?;
        if !meta.is_file() {
            return Err(WorkspaceError::NotFound(format!(
                "not a file: {}",
                rel_path
            )));
        }

        let bytes = std::fs::read(&abs_path)
            .map_err(|_| WorkspaceError::NotFound(format!("failed to read file: {}", rel_path)))?;

        Ok(RawFileDto {
            bytes,
            mime_type: Self::mime_type_for(&rel_path).to_string(),
            size: meta.len(),
        })
    }

    async fn find_files(&self, params: &FindFilesParams) -> Result<FindResponse, WorkspaceError> {
        let pattern = params.q.as_deref().unwrap_or("").trim();
        if pattern.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "Missing required 'q' parameter".to_string(),
            });
        }
        let pattern = pattern.to_string();
        let limit = params
            .limit
            .unwrap_or(DEFAULT_FILENAME_LIMIT)
            .clamp(1, MAX_FILENAME_LIMIT);
        let workspace_root = self.resolve_root(params.workspace_id.as_deref())?;
        let workspace_root_str = workspace_root.to_string_lossy().to_string();

        let result = tokio::task::spawn_blocking(move || {
            run_filename_search(&workspace_root_str, &pattern, limit)
        })
        .await
        .map_err(|_| WorkspaceError::Persist("Filename search task panicked".to_string()))?;

        Ok(result)
    }

    async fn search_files(
        &self,
        params: &SearchFilesParams,
    ) -> Result<SearchResponse, WorkspaceError> {
        let pattern = params.q.as_deref().unwrap_or("");
        if pattern.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "Missing required 'q' parameter".to_string(),
            });
        }

        let re_pattern = if params.whole_word {
            format!(r"\b(?:{})\b", pattern)
        } else {
            pattern.to_string()
        };
        let re = regex::RegexBuilder::new(&re_pattern)
            .case_insensitive(!params.case_sensitive)
            .build()
            .map_err(|e| WorkspaceError::BadRequest {
                status: 400,
                message: format!("Invalid regex: {}", e),
            })?;

        let workspace_root = self.resolve_root(params.workspace_id.as_deref())?;
        let workspace_root_str = workspace_root.to_string_lossy().to_string();
        let max_results = params
            .max_results
            .unwrap_or(DEFAULT_MAX_SEARCH_RESULTS)
            .min(ABSOLUTE_MAX_SEARCH_RESULTS);
        let include_glob = params.include.clone();

        let result = tokio::task::spawn_blocking(move || {
            run_search(
                &workspace_root_str,
                &re,
                include_glob.as_deref(),
                max_results,
            )
        })
        .await
        .map_err(|_| WorkspaceError::Persist("Search task panicked".to_string()))?;

        Ok(result)
    }
}

// ── Blocking helpers (filename + content search) ───────────────────────────

/// Convert an already-canonicalized path into a portable string form
/// (no Windows `\\?\` extended-length prefix, forward slashes).
///
/// Used by every HTTP response field that exposes the workspace root
/// to clients. Keeping the root free of OS-specific verbatim syntax
/// prevents downstream bugs such as `file://%3F/d%3A/...` URLs after
/// `monaco.Uri.file` / `vscode-uri` on the desktop side — see the
/// `list_tree` impl comment for the full failure chain.
fn canonical_to_root_string(canonical: &Path) -> String {
    let s = canonical.to_string_lossy();
    let stripped = s.strip_prefix(r"\\?\").unwrap_or(s.as_ref());
    stripped.replace('\\', "/")
}

/// Resolve a workspace-root string for filesystem walks used by the
/// blocking search helpers. Mirrors [`canonical_to_root_string`] but
/// accepts a raw (possibly not-yet-canonicalized) string for callers
/// that hold a `&str` rather than a `PathBuf`.
fn normalised_root(workspace_root: &str) -> String {
    let canonical = Path::new(workspace_root)
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(workspace_root));
    canonical_to_root_string(&canonical)
}

fn run_filename_search(workspace_root: &str, pattern: &str, limit: usize) -> FindResponse {
    let q_lower = pattern.to_lowercase();
    let q_segments: Vec<&str> = q_lower
        .split(|c: char| c.is_whitespace() || c == '/' || c == '\\')
        .filter(|s| !s.is_empty())
        .collect();

    let root_str = normalised_root(workspace_root);

    let walker = WalkBuilder::new(workspace_root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut scored: Vec<FindMatchDto> = Vec::new();
    let mut scanned: usize = 0;
    let mut truncated = false;

    'outer: for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let ft = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };
        if !ft.is_file() {
            continue;
        }

        scanned += 1;
        if scanned > MAX_FILENAME_SCAN {
            truncated = true;
            break 'outer;
        }

        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };

        let rel_path = path
            .strip_prefix(workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let Some(score) =
            RuntimeWorkspaceQueryService::score_match(name, &rel_path, &q_lower, &q_segments)
        else {
            continue;
        };

        scored.push(FindMatchDto {
            name: name.to_string(),
            rel_path,
            entry_type: "file".to_string(),
            score,
        });
    }

    scored.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then(a.rel_path.len().cmp(&b.rel_path.len()))
            .then(a.rel_path.cmp(&b.rel_path))
    });

    let total = scored.len();
    let matches: Vec<FindMatchDto> = scored.into_iter().take(limit).collect();

    FindResponse {
        root: root_str,
        scanned,
        truncated: truncated || total > matches.len(),
        matches,
    }
}

fn run_search(
    workspace_root: &str,
    re: &Regex,
    include_glob: Option<&str>,
    max_results: usize,
) -> SearchResponse {
    let mut results: Vec<SearchMatchDto> = Vec::with_capacity(max_results);
    let mut total_matches: usize = 0;
    let mut truncated = false;

    let walker = WalkBuilder::new(workspace_root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    'outer: for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();

        if let Ok(meta) = entry.metadata()
            && meta.len() > SEARCH_BAILOUT_BYTES
        {
            continue;
        }

        if let Some(glob) = include_glob {
            let file_name = path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_default();
            let matched = glob.split(',').any(|g| {
                let pat = g.trim();
                if pat.starts_with("*.") {
                    file_name.ends_with(&pat[1..])
                } else {
                    file_name.contains(pat)
                }
            });
            if !matched {
                continue;
            }
        }

        if RuntimeWorkspaceQueryService::is_binary_path(path) {
            continue;
        }

        let rel_path = path
            .strip_prefix(workspace_root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for (line_num, line) in content.lines().enumerate() {
            if let Some(m) = re.find(line) {
                total_matches += 1;
                if results.len() < max_results {
                    results.push(SearchMatchDto {
                        file: rel_path.clone(),
                        line: line_num + 1,
                        column: m.start() + 1,
                        text: line.trim_end().to_string(),
                    });
                } else {
                    truncated = true;
                    break 'outer;
                }
            }
        }
    }

    SearchResponse {
        matches: results,
        total_matches,
        truncated,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecases::workspace_query::ListTreeParams;
    use tempfile::tempdir;

    /// On Windows, `std::fs::canonicalize` returns the verbatim
    /// `\\?\D:\...` form. The helper must strip it so the resulting
    /// string is safe to feed into `monaco.Uri.file` / LSP `rootUri`.
    /// On non-Windows platforms the prefix is never produced, so we
    /// only assert the no-prefix invariant universally.
    #[test]
    fn canonical_to_root_string_strips_windows_verbatim_prefix() {
        let tmp = tempdir().expect("tempdir");
        let canonical = tmp.path().canonicalize().expect("canonicalize");
        let s = canonical_to_root_string(&canonical);

        // Universal contract: no Windows extended-length prefix, ever.
        // `std::path::Path::to_string_lossy` keeps backslashes on
        // Windows so the prefix appears as `\\?\` (backslash form);
        // `replace('\\', "/")` would have turned it into `//?/`
        // (forward-slash form). The helper must strip both.
        let canonical_str = canonical.to_string_lossy();
        assert!(
            !canonical_str.starts_with(r"\\?\") || !s.starts_with(r"\\?\"),
            "result must not retain Windows verbatim backslash prefix, got: {s}"
        );
        assert!(
            !s.starts_with("//?/"),
            "result must not retain forward-slash verbatim prefix, got: {s}"
        );
        // Universal contract: forward slashes only.
        assert!(
            !s.contains('\\'),
            "root string must use forward slashes, got: {s}"
        );

        // Cross-platform: result points at the temp directory.
        let canonical_str = canonical.to_string_lossy();
        let expected = canonical_str
            .strip_prefix(r"\\?\")
            .unwrap_or(canonical_str.as_ref())
            .replace('\\', "/");
        assert_eq!(s, expected);
    }

    /// `normalised_root` is the thin string-accepting wrapper used by
    /// `find_files` / `search_files`. It must agree with
    /// `canonical_to_root_string` on every platform.
    #[test]
    fn normalised_root_matches_canonical_to_root_string() {
        let tmp = tempdir().expect("tempdir");
        let input = tmp.path().to_string_lossy().to_string();
        let a = normalised_root(&input);
        let b = canonical_to_root_string(&tmp.path().canonicalize().expect("canonicalize"));
        assert_eq!(a, b);
    }

    #[tokio::test]
    async fn package_tree_exposes_only_prompts_and_skills_and_reads_files() {
        let temp = tempdir().expect("tempdir");
        let work = temp.path().join("work");
        let package = temp.path().join("package");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(package.join("prompts")).unwrap();
        std::fs::create_dir_all(package.join("skills/demo")).unwrap();
        std::fs::write(package.join("prompts/system.md"), "prompt").unwrap();
        std::fs::write(package.join("skills/demo/SKILL.md"), "skill").unwrap();
        std::fs::write(package.join("manifest.toml"), "hidden").unwrap();
        let svc = RuntimeWorkspaceQueryService::new_with_package_dir(
            work,
            "test-agent".to_string(),
            package,
        );
        let tree = svc
            .list_tree(&ListTreeParams {
                workspace_id: Some("__agent_package__".to_string()),
                path: Some(String::new()),
            })
            .await
            .expect("package root");
        assert_eq!(
            tree.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["prompts", "skills"]
        );
        let prompt = svc
            .read_file(&ReadFileParams {
                workspace_id: Some("__agent_package__".to_string()),
                path: "prompts/system.md".to_string(),
            })
            .await
            .expect("read prompt");
        assert_eq!(prompt.content, "prompt");
        let err = svc
            .read_file(&ReadFileParams {
                workspace_id: Some("__agent_package__".to_string()),
                path: "manifest.toml".to_string(),
            })
            .await
            .expect_err("metadata must be hidden");
        assert_eq!(err.http_status(), 400);
    }

    /// Regression test for the bug where `list_tree`'s response
    /// `root` field surfaced as `//?/D:/...` on Windows, which the
    /// desktop app then fed into `monaco.Uri.file`, producing
    /// `file://%3F/d%3A/...` — an URI that rust-analyzer rejects with
    /// "invalid international domain name" during `initialize`.
    ///
    /// Asserts the contract documented on `TreeResponse::root`: no
    /// Windows `\\?\` prefix, forward slashes only.
    #[tokio::test]
    async fn list_tree_root_is_normalised_on_windows() {
        let tmp = tempdir().expect("tempdir");
        // Seed a directory entry so `read_dir` has something to list —
        // irrelevant to the bug, but keeps the call cheap and
        // unconditional.
        std::fs::write(tmp.path().join("seed.txt"), "x").expect("write");

        // work_dir == tempdir; resolve_root(workspace_id = "__agent_home__")
        // returns work_dir unchanged.
        let svc =
            RuntimeWorkspaceQueryService::new(tmp.path().to_path_buf(), "test-agent".to_string());

        let resp = svc
            .list_tree(&ListTreeParams {
                workspace_id: None,
                path: Some(String::new()),
            })
            .await
            .expect("list_tree");

        // The bug surfaced the prefix as forward-slash form (`//?/`)
        // because `to_string_lossy().replace('\\', "/")` rewrites the
        // backslashes of the verbatim prefix. Reject both forms here
        // — without the fix the result is `//?/C:/Users/...` and the
        // assertion below fails loudly.
        assert!(
            !resp.root.starts_with("//?/") && !resp.root.starts_with(r"\\?\"),
            "TreeResponse.root must not contain Windows verbatim prefix, got: {}",
            resp.root
        );
        assert!(
            !resp.root.contains('\\'),
            "TreeResponse.root must use forward slashes, got: {}",
            resp.root
        );
        // Sanity: ends with the tempdir's last segment.
        let expected_suffix = tmp
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert!(
            resp.root.ends_with(&expected_suffix),
            "root should end with the tempdir leaf, got: {} (want suffix: {expected_suffix})",
            resp.root
        );
    }

    /// `list_tree` mirrors VSCode's default `files.exclude`: hidden
    /// entries are shown unless they are VCS metadata / OS junk
    /// (`.git`, `.svn`, `.hg`, `CVS`, `.DS_Store`, `Thumbs.db`).
    /// Regression guard for the old blanket "hide all dotfiles" filter.
    #[tokio::test]
    async fn list_tree_shows_hidden_except_vscode_default_excludes() {
        let tmp = tempdir().expect("tempdir");
        let root = tmp.path();

        // Hidden entries that MUST be visible (VSCode shows these).
        std::fs::create_dir(root.join(".cargo")).expect("mkdir .cargo");
        std::fs::create_dir(root.join(".vscode")).expect("mkdir .vscode");
        std::fs::write(root.join(".gitignore"), "target/\n").expect("write .gitignore");
        std::fs::write(root.join("readme.txt"), "hi").expect("write readme");

        // VSCode-default-excluded entries that MUST stay hidden.
        std::fs::create_dir(root.join(".git")).expect("mkdir .git");
        std::fs::create_dir(root.join(".svn")).expect("mkdir .svn");
        std::fs::create_dir(root.join(".hg")).expect("mkdir .hg");
        std::fs::create_dir(root.join("CVS")).expect("mkdir CVS");
        std::fs::write(root.join(".DS_Store"), "junk").expect("write .DS_Store");
        std::fs::write(root.join("Thumbs.db"), "junk").expect("write Thumbs.db");

        // A hidden dir with a hidden child — children_count must count
        // `.cargo`'s visible children and skip excluded ones.
        std::fs::write(root.join(".cargo").join("config.toml"), "x").expect("write config");
        std::fs::create_dir(root.join(".cargo").join(".git")).expect("mkdir .cargo/.git");

        let svc = RuntimeWorkspaceQueryService::new(root.to_path_buf(), "test-agent".to_string());
        let resp = svc
            .list_tree(&ListTreeParams {
                workspace_id: None,
                path: Some(String::new()),
            })
            .await
            .expect("list_tree");

        let names: Vec<&str> = resp.entries.iter().map(|e| e.name.as_str()).collect();

        // Visible hidden entries.
        for expected in [".cargo", ".vscode", ".gitignore", "readme.txt"] {
            assert!(
                names.contains(&expected),
                "tree must include {expected:?}, got: {names:?}"
            );
        }
        // Excluded entries.
        for excluded in [".git", ".svn", ".hg", "CVS", ".DS_Store", "Thumbs.db"] {
            assert!(
                !names.contains(&excluded),
                "tree must NOT include {excluded:?}, got: {names:?}"
            );
        }

        // children_count on `.cargo` counts visible children only
        // (config.toml = 1; `.git` is excluded).
        let cargo = resp
            .entries
            .iter()
            .find(|e| e.name == ".cargo")
            .expect(".cargo entry");
        assert_eq!(
            cargo.children_count,
            Some(1),
            "children_count must skip excluded entries"
        );
    }

    /// `read_file_raw` (ADR-055 L2-7) must return verbatim bytes + MIME
    /// and reject path traversal, so the Gateway's HTML-preview reverse
    /// proxy serves sub-resources correctly and never escapes the
    /// workspace root.
    #[tokio::test]
    async fn read_file_raw_returns_bytes_and_blocks_traversal() {
        let tmp = tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("index.html"), "<h1>hi</h1>").expect("write");

        let svc =
            RuntimeWorkspaceQueryService::new(tmp.path().to_path_buf(), "test-agent".to_string());

        // Agent home: raw bytes + HTML MIME (not a JSON envelope).
        let dto = svc
            .read_file_raw(&ReadFileParams {
                workspace_id: None,
                path: "index.html".to_string(),
            })
            .await
            .expect("read_file_raw");
        assert_eq!(dto.bytes, b"<h1>hi</h1>");
        assert_eq!(dto.mime_type, "text/html");

        // Path traversal must be rejected by the shared `resolve_within`.
        let err = svc
            .read_file_raw(&ReadFileParams {
                workspace_id: None,
                path: "../secret.txt".to_string(),
            })
            .await
            .expect_err("traversal must fail");
        assert_eq!(err.http_status(), 400);
    }
}
