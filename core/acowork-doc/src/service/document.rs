//! `DocumentService` trait — business operations on a single document.
//!
//! Audit point for optimistic-version concurrency (design §4 PUT / §5.4).
//! Every mutation that targets an existing document carries the caller's
//! `base_version`; a mismatch returns `DocError::VersionConflict` with the
//! `current_version` so the caller can decide whether to rebase or surface
//! a UI merge prompt.
//!
//! Implementations live in `document_impl.rs`; the API layer calls these
//! trait methods only.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{Ace, DocMeta, DocRead, ImportSource};

/// Input for creating a new document.
///
/// `parent_dir_id` is the directory to host the file. Title becomes both
/// the filename (sans `.md`) and `DocMeta.name` — they must agree at all
/// times (design §3.3).
#[derive(Debug, Clone)]
pub struct CreateDocumentInput {
    pub parent_dir_id: String,
    pub title: String,
    pub content: String,
    pub import: Option<ImportSource>,
    /// Trusted identity of the caller (design §9) — recorded on the new
    /// node so its creator may manage the ACL. `None` for synthesised /
    /// pre-ACL creations.
    pub creator: Option<String>,
}

/// Input for `DocumentService::update`.
///
/// `base_version` is the caller's view of the document version (design
/// §4). When it matches the on-disk version, the update is applied and
/// the new version is `base_version + 1`. When it doesn't, the call
/// fails with `DocError::VersionConflict { base_version, current_version }`.
#[derive(Debug, Clone)]
pub struct UpdateDocumentInput {
    pub base_version: u64,
    /// Optional new title (must remain equal to the filename stem).
    /// `None` keeps the existing title.
    pub title: Option<String>,
    pub content: String,
}

/// Service-trait contract. Methods are `async`; `Send + Sync` is required
/// so the impl can be shared via `Arc<dyn DocumentService>` in axum state.
#[async_trait]
pub trait DocumentService: Send + Sync {
    /// Look up a document by id and return its metadata + body.
    async fn read(&self, doc_id: &str) -> Result<DocRead>;

    /// List documents under a directory (active / non-deleted only).
    async fn list(&self, dir_id: &str) -> Result<Vec<DocMeta>>;

    /// Create a new document under `parent_dir_id`. Generates the
    /// `doc_id` server-side; the title must be a legal filename (see
    /// `path::validate_doc_name`).
    async fn create(&self, input: CreateDocumentInput) -> Result<DocMeta>;

    /// Apply a content update guarded by `base_version`. On mismatch
    /// returns `DocError::VersionConflict` with the current version so
    /// the caller can decide whether to rebase. May optionally rename
    /// the document in the same call (atomic: title + filename + meta
    /// move together).
    async fn update(&self, doc_id: &str, input: UpdateDocumentInput) -> Result<DocMeta>;

    /// Rename a document (both `DocMeta.name` and the `.md` filename
    /// move together; atomic on disk via the impl). Version-checked —
    /// stale `base_version` returns `DocError::VersionConflict`.
    async fn rename(
        &self,
        doc_id: &str,
        new_title: &str,
        base_version: u64,
    ) -> Result<DocMeta>;

    /// Move a document to another directory. Cross-directory atomicity
    /// is the impl's responsibility (target first, source cleanup second,
    /// rollback on failure). When `overwrite` is `false` and the target
    /// has a doc with the same title, the call fails with `NameConflict`.
    async fn move_doc(
        &self,
        doc_id: &str,
        target_dir_id: &str,
        overwrite: bool,
    ) -> Result<DocMeta>;

    /// Delete a document: write `.md` to `.trash/` (or just unlink in v1)
    /// and mark `DocMeta.deleted = true`. The directory's `library.json`
    /// is rewritten atomically.
    async fn delete(&self, doc_id: &str) -> Result<()>;

    /// Re-apply an explicit ACL onto an existing document **without** the
    /// management gate. Used only by the restore path (a trash slot
    /// re-creates the doc, then reattaches its snapshotted ACL) — the
    /// user-facing ACL editor goes through `AclService::set`, which does
    /// enforce `manage`. `None` leaves the node inheriting.
    async fn set_restored_acl(&self, doc_id: &str, acl: Option<Vec<Ace>>) -> Result<()>;

    /// Absolute on-disk path of the document, relative to the library
    /// root (e.g. `"项目A/PRD.md"`). Used by search and move logic.
    async fn path_of(&self, doc_id: &str) -> Result<String>;
}
