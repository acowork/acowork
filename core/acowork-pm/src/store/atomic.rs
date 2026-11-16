//! 原子写 + 路径安全工具。
//!
//! 所有写操作都应通过 [`atomic_write_json`] 进行，确保文件系统级原子性；
//! 所有从外部接收的 ID 在参与路径拼接前都必须通过 [`validate_id_format`]。

use std::path::{Path, PathBuf};
use tokio::fs;

use crate::error::{PmError, Result};

/// Atomic JSON write: stage `*.json.tmp`, then `rename` over the target.
///
/// `rename` is atomic on the same filesystem (POSIX / Windows NTFS), so a
/// crash before the rename keeps the old file and one after it keeps the new
/// one — there is no half-written state.
///
/// ADR-117 D4 layer 2: before overwriting, keep the last *parseable* version
/// as `<name>.json.bak`. An externally corrupted target is deliberately not
/// backed up, so PM can never replace the only good backup with garbage.
/// The backup itself is staged + renamed, so a crash mid-write cannot destroy
/// the only good copy either. Backup failures are logged and never fail the
/// write.
pub async fn atomic_write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    backup_last_good(path).await;
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(value)?;
    fs::write(&tmp, json.as_bytes()).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

/// One previous version per file, overwritten on each successful backup.
/// A fixed suffix keeps the data directory from growing without bound, and
/// `walk_tasks` only ever looks for `task.json`, so `.bak` is never indexed.
///
/// "Parseable" means valid JSON, not "deserializes into the PM schema": a
/// syntactically valid object with a wrong field type still counts as good.
/// Closing that gap needs a `DeserializeOwned` bound on `atomic_write_json`,
/// which the `activity.json` slice writer does not satisfy yet (ADR-117 §6).
async fn backup_last_good(path: &Path) {
    let bytes = match fs::read(path).await {
        Ok(b) => b,
        Err(_) => return, // first write or unreadable target: nothing to keep
    };
    if serde_json::from_slice::<serde_json::Value>(&bytes).is_err() {
        tracing::warn!(
            path = %path.display(),
            "target is not valid JSON — leaving the existing .bak untouched"
        );
        return;
    }
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let bak = path.with_file_name(format!("{name}.bak"));
    let tmp = path.with_file_name(format!("{name}.bak.tmp"));
    if let Err(e) = fs::write(&tmp, &bytes).await {
        tracing::warn!(path = %tmp.display(), error = %e, "failed to stage .bak before overwrite");
        return;
    }
    // rename replaces the previous backup atomically on the same filesystem.
    // On failure the old .bak survives intact; only the staged copy is lost.
    if let Err(e) = fs::rename(&tmp, &bak).await {
        tracing::warn!(path = %bak.display(), error = %e, "failed to keep .bak before overwrite");
    }
}

/// 原子读 JSON（带友好错误）。
pub async fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).await?;
    let value = serde_json::from_slice(&bytes)?;
    Ok(value)
}

/// `fs::rename` 在跨文件系统时会失败。提供 fallback：先 `copy` 再 `remove`。
///
/// 用于目录迁移（reparent）跨挂载点的边界情况。
pub async fn rename_or_fallback(from: &Path, to: &Path) -> Result<()> {
    match fs::rename(from, to).await {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) /* EXDEV */ || e.kind() == std::io::ErrorKind::CrossesDevices => {
            // 跨设备：copy + remove
            fs::create_dir_all(to.parent().unwrap_or_else(|| Path::new("."))).await?;
            fs::copy(from, to).await?;
            fs::remove_dir_all(from).await?;
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// `canonicalize` 后必须仍在根目录下（防 `..` 注入 / 符号链接逃逸）。
///
/// 注意：调用前 `path` 必须存在（`canonicalize` 要求）。
pub async fn canonicalize_within(path: &Path, root: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).await?;
    let canonical_root = fs::canonicalize(root).await?;
    if !canonical.starts_with(&canonical_root) {
        return Err(PmError::PathTraversal(path.display().to_string()));
    }
    Ok(canonical)
}

/// 校验 ID 格式：`{prefix}{suffix}`，suffix 字符集 `[a-zA-Z0-9-]`，长度 3-64。
///
/// 返回 [`PmError::InvalidId`] 当格式不符。
pub fn validate_id_format(id: &str, prefix: &str, kind: &str) -> Result<()> {
    if id.len() < 3 || id.len() > 64 || !id.starts_with(prefix) {
        return Err(PmError::InvalidId(format!("{}: {}", kind, id)));
    }
    let suffix = &id[prefix.len()..];
    if !suffix.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(PmError::InvalidId(format!("{}: {}", kind, id)));
    }
    Ok(())
}

