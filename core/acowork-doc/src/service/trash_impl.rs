//! Concrete `TrashService` implementation.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};

use crate::error::Result;
use crate::service::directory::DirectoryService;
use crate::service::document::{CreateDocumentInput, DocumentService};
use crate::service::trash::TrashService;
use crate::store::trash::TrashStore;
use crate::types::{DocMeta, TrashEntry};

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub struct LibraryTrashService {
    store: TrashStore,
    docs: Arc<dyn DocumentService>,
    dirs: Arc<dyn DirectoryService>,
    clock: Clock,
    retention: Duration,
}

impl LibraryTrashService {
    /// Build the service. `docs` is the document service the restore
    /// path re-creates into, `dirs` the directory service that tells whether
    /// that parent is still part of the tree; `store` shares the same library
    /// root; `retention_days` comes from `DocConfig::trash_retention_days`.
    pub fn new(
        docs: Arc<dyn DocumentService>,
        dirs: Arc<dyn DirectoryService>,
        store: TrashStore,
        retention_days: u32,
    ) -> Self {
        let clock: Clock = Arc::new(Utc::now);
        let retention = Duration::days(i64::from(retention_days));
        Self { store, docs, dirs, clock, retention }
    }

    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    fn now(&self) -> DateTime<Utc> {
        (self.clock)()
    }
}

#[async_trait]
impl TrashService for LibraryTrashService {
    async fn list(&self) -> Result<Vec<TrashEntry>> {
        // Lazy purge: drop expired slots before reporting the view.
        let cutoff = self.now() - self.retention;
        let _ = self.store.purge_expired(cutoff).await?;
        self.store.list_entries().await
    }

    async fn restore(&self, trash_id: &str) -> Result<DocMeta> {
        // First drop anything already past retention (the restore target
        // must not resurrect an entry the window considers gone).
        let cutoff = self.now() - self.retention;
        let _ = self.store.purge_expired(cutoff).await?;
        let (entry, body) = self.store.find(trash_id).await?;
        // The slot's parent must still be part of the tree. A pruned directory
        // leaves its `library.json` on disk, so `create` would otherwise
        // succeed and hand back a document no listing can ever show. Fail
        // before touching the slot (the caller keeps it).
        self.dirs.read(&entry.original_dir_id).await?;
        let content = self.store.read_content(&body).await?;
        let created = self
            .docs
            .create(CreateDocumentInput {
                parent_dir_id: entry.original_dir_id.clone(),
                title: entry.original_name.clone(),
                content,
                import: None,
                // Restore the original owner (design §9) so the restored
                // doc keeps its creator-based manage gate instead of
                // degrading to admin-only.
                creator: entry.creator.clone(),
            })
            .await?;
        // Reattach the snapshotted explicit ACL (if any) so a private node
        // stays private across the delete→restore round-trip; `None` was
        // never set so the doc simply inherits (no call needed).
        if entry.acl.is_some() {
            self.docs
                .set_restored_acl(&created.doc_id, entry.acl.clone())
                .await?;
        }
        // Drop the slot only after the document is safely re-created.
        self.store.purge(trash_id).await?;
        Ok(created)
    }

    async fn purge(&self, trash_id: &str) -> Result<()> {
        self.store.purge(trash_id).await
    }

