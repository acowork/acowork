//! `.trash/` directory storage — one slot per soft-deleted document.
//!
//! Layout (each slot = a content file + a metadata sidecar):
//! ```text
//! .trash/
//!   20260905T063200_PRD.md          <- original `.md` body, renamed
//!   20260905T063200_PRD.meta.json   <- `TrashEntry` for restore / list
//! ```
//!
//! The `trash_id` is independent of the original `doc_id` so we can
//! restore the document under a fresh id when its target directory has
//! since been removed or renamed. See `service::trash` for the public
//! API used by REST handlers.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use tokio::fs;

use crate::error::{DocError, Result};
use crate::path::ensure_within_library;
use crate::store::atomic::{atomic_write_json, read_json};
use crate::types::TrashEntry;

pub struct TrashStore {
    root: PathBuf,
}

impl TrashStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Absolute path of `.trash/` inside the library root.
    pub fn dir(&self) -> PathBuf {
        self.root.join(".trash")
    }

    /// Ensure `.trash/` exists (called from startup / lazy paths).
    pub async fn ensure(&self) -> Result<()> {
        fs::create_dir_all(self.dir()).await?;
        Ok(())
    }

    /// Drop a document's content into `.trash/{stamp}_{safe_name}.md`
    /// and write the matching `.meta.json` sidecar. Returns the trash
    /// slot filename so the caller can later restore / purge.
    pub async fn record_doc(
        &self,
        entry: &TrashEntry,
        content: &str,
    ) -> Result<String> {
        self.ensure().await?;
        // Cross-check: refuse to escape the library root.
        let dest_dir = ensure_within_library(&self.root, &self.dir())?;
        let stamp = entry.deleted_at.format("%Y%m%dT%H%M%S").to_string();
        let safe_name = entry.original_name.replace(['\\', '/', '.'], "_");
        let stamped = format!("{stamp}_{safe_name}.md");
        let body_path = dest_dir.join(&stamped);
        let meta_path = body_path.with_extension("meta.json");
        fs::write(&body_path, content.as_bytes()).await?;
        atomic_write_json(&meta_path, entry).await?;
        Ok(stamped)
    }

    /// Read every `.meta.json` in `.trash/`. Skips files without a
    /// sidecar (defensive: a crash between `write body` and `write meta`
    /// leaves an orphan that `purge_orphans` would clean up).
    pub async fn list_entries(&self) -> Result<Vec<TrashEntry>> {
        self.ensure().await?;
        let mut out = Vec::new();
        let mut entries = fs::read_dir(self.dir()).await?;
        while let Some(e) = entries.next_entry().await? {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if !name.ends_with(".meta.json") {
                continue;
            }
            match read_json::<TrashEntry>(&e.path()).await {
                Ok(meta) => out.push(meta),
                Err(_) => continue, // skip corrupt entries
            }
        }
        // Most recent first.
        out.sort_by_key(|e| std::cmp::Reverse(e.deleted_at));
        Ok(out)
    }

    /// Locate a single trash slot by id; returns both the entry and the
    /// content file path on disk.
    pub async fn find(&self, trash_id: &str) -> Result<(TrashEntry, PathBuf)> {
        let entries = self.list_entries().await?;
        for entry in entries {
            if entry.trash_id == trash_id {
                let dir = self.dir();
                let stamp = entry.deleted_at.format("%Y%m%dT%H%M%S").to_string();
                let safe_name = entry.original_name.replace(['\\', '/', '.'], "_");
                let body = dir.join(format!("{stamp}_{safe_name}.md"));
                if !body.exists() {
                    return Err(DocError::TrashMissing(trash_id.to_string()));
                }
                return Ok((entry, body));
            }
        }
        Err(DocError::TrashMissing(trash_id.to_string()))
    }

    /// Read the content file of a trash slot.
    pub async fn read_content(&self, body_path: &Path) -> Result<String> {
        Ok(fs::read_to_string(body_path).await?)
    }

    /// Drop a trash slot (meta + content).
    pub async fn purge(&self, trash_id: &str) -> Result<()> {
        let (entry, body) = self.find(trash_id).await?;
        let meta = body.with_extension("meta.json");
        let _ = fs::remove_file(&body).await;
        let _ = fs::remove_file(&meta).await;
        let _ = entry; // silence unused
        Ok(())
    }

    /// Drop every slot whose `deleted_at` is older than `cutoff`. Returns
    /// the number of slots removed. Used by the 30-day lazy purge
    /// (design §3.3) — no background task is needed; we run this on
    /// `list` / `restore` / startup.
    pub async fn purge_expired(&self, cutoff: DateTime<Utc>) -> Result<usize> {
        let entries = self.list_entries().await?;
        let mut removed = 0;
        for entry in entries {
            if entry.deleted_at < cutoff {
                let dir = self.dir();
                let stamp = entry.deleted_at.format("%Y%m%dT%H%M%S").to_string();
                let safe_name = entry.original_name.replace(['\\', '/', '.'], "_");
                let body = dir.join(format!("{stamp}_{safe_name}.md"));
                let meta = body.with_extension("meta.json");
                let _ = fs::remove_file(&body).await;
                let _ = fs::remove_file(&meta).await;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Drop the physical `.md` for an entry without touching the
    /// sidecar — used by the document-level restore flow which moves
    /// the body back into the library tree and then forgets the trash
    /// sidecar in one step.
    pub async fn move_body_back(&self, body_path: &Path, dest: &Path) -> Result<()> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::rename(body_path, dest).await?;
        Ok(())
    }

    /// Drop just the meta sidecar (used after `move_body_back`).
    pub async fn drop_meta_for(&self, body_path: &Path) -> Result<()> {
        let meta = body_path.with_extension("meta.json");
        let _ = fs::remove_file(&meta).await;
        Ok(())
    }

    /// Move a trash slot's body to a new location, **renaming** the
    /// filename stem. Used when restoring under a different name to
    /// avoid collisions. The meta sidecar is rewritten to track the
    /// rename, but the file stays in `.trash/` until the caller
    /// explicitly purges or restores.
    pub async fn rename_slot(
        &self,
        trash_id: &str,
        new_original_name: &str,
    ) -> Result<()> {
        let (mut entry, body) = self.find(trash_id).await?;
        let stamp = entry.deleted_at.format("%Y%m%dT%H%M%S").to_string();
        let new_safe = new_original_name.replace(['\\', '/', '.'], "_");
        let new_body = self.dir().join(format!("{stamp}_{new_safe}.md"));
        let new_meta = new_body.with_extension("meta.json");
        if body != new_body {
            fs::rename(&body, &new_body).await?;
        }
        entry.original_name = new_original_name.to_string();
        atomic_write_json(&new_meta, &entry).await?;
        // Remove old meta if filename changed.
        let old_meta = body.with_extension("meta.json");
        if old_meta != new_meta {
            let _ = fs::remove_file(&old_meta).await;
        }
        Ok(())
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ROOT_DIR_ID;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn tmp_store() -> (TempDir, TrashStore) {
        let dir = TempDir::new().unwrap();
        let store = TrashStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    fn entry(trash_id: &str, name: &str, deleted_at: DateTime<Utc>) -> TrashEntry {
        TrashEntry {
            trash_id: trash_id.to_string(),
            doc_id: Some(format!("doc-{name}-id")),
            original_dir_id: ROOT_DIR_ID.to_string(),
            original_name: name.to_string(),
            deleted_at,
            file_size_bytes: 0,
            creator: None,
            acl: None,
        }
    }

    #[tokio::test]
    async fn record_then_list_roundtrip() {
        let (_tmp, store) = tmp_store();
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
        let e = entry("tr-aaaaaaaaaaaa", "PRD", now);
        store.record_doc(&e, "# hello").await.unwrap();
        let list = store.list_entries().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].trash_id, "tr-aaaaaaaaaaaa");
        assert_eq!(list[0].original_name, "PRD");
    }

    #[tokio::test]
    async fn find_returns_body_path_and_content() {
        let (_tmp, store) = tmp_store();
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
        let e = entry("tr-bbbbbbbbbbbb", "DOC", now);
        store.record_doc(&e, "body").await.unwrap();
        let (meta, body) = store.find("tr-bbbbbbbbbbbb").await.unwrap();
        assert_eq!(meta.original_name, "DOC");
        let content = store.read_content(&body).await.unwrap();
        assert_eq!(content, "body");
    }

    #[tokio::test]
    async fn purge_removes_both_files() {
        let (_tmp, store) = tmp_store();
        let now = Utc.with_ymd_and_hms(2026, 9, 5, 12, 0, 0).unwrap();
        store.record_doc(&entry("tr-cccccccccccc", "X", now), "y").await.unwrap();
        store.purge("tr-cccccccccccc").await.unwrap();
        assert_eq!(store.list_entries().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn purge_expired_only_removes_old() {
        let (_tmp, store) = tmp_store();
        let old = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let recent = Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0).unwrap();
        store.record_doc(&entry("tr-old00000000", "A", old), "x").await.unwrap();
        store.record_doc(&entry("tr-new00000000", "B", recent), "y").await.unwrap();
        let cutoff = Utc.with_ymd_and_hms(2026, 6, 1, 0, 0, 0).unwrap();
        let removed = store.purge_expired(cutoff).await.unwrap();
        assert_eq!(removed, 1);
        let remaining = store.list_entries().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].trash_id, "tr-new00000000");
    }
}
