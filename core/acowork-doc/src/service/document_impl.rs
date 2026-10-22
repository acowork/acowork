//! Concrete `DocumentService` implementation backed by `LibraryStore`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::fs;

use crate::error::{DocError, Result};
use crate::path::{validate_doc_id, validate_doc_name, validate_dir_id};
use crate::service::document::{
    CreateDocumentInput, DocumentService, UpdateDocumentInput,
};
use crate::store::library::LibraryStore;
use crate::store::trash::TrashStore;
use crate::types::{generate_doc_id, generate_trash_id, Ace, DocMeta, DocRead, LibraryIndex, TrashEntry};

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Production `DocumentService` backed by a `LibraryStore`.
pub struct LibraryDocumentService {
    store: LibraryStore,
    trash: TrashStore,
    clock: Clock,
}

impl LibraryDocumentService {
    pub fn new(store: LibraryStore) -> Self {
        let clock: Clock = Arc::new(Utc::now);
        let trash = TrashStore::new(store.root().to_path_buf());
        Self { store, trash, clock }
    }

    /// Inject a custom clock (used by tests for deterministic timestamps).
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }

    /// Write a `.md` file with the given content (UTF-8).
    async fn write_content(&self, path: &Path, content: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(path, content.as_bytes()).await?;
        Ok(())
    }

    /// Resolve a `doc_id` → `(parent_dir_id, absolute_path, DocMeta)`.
    async fn locate(&self, doc_id: &str) -> Result<(String, PathBuf, DocMeta, LibraryIndex)> {
        validate_doc_id(doc_id)?;
        let (parent_dir_id, idx) = self.store.locate_doc(doc_id).await?;
        let entry = idx
            .files
            .iter()
            .find(|f| f.doc_id == doc_id && !f.deleted)
            .cloned()
            .ok_or_else(|| DocError::DocNotFound(doc_id.to_string()))?;
        let dir_path = self.store.dir_path(&parent_dir_id)?;
        let file_path = dir_path.join(format!("{}.md", entry.name));
        Ok((parent_dir_id, file_path, entry, idx))
    }
}

#[async_trait]
impl DocumentService for LibraryDocumentService {
    async fn read(&self, doc_id: &str) -> Result<DocRead> {
        let (parent_dir_id, file_path, entry, _idx) = self.locate(doc_id).await?;
        let content = fs::read_to_string(&file_path)
            .await
            .map_err(DocError::Io)?;
        // Relative path: for root it's just the filename; for subdirs it's `dir/PRD.md`.
        let path = if parent_dir_id == crate::types::ROOT_DIR_ID {
            format!("{}.md", entry.name)
        } else {
            format!("{}/{}.md", parent_dir_id, entry.name)
        };
        Ok(DocRead {
            meta: entry,
            content,
            path,
        })
    }

    async fn create(&self, input: CreateDocumentInput) -> Result<DocMeta> {
        validate_dir_id(&input.parent_dir_id)?;
        validate_doc_name(&input.title)?;
        // Load parent directory's library.json (must exist — subdirs are
        // created via DirectoryService::create first).
        let mut idx = self.store.load(&input.parent_dir_id).await?;
        // Name uniqueness within the directory.
        if idx.files.iter().any(|f| !f.deleted && f.name == input.title) {
            return Err(DocError::NameConflict(input.title));
        }
        let now = self.now();
        let doc_id = generate_doc_id();
        let file_path = self
            .store
            .dir_path(&input.parent_dir_id)?
            .join(format!("{}.md", input.title));
        self.write_content(&file_path, &input.content).await?;
        let mut meta = DocMeta::new(doc_id.clone(), input.title.clone(), input.import, now);
        // Design §9: the creator may always view/edit/manage-ACL its node.
        meta.creator = input.creator.clone();
        idx.files.push(meta.clone());
        self.store.save(&idx).await?;
        crate::mqtt_publisher::notify_tree_changed(vec![input.parent_dir_id]);
        Ok(meta)
    }

