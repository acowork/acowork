//! Concrete `DirectoryService` implementation backed by `LibraryStore`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tokio::fs;

use crate::error::{DocError, Result};
use crate::path::{validate_doc_name, validate_dir_id};
use crate::service::directory::{CreateDirectoryInput, DirectoryService};
use crate::store::library::LibraryStore;
use crate::store::trash::TrashStore;
use crate::types::{
    generate_dir_id, generate_trash_id, DirMeta, LibraryIndex, TrashEntry, TreeNode,
    ROOT_DIR_ID,
};

type Clock = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

pub struct LibraryDirectoryService {
    store: LibraryStore,
    trash: TrashStore,
    clock: Clock,
}

impl LibraryDirectoryService {
    pub fn new(store: LibraryStore) -> Self {
        let clock: Clock = Arc::new(Utc::now);
        let trash = TrashStore::new(store.root().to_path_buf());
        Self { store, trash, clock }
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
impl DirectoryService for LibraryDirectoryService {
    async fn create(&self, input: CreateDirectoryInput) -> Result<DirMeta> {
        validate_dir_id(&input.parent_dir_id)?;
        validate_doc_name(&input.name)?;
        let mut parent_idx = self.store.load(&input.parent_dir_id).await?;
        if parent_idx.dirs.iter().any(|d| !d.deleted && d.name == input.name) {
            return Err(DocError::NameConflict(input.name));
        }
        let dir_id = generate_dir_id();
        let dir_path = self.store.dir_path(&dir_id)?;
        fs::create_dir_all(&dir_path).await?;
        // Initial empty library.json for the new directory.
        let fresh = LibraryIndex {
            dir_id: dir_id.clone(),
            parent: Some(input.parent_dir_id.clone()),
            files: vec![],
            dirs: vec![],
            schema_version: 1,
            // Design §9: creator lives on the directory's own index.
            creator: input.creator.clone(),
            acl: None,
        };
        self.store.save(&fresh).await?;
        // Patch parent.
        let now = self.now();
        parent_idx.dirs.push(DirMeta {
            dir_id: dir_id.clone(),
            name: input.name.clone(),
            updated_at: now,
            deleted: false,
            // Design §9 display cache mirrored on the parent entry so tree
            // responses carry ACL state without extra per-child loads.
            creator: input.creator.clone(),
            acl: None,
        });
        self.store.save(&parent_idx).await?;
        crate::mqtt_publisher::notify_tree_changed(vec![input.parent_dir_id]);
        Ok(DirMeta {
            dir_id,
            name: input.name,
            updated_at: now,
            deleted: false,
            creator: input.creator,
            acl: None,
        })
    }

    async fn read(&self, dir_id: &str) -> Result<DirMeta> {
        validate_dir_id(dir_id)?;
        if dir_id == ROOT_DIR_ID {
            return Ok(DirMeta {
                dir_id: ROOT_DIR_ID.to_string(),
                name: ROOT_DIR_ID.to_string(),
                updated_at: self.now(),
                deleted: false,
                creator: None,
                acl: None,
            });
        }
        // Load the directory's own `library.json` to find its `parent`
        // (avoids reverse-engineering parent from the filesystem path,
        // which breaks under TempDir's random top-level directory name).
        let self_idx = self.store.load(dir_id).await?;
        let parent_id = self_idx.parent.clone().unwrap_or_else(|| {
            crate::types::ROOT_DIR_ID.to_string()
        });
        let parent_idx = self.store.load(&parent_id).await?;
        parent_idx
            .dirs
            .iter()
            .find(|d| d.dir_id == dir_id && !d.deleted)
            .cloned()
            .ok_or_else(|| DocError::DirNotFound(dir_id.to_string()))
    }

    async fn list_tree(&self, dir_id: &str) -> Result<TreeNode> {
        validate_dir_id(dir_id)?;
        let idx = self.store.load(dir_id).await?;
        let path = if dir_id == ROOT_DIR_ID {
            String::new()
        } else {
            dir_id.to_string()
        };
        Ok(TreeNode {
            dir_id: dir_id.to_string(),
            name: if dir_id == ROOT_DIR_ID {
                ROOT_DIR_ID.to_string()
            } else {
                idx.dirs
                    .iter()
                    .find(|d| d.dir_id == dir_id)
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| dir_id.to_string())
            },
            path,
            files: idx.files.into_iter().filter(|f| !f.deleted).collect(),
            dirs: idx.dirs.into_iter().filter(|d| !d.deleted).collect(),
        })
    }

    async fn rename(&self, dir_id: &str, new_name: &str) -> Result<DirMeta> {
        validate_dir_id(dir_id)?;
        validate_doc_name(new_name)?;
        if dir_id == ROOT_DIR_ID {
            return Err(DocError::BadRequest(String::from(
                "cannot rename root directory",
            )));
        }
        let dir_path = self.store.dir_path(dir_id)?;
        let _ = dir_path; // parent_id comes from the index, not the filesystem
        let self_idx = self.store.load(dir_id).await?;
        let parent_id = self_idx
            .parent
            .clone()
            .unwrap_or_else(|| ROOT_DIR_ID.to_string());
        let mut parent_idx = self.store.load(&parent_id).await?;
        let exists = parent_idx
            .dirs
            .iter()
            .any(|d| d.dir_id == dir_id && !d.deleted);
        if !exists {
            return Err(DocError::DirNotFound(dir_id.to_string()));
        }
        let dup = parent_idx
            .dirs
            .iter()
            .any(|d| d.dir_id != dir_id && !d.deleted && d.name == new_name);
        if dup {
            return Err(DocError::NameConflict(new_name.into()));
        }
        // Now we can take a mut borrow safely.
        let entry = parent_idx
            .dirs
            .iter_mut()
            .find(|d| d.dir_id == dir_id && !d.deleted)
            .expect("checked exists above");
        if entry.name == new_name {
            return Ok(entry.clone());
        }
        entry.name = new_name.to_string();
        entry.updated_at = self.now();
        let updated = entry.clone();
        self.store.save(&parent_idx).await?;
        crate::mqtt_publisher::notify_tree_changed(vec![parent_id]);
        Ok(updated)
    }

    async fn delete(&self, dir_id: &str) -> Result<()> {
        validate_dir_id(dir_id)?;
        if dir_id == ROOT_DIR_ID {
            return Err(DocError::BadRequest(String::from(
                "cannot delete root directory",
            )));
        }
        // Two-pass: first gather every (doc_id, dir_id, name) inside the
        // subtree, then record each into `.trash/` with a TrashEntry
        // sidecar (content is copied in), then remove the original file
        // and mark library entries deleted.
        let now = self.now();
        // Parent id for the change event (the tree refresh target).
        let parent_id = self
            .store
            .load(dir_id)
            .await?
            .parent
            .unwrap_or_else(|| ROOT_DIR_ID.to_string());
        let mut stack = vec![dir_id.to_string()];
        let mut to_delete: Vec<(
            String,
            String,
            String,
            std::path::PathBuf,
            Option<String>,
            Option<Vec<crate::types::Ace>>,
        )> = vec![];
        while let Some(d) = stack.pop() {
            let idx = self.store.load(&d).await?;
            for f in &idx.files {
                if f.deleted {
                    continue;
                }
                let dir_path = self.store.dir_path(&d)?;
                let file_path = dir_path.join(format!("{}.md", f.name));
                to_delete.push((
                    f.doc_id.clone(),
                    d.clone(),
                    f.name.clone(),
                    file_path,
                    f.creator.clone(),
                    f.acl.clone(),
                ));
            }
            for sub in &idx.dirs {
                if !sub.deleted {
                    stack.push(sub.dir_id.clone());
                }
            }
        }
        for (doc_id, parent_dir_id, name, file_path, creator, acl) in &to_delete {
            if !file_path.exists() {
                continue;
            }
            match fs::read_to_string(file_path).await {
                Ok(content) => {
                    let entry = TrashEntry {
                        trash_id: generate_trash_id(),
                        doc_id: Some(doc_id.clone()),
                        original_dir_id: parent_dir_id.clone(),
                        original_name: name.clone(),
                        deleted_at: now,
                        file_size_bytes: content.len() as u64,
                        // Snapshot ACL ownership for the delete→restore
                        // round-trip (design §9).
                        creator: creator.clone(),
                        acl: acl.clone(),
                    };
                    let _ = self.trash.record_doc(&entry, &content).await;
                    let _ = fs::remove_file(file_path).await;
                }
                Err(_) => continue,
            }
        }
        let mut stack = vec![dir_id.to_string()];
        while let Some(d) = stack.pop() {
            let mut idx = self.store.load(&d).await?;
            let mut changed = false;
            for f in idx.files.iter_mut() {
                if !f.deleted {
                    f.deleted = true;
                    f.updated_at = self.now();
                    changed = true;
                }
            }
            for sub in idx.dirs.iter_mut() {
                if !sub.deleted {
                    sub.deleted = true;
                    sub.updated_at = self.now();
                    changed = true;
                    stack.push(sub.dir_id.clone());
                }
            }
            if changed {
                self.store.save(&idx).await?;
            }
        }
        crate::mqtt_publisher::notify_tree_changed(vec![parent_id]);
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::document::{CreateDocumentInput, DocumentService};
    use crate::service::document_impl::LibraryDocumentService;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_clock() -> Clock {
        Arc::new(|| Utc.with_ymd_and_hms(2026, 9, 8, 12, 0, 0).unwrap())
    }

    async fn setup() -> (TempDir, Arc<LibraryDirectoryService>, Arc<LibraryDocumentService>) {
        let tmp = TempDir::new().unwrap();
        let store = LibraryStore::new(tmp.path().to_path_buf());
        store.ensure_root().await.unwrap();
        let dirs = Arc::new(
            LibraryDirectoryService::new(LibraryStore::new(tmp.path().to_path_buf()))
                .with_clock(fixed_clock()),
        );
        let docs = Arc::new(LibraryDocumentService::new(LibraryStore::new(
            tmp.path().to_path_buf(),
        )));
        (tmp, dirs, docs)
    }

    #[tokio::test]
    async fn create_dir_writes_files_and_parents_index() {
        let (tmp, dirs, _docs) = setup().await;
        let meta = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "A".into(),
                creator: None,
            })
            .await
            .unwrap();
        assert!(meta.dir_id.starts_with("dir-"));
        let root_idx = LibraryStore::new(tmp.path().to_path_buf())
            .load(ROOT_DIR_ID)
            .await
            .unwrap();
        assert!(root_idx.dirs.iter().any(|d| d.dir_id == meta.dir_id));
        assert!(tmp.path().join(&meta.dir_id).join("library.json").exists());
    }

