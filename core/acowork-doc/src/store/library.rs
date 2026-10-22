//! `LibraryStore` — per-directory read/write of `library.json`.
//!
//! Mapping (design §3.2):
//!
//! | dir_id        | on-disk path                          |
//! |---------------|---------------------------------------|
//! | `"root"`      | `{root}/library.json`                 |
//! | `"dir-{hex}"` | `{root}/dir-{hex}/library.json`       |
//!
//! The `dir_id` *is* the directory name (no nested subdirs); the JSON
//! index only caches the immediate level's `files[]` / `dirs[]`. Move
//! across directories is therefore a rename of both the `.md` file and
//! the corresponding `library.json` entries (service-layer concern).
//!
//! I/O always goes through [`atomic_write_json`] / [`read_json`] so that
//! a torn-write never leaks to readers.

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{DocError, Result};
use crate::path::validate_dir_id;
use crate::store::atomic::{atomic_write_json, read_json};
use crate::types::{DirId, LibraryIndex, ROOT_DIR_ID};

/// Owns the library root and brokers all `library.json` reads / writes.
#[derive(Debug, Clone)]
pub struct LibraryStore {
    root: PathBuf,
}

impl LibraryStore {
    /// Create a store rooted at `root`. Does **not** create the root
    /// directory — call [`Self::ensure_root`] (or the service-layer
    /// bootstrap) to materialise on-disk state.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Library root path (caller-provided; may not exist yet).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create the library root directory and a default empty `library.json`
    /// if they don't exist. Idempotent — safe to call on every startup.
    pub async fn ensure_root(&self) -> Result<()> {
        fs::create_dir_all(&self.root).await?;
        let root_idx_path = self.dir_path(ROOT_DIR_ID)?.join("library.json");
        if !root_idx_path.exists() {
            atomic_write_json(&root_idx_path, &LibraryIndex::root()).await?;
        }
        Ok(())
    }

    /// Resolve a `dir_id` to its containing directory on disk.
    pub fn dir_path(&self, dir_id: &str) -> Result<PathBuf> {
        if dir_id == ROOT_DIR_ID {
            return Ok(self.root.clone());
        }
        validate_dir_id(dir_id)?;
        Ok(self.root.join(dir_id))
    }

    /// Resolve a `dir_id` to the `library.json` file inside its directory.
    pub fn index_path(&self, dir_id: &str) -> Result<PathBuf> {
        Ok(self.dir_path(dir_id)?.join("library.json"))
    }

    /// Load the `library.json` of `dir_id`. Returns an error if the file
    /// is missing or unparseable.
    pub async fn load(&self, dir_id: &str) -> Result<LibraryIndex> {
        let p = self.index_path(dir_id)?;
        read_json(&p).await
    }

    /// Atomically save a `LibraryIndex` for `dir_id`.
    pub async fn save(&self, idx: &LibraryIndex) -> Result<()> {
        let p = self.index_path(&idx.dir_id)?;
        atomic_write_json(&p, idx).await
    }