    async fn update(&self, doc_id: &str, input: UpdateDocumentInput) -> Result<DocMeta> {
        let (parent_dir_id, file_path, mut entry, mut idx) = self.locate(doc_id).await?;
        let prev_name = entry.name.clone();
        if entry.version != input.base_version {
            return Err(DocError::VersionConflict {
                base_version: input.base_version,
                current_version: entry.version,
            });
        }
        entry.version += 1;
        entry.updated_at = self.now();
        // Optional rename inside the same update: must rename file first
        // so the next write_content goes to the new location.
        if let Some(new_title) = input.title.as_deref() {
            validate_doc_name(new_title)?;
            if new_title != entry.name {
                if idx
                    .files
                    .iter()
                    .any(|f| !f.deleted && f.name == new_title && f.doc_id != doc_id)
                {
                    return Err(DocError::NameConflict(new_title.into()));
                }
                let dir_path = self.store.dir_path(&parent_dir_id)?;
                let new_path = dir_path.join(format!("{}.md", new_title));
                fs::rename(&file_path, &new_path).await?;
                entry.name = new_title.to_string();
            }
        }
        // Write content to (possibly renamed) path.
        let write_path = self.store.dir_path(&parent_dir_id)?.join(format!("{}.md", entry.name));
        self.write_content(&write_path, &input.content).await?;
        // Patch idx in place.
        if let Some(slot) = idx.files.iter_mut().find(|f| f.doc_id == doc_id) {
            *slot = entry.clone();
        }
        self.store.save(&idx).await?;
        // A title rename inside `update` changes what the tree displays —
        // publish so the Desktop refreshes (same as the dedicated `rename`
        // path). Content-only updates leave the tree untouched.
        if entry.name != prev_name {
            crate::mqtt_publisher::notify_tree_changed(vec![parent_dir_id]);
        }
        Ok(entry)
    }

    async fn list(&self, dir_id: &str) -> Result<Vec<DocMeta>> {
        validate_dir_id(dir_id)?;
        let idx = self.store.load(dir_id).await?;
        Ok(idx.files.into_iter().filter(|f| !f.deleted).collect())
    }

    async fn rename(
        &self,
        doc_id: &str,
        new_title: &str,
        base_version: u64,
    ) -> Result<DocMeta> {
        validate_doc_name(new_title)?;
        let (parent_dir_id, file_path, mut entry, mut idx) = self.locate(doc_id).await?;
        if entry.version != base_version {
            return Err(DocError::VersionConflict {
                base_version,
                current_version: entry.version,
            });
        }
        if entry.name == new_title {
            return Ok(entry); // no-op
        }
        // Uniqueness within the directory.
        if idx
            .files
            .iter()
            .any(|f| !f.deleted && f.name == new_title)
        {
            return Err(DocError::NameConflict(new_title.into()));
        }
        let dir_path = self.store.dir_path(&parent_dir_id)?;
        let new_path = dir_path.join(format!("{}.md", new_title));
        fs::rename(&file_path, &new_path).await?;
        entry.name = new_title.to_string();
        entry.updated_at = self.now();
        if let Some(slot) = idx.files.iter_mut().find(|f| f.doc_id == doc_id) {
            *slot = entry.clone();
        }
        self.store.save(&idx).await?;
        crate::mqtt_publisher::notify_tree_changed(vec![parent_dir_id]);
        Ok(entry)
    }

    async fn move_doc(
        &self,
        doc_id: &str,
        target_dir_id: &str,
        overwrite: bool,
    ) -> Result<DocMeta> {
        validate_dir_id(target_dir_id)?;
        let (source_dir_id, file_path, mut entry, mut source_idx) = self.locate(doc_id).await?;
        if source_dir_id == target_dir_id {
            return Ok(entry); // no-op
        }
        let target_idx_path = self.store.index_path(target_dir_id)?;
        if !target_idx_path.exists() {
            return Err(DocError::DirNotFound(target_dir_id.to_string()));
        }
        let mut target_idx = self.store.load(target_dir_id).await?;
        let clash = target_idx
            .files
            .iter()
            .position(|f| !f.deleted && f.name == entry.name);
        match clash {
            Some(_) if !overwrite => return Err(DocError::NameConflict(entry.name.clone())),
            Some(pos) => {
                // Overwrite: remove the colliding entry from target.
                target_idx.files.remove(pos);
            }
            None => {}
        }
        // Atomicity strategy (design §3.3): write target library.json first
        // (with the new entry), then remove from source library.json, then
        // rename the file. If any step fails, roll back.
        let target_dir_path = self.store.dir_path(target_dir_id)?;
        let new_file_path = target_dir_path.join(format!("{}.md", entry.name));
        // Step 1: add to target idx (in-memory + save).
        target_idx.files.push(entry.clone());
        self.store.save(&target_idx).await?;
        // Step 2: rename file.
        if let Err(e) = fs::rename(&file_path, &new_file_path).await {
            // Rollback target idx.
            target_idx.files.retain(|f| f.doc_id != doc_id);
            let _ = self.store.save(&target_idx).await;
            return Err(e.into());
        }
        // Step 3: remove from source idx.
        source_idx.files.retain(|f| f.doc_id != doc_id);
        if let Err(e) = self.store.save(&source_idx).await {
            // Rollback file + target.
            let _ = fs::rename(&new_file_path, &file_path).await;
            target_idx.files.retain(|f| f.doc_id != doc_id);
            let _ = self.store.save(&target_idx).await;
            return Err(e);
        }
        entry.updated_at = self.now();
        crate::mqtt_publisher::notify_tree_changed(vec![source_dir_id, target_dir_id.to_string()]);
        Ok(entry)
    }

