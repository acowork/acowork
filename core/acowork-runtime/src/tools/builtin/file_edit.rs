//! File edit tool — precise string replacement in files.
//!
//! Matching strategy (in order):
//! 1. Exact string match — fast path, preferred.
//! 2. Whitespace-flexible line matching — normalizes whitespace differences
//!    (indentation, trailing spaces, tab/space mixing) to handle LLM-generated
//!    `old_text` that doesn't exactly match file content.
//!
//! Concurrency
//! -----------
//! The read–match–write sequence is non-atomic at the OS level. Without
//! per-path serialization, N concurrent edits to the same file all read the
//! original content, each builds `new_content` with only its own delta, and
//! the last `tokio::fs::write` wins — silently dropping every earlier edit
//! even though each call returns `ok: true`. We hold a per-path async mutex
//! across the entire read → match → write window to prevent this. Locks are
//! scoped per path, so edits to *different* files still parallelize. The
//! path → lock map is bounded: idle entries (no live holder) are swept once
//! it exceeds `MAX_IDLE_LOCKS`, so long-lived sessions that touch many
//! distinct paths do not leak memory.
//!
//! Line endings
//! ------------
//! CRLF normalization is automatic. `detect_line_ending` inspects the file
//! content; if the file uses CRLF uniformly (or predominantly), both
//! `old_text` and `new_text` are upgraded LF→CRLF before matching and
//! splicing. Callers may therefore pass LF-only `old_text` against a CRLF
//! repository without rewriting their input. The replacement byte range
//! stays in raw file coordinates, so the file's existing line endings are
//! preserved byte-for-byte outside the replaced region.
//!
//! Files with a genuinely mixed line ending (CRLF and LF interleaved with
//! no clear majority) are left alone — auto-upgrade would silently turn LF
//! lines into CRLF, which is not what the caller asked for. The caller
//! should pre-normalize the file in that case.
//!
//! Internally `compute_line_spans` strips a leading `\r` from each line's
//! `content_end` when followed by `\n`, so the comparison itself is
//! line-terminator-agnostic.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

/// Per-file mutex guarding one critical section.
type FileLock = Arc<Mutex<()>>;

/// Max entries kept in `PathLockMap` before idle entries are swept.
///
/// Entries whose `Arc` handle has been dropped (`Weak::strong_count() == 0`)
/// can never be acquired again — the next request rebuilds them — so they are
/// pure cache. A dead entry costs ~200 bytes; 256 ≈ 40KB worst case, which
/// covers typical sessions (dozens of distinct edited paths) while keeping
/// memory bounded. Entries with a live holder (an in-flight edit) are never
/// removed — that would break mutual exclusion between concurrent edits.
const MAX_IDLE_LOCKS: usize = 256;

/// Map from resolved path to a weak reference of its `FileLock`. Guarded by
/// an outer mutex. Weak refs let entries be swept once their lock is idle,
/// bounding the map for long-lived sessions that touch many distinct paths.
type PathLockMap = Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>;

pub struct FileEditTool {
    /// Per-path serialization lock. Maps full resolved path → mutex held
    /// across the read→match→write critical section.
    locks: Arc<PathLockMap>,
}

impl Default for FileEditTool {
    fn default() -> Self {
        Self {
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl FileEditTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "file_edit".to_string(),
            // DEPRECATED-TOOL: this description used to tell the model to
            // "use file_read to re-read the target area". `file_read` is
            // unregistered (see `tools/builtin/mod.rs`), so the instruction
            // now names the shell commands that actually exist. Restore the
            // old wording together with the registration.
            description: "Edit a file by replacing an exact string match with new content. Exact matching is preferred; Ensure that before calling, you have re-read the target area (e.g. `sed -n '120,140p' <path>` on bash, `Get-Content <path> | Select-Object -Skip 119 -First 21` on PowerShell) so old_text is copied from the file rather than from memory; Strictly copy old_text byte-by-byte: indentation, CRLF, and trailing whitespace must exactly match the actual file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path" },
                    "old_text": { "type": "string", "description": "The exact text to find and replace. Strictly copy old_text byte-by-byte: indentation, CRLF, and trailing whitespace must exactly match the actual file. CRLF is common in Windows repositories." },
                    "new_text": { "type": "string", "description": "The replacement text" }
                },
                "required": ["path", "old_text", "new_text"]
            }),
        }
    }
}

