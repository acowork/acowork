//! Workspace mutation use case.
//!
//! ADR-040: write-side workspace operations live behind a trait so the
//! HTTP handlers (server.rs) don't directly mutate `agent_workspaces.json`
//! or touch the filesystem. The implementation
//! [`crate::usecases::RuntimeWorkspaceMutationService`] owns the only
//! `write-rename` path for the config and the only file/dir mutation
//! calls, keeping a single audit point for path-traversal guards and
//! atomic persistence.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::usecases::workspace_query::WorkspaceError;

// ── Request DTOs ───────────────────────────────────────────────────────────

/// One workspace entry as submitted by the desktop.
///
/// `access` is `"read-only"` / `"read-write"` (matches the gateway's
/// `AccessLevel` kebab-case serialisation); unknown variants surface as
/// `WorkspaceError::InvalidPath`.
///
/// `id` is **optional** — the runtime is the authoritative source of
/// workspace IDs and generates a fresh `ws-` + 12 hex chars when the
/// desktop omits it (e.g. on `POST /workspaces` for a brand-new entry).
/// This restores the pre-ADR-040 Gateway-direct contract where the
/// server was the sole owner of `id` / `added_at`.
///
/// `path` and `access` are **optional** at the wire level so the same
/// DTO can serve both `POST /workspaces` (both required — enforced by
/// the impl) and `PUT /workspaces/{ws_id}` (partial update — desktop
/// sends only the fields it wants to change, e.g. `{access}` for an
/// access-level toggle).
///
/// `alias` is the optional human-friendly label shown in the desktop
/// selector / file tree; persisted as-is so round trips preserve it.
/// `None` on create / unset-on-update — the impl only writes the field
/// when `Some` is supplied, matching the existing `prompt_file` /
/// `last_active` semantics.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceEntryInput {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub access: Option<String>,
    #[serde(default)]
    pub alias: Option<String>,
    #[serde(default)]
    pub prompt_file: Option<String>,
    #[serde(default)]
    pub last_active: Option<bool>,
}

/// Response for `create_workspace` / `update_workspace`.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceMutationResponse {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Echo of the resulting entry as it was persisted (for `create`)
    /// or its prior value (for `update`). Kept as raw JSON so
    /// newly-added fields survive round trips.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry: Option<serde_json::Value>,
}

/// Request body for `set_prompt_file`.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PromptFileBody {
    #[serde(default)]
    pub prompt_file: Option<String>,
}

/// Body for `create_file`. Path + workspace_id can live in the
/// querystring OR the body (querystring wins if both are set) — this
/// matches the desktop's `buildFileUrl` helper and is documented at
/// the HTTP layer too.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CreateFileBody {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub overwrite: bool,
}

/// Body for `write_file` (only `content` is in the body — path +
/// workspace_id are in the querystring).
///
/// `contentBase64` carries binary bytes instead of `content` as text — the
/// write-back path of a hosted app's visual canvas (ADR-121): WordCraft
/// hands back a `.docx` byte array, not a string. When both fields are
/// present `contentBase64` wins.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WriteFileBody {
    #[serde(default)]
    pub content: String,
    #[serde(default, rename = "contentBase64")]
    pub content_base64: Option<String>,
}

impl WriteFileBody {
    /// The bytes to write: the base64 field decoded, or the text as UTF-8.
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        use base64::Engine as _;
        match self.content_base64.as_deref().filter(|s| !s.is_empty()) {
            Some(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64)
                .map_err(|e| format!("invalid 'contentBase64': {e}")),
            None => Ok(self.content.clone().into_bytes()),
        }
    }
}

/// Body for `delete_file` + `create_dir` + `delete_dir`.
/// `path` and `workspace_id` may be in querystring or body (querystring
/// wins).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PathOnlyBody {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// Body for `copy_item` + `rename_item`. Both endpoints accept the same
/// payload shape — a source path, a destination path, and an optional
/// `workspace_id` selector (querystring wins; the body is the fallback
/// for clients that can't carry one on DELETE-style calls, even though
/// these are POST).
///
/// `source` and `dest` are workspace-relative paths (same shape as
/// `PathOnlyBody::path`). They MUST stay inside the resolved workspace
/// root — the implementation's `resolve_within` enforces this with the
/// canonicalize-based path-traversal guard.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct CopyMoveBody {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub dest: Option<String>,
}