    /// Walk the library root and return every `library.json` it finds,
    /// paired with its relative path (for startup reconciliation).
    pub async fn list_all_indexes(&self) -> Result<Vec<(PathBuf, LibraryIndex)>> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let idx_path = dir.join("library.json");
            if idx_path.exists() {
                let idx: LibraryIndex = read_json(&idx_path).await?;
                out.push((idx_path, idx));
            }
            // One level deep only: `dir_id` IS the directory name.
            let mut entries = fs::read_dir(&dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let file_type = entry.file_type().await?;
                if !file_type.is_dir() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                // Skip system dirs (.trash, .requests) and any non `dir-*`.
                if name.starts_with('.') {
                    continue;
                }
                if !name.starts_with("dir-") {
                    continue;
                }
                stack.push(entry.path());
            }
        }
        Ok(out)
    }

    /// Look up the `dir_id` containing the given `doc_id`, by scanning
    /// every `library.json`. O(N) — service layer should cache.
    pub async fn locate_doc(&self, doc_id: &str) -> Result<(DirId, LibraryIndex)> {
        for (_path, idx) in self.list_all_indexes().await? {
            if idx.files.iter().any(|f| f.doc_id == doc_id) {
                let parent = idx.dir_id.clone();
                return Ok((parent, idx));
            }
        }
        Err(DocError::DocNotFound(doc_id.into()))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_root() -> (TempDir, LibraryStore) {
        let dir = TempDir::new().unwrap();
        let store = LibraryStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    #[tokio::test]
    async fn ensure_root_creates_library_json() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        let idx_path = store.index_path(ROOT_DIR_ID).unwrap();
        assert!(idx_path.exists());
        let idx: LibraryIndex = read_json(&idx_path).await.unwrap();
        assert_eq!(idx.dir_id, ROOT_DIR_ID);
        assert!(idx.files.is_empty());
        assert!(idx.dirs.is_empty());
    }

    #[tokio::test]
    async fn ensure_root_is_idempotent() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        store.ensure_root().await.unwrap();
        // No double-create error.
        let idx = store.load(ROOT_DIR_ID).await.unwrap();
        assert_eq!(idx.dir_id, ROOT_DIR_ID);
    }

    #[tokio::test]
    async fn dir_path_resolves_root_and_subdirs() {
        let (_tmp, store) = tmp_root();
        assert_eq!(store.dir_path(ROOT_DIR_ID).unwrap(), store.root());
        let p = store.dir_path("dir-abcdef012345").unwrap();
        assert_eq!(p, store.root().join("dir-abcdef012345"));
    }

    #[tokio::test]
    async fn dir_path_rejects_garbage() {
        let (_tmp, store) = tmp_root();
        assert!(store.dir_path("../etc").is_err());
        assert!(store.dir_path("doc-abcdef012345").is_err()); // wrong prefix
        assert!(store.dir_path("").is_err());
    }

    #[tokio::test]
    async fn load_save_roundtrip() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        let mut idx = LibraryIndex::root();
        idx.files.push(crate::types::DocMeta::new(
            "doc-abcdef012345".into(),
            "测试".into(),
            None,
            chrono::Utc::now(),
        ));
        store.save(&idx).await.unwrap();
        let back = store.load(ROOT_DIR_ID).await.unwrap();
        assert_eq!(idx, back);
    }

    #[tokio::test]
    async fn list_all_indexes_finds_root_and_subs() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        // Create a sub-directory with its own library.json.
        let sub = store.root().join("dir-abcdef012345");
        fs::create_dir(&sub).await.unwrap();
        let mut sub_idx = LibraryIndex {
            dir_id: "dir-abcdef012345".into(),
            parent: Some(ROOT_DIR_ID.into()),
            files: vec![],
            dirs: vec![],
            schema_version: 1,
            creator: None,
            acl: None,
        };
        sub_idx.files.push(crate::types::DocMeta::new(
            "doc-111111111111".into(),
            "child".into(),
            None,
            chrono::Utc::now(),
        ));
        store.save(&sub_idx).await.unwrap();

        let all = store.list_all_indexes().await.unwrap();
        assert_eq!(all.len(), 2);
        let ids: Vec<_> = all.iter().map(|(_, i)| i.dir_id.as_str()).collect();
        assert!(ids.contains(&ROOT_DIR_ID));
        assert!(ids.contains(&"dir-abcdef012345"));
    }

    #[tokio::test]
    async fn list_all_indexes_skips_dot_dirs() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        // .trash and .requests must be skipped even if they contain a
        // stray library.json (shouldn't, but defensive).
        for d in [".trash", ".requests"] {
            let p = store.root().join(d);
            fs::create_dir(&p).await.unwrap();
            fs::write(
                p.join("library.json"),
                br#"{"dir_id":"should_not_load","parent":null,"files":[],"dirs":[],"schema_version":1}"#,
            )
            .await
            .unwrap();
        }
        let all = store.list_all_indexes().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.dir_id, ROOT_DIR_ID);
    }

    #[tokio::test]
    async fn locate_doc_finds_across_subdirs() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        let sub = store.root().join("dir-abcdef012345");
        fs::create_dir(&sub).await.unwrap();
        let sub_idx = LibraryIndex {
            dir_id: "dir-abcdef012345".into(),
            parent: Some(ROOT_DIR_ID.into()),
            files: vec![crate::types::DocMeta::new(
                "doc-111111111111".into(),
                "child".into(),
                None,
                chrono::Utc::now(),
            )],
            dirs: vec![],
            schema_version: 1,
            creator: None,
            acl: None,
        };
        store.save(&sub_idx).await.unwrap();

        let (parent, idx) = store.locate_doc("doc-111111111111").await.unwrap();
        assert_eq!(parent, "dir-abcdef012345");
        assert_eq!(idx.dir_id, "dir-abcdef012345");
    }

    #[tokio::test]
    async fn locate_doc_missing_returns_not_found() {
        let (_tmp, store) = tmp_root();
        store.ensure_root().await.unwrap();
        let err = store.locate_doc("doc-999999999999").await.unwrap_err();
        assert!(matches!(err, DocError::DocNotFound(_)), "got: {err:?}");
    }
}