/// Get (or lazily create) the per-path mutex for `full_path`.
/// The outer map lock is held only long enough to look up / insert an entry;
/// the returned `FileLock` is what callers lock for the critical section.
///
/// When the map exceeds `MAX_IDLE_LOCKS`, entries with no live `Arc` holder
/// are swept first, so the map stays bounded for sessions that touch many
/// distinct paths. Sweeping only drops entries that cannot be re-acquired —
/// any lock still held by an in-flight edit is retained.
async fn path_lock_for(map: &Arc<PathLockMap>, path: &Path) -> FileLock {
    let mut guard = map.lock().await;
    if guard.len() > MAX_IDLE_LOCKS {
        guard.retain(|_, lock| lock.strong_count() > 0);
    }
    if let Some(lock) = guard.get(path).and_then(|lock| lock.upgrade()) {
        return lock;
    }
    let lock: FileLock = Arc::new(Mutex::new(()));
    guard.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

// ---------------------------------------------------------------------------
// Matching helpers
// ---------------------------------------------------------------------------

/// Normalize whitespace in a line for flexible comparison:
/// - Trim trailing spaces/tabs
/// - Collapse runs of spaces/tabs to a single space
fn normalize_line(line: &str) -> String {
    let trimmed = line.trim_end_matches([' ', '\t']);
    let mut normalized = String::with_capacity(trimmed.len());
    let mut in_whitespace_run = false;

    for ch in trimmed.chars() {
        if ch == ' ' || ch == '\t' {
            if !in_whitespace_run {
                normalized.push(' ');
                in_whitespace_run = true;
            }
        } else {
            normalized.push(ch);
            in_whitespace_run = false;
        }
    }

    normalized
}

/// Byte ranges of each line in the content.
#[derive(Debug, Clone, Copy)]
struct LineSpan {
    start: usize,       // byte offset of first character
    content_end: usize, // byte offset after last non-line-terminator character
    end: usize,         // byte offset after line terminator (or content_end for last line)
}

fn compute_line_spans(content: &str) -> Vec<LineSpan> {
    let mut spans = Vec::new();
    let bytes = content.as_bytes();
    let mut line_start = 0usize;

    for (idx, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' {
            let mut content_end = idx;
            if content_end > line_start && bytes[content_end - 1] == b'\r' {
                content_end -= 1;
            }
            spans.push(LineSpan {
                start: line_start,
                content_end,
                end: idx + 1,
            });
            line_start = idx + 1;
        }
    }

    if line_start < content.len() {
        spans.push(LineSpan {
            start: line_start,
            content_end: content.len(),
            end: content.len(),
        });
    }

    spans
}

/// Detected line-ending style of the target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    /// No newlines at all, or every newline is bare `\n`.
    Lf,
    /// Every newline is `\r\n`, or CRLF vastly outnumbers bare LF.
    Crlf,
    /// Both kinds are present in non-trivial amounts and no clear majority.
    Mixed,
}

/// Count CRLF vs bare LF pairs in `content` and classify the file's
/// dominant line ending. Empty content resolves to `Lf` (a no-op for the
/// upgrader) since there is nothing to normalize.
fn detect_line_ending(content: &str) -> LineEnding {
    let bytes = content.as_bytes();
    let mut crlf = 0usize;
    let mut lf_only = 0usize;
    for i in 0..bytes.len() {
        if bytes[i] == b'\n' {
            if i > 0 && bytes[i - 1] == b'\r' {
                crlf += 1;
            } else {
                lf_only += 1;
            }
        }
    }
    match (crlf, lf_only) {
        (0, _) => LineEnding::Lf,
        (_, 0) => LineEnding::Crlf,
        (c, l) => {
            if c >= l.saturating_mul(2) {
                LineEnding::Crlf
            } else if l >= c.saturating_mul(2) {
                LineEnding::Lf
            } else {
                LineEnding::Mixed
            }
        }
    }
}

