//! Unified cache-management wire contract (ADR-123 §1.2 D2).
//!
//! Every service that owns bytes on disk answers a pair of admin endpoints:
//!
//! | verb | service path | who calls it |
//! |---|---|---|
//! | `GET` | `…/storage-report` … | the Gateway's aggregator (`http::cache_api`) |
//! | `POST` | `…/storage-cleanup` | the Gateway's forwarder, on a Desktop click |
//!
//! The Gateway only aggregates and forwards (ADR-123 §1.3 red line 4): it never
//! scans a service's directory and never deletes anything a service owns. That
//! is why the types live here rather than in the Gateway — the same struct is
//! serialized by five different processes, and a one-sided field rename would
//! break the aggregate silently instead of failing a build.
//!
//! ## Why a cleanup request carries ids and nothing else
//!
//! [`CleanupRequest::ids`] names *which categories to clean*, never how much or
//! what to keep — the retention policy (log age, TTL window, batch size) belongs
//! to the owner (ADR-123 §1.3 red line 3). The Gateway and the Desktop cannot
//! express "delete 200 MB", so they cannot cut a corner that loses user assets.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Whether deleting this category can lose a user/business asset (ADR-123 D4).
///
/// The single test: *would deleting it lose something the user put there?*
/// Logs, swept chunk stores and download caches are [`StorageKind::Cache`]
/// (regenerable or already-due-for-collection); `memory.db`, the document
/// library, project SQLite, chat attachments and the workspace are
/// [`StorageKind::Data`] — shown with a size, never offered a delete button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    Cache,
    Data,
}

/// One row of a service's storage report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageItem {
    /// Stable machine key the cleanup request echoes back (`"logs"`,
    /// `"uploads"`, `"transfers"`, …). Never localized; the Desktop maps it to
    /// a label and falls back to the raw id for an unknown one, so a service
    /// can add a category without a frontend release.
    pub id: String,
    /// Cache vs data (see [`StorageKind`]).
    pub kind: StorageKind,
    pub bytes: u64,
    /// Whether the owner has a cleanup verb for this category. Always `false`
    /// for [`StorageKind::Data`] — the two fields are separate so a read-only
    /// cache-class row (e.g. the in-flight transfer store, which only the TTL
    /// sweep may touch) can still render without a button.
    pub cleanable: bool,
}

/// A service's answer to `GET …/storage-report`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageReport {
    pub items: Vec<StorageItem>,
}

impl StorageReport {
    pub fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.bytes).sum()
    }

    /// Whether any row can be cleaned — the Desktop hides the "clean" action
    /// for a service whose report says no.
    pub fn has_cleanable(&self) -> bool {
        self.items.iter().any(|i| i.cleanable)
    }
}

/// Body of `POST …/storage-cleanup`: category ids, nothing else (see the module
/// doc).
///
/// ## The two rules every owner implements identically
///
/// 1. **Validate the whole list first, then act.** A request that names one
///    collectable and one bogus category must delete nothing: an id can only be
///    refused before the sweep starts, never halfway through it. (An owner that
///    deletes as it walks the list leaves the disk half-swept and reports an
///    error the user reads as "nothing happened".)
/// 2. **Two distinct refusals**, because they mean different things to the
///    caller: an id this service does not know → `400`; a category it knows but
///    will not delete (data class) → `409`. Never a silent no-op — the Desktop
///    prints the freed-byte count from the answer, so a no-op "success" would be
///    a lie the user then acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupRequest {
    #[serde(default)]
    pub ids: Vec<String>,
}

/// Answer to a cleanup: the bytes the owner actually freed.
///
/// Measured by the owner (size before − size after), because only it knows
/// what its retention policy decided to drop. Clients refresh the report
/// afterwards; this field is for the "已清理 X" feedback.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupResult {
    pub freed_bytes: u64,
}

/// Volume watermark state published on `acowork/desktop/storage` (ADR-123 D5/D7).
///
/// Both fields are volume-level facts about the machine the Gateway runs on —
/// no service directory is ever traversed to produce them (ADR-123 §1.3
/// red line 1's only other exception). `alarm` is the derived verdict so the
/// Desktop never has to re-implement the threshold comparison.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DiskWatermark {
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// `(total - available) / total * 100`, clamped to `0.0..=100.0`.
    pub used_percent: f64,
    pub threshold_percent: f64,
    /// `used_percent >= threshold_percent` — the dialog's trigger.
    pub alarm: bool,
}

impl DiskWatermark {
    /// Derive the verdict from a volume sample. `total == 0` (an unreadable
    /// volume) reports no alarm: a false "disk full" dialog is worse than a
    /// missed one, and the next tick retries.
    pub fn new(total_bytes: u64, available_bytes: u64, threshold_percent: f64) -> Self {
        let available_bytes = available_bytes.min(total_bytes);
        let used_percent = if total_bytes == 0 {
            0.0
        } else {
            ((total_bytes - available_bytes) as f64 / total_bytes as f64 * 100.0).clamp(0.0, 100.0)
        };
        Self {
            total_bytes,
            available_bytes,
            used_percent,
            threshold_percent,
            alarm: total_bytes > 0 && used_percent >= threshold_percent,
        }
    }
}

