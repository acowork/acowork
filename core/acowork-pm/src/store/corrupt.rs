//! Forensic copies of unreadable task files (ADR-117 D4, layer 1).
//!
//! A corrupt `task.json` stays where it is. The tree is the single source of
//! truth, so moving the file away would silently delete the task from the
//! store — the exact "my projects are gone" symptom this defence exists to
//! prevent. Instead we keep a byte-for-byte copy under
//! `{data_dir}/.corrupt/{project_id}/{task_id}/task.json` and surface the
//! record through `/health`, so an operator can restore it by copying back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::Serialize;
use tokio::fs;

/// Snapshot of unreadable files found during the last index rebuild.
/// Rebuilt from scratch on every rebuild, so a repaired file drops off.
pub type CorruptRegistry = Arc<RwLock<Vec<CorruptRecord>>>;

#[derive(Debug, Clone, Serialize)]
pub struct CorruptRecord {
    pub project_id: String,
    pub task_id: String,
    /// Absolute path of the original file, still in the authoritative tree.
    pub path: String,
    /// Absolute path of the forensic copy, if it could be written.
    pub quarantine_path: Option<String>,
    pub error: String,
    pub detected_at: DateTime<Utc>,
}

/// One stable copy per task. A fixed path (rather than a timestamped
/// directory) keeps repeated restarts from growing `.corrupt/` forever and
/// makes the manual restore path unambiguous.
pub fn quarantine_path(data_dir: &Path, project_id: &str, task_id: &str) -> PathBuf {
    data_dir
        .join(".corrupt")
        .join(project_id)
        .join(task_id)
        .join("task.json")
}

/// Copy an unreadable file aside for forensics. Best effort: a failed copy
/// loses only the duplicate, never the original, and never fails the rebuild.
pub async fn quarantine_copy(
    data_dir: &Path,
    project_id: &str,
    task_id: &str,
    source: &Path,
) -> Option<PathBuf> {
    let dest = quarantine_path(data_dir, project_id, task_id);
    if let Some(parent) = dest.parent()
        && let Err(e) = fs::create_dir_all(parent).await
    {
        tracing::warn!(path = %parent.display(), error = %e, "cannot create corrupt quarantine dir");
        return None;
    }
    match fs::copy(source, &dest).await {
        Ok(_) => Some(dest),
        Err(e) => {
            tracing::warn!(from = %source.display(), to = %dest.display(), error = %e, "cannot quarantine unreadable task.json");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn quarantine_copy_is_byte_for_byte_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("task.json");
        let bytes = b"{\"id\":\"t-1\"} trailing junk";
        std::fs::write(&source, bytes).unwrap();

        let first = quarantine_copy(tmp.path(), "p-1", "t-1", &source)
            .await
            .expect("copy should succeed");
        assert_eq!(std::fs::read(&first).unwrap(), bytes);
        assert_eq!(first, quarantine_path(tmp.path(), "p-1", "t-1"));
        // The original stays in the authoritative tree.
        assert_eq!(std::fs::read(&source).unwrap(), bytes);

        // A second detection overwrites the same path instead of accumulating.
        let second = quarantine_copy(tmp.path(), "p-1", "t-1", &source)
            .await
            .expect("second copy should succeed");
        assert_eq!(first, second);
        assert_eq!(std::fs::read_dir(tmp.path().join(".corrupt")).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn failed_copy_never_touches_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("missing.json");
        assert!(quarantine_copy(tmp.path(), "p-1", "t-2", &source)
            .await
            .is_none());
        assert!(!source.exists());
    }
}