    async fn purge_expired(&self) -> Result<usize> {
        let cutoff = self.now() - self.retention;
        self.store.purge_expired(cutoff).await
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DocError;
    use crate::service::directory_impl::LibraryDirectoryService;
    use crate::service::document::{CreateDocumentInput, DocumentService};
    use crate::service::document_impl::LibraryDocumentService;
    use crate::service::trash::TrashService;
    use crate::store::library::LibraryStore;
    use crate::types::ROOT_DIR_ID;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_clock() -> Clock {
        Arc::new(|| Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap())
    }

    async fn setup() -> (
        TempDir,
        Arc<LibraryDocumentService>,
        Arc<LibraryTrashService>,
    ) {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let docs = Arc::new(LibraryDocumentService::new(
            LibraryStore::new(tmp.path().to_path_buf()),
        ));
        let dirs = Arc::new(LibraryDirectoryService::new(
            LibraryStore::new(tmp.path().to_path_buf()),
        ));
        let trash = Arc::new(
            LibraryTrashService::new(
                docs.clone(),
                dirs,
                TrashStore::new(tmp.path().to_path_buf()),
                30,
            )
            .with_clock(fixed_clock()),
        );
        (tmp, docs, trash)
    }

    #[tokio::test]
    async fn delete_then_list_shows_slot() {
        let (_tmp, docs, trash) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "delete-me".into(),
                content: "body".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        docs.delete(&meta.doc_id).await.unwrap();
        let list = trash.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].original_name, "delete-me");
        assert_eq!(list[0].doc_id.as_deref(), Some(meta.doc_id.as_str()));
    }

    #[tokio::test]
    async fn restore_recreates_doc_and_clears_slot() {
        let (_tmp, docs, trash) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "rescue".into(),
                content: "珍贵内容".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        docs.delete(&meta.doc_id).await.unwrap();
        let list = trash.list().await.unwrap();
        let restored = trash.restore(&list[0].trash_id).await.unwrap();
        assert_eq!(restored.name, "rescue");
        // Content preserved.
        let read = docs.read(&restored.doc_id).await.unwrap();
        assert_eq!(read.content, "珍贵内容");
        // Slot gone.
        assert!(trash.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn purge_permanently_removes() {
        let (_tmp, docs, trash) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "gone-forever".into(),
                content: "x".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        docs.delete(&meta.doc_id).await.unwrap();
        let list = trash.list().await.unwrap();
        trash.purge(&list[0].trash_id).await.unwrap();
        assert!(trash.list().await.unwrap().is_empty());
        // Restoring a purged slot fails.
        let err = trash.restore(&list[0].trash_id).await.unwrap_err();
        assert!(matches!(err, DocError::TrashMissing(_)));
    }

    #[tokio::test]
    async fn restore_name_conflict_keeps_slot() {
        let (_tmp, docs, trash) = setup().await;
        let meta = docs
            .create(CreateDocumentInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                title: "same-name".into(),
                content: "old".into(),
                import: None,
                creator: None,
            })
            .await
            .unwrap();
        docs.delete(&meta.doc_id).await.unwrap();
        // Someone created a new doc with the same title in the root.
        docs.create(CreateDocumentInput {
            parent_dir_id: ROOT_DIR_ID.into(),
            title: "same-name".into(),
            content: "new".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        let list = trash.list().await.unwrap();
        let err = trash.restore(&list[0].trash_id).await.unwrap_err();
        assert!(matches!(err, DocError::NameConflict(_)), "{err:?}");
        // Slot still there for manual purge.
        assert_eq!(trash.list().await.unwrap().len(), 1);
    }

    /// The documented contract of `restore` is "if the original directory no
    /// longer exists the call fails and the slot is left untouched". A pruned
    /// directory still has its `library.json` on disk, so `create` used to
    /// succeed and hand back a document that no listing could ever show.
    #[tokio::test]
    async fn restore_into_a_pruned_directory_fails_and_keeps_the_slot() {
        use crate::service::directory::CreateDirectoryInput;

        let tmp = TempDir::new().unwrap();
        let root = tmp.path().to_path_buf();
        let seed = LibraryStore::new(root.clone());
        seed.ensure_root().await.unwrap();
        let docs = Arc::new(LibraryDocumentService::new(LibraryStore::new(root.clone())));
        let dirs = Arc::new(LibraryDirectoryService::new(LibraryStore::new(root.clone())));
        let trash = LibraryTrashService::new(
            docs.clone(),
            dirs.clone(),
            TrashStore::new(root.clone()),
            30,
        )
        .with_clock(fixed_clock());

        let dir = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.to_string(),
                name: "gone".into(),
                creator: Some("agent-a".into()),
            })
            .await
            .unwrap();
        let doc = docs
            .create(CreateDocumentInput {
                parent_dir_id: dir.dir_id.clone(),
                title: "note".into(),
                content: "body".into(),
                import: None,
                creator: Some("agent-a".into()),
            })
            .await
            .unwrap();
        docs.delete(&doc.doc_id).await.unwrap();
        dirs.delete(&dir.dir_id).await.unwrap();

        let slots = trash.list().await.unwrap();
        assert_eq!(slots.len(), 1);
        let err = trash.restore(&slots[0].trash_id).await.unwrap_err();
        assert!(matches!(err, DocError::DirNotFound(_)), "{err:?}");
        assert_eq!(
            trash.list().await.unwrap().len(),
            1,
            "a refused restore must leave the slot in place"
        );
    }

    #[tokio::test]
    async fn expired_slots_are_purged_on_list() {
        // A 1-day retention means a slot deleted 2 days ago disappears
        // on the next list(). Deterministic: both slots are recorded
        // directly against the trash store (no real-clock dependency).
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let docs = Arc::new(LibraryDocumentService::new(LibraryStore::new(
            tmp.path().to_path_buf(),
        )));
        let trash = Arc::new(
            LibraryTrashService::new(
                docs.clone(),
                Arc::new(LibraryDirectoryService::new(LibraryStore::new(
                    tmp.path().to_path_buf(),
                ))),
                TrashStore::new(tmp.path().to_path_buf()),
                1, // 1-day retention
            )
            .with_clock(fixed_clock()), // now() = 2026-09-08T12:00:00Z
        );
        let trash_store = TrashStore::new(tmp.path().to_path_buf());
        let old = Utc.with_ymd_and_hms(2026, 9, 6, 12, 0, 0).unwrap(); // stale
        let fresh = Utc.with_ymd_and_hms(2026, 9, 8, 11, 0, 0).unwrap(); // kept
        trash_store
            .record_doc(
                &TrashEntry {
                    trash_id: "tr-aaa111111111".into(),
                    doc_id: None,
                    original_dir_id: ROOT_DIR_ID.to_string(),
                    original_name: "stale".into(),
                    deleted_at: old,
                    file_size_bytes: 1,
                    creator: None,
                    acl: None,
                },
                "x",
            )
            .await
            .unwrap();
        trash_store
            .record_doc(
                &TrashEntry {
                    trash_id: "tr-bbb222222222".into(),
                    doc_id: None,
                    original_dir_id: ROOT_DIR_ID.to_string(),
                    original_name: "kept".into(),
                    deleted_at: fresh,
                    file_size_bytes: 1,
                    creator: None,
                    acl: None,
                },
                "y",
            )
            .await
            .unwrap();
        // Both slots exist on disk before list().
        assert_eq!(trash_store.list_entries().await.unwrap().len(), 2);
        // list() purges the stale one and reports only the fresh one.
        let after = trash.list().await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].trash_id, "tr-bbb222222222");
    }
}
