//! `DirectoryService` trait — operations on a directory node.
//!
//! Covers the directory half of design §4 (POST/PATCH/DELETE on
//! `/api/dirs/:id`). Document CRUD lives in `document.rs`; both
//! implementations share the same `LibraryStore` instance so the
//! per-directory `library.json` mutations stay consistent.

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{DirMeta, TreeNode};

#[derive(Debug, Clone)]
pub struct CreateDirectoryInput {
    pub parent_dir_id: String,
    pub name: String,
    /// Trusted identity of the caller (design §9) — recorded as the new
    /// directory's creator so it may manage the ACL.
    pub creator: Option<String>,
    /// Library icon reference. Only used for top-level nodes (libraries);
    /// a nested directory ignores it.
    pub icon: Option<String>,
    /// Library description. Same top-level-only scope as `icon`.
    pub description: Option<String>,
}

/// Partial metadata update for a directory / library.
///
/// Double options distinguish three intents per field: `None` = leave
/// untouched, `Some(None)` = clear, `Some(Some(v))` = set. The HTTP layer
/// maps an absent JSON key to `None` and an explicit `null` to
/// `Some(None)` — see `api::libraries`.
#[derive(Debug, Clone, Default)]
pub struct UpdateDirectoryMeta {
    pub name: Option<String>,
    pub icon: Option<Option<String>>,
    pub description: Option<Option<String>>,
    /// Transfer ownership: rewrites the node's `creator`, the value the ACL
    /// manage gate trusts. Gated on the current owner / an admin by the
    /// API layer.
    pub owner: Option<String>,
}

#[async_trait]
pub trait DirectoryService: Send + Sync {
    /// Create a new subdirectory under `parent_dir_id`. Materialises the
    /// on-disk directory and writes its `library.json` (parent's
    /// `dirs[]` is also updated).
    async fn create(&self, input: CreateDirectoryInput) -> Result<DirMeta>;

    /// Read a directory's metadata.
    async fn read(&self, dir_id: &str) -> Result<DirMeta>;

    /// List the immediate children of `dir_id` (files + sub-dirs).
    async fn list_tree(&self, dir_id: &str) -> Result<TreeNode>;

    /// Rename a directory (both the physical folder and the parent's
    /// `dirs[]` entry move together).
    async fn rename(&self, dir_id: &str, new_name: &str) -> Result<DirMeta>;

    /// Apply a partial metadata update (name / icon / description / owner).
    /// Used by the library surface and by `rename` itself, so both paths
    /// share one write routine and cannot drift.
    async fn update_meta(
        &self,
        dir_id: &str,
        patch: UpdateDirectoryMeta,
    ) -> Result<DirMeta>;

    /// Delete a directory and every document inside it (cascade into
    /// `.trash/`). Implemented in PR-1 with single-directory .trash;
    /// cross-directory restore comes with the recycle-bin service in PR-2.
    async fn delete(&self, dir_id: &str) -> Result<()>;

    /// Human-readable path of a directory relative to the library root
    /// (`"研发/会议"`, `""` for the root).
    ///
    /// Resolved by walking **up** the `parent` chain, so the cost is
    /// O(depth) index reads. The alternative — walking the whole tree down
    /// from the root — costs O(directories) per lookup, i.e. a full library
    /// scan to render one `mv` result path. Callers that render many paths in
    /// one request (search hits) memoise the per-directory result for the
    /// duration of that request; there is deliberately no cross-request cache,
    /// because every directory mutation would then owe an invalidation hook.
    async fn path_of(&self, dir_id: &str) -> Result<String>;
}