/// Querystring for `read_file` / `write_file` / `delete_file` (path is
/// here so DELETE bodies aren't required for HTTP clients that can't
/// carry one).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct FilePathQuery {
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub path: String,
}

// ── Trait ──────────────────────────────────────────────────────────────────

/// Write-side workspace operations.
///
/// Atomicity: every mutation persists the workspace config via the
/// `write-tmp-rename` pattern, so concurrent updates from the desktop
/// + Gateway CLI can't lose data. File / dir mutations are bare
/// `std::fs` calls — the path-traversal guard lives in the
/// implementation (see [`crate::usecases::RuntimeWorkspaceMutationService`]).
#[async_trait]
pub trait WorkspaceMutationService: Send + Sync {
    /// `POST /workspaces` — add a new workspace entry.
    /// Returns `AlreadyExists` if `id` collides.
    async fn create_workspace(
        &self,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `PUT /workspaces/{ws_id}` — update an existing workspace entry.
    /// Returns `WorkspaceNotFound` if `ws_id` is unknown.
    async fn update_workspace(
        &self,
        ws_id: &str,
        entry: WorkspaceEntryInput,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `PUT /workspaces/{ws_id}/prompt-file` — set the prompt_file
    /// field on a workspace entry (or clear it via `null`).
    async fn set_prompt_file(
        &self,
        ws_id: &str,
        body: PromptFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `DELETE /workspaces/{ws_id}` — remove a workspace entry.
    async fn delete_workspace(
        &self,
        ws_id: &str,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/file` — create a new text file.
    /// Returns `AlreadyExists` if the file exists and `overwrite=false`.
    async fn create_file(
        &self,
        body: CreateFileBody,
        query_ws: Option<&str>,
        query_path: Option<&str>,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `PUT /workspaces/file` — overwrite an existing text file.
    /// Returns `NotFound` if the file is missing.
    async fn write_file(
        &self,
        query: FilePathQuery,
        body: WriteFileBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/uploads/{upload_id}/complete` — replace an existing
    /// file with an already-merged staging file (ADR-122 §3 / §4.2).
    ///
    /// The hosted-app save path: the staged bytes are `rename`d onto the
    /// target, so the file is never truncated first and a crash mid-save
    /// leaves the previous content intact. Still resolves through the same
    /// traversal guard as `write_file`, and still answers `NotFound` for a
    /// missing target (`PUT /workspaces/file` only overwrites; a Save As
    /// creates the file first).
    async fn write_file_from_staged(
        &self,
        query: FilePathQuery,
        staged: std::path::PathBuf,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `DELETE /workspaces/file` — remove a file.
    async fn delete_file(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/dir` — create a directory (recursive).
    async fn create_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `DELETE /workspaces/dir` — remove a directory recursively.
    async fn delete_dir(
        &self,
        query_ws: Option<&str>,
        body: PathOnlyBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/copy` — copy a file or directory tree to a new
    /// location inside the same workspace. Both paths must resolve under
    /// the workspace root (`resolve_within` enforces the traversal
    /// guard). If `dest` already exists, returns `AlreadyExists` so the
    /// desktop can surface a clear "destination exists" error rather
    /// than overwriting user data silently.
    async fn copy_item(
        &self,
        body: CopyMoveBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;

    /// `POST /workspaces/rename` — atomically rename (or move) a file or
    /// directory inside the same workspace. Both paths must resolve
    /// under the workspace root. If `dest` already exists, returns
    /// `AlreadyExists` for the same reason as `copy_item`. The
    /// underlying syscall (`std::fs::rename`) is atomic on the same
    /// filesystem; cross-filesystem moves fall back to copy+delete at
    /// the OS layer.
    async fn rename_item(
        &self,
        body: CopyMoveBody,
    ) -> Result<WorkspaceMutationResponse, WorkspaceError>;
}
