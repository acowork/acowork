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

    /// Delete a directory and every document inside it (cascade into
    /// `.trash/`). Implemented in PR-1 with single-directory .trash;
    /// cross-directory restore comes with the recycle-bin service in PR-2.
    async fn delete(&self, dir_id: &str) -> Result<()>;
}