    async fn delete(&self, doc_id: &str) -> Result<()> {
        let (parent_dir_id, file_path, mut entry, mut idx) = self.locate(doc_id).await?;
        // Soft delete (design §3.3): record the doc into `.trash/` with a
        // `TrashEntry` sidecar, drop the original file, and mark the
        // `library.json` entry deleted. The recycle-bin service (PR-2)
        // lists / restores / expires these slots.
        let content = fs::read_to_string(&file_path).await?;
        let now = self.now();
        let trash_entry = TrashEntry {
            trash_id: generate_trash_id(),
            doc_id: Some(doc_id.to_string()),
            original_dir_id: parent_dir_id.clone(),
            original_name: entry.name.clone(),
            deleted_at: now,
            file_size_bytes: content.len() as u64,
            // Snapshot the node's ACL ownership so a delete→restore
            // round-trip does not silently strip creator / private ACL
            // (design §9).
            creator: entry.creator.clone(),
            acl: entry.acl.clone(),
        };
        self.trash.record_doc(&trash_entry, &content).await?;
        let _ = fs::remove_file(&file_path).await;
        entry.deleted = true;
        entry.updated_at = now;
        if let Some(slot) = idx.files.iter_mut().find(|f| f.doc_id == doc_id) {
            *slot = entry;
        }
        self.store.save(&idx).await?;
        crate::mqtt_publisher::notify_tree_changed(vec![parent_dir_id]);
        Ok(())
    }

    async fn path_of(&self, doc_id: &str) -> Result<String> {
        let (parent_dir_id, _file_path, entry, _idx) = self.locate(doc_id).await?;
        Ok(if parent_dir_id == crate::types::ROOT_DIR_ID {
            format!("{}.md", entry.name)
        } else {
            format!("{}/{}.md", parent_dir_id, entry.name)
        })
    }

    async fn set_restored_acl(&self, doc_id: &str, acl: Option<Vec<Ace>>) -> Result<()> {
        // Internal restore-path write: reattach the snapshotted explicit
        // ACL onto the freshly re-created document. Deliberately bypasses
        // the manage gate (the caller already held edit on the target dir).
        let (parent_dir_id, _file_path, mut entry, mut idx) = self.locate(doc_id).await?;
        entry.acl = acl;
        if let Some(slot) = idx.files.iter_mut().find(|f| f.doc_id == doc_id) {
            *slot = entry;
        }
        self.store.save(&idx).await?;
        crate::mqtt_publisher::notify_tree_changed(vec![parent_dir_id]);
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::directory::{CreateDirectoryInput, DirectoryService};
    use crate::service::directory_impl::LibraryDirectoryService;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_clock() -> Clock {
        Arc::new(|| Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap())
    }

    async fn setup() -> (TempDir, Arc<LibraryDocumentService>, Arc<LibraryDirectoryService>) {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let docs = Arc::new(
            LibraryDocumentService::new(LibraryStore::new(tmp.path().to_path_buf()))
                .with_clock(fixed_clock()),
        );
        let dirs = Arc::new(LibraryDirectoryService::new(LibraryStore::new(
            tmp.path().to_path_buf(),
        )));
        let _ = store;
        (tmp, docs, dirs)
    }

    #[tokio::test]
    async fn create_then_read_roundtrip() {
        let (_tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "hello".into(),
                content: "# hi".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let read = docs.read(&meta.doc_id).await.unwrap();
        assert_eq!(read.meta.name, "hello");
        assert_eq!(read.content, "# hi");
    }

    #[tokio::test]
    async fn create_rejects_duplicate_name() {
        let (_tmp, docs, _dirs) = setup().await;
        docs.create(CreateDocumentInput {
            parent_dir_id: crate::types::ROOT_DIR_ID.into(),
            title: "dup".into(),
            content: "x".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        let err = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "dup".into(),
                content: "y".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::NameConflict(_)));
    }

    #[tokio::test]
    async fn create_rejects_reserved_name() {
        let (_tmp, docs, _dirs) = setup().await;
        let err = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: ".hidden".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::ReservedName(_)));
    }

