//! Concrete `DocumentService` implementation backed by `LibraryStore`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::fs;

use crate::error::{DocError, Result};
use crate::path::{validate_doc_id, validate_doc_name, validate_dir_id};
use crate::service::document::{
    AppendDocumentInput, CreateDocumentInput, DocStat, DocumentService, UpdateDocumentInput,
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

    /// Undo a partially applied replace: restore the destination index entry
    /// and drop the recycle-bin slot, so a failed move leaves no trace.
    async fn rollback_replace(
        &self,
        target_idx: &LibraryIndex,
        target_before: Vec<DocMeta>,
        displaced: &Option<(String, String)>,
    ) {
        let mut restored = target_idx.clone();
        restored.files = target_before;
        let _ = self.store.save(&restored).await;
        if let Some((trash_id, _)) = displaced {
            let _ = self.trash.purge(trash_id).await;
        }
    }
}

#[async_trait]
impl DocumentService for LibraryDocumentService {
    async fn read(&self, doc_id: &str) -> Result<DocRead> {
        let (parent_dir_id, file_path, entry, _idx) = self.locate(doc_id).await?;
        let content = fs::read_to_string(&file_path)
            .await
            .map_err(DocError::Io)?;
        // Path keyed by `dir_id` (see `DocStat::path`): the on-disk relative
        // path, which the MCP layer translates into the human path.
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

    async fn stat(&self, doc_id: &str) -> Result<DocStat> {
        // `locate` resolves the on-disk path but does not read the body —
        // the size comes from the file's metadata, so stat stays O(1) in
        // the document size.
        let (parent_dir_id, file_path, entry, _idx) = self.locate(doc_id).await?;
        // A metadata failure means the index and the filesystem disagree
        // (truncated library / external `rm`). Reporting `0` would tell the
        // caller "empty document" and cancel its version check against a
        // phantom baseline, so surface the error instead — `DocError::Io`
        // already maps to 500 with the path in the message.
        let size_bytes = fs::metadata(&file_path)
            .await
            .map_err(|e| {
                DocError::CorruptIndex(format!(
                    "doc {doc_id} is indexed at {} but unreadable: {e}",
                    file_path.display()
                ))
            })?
            .len();
        let path = if parent_dir_id == crate::types::ROOT_DIR_ID {
            format!("{}.md", entry.name)
        } else {
            format!("{}/{}.md", parent_dir_id, entry.name)
        };
        Ok(DocStat {
            meta: entry,
            path,
            size_bytes,
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

    async fn append(&self, doc_id: &str, input: AppendDocumentInput) -> Result<DocMeta> {
        let (_parent_dir_id, file_path, mut entry, mut idx) = self.locate(doc_id).await?;
        if let Some(base_version) = input.base_version
            && entry.version != base_version
        {
            return Err(DocError::VersionConflict {
                base_version,
                current_version: entry.version,
            });
        }
        // Read-modify-write. The version bump inside this single trait call
        // is what keeps two concurrent appenders from both succeeding on the
        // same base: the second one re-reads under its own `locate`, so an
        // explicit `base_version` is the caller's tool for a hard guarantee.
        let mut content = fs::read_to_string(&file_path).await?;
        content.push_str(&input.content);
        entry.version += 1;
        entry.updated_at = self.now();
        self.write_content(&file_path, &content).await?;
        if let Some(slot) = idx.files.iter_mut().find(|f| f.doc_id == doc_id) {
            *slot = entry.clone();
        }
        self.store.save(&idx).await?;
        // Appending never changes the tree shape (name unchanged), so no
        // `notify_tree_changed` — same rule as a content-only `update`.
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
        new_name: Option<&str>,
        overwrite: bool,
    ) -> Result<DocMeta> {
        validate_dir_id(target_dir_id)?;
        let (source_dir_id, file_path, mut entry, mut source_idx) = self.locate(doc_id).await?;
        let name = match new_name {
            Some(n) => {
                validate_doc_name(n)?;
                n.to_string()
            }
            None => entry.name.clone(),
        };
        let same_dir = source_dir_id == target_dir_id;
        if same_dir && name == entry.name {
            return Ok(entry); // no-op
        }
        let mut target_idx = if same_dir {
            source_idx.clone()
        } else {
            if !self.store.index_path(target_dir_id)?.exists() {
                return Err(DocError::DirNotFound(target_dir_id.to_string()));
            }
            self.store.load(target_dir_id).await?
        };
        let target_dir_path = self.store.dir_path(target_dir_id)?;
        let new_file_path = target_dir_path.join(format!("{name}.md"));

        // ── Overwrite: take custody of the destination *before* touching it ──
        //
        // The rename below replaces the destination file in place, so the
        // displaced body exists nowhere afterwards. Two things must therefore
        // happen first: it goes to the recycle bin (a replacement stays
        // recoverable), and its bytes are held in memory so the rollback path
        // can put the file back. Doing the destructive part last — as the MCP
        // layer used to, by deleting the destination up front — left the
        // destination deleted and the source unmoved whenever the move itself
        // failed.
        let clash = target_idx
            .files
            .iter()
            .position(|f| !f.deleted && f.name == name && f.doc_id != doc_id);
        let displaced = match clash {
            Some(_) if !overwrite => return Err(DocError::NameConflict(name)),
            Some(pos) => Some(target_idx.files.remove(pos)),
            None => None,
        };
        // The displaced entry matched on `name`, so its file is the file the
        // rename below is about to overwrite.
        let displaced_body = match &displaced {
            Some(meta) => {
                let body = fs::read_to_string(&new_file_path).await.map_err(|e| {
                    DocError::CorruptIndex(format!(
                        "cannot replace {}: indexed but unreadable: {e}",
                        new_file_path.display()
                    ))
                })?;
                let slot = TrashEntry {
                    trash_id: generate_trash_id(),
                    doc_id: Some(meta.doc_id.clone()),
                    original_dir_id: target_dir_id.to_string(),
                    original_name: meta.name.clone(),
                    deleted_at: self.now(),
                    file_size_bytes: body.len() as u64,
                    creator: meta.creator.clone(),
                    acl: meta.acl.clone(),
                };
                self.trash.record_doc(&slot, &body).await?;
                Some((slot.trash_id, body))
            }
            None => None,
        };
        // Snapshot taken after the displaced entry was pulled out, so
        // restoring it puts the destination index back byte-for-byte.
        let target_before = target_idx.files.clone();

        // Step 1: destination index — the document's entry moves (same-dir
        // rename must not leave the stale entry behind).
        target_idx.files.retain(|f| f.doc_id != doc_id);
        entry.name = name;
        entry.updated_at = self.now();
        target_idx.files.push(entry.clone());
        if let Err(e) = self.store.save(&target_idx).await {
            self.rollback_replace(&target_idx, target_before, &displaced_body).await;
            return Err(e);
        }

        // Step 2: the file.
        if let Err(e) = fs::rename(&file_path, &new_file_path).await {
            // The rename is atomic: nothing moved, the displaced file is
            // untouched. Undo the index write and the trash slot.
            self.rollback_replace(&target_idx, target_before, &displaced_body).await;
            return Err(e.into());
        }

        // Step 3: source index (cross-directory only).
        if !same_dir {
            source_idx.files.retain(|f| f.doc_id != doc_id);
            if let Err(e) = self.store.save(&source_idx).await {
                // The source file goes back where it started, but the rename
                // has already replaced the destination body — restore it from
                // the copy taken above.
                let _ = fs::rename(&new_file_path, &file_path).await;
                if let Some((_, body)) = &displaced_body {
                    let _ = self.write_content(&new_file_path, body).await;
                }
                self.rollback_replace(&target_idx, target_before, &displaced_body).await;
                return Err(e);
            }
        }
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
            .move_doc(&meta.doc_id, &sub_meta.dir_id, None, false)
            .await
            .unwrap();
        assert_eq!(moved.name, "PRD");
        let root = _tmp.path().to_path_buf();
        assert!(root.join(&sub_meta.dir_id).join("PRD.md").exists());
        assert!(!root.join("PRD.md").exists());
    }

    #[tokio::test]
    async fn move_doc_overwrite_parks_the_replaced_body_in_the_trash() {
        let (tmp, docs, dirs) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                name: "归档".into(),
                creator: None,
            })
            .await
            .unwrap();
        let src = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "PRD".into(),
                content: "new body".into(),
                import: None,
                creator: Some("agent-a".into()),
            })
            .await
            .unwrap();
        let dst = docs
            .create(CreateDocumentInput {
                parent_dir_id: sub.dir_id.clone(),
                title: "PRD".into(),
                content: "old body".into(),
                import: None,
                creator: Some("agent-b".into()),
            })
            .await
            .unwrap();

        let moved = docs
            .move_doc(&src.doc_id, &sub.dir_id, None, true)
            .await
            .unwrap();
        assert_eq!(moved.doc_id, src.doc_id);
        // The replaced document is no longer indexed...
        let probe = LibraryStore::new(tmp.path().to_path_buf());
        let (_p, idx) = probe.locate_doc(&src.doc_id).await.unwrap();
        let live: Vec<_> = idx.files.iter().filter(|f| !f.deleted).collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].doc_id, src.doc_id);
        // ...but its body is recoverable, not unlinked.
        let trash = TrashStore::new(tmp.path().to_path_buf());
        let entries = trash.list_entries().await.unwrap();
        assert_eq!(entries.len(), 1, "replaced body must land in the trash");
        assert_eq!(entries[0].doc_id.as_deref(), Some(dst.doc_id.as_str()));
        assert_eq!(entries[0].original_name, "PRD");
        let (_, body_path) = trash.find(&entries[0].trash_id).await.unwrap();
        assert_eq!(trash.read_content(&body_path).await.unwrap(), "old body");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join(&sub.dir_id).join("PRD.md")).unwrap(),
            "new body"
        );
    }

    #[tokio::test]
    async fn move_doc_without_overwrite_is_a_name_conflict_and_moves_nothing() {
        let (tmp, docs, dirs) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                name: "归档".into(),
                creator: None,
            })
            .await
            .unwrap();
        let src = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "PRD".into(),
                content: "a".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let dst = docs
            .create(CreateDocumentInput {
                parent_dir_id: sub.dir_id.clone(),
                title: "PRD".into(),
                content: "b".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let err = docs
            .move_doc(&src.doc_id, &sub.dir_id, None, false)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::NameConflict(n) if n == "PRD"));
        // Nothing was displaced by the refusal.
        assert_eq!(docs.read(&dst.doc_id).await.unwrap().content, "b");
        assert_eq!(docs.read(&src.doc_id).await.unwrap().content, "a");
        let trash = TrashStore::new(tmp.path().to_path_buf());
        assert!(trash.list_entries().await.unwrap().is_empty());
    }

    /// The destination must survive a replace that never lands. The MCP layer
    /// used to delete it up front, which produced exactly the state asserted
    /// against here: destination gone, source unmoved.
    #[tokio::test]
    async fn failed_replace_leaves_the_destination_intact() {
        let (tmp, docs, dirs) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                name: "归档".into(),
                creator: None,
            })
            .await
            .unwrap();
        let src = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "PRD".into(),
                content: "new body".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let dst = docs
            .create(CreateDocumentInput {
                parent_dir_id: sub.dir_id.clone(),
                title: "PRD".into(),
                content: "old body".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        // Index/filesystem drift: the destination is indexed but its body is
        // gone, so the replace cannot take custody of it.
        std::fs::remove_file(tmp.path().join(&sub.dir_id).join("PRD.md")).unwrap();

        let err = docs
            .move_doc(&src.doc_id, &sub.dir_id, None, true)
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::CorruptIndex(_)), "got {err:?}");
        // Source untouched (still readable, still in the root index)...
        assert_eq!(docs.read(&src.doc_id).await.unwrap().content, "new body");
        // ...and the destination is still the destination.
        let (_p, idx) = LibraryStore::new(tmp.path().to_path_buf())
            .locate_doc(&dst.doc_id)
            .await
            .unwrap();
        assert!(idx.files.iter().any(|f| f.doc_id == dst.doc_id && !f.deleted));
        assert!(TrashStore::new(tmp.path().to_path_buf())
            .list_entries()
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn same_dir_rename_with_overwrite_replaces_in_place() {
        let (tmp, docs, _dirs) = setup().await;
        let src = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "draft".into(),
                content: "new".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        let dst = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "final".into(),
                content: "old".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();

        let renamed = docs
            .move_doc(&src.doc_id, crate::types::ROOT_DIR_ID, Some("final"), true)
            .await
            .unwrap();
        assert_eq!(renamed.name, "final");
        // One live entry named `final`, and it is the source document.
        let (_p, idx) = LibraryStore::new(tmp.path().to_path_buf())
            .locate_doc(&src.doc_id)
            .await
            .unwrap();
        let live: Vec<_> = idx.files.iter().filter(|f| !f.deleted).collect();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].doc_id, src.doc_id);
        assert_eq!(live[0].name, "final");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("final.md")).unwrap(),
            "new"
        );
        assert!(!tmp.path().join("draft.md").exists());
        // The replaced document is recoverable.
        let entries = TrashStore::new(tmp.path().to_path_buf())
            .list_entries()
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].doc_id.as_deref(), Some(dst.doc_id.as_str()));
    }

    #[tokio::test]
    async fn stat_fails_loudly_when_the_indexed_file_is_missing() {
        let (tmp, docs, _dirs) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: crate::types::ROOT_DIR_ID.into(),
                title: "gone".into(),
                content: "body".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        std::fs::remove_file(tmp.path().join("gone.md")).unwrap();
        // `0` would read as "empty document" and let a caller version-check
        // against a phantom baseline; the disagreement must surface.
        let err = docs.stat(&meta.doc_id).await.unwrap_err();
        assert!(matches!(err, DocError::CorruptIndex(_)), "got {err:?}");
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
            .move_doc(&meta.doc_id, crate::types::ROOT_DIR_ID, None, false)
            .await
            .unwrap();
        assert_eq!(moved.doc_id, meta.doc_id);
    }
}