/// Upgrade bare LF to CRLF inside `text` iff `target` is `Crlf`. Every `\n`
/// not already preceded by `\r` gets a leading `\r` inserted. If `target`
/// is `Lf` or `Mixed`, the text is returned unchanged.
fn upgrade_to_target_line_ending(text: &str, target: LineEnding) -> String {
    if target != LineEnding::Crlf {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut prev = '\0';
    for ch in text.chars() {
        if ch == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(ch);
        prev = ch;
    }
    out
}

/// Match outcome with start/end byte offsets and a flag indicating whether
/// whitespace-flexible matching was used.
#[derive(Debug, Clone, Copy)]
struct MatchOutcome {
    start: usize,
    end: usize,
    used_whitespace_flex: bool,
}

/// Try whitespace-flexible line matching.
///
/// When exact matching fails, this function normalizes whitespace in both the
/// `old_string` and file content, then tries to find a unique match line-by-line.
fn try_flexible_line_match(content: &str, old_string: &str) -> Result<MatchOutcome, String> {
    let content_spans = compute_line_spans(content);
    let old_spans = compute_line_spans(old_string);

    if old_spans.is_empty() || content_spans.len() < old_spans.len() {
        return Err("old_text not found in file".into());
    }

    // Refuse to match across mismatched line endings. `compute_line_spans`
    // strips `\r` from each line's content, so the flexible matcher would
    // otherwise happily match CRLF `old_text` against LF file content (or
    // vice versa) and produce a mixed file. Catch it explicitly so the
    // caller sees a clear error rather than silently corrupting the file.
    let file_ending = detect_line_ending(content);
    let old_ending = detect_line_ending(old_string);
    if file_ending != old_ending
        && file_ending != LineEnding::Mixed
        && old_ending != LineEnding::Mixed
    {
        return Err(format!(
            "old_text line ending does not match file line ending (old={old_ending:?}, file={file_ending:?}); refusing to mix line endings"
        ));
    }

    let normalized_old_lines: Vec<String> = old_spans
        .iter()
        .map(|span| normalize_line(&old_string[span.start..span.content_end]))
        .collect();
    let normalized_content_lines: Vec<String> = content_spans
        .iter()
        .map(|span| normalize_line(&content[span.start..span.content_end]))
        .collect();

    let mut match_count = 0usize;
    let mut matched_start_line = 0usize;
    let window_size = old_spans.len();

    for start_line in 0..=(content_spans.len() - window_size) {
        let mut window_matches = true;
        for line_offset in 0..window_size {
            if normalized_content_lines[start_line + line_offset]
                != normalized_old_lines[line_offset]
            {
                window_matches = false;
                break;
            }
        }

        if window_matches {
            match_count += 1;
            if match_count == 1 {
                matched_start_line = start_line;
            }
        }
    }

    if match_count == 0 {
        return Err("old_text not found in file".into());
    }

    if match_count > 1 {
        return Err(format!(
            "old_text matches {match_count} times with whitespace flexibility; must match exactly once"
        ));
    }

    let first_span = content_spans[matched_start_line];
    let last_span = content_spans[matched_start_line + window_size - 1];
    let end = if old_string.ends_with('\n') {
        last_span.end
    } else {
        last_span.content_end
    };

    Ok(MatchOutcome {
        start: first_span.start,
        end,
        used_whitespace_flex: true,
    })
}

/// Resolve a match in `content` for `old_string`.
///
/// Tries exact string matching first. If no exact match is found, falls back
/// to whitespace-flexible line matching.
fn resolve_match(content: &str, old_string: &str) -> Result<MatchOutcome, String> {
    // Auto-normalize `old_string` to the file's line ending so LF-only inputs
    // match CRLF files. The replacement byte range stays in raw file
    // coordinates; only the haystack string is rewritten. `Mixed` files fall
    // through unchanged because we cannot infer the caller's intent.
    let ending = detect_line_ending(content);
    let normalized_old = upgrade_to_target_line_ending(old_string, ending);

    // 1. Exact match
    let mut exact_matches = content.match_indices(&normalized_old);
    if let Some((start, _)) = exact_matches.next() {
        if exact_matches.next().is_some() {
            let match_count = 2 + exact_matches.count();
            return Err(format!(
                "old_text matches {match_count} times; must match exactly once"
            ));
        }
        return Ok(MatchOutcome {
            start,
            end: start + normalized_old.len(),
            used_whitespace_flex: false,
        });
    }

    // 2. Whitespace-flexible fallback
    try_flexible_line_match(content, &normalized_old)
}

#[async_trait]
impl Tool for FileEditTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let path = params["path"].as_str().unwrap_or("");
        let old_text = params["old_text"].as_str().unwrap_or("");
        let new_text = params["new_text"].as_str().unwrap_or("");

        if path.is_empty() || old_text.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Missing required parameters".to_string()),
                token_usage: None,
            });
        }

        let full_path = acowork_core::path_utils::resolve(path, work_dir);
        tracing::debug!(
            work_dir = ?work_dir,
            input_path = %path,
            full_path = %full_path.display(),
            exists = full_path.exists(),
            "file_edit: resolving path"
        );

        // Acquire the per-path lock BEFORE reading. Holds across the entire
        // read→match→write window so concurrent edits to the same path are
        // serialized. See module-level docs for the failure mode this prevents.
        let path_lock = path_lock_for(&self.locks, &full_path).await;
        let _path_guard = path_lock.lock().await;

        let content = match tokio::fs::read_to_string(&full_path).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    work_dir = ?work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_edit: failed to read file"
                );
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to read file: {e}")),
                    token_usage: None,
                });
            }
        };

        // Auto-upgrade `new_text` to the file's line ending so CRLF files stay
        // CRLF even when the caller passes LF-only `new_text`. `resolve_match`
        // performs the same upgrade internally on `old_text`; we re-detect
        // here rather than threading the value through the match return to
        // keep `MatchOutcome` a plain `(start, end, used_whitespace_flex)`
        // triple. Detection is O(n) on `content`, which is bounded.
        let ending = detect_line_ending(&content);
        let final_new_text = upgrade_to_target_line_ending(new_text, ending);

        // Resolve the match — exact first, then whitespace-flexible fallback
        let match_outcome = match resolve_match(&content, old_text) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(error),
                    token_usage: None,
                });
            }
        };

        if match_outcome.end < match_outcome.start || match_outcome.end > content.len() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Internal matching error: invalid replacement range".into()),
                token_usage: None,
            });
        }

        let mut new_content = String::with_capacity(
            content.len() - (match_outcome.end - match_outcome.start) + new_text.len(),
        );
        new_content.push_str(&content[..match_outcome.start]);
        new_content.push_str(&final_new_text);
        new_content.push_str(&content[match_outcome.end..]);

        match tokio::fs::write(&full_path, &new_content).await {
            Ok(()) => Ok(ToolResult {
                ok: true,
                content: format!(
                    "Edited {path}: replaced 1 occurrence ({} bytes){}",
                    new_content.len(),
                    if match_outcome.used_whitespace_flex {
                        " (matched with whitespace flexibility)"
                    } else {
                        ""
                    }
                ),
                error: None,
                token_usage: None,
            }),
            Err(e) => {
                tracing::warn!(
                    work_dir = ?work_dir,
                    input_path = %path,
                    full_path = %full_path.display(),
                    error = %e,
                    "file_edit: failed to write file"
                );
                Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to write file: {e}")),
                    token_usage: None,
                })
            }
        }
        // _path_guard drops here, releasing the per-path lock.
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Build a unique temp path so parallel test runs don't collide.
    fn unique_tmp(suffix: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("acowork_file_edit_{suffix}_{pid}_{nanos}.txt"))
    }

    /// Regression: N concurrent edits on disjoint targets in the same file
    /// must all apply. Without the per-path lock, all N readers see the same
    /// pre-edit content, each builds `new_content` with only its own delta,
    /// and the last write wins — earlier edits vanish silently even though
    /// every call returns `ok: true`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_edits_same_file_all_apply() {
        let path = unique_tmp("concurrent");
        let initial = "AAA\nBBB\nCCC\nDDD\nEEE\n";
        tokio::fs::write(&path, initial).await.unwrap();

        let tool = Arc::new(FileEditTool::new());
        let applied = Arc::new(AtomicUsize::new(0));

        let targets = ["AAA", "BBB", "CCC", "DDD", "EEE"];
        let mut handles = Vec::new();
        for target in targets {
            let tool = tool.clone();
            let path = path.clone();
            let applied = applied.clone();
            handles.push(tokio::spawn(async move {
                let result = tool
                    .execute(
                        serde_json::json!({
                            "path": path.to_str().unwrap(),
                            "old_text": target,
                            "new_text": format!("{target}_x"),
                        }),
                        None,
                    )
                    .await
                    .expect("execute returned inner Err");
                if result.ok {
                    applied.fetch_add(1, Ordering::SeqCst);
                } else {
                    eprintln!("edit {target} failed: {:?}", result.error);
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(applied.load(Ordering::SeqCst), targets.len());
        let expected = "AAA_x\nBBB_x\nCCC_x\nDDD_x\nEEE_x\n";
        assert_eq!(final_content, expected);
    }

    /// Multiple concurrent edits on the SAME `old_text`: serialization means
    /// exactly one wins; the others see the already-transformed content and
    /// correctly report `old_text not found`. This pins down the serializable
    /// contract that the lock provides.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_edits_same_target_only_one_succeeds() {
        let path = unique_tmp("same_target");
        tokio::fs::write(&path, "X\nY\nZ\n").await.unwrap();

        let tool = Arc::new(FileEditTool::new());
        let mut handles = Vec::new();
        for _ in 0..3 {
            let tool = tool.clone();
            let path = path.clone();
            handles.push(tokio::spawn(async move {
                tool.execute(
                    serde_json::json!({
                        "path": path.to_str().unwrap(),
                        "old_text": "Y\n",
                        "new_text": "Y_replaced\n",
                    }),
                    None,
                )
                .await
                .expect("execute returned inner Err")
            }));
        }

        let mut ok_count = 0;
        let mut not_found_count = 0;
        for h in handles {
            let r = h.await.unwrap();
            if r.ok {
                ok_count += 1;
            } else if r.error.as_deref().unwrap_or("").contains("not found") {
                not_found_count += 1;
            }
        }
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(ok_count, 1, "exactly one edit should succeed");
        assert_eq!(not_found_count, 2, "the other two should report not found");
    }

    /// Edits to *different* paths must NOT serialize against each other.
    /// We start two edits on two different files at the same time and check
    /// both finish quickly and correctly. This guards against accidental
    /// global locking.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn edits_on_different_paths_parallel() {
        let path_a = unique_tmp("diff_a");
        let path_b = unique_tmp("diff_b");
        tokio::fs::write(&path_a, "alpha\n").await.unwrap();
        tokio::fs::write(&path_b, "beta\n").await.unwrap();

        let tool = Arc::new(FileEditTool::new());
        let t = tool.clone();
        let pa = path_a.clone();
        let pb = path_b.clone();
        let h_a = tokio::spawn(async move {
            t.execute(
                serde_json::json!({
                    "path": pa.to_str().unwrap(),
                    "old_text": "alpha",
                    "new_text": "ALPHA",
                }),
                None,
            )
            .await
            .unwrap()
        });
        let t = tool.clone();
        let h_b = tokio::spawn(async move {
            t.execute(
                serde_json::json!({
                    "path": pb.to_str().unwrap(),
                    "old_text": "beta",
                    "new_text": "BETA",
                }),
                None,
            )
            .await
            .unwrap()
        });

        let r_a = h_a.await.unwrap();
        let r_b = h_b.await.unwrap();
        let ca = tokio::fs::read_to_string(&path_a).await.unwrap();
        let cb = tokio::fs::read_to_string(&path_b).await.unwrap();
        let _ = tokio::fs::remove_file(&path_a).await;
        let _ = tokio::fs::remove_file(&path_b).await;

        assert!(r_a.ok && r_b.ok, "both edits must succeed");
        assert_eq!(ca, "ALPHA\n");
        assert_eq!(cb, "BETA\n");
    }

    /// The lock map must stay bounded: after touching far more distinct paths
    /// than `MAX_IDLE_LOCKS`, the map must have swept the dead entries instead
    /// of growing unboundedly. (No filesystem access — pure lock-map behavior.)
    #[tokio::test]
    async fn path_lock_map_is_bounded() {
        let map: Arc<PathLockMap> = Arc::new(Mutex::new(HashMap::new()));

        // Acquire and immediately release a lock for each distinct path.
        for i in 0..(MAX_IDLE_LOCKS + 64) {
            let p = PathBuf::from(format!("/tmp/acowork_lock_map_{i}"));
            drop(path_lock_for(&map, &p).await);
        }

        // The sweep runs on the first call past the threshold; afterwards the
        // map holds only live entries (here: none, since every lock was
        // dropped). The exact residual size is an implementation detail — the
        // invariant is that the map is bounded.
        let guard = map.lock().await;
        assert!(
            guard.len() <= MAX_IDLE_LOCKS,
            "lock map grew unbounded: {} entries",
            guard.len()
        );
        drop(guard);
    }

    /// CRLF round-trip: an LF-only `old_text` must replace correctly in a
    /// CRLF file without losing any `\r` on the trailing line terminator.
    /// Exercises the whitespace-flexible fallback path with CRLF content.
    /// CRLF round-trip: when editing a CRLF file, callers MUST supply CRLF
    /// line terminators in both `old_text` and `new_text`. Otherwise the
    /// replacement range (computed from `compute_line_spans`) consumes the
    /// `\r` and the user's LF-only `new_text` drops it — producing a mixed
    /// LF/CRLF file. This test pins down the supported (CRLF-explicit) usage.
    /// See module-level docs "Line endings" for the full contract.
    #[tokio::test]
    async fn crlf_file_crlf_old_text_round_trip() {
        let path = unique_tmp("crlf");
        let initial = "AAA\r\nBBB\r\nCCC\r\n";
        tokio::fs::write(&path, initial).await.unwrap();

        let tool = FileEditTool::new();
        let r = tool
            .execute(
                serde_json::json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "AAA\r\nBBB",
                    "new_text": "AAA_replaced\r\nBBB_replaced",
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r.ok, "edit failed: {:?}", r.error);

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;

        // CRLF preserved on every line, including the two we replaced.
        assert_eq!(final_content, "AAA_replaced\r\nBBB_replaced\r\nCCC\r\n");
    }

    /// LF-only `old_text` against a CRLF file must match AND the file must
    /// remain CRLF end-to-end. This is the regression that motivated the
    /// auto-upgrade: prior to this change, matching worked via the
    /// whitespace-flexible fallback but the spliced `new_text` (LF) caused
    /// the replaced lines to lose their `\r`, producing a mixed file.
    #[tokio::test]
    async fn crlf_file_lf_old_text_upgraded() {
        let path = unique_tmp("lf_old");
        let initial = "AAA\r\nBBB\r\nCCC\r\n";
        tokio::fs::write(&path, initial).await.unwrap();

        let tool = FileEditTool::new();
        let r = tool
            .execute(
                serde_json::json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "AAA\nBBB",
                    "new_text": "AAA_replaced\nBBB_replaced",
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r.ok, "edit failed: {:?}", r.error);

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;

        // Every line in the result must be CRLF, including the replaced
        // ones — no mixed line endings.
        assert_eq!(final_content, "AAA_replaced\r\nBBB_replaced\r\nCCC\r\n");
        assert!(!final_content.contains("\r\n\r\n"), "no double CRLF");
        // Sanity: zero bare LF left.
        let crlf_count = final_content.matches("\r\n").count();
        let lf_only_count = final_content.matches('\n').count() - crlf_count;
        assert_eq!(
            lf_only_count, 0,
            "replaced lines leaked LF: {final_content:?}"
        );
    }

    /// Trailing-newline variant: `old_text` ends with `\n`. CRLF upgrade
    /// must still cover the trailing `\n`.
    #[tokio::test]
    async fn crlf_file_lf_old_text_with_trailing_newline() {
        let path = unique_tmp("lf_old_trail");
        let initial = "AAA\r\nBBB\r\nCCC\r\n";
        tokio::fs::write(&path, initial).await.unwrap();

        let tool = FileEditTool::new();
        let r = tool
            .execute(
                serde_json::json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "AAA\nBBB\n",
                    "new_text": "AAA_replaced\nBBB_replaced\n",
                }),
                None,
            )
            .await
            .unwrap();
        assert!(r.ok, "edit failed: {:?}", r.error);

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;

        assert_eq!(final_content, "AAA_replaced\r\nBBB_replaced\r\nCCC\r\n");
    }

    /// LF files must NOT be upgraded. Passing CRLF `old_text` against an LF
    /// file should error out clearly (rather than silently creating a mixed
    /// file via the whitespace-flexible fallback).
    #[tokio::test]
    async fn lf_file_crlf_old_text_does_not_upgrade() {
        let path = unique_tmp("lf_file");
        let initial = "AAA\nBBB\nCCC\n";
        tokio::fs::write(&path, initial).await.unwrap();

        let tool = FileEditTool::new();
        let r = tool
            .execute(
                serde_json::json!({
                    "path": path.to_str().unwrap(),
                    "old_text": "AAA\r\nBBB",
                    "new_text": "AAA_replaced\r\nBBB_replaced",
                }),
                None,
            )
            .await
            .unwrap();
        assert!(
            !r.ok,
            "should not have matched CRLF old_text against LF file"
        );
        let err = r.error.as_deref().unwrap_or("");
        assert!(
            err.contains("line ending"),
            "expected line-ending mismatch error, got: {err:?}"
        );

        let final_content = tokio::fs::read_to_string(&path).await.unwrap();
        let _ = tokio::fs::remove_file(&path).await;
        // File must be unchanged — no partial write, no mixed line endings.
        assert_eq!(final_content, initial);
    }

    /// Pure unit tests for `detect_line_ending` and the upgrader. These don't
    /// touch the filesystem and pin down the edge cases that an integration
    /// test might miss.
    #[test]
    fn detect_line_ending_classifies_correctly() {
        assert_eq!(detect_line_ending(""), LineEnding::Lf);
        assert_eq!(detect_line_ending("no newlines here"), LineEnding::Lf);
        assert_eq!(detect_line_ending("AAA\nBBB\n"), LineEnding::Lf);
        assert_eq!(detect_line_ending("AAA\r\nBBB\r\n"), LineEnding::Crlf);
        // One bare LF + many CRLFs → CRLF (majority).
        assert_eq!(
            detect_line_ending("AAA\nBBB\r\nCCC\r\nDDD\r\n"),
            LineEnding::Crlf
        );
        // Many CRLFs + one bare LF → Mixed (no 2x majority).
        assert_eq!(detect_line_ending("AAA\nBBB\r\n"), LineEnding::Mixed);
    }

    #[test]
    fn upgrade_inserts_cr_only_for_unpaired_lf() {
        let upgraded = upgrade_to_target_line_ending("AAA\nBBB\n", LineEnding::Crlf);
        assert_eq!(upgraded, "AAA\r\nBBB\r\n");

        // Already-CRLF input passes through unchanged (no double `\r`).
        let passthrough = upgrade_to_target_line_ending("AAA\r\nBBB\r\n", LineEnding::Crlf);
        assert_eq!(passthrough, "AAA\r\nBBB\r\n");

        // Empty + non-newline content unchanged.
        assert_eq!(upgrade_to_target_line_ending("", LineEnding::Crlf), "");
        assert_eq!(
            upgrade_to_target_line_ending("no newlines", LineEnding::Crlf),
            "no newlines"
        );

        // Lf target: pass through regardless of input bytes.
        assert_eq!(
            upgrade_to_target_line_ending("AAA\nBBB\n", LineEnding::Lf),
            "AAA\nBBB\n"
        );
        // Mixed target: pass through (no implicit upgrade).
        assert_eq!(
            upgrade_to_target_line_ending("AAA\nBBB\n", LineEnding::Mixed),
            "AAA\nBBB\n"
        );

        // Non-ASCII content preserved verbatim; UTF-8 sequences never split.
        let utf8 = "你好\n世界\n";
        assert_eq!(
            upgrade_to_target_line_ending(utf8, LineEnding::Crlf),
            "你好\r\n世界\r\n"
        );
        assert_eq!(upgrade_to_target_line_ending(utf8, LineEnding::Lf), utf8);
    }
}