    #[tokio::test]
    async fn update_increments_version_and_succeeds_on_match() {
        let (_tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "v".into(),
                content: "v1".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        assert_eq!(meta.version, 1);
        let updated = docs
            .update(
                &meta.doc_id,
                UpdateDocumentInput {
                    base_version: 1,
                    title: None,
                    content: "v2".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.version, 2);
        let read = docs.read(&meta.doc_id).await.unwrap();
        assert_eq!(read.content, "v2");
    }

    #[tokio::test]
    async fn update_with_title_renames_file_and_meta() {
        let (tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "old".into(),
                content: "v1".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let updated = docs
            .update(
                &meta.doc_id,
                UpdateDocumentInput {
                    base_version: 1,
                    title: Some("new".into()),
                    content: "v2".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.name, "new");
        // The file on disk is renamed too (this path feeds the tree's
        // displayed name — a missed notify would desync the sidebar).
        assert!(tmp.path().join("new.md").exists());
        assert!(!tmp.path().join("old.md").exists());
    }

    #[tokio::test]
    async fn update_returns_version_conflict_on_stale_base() {
        let (_tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "v".into(),
                content: "v1".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        docs.update(
            &meta.doc_id,
            UpdateDocumentInput {
                base_version: 1,
                title: None,
                content: "v2".into(),
            },
        )
        .await
        .unwrap();
        let err = docs
            .update(
                &meta.doc_id,
                UpdateDocumentInput {
                    base_version: 1,
                    title: None,
                    content: "v3".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, DocError::VersionConflict { base_version: 1, current_version: 2 }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn rename_changes_filename_and_meta() {
        let (_tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "old".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let renamed = docs.rename(&meta.doc_id, "new", 1).await.unwrap();
        assert_eq!(renamed.name, "new");
        let tmp_path = _tmp.path().to_path_buf();
        assert!(tmp_path.join("new.md").exists());
        assert!(!tmp_path.join("old.md").exists());
    }

    #[tokio::test]
    async fn delete_marks_entry_deleted_and_moves_file_to_trash() {
        let (_tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "bye".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        docs.delete(&meta.doc_id).await.unwrap();
        let root = _tmp.path().to_path_buf();
        assert!(!root.join("bye.md").exists());
        assert!(root.join(".trash").exists());
        assert!(std::fs::read_dir(root.join(".trash"))
            .unwrap()
            .next()
            .is_some());
        let probe = LibraryStore::new(root.clone());
        let (_parent, idx) = probe.locate_doc(&meta.doc_id).await.unwrap();
        assert!(idx.files.iter().any(|f| f.doc_id == meta.doc_id && f.deleted));
    }

    #[tokio::test]
    async fn move_doc_across_dirs_updates_both_indexes() {
        let (_tmp, docs, dirs) = setup().await;
        let sub_meta = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                name: "项目A".into(),
                creator: None,
            })
            .await
            .unwrap();
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "PRD".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let moved = docs
            .move_doc(&meta.doc_id, &sub_meta.dir_id, false)
            .await
            .unwrap();
        assert_eq!(moved.name, "PRD");
        let root = _tmp.path().to_path_buf();
        assert!(root.join(&sub_meta.dir_id).join("PRD.md").exists());
        assert!(!root.join("PRD.md").exists());
    }

    #[tokio::test]
    async fn move_doc_same_dir_is_noop() {
        let (_tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "x".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let moved = docs
            .move_doc(&meta.doc_id, crate::types::ROOT_DIR_ID, false)
            .await
            .unwrap();
        assert_eq!(moved.doc_id, meta.doc_id);
    }
}