/// 任务目录内保留名（绝不能作为任务 ID 使用）。
///
/// 即使 UUID 不会撞保留名，仍然校验作为第二道防线。
pub const TASK_DIR_RESERVED: &[&str] = &[
    "task.json",
    "activity.json",
    "attachments",
    "result.json",
    "notes.jsonl",
    "checklist.json",
];

pub fn check_not_reserved(id: &str) -> Result<()> {
    if TASK_DIR_RESERVED.contains(&id) {
        return Err(PmError::ReservedId(id.to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn atomic_write_then_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.json");
        atomic_write_json(&path, &serde_json::json!({"a": 1})).await.unwrap();
        let val: serde_json::Value = read_json(&path).await.unwrap();
        assert_eq!(val["a"], 1);
    }

    #[tokio::test]
    async fn bak_keeps_the_last_parseable_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.json");
        let bak = path.with_file_name("task.json.bak");

        atomic_write_json(&path, &serde_json::json!({"v": 1}))
            .await
            .unwrap();
        assert!(!bak.exists(), "a first write has no previous version to keep");

        atomic_write_json(&path, &serde_json::json!({"v": 2}))
            .await
            .unwrap();
        let preserved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&bak).unwrap()).unwrap();
        assert_eq!(preserved["v"], 1);
        assert!(
            !path.with_file_name("task.json.bak.tmp").exists(),
            "the staged copy must be renamed away, never left behind"
        );
    }

    #[tokio::test]
    async fn corrupted_target_never_replaces_the_good_bak() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task.json");
        let bak = path.with_file_name("task.json.bak");

        atomic_write_json(&path, &serde_json::json!({"v": 1}))
            .await
            .unwrap();
        atomic_write_json(&path, &serde_json::json!({"v": 2}))
            .await
            .unwrap();
        let good_bak = std::fs::read(&bak).unwrap();

        // An external process appends garbage, then PM writes again: the
        // last good backup must survive that overwrite.
        std::fs::write(&path, b"{\"v\": 3} trailing").unwrap();
        atomic_write_json(&path, &serde_json::json!({"v": 4}))
            .await
            .unwrap();

        assert_eq!(std::fs::read(&bak).unwrap(), good_bak);
        let current: serde_json::Value = read_json(&path).await.unwrap();
        assert_eq!(current["v"], 4);
    }

    #[test]
    fn id_format_validation() {
        assert!(validate_id_format("t-abc123", "t-", "task").is_ok());
        assert!(validate_id_format("t-", "t-", "task").is_err()); // empty suffix
        assert!(validate_id_format("p-abc", "t-", "task").is_err()); // wrong prefix
        assert!(validate_id_format("t-abc/def", "t-", "task").is_err()); // invalid char
        assert!(validate_id_format(&format!("t-{}", "x".repeat(70)), "t-", "task").is_err());
    }

    #[test]
    fn reserved_names_rejected() {
        assert!(check_not_reserved("task.json").is_err());
        assert!(check_not_reserved("attachments").is_err());
        assert!(check_not_reserved("t-abc").is_ok());
    }
}