/// Total bytes of every regular file under `root`.
///
/// Shared by all five owners so the "how big is this directory" answer is one
/// implementation, not five subtly different ones (missing root = 0, symlinks
/// are not followed, unreadable entries are skipped rather than failing the
/// whole report — a report is a diagnostic, and one locked log file must not
/// blank the card).
///
/// `ponytail:` a full walk on every report request; no snapshot cache. The
/// trees involved (logs, chunk stores, SQLite + library dirs) are small enough
/// that a card open is measured in milliseconds, and a stale cache would make
/// the number the user acts on wrong. Upgrade path if a workspace ever grows
/// past ~10^5 files: have the owner keep a counter updated on write.
pub async fn dir_bytes(root: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&dir).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total += entry.metadata().await.map(|m| m.len()).unwrap_or(0);
            }
            // Symlinks are neither: following them could walk out of the
            // service's own tree and double-count a shared file.
        }
    }
    total
}

/// How long a log file nobody has touched stays on disk before a cleanup may
/// collect it. One policy for every log tree on the machine (Gateway, Node,
/// Runtime) — a Runtime writes one file per boot, so a week is already far more
/// history than anyone reads.
pub const LOG_RETENTION_SECS: u64 = 7 * 24 * 3600;

/// Delete regular files under `dir` (recursively) whose mtime is older than
/// `max_age_secs`; returns the bytes actually removed.
///
/// Recursive on purpose: the byte figure a service *reports* comes from
/// [`dir_bytes`], which walks the whole tree. A purge that stopped at the first
/// level would let the card show "500 MB" next to a button that frees 0 — the
/// reported tree and the collectable tree have to be the same tree.
///
/// **Age, not name**: a file still being appended to is never old, whatever it
/// is called, so a live log survives regardless of the naming scheme its owner
/// chose. On Windows an open file fails `remove_file` and is simply skipped —
/// the next cleanup collects it. Directories (and therefore symlinks, which are
/// not followed) are never removed; an emptied subdirectory is left in place,
/// same as `dir_bytes` ignores it.
pub async fn purge_files_older_than(dir: &Path, max_age_secs: u64) -> u64 {
    let mut freed = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(mut entries) = tokio::fs::read_dir(&current).await else {
            continue;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let Ok(ft) = entry.file_type().await else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if !ft.is_file() {
                continue; // symlink: never followed, never removed
            }
            let Ok(meta) = entry.metadata().await else {
                continue;
            };
            let old = meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .map(|age| age.as_secs() > max_age_secs)
                .unwrap_or(false);
            if !old {
                continue;
            }
            let len = meta.len();
            if tokio::fs::remove_file(entry.path()).await.is_ok() {
                freed += len;
            }
        }
    }
    freed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watermark_verdict_follows_the_threshold() {
        // 92 of 100 GB used → alarm at 90%.
        let w = DiskWatermark::new(100, 8, 90.0);
        assert!(w.alarm);
        assert!((w.used_percent - 92.0).abs() < 1e-9);

        // 50% used → no alarm.
        let w = DiskWatermark::new(100, 50, 90.0);
        assert!(!w.alarm);
    }

    #[test]
    fn unreadable_volume_never_raises_a_false_alarm() {
        let w = DiskWatermark::new(0, 0, 90.0);
        assert!(!w.alarm);
        assert_eq!(w.used_percent, 0.0);
    }

    #[tokio::test]
    async fn dir_bytes_sums_nested_files_and_ignores_a_missing_root() {
        let root = std::env::temp_dir().join(format!(
            "acowork-dir-bytes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(root.join("nested")).await.unwrap();
        tokio::fs::write(root.join("a.log"), b"12345").await.unwrap();
        tokio::fs::write(root.join("nested/b.log"), b"123")
            .await
            .unwrap();

        assert_eq!(dir_bytes(&root).await, 8);
        assert_eq!(dir_bytes(&root.join("missing")).await, 0);

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    /// The purge reaches a file the report counted: nested subtrees are walked,
    /// and a fresh file in a nested directory survives.
    #[tokio::test]
    async fn purge_walks_subdirectories_like_the_report_does() {
        let root = std::env::temp_dir().join(format!(
            "acowork-purge-nested-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(root.join("old")).await.unwrap();
        tokio::fs::create_dir_all(root.join("fresh")).await.unwrap();
        tokio::fs::write(root.join("old/nested.log"), b"1234")
            .await
            .unwrap();
        tokio::fs::write(root.join("fresh/nested.log"), b"1234")
            .await
            .unwrap();

        // Age only the `old` subtree past the ceiling.
        let old_file = std::fs::File::options()
            .write(true)
            .open(root.join("old/nested.log"))
            .unwrap();
        old_file
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(60))
            .unwrap();
        drop(old_file);

        assert_eq!(purge_files_older_than(&root, 30).await, 4);
        assert!(!root.join("old/nested.log").exists());
        assert!(root.join("fresh/nested.log").exists());

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn purge_collects_only_files_past_the_age_ceiling() {
        let root = std::env::temp_dir().join(format!(
            "acowork-purge-old-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(root.join("fresh.log"), b"12345").await.unwrap();

        // A file written a moment ago is never old, whatever the ceiling.
        assert_eq!(purge_files_older_than(&root, LOG_RETENTION_SECS).await, 0);
        assert!(root.join("fresh.log").exists());

        // And the same file past a zero-second ceiling is collectable, with
        // the bytes it really released reported back.
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        assert_eq!(purge_files_older_than(&root, 0).await, 5);
        assert!(!root.join("fresh.log").exists());

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }
}