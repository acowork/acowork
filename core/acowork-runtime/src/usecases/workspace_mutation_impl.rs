//! RuntimeWorkspaceMutationService — implements [`WorkspaceMutationService`].
//!
//! ADR-040: the only implementation of the workspace mutation trait.
//! Holds the agent `work_dir` and provides a single audit point for:
//! - atomic `agent_workspaces.json` persistence (write-tmp-rename)
//! - file/dir mutation calls that go through the path-traversal guard
//!
//! The query counterpart [`crate::usecases::RuntimeWorkspaceQueryService`]
//! owns the `resolve_within` helper used here; mutations reuse it so
//! the path-traversal guard is identical for reads and writes.

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::usecases::workspace_mutation::{
    CopyMoveBody, CreateFileBody, FilePathQuery, PathOnlyBody, PromptFileBody, WorkspaceEntryInput,
    WorkspaceMutationResponse, WorkspaceMutationService,
};
use crate::usecases::workspace_query::WorkspaceError;

pub struct RuntimeWorkspaceMutationService {
    work_dir: PathBuf,
    package_dir: Option<PathBuf>,
}

/// RFC3339 mtime of a file's metadata, or `None` when unavailable.
///
/// ADR-058: shared by `write_file`'s response so the Desktop caches the
/// same `modified` shape the read endpoint (`WorkspaceFileDto::modified`)
/// returns.
fn metadata_modified_rfc3339(meta: &std::fs::Metadata) -> Option<String> {
    use chrono::DateTime;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
        .and_then(|d| DateTime::from_timestamp(d.as_secs() as i64, 0))
        .map(|dt| dt.to_rfc3339())
}

impl RuntimeWorkspaceMutationService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir,
            package_dir: None,
        }
    }

    pub fn new_with_package_dir(work_dir: PathBuf, package_dir: PathBuf) -> Self {
        Self {
            work_dir,
            package_dir: Some(package_dir),
        }
    }

    /// Generate a fresh workspace id (`ws-` + 12 lowercase hex chars).
    ///
    /// Mirrors the pre-ADR-040 Gateway-direct format so newly-created
    /// entries are indistinguishable from existing ones on disk.
    /// 12 hex chars = 48 bits of entropy — sufficient collision
    /// resistance for the per-agent config (the file holds a handful of
    /// entries at most, never thousands).
    fn generate_workspace_id() -> String {
        let uuid = uuid::Uuid::new_v4().simple().to_string();
        format!("ws-{}", &uuid[..12])
    }

    /// Now as an RFC 3339 string. Wrapped so tests can swap it out if
    /// they ever need a deterministic clock.
    fn now_rfc3339() -> String {
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }

    /// Resolve path + workspace_id preferring querystring over body.
    /// Returns `(workspace_id, path)` with empty `path` if neither side
    /// supplied one. The caller is responsible for the "missing path"
    /// error.
    fn resolve_path_workspace(
        query_ws: Option<&str>,
        query_path: Option<&str>,
        body_ws: Option<&str>,
        body_path: Option<&str>,
    ) -> (Option<String>, String) {
        let workspace_id = query_ws
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| body_ws.filter(|s| !s.is_empty()).map(|s| s.to_string()));
        let path = query_path
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .or_else(|| body_path.filter(|s| !s.is_empty()).map(|s| s.to_string()))
            .unwrap_or_default();
        (workspace_id, path)
    }

    /// Resolve `(workspace_id, source, dest)` from a [`CopyMoveBody`].
    /// All three are workspace-relative paths; only `workspace_id` may
    /// be carried in the querystring (the desktop's `workspaceStore`
    /// puts it there). Returns empty strings if any path is missing —
    /// the trait method is responsible for the "missing source/dest"
    /// error so the message stays close to the field names.
    fn resolve_copy_move(body: &CopyMoveBody) -> (Option<String>, String, String) {
        let workspace_id = body.workspace_id.as_deref().and_then(|s| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        });
        let source = body.source.clone().unwrap_or_default();
        let dest = body.dest.clone().unwrap_or_default();
        (workspace_id, source, dest)
    }

    /// Recursively copy a directory tree. Returns the underlying I/O
    /// error message verbatim (no path leakage) so the trait method
    /// can wrap it in `WorkspaceError::Persist`.
    fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("failed to create destination directory: {}", e))?;
        for entry in
            std::fs::read_dir(src).map_err(|e| format!("failed to read source directory: {}", e))?
        {
            let entry = entry.map_err(|e| format!("failed to read entry: {}", e))?;
            let file_type = entry
                .file_type()
                .map_err(|e| format!("failed to read entry type: {}", e))?;
            let src_child = entry.path();
            // The file name alone is enough — `dst` is always absolute
            // (it comes from `resolve_within` joining onto the
            // workspace root).
            let dst_child = dst.join(entry.file_name());
            if file_type.is_dir() {
                Self::copy_dir_recursive(&src_child, &dst_child)?;
            } else if file_type.is_symlink() {
                // Re-materialise symlinks as their target bytes so the
                // copy is independent of the source filesystem. Avoids
                // dangling symlinks if the source is later deleted.
                let target = std::fs::read(&src_child)
                    .map_err(|e| format!("failed to read symlink target: {}", e))?;
                std::fs::write(&dst_child, &target)
                    .map_err(|e| format!("failed to write symlink target: {}", e))?;
            } else {
                std::fs::copy(&src_child, &dst_child)
                    .map_err(|e| format!("failed to copy file: {}", e))?;
            }
        }
        Ok(())
    }

    /// Resolve + path-traversal guard. Reuses the same logic as
    /// [`RuntimeWorkspaceQueryService::resolve_within`] so mutations
    /// and queries share a single security boundary.
    fn resolve_within(
        &self,
        workspace_id: Option<&str>,
        requested_path: &str,
    ) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
        if crate::usecases::package_workspace::is_package_workspace(workspace_id) {
            let package_dir = self.package_dir.as_deref().ok_or_else(|| {
                WorkspaceError::WorkspaceNotFound("__agent_package__".to_string())
            })?;
            return crate::usecases::package_workspace::resolve_within(package_dir, requested_path);
        }
        resolve_within_static(&self.work_dir, workspace_id, requested_path)
    }

    // ── Config (de)serialisation ──────────────────────────────────────

    fn workspaces_config_path(&self) -> PathBuf {
        self.work_dir.join("config").join("agent_workspaces.json")
    }

    /// Load the on-disk workspace config.
    ///
    /// Returns `Ok(Default)` when the file does not exist (first run).
    /// Any read/parse failure is surfaced as an error — **never** a
    /// silent empty fallback: a corrupt or schema-mismatched file must
    /// fail the mutation (500) rather than let `save_config` overwrite
    /// the user's workspace list with an empty one.
    fn load_config(&self) -> Result<WorkspacesConfig, WorkspaceError> {
        let path = self.workspaces_config_path();
        if !path.exists() {
            return Ok(WorkspacesConfig::default());
        }
        let content = std::fs::read_to_string(&path)
            .map_err(|e| WorkspaceError::Persist(format!("read {}: {}", path.display(), e)))?;
        let cfg: WorkspacesConfig = serde_json::from_str(&content)
            .map_err(|e| WorkspaceError::Persist(format!("parse {}: {}", path.display(), e)))?;
        Ok(cfg)
    }

    fn save_config(&self, cfg: &WorkspacesConfig) -> Result<(), WorkspaceError> {
        let dir = self.work_dir.join("config");
        std::fs::create_dir_all(&dir)
            .map_err(|e| WorkspaceError::Persist(format!("create config dir: {}", e)))?;
        let path = self.workspaces_config_path();
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(cfg).map_err(WorkspaceError::Json)?;
        std::fs::write(&tmp, &json)
            .map_err(|e| WorkspaceError::Persist(format!("write tmp: {}", e)))?;
        std::fs::rename(&tmp, &path)
            .map_err(|e| WorkspaceError::Persist(format!("rename tmp: {}", e)))?;
        // Fix-3 observability: log the on-disk path + entries count after
        // every workspace mutation so users reporting "workspace
        // disappeared on restart" can correlate the persisted file path
        // with the work_dir the runtime was actually using. See
        // `desktop-onboarding-bugfix_154b7ff7.md` §Fix 3.
        tracing::info!(
            work_dir = %self.work_dir.display(),
            config_path = %path.display(),
            additional_dirs = cfg.additional_dirs.len(),
            "persisted agent_workspaces.json after workspace mutation"
        );
        Ok(())
    }
}