    #[tokio::test]
    async fn create_dir_rejects_duplicate_name() {
        let (_tmp, dirs, _docs) = setup().await;
        dirs.create(CreateDirectoryInput {
            parent_dir_id: ROOT_DIR_ID.into(),
            name: "dup".into(),
            creator: None,
        })
        .await
        .unwrap();
        let err = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "dup".into(),
                creator: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, DocError::NameConflict(_)));
    }

    #[tokio::test]
    async fn read_root_and_subdir() {
        let (_tmp, dirs, _docs) = setup().await;
        let root = dirs.read(ROOT_DIR_ID).await.unwrap();
        assert_eq!(root.dir_id, ROOT_DIR_ID);
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "x".into(),
                creator: None,
            })
            .await
            .unwrap();
        let read = dirs.read(&sub.dir_id).await.unwrap();
        assert_eq!(read.name, "x");
    }

    #[tokio::test]
    async fn list_tree_returns_immediate_children() {
        let (_tmp, dirs, docs) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "A".into(),
                creator: None,
            })
            .await
            .unwrap();
        docs.create(CreateDocumentInput {
            parent_dir_id: ROOT_DIR_ID.into(),
            title: "root-doc".into(),
            content: "x".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        docs.create(CreateDocumentInput {
            parent_dir_id: sub.dir_id.clone(),
            title: "sub-doc".into(),
            content: "y".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        let tree = dirs.list_tree(ROOT_DIR_ID).await.unwrap();
        assert_eq!(tree.files.len(), 1);
        assert_eq!(tree.dirs.len(), 1);
        let sub_tree = dirs.list_tree(&sub.dir_id).await.unwrap();
        assert_eq!(sub_tree.files.len(), 1);
        assert_eq!(sub_tree.dirs.len(), 0);
    }

    #[tokio::test]
    async fn rename_dir_updates_parent_index() {
        let (_tmp, dirs, _docs) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "old".into(),
                creator: None,
            })
            .await
            .unwrap();
        let renamed = dirs.rename(&sub.dir_id, "new").await.unwrap();
        assert_eq!(renamed.name, "new");
        let root_idx = LibraryStore::new(_tmp.path().to_path_buf())
            .load(ROOT_DIR_ID)
            .await
            .unwrap();
        let entry = root_idx.dirs.iter().find(|d| d.dir_id == sub.dir_id).unwrap();
        assert_eq!(entry.name, "new");
    }

    #[tokio::test]
    async fn delete_dir_cascades_into_trash() {
        let (_tmp, dirs, docs) = setup().await;
        let sub = dirs
            .create(CreateDirectoryInput {
                parent_dir_id: ROOT_DIR_ID.into(),
                name: "bye".into(),
                creator: None,
            })
            .await
            .unwrap();
        docs.create(CreateDocumentInput {
            parent_dir_id: sub.dir_id.clone(),
            title: "inside".into(),
            content: "x".into(),
            import: None,
            creator: None,
        })
        .await
        .unwrap();
        dirs.delete(&sub.dir_id).await.unwrap();
        assert!(_tmp.path().join(".trash").exists());
        let sub_idx = LibraryStore::new(_tmp.path().to_path_buf())
            .load(&sub.dir_id)
            .await
            .unwrap();
        assert!(sub_idx.files.iter().all(|f| f.deleted));
    }

    #[tokio::test]
    async fn delete_root_rejected() {
        let (_tmp, dirs, _docs) = setup().await;
        let err = dirs.delete(ROOT_DIR_ID).await.unwrap_err();
        assert!(matches!(err, DocError::BadRequest(_)));
    }
}