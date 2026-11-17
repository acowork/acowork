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
    /// Internal only — see [`CorruptRecord::public_details`].
    pub path: String,
    /// Absolute path of the forensic copy, if it could be written.
    /// Internal only — see [`CorruptRecord::public_details`].
    pub quarantine_path: Option<String>,
    /// Raw IO/parse error text. Internal only; it can embed absolute paths
    /// and platform-specific detail, so `/health` never serializes it.
    pub error: String,
    pub detected_at: DateTime<Utc>,
}

impl CorruptRecord {
    /// The `/health` projection of a corrupt-file finding.
    ///
    /// `CorruptRecord` itself is deliberately serializable (so it can be
    /// cloned into the shared registry and logged in full), but exposing it
    /// verbatim over an unauthenticated health route would leak the host's
    /// absolute directory layout and raw IO error text. Operators still get
    /// everything they need to locate the file: project/task id, a path
    /// relative to `data_dir`, whether a forensic copy exists, and when it
    /// was seen. The detailed error stays in the PM process log.
    pub fn public_details(&self, data_dir: &Path) -> serde_json::Value {
        serde_json::json!({
            "project_id": self.project_id,
            "task_id": self.task_id,
            "relative_path": self.relative_path(data_dir),
            "quarantined": self.quarantine_path.is_some(),
            "detected_at": self.detected_at.to_rfc3339(),
            "reason": self.reason(),
        })
    }

    fn relative_path(&self, data_dir: &Path) -> String {
        let original = Path::new(&self.path);
        match original.strip_prefix(data_dir) {
            Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
            // A record from a different data_dir layout: expose the file
            // name only rather than the absolute host path.
            Err(_) => original
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "<unknown>".to_string()),
        }
    }

    /// Coarse classification, not the raw error string.
    fn reason(&self) -> &'static str {
        let e = self.error.to_ascii_lowercase();
        if e.contains("expected")
            || e.contains("eof")
            || e.contains("trailing")
            || e.contains("invalid type")
            || e.contains("missing field")
            || e.contains("key must be")
        {
            "invalid_json"
        } else if e.contains("not found")
            || e.contains("denied")
            || e.contains("os error")
            || e.contains("permission")
        {
            "io_error"
        } else {
            "unreadable"
        }
    }
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
///
/// The copy is staged and renamed, so a crash mid-copy can never leave a
/// partially written forensic file at the stable destination (which would
/// replace the only good evidence with a truncated one).
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
    let staged = dest.with_extension("json.tmp");
    if let Err(e) = fs::copy(source, &staged).await {
        tracing::warn!(from = %source.display(), to = %staged.display(), error = %e, "cannot quarantine unreadable task.json");
        let _ = fs::remove_file(&staged).await;
        return None;
    }
    if let Err(e) = fs::rename(&staged, &dest).await {
        tracing::warn!(from = %staged.display(), to = %dest.display(), error = %e, "cannot commit quarantined task.json");
        let _ = fs::remove_file(&staged).await;
        return None;
    }
    Some(dest)
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
        // Staging must not leave a `.tmp` beside the forensic copy.
        assert!(
            !first.with_extension("json.tmp").exists(),
            "staged quarantine copy must be renamed away"
        );

        // A second detection overwrites the same path instead of accumulating.
        let second = quarantine_copy(tmp.path(), "p-1", "t-1", &source)
            .await
            .expect("second copy should succeed");
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_dir(tmp.path().join(".corrupt"))
                .unwrap()
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn failed_copy_never_touches_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("missing.json");
        assert!(
            quarantine_copy(tmp.path(), "p-1", "t-2", &source)
                .await
                .is_none()
        );
        assert!(!source.exists());
        assert!(
            !quarantine_path(tmp.path(), "p-1", "t-2")
                .with_extension("json.tmp")
                .exists(),
            "a failed copy must not leave a staged file behind"
        );
    }

    /// `/health` is an unauthenticated probe: the raw registry entry holds
    /// absolute host paths and IO error text, so only a redacted projection
    /// may be serialized.
    #[test]
    fn public_details_never_leak_absolute_paths_or_raw_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().to_path_buf();
        let absolute = data_dir
            .join("projects/p-1/tasks/t-1/task.json")
            .display()
            .to_string();
        let record = CorruptRecord {
            project_id: "p-1".into(),
            task_id: "t-1".into(),
            path: absolute.clone(),
            quarantine_path: Some(format!("C:/secret/.corrupt/{absolute}")),
            error: format!("Permission denied (os error 5) while reading {absolute}"),
            detected_at: Utc::now(),
        };
        let details = record.public_details(&data_dir);
        let text = serde_json::to_string(&details).unwrap();
        assert!(!text.contains(&absolute), "leaked absolute path: {text}");
        assert!(!text.contains("os error"), "leaked raw error: {text}");
        assert!(
            !text.contains("C:/secret"),
            "leaked quarantine path: {text}"
        );
        assert_eq!(details["project_id"], "p-1");
        assert_eq!(details["task_id"], "t-1");
        assert_eq!(details["quarantined"], true);
        assert_eq!(details["reason"], "io_error");
        assert!(
            details["relative_path"]
                .as_str()
                .unwrap()
                .contains("t-1/task.json"),
            "relative path missing: {details}"
        );
    }

    #[test]
    fn public_details_classify_parse_errors_and_hide_foreign_paths() {
        let record = CorruptRecord {
            project_id: "p-2".into(),
            task_id: "t-9".into(),
            path: "/etc/other/host/task.json".into(),
            quarantine_path: None,
            error: "trailing characters at line 25 column 2".into(),
            detected_at: Utc::now(),
        };
        let details = record.public_details(Path::new("/somewhere/else"));
        assert_eq!(details["reason"], "invalid_json");
        assert_eq!(details["quarantined"], false);
        assert_eq!(details["relative_path"], "task.json");
    }
}