// ── Workspace config on-disk schema ───────────────────────────────────────

/// On-disk schema for `agent_workspaces.json`.
///
/// Deliberately minimal: the old `version` field carried no logic
/// anywhere in the codebase (both readers ignored it), so it has been
/// removed. Unknown keys in existing files are ignored by serde during
/// deserialization, and `save_config` writes the normalized shape
/// without it.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct WorkspacesConfig {
    #[serde(default)]
    additional_dirs: Vec<serde_json::Value>,
}

// ── Standalone path-traversal guard (mirrors query impl) ──────────────────

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

pub(crate) fn resolve_workspace_root(
    work_dir: &Path,
    workspace_id: Option<&str>,
) -> Result<PathBuf, WorkspaceError> {
    let ws_id = match workspace_id {
        Some(id) if !id.is_empty() && id != "__agent_home__" => id,
        _ => return Ok(work_dir.to_path_buf()),
    };

    let config_path = work_dir.join("config").join("agent_workspaces.json");
    let content = std::fs::read_to_string(&config_path)
        .map_err(|_| WorkspaceError::WorkspaceNotFound(ws_id.to_string()))?;
    let val: serde_json::Value = serde_json::from_str(&content).map_err(WorkspaceError::Json)?;

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

/// Canonicalize + `starts_with` workspace-root traversal guard. Shared by
/// the workspace mutation handlers and the git query service (ADR-078
/// decision 3): `../`, absolute paths and symlink escapes are rejected.
pub(crate) fn resolve_within_static(
    work_dir: &Path,
    workspace_id: Option<&str>,
    requested_path: &str,
) -> Result<(PathBuf, PathBuf, String), WorkspaceError> {
    let workspace_root = resolve_workspace_root(work_dir, workspace_id)?;
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

// ── Trait impl ────────────────────────────────────��────────────────────────

#[async_trait]
impl WorkspaceMutationService for RuntimeWorkspaceMutationService {
    async fn create_workspace(
        &self,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        // ── Required-field validation ──────────────────────────────────
        //
        // `WorkspaceEntryInput` is shared with the partial-update route
        // (`PUT /workspaces/{ws_id}`), so the wire-level DTO keeps every
        // body field optional. For create, however, the desktop always
        // supplies `path` + `access`; anything else is a malformed
        // request and surfaces here as 400 before we touch the config.
        let path = entry
            .path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkspaceError::BadRequest {
                status: 400,
                message: "missing required field 'path'".to_string(),
            })?;
        let access = entry
            .access
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| WorkspaceError::BadRequest {
                status: 400,
                message: "missing required field 'access'".to_string(),
            })?;

        // ── Path sanity guard ──────────────────────────────────────────
        //
        // Restores the pre-ADR-040 Gateway-direct behaviour: refuse to
        // register a path that doesn't exist or isn't a directory. This
        // is both a UX hint (the desktop dialog should never silently
        // accept a typo) and a defensive measure — workspaces that
        // resolve to files or missing paths break `resolve_within` later
        // with cryptic canonicalize errors.
        let path_buf = PathBuf::from(path);
        let path_meta = std::fs::metadata(&path_buf).map_err(|e| WorkspaceError::BadRequest {
            status: 400,
            message: format!("path not accessible: {} ({})", path, e),
        })?;
        if !path_meta.is_dir() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: format!("path is not a directory: {}", path),
            });
        }

        // ── Id resolution ──────────────────────────────────────────────
        //
        // The runtime is the authoritative source of workspace IDs. If
        // the desktop omitted `id` (the normal case — restored from the
        // pre-ADR-040 contract), mint a fresh one. If the desktop did
        // supply one, enforce it and 409 on collision.
        let id = match entry.id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(client_id) => client_id.to_string(),
            None => Self::generate_workspace_id(),
        };

        let mut cfg = self.load_config()?;
        if cfg
            .additional_dirs
            .iter()
            .any(|d| d.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        {
            return Err(WorkspaceError::BadRequest {
                status: 409,
                message: format!("workspace id already exists: {}", id),
            });
        }

        // ── Persist the on-disk schema ─────────────────────────────────
        //
        // The fields written here match the pre-ADR-040 Gateway-direct
        // output so existing desktop selectors / file-tree components
        // (which already rely on `alias`, `added_at`, `select_count`,
        // `last_selected_at`) keep rendering new entries correctly.
        let mut new_entry = serde_json::json!({
            "id": id,
            "path": path,
            "access": access,
            "added_at": Self::now_rfc3339(),
            "last_active": entry.last_active.unwrap_or(false),
            "select_count": 0,
            "last_selected_at": serde_json::Value::Null,
        });
        // Optional fields — only write the key when the desktop
        // actually supplied it, so the on-disk file stays minimal for
        // users who don't use aliases / prompt-files.
        if let Some(alias) = entry
            .alias
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            new_entry["alias"] = serde_json::json!(alias);
        }
        if let Some(pf) = entry
            .prompt_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            new_entry["prompt_file"] = serde_json::json!(pf);
        }

        // Auto-inject a root-level `AGENTS.md` as the workspace prompt
        // file when the caller didn't specify one. AGENTS.md is the
        // de-facto project-prompt standard, so this is the sensible
        // default; users can still toggle it off from the file-tree
        // "取消注入上下文" menu (PUT /workspaces/{id}/prompt-file with a
        // null value). Defaults on create only — never on update/list —
        // so re-adding an edited path honors an explicit choice.
        //
        // `.is_file()` (not `.exists()`) guards against a directory that
        // happens to be named AGENTS.md. Case-insensitive filesystems
        // (Windows/macOS) match `agents.md` for free.
        let has_explicit_prompt = new_entry.get("prompt_file").is_some();
        if !has_explicit_prompt && path_buf.join("AGENTS.md").is_file() {
            new_entry["prompt_file"] = serde_json::json!("AGENTS.md");
        }

        cfg.additional_dirs.push(new_entry.clone());
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(new_entry),
        })
    }

    async fn update_workspace(
        &self,
        ws_id: &str,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config()?;
        let target = cfg
            .additional_dirs
            .iter_mut()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(ws_id))
            .ok_or_else(|| WorkspaceError::WorkspaceNotFound(ws_id.to_string()))?;

        // Partial-update semantics: every body field is optional, and we
        // only mutate the fields the desktop actually supplied. This
        // restores the pre-ADR-040 `UpdateWorkspaceRequest { access?,
        // alias? }` shape so the common `{access}` PUT (access-level
        // toggle in the desktop manager) keeps working without the
        // desktop having to re-send every other field.
        //
        // The `id` field is intentionally ignored — the URL path is the
        // authoritative selector. If the desktop happens to send a
        // different id we silently keep the original; this avoids a
        // 422 when an old desktop build happens to echo the path id
        // back, and matches the historical behaviour.
        if let Some(path) = entry
            .path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            target["path"] = serde_json::json!(path);
        }
        if let Some(access) = entry
            .access
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            target["access"] = serde_json::json!(access);
        }
        if let Some(alias) = entry
            .alias
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            target["alias"] = serde_json::json!(alias);
        }
        if let Some(pf) = entry
            .prompt_file
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            target["prompt_file"] = serde_json::json!(pf);
        }
        if let Some(la) = entry.last_active {
            target["last_active"] = serde_json::json!(la);
        }

        let updated = target.clone();
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(updated),
        })
    }

    async fn set_prompt_file(
        &self,
        ws_id: &str,
        body: PromptFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config()?;
        let target = cfg
            .additional_dirs
            .iter_mut()
            .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(ws_id))
            .ok_or_else(|| WorkspaceError::WorkspaceNotFound(ws_id.to_string()))?;
        target["prompt_file"] = serde_json::json!(body.prompt_file);
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: None,
        })
    }

    async fn delete_workspace(
        &self,
        ws_id: &str,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let mut cfg = self.load_config()?;
        let initial_len = cfg.additional_dirs.len();
        cfg.additional_dirs
            .retain(|d| d.get("id").and_then(|v| v.as_str()) != Some(ws_id));
        if cfg.additional_dirs.len() == initial_len {
            return Err(WorkspaceError::WorkspaceNotFound(ws_id.to_string()));
        }
        self.save_config(&cfg)?;
        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: None,
        })
    }

    async fn create_file(
        &self,
        body: CreateFileBody,
        query_ws: Option<&str>,
        query_path: Option<&str>,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            query_path,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) = self.resolve_within(workspace_id.as_deref(), &path)?;

        if abs_path.exists() && !body.overwrite {
            return Err(WorkspaceError::BadRequest {
                status: 409,
                message: format!("file already exists: {}", rel_path),
            });
        }
        if let Some(parent) = abs_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                WorkspaceError::Persist(format!("failed to create parent directory: {}", e))
            })?;
        }
        std::fs::write(&abs_path, body.content.as_bytes())
            .map_err(|e| WorkspaceError::Persist(format!("failed to write file: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"created": true, "path": rel_path})),
        })
    }

    async fn write_file(
        &self,
        query: FilePathQuery,
        body: crate::usecases::workspace_mutation::WriteFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (_root, abs_path, rel_path) =
            self.resolve_within(query.workspace_id.as_deref(), &query.path)?;

        if !abs_path.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "file does not exist: {}",
                rel_path
            )));
        }
        if abs_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "path is a directory; use DELETE /workspaces/dir instead: {}",
                rel_path
            )));
        }
        std::fs::write(&abs_path, body.content.as_bytes())
            .map_err(|e| WorkspaceError::Persist(format!("failed to write file: {}", e)))?;

        // ADR-058: echo the post-write disk metadata so the Desktop's
        // fileEditorStore can cache `diskModified` + `diskSize` right
        // after save (used for external-modification conflict checks).
        let (modified, size) = std::fs::metadata(&abs_path)
            .map(|m| (metadata_modified_rfc3339(&m), m.len()))
            .unwrap_or((None, body.content.len() as u64));

        let mut entry = serde_json::json!({"written": true, "path": rel_path, "size": size});
        if let Some(m) = modified {
            entry["modified"] = serde_json::Value::String(m);
        }

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(entry),
        })
    }

    async fn delete_file(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            None,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) = self.resolve_within(workspace_id.as_deref(), &path)?;

        if crate::usecases::package_workspace::is_package_workspace(workspace_id.as_deref())
            && crate::usecases::package_workspace::is_protected_root(&rel_path)
        {
            return Err(WorkspaceError::InvalidPath(
                "cannot delete prompts/ or skills/ root".to_string(),
            ));
        }
        if !abs_path.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "file does not exist: {}",
                rel_path
            )));
        }
        if abs_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "path is a directory; use DELETE /workspaces/dir instead: {}",
                rel_path
            )));
        }
        std::fs::remove_file(&abs_path)
            .map_err(|e| WorkspaceError::Persist(format!("failed to delete file: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"deleted": true, "path": rel_path})),
        })
    }

    async fn create_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            None,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) = self.resolve_within(workspace_id.as_deref(), &path)?;

        std::fs::create_dir_all(&abs_path)
            .map_err(|e| WorkspaceError::Persist(format!("failed to create directory: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"created": true, "path": rel_path})),
        })
    }

    async fn delete_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, path) = Self::resolve_path_workspace(
            query_ws,
            None,
            body.workspace_id.as_deref(),
            body.path.as_deref(),
        );
        if path.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'path' in querystring or body".to_string(),
            });
        }
        let (_root, abs_path, rel_path) = self.resolve_within(workspace_id.as_deref(), &path)?;
        if crate::usecases::package_workspace::is_package_workspace(workspace_id.as_deref())
            && crate::usecases::package_workspace::is_protected_root(&rel_path)
        {
            return Err(WorkspaceError::InvalidPath(
                "cannot delete prompts/ or skills/ root".to_string(),
            ));
        }

        if !abs_path.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "directory does not exist: {}",
                rel_path
            )));
        }
        if !abs_path.is_dir() {
            return Err(WorkspaceError::InvalidPath(format!(
                "path is a file; use DELETE /workspaces/file instead: {}",
                rel_path
            )));
        }
        std::fs::remove_dir_all(&abs_path)
            .map_err(|e| WorkspaceError::Persist(format!("failed to delete directory: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({"deleted": true, "path": rel_path})),
        })
    }

    async fn copy_item(
        &self,
        body: CopyMoveBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, source, dest) = Self::resolve_copy_move(&body);
        if source.is_empty() || dest.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'source' or 'dest' in body".to_string(),
            });
        }
        let (_root, abs_src, rel_src) = self.resolve_within(workspace_id.as_deref(), &source)?;
        let (_root, abs_dest, rel_dest) = self.resolve_within(workspace_id.as_deref(), &dest)?;
        if crate::usecases::package_workspace::is_package_workspace(workspace_id.as_deref()) {
            let src_root = rel_src.split('/').next();
            let dest_root = rel_dest.split('/').next();
            if src_root != dest_root
                || crate::usecases::package_workspace::is_protected_root(&rel_src)
                || crate::usecases::package_workspace::is_protected_root(&rel_dest)
            {
                return Err(WorkspaceError::InvalidPath(
                    "package operations must stay within prompts/ or skills/".to_string(),
                ));
            }
        }

        if !abs_src.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "source does not exist: {}",
                rel_src
            )));
        }
        if abs_dest.exists() {
            return Err(WorkspaceError::BadRequest {
                status: 409,
                message: format!("destination already exists: {}", rel_dest),
            });
        }
        if abs_src == abs_dest {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "source and destination are the same path".to_string(),
            });
        }
        // The destination's parent directory must exist on Unix —
        // `std::fs::copy` does not auto-create parents. Surface a
        // clearer error than letting the syscall return ENOENT.
        if let Some(parent) = abs_dest.parent()
            && !parent.exists()
        {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: format!("destination parent directory does not exist: {}", rel_dest),
            });
        }

        if abs_src.is_dir() {
            Self::copy_dir_recursive(&abs_src, &abs_dest)
                .map_err(|e| WorkspaceError::Persist(format!("copy failed: {}", e)))?;
        } else {
            std::fs::copy(&abs_src, &abs_dest)
                .map_err(|e| WorkspaceError::Persist(format!("failed to copy file: {}", e)))?;
        }

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({
                "copied": true,
                "source": rel_src,
                "dest": rel_dest,
            })),
        })
    }

    async fn rename_item(
        &self,
        body: CopyMoveBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError> {
        let (workspace_id, source, dest) = Self::resolve_copy_move(&body);
        if source.is_empty() || dest.is_empty() {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "missing 'source' or 'dest' in body".to_string(),
            });
        }
        let (_root, abs_src, rel_src) = self.resolve_within(workspace_id.as_deref(), &source)?;
        let (_root, abs_dest, rel_dest) = self.resolve_within(workspace_id.as_deref(), &dest)?;
        if crate::usecases::package_workspace::is_package_workspace(workspace_id.as_deref()) {
            let src_root = rel_src.split('/').next();
            let dest_root = rel_dest.split('/').next();
            if src_root != dest_root
                || crate::usecases::package_workspace::is_protected_root(&rel_src)
                || crate::usecases::package_workspace::is_protected_root(&rel_dest)
            {
                return Err(WorkspaceError::InvalidPath(
                    "package operations must stay within prompts/ or skills/".to_string(),
                ));
            }
        }

        if !abs_src.exists() {
            return Err(WorkspaceError::NotFound(format!(
                "source does not exist: {}",
                rel_src
            )));
        }
        if abs_dest.exists() {
            return Err(WorkspaceError::BadRequest {
                status: 409,
                message: format!("destination already exists: {}", rel_dest),
            });
        }
        if abs_src == abs_dest {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: "source and destination are the same path".to_string(),
            });
        }
        // `std::fs::rename` is atomic on the same filesystem and falls
        // back to copy+delete on cross-filesystem moves. Either way it
        // requires the destination's parent directory to exist on Unix.
        if let Some(parent) = abs_dest.parent()
            && !parent.exists()
        {
            return Err(WorkspaceError::BadRequest {
                status: 400,
                message: format!("destination parent directory does not exist: {}", rel_dest),
            });
        }

        std::fs::rename(&abs_src, &abs_dest)
            .map_err(|e| WorkspaceError::Persist(format!("failed to rename: {}", e)))?;

        Ok(WorkspaceMutationResponse {
            ok: true,
            entry: Some(serde_json::json!({
                "renamed": true,
                "source": rel_src,
                "dest": rel_dest,
            })),
        })
    }
}

// ── FilePathQuery note ─────────────────────────────────────────────────────
//
// `write_file` in this trait accepts `FilePathQuery` (defined in
// `workspace_mutation.rs`). The query service has its own
// `FilePathQuery` with identical fields — keeping them separate avoids
// a circular trait ↔ impl dependency between the two services. The HTTP
// layer maps a single `Query<FilePathQuery>` extractor into whichever
// type the handler needs.
//
// We intentionally do not add a `From` impl between the two: callers
// pass `FilePathQuery` directly via the HTTP layer's `Json` body /
// `Query` extractor, and the runtime never constructs one from the other.

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecases::workspace_mutation::WorkspaceEntryInput;
    use tempfile::tempdir;

    /// Build a workspace service pointed at a temp dir so the
    /// `agent_workspaces.json` it manages is fully isolated per test.
    fn make_service(dir: &tempfile::TempDir) -> RuntimeWorkspaceMutationService {
        RuntimeWorkspaceMutationService::new(dir.path().to_path_buf())
    }

    #[tokio::test]
    async fn package_workspace_allows_edits_but_blocks_escape_and_root_delete() {
        let temp = tempdir().expect("tempdir");
        let work = temp.path().join("work");
        let package = temp.path().join("package");
        std::fs::create_dir_all(work.join("config")).unwrap();
        std::fs::create_dir_all(package.join("prompts")).unwrap();
        std::fs::create_dir_all(package.join("skills")).unwrap();
        std::fs::write(package.join("prompts/system.md"), "old").unwrap();
        std::fs::write(package.join("manifest.toml"), "private").unwrap();
        let svc = RuntimeWorkspaceMutationService::new_with_package_dir(work, package.clone());

        svc.write_file(
            FilePathQuery {
                workspace_id: Some("__agent_package__".into()),
                path: "prompts/system.md".into(),
            },
            crate::usecases::workspace_mutation::WriteFileBody {
                content: "new".into(),
            },
        )
        .await
        .expect("write allowed prompt");
        assert_eq!(
            std::fs::read_to_string(package.join("prompts/system.md")).unwrap(),
            "new"
        );

        let outside = svc
            .create_file(
                CreateFileBody {
                    workspace_id: Some("__agent_package__".into()),
                    path: Some("manifest.toml".into()),
                    content: "overwrite".into(),
                    overwrite: true,
                },
                None,
                None,
            )
            .await
            .expect_err("package metadata must be inaccessible");
        assert_eq!(outside.http_status(), 400);

        let root_delete = svc
            .delete_dir(
                Some("__agent_package__"),
                PathOnlyBody {
                    workspace_id: None,
                    path: Some("prompts".into()),
                },
            )
            .await
            .expect_err("top-level package subtree is protected");
        assert_eq!(root_delete.http_status(), 400);
        assert!(package.join("prompts/system.md").exists());
    }

    /// Smoke test for the create / read round-trip on the partial-update
    /// path: a `{path, access}` body without `id` is the original
    /// pre-ADR-040 Gateway-direct contract and must succeed.
    #[tokio::test]
    async fn create_workspace_minimal_payload_succeeds() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let resp = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(dir.path().to_string_lossy().to_string()),
                access: Some("read-only".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect("create_workspace succeeds");

        let entry = resp.entry.expect("entry returned on create");
        let id = entry
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id assigned");
        assert!(
            id.starts_with("ws-") && id.len() == "ws-".len() + 12,
            "id should be ws-<12 hex chars>, got: {id}"
        );
        assert_eq!(
            entry.get("path").and_then(|v| v.as_str()),
            Some(dir.path().to_string_lossy().to_string()).as_deref(),
        );
        assert_eq!(
            entry.get("access").and_then(|v| v.as_str()),
            Some("read-only")
        );
        assert_eq!(
            entry.get("last_active").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert_eq!(entry.get("select_count").and_then(|v| v.as_u64()), Some(0));
        assert!(
            entry.get("added_at").and_then(|v| v.as_str()).is_some(),
            "added_at must be set on create"
        );
        // Optional fields must be omitted (not present as `null`) when
        // the desktop didn't supply them — keeps the on-disk file
        // minimal and avoids serde round-trip ambiguity.
        assert!(
            entry.get("alias").is_none(),
            "alias should be absent when not supplied"
        );
        assert!(
            entry.get("prompt_file").is_none(),
            "prompt_file should be absent when not supplied"
        );
    }

    /// Creating a workspace whose root contains `AGENTS.md` (with no
    /// explicit `prompt_file`) must auto-inject it as the prompt file.
    #[tokio::test]
    async fn create_workspace_auto_injects_root_agents_md() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        // A real workspace subdir holding a root-level AGENTS.md.
        let ws_dir = dir.path().join("repo");
        std::fs::create_dir_all(&ws_dir).expect("create ws dir");
        std::fs::write(ws_dir.join("AGENTS.md"), "# agents\nbe terse").expect("write AGENTS.md");

        let resp = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(ws_dir.to_string_lossy().to_string()),
                access: Some("read-write".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect("create_workspace succeeds");

        let entry = resp.entry.expect("entry returned on create");
        assert_eq!(
            entry.get("prompt_file").and_then(|v| v.as_str()),
            Some("AGENTS.md"),
            "root AGENTS.md must be auto-injected as prompt_file",
        );
    }

    /// An explicit `prompt_file` from the caller must win over the
    /// AGENTS.md default (including an explicit choice of a different
    /// file, e.g. CLAUDE.md).
    #[tokio::test]
    async fn create_workspace_explicit_prompt_file_wins_over_agents_md() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let ws_dir = dir.path().join("repo");
        std::fs::create_dir_all(&ws_dir).expect("create ws dir");
        std::fs::write(ws_dir.join("AGENTS.md"), "# agents").expect("write AGENTS.md");
        std::fs::write(ws_dir.join("CLAUDE.md"), "# claude").expect("write CLAUDE.md");

        let resp = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(ws_dir.to_string_lossy().to_string()),
                access: Some("read-write".to_string()),
                alias: None,
                prompt_file: Some("CLAUDE.md".to_string()),
                last_active: None,
            })
            .await
            .expect("create_workspace succeeds");

        let entry = resp.entry.expect("entry returned on create");
        assert_eq!(
            entry.get("prompt_file").and_then(|v| v.as_str()),
            Some("CLAUDE.md"),
            "explicit prompt_file must not be clobbered by the AGENTS.md default",
        );
    }

    /// A directory named `AGENTS.md` must NOT trigger auto-injection
    /// (`.is_file()` guard).
    #[tokio::test]
    async fn create_workspace_ignores_agents_md_directory() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let ws_dir = dir.path().join("repo");
        std::fs::create_dir_all(ws_dir.join("AGENTS.md")).expect("create AGENTS.md dir");

        let resp = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(ws_dir.to_string_lossy().to_string()),
                access: Some("read-write".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect("create_workspace succeeds");

        let entry = resp.entry.expect("entry returned on create");
        assert!(
            entry.get("prompt_file").is_none(),
            "a directory named AGENTS.md must not be injected",
        );
    }

    /// The frontend's `WorkspaceManager` ships `{path, alias, access}`
    /// — no `id`. This test pins that exact payload (the regression
    /// that prompted this fix) so we never regress to a 422 again.
    #[tokio::test]
    async fn create_workspace_legacy_payload_persists_alias_and_id() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let resp = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(dir.path().to_string_lossy().to_string()),
                access: Some("read-write".to_string()),
                alias: Some("legacy".to_string()),
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect("legacy {path, alias, access} payload must succeed");

        let entry = resp.entry.expect("entry returned on create");
        assert_eq!(entry.get("alias").and_then(|v| v.as_str()), Some("legacy"));
        assert_eq!(
            entry.get("access").and_then(|v| v.as_str()),
            Some("read-write")
        );
        assert!(
            entry
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.starts_with("ws-"))
                .unwrap_or(false),
            "id must be server-generated when client omits it"
        );
    }

    /// When the desktop supplies a client-side id, the runtime must
    /// honour it AND reject duplicates with a 409.
    #[tokio::test]
    async fn create_workspace_rejects_duplicate_client_id() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let first = svc
            .create_workspace(WorkspaceEntryInput {
                id: Some("ws-fixed".to_string()),
                path: Some(dir.path().to_string_lossy().to_string()),
                access: Some("read-only".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect("first insert succeeds");

        assert_eq!(
            first.entry.unwrap().get("id").and_then(|v| v.as_str()),
            Some("ws-fixed")
        );

        let err = svc
            .create_workspace(WorkspaceEntryInput {
                id: Some("ws-fixed".to_string()),
                path: Some(dir.path().to_string_lossy().to_string()),
                access: Some("read-only".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect_err("duplicate id must fail");

        match err {
            WorkspaceError::BadRequest { status, .. } => assert_eq!(status, 409),
            other => panic!("expected BadRequest 409, got {other:?}"),
        }
    }

    /// Required-field validation must surface as 400 BEFORE any path /
    /// config side effects, mirroring the old Gateway-direct contract.
    #[tokio::test]
    async fn create_workspace_missing_required_fields_is_400() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let no_path = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: None,
                access: Some("read-only".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect_err("missing path must fail");
        match no_path {
            WorkspaceError::BadRequest { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("path"), "msg = {message}");
            }
            other => panic!("expected BadRequest 400 for missing path, got {other:?}"),
        }

        let no_access = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(dir.path().to_string_lossy().to_string()),
                access: None,
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect_err("missing access must fail");
        match no_access {
            WorkspaceError::BadRequest { status, message } => {
                assert_eq!(status, 400);
                assert!(message.contains("access"), "msg = {message}");
            }
            other => panic!("expected BadRequest 400 for missing access, got {other:?}"),
        }
    }

    /// Pointing at a file (or non-existent path) must fail with 400 —
    /// reproduces the pre-ADR-040 `is_dir()` guard.
    #[tokio::test]
    async fn create_workspace_rejects_non_directory_path() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("not-a-dir.txt");
        std::fs::write(&file, "x").expect("write file");
        let svc = make_service(&dir);

        let err = svc
            .create_workspace(WorkspaceEntryInput {
                id: None,
                path: Some(file.to_string_lossy().to_string()),
                access: Some("read-only".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect_err("non-directory must fail");

        match err {
            WorkspaceError::BadRequest { status, message } => {
                assert_eq!(status, 400);
                assert!(
                    message.contains("not a directory"),
                    "msg should explain the failure, got: {message}"
                );
            }
            other => panic!("expected BadRequest 400, got {other:?}"),
        }
    }

    /// Regression for the PUT path: the desktop sends only `{access}`
    /// when toggling read-only / read-write in the manager. The impl
    /// must NOT clobber `path` / `id` / `alias` / `prompt_file` when
    /// they're absent from the body.
    #[tokio::test]
    async fn update_workspace_access_only_does_not_clobber_other_fields() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        // Seed an entry with the full on-disk schema so we can detect
        // any field clobbering on update.
        let created = svc
            .create_workspace(WorkspaceEntryInput {
                id: Some("ws-keep".to_string()),
                path: Some(dir.path().to_string_lossy().to_string()),
                access: Some("read-only".to_string()),
                alias: Some("keep-me".to_string()),
                prompt_file: Some("AGENTS.md".to_string()),
                last_active: Some(false),
            })
            .await
            .expect("create");
        let created_id = created
            .entry
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .expect("id")
            .to_string();

        // Desktop toggles to read-write — sends ONLY `{access}`.
        let updated = svc
            .update_workspace(
                &created_id,
                WorkspaceEntryInput {
                    id: None,
                    path: None,
                    access: Some("read-write".to_string()),
                    alias: None,
                    prompt_file: None,
                    last_active: None,
                },
            )
            .await
            .expect("access-only update must succeed");

        let entry = updated.entry.expect("entry returned on update");
        assert_eq!(
            entry.get("access").and_then(|v| v.as_str()),
            Some("read-write")
        );
        // All other fields MUST be preserved.
        assert_eq!(entry.get("id").and_then(|v| v.as_str()), Some("ws-keep"));
        assert_eq!(
            entry.get("path").and_then(|v| v.as_str()),
            Some(dir.path().to_string_lossy().to_string()).as_deref(),
        );
        assert_eq!(entry.get("alias").and_then(|v| v.as_str()), Some("keep-me"));
        assert_eq!(
            entry.get("prompt_file").and_then(|v| v.as_str()),
            Some("AGENTS.md")
        );
    }

    /// Alias update via the dedicated `PUT /workspaces/{id}` body — the
    /// alias-only branch of `update_workspace`.
    #[tokio::test]
    async fn update_workspace_persists_alias_change() {
        let dir = tempdir().expect("tempdir");
        let svc = make_service(&dir);

        let created = svc
            .create_workspace(WorkspaceEntryInput {
                id: Some("ws-alias".to_string()),
                path: Some(dir.path().to_string_lossy().to_string()),
                access: Some("read-only".to_string()),
                alias: Some("old".to_string()),
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect("create");
        let id = created
            .entry
            .as_ref()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .expect("id")
            .to_string();

        let updated = svc
            .update_workspace(
                &id,
                WorkspaceEntryInput {
                    id: None,
                    path: None,
                    access: None,
                    alias: Some("new".to_string()),
                    prompt_file: None,
                    last_active: None,
                },
            )
            .await
            .expect("alias-only update must succeed");

        assert_eq!(
            updated.entry.unwrap().get("alias").and_then(|v| v.as_str()),
            Some("new")
        );
    }

    /// Internal sanity: id generator matches the pre-ADR-040 format.
    #[test]
    fn generate_workspace_id_format() {
        let id = RuntimeWorkspaceMutationService::generate_workspace_id();
        assert!(id.starts_with("ws-"), "must start with ws-, got: {id}");
        let suffix = &id["ws-".len()..];
        assert_eq!(suffix.len(), 12, "suffix must be 12 chars, got: {suffix}");
        assert!(
            suffix
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "suffix must be lowercase hex, got: {suffix}"
        );
        // Two consecutive calls must produce distinct ids.
        let id2 = RuntimeWorkspaceMutationService::generate_workspace_id();
        assert_ne!(id, id2, "uuid v4 must produce distinct ids");
    }

    /// Regression for the "adding a workspace wipes the existing list"
    /// bug: a legacy `agent_workspaces.json` that carries a `version`
    /// key (any shape — the schema no longer has that field) must load
    /// without error, and `create_workspace` must **append** rather than
    /// replace the whole file.
    ///
    /// Before the fix, `WorkspacesConfig` required `version: u32`, so a
    /// file written with `"version": "1.0.0"` failed deserialization and
    /// `load_config()` silently returned an empty config — the next
    /// create persisted only the new entry, wiping every existing
    /// workspace.
    #[tokio::test]
    async fn create_workspace_preserves_legacy_config_with_version_key() {
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        let existing = serde_json::json!({
            "id": "ws-keepme",
            "path": "/keep/me",
            "access": "read-only",
            "added_at": "2026-01-01T00:00:00Z",
            "last_active": false,
            "select_count": 0,
            "last_selected_at": null,
        });
        // Legacy shape: string version + one pre-existing workspace.
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            serde_json::to_string_pretty(&serde_json::json!({
                "version": "1.0.0",
                "additional_dirs": [existing],
            }))
            .unwrap(),
        )
        .unwrap();

        let new_path = dir.path().join("new-dir");
        std::fs::create_dir_all(&new_path).unwrap();

        let svc = make_service(&dir);
        svc.create_workspace(WorkspaceEntryInput {
            id: Some("ws-new".to_string()),
            path: Some(new_path.to_string_lossy().to_string()),
            access: Some("read-write".to_string()),
            alias: None,
            prompt_file: None,
            last_active: None,
        })
        .await
        .expect("create must succeed on legacy config");

        // Both the pre-existing and the newly-created workspace survive.
        let on_disk: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("config").join("agent_workspaces.json"))
                .unwrap(),
        )
        .unwrap();
        let ids: Vec<&str> = on_disk["additional_dirs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["ws-keepme", "ws-new"]);
        // The normalized schema no longer writes a version key.
        assert!(
            on_disk.get("version").is_none(),
            "saved config should be normalized (no version key), got: {on_disk}"
        );
    }

    /// A corrupt config file must fail the mutation with an error — not
    /// silently fall back to an empty config and overwrite user data.
    #[tokio::test]
    async fn create_workspace_fails_on_corrupt_config() {
        let dir = tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(
            dir.path().join("config").join("agent_workspaces.json"),
            "{ this is not json",
        )
        .unwrap();

        let new_path = dir.path().join("new-dir");
        std::fs::create_dir_all(&new_path).unwrap();

        let svc = make_service(&dir);
        let err = svc
            .create_workspace(WorkspaceEntryInput {
                id: Some("ws-new".to_string()),
                path: Some(new_path.to_string_lossy().to_string()),
                access: Some("read-write".to_string()),
                alias: None,
                prompt_file: None,
                last_active: None,
            })
            .await
            .expect_err("corrupt config must fail, not wipe the file");

        match err {
            WorkspaceError::Persist(_) => {}
            other => panic!("expected Persist error for corrupt config, got {other:?}"),
        }
    }
}
