//! Session lifecycle management and JSONL conversation file writing.
//!
//! Provides `ConversationSession` for managing a single session's JSONL file
//! and `ConversationWriter` for channel-based single-writer thread architecture.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, OnceLock, RwLock};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::error::Result;

use crate::agent::session_state::TodoItem;

// Re-export session meta types from `acowork_core::session_meta` so callers
// using `crate::conversation::SessionMeta` keep compiling while the data model
// lives where the `SessionMetaStore` trait lives.
//
// `TodoItem` / `TodoStatus` are NOT re-exported here: they live in
// `crate::agent::session_state` (kept as a thin re-export) and the two
// surfaces use the same type via `pub use` aliasing.
pub use acowork_core::session_meta::{
    ModelUsageEntry, SessionMeta, SessionMetaStore, SessionPerf, SessionScope, SessionTokens,
    SessionVisibility, TokenStatsRow, model_usage_key,
};
use acowork_core::protocol::{ContextUsageSection, ModelCostInfo};
use acowork_core::providers::traits::UsageInfo;

/// Format version for the JSONL conversation file.
///
/// v3 (current): replaces `last_input_tokens` / `last_output_tokens` with a
///   structured `tokens` field on `SessionMeta`. See ADR-027.
const CONVERSATION_FORMAT_VERSION: u32 = 3;

/// Entry kind discriminator for `ConversationEntry.kind`.
pub const ENTRY_KIND_COMPACTION: &str = "compaction";

/// Metadata key marking an entry as an *internal runtime artifact* (e.g. the
/// output-budget nudge injected by `AgentLoop::handle_output_budget_exhausted`).
///
/// Value is boolean `true`. See [`ConversationEntry::is_internal`] for the
/// reader contract.
pub const META_KEY_INTERNAL: &str = "internal";

/// A single line in the conversation JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationEntry {
    /// Unique message ID (UUID v4)
    pub id: String,
    /// ISO 8601 timestamp with millisecond precision
    pub ts: String,
    /// For regular messages: "user" | "assistant" | "thought" | "tool_call" | "tool_result" | "system".
    /// For compaction events: still set to "system" so legacy readers degrade gracefully,
    /// but `kind` should be checked first.
    pub role: String,
    /// Full message content. For `kind="compaction"`, this carries the summary text.
    pub content: String,
    /// Optional metadata (e.g. tool_call_id, tool_name, or `CompactionEventMeta`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Entry kind. `None` or `"message"` denotes a regular message (default).
    /// `"compaction"` denotes an LLM-driven compaction event.
    /// Added in JSONL v2.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

impl ConversationEntry {
    /// Whether this entry is an internal runtime artifact rather than a genuine
    /// conversation turn.
    ///
    /// Marked by [`ConversationSession::append_internal_message`] with
    /// `metadata.internal = true`. The contract for readers is:
    ///
    /// - **Do not surface it to the user.** The Desktop filters these entries
    ///   out of the chat transcript when loading history.
    /// - **Do not replay it as a conversation turn.** The session restorer
    ///   skips them, because an interrupted turn is never resumed after a
    ///   restart — replaying a nudge would leak a stale internal instruction
    ///   into subsequent turns.
    pub fn is_internal(&self) -> bool {
        self.metadata
            .as_ref()
            .and_then(|m| m.get(META_KEY_INTERNAL))
            .and_then(|v| v.as_bool())
            == Some(true)
    }
}

/// Structured metadata payload for `kind="compaction"` entries.
///
/// Stored in `ConversationEntry.metadata` as a JSON object so legacy
/// readers can still parse the entry as opaque metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionEventMeta {
    /// First entry id covered by the summary (inclusive).
    /// May be empty if the compaction occurred before any message id was
    /// recorded (e.g. forced manual trigger on an empty session — pathological).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compacted_from_id: String,
    /// Last entry id covered by the summary (inclusive).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub compacted_to_id: String,
    /// ADR-061: compression level (1-8) applied by this compaction event.
    /// Diagnostic only; the restorer anchors on the event's *position* in
    /// the log, not on this value.
    pub level: u8,
    /// Compaction model used (diagnostic only).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    /// History token estimate before compaction (diagnostic only).
    pub before_tokens: u64,
    /// History token estimate after compaction (diagnostic only).
    pub after_tokens: u64,
}

// ── Attachment metadata (ADR-046) ──────────────────────────────────────────
//
// Every attachment is persisted as a `ConversationEntry` with `role: "system"`
// and `metadata` carrying one of the variants below. The frontend uses
// `metadata.type` to choose a renderer (chip / thumbnail / file link).
//
// ADR-046 replaces the prior flat `document_upload` metadata and the
// lossy `attached_context` payload. Five types cover every user-attached
// item (file upload, image upload, workspace file, workspace selection,
// workspace folder).

/// Discriminator tag for `AttachmentMeta`. The on-disk value is stable
/// across versions — adding new variants is a forward-compatible change.
pub const ATTACHMENT_TYPE_FILE_UPLOAD: &str = "file_upload";
pub const ATTACHMENT_TYPE_IMAGE_UPLOAD: &str = "image_upload";
pub const ATTACHMENT_TYPE_ATTACHED_FILE: &str = "attached_file";
pub const ATTACHMENT_TYPE_ATTACHED_SELECTION: &str = "attached_selection";
pub const ATTACHMENT_TYPE_ATTACHED_FOLDER: &str = "attached_folder";

/// Discriminated union for `ConversationEntry.metadata` on attachment
/// entries. The serialized form is a flat JSON object with a `"type"`
/// field (e.g. `{"type":"file_upload","document_id":"...","filename":"..."}`)
/// — matched by the frontend's `metadata.type` branch dispatch.
///
/// All variants serialize without a wrapping object so existing readers
/// that ignore `metadata` continue to parse the entry as a benign system
/// note.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentMeta {
    /// User-uploaded document (PDF/DOCX/PPTX/XLSX). Filesystem blob is at
    /// `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>` —
    /// see [`crate::usecases::attachment::on_disk_name`].
    FileUpload(FileUploadMeta),
    /// User-uploaded image (PNG/JPG). Filesystem blob is at
    /// `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>` —
    /// see [`crate::usecases::attachment::on_disk_name`].
    /// `width`/`height` are best-effort
    /// hints supplied by the desktop frontend (which uses `new Image()` to
    /// read real dimensions); a CLI client may omit them and the renderer
    /// falls back to `<img onLoad>` natural sizing.
    ImageUpload(ImageUploadMeta),
    /// User-attached workspace file (read-only reference, not copied).
    AttachedFile(AttachedFileMeta),
    /// User-attached workspace selection with explicit line range.
    AttachedSelection(AttachedSelectionMeta),
    /// User-attached workspace folder. Directory contents are NOT copied;
    /// the LLM is expected to walk the path on demand via its own tools.
    AttachedFolder(AttachedFolderMeta),
}

/// Metadata for a user-uploaded document (PDF/DOCX/PPTX/XLSX).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct FileUploadMeta {
    /// Content hash + random suffix identifying the blob on disk
    /// (`<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>`,
    /// see [`crate::usecases::attachment::on_disk_name`]).
    pub document_id: String,
    pub filename: String,
    /// Lowercase extension without the dot (e.g. "pdf", "docx").
    pub format: String,
    pub size_bytes: u64,
}

/// Metadata for a user-uploaded image (PNG/JPG).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct ImageUploadMeta {
    pub document_id: String,
    pub filename: String,
    pub format: String,
    pub size_bytes: u64,
    /// Optional real pixel width, supplied by clients that can read it
    /// (e.g. desktop frontend via `new Image()`). `None` is allowed — the
    /// renderer falls back to the browser's natural sizing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Optional real pixel height, supplied alongside `width`. Same
    /// fallback rules apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// Metadata for a workspace file attached via "Add to Chat". The path
/// points at the original file on disk — no copy is made.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct AttachedFileMeta {
    pub abs_path: String,
    pub name: String,
}

/// Metadata for a workspace selection attached via "Add to Chat" with
/// an explicit line range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct AttachedSelectionMeta {
    pub abs_path: String,
    pub name: String,
    /// 1-based start line (inclusive).
    pub start_line: u32,
    /// 1-based end line (inclusive).
    pub end_line: u32,
}

/// Metadata for a workspace folder attached via "Add to Chat". Contents
/// are NOT copied — the LLM uses its own tools to enumerate / read files
/// on demand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct AttachedFolderMeta {
    pub abs_path: String,
    pub name: String,
}

/// Commands sent to the background writer thread.
pub enum WriterCommand {
    /// Append a conversation entry to the JSONL file.
    AppendEntry(ConversationEntry),
    /// Append a compaction marker entry synchronously.
    ///
    /// Behaviourally identical to [`WriterCommand::AppendEntry`] for a
    /// `kind == "compaction"` entry, but the writer sends a reply on
    /// `done` only **after** it has seeked, written the entry, and updated
    /// the shared `last_compaction_offset` Arc.
    ///
    /// This synchronous handshake is required so that the caller (typically
    /// `ConversationSession::append_compaction_event`) can guarantee that
    /// the very next `write_meta()` reads a non-stale `last_compaction_offset`
    /// from the shared Arc. Without it, the writer thread is fire-and-forget
    /// and any `write_meta()` racing the writer would observe `None`
    /// and persist a stale meta file, defeating ADR-024's O(1) restore offset.
    ///
    /// Uses `std::sync::mpsc::SyncSender` rather than `tokio::sync::oneshot`
    /// because the writer is a `std::thread` (uses `blocking_recv` on the
    /// tokio mpsc above) and the caller is invoked from inside the async
    /// session-task loop — `tokio::oneshot::blocking_recv` panics if called
    /// from within a tokio runtime context, while a std sync_channel is
    /// safe to block on from any context (async task or not).
    ///
    /// Compaction is rare (only at 80%+ context pressure), so the
    /// synchronous blocking cost is negligible.
    AppendCompactionEntry {
        entry: ConversationEntry,
        done: std::sync::mpsc::SyncSender<()>,
    },
    /// ADR-060 v2 §5.4: synchronous flush — call `file.flush()` and reply on
    /// `done` once all previously-enqueued `AppendEntry` commands have been
    /// physically written to disk. Used by the compact-and-inject path to
    /// guarantee the synthetic `tool_call` / `tool_result` rows appended
    /// after the compaction event are durable before `compact_history_if_needed`
    /// returns, so a process restart immediately after compaction does not
    /// race the writer and lose the round.
    ///
    /// Without this handshake, the writer is fire-and-forget and the test
    /// harness (and any caller that depends on post-compaction durability)
    /// could observe a JSONL file that does not yet contain the injected
    /// round — `restore_history_from_jsonl` would then return just the
    /// compaction marker, and the canonical todo state would be lost.
    ///
    /// As with `AppendCompactionEntry`, `tokio::oneshot` is safe here because
    /// the writer awaits via `blocking_recv` and the caller's `.await` on the
    /// returned receiver hops back to the async runtime context.
    Flush { done: oneshot::Sender<()> },
    /// Flush and shut down the writer.
    Shutdown(oneshot::Sender<()>),
}

/// Background writer that exclusively owns the JSONL file handle.
///
/// ADR-024: the writer no longer manages a metadata header line.
/// Metadata is stored in a separate per-session file (`meta/{session_id}.json`).
pub struct ConversationWriter {
    file: std::fs::File,
    receiver: mpsc::UnboundedReceiver<WriterCommand>,
    /// ADR-024: absolute byte offset of the most recent compaction marker,
    /// shared with `ConversationSession` (the writer updates it on every
    /// compaction write; the session reads it in `build_meta`).
    /// `None` if no compaction has been written during this writer's lifetime.
    last_compaction_offset: Arc<std::sync::Mutex<Option<u64>>>,
    /// ADR-022: Committed line count — incremented after each successful
    /// disk write so `read_messages_since` never sees a count ahead of
    /// the actual file.
    committed_lines: Arc<AtomicUsize>,
    /// Path of the JSONL file (for post-write log-index reconciliation).
    log_path: PathBuf,
    /// Shared line index for this log. The writer advances it after every
    /// successful write and BEFORE incrementing `committed_lines`, so a
    /// reader that observes `committed_lines = N` is guaranteed to find an
    /// index covering at least N lines (ADR-022 ordering invariant).
    log: Arc<Mutex<LogIndex>>,
}

impl ConversationWriter {
    /// Create a new writer.
    fn new(
        file: std::fs::File,
        receiver: mpsc::UnboundedReceiver<WriterCommand>,
        last_compaction_offset: Arc<std::sync::Mutex<Option<u64>>>,
        committed_lines: Arc<AtomicUsize>,
        log_path: PathBuf,
        log: Arc<Mutex<LogIndex>>,
    ) -> Self {
        Self {
            file,
            receiver,
            last_compaction_offset,
            committed_lines,
            log_path,
            log,
        }
    }

    /// §6/§7 fast path: fold the line just written into the log index without
    /// re-reading the file. The writer is the sole appender while a session is
    /// active, so the appended `(offset, len)` always continues the index; the
    /// rare desync (an external writer truncated or rotated the log) falls back
    /// to a full reconcile so the shared index never drifts from disk. Called
    /// before `committed_lines` advances (ADR-022): a reader that sees the new
    /// count can always resolve it against the index.
    fn commit_appended_line(&self, offset: u64, len: u64) {
        let mut idx = match self.log.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        let continues = offset == idx.end && idx.spans.last().is_none_or(|s| s.terminated);
        if continues {
            let data_index = idx.total_data;
            idx.total_data += 1;
            idx.spans.push(LogSpan {
                offset,
                len,
                terminated: true,
                kind: LogLineKind::Data,
                data_index: Some(data_index),
            });
            idx.end = offset + len;
        } else if let Err(e) = reconcile_log(&self.log_path, &mut idx) {
            tracing::warn!("conversation writer: log index reconcile failed: {e}");
        }
    }

    /// Run the writer loop. Blocks until Shutdown is received.
    fn run(mut self) {
        while let Some(cmd) = self.receiver.blocking_recv() {
            match cmd {
                WriterCommand::AppendEntry(entry) => {
                    let is_compaction = entry.kind.as_deref() == Some(ENTRY_KIND_COMPACTION);
                    // Capture absolute offset before writing (seek(End(0))
                    // positions us at the byte where the entry will land).
                    let abs_offset = if is_compaction {
                        match self.file.seek(std::io::SeekFrom::End(0)) {
                            Ok(pos) => Some(pos),
                            Err(e) => {
                                tracing::error!("Failed to seek for compaction entry: {}", e);
                                None
                            }
                        }
                    } else {
                        None
                    };
                    match self.write_entry(&entry, abs_offset.is_some()) {
                        Err(e) => tracing::error!("Failed to write conversation entry: {}", e),
                        Ok((start, len)) => {
                            // ADR-022: advance the log index, then committed_lines,
                            // AFTER the entry is physically written to disk. This
                            // guarantees read_messages_since never sees a line count
                            // ahead of the actual file contents, and the index always
                            // backs a count a reader resolves against it.
                            self.commit_appended_line(start, len);
                            self.committed_lines.fetch_add(1, Ordering::Relaxed);
                            if let Some(abs) = abs_offset {
                                // ADR-024: absolute offset (no header to subtract from).
                                if let Ok(mut guard) = self.last_compaction_offset.lock() {
                                    *guard = Some(abs);
                                }
                                tracing::debug!(abs_offset = abs, "Recorded compaction offset");
                            }
                        }
                    }
                }
                WriterCommand::AppendCompactionEntry { entry, done } => {
                    // Synchronous compaction write: capture the offset,
                    // write the entry, and update the shared Arc — all
                    // before sending the reply on `done`. Callers (see
                    // `ConversationSession::append_compaction_event`) block
                    // on `done` so the very next `write_meta()` is
                    // guaranteed to read a fresh `last_compaction_offset`.
                    let abs_offset = match self.file.seek(std::io::SeekFrom::End(0)) {
                        Ok(pos) => Some(pos),
                        Err(e) => {
                            tracing::error!("Failed to seek for compaction entry: {}", e);
                            None
                        }
                    };
                    match self.write_entry(&entry, abs_offset.is_some()) {
                        Err(e) => tracing::error!("Failed to write compaction entry: {}", e),
                        Ok((start, len)) => {
                            self.commit_appended_line(start, len);
                            self.committed_lines.fetch_add(1, Ordering::Relaxed);
                            if let Some(abs) = abs_offset {
                                if let Ok(mut guard) = self.last_compaction_offset.lock() {
                                    *guard = Some(abs);
                                }
                                tracing::debug!(abs_offset = abs, "Recorded compaction offset (sync)");
                            }
                        }
                    }
                    // Always reply, even on failure, so the caller never
                    // deadlocks waiting for a `done` that never arrives.
                    // The std sync_channel `send` only fails if the
                    // receiver was dropped (caller panicked before
                    // `recv`), which we treat as benign.
                    let _ = done.send(());
                }
                WriterCommand::Shutdown(tx) => {
                    if let Err(e) = self.file.flush() {
                        tracing::error!("Failed to flush conversation file: {}", e);
                    }
                    let _ = tx.send(());
                    break;
                }
                WriterCommand::Flush { done } => {
                    // ADR-060 v2 §5.4: drain any pending AppendEntry commands
                    // first by the simple fact that this arm is reached AFTER
                    // them in FIFO channel order — we are the single consumer
                    // thread. Then `flush()` so the OS commits the writes.
                    if let Err(e) = self.file.flush() {
                        tracing::error!(
                            "Failed to flush conversation file on Flush command: {}",
                            e
                        );
                    }
                    // Always reply, even on failure, so the caller never
                    // deadlocks waiting for a `done` that never arrives.
                    let _ = done.send(());
                }
            }
        }
    }

    /// Write a single entry as a JSON line.
    ///
    /// Builds the complete line in memory first, then issues a single
    /// `write_all` call so the OS can apply atomicity for small writes.
    /// Follows up with `sync_data` to flush to disk.
    ///
    /// If `already_positioned` is `true`, the file cursor is already at the
    /// end (set by the caller who captured the pre-write absolute offset).
    ///
    /// Returns the absolute `(start_offset, byte_len)` of the line actually
    /// written (including the trailing newline) so the caller can fold it into
    /// the log index in O(1) without a read-back.
    fn write_entry(
        &mut self,
        entry: &ConversationEntry,
        already_positioned: bool,
    ) -> std::io::Result<(u64, u64)> {
        if !already_positioned {
            // Seek to end for append; handles resume where file position may be at 0
            self.file.seek(std::io::SeekFrom::End(0))?;
        }
        let start = self.file.stream_position()?;
        // Build the complete line in memory first to ensure atomic write
        let mut line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        let len = line.len() as u64;
        // Single write_all call — OS-level atomicity for small writes
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok((start, len))
    }
}

// ── Log index: request paths never read the whole JSONL ──────────────
//
// Design iron rule: no request / tool-read path may load or count every line
// of a session log. Line counts, pagination windows, and incremental tails
// are served from a small in-memory index per log path. A full byte scan
// happens at most once per session per process (first-touch reconciliation,
// see `calibrate_open`); afterwards the index only reads bytes appended
// since the previous scan.

/// Classification of one physical JSONL line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLineKind {
    /// A conversation entry (possibly corrupt JSON — it still counts as a
    /// line, matching the pre-index pagination `total` semantics).
    Data,
    /// Whitespace-only line — skipped by pagination.
    Blank,
    /// Legacy v1 metadata header on line 0 — skipped by pagination.
    Header,
}

/// Byte extent of one physical line in the log.
#[derive(Debug, Clone, Copy)]
struct LogSpan {
    /// Absolute byte offset of the line's first byte.
    offset: u64,
    /// Byte length including the trailing newline.
    len: u64,
    /// Whether the line ended with a newline on disk.
    terminated: bool,
    kind: LogLineKind,
    /// Position among the `Data` lines (pagination / ADR-050 coordinates).
    data_index: Option<u64>,
}

/// Incremental index over one JSONL log file.
#[derive(Debug, Default)]
struct LogIndex {
    /// Physical lines in file order. `read_messages_since*` numbering matches
    /// `BufRead::lines().enumerate()` exactly — blanks and a final
    /// unterminated line are counted, nothing else.
    spans: Vec<LogSpan>,
    /// Number of `Data` lines: the pagination `total`.
    total_data: u64,
    /// Byte length of the file at the last scan; equal to the file size when
    /// the index is up to date.
    end: u64,
}

/// Process-wide registry of per-log-path indexes.
static LOG_INDEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<LogIndex>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get (or register) the shared index for `path`.
fn log_index_for(path: &Path) -> Arc<Mutex<LogIndex>> {
    let mut map = LOG_INDEXES.lock().unwrap();
    map.entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(LogIndex::default())))
        .clone()
}

/// Drop any scanned state for `path` (fresh / truncated file).
fn reset_log_index(path: &Path) {
    let index = log_index_for(path);
    if let Ok(mut idx) = index.lock() {
        *idx = LogIndex::default();
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.len() >= needle.len()
        && haystack.windows(needle.len()).any(|w| w == needle)
}

fn is_blank_bytes(body: &[u8]) -> bool {
    body.iter().all(u8::is_ascii_whitespace)
}

/// The marker pair legacy readers used to recognise the v1 metadata header
/// line. Only checked against line 0 (a mid-file data line whose *content*
/// happens to mention both markers must not vanish from pagination).
///
/// A genuine data line always serializes a `ConversationEntry`, which carries
/// both `role` and `content`; the v1 metadata header carries neither. Requiring
/// their absence keeps a first message whose body happens to embed the two
/// marker tokens from being misread as a header — and, in
/// `strip_legacy_header_line`, from being physically deleted.
fn looks_like_meta_header(body: &[u8]) -> bool {
    contains_bytes(body, b"\"version\"")
        && contains_bytes(body, b"\"session_id\"")
        && !contains_bytes(body, b"\"role\"")
        && !contains_bytes(body, b"\"content\"")
}

/// Bring `idx` up to date with the bytes currently on disk.
///
/// Cost is O(bytes appended since the last scan): only the new tail is read
/// and no JSON parsing happens beyond line classification. A truncated or
/// rotated file resets the index and forces one full re-scan. A previously
/// unterminated tail is re-scanned because the writer may have completed it.
fn reconcile_log(path: &Path, idx: &mut LogIndex) -> std::io::Result<()> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            *idx = LogIndex::default();
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let file_len = file.metadata()?.len();
    if file_len < idx.end {
        // External truncation / rotation — rebuild from scratch.
        *idx = LogIndex::default();
    }
    let mut scan_from = idx.end;
    if let Some(last) = idx.spans.last().copied()
        && !last.terminated
    {
        // Re-scan the unterminated tail: bytes may have been appended.
        idx.spans.pop();
        if last.kind == LogLineKind::Data {
            idx.total_data -= 1;
        }
        scan_from = last.offset;
    }
    if scan_from == file_len {
        idx.end = file_len;
        return Ok(());
    }
    let mut reader = BufReader::new(file);
    reader.seek(std::io::SeekFrom::Start(scan_from))?;
    let mut pos = scan_from;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let terminated = buf.last() == Some(&b'\n');
        let body = if terminated {
            &buf[..buf.len() - 1]
        } else {
            &buf[..]
        };
        let kind = if is_blank_bytes(body) {
            LogLineKind::Blank
        } else if pos == 0 && looks_like_meta_header(body) {
            LogLineKind::Header
        } else {
            LogLineKind::Data
        };
        let data_index = if kind == LogLineKind::Data {
            let i = idx.total_data;
            idx.total_data += 1;
            Some(i)
        } else {
            None
        };
        idx.spans.push(LogSpan {
            offset: pos,
            len: buf.len() as u64,
            terminated,
            kind,
            data_index,
        });
        pos += buf.len() as u64;
        if !terminated {
            break;
        }
    }
    idx.end = pos;
    Ok(())
}

/// Read the raw text of one indexed span (without the line terminator).
///
/// Returns `None` when the byte range is no longer on disk (a concurrent
/// truncate raced the scan) — callers treat that like the corrupt lines the
/// legacy readers skipped. Invalid UTF-8 is lossily decoded; the subsequent
/// JSON parse then fails and the line is skipped, like before.
fn read_log_span_text(
    file: &mut std::fs::File,
    span: &LogSpan,
) -> std::io::Result<Option<String>> {
    let mut buf = vec![0u8; span.len as usize];
    file.seek(std::io::SeekFrom::Start(span.offset))?;
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    if filled == 0 {
        return Ok(None);
    }
    buf.truncate(filled);
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    Ok(Some(String::from_utf8_lossy(&buf).into_owned()))
}

/// Cold-path physical line count for `read_messages_since`-style coordinates
/// when the writer's `committed_lines` counter is not yet populated.
///
/// Backed by the log index: the first call per session per process performs
/// one full scan (first-touch reconciliation); later calls only read appended
/// bytes. Replaces the old per-request `count_jsonl_lines` full pass.
pub fn physical_line_count(path: &Path) -> usize {
    let index = log_index_for(path);
    let mut idx = match index.lock() {
        Ok(g) => g,
        Err(_) => return 0,
    };
    if reconcile_log(path, &mut idx).is_err() {
        return 0;
    }
    idx.spans.len()
}

/// Open-time calibration (design iron rule 3: the ONE allowed full pass,
/// once per session per process).
///
/// 1. Strip a legacy v1 metadata header line from line 0 (rewrite-once).
/// 2. Full reconcile of the log index.
/// 3. Drop an unterminated tail line left by a crashed writer: the resume
///    path would otherwise append onto it and glue two entries together.
///
/// Returns the physical line count after calibration. MUST be called before
/// the writer opens the file handle: on Windows, renaming over a file with
/// open handles fails.
fn calibrate_open(path: &Path) -> std::io::Result<usize> {
    strip_legacy_header_line(path)?;
    let index = log_index_for(path);
    let mut idx = index.lock().unwrap();
    reconcile_log(path, &mut idx)?;
    if let Some(last) = idx.spans.last().copied()
        && !last.terminated
    {
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_len(last.offset)?;
        idx.spans.pop();
        if last.kind == LogLineKind::Data {
            idx.total_data -= 1;
        }
        idx.end = last.offset;
        tracing::warn!(
            offset = last.offset,
            "conversation: truncated unterminated tail line (crash repair)"
        );
    }
    Ok(idx.spans.len())
}

/// Remove a legacy metadata header line at line 0 (v1 files, or hand-edited
/// JSONL). One-time migration rewrite — the read paths no longer classify
/// every line against the header markers.
fn strip_legacy_header_line(path: &Path) -> std::io::Result<()> {
    let mut file = match std::fs::OpenOptions::new().read(true).write(true).open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let mut first = Vec::new();
    {
        let mut reader = BufReader::new(&mut file);
        reader.read_until(b'\n', &mut first)?;
    }
    let body = if first.last() == Some(&b'\n') {
        &first[..first.len() - 1]
    } else {
        &first[..]
    };
    if first.is_empty() || !looks_like_meta_header(body) {
        return Ok(());
    }
    // Read everything after the header line and rewrite the file without it.
    file.seek(std::io::SeekFrom::Start(first.len() as u64))?;
    let mut rest = Vec::new();
    file.read_to_end(&mut rest)?;
    drop(file);
    let tmp = {
        let mut name = path.as_os_str().to_os_string();
        name.push(".header-strip.tmp");
        PathBuf::from(name)
    };
    std::fs::write(&tmp, &rest)?;
    std::fs::rename(&tmp, path)?;
    reset_log_index(path);
    tracing::info!("conversation: stripped legacy metadata header line at open");
    Ok(())
}

/// Initial configuration for creating a new `ConversationSession`.
///
/// Replaces a long positional parameter list with named fields, making call
/// sites self-documenting and trivial to extend.
pub struct SessionConfig {
    pub agent_id: String,
    pub workspace_id: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
}

/// Manages a single conversation session's JSONL file.
///
/// `ConversationSession` is `Send + Sync` so it can be held by `AgentLoop`
/// in async contexts.
pub struct ConversationSession {
    session_id: String,
    agent_id: String,
    created_at: String,
    /// Whether the session title has been set (first user message).
    title_set: AtomicBool,
    /// Currently persisted title, for deduplicating force-update calls
    /// and serving the `title()` getter without disk I/O.
    current_title: std::sync::Mutex<Option<String>>,
    /// Per-session workspace selection, persisted in meta file.
    /// `None` or `"__agent_home__"` means the agent's home directory.
    /// Wrapped in Mutex for interior mutability so that both file persistence
    /// and in-memory state are updated atomically on the API side.
    workspace_id: std::sync::Mutex<Option<String>>,
    /// Per-session model selection (ADR-012).
    model: std::sync::Mutex<Option<String>>,
    /// Per-session provider selection (ADR-012).
    provider: std::sync::Mutex<Option<String>>,
    /// Per-session account selection (multi-account, ADR-012): which of the
    /// provider's accounts (API keys) this session uses. Persisted in the meta
    /// file; `None` = provider's first account.
    account_id: std::sync::Mutex<Option<String>>,
    /// Per-session reasoning effort override, persisted in meta file.
    reasoning_effort: std::sync::Mutex<Option<String>>,
    /// Per-session temperature override, persisted in meta file.
    temperature: std::sync::Mutex<Option<f32>>,
    /// Per-session context window override (ADR-074), persisted in meta
    /// file. `None` = no override (inherit the per-agent chain).
    context_window: std::sync::Mutex<Option<u64>>,
    /// ADR-076 §决策 4: owner of this session, mirrored from
    /// `SessionMeta.initiator`. Immutable after creation — the only writer
    /// is [`Self::set_initiator`], called by the creation path once.
    initiator: std::sync::Mutex<Option<String>>,
    /// ADR-076 §决策 4: read visibility, mirrored from
    /// `SessionMeta.visibility`. `None` = public (the default).
    visibility: std::sync::Mutex<Option<SessionVisibility>>,
    /// Design 27 §8.2: Chat session binding, mirrored from
    /// `SessionMeta.chat_session_id`. Stamped with `Members` visibility
    /// when a session is created from a `chat:` routing key.
    chat_session_id: std::sync::Mutex<Option<String>>,
    /// ADR-060: todo snapshot mirror (Block C source).
    ///
    /// The single persistence owner is [`ConversationSession`] — the
    /// runtime-side `SessionState.todos` mirrors into this via
    /// [`Self::set_todos`]; no double-writer.
    todos: std::sync::Mutex<Option<Vec<TodoItem>>>,
    /// ADR-027: snapshot + cumulative token counts (raw Provider values).
    /// `None` means no LLM call has been recorded yet.
    tokens: std::sync::Mutex<Option<SessionTokens>>,
    /// Per-model usage breakdown, attributed per LLM call by provider/model.
    /// Mirrors `SessionMeta.model_usage`; folded by
    /// [`Self::apply_model_usage`] alongside [`Self::apply_llm_usage`]
    /// (same zero-skip gate, so Σ detail entries == the `tokens` totals).
    /// The store diffs the persisted map against the previous row inside the
    /// upsert transaction to maintain the agent-level `token_stats` table.
    /// `None` = no attributed call yet.
    model_usage: std::sync::Mutex<Option<std::collections::BTreeMap<String, ModelUsageEntry>>>,
    /// Per-session lifetime LLM-call counter, mirrored from
    /// `SessionMeta.llm_call_counter`. The single mutation owner is
    /// [`Self::bump_llm_call_counter`] (mirroring `accumulate_llm_usage`
    /// for tokens); `None` means no LLM call has been recorded yet.
    llm_call_counter: std::sync::Mutex<Option<u32>>,
    /// Session-level LLM speed / responsiveness accumulators, mirrored from
    /// `SessionMeta.perf`. The single mutation owner is
    /// [`Self::record_llm_round`] (via [`Self::apply_llm_perf`]); `None`
    /// means no call has produced a measurement yet.
    perf: std::sync::Mutex<Option<SessionPerf>>,
    /// Last calibrated session chars/token ratio, mirrored from
    /// `SessionMeta.model_ratio`. Written by [`Self::set_model_ratio`]
    /// after each reliable usage calibration.
    model_ratio: std::sync::Mutex<Option<f64>>,
    /// Running message count, incremented on every `append_message`.
    message_count: AtomicU64,
    /// Last time the meta file was written from `append_message`.
    /// Guards against excessive I/O from rapid `append_message` calls.
    last_meta_write: std::sync::Mutex<Instant>,
    sender: mpsc::UnboundedSender<WriterCommand>,
    /// Path to the JSONL file (for session-level distillation on close).
    session_file_path: PathBuf,
    /// Path to the conversations directory (for meta file writes).
    conversations_dir: PathBuf,
    /// Channel for emitting config-change notifications.
    ///
    /// Config mutators send a `ConfigChange` snapshot through this channel
    /// after `write_meta()`. The relay publishes immediately (no throttle)
    /// to the retained `sessions/{sid}/config` MQTT topic.
    config_change_tx: mpsc::UnboundedSender<ConfigChange>,
    /// Channel for emitting state-change notifications.
    ///
    /// State mutators (message_count, tokens) and `emit_session_state()`
    /// send a `StateChange` snapshot through this channel. The relay
    /// coalesces behind a 3 s cooldown and publishes to the retained
    /// `sessions/{sid}/state` MQTT topic.
    state_change_tx: mpsc::UnboundedSender<StateChange>,
    /// ADR-047: monotonic config version counter.
    ///
    /// Incremented every time `apply_config()` mutates a config field.
    /// `SessionTask` polls this at turn boundaries to detect config
    /// changes that occurred during the previous inference turn and
    /// applies deferred LLM-side effects.
    config_version: AtomicU64,
    /// Cached runtime-only fields sourced from `SessionRuntimeSnapshot`
    /// via `update_runtime_state_cache()`. `ConversationSession` does not
    /// own `SessionRuntimeSnapshot` (it lives on `SessionState`), so
    /// these are cached here whenever `emit_session_state()` runs.
    /// `build_session_state_snapshot()` reads them to produce a complete
    /// `SessionState` proto.
    last_status: std::sync::Mutex<String>,
    last_ratio: std::sync::Mutex<f64>,
    last_context_usage: std::sync::Mutex<String>,
    /// Inbound intents routed into this session while it was busy, kept as the
    /// source of truth for the execution pending-queue projection (ADR-099,
    /// design 27 §5.5.5). Recorded by the routing path on enqueue and cleared
    /// by `SessionTask` when the intent is dequeued to start a turn; projected
    /// into the retained `SessionState` snapshot by
    /// [`Self::build_session_state_snapshot`]. Empty for an idle session.
    pending_intents: std::sync::Mutex<Vec<crate::agent::session_state::PendingIntent>>,
    /// ADR-067: per-section byte sizes from the most recent LLM call.
    ///
    /// Cached as the typed `Vec<ContextUsageSection>` directly — no JSON
    /// round-trip — so [`Self::last_context_usage_sections`] can hand the
    /// list straight to `emit_session_state` for merging into the
    /// persisted-token-derived `ContextUsageInfo`. `None` until the first
    /// LLM call populates it via [`Self::cache_context_usage_sections`].
    last_context_usage_sections: std::sync::Mutex<Option<Vec<ContextUsageSection>>>,
    /// ADR-024: absolute byte offset of the most recent compaction marker
    /// in the JSONL. Shared with `ConversationWriter` (the writer updates
    /// it synchronously on every compaction write via
    /// `WriterCommand::AppendCompactionEntry`). `build_meta` reads it for
    /// persistence to `meta.json`. On `resume()` this is initialized from
    /// `meta.last_compaction_offset` so the next restore can use O(1)
    /// offset skip instead of falling back to the O(N) rposition scan.
    last_compaction_offset: Arc<std::sync::Mutex<Option<u64>>>,
}

impl std::fmt::Debug for ConversationSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConversationSession")
            .field("session_id", &self.session_id)
            .field("agent_id", &self.agent_id)
            .field("config_version", &self.config_version)
            .finish()
    }
}

/// Config-change notification carrying the full proto snapshot.
///
/// ADR-043: replaces the config portion of the deleted `MetaChangeKind`.
/// Always published immediately by the relay (no throttle) - config
/// changes are low-frequency user actions.
#[derive(Debug, Clone)]
pub struct ConfigChange {
    /// The snapshot built by the mutator on the ORIGINAL ConversationSession.
    pub snapshot: acowork_core::mqtt_proto::SessionConfig,
}

/// State-change notification carrying the full proto snapshot.
///
/// ADR-043: replaces the runtime portion of the deleted `MetaChangeKind`
/// and the deleted `ChunkEvent::SessionStateChanged`. Coalesced by the
/// relay behind a 3 s cooldown - state changes are high-frequency
/// telemetry (message_count, tokens, status).
#[derive(Debug, Clone)]
pub struct StateChange {
    /// The snapshot built by the mutator on the ORIGINAL ConversationSession.
    pub snapshot: acowork_core::mqtt_proto::SessionState,
}

/// Per-call attribution for the per-model usage detail (`SessionMeta.model_usage`).
///
/// Folded into the in-memory detail map by [`ConversationSession::apply_model_usage`]
/// on every persisting usage call; `SqliteSessionMetaStore` then diffs the
/// persisted map against the row's previous value inside the upsert
/// transaction to maintain the agent-level `token_stats` aggregate.
/// Missing segments fold to `"unknown"` via [`model_usage_key`].
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelUsageAttribution<'a> {
    /// Provider id this call went to (`None` → `"unknown"`).
    pub provider_id: Option<&'a str>,
    /// Model id that served this call (`None` → `"unknown"`).
    pub model_id: Option<&'a str>,
    /// Estimated cost of this call in USD (`None` = price unknown —
    /// folds as "no cost known", never as free).
    pub cost_delta: Option<f64>,
}

/// Estimate one LLM call's cost from the model's price table and raw usage.
///
/// Cache tokens are deliberately NOT billed: `ModelCostInfo` only carries
/// input/output per-million prices (no cache unit prices exist), so cache
/// read/write contribute nothing to the estimate. Returns `None` when any
/// price is missing — "宁可 miss 也不估计".
pub fn estimate_call_cost(cost: Option<&ModelCostInfo>, usage: &UsageInfo) -> Option<f64> {
    let cost = cost?;
    let input = cost.input_per_million?;
    let output = cost.output_per_million?;
    // Mirror the ADR-027 zero-skip gate from `apply_llm_usage`: a provider
    // fallback reporting `prompt_tokens == 0` did not bill input either.
    let billed_input = if usage.prompt_tokens > 0 {
        usage.prompt_tokens
    } else {
        0
    };
    Some(
        billed_input as f64 / 1_000_000.0 * input
            + usage.completion_tokens as f64 / 1_000_000.0 * output,
    )
}

/// Minimum interval between meta file writes triggered by `append_message`.
///
/// Metadata-only mutations (title, model, provider, workspace) always write
/// immediately.  Only the high-frequency `append_message` path respects this
/// cooldown, capping meta write I/O regardless of conversation speed.
const META_WRITE_COOLDOWN_MS: u64 = 3000;

impl ConversationSession {
    /// Build a complete `SessionMeta` from the current in-memory state.
    fn build_meta(&self) -> SessionMeta {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let tokens = self.tokens.lock().ok().and_then(|t| t.clone());
        let llm_call_counter = self.llm_call_counter.lock().ok().and_then(|c| *c);
        // ADR-024: read the absolute byte offset of the most recent
        // compaction marker from the shared Arc. The writer updates this
        // synchronously via `WriterCommand::AppendCompactionEntry`, so the
        // value is fresh as long as compaction writes are awaited
        // (see `append_compaction_event`).
        let last_compaction_offset = self.last_compaction_offset.lock().ok().and_then(|g| *g);
        SessionMeta {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            created_at: self.created_at.clone(),
            initiator: self.initiator.lock().ok().and_then(|u| u.clone()),
            visibility: self.visibility.lock().ok().and_then(|v| *v),
            chat_session_id: self.chat_session_id.lock().ok().and_then(|c| c.clone()),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            account_id: self.account_id.lock().ok().and_then(|a| a.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            context_window: self.context_window.lock().ok().and_then(|c| *c),
            todos: self.todos.lock().ok().and_then(|t| t.clone()),
            message_count: self.message_count.load(Ordering::Relaxed),
            last_active_at: now,
            tokens,
            model_usage: self
                .model_usage
                .lock()
                .ok()
                .and_then(|m| m.clone())
                .filter(|m| !m.is_empty()),
            llm_call_counter,
            perf: self.perf.lock().ok().and_then(|p| p.clone()),
            model_ratio: self.model_ratio.lock().ok().and_then(|r| *r),
            last_compaction_offset,
            corrupted: false,
        }
    }

    /// Build a `SessionConfig` proto snapshot from the current in-memory state.
    ///
    /// ADR-043: carries only user-configurable fields (title, provider,
    /// model, reasoning_effort, temperature, workspace_id). Runtime
    /// telemetry is in `build_session_state_snapshot()`.
    ///
    /// Differs from the disk-side `SessionMeta` (built by `build_meta`) by:
    /// - flattening `Option<String>` to `""` for empty values
    /// - flattening `Option<f32>` (temperature) to `f32::NAN` for the
    ///   "no override" sentinel (prost encodes `float` without an Option;
    ///   NaN lets the client distinguish "missing" from "0.0")
    /// - dropping disk-only fields (version, created_at, last_compaction_offset,
    ///   corrupted)
    pub fn build_session_config_snapshot(
        &self,
        llm_availability: acowork_core::mqtt_proto::LlmAvailability,
    ) -> acowork_core::mqtt_proto::SessionConfig {
        let full = self.build_meta();
        acowork_core::mqtt_proto::SessionConfig {
            agent_id: full.agent_id,
            session_id: full.session_id,
            title: full.title.unwrap_or_default(),
            provider_id: full.provider.unwrap_or_default(),
            model_id: full.model.unwrap_or_default(),
            reasoning_effort: full.reasoning_effort.unwrap_or_default(),
            temperature: full.temperature.unwrap_or(f32::NAN),
            workspace_id: full.workspace_id.unwrap_or_default(),
            llm_availability: llm_availability as i32,
            context_window: full.context_window,
            account_id: full.account_id.unwrap_or_default(),
        }
    }

    /// Build a `SessionState` proto snapshot from the current in-memory state.
    ///
    /// ADR-043: carries only runtime telemetry (status, message_count,
    /// tokens, ratio, context_usage, updated_at). The runtime-only fields
    /// (status, ratio, context_usage) are read from the cached
    /// values updated by `update_runtime_state_cache()`.
    pub fn build_session_state_snapshot(&self) -> acowork_core::mqtt_proto::SessionState {
        let full = self.build_meta();
        let tokens = full.tokens.clone();
        let status = self
            .last_status
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        let ratio = self
            .last_ratio
            .lock()
            .map(|r| *r)
            .unwrap_or(crate::token::counter::DEFAULT_RATIO);
        let context_usage = self
            .last_context_usage
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        acowork_core::mqtt_proto::SessionState {
            agent_id: full.agent_id,
            session_id: full.session_id,
            status,
            message_count: full.message_count,
            input_tokens: tokens.as_ref().map(|t| t.last_input).unwrap_or(0),
            output_tokens: tokens.as_ref().map(|t| t.last_output).unwrap_or(0),
            total_input_tokens: tokens.as_ref().map(|t| t.total_input).unwrap_or(0),
            total_output_tokens: tokens.as_ref().map(|t| t.total_output).unwrap_or(0),
            ratio,
            context_usage,
            updated_at: full.last_active_at,
            pending_intents: self
                .pending_intents()
                .into_iter()
                .map(|p| acowork_core::mqtt_proto::PendingIntent {
                    message_id: p.message_id,
                    from: p.from,
                    preview: p.preview,
                    from_chat: p.from_chat,
                })
                .collect(),
        }
    }

    /// Snapshot the current pending-intent queue (ADR-099, design 27 §5.5.5).
    pub fn pending_intents(&self) -> Vec<crate::agent::session_state::PendingIntent> {
        self.pending_intents
            .lock()
            .ok()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// Record an intent routed into this session while it was busy. Dedups on
    /// `message_id` so a redelivered trigger never shows twice. Does not push
    /// the state change — the caller pairs it with
    /// [`Self::notify_state_change`] once.
    pub fn push_pending_intent(&self, intent: crate::agent::session_state::PendingIntent) {
        if let Ok(mut q) = self.pending_intents.lock()
            && !q.iter().any(|p| p.message_id == intent.message_id)
        {
            q.push(intent);
        }
    }

    /// Drop a pending-intent row once the session dequeues it to start a turn.
    /// Removes by `message_id`; a no-op when the id is not queued (e.g. the
    /// intent reached an idle session and was never recorded).
    pub fn remove_pending_intent(&self, message_id: &str) {
        if let Ok(mut q) = self.pending_intents.lock() {
            let before = q.len();
            q.retain(|p| p.message_id != message_id);
            // Only worth a push if a row actually left the queue.
            if q.len() != before {
                drop(q);
                self.notify_state_change();
            }
        }
    }

    /// Cache runtime-only fields from `SessionRuntimeSnapshot`.
    ///
    /// Called by `AgentLoopSession::emit_session_state()` after it updates
    /// the `SessionRuntimeSnapshot`. This allows `build_session_state_snapshot()`
    /// to produce a complete `SessionState` proto without needing the caller
    /// to pass in the runtime fields.
    pub fn update_runtime_state_cache(&self, status: &str, ratio: f64, context_usage: &str) {
        if let Ok(mut s) = self.last_status.lock() {
            *s = status.to_string();
        }
        if let Ok(mut r) = self.last_ratio.lock() {
            *r = ratio;
        }
        if let Ok(mut c) = self.last_context_usage.lock() {
            *c = context_usage.to_string();
        }
    }

    /// Cache per-section byte sizes from the most recent LLM call.
    ///
    /// ADR-067: called by `process_llm_response_usage` right after it
    /// computes `sections`, so the sections survive into the retained
    /// `session_state` snapshot. [`Self::update_runtime_state_cache`] is
    /// called by `emit_session_state` with a persisted-token-derived
    /// `ContextUsageInfo` that has no `ContextBuilder` and therefore no
    /// `sections` — that path re-merges the sections cached here.
    ///
    /// Stores the typed `Vec<ContextUsageSection>` directly so callers can
    /// recover the sections without paying the JSON serialize/deserialize
    /// round-trip on every LLM call.
    pub fn cache_context_usage_sections(&self, sections: Vec<ContextUsageSection>) {
        if let Ok(mut c) = self.last_context_usage_sections.lock() {
            *c = Some(sections);
        }
    }

    /// Read the most recently cached per-section byte sizes.
    ///
    /// ADR-067: used by `emit_session_state` to merge sections into the
    /// persisted-token-derived `ContextUsageInfo`. Returns `None` until
    /// the first LLM call has populated the cache.
    pub fn last_context_usage_sections(&self) -> Option<Vec<ContextUsageSection>> {
        self.last_context_usage_sections
            .lock()
            .ok()
            .and_then(|s| s.clone())
    }

    /// Notify the config relay of a config field change.
    ///
    /// Builds a `SessionConfig` snapshot and sends it through
    /// `config_change_tx`. Called by config mutators after `write_meta()`.
    pub fn notify_config_change(&self) {
        let _ = self.config_change_tx.send(ConfigChange {
            snapshot: self.build_session_config_snapshot(
                acowork_core::mqtt_proto::LlmAvailability::Unspecified,
            ),
        });
    }

    /// Notify the state relay of a runtime state change.
    ///
    /// Builds a `SessionState` snapshot and sends it through
    /// `state_change_tx`. Called by state mutators (message_count, tokens)
    /// and by `emit_session_state()` (status, ratio, context_usage).
    pub fn notify_state_change(&self) {
        let _ = self.state_change_tx.send(StateChange {
            snapshot: self.build_session_state_snapshot(),
        });
    }

    /// Write the current in-memory state to the per-session meta file.
    ///
    /// Also updates `last_meta_write` timestamp for cooldown tracking.
    ///
    /// # Deleted-session guard
    ///
    /// If the session's JSONL file no longer exists, the session has been
    /// deleted (or pruned) and the meta file must NOT be re-created. Without
    /// this guard, a detached tail-distillation task holding the last
    /// `Arc<ConversationSession>` would re-write the meta file *after*
    /// `SessionManager::delete_session` removed it, leaving an orphaned meta
    /// entry that the next startup scan treats as the "latest" session —
    /// which then fails to resume because its JSONL is gone.
    fn write_meta(&self) {
        if !self.session_file_path.exists() {
            return;
        }
        let meta = self.build_meta();
        if let Err(e) = write_session_meta(&self.conversations_dir, &meta) {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "Failed to write session meta file"
            );
        }
        if let Ok(mut last) = self.last_meta_write.lock() {
            *last = Instant::now();
        }
    }

    /// Create a new session with optional initial metadata.
    ///
    /// Creates a pure JSONL file (no metadata header — see ADR-024) and
    /// writes the initial session metadata through the active meta store
    /// (ADR-082 §4 step 3: SQLite).
    ///
    /// Returns `(session, meta_change_rx)` where `meta_change_rx` is the
    /// receiver side of the meta-change notification channel — the caller
    /// (typically the session_task) consumes it to forward
    /// `ChunkEvent::SessionMetaChanged` events to MQTT. If the caller does
    /// not need meta-change notifications, drop the receiver.
    pub fn new(
        work_dir: &Path,
        session_id: &str,
        config: SessionConfig,
        max_sessions: usize,
        committed_lines: Arc<AtomicUsize>,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<ConfigChange>,
        mpsc::UnboundedReceiver<StateChange>,
    )> {
        let conversations_dir = work_dir.join("conversations");
        std::fs::create_dir_all(&conversations_dir)?;

        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(&file_path)?;

        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        // ADR-024: shared `last_compaction_offset` between writer and
        // session. Brand-new session starts with `None` — no compaction
        // has occurred yet.
        let last_compaction_offset = Arc::new(std::sync::Mutex::new(None));

        // The truncate above emptied the file; drop any stale index a
        // previous holder of this path (e.g. an older session with the same
        // id) registered.
        reset_log_index(&file_path);
        let log = log_index_for(&file_path);

        // ADR-024: no JSONL header — file starts at line 0.
        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(
            file,
            rx,
            last_compaction_offset.clone(),
            committed_lines,
            file_path.clone(),
            log,
        );
        std::thread::spawn(move || writer.run());

        // Meta-change notification channel. The receiver is consumed by the
        // session_task's relay task (see `session_task.rs::spawn_meta_relay`).
        // We create it here so callers of `ConversationSession::new` can
        // extract it before handing ownership to the session_task.
        // Config + state change notification channels (ADR-043).
        // The receivers are consumed by the session_task's relay tasks
        // (see `subsystems.rs::spawn_config_change_relay` and
        // `spawn_state_change_relay`). We create them here so callers of
        // `ConversationSession::new` can extract them before handing
        // ownership to the session_task.
        let (config_tx, config_rx) = mpsc::unbounded_channel::<ConfigChange>();
        let (state_tx, state_rx) = mpsc::unbounded_channel::<StateChange>();

        let session = Self {
            session_id: session_id.to_string(),
            agent_id: config.agent_id,
            created_at: now.clone(),
            title_set: AtomicBool::new(false),
            current_title: std::sync::Mutex::new(None),
            workspace_id: std::sync::Mutex::new(config.workspace_id),
            model: std::sync::Mutex::new(config.model),
            provider: std::sync::Mutex::new(config.provider),
            account_id: std::sync::Mutex::new(None),
            reasoning_effort: std::sync::Mutex::new(None),
            temperature: std::sync::Mutex::new(None),
            context_window: std::sync::Mutex::new(None),
            todos: std::sync::Mutex::new(None),
            initiator: std::sync::Mutex::new(None),
            visibility: std::sync::Mutex::new(None),
            chat_session_id: std::sync::Mutex::new(None),
            tokens: std::sync::Mutex::new(None),
            model_usage: std::sync::Mutex::new(None),
            llm_call_counter: std::sync::Mutex::new(None),
            perf: std::sync::Mutex::new(None),
            model_ratio: std::sync::Mutex::new(None),
            message_count: AtomicU64::new(0),
            last_meta_write: std::sync::Mutex::new(Instant::now()),
            sender: tx,
            session_file_path: file_path,
            conversations_dir: conversations_dir.clone(),
            config_change_tx: config_tx,
            state_change_tx: state_tx,
            config_version: AtomicU64::new(0),
            last_status: std::sync::Mutex::new(String::new()),
            last_ratio: std::sync::Mutex::new(crate::token::counter::DEFAULT_RATIO),
            last_context_usage: std::sync::Mutex::new(String::new()),
            pending_intents: std::sync::Mutex::new(Vec::new()),
            last_context_usage_sections: std::sync::Mutex::new(None),
            last_compaction_offset,
        };

        // ADR-082 §4 step 3: write the initial meta row through the
        // active store (SQLite). Replaces the legacy per-session JSON file.
        session.write_meta();

        // Enforce max-sessions limit: prune the oldest sessions if the
        // limit now exceeds the configured threshold.
        if max_sessions > 0 {
            prune_excess_sessions(&conversations_dir, max_sessions);
        }

        Ok((session, config_rx, state_rx))
    }

    /// Resume an existing session.
    ///
    /// Opens the existing JSONL file in append mode, reads metadata from the
    /// active meta store (ADR-082 §4 step 3: SQLite), and starts the
    /// background writer thread.
    ///
    /// Returns `(session, meta_change_rx)` — see [`Self::new`] for the
    /// semantics of the meta-change receiver.
    pub fn resume(
        work_dir: &Path,
        session_id: &str,
        committed_lines: Arc<AtomicUsize>,
    ) -> Result<(
        Self,
        mpsc::UnboundedReceiver<ConfigChange>,
        mpsc::UnboundedReceiver<StateChange>,
    )> {
        let conversations_dir = work_dir.join("conversations");
        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));

        // §9 open calibration: the one allowed full pass per session per
        // process — reconciles the log index, strips a legacy v1 header
        // line, and truncates an unterminated tail left by a crash. It
        // runs BEFORE the writer takes the file handle because on Windows
        // the header-strip rename cannot replace a file with open handles.
        //
        // Initialize committed_lines to the actual number of existing lines
        // in the JSONL file. Without this, the counter stays at 0 (the
        // value passed by callers like `SessionManager::open`), which causes
        // the delivery cursor (ADR-025) to be reset to {0, 0} during
        // full-load — and the first incremental poll re-delivers all
        // historical messages.
        // A calibration failure is a hard open error, not a silent 0: swallowing
        // it would (a) skip the crash-tail repair, letting the next append glue
        // onto a torn line, and (b) reset the ADR-025 delivery cursor to {0, 0},
        // redelivering all history on the first incremental poll. `resume`
        // already returns `Result`, so fail the open rather than mis-initialize.
        let existing_lines = calibrate_open(&file_path)?;
        committed_lines.store(existing_lines, Ordering::Relaxed);

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&file_path)?;

        // ADR-024: read metadata from per-session meta file.
        let meta = read_session_meta(&conversations_dir, session_id).map_err(|e| {
            std::io::Error::new(e.kind(), format!("Failed to read session meta: {}", e))
        })?;

        // ADR-024: restore the absolute byte offset of the most recent
        // compaction marker from meta, so the next restore (or any code
        // path that reads `build_meta()` immediately) sees the persisted
        // value instead of starting from `None`.
        let last_compaction_offset = Arc::new(std::sync::Mutex::new(meta.last_compaction_offset));

        let (tx, rx) = mpsc::unbounded_channel::<WriterCommand>();
        let writer = ConversationWriter::new(
            file,
            rx,
            last_compaction_offset.clone(),
            committed_lines,
            file_path.clone(),
            log_index_for(&file_path),
        );
        std::thread::spawn(move || writer.run());

        let (config_tx, config_rx) = mpsc::unbounded_channel::<ConfigChange>();
        let (state_tx, state_rx) = mpsc::unbounded_channel::<StateChange>();

        Ok((
            Self {
                session_id: session_id.to_string(),
                agent_id: meta.agent_id,
                created_at: meta.created_at,
                title_set: AtomicBool::new(meta.title.is_some()),
                current_title: std::sync::Mutex::new(meta.title.clone()),
                workspace_id: std::sync::Mutex::new(meta.workspace_id),
                model: std::sync::Mutex::new(meta.model),
                provider: std::sync::Mutex::new(meta.provider),
                account_id: std::sync::Mutex::new(meta.account_id),
                reasoning_effort: std::sync::Mutex::new(meta.reasoning_effort),
                temperature: std::sync::Mutex::new(meta.temperature),
                context_window: std::sync::Mutex::new(meta.context_window),
                todos: std::sync::Mutex::new(meta.todos),
                initiator: std::sync::Mutex::new(meta.initiator),
                visibility: std::sync::Mutex::new(meta.visibility),
                chat_session_id: std::sync::Mutex::new(meta.chat_session_id.clone()),
                tokens: std::sync::Mutex::new(meta.tokens.clone()),
                model_usage: std::sync::Mutex::new(meta.model_usage.clone()),
                llm_call_counter: std::sync::Mutex::new(meta.llm_call_counter),
                perf: std::sync::Mutex::new(meta.perf.clone()),
                model_ratio: std::sync::Mutex::new(meta.model_ratio),
                message_count: AtomicU64::new(meta.message_count),
                last_meta_write: std::sync::Mutex::new(Instant::now()),
                sender: tx,
                session_file_path: file_path,
                conversations_dir,
                config_change_tx: config_tx,
                state_change_tx: state_tx,
                config_version: AtomicU64::new(0),
                last_status: std::sync::Mutex::new(String::new()),
                last_ratio: std::sync::Mutex::new(crate::token::counter::DEFAULT_RATIO),
                last_context_usage: std::sync::Mutex::new(String::new()),
                pending_intents: std::sync::Mutex::new(Vec::new()),
                last_context_usage_sections: std::sync::Mutex::new(None),
                last_compaction_offset,
            },
            config_rx,
            state_rx,
        ))
    }

    /// Append a message to the conversation.
    ///
    /// This is non-blocking: the message is sent via channel to the
    /// background writer thread.
    pub fn append_message(&self, role: &str, content: &str, metadata: Option<serde_json::Value>) {
        self.append_message_with_id(role, content, metadata, None);
    }

    /// Append an *internal* runtime message (e.g. the output-budget nudge).
    ///
    /// Stored in JSONL with `metadata.internal = true` so readers recognize it
    /// via [`ConversationEntry::is_internal`]. It is deliberately **not**
    /// broadcast over MQTT (only [`SessionCore::flush_streaming_line`] emits
    /// `ChunkEvent::RecordComplete`), so the live UI never sees it.
    pub fn append_internal_message(&self, role: &str, content: &str) {
        let mut meta = serde_json::Map::new();
        meta.insert(META_KEY_INTERNAL.to_string(), serde_json::Value::Bool(true));
        self.append_message(role, content, Some(serde_json::Value::Object(meta)));
    }

    /// Append a message with an explicit ID.
    ///
    /// When `id` is `Some`, the entry is stored with that ID instead of
    /// generating a new UUID. This is used for user messages where the
    /// frontend generates a deterministic ID (`msg-{uuid}`) and sends it
    /// via `message_id` — the backend stores it as-is so the frontend
    /// can deduplicate by ID when polling session messages.
    pub fn append_message_with_id(
        &self,
        role: &str,
        content: &str,
        metadata: Option<serde_json::Value>,
        id: Option<String>,
    ) {
        let entry = ConversationEntry {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: role.to_string(),
            content: content.to_string(),
            metadata,
            kind: None,
        };
        if let Err(e) = self.sender.send(WriterCommand::AppendEntry(entry)) {
            tracing::error!("Failed to send message to conversation writer: {}", e);
        }
        // Update message count and write meta so the frontend sees the latest
        // active-at timestamp without a directory scan.
        self.message_count.fetch_add(1, Ordering::Relaxed);
        // ADR-024: throttle meta writes on the high-frequency append path.
        // ADR-043: Always send a state change notification even when the meta
        // file write is skipped, so the relay always has the latest in-memory
        // snapshot. This ensures that a workspace_id / reasoning_effort /
        // temperature change made during streaming (via update_workspace_id /
        // update_reasoning_effort / update_temperature) is reflected in the
        // next MQTT publish even if the caller's write_meta() reset the
        // cooldown timer, without having to wait for the cooldown to fully
        // elapse.
        if let Ok(last) = self.last_meta_write.lock()
            && last.elapsed().as_millis() < META_WRITE_COOLDOWN_MS as u128
        {
            // Meta file write skipped (cooldown active), but still notify
            // the relay with the latest in-memory snapshot.
            self.notify_state_change();
            return;
        }
        self.write_meta();
        // Hot field — the relay task coalesces these behind a 3 s cooldown
        // (same cadence as the meta write itself, so we never publish more
        // often than we persist).
        self.notify_state_change();
    }

    /// Append a compaction event to the JSONL.
    ///
    /// Used by [`AgentLoop::compact_history_if_needed`] after a successful
    /// LLM-driven compaction to mark the boundary between compacted and
    /// surviving messages. The session restorer uses the most recent such
    /// event to determine the replay window.
    ///
    /// Synchronous: blocks until the writer has seeked, written the entry,
    /// and updated the shared `last_compaction_offset` Arc. Compaction is
    /// rare (only at 80%+ context pressure), so the blocking cost is
    /// negligible, but the guarantee is required so the very next
    /// `write_meta()` reads a fresh `last_compaction_offset` instead of
    /// racing the writer thread (which would otherwise persist `None` and
    /// force the next restore to fall back to the O(N) rposition scan).
    ///
    /// The entry's `role` is set to `"system"` so legacy v1 readers (and any
    /// frontend that ignores `kind`) treat it as a benign system note.
    pub fn append_compaction_event(&self, summary: &str, meta: CompactionEventMeta) {
        let metadata_value = serde_json::to_value(&meta).ok();
        let entry = ConversationEntry {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            role: "system".to_string(),
            content: summary.to_string(),
            metadata: metadata_value,
            kind: Some(ENTRY_KIND_COMPACTION.to_string()),
        };
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel::<()>(0);
        if let Err(e) = self.sender.send(WriterCommand::AppendCompactionEntry {
            entry,
            done: done_tx,
        }) {
            tracing::error!(
                "Failed to send compaction event to conversation writer: {}",
                e
            );
            return;
        }
        // Block until the writer has flushed the entry + updated the shared
        // `last_compaction_offset`. The writer always sends on `done` (even
        // on write failure) so this cannot deadlock under normal conditions.
        // A std `sync_channel(0)` `recv` is safe to block on from any context
        // — including inside an async tokio task — unlike
        // `tokio::sync::oneshot::blocking_recv` which panics when invoked
        // from within a tokio runtime.
        match done_rx.recv() {
            Ok(()) => {}
            Err(e) => tracing::error!(
                "Compaction writer dropped its done channel before reply: {}",
                e
            ),
        }

        // ADR-024 / ADR-060 v2 §5.4: persist the freshly-recorded
        // `last_compaction_offset` to meta.json **now**. The writer has
        // already updated the shared Arc (synchronously, via the handshake
        // above); the next `write_meta()` on the high-frequency append path
        // (`append_message_with_id`) may be suppressed by the 3 s cooldown,
        // so relying on it would leave meta.json with `None` and force the
        // next restore to fall back to the O(N) rposition scan — or worse,
        // lose the injected todo round entirely. Compaction is rare, so the
        // extra meta write here is negligible.
        self.write_meta();
    }

    /// ADR-060 v2 §5.4: synchronous flush — block until all previously
    /// enqueued `AppendEntry` commands (including any `tool_call` /
    /// `tool_result` rows appended via `append_message_with_id`) have been
    /// physically written to the JSONL file.
    ///
    /// The compact-and-inject path calls this AFTER
    /// `append_compaction_event` to guarantee the synthetic rows
    /// appended for the injected todo_write round are durable on disk
    /// before `compact_history_if_needed` returns. Without it, a process
    /// restart immediately after compaction would race the writer thread
    /// and the restorer would lose the round (it lands before the
    /// compaction marker in the JSONL).
    pub async fn flush_pending(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel::<()>();
        if let Err(e) = self.sender.send(WriterCommand::Flush { done: tx }) {
            return Err(crate::error::RuntimeError::Io(std::io::Error::other(
                format!("flush send failed: {e}"),
            )));
        }
        let _ = rx.await;
        Ok(())
    }

    /// Return the session ID.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Return the agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Return the current persisted session title, if any.
    pub fn title(&self) -> Option<String> {
        self.current_title.lock().ok().and_then(|t| t.clone())
    }

    /// Return the path to the JSONL session file.
    ///
    /// Used by session-level episode distillation on close.
    pub fn session_path(&self) -> &Path {
        &self.session_file_path
    }

    /// Close the session.
    ///
    /// Sends a Shutdown command to the writer thread and waits for
    /// it to flush and finish.
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel::<()>();
        if let Err(e) = self.sender.send(WriterCommand::Shutdown(tx)) {
            tracing::error!("Failed to send shutdown to conversation writer: {}", e);
            return Err(crate::error::RuntimeError::Io(std::io::Error::other(
                format!("shutdown send failed: {}", e),
            )));
        }
        let _ = rx.await;
        Ok(())
    }

    /// Set the session title from the first user message.
    ///
    /// Truncates via [`crate::prompt::truncate_title_for_display`] (which
    /// prefers natural break points over a blind cut). Only sets the title
    /// once — subsequent calls are no-ops.
    pub fn set_title(&self, content: &str) {
        if self.title_set.swap(true, Ordering::Relaxed) {
            return;
        }
        let title = crate::prompt::truncate_title_for_display(content);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(title);
        }
        // ADR-024: write entire meta file instead of rewrite_metadata + update_index_entry
        self.write_meta();
        self.notify_config_change();
        tracing::info!(session_id = %self.session_id, "Session title set");
    }

    /// Force-update the session title (used by API, not first-message auto-set).
    ///
    /// Unlike `set_title`, this always writes the title even if one was
    /// already set. Used by the `update_session_title` action from Gateway.
    /// Returns `true` if the title was actually written (was different from current).
    pub fn update_title_force(&self, title: &str) -> bool {
        // No-op if the title hasn't changed
        if let Ok(current) = self.current_title.lock()
            && current.as_deref() == Some(title)
        {
            return false;
        }
        let truncated = crate::prompt::truncate_title_for_display(title);
        self.title_set.store(true, Ordering::Relaxed);
        // Track current title for dedup
        if let Ok(mut current) = self.current_title.lock() {
            *current = Some(truncated.clone());
        }
        // ADR-024: write entire meta file instead of rewrite_metadata + update_index_entry
        self.write_meta();
        self.notify_config_change();
        tracing::info!(session_id = %self.session_id, title = %truncated, "Session title force-updated via API");
        true
    }

    /// Persist the per-session workspace selection to the meta file.
    ///
    /// The authoritative workspace_id is stored in [`SessionCore`];
    /// this method only persists to disk.
    pub fn update_workspace_id(&self, workspace_id: &str) {
        // Update in-memory state FIRST so that subsequent metadata updates
        // (e.g. set_title via first user message) don't lose workspace_id.
        if let Ok(mut w) = self.workspace_id.lock() {
            *w = Some(workspace_id.to_string());
        }
        self.write_meta();
        self.notify_config_change();
        tracing::info!(
            session_id = %self.session_id,
            workspace_id = %workspace_id,
            "Session workspace_id persisted to meta file + config_change_tx notified"
        );
    }

    /// Return the persisted workspace_id, if any.
    pub fn workspace_id(&self) -> Option<String> {
        self.workspace_id.lock().ok().and_then(|w| w.clone())
    }

    /// Return the persisted model, if any (ADR-012).
    pub fn model(&self) -> Option<String> {
        self.model.lock().ok().and_then(|m| m.clone())
    }

    /// Return the persisted provider, if any (ADR-012).
    pub fn provider(&self) -> Option<String> {
        self.provider.lock().ok().and_then(|p| p.clone())
    }

    /// Return the persisted account selection, if any (multi-account, ADR-012).
    pub fn account_id(&self) -> Option<String> {
        self.account_id.lock().ok().and_then(|a| a.clone())
    }

    /// Persist the per-session model and provider selection to meta file (ADR-012).
    ///
    /// Does NOT mutate the in-memory `SessionState` — the caller is
    /// responsible for keeping the two in sync.
    pub fn update_model_provider(&self, model: &str, provider: Option<&str>) {
        // Update in-memory state FIRST so that subsequent metadata updates
        // (e.g. set_title via first user message) don't lose model/provider.
        if let Ok(mut m) = self.model.lock() {
            *m = Some(model.to_string());
        }
        if let Ok(mut p) = self.provider.lock() {
            *p = provider.map(|s| s.to_string());
        }
        self.write_meta();
        self.notify_config_change();
        tracing::info!(
            session_id = %self.session_id,
            model = %model,
            provider = ?provider,
            "Session model/provider persisted to meta file + config_change_tx notified"
        );
    }

    /// Persist the per-session multi-account `account_id` pick to the
    /// meta file. Mirrors [`update_model_provider`] for the multi-account
    /// dimension: updates the in-memory field, writes the meta file, and
    /// notifies the config-change channel so MQTT re-publishes the
    /// session_config envelope (front-end `providerAccountId` selector).
    pub fn update_account_id(&self, account_id: &str) {
        if let Ok(mut a) = self.account_id.lock() {
            *a = Some(account_id.to_string());
        }
        self.write_meta();
        self.notify_config_change();
        tracing::info!(
            session_id = %self.session_id,
            account_id = %account_id,
            "Session account_id persisted to meta file + config_change_tx notified"
        );
    }

    /// Return the persisted reasoning_effort string, if any.
    pub fn reasoning_effort(&self) -> Option<String> {
        self.reasoning_effort.lock().ok().and_then(|r| r.clone())
    }

    /// Persist the per-session reasoning_effort override to meta file.
    ///
    /// Updates in-memory state and writes the meta file.
    pub fn update_reasoning_effort(&self, effort: Option<String>) {
        if let Ok(mut r) = self.reasoning_effort.lock() {
            *r = effort;
        }
        self.write_meta();
        self.notify_config_change();
        tracing::info!(
            session_id = %self.session_id,
            "Session reasoning_effort persisted to meta file"
        );
    }

    /// Update `reasoning_effort` in-memory **without** calling
    /// `write_meta()` or `notify_config_change()`.
    ///
    /// Used by `SessionManager::route_model_switch` to pre-set the
    /// new model's default `reasoning_effort` **before** calling
    /// `apply_config()`.  This way, the single `notify_config_change()`
    /// inside `apply_config()` publishes a snapshot that already
    /// contains the correct `reasoning_effort` for the new model —
    /// there is no window where a stale value from the previous model
    /// is published.
    ///
    /// `write_meta()` and `config_version` increment are handled by
    /// the subsequent `apply_config()` call.
    pub fn set_reasoning_effort_raw(&self, effort: Option<String>) {
        if let Ok(mut r) = self.reasoning_effort.lock() {
            *r = effort;
        }
    }

    /// Return the persisted temperature, if any.
    pub fn temperature(&self) -> Option<f32> {
        self.temperature.lock().ok().and_then(|t| *t)
    }

    /// Persist the per-session temperature override to meta file.
    pub fn update_temperature(&self, temperature: Option<f32>) {
        if let Ok(mut t) = self.temperature.lock() {
            *t = temperature;
        }
        self.write_meta();
        self.notify_config_change();
        tracing::info!(
            session_id = %self.session_id,
            "Session temperature persisted to meta file"
        );
    }

    /// Return the persisted per-session context window override (ADR-074),
    /// if any. `None` = no override — the session inherits the per-agent
    /// resolution chain (Layer 1+ in ADR-026).
    pub fn context_window(&self) -> Option<u64> {
        self.context_window.lock().ok().and_then(|c| *c)
    }

    /// Persist the per-session context window override to meta file
    /// (ADR-074). `None` (or an invalid value such as `0` / out-of-range)
    /// clears the override: the field is normalized to `None` and omitted
    /// from disk, so "cleared" and "never set" are the same state.
    pub fn update_context_window(&self, context_window: Option<u64>) {
        let normalized =
            context_window.filter(|n| crate::agent::session_config::is_valid_context_window(*n));
        if let Ok(mut c) = self.context_window.lock() {
            *c = normalized;
        }
        self.write_meta();
        self.notify_config_change();
        tracing::info!(
            session_id = %self.session_id,
            has_override = normalized.is_some(),
            "Session context_window persisted to meta file"
        );
    }

    /// Return the persisted todo list, if any (ADR-060 §6.1).
    pub fn todos(&self) -> Option<Vec<TodoItem>> {
        self.todos.lock().ok().and_then(|t| t.clone())
    }

    /// Last calibrated session chars/token ratio, if any.
    pub fn model_ratio(&self) -> Option<f64> {
        self.model_ratio.lock().ok().and_then(|r| *r)
    }

    /// Persist the calibrated session chars/token ratio to the meta file.
    ///
    /// Written after each reliable LLM usage calibration so a resumed
    /// session restores the token-estimation scene (instead of default 3.5).
    /// Content-equal updates skip the write.
    pub fn set_model_ratio(&self, ratio: f64) {
        {
            let mut slot = self.model_ratio.lock().unwrap_or_else(|e| e.into_inner());
            if *slot == Some(ratio) {
                return;
            }
            *slot = Some(ratio);
        }
        self.write_meta();
    }

    /// Persist the todo snapshot to the meta file (ADR-060 §6.1).
    ///
    /// Data flow: `todo_write` tool → `SessionState::update_todos()` →
    /// this method (sync mirror). `ConversationSession` is the single
    /// persistence owner; `SessionState` never writes meta directly.
    ///
    /// Write policy: content-equal updates skip the write entirely; changed
    /// updates persist IMMEDIATELY (metadata-mutation semantics, same as
    /// title/model/provider — `META_WRITE_COOLDOWN_MS` guards only the
    /// high-frequency `append_message` path). Immediate write guarantees
    /// the first `todo_write` after session creation survives a kill within
    /// the cooldown window (ADR-060 §6.1: restart must restore todos).
    /// Persist the todo snapshot to the meta file (ADR-060 §6.1).
    ///
    /// Data flow: `todo_write` tool → `SessionState::update_todos()` →
    /// this method (sync mirror). `ConversationSession` is the single
    /// persistence owner; `SessionState` never writes meta directly.
    ///
    /// Write policy: content-equal updates skip the write entirely; changed
    /// updates persist IMMEDIATELY (metadata-mutation semantics, same as
    /// title/model/provider — `META_WRITE_COOLDOWN_MS` guards only the
    /// high-frequency `append_message` path). Immediate write guarantees
    /// the first `todo_write` after session creation survives a kill within
    /// the cooldown window (ADR-060 §6.1: restart must restore todos).
    pub fn set_todos(&self, todos: &[TodoItem]) {
        {
            let mut slot = self.todos.lock().unwrap_or_else(|e| e.into_inner());
            // Skip the write when the content is unchanged — a byte-stable
            // todo list must not touch the meta file (ADR-060 §5.4/§6.1).
            if slot.as_deref() == Some(todos) {
                return;
            }
            *slot = if todos.is_empty() {
                None
            } else {
                Some(todos.to_vec())
            };
        }
        // Changed content → persist immediately (metadata mutation).
        self.write_meta();
        tracing::debug!(
            session_id = %self.session_id,
            todo_count = todos.len(),
            "Todo snapshot persisted to meta file"
        );
    }

    /// ADR-076 §决策 4: record the owning account.
    ///
    /// Called **once** by the session-creation path. Ownership is an
    /// immutable fact (invariant 4), so a later call that would *change*
    /// it is ignored with a warning rather than silently re-assigning the
    /// session; an idempotent re-set of the same value is a no-op.
    pub fn set_initiator(&self, initiator: Option<String>) {
        {
            let mut slot = self.initiator.lock().unwrap_or_else(|e| e.into_inner());
            if *slot == initiator {
                return;
            }
            if slot.is_some() {
                tracing::warn!(
                    session_id = %self.session_id,
                    "session ownership is immutable — ignoring re-assignment"
                );
                return;
            }
            *slot = initiator;
        }
        // Persist immediately: `ConversationSession::new` writes the meta
        // file before the owner is known, and the listing/authorization
        // paths read that file. Leaving the owner in memory only would
        // expose the session as ownerless (writable by anyone) to every
        // reader until some unrelated mutation happened to flush meta.
        self.write_meta();
    }

    /// ADR-076 §决策 4: the owning account, if any (`None` = legacy /
    /// local mode).
    pub fn initiator(&self) -> Option<String> {
        self.initiator.lock().ok().and_then(|u| u.clone())
    }

    /// ADR-076 §决策 4: whether this session is restricted to its owner.
    ///
    /// An unset visibility means public — see [`SessionVisibility`].
    pub fn is_private(&self) -> bool {
        self.visibility()
            .is_some_and(SessionVisibility::is_private)
    }

    /// ADR-076 §决策 4: the raw visibility setting (`None` = unset =
    /// public).
    pub fn visibility(&self) -> Option<SessionVisibility> {
        self.visibility.lock().ok().and_then(|v| *v)
    }

    /// ADR-076 §决策 4: set the read visibility and persist it.
    ///
    /// Mutable by design (unlike [`Self::set_initiator`]): sharing a
    /// session is a reversible user decision, not an identity fact.
    /// `None` clears the field, restoring the public default — and,
    /// because the field is `skip_serializing_if`, restoring the
    /// pre-ADR-076 on-disk shape.
    pub fn set_visibility(&self, visibility: Option<SessionVisibility>) {
        {
            let mut slot = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
            if *slot == visibility {
                return;
            }
            *slot = visibility;
        }
        self.write_meta();
    }

    /// Design 27 §8.2 / ADR-100 D9: the Chat session this execution session
    /// was started from (stamped by `set_chat_binding`), if any. A turn the
    /// user composes directly in this session owes that Chat a result even
    /// when no routed request carried the debt, so the turn-end echo reads
    /// this binding. `None` for a session that never came from a Chat.
    pub fn chat_session_id(&self) -> Option<String> {
        self.chat_session_id
            .lock()
            .ok()
            .and_then(|c| c.clone())
    }

    /// Design 27 §8.2: bind this execution session to a Chat session and
    /// mark it `Members`-visible in one persisted step. Called once at
    /// creation from the `chat:` routing path; the pair is what makes the
    /// group read check (`is_readable_by`) work without an ACL table.
    pub fn set_chat_binding(&self, chat_session_id: &str) {
        {
            let mut slot = self
                .chat_session_id
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if slot.as_deref() == Some(chat_session_id)
                && self.visibility() == Some(SessionVisibility::Members)
            {
                return;
            }
            *slot = Some(chat_session_id.to_string());
        }
        {
            let mut v = self.visibility.lock().unwrap_or_else(|e| e.into_inner());
            *v = Some(SessionVisibility::Members);
        }
        self.write_meta();
    }

    /// THE single entry point for ALL config changes (ADR-047).
    ///
    /// Synchronous: memory + meta.json + MQTT notification.
    /// Called from SessionManager / SessionConfigService, NOT from SessionTask.
    ///
    /// After mutation, `config_version` is incremented so `SessionTask`
    /// can detect the change at the next turn boundary and apply
    /// deferred LLM-side effects via `llm_effects::apply_llm_effects`.
    pub fn apply_config(&self, delta: &crate::agent::session_config::SessionConfigDelta) {
        let mut changed = false;

        if let Some(ref model) = delta.model {
            if let Ok(mut m) = self.model.lock() {
                *m = Some(model.clone());
            }
            changed = true;
        }
        if let Some(ref provider) = delta.provider {
            if let Ok(mut p) = self.provider.lock() {
                *p = Some(provider.clone());
            }
            changed = true;
        }
        if let Some(ref account_id) = delta.account_id {
            if let Ok(mut a) = self.account_id.lock() {
                *a = Some(account_id.clone());
            }
            changed = true;
        }
        if let Some(ref workspace_id) = delta.workspace_id {
            if let Ok(mut w) = self.workspace_id.lock() {
                *w = Some(workspace_id.clone());
            }
            changed = true;
        }
        if let Some(ref effort) = delta.reasoning_effort {
            if let Ok(mut r) = self.reasoning_effort.lock() {
                *r = Some(effort.clone());
            }
            changed = true;
        }
        if let Some(temp) = delta.temperature {
            if let Ok(mut t) = self.temperature.lock() {
                *t = Some(temp);
            }
            changed = true;
        }
        // ADR-074: context_window override. Write-path normalization:
        // the HTTP layer already rejects out-of-range values with 400,
        // so this branch only guards against hand-edited meta / stale
        // callers — `Some(0)` or any out-of-range value means "clear"
        // (fall back to the per-agent chain) and is normalized to `None`
        // so the field disappears from disk (D4 / §1.3).
        if let Some(cw) = delta.context_window {
            let normalized = if crate::agent::session_config::is_valid_context_window(cw) {
                Some(cw)
            } else {
                tracing::warn!(
                    session_id = %self.session_id,
                    context_window = cw,
                    "Session context_window invalid, clearing override"
                );
                None
            };
            if let Ok(mut c) = self.context_window.lock() {
                *c = normalized;
            }
            changed = true;
        }
        if let Some(ref title) = delta.title {
            let truncated = crate::prompt::truncate_title_for_display(title);
            if let Ok(mut current) = self.current_title.lock() {
                *current = Some(truncated);
            }
            self.title_set.store(true, Ordering::Relaxed);
            changed = true;
        }

        if changed {
            self.write_meta();
            self.notify_config_change();
            self.config_version.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Monotonic config version counter (ADR-047).
    ///
    /// `SessionTask` polls this at turn boundaries. A change since the
    /// last poll indicates config was mutated during the previous
    /// inference turn; the task should apply deferred LLM-side effects.
    pub fn config_version(&self) -> u64 {
        self.config_version.load(Ordering::Acquire)
    }

    /// Read-only snapshot of current session config (ADR-047).
    ///
    /// Used by HTTP GET, MQTT retained, and LLM-side effect application.
    pub fn config_snapshot(&self) -> crate::agent::session_config::SessionConfigSnapshot {
        crate::agent::session_config::SessionConfigSnapshot {
            model: self.model.lock().ok().and_then(|m| m.clone()),
            provider: self.provider.lock().ok().and_then(|p| p.clone()),
            account_id: self.account_id.lock().ok().and_then(|a| a.clone()),
            workspace_id: self.workspace_id.lock().ok().and_then(|w| w.clone()),
            reasoning_effort: self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            temperature: self.temperature.lock().ok().and_then(|t| *t),
            context_window: self.context_window.lock().ok().and_then(|c| *c),
            title: self.current_title.lock().ok().and_then(|t| t.clone()),
        }
    }

    /// Return the full [`SessionTokens`] (last + totals), if any LLM call
    /// has been recorded yet.
    ///
    /// Used on resume to seed the frontend "context usage" indicator with
    /// the same raw `prompt_tokens`/`completion_tokens` that the most
    /// recent LLM response reported. Window-derived fields
    /// (`context_window`, `usable_context`, `usage_percent`) are
    /// recomputed at resume time from the *current* model capabilities —
    /// this getter only returns the raw API-fact values.
    pub fn tokens(&self) -> Option<SessionTokens> {
        self.tokens.lock().ok().and_then(|t| t.clone())
    }

    /// Return the per-session lifetime LLM-call count, if any LLM call has
    /// been recorded yet. Mirrors [`Self::tokens`]: `None` before the
    /// first call, then monotonically increasing for the session's life.
    pub fn llm_call_counter(&self) -> Option<u32> {
        self.llm_call_counter.lock().ok().and_then(|c| *c)
    }

    /// Bump the per-session LLM-call counter and return the new 1-based
    /// value. The single mutation owner, mirroring `accumulate_llm_usage`
    /// for tokens: persists to meta.json and notifies the state relay so
    /// the retained `session_state` snapshot carries the fresh count.
    ///
    /// Called once per real LLM response in
    /// `AgentLoop::process_llm_response_usage` — compaction /
    /// title-generation calls use different paths and must NOT bump.
    pub fn bump_llm_call_counter(&self) -> u32 {
        let next = self.apply_llm_call_counter();
        self.write_meta();
        self.notify_state_change();
        next
    }

    /// In-memory half of [`Self::bump_llm_call_counter`].
    fn apply_llm_call_counter(&self) -> u32 {
        self.llm_call_counter
            .lock()
            .ok()
            .map(|mut guard| {
                let next = guard.unwrap_or(0).saturating_add(1);
                *guard = Some(next);
                next
            })
            .unwrap_or(0)
    }

    /// The session's current LLM speed / responsiveness averages.
    pub fn perf(&self) -> Option<SessionPerf> {
        self.perf.lock().ok().and_then(|p| p.clone())
    }

    /// Record one completed LLM round-trip — token usage, wall-clock speed /
    /// TTFT, and the per-session call count — in a **single** persisted
    /// write.
    ///
    /// This is the only entry point the dialog path should use. The three
    /// accumulators describe the same event, so writing them through their
    /// individual persisting wrappers would issue three full-row
    /// `INSERT ... ON CONFLICT` upserts and three state-change notifications
    /// per LLM call, on the hottest path in the runtime. The invariant here
    /// is deliberate and test-pinned: **one round-trip, one meta write**.
    ///
    /// Returns the post-increment 1-based call count, matching
    /// [`Self::bump_llm_call_counter`], so the caller can stamp it onto the
    /// live `ContextUsage` push without a second read.
    ///
    /// `tps` / `ttft_ms` are this call's measurements; `None` (no output
    /// produced, no usage reported, the stream cancelled before its first
    /// token) folds as "no measurement", never as 0.
    ///
    /// `attribution` folds the same usage into the per-model detail map
    /// (`SessionMeta.model_usage`) — see [`Self::apply_model_usage`].
    pub fn record_llm_round(
        &self,
        usage: &UsageInfo,
        tps: Option<f64>,
        ttft_ms: Option<f64>,
        attribution: &ModelUsageAttribution<'_>,
    ) -> u32 {
        self.apply_llm_usage(usage);
        self.apply_model_usage(usage, attribution);
        self.apply_llm_perf(tps, ttft_ms);
        let iteration = self.apply_llm_call_counter();
        self.write_meta();
        // Hot field — the relay task coalesces these behind a 3 s cooldown.
        self.notify_state_change();
        iteration
    }

    /// In-memory half of the perf accumulation: fold one call's measurements
    /// into the session accumulators without touching storage.
    ///
    /// Each figure advances against its own sample count (see
    /// [`SessionPerf::merged`]), so a call that measured only TTFT still
    /// yields an honest speed mean.
    fn apply_llm_perf(&self, tps: Option<f64>, ttft_ms: Option<f64>) {
        if tps.is_none() && ttft_ms.is_none() {
            return;
        }
        let next = {
            let Ok(mut guard) = self.perf.lock() else {
                return;
            };
            let merged = guard
                .clone()
                .unwrap_or_default()
                .merged(tps, ttft_ms);
            *guard = Some(merged.clone());
            merged
        };
        tracing::debug!(
            session_id = %self.session_id,
            avg_tps = ?next.avg_tps(),
            avg_ttft_ms = ?next.avg_ttft_ms(),
            tps_samples = next.tps_samples,
            ttft_samples = next.ttft_ms_samples,
            "Recorded LLM performance sample"
        );
    }

    /// Persist a single LLM call's raw `usage` into the session token
    /// accumulator (ADR-027).
    ///
    /// Semantics:
    /// - `last_input` / `last_output` are **always** overwritten with the
    ///   raw Provider values, including zero. This makes the snapshot
    ///   honestly reflect what the Provider returned (e.g. when
    ///   `prompt_tokens_reliable == false` and the local tokenizer
    ///   fallback would have produced a different number).
    /// - `total_input` only accumulates when `usage.prompt_tokens > 0`;
    ///   a Provider fallback does not silently pollute the running sum.
    /// - `total_output` always accumulates (Providers report completion
    ///   counts more reliably than prompt counts, and a zero here is
    ///   usually a true zero — e.g. an aborted streaming response).
    ///
    /// `last_active_at` is bumped and the meta file is rewritten on every
    /// call. Callers should invoke this once per LLM round-trip, not on
    /// every streaming event.
    pub fn accumulate_llm_usage(
        &self,
        usage: &UsageInfo,
        attribution: &ModelUsageAttribution<'_>,
    ) {
        self.apply_llm_usage(usage);
        self.apply_model_usage(usage, attribution);
        self.write_meta();
        // Hot field — the relay task coalesces these behind a 3 s cooldown.
        self.notify_state_change();
    }

    /// In-memory half of [`Self::accumulate_llm_usage`]: fold one call's
    /// usage into the token accumulator without touching storage.
    ///
    /// Split out so [`Self::record_llm_round`] can fold a whole LLM
    /// round-trip (tokens + speed + call count) and pay for exactly one
    /// meta write. See that method for the persistence invariant.
    fn apply_llm_usage(&self, usage: &UsageInfo) {
        if let Ok(mut guard) = self.tokens.lock() {
            // ADR-027: zero-skip on the input side.  A Provider fallback
            // returns `prompt_tokens == 0`; treating that as a real call
            // would overwrite an established baseline.
            //
            // ADR-066: cache_read shares that zero-skip gate — when a
            // Provider reports a reliable input count, the cache breakdown
            // is also reliable; when input is 0 the cache fields are 0 too
            // (don't let a half-reported usage pollute the running total).
            let total_input = if usage.prompt_tokens > 0 {
                guard
                    .as_ref()
                    .map(|t| t.total_input)
                    .unwrap_or(0)
                    .saturating_add(usage.prompt_tokens)
            } else {
                guard.as_ref().map(|t| t.total_input).unwrap_or(0)
            };
            let total_cache_read = if usage.prompt_tokens > 0 {
                guard
                    .as_ref()
                    .map(|t| t.total_cache_read)
                    .unwrap_or(0)
                    .saturating_add(usage.cache_read_tokens)
            } else {
                guard.as_ref().map(|t| t.total_cache_read).unwrap_or(0)
            };
            let total_output = guard
                .as_ref()
                .map(|t| t.total_output)
                .unwrap_or(0)
                .saturating_add(usage.completion_tokens);
            // cache_write is part of the input cost (Anthropic charges
            // 1.25× for `cache_creation_input_tokens`) but does NOT share
            // the zero-skip gate: an Anthropic cache write with zero
            // regular input tokens is still a real, billable event.
            let total_cache_write = guard
                .as_ref()
                .map(|t| t.total_cache_write)
                .unwrap_or(0)
                .saturating_add(usage.cache_write_tokens);
            *guard = Some(SessionTokens {
                last_input: usage.prompt_tokens,
                last_output: usage.completion_tokens,
                total_input,
                total_output,
                last_cache_read: usage.cache_read_tokens,
                last_cache_write: usage.cache_write_tokens,
                total_cache_read,
                total_cache_write,
            });
        }
    }

    /// In-memory fold of one call's usage into the per-model detail map
    /// (`SessionMeta.model_usage`) without touching storage.
    ///
    /// Mirrors the ADR-027 / ADR-066 zero-skip gate from
    /// [`Self::apply_llm_usage`] exactly (input + cache_read share the
    /// `prompt_tokens > 0` gate; output + cache_write always accumulate),
    /// so Σ detail entries == the `tokens` cumulative totals at every
    /// persist point — the store's upsert diff then maintains the
    /// agent-level `token_stats` aggregate against that invariant.
    ///
    /// Cost folding is poison-on-unknown: a fresh bucket takes the call's
    /// `cost_delta` (which may be `None`), an existing bucket only keeps a
    /// running cost while both sides are known; the first `None` contribution
    /// flips the bucket's cost to `None` permanently (until
    /// `rebuild_token_stats` / a corrected attribution — "宁可 miss 也不估计").
    fn apply_model_usage(&self, usage: &UsageInfo, attribution: &ModelUsageAttribution<'_>) {
        // Fold the same gated deltas as apply_llm_usage.
        let (d_in, d_cr) = if usage.prompt_tokens > 0 {
            (usage.prompt_tokens, usage.cache_read_tokens)
        } else {
            (0, 0)
        };
        let d_out = usage.completion_tokens;
        let d_cw = usage.cache_write_tokens;
        if d_in == 0 && d_out == 0 && d_cr == 0 && d_cw == 0 && attribution.cost_delta.is_none() {
            return;
        }
        let key = model_usage_key(attribution.provider_id, attribution.model_id);
        if let Ok(mut guard) = self.model_usage.lock() {
            let map = guard.get_or_insert_with(Default::default);
            match map.entry(key) {
                std::collections::btree_map::Entry::Vacant(v) => {
                    v.insert(ModelUsageEntry {
                        input: d_in,
                        output: d_out,
                        cache_read: d_cr,
                        cache_write: d_cw,
                        cost: attribution.cost_delta,
                    });
                }
                std::collections::btree_map::Entry::Occupied(mut o) => {
                    let e = o.get_mut();
                    e.input = e.input.saturating_add(d_in);
                    e.output = e.output.saturating_add(d_out);
                    e.cache_read = e.cache_read.saturating_add(d_cr);
                    e.cache_write = e.cache_write.saturating_add(d_cw);
                    e.cost = match (e.cost, attribution.cost_delta) {
                        (Some(prev), Some(delta)) => Some(prev + delta),
                        _ => None,
                    };
                }
            }
        }
    }

    /// Persist a compaction LLM call's raw `usage` into the session token
    /// accumulator.
    ///
    /// Unlike [`Self::accumulate_llm_usage`], this method does **not** overwrite
    /// `last_input` with the compaction LLM's `prompt_tokens`. The compaction
    /// LLM was given the **full pre-compaction history** plus a summary
    /// instruction as input, so its prompt size is unrepresentative of the
    /// post-compaction session state — overwriting `last_input` would inflate
    /// the displayed context usage until the next main-dialog LLM call
    /// recalibrates it via `accumulate_llm_usage`.
    ///
    /// What this method does:
    /// - Accumulates `total_input` / `total_output` for billing/metrics parity
    ///   with main-dialog calls (the tokens really were spent, just on a
    ///   meta-call).
    /// - Preserves the previous `last_input` / `last_output` so the next
    ///   `emit_session_state()` push continues to reflect the most recent
    ///   user-facing LLM call.
    /// - Writes meta and notifies the state relay so the cumulative totals
    ///   appear in the next `SessionStateChanged` push.
    ///
    /// **Caller contract**: invoke exactly once per successful compaction,
    /// after the summary LLM call returns and before
    /// [`Self::set_history_anchor`] (which anchors `last_input` to the
    /// post-compaction `history.token_count()`).
    pub fn accumulate_compaction_usage(
        &self,
        usage: &UsageInfo,
        attribution: &ModelUsageAttribution<'_>,
    ) {
        self.apply_model_usage(usage, attribution);
        if let Ok(mut guard) = self.tokens.lock() {
            let prev_last_input = guard.as_ref().map(|t| t.last_input).unwrap_or(0);
            let prev_last_output = guard.as_ref().map(|t| t.last_output).unwrap_or(0);
            // ADR-066: cache breakdown must NOT overwrite the per-turn
            // `last_cache_*` snapshot — the compaction LLM call was given
            // the full pre-compaction history as input, so its cache
            // figures are unrepresentative of the post-compaction
            // session state (same rationale as `last_input` preservation).
            let prev_last_cache_read = guard.as_ref().map(|t| t.last_cache_read).unwrap_or(0);
            let prev_last_cache_write = guard.as_ref().map(|t| t.last_cache_write).unwrap_or(0);
            let total_input = if usage.prompt_tokens > 0 {
                guard
                    .as_ref()
                    .map(|t| t.total_input)
                    .unwrap_or(0)
                    .saturating_add(usage.prompt_tokens)
            } else {
                guard.as_ref().map(|t| t.total_input).unwrap_or(0)
            };
            // ADR-066: cache_read follows the input zero-skip gate (see
            // `accumulate_llm_usage` rationale).
            let total_cache_read = if usage.prompt_tokens > 0 {
                guard
                    .as_ref()
                    .map(|t| t.total_cache_read)
                    .unwrap_or(0)
                    .saturating_add(usage.cache_read_tokens)
            } else {
                guard.as_ref().map(|t| t.total_cache_read).unwrap_or(0)
            };
            let total_output = guard
                .as_ref()
                .map(|t| t.total_output)
                .unwrap_or(0)
                .saturating_add(usage.completion_tokens);
            // cache_write does NOT share the zero-skip gate (see
            // `accumulate_llm_usage` rationale).
            let total_cache_write = guard
                .as_ref()
                .map(|t| t.total_cache_write)
                .unwrap_or(0)
                .saturating_add(usage.cache_write_tokens);
            *guard = Some(SessionTokens {
                last_input: prev_last_input,
                last_output: prev_last_output,
                total_input,
                total_output,
                last_cache_read: prev_last_cache_read,
                last_cache_write: prev_last_cache_write,
                total_cache_read,
                total_cache_write,
            });
        }
        self.write_meta();
        self.notify_state_change();
    }

    /// Anchor `last_input` to the post-compaction history size so the next
    /// `emit_session_state()` push reports the new (smaller) `usage_percent`
    /// **immediately**, without waiting for the next main-dialog LLM call.
    ///
    /// `new_tokens` typically comes from `history.token_count()` after
    /// `replace_middle_with_summary`. It is a **heuristic** local estimate,
    /// not a Provider-reported value — the next `accumulate_llm_usage()`
    /// call will overwrite it with the API's authoritative `prompt_tokens`
    /// for the next main dialog turn.
    ///
    /// Why this is needed:
    /// - `emit_session_state()` derives `context_usage.usage_percent` from
    ///   `tokens.last_input`. Before this anchor, that field still reflects
    ///   the pre-compaction LLM call's `prompt_tokens`, which is much
    ///   larger than the post-compaction history.
    /// - Without this anchor, the `SessionStateChanged` push sent via
    ///   `notify_state_change()` (triggered by `accumulate_compaction_usage`)
    ///   carries a stale, inflated percentage that contradicts the
    ///   standalone `ContextUsage` event also published at the same instant.
    ///   Frontends that key off `SessionStateChanged` then show a confusing
    ///   "70% before, 20% after next message" jump.
    ///
    /// `last_output` is set to 0 because there is no completion event
    /// associated with the compaction event itself; the summary's completion
    /// tokens were already counted into `total_output` by
    /// `accumulate_compaction_usage`.
    pub fn set_history_anchor(&self, new_tokens: u64) {
        if let Ok(mut guard) = self.tokens.lock() {
            // ADR-066: cache `total_*` fields are Provider-aggregated
            // metrics that must survive the anchor rewrite — the
            // post-compaction history size has no bearing on how many
            // tokens the previous LLM calls hit from cache.
            //
            // `last_cache_*` are zeroed in lock-step with `last_output`
            // = 0: there is no fresh per-turn cache breakdown until the
            // next `accumulate_llm_usage` call, mirroring the rationale
            // for `last_input` being overwritten by the anchor value.
            let (total_input, total_output, total_cache_read, total_cache_write) = guard
                .as_ref()
                .map(|t| {
                    (
                        t.total_input,
                        t.total_output,
                        t.total_cache_read,
                        t.total_cache_write,
                    )
                })
                .unwrap_or((0, 0, 0, 0));
            *guard = Some(SessionTokens {
                last_input: new_tokens,
                last_output: 0,
                total_input,
                total_output,
                last_cache_read: 0,
                last_cache_write: 0,
                total_cache_read,
                total_cache_write,
            });
        }
        self.write_meta();
        self.notify_state_change();
    }
}

// Send + Sync are auto-derived: all fields (String, Mutex<X: Send>,
// AtomicBool/AtomicU64, Instant, UnboundedSender, PathBuf) are Send + Sync.

/// Manual `Clone` so the struct can be cheaply captured into a spawned task
/// (e.g. session-end distillation that needs to call
/// [`Self::accumulate_llm_usage`] after the parent has already `.close()`d).
///
/// Each clone gets fresh `Mutex` / `Atomic` guards holding the same inner
/// value at the moment of cloning; later mutations on one clone are not
/// observed by another clone (the clone is a logical snapshot for read-only
/// purposes — callers must not mutate shared state from two clones
/// concurrently). The writer-thread channel sender is shared so any clone
/// can still append JSONL entries until the writer thread receives
/// `Shutdown`.
impl Clone for ConversationSession {
    fn clone(&self) -> Self {
        Self {
            session_id: self.session_id.clone(),
            agent_id: self.agent_id.clone(),
            created_at: self.created_at.clone(),
            title_set: AtomicBool::new(self.title_set.load(Ordering::Relaxed)),
            current_title: std::sync::Mutex::new(
                self.current_title.lock().ok().and_then(|t| t.clone()),
            ),
            workspace_id: std::sync::Mutex::new(
                self.workspace_id.lock().ok().and_then(|w| w.clone()),
            ),
            model: std::sync::Mutex::new(self.model.lock().ok().and_then(|m| m.clone())),
            provider: std::sync::Mutex::new(self.provider.lock().ok().and_then(|p| p.clone())),
            account_id: std::sync::Mutex::new(self.account_id.lock().ok().and_then(|a| a.clone())),
            reasoning_effort: std::sync::Mutex::new(
                self.reasoning_effort.lock().ok().and_then(|r| r.clone()),
            ),
            temperature: std::sync::Mutex::new(self.temperature.lock().ok().and_then(|t| *t)),
            context_window: std::sync::Mutex::new(self.context_window.lock().ok().and_then(|c| *c)),
            todos: std::sync::Mutex::new(self.todos.lock().ok().and_then(|t| t.clone())),
            initiator: std::sync::Mutex::new(self.initiator.lock().ok().and_then(|u| u.clone())),
            visibility: std::sync::Mutex::new(self.visibility.lock().ok().and_then(|v| *v)),
            chat_session_id: std::sync::Mutex::new(self.chat_session_id.lock().ok().and_then(|c| c.clone())),
            tokens: std::sync::Mutex::new(self.tokens.lock().ok().and_then(|t| t.clone())),
            model_usage: std::sync::Mutex::new(
                self.model_usage.lock().ok().and_then(|m| m.clone()),
            ),
            llm_call_counter: std::sync::Mutex::new(
                self.llm_call_counter.lock().ok().and_then(|c| *c),
            ),
            perf: std::sync::Mutex::new(self.perf.lock().ok().and_then(|p| p.clone())),
            model_ratio: std::sync::Mutex::new(self.model_ratio.lock().ok().and_then(|r| *r)),
            message_count: AtomicU64::new(self.message_count.load(Ordering::Relaxed)),
            last_meta_write: std::sync::Mutex::new(
                self.last_meta_write
                    .lock()
                    .ok()
                    .map(|i| *i)
                    .unwrap_or_else(Instant::now),
            ),
            sender: self.sender.clone(),
            session_file_path: self.session_file_path.clone(),
            conversations_dir: self.conversations_dir.clone(),
            config_change_tx: self.config_change_tx.clone(),
            state_change_tx: self.state_change_tx.clone(),
            config_version: AtomicU64::new(self.config_version.load(Ordering::Relaxed)),
            last_status: std::sync::Mutex::new(
                self.last_status
                    .lock()
                    .ok()
                    .map(|s| s.clone())
                    .unwrap_or_default(),
            ),
            last_ratio: std::sync::Mutex::new(
                self.last_ratio
                    .lock()
                    .ok()
                    .map(|r| *r)
                    .unwrap_or(crate::token::counter::DEFAULT_RATIO),
            ),
            last_context_usage: std::sync::Mutex::new(
                self.last_context_usage
                    .lock()
                    .ok()
                    .map(|s| s.clone())
                    .unwrap_or_default(),
            ),
            pending_intents: std::sync::Mutex::new(
                self.pending_intents.lock().ok().map(|p| p.clone()).unwrap_or_default(),
            ),
            // ADR-067: the sections cache is owned per-session — each clone
            // starts empty and receives its own writes. Sharing it (Arc)
            // would require an Arc<Mutex<…>> for no benefit, since
            // `cache_context_usage_sections` always overwrites with the
            // latest value and the writer is single-threaded per session.
            last_context_usage_sections: std::sync::Mutex::new(None),
            // ADR-024: share the same Arc with the original — clones observe
            // future compaction writes from the writer thread exactly like
            // the parent does. This matches the comment above that the
            // sender is shared for the same reason.
            last_compaction_offset: self.last_compaction_offset.clone(),
        }
    }
}

impl Drop for ConversationSession {
    fn drop(&mut self) {
        // Force-flush the final state to the meta file so the frontend sees
        // the correct message_count and last_active_at even if the last
        // `append_message` fell within the cooldown window.
        self.write_meta();
        // ADR-043: notify both relays so the final snapshots are published
        // before the channels are dropped. Best-effort - the receivers may
        // already be gone, in which case `send` is a silent no-op.
        // Empty session_id acts as a Drop sentinel - the relay falls back
        // to its own ConversationSession clone for the real snapshot.
        let _ = self.config_change_tx.send(ConfigChange {
            snapshot: acowork_core::mqtt_proto::SessionConfig::default(),
        });
        let _ = self.state_change_tx.send(StateChange {
            snapshot: acowork_core::mqtt_proto::SessionState::default(),
        });
    }
}

/// Generate a new session ID.
///
/// Format: `{YYYYMMDD_HHMMSS}_{6-char short UUID}`
/// Example: `20260503_143022_a1b2c3`
pub fn generate_session_id() -> String {
    let now = chrono::Local::now();
    let timestamp = now.format("%Y%m%d_%H%M%S").to_string();
    let short_uuid = uuid::Uuid::new_v4().to_string();
    let short_uuid = &short_uuid[..6];
    format!("{}_{}", timestamp, short_uuid)
}

/// Information about a scanned session.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    /// Session identifier
    pub session_id: String,
    /// ISO 8601 creation timestamp
    pub created_at: String,
    /// ISO 8601 timestamp of the most recent activity
    pub last_active_at: String,
    /// Number of messages in the session
    pub message_count: u32,
    /// Optional session title
    pub title: Option<String>,
    /// Whether the session metadata was recovered from a corrupted first line
    pub corrupted: bool,
    /// Per-session model selection (ADR-012), from JSONL metadata
    pub model: Option<String>,
    /// Per-session provider selection (ADR-012), from JSONL metadata
    pub provider: Option<String>,
    /// Per-session account selection (multi-account), from JSONL metadata
    pub account_id: Option<String>,
    /// Per-session workspace selection, from JSONL metadata
    pub workspace_id: Option<String>,
    /// ADR-076 §决策 4: read visibility (`None` = public). Surfaced so
    /// the client can render the share toggle without a second request.
    pub visibility: Option<SessionVisibility>,
    /// ADR-076 §决策 4: who initiated this session (account or agent
    /// instance UUID). The client renders the execution surface's identity
    /// from this — never from the logged-in user, which is who happens to be
    /// looking, not who started the work.
    pub initiator: Option<String>,
    /// ADR-076 §决策 4: whether the caller may modify this session —
    /// the same `is_writable_by` the write handlers enforce.
    ///
    /// Deliberately *not* named `owned_by_me`: a `local`-mode caller may
    /// write a session it did not initiate, so an ownership flag would make
    /// the UI disable controls that actually work. Clients need this to
    /// disable the workspace / model / title inputs up front instead of
    /// firing a request that 404s (and to open a foreign session read-only).
    pub can_write: bool,
}

// ── Session Index (fast O(1) lookup) ───────────────────────────────────────
//
// ADR-024: the index.json + SessionIndexEntry + SessionIndex system has
// been superseded by the session-meta store (ADR-082 §4 step 3: SQLite).
// Use `scan_sessions_from_meta()` for listing and `read_session_meta()` for
// single-session lookup.

// ── Per-session meta I/O (ADR-024, ADR-082 §4 step 3) ─────────────────────
//
// Session meta lives in the workspace's `memory/private.sqlite` (`sessions` /
// `fts_sessions`) — there is no JSON backend. The free functions below are thin
// wrappers over the SQLite store.

/// Session-meta stores keyed by the `.sqlite` file they serve.
///
/// [`install_session_meta_backend`] seeds the entry for the running workspace
/// with the store `AgentCore` already opened, so session meta shares the memory
/// connection. A directory with no entry — a test, or a call that lands before
/// the memory backend is up — opens its own, see [`session_meta_store`].
static SESSION_META_STORES: OnceLock<Mutex<HashMap<PathBuf, Arc<dyn SessionMetaStore>>>> =
    OnceLock::new();

fn session_meta_registry() -> &'static Mutex<HashMap<PathBuf, Arc<dyn SessionMetaStore>>> {
    SESSION_META_STORES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `private.sqlite` for the workspace owning `conversations_dir`
/// (`…/conversations` -> `…/memory/private.sqlite`).
fn session_meta_db_path(conversations_dir: &Path) -> PathBuf {
    let workspace = conversations_dir.parent().unwrap_or(conversations_dir);
    // Caller contract: the *workspace's* conversations dir. Hand in a doubled
    // path (`…/conversations/conversations`, what `POST /sessions/{sid}/open`
    // used to do) and the store lands *inside* the log dir as
    // `…/conversations/memory/private.sqlite` — a second, empty DB.
    debug_assert!(
        !workspace.ends_with("conversations"),
        "session-meta store must not live inside the conversations dir: {}",
        workspace.join("memory").join("private.sqlite").display()
    );
    workspace.join("memory").join("private.sqlite")
}

/// Install the session-meta backend for a workspace (ADR-082 §4 step 3).
///
/// Called once at startup with the same `Arc<SqliteStore>` the memory backend
/// uses, so both share one connection. A re-install for the same workspace
/// replaces the entry.
pub fn install_session_meta_backend(
    conversations_dir: &Path,
    store: Arc<dyn SessionMetaStore>,
) {
    session_meta_registry()
        .lock()
        .unwrap()
        .insert(session_meta_db_path(conversations_dir), store);
}

/// The session-meta store for `conversations_dir`. Always SQLite.
///
/// A registered store wins (production: the memory-shared connection).
/// Otherwise the workspace's `private.sqlite` is opened on demand — dim-agnostic
/// so a later memory-backend open still gets to record the real embedding
/// dimension, see `SqliteStore::open_dim_agnostic`. `find_latest_session` runs
/// before the memory backend is up, so the open has to live here rather than
/// only in [`install_session_meta_backend`].
pub(crate) fn session_meta_store(conversations_dir: &Path) -> std::io::Result<Arc<dyn SessionMetaStore>> {
    let db_path = session_meta_db_path(conversations_dir);
    if let Some(store) = session_meta_registry().lock().unwrap().get(&db_path) {
        return Ok(store.clone());
    }
    let db = acowork_sqlite::SqliteStore::open_dim_agnostic(&db_path).map_err(|e| {
        std::io::Error::other(format!("open session-meta store {}: {e}", db_path.display()))
    })?;
    let store: Arc<dyn SessionMetaStore> =
        Arc::new(acowork_sqlite::SqliteSessionMetaStore::new(Arc::new(db)));

    session_meta_registry()
        .lock()
        .unwrap()
        .insert(db_path, store.clone());
    Ok(store)
}

/// Legacy free-function form of `SessionMetaStore::upsert`.
pub fn write_session_meta(conversations_dir: &Path, meta: &SessionMeta) -> std::io::Result<()> {
    session_meta_store(conversations_dir)?
        .upsert(meta)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Legacy free-function form of `SessionMetaStore::get`.
pub fn read_session_meta(
    conversations_dir: &Path,
    session_id: &str,
) -> std::io::Result<SessionMeta> {
    match session_meta_store(conversations_dir)?.get(session_id) {
        Ok(Some(meta)) => Ok(meta),
        Ok(None) => Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {session_id} not found"),
        )),
        Err(e) => Err(std::io::Error::other(e.to_string())),
    }
}

/// `true` when the session-meta store holds a row for `session_id`.
///
/// Unlike [`read_session_meta`] this keeps "absent" (`Ok(None)`) distinct from
/// a backend error, which is exactly what the session lifecycle state machine
/// needs. A missing database, or one without the row, both read as absent.
pub fn session_meta_exists(conversations_dir: &Path, session_id: &str) -> bool {
    session_meta_store(conversations_dir)
        .ok()
        .is_some_and(|store| matches!(store.get(session_id), Ok(Some(_))))
}

/// Legacy free-function form of `SessionMetaStore::delete`.
pub fn delete_session_meta(conversations_dir: &Path, session_id: &str) -> std::io::Result<()> {
    session_meta_store(conversations_dir)?
        .delete(session_id)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Legacy free-function form of `SessionMetaStore::list_recent`,
/// preserving the `(session_id, SessionMeta)` tuple shape every caller
/// already destructures.
pub fn scan_sessions_from_meta(conversations_dir: &Path) -> Vec<(String, SessionMeta)> {
    let store = match session_meta_store(conversations_dir) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(error = %e, "scan_sessions_from_meta: store unavailable");
            return Vec::new();
        }
    };
    match store.list_recent(usize::MAX) {
        Ok(rows) => rows.into_iter().map(|m| (m.session_id.clone(), m)).collect(),
        Err(e) => {
            tracing::warn!(error = %e, "scan_sessions_from_meta: store failed");
            Vec::new()
        }
    }
}

/// Prune excess sessions when **an owner's** session count exceeds
/// `max_sessions`.
///
/// ADR-076 §决策 4: the cap is per **owner**, not per agent.  Before the
/// account system existed one agent had exactly one user, so the two were
/// the same thing; now that an agent's history is shared, a global cap
/// would let one account's activity archive another account's history.
///
/// Sessions are ordered by `last_active_at` (oldest first) and archived
/// until each owner's bucket is within the limit.  Ownerless sessions
/// (`initiator == None` — pre-ADR-076 data and every session in `local`
/// mode) form a single bucket and keep the pre-existing semantics.
///
/// Returns the number of sessions pruned.
///
/// # Safety
///
/// This function only looks at the session-meta store; it does NOT interact
/// with `SessionManager`.  By design it can only prune sessions that have been
/// evicted from memory (idle timeout), because active sessions constantly
/// update their `last_active_at` and will never be the oldest.
pub(crate) fn prune_excess_sessions(conversations_dir: &Path, max_sessions: usize) -> usize {
    if max_sessions == 0 {
        return 0;
    }

    // The meta backend tells us which sessions to delete; this function
    // owns the matching JSONL archival. The trait returns the deleted
    // ids newest-first, but we want oldest-first because that's the
    // historical order the old code pruned in.
    let store = match session_meta_store(conversations_dir) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!(error = %e, "prune_excess_sessions: store unavailable");
            return 0;
        }
    };
    let victims = match store.prune_to(max_sessions) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "prune_excess_sessions: store failed");
            return 0;
        }
    };
    if victims.is_empty() {
        return 0;
    }

    // `prune_to` returns victims oldest-first, so we
    // can archive the JSONL files in order without an extra sort.
    let mut pruned = 0usize;
    for session_id in victims.iter() {
        let jsonl_path = conversations_dir.join(format!("{session_id}.jsonl"));
        let archive_path = conversations_dir.join(format!("{session_id}.jsonl.archive"));

        // ADR-024: archive the JSONL file (rename) instead of deleting.
        match std::fs::rename(&jsonl_path, &archive_path) {
            Ok(()) => {
                tracing::debug!(
                    session_id = %session_id,
                    archive = %archive_path.display(),
                    "Archived excess session JSONL"
                );
                pruned += 1;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // JSONL already gone — still count as pruned.
                pruned += 1;
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = %jsonl_path.display(),
                    error = %e,
                    "Failed to archive JSONL file during session pruning"
                );
                continue;
            }
        }
    }

    if pruned > 0 {
        tracing::info!(pruned, "Archived excess sessions");
    }

    pruned
}

/// Paginated message result.
///
/// ADR-050: Pagination uses a **forward** (oldest-end) `offset` model —
///
/// - `offset = 0`  → the **oldest** `limit` raw entries (i.e. entries `[0, limit)`).
/// - `offset = K`  → the `limit` raw entries starting at index K
///   (i.e. entries `[K, K+limit)`, clamped to `total`).
///
/// To scroll **older** (toward smaller offsets / older messages):
///   `next_offset = max(0, offset - returned_limit)`.
/// To scroll **newer** (toward the bottom of the conversation):
///   `next_offset = offset + returned_limit`.
/// The cache window has reached the **newest** message when
/// `offset + limit >= total`.  See ADR-050 §3.2.
#[derive(Debug, Clone)]
pub struct PaginatedMessages {
    /// Messages in the current page, in chronological order
    /// (oldest → newest within the page).
    pub messages: Vec<ConversationEntry>,
    /// Echo of the requested offset (forward semantics: 0 = oldest entry).
    pub offset: u64,
    /// Number of messages actually returned (≤ requested limit).
    pub limit: u32,
    /// Total number of message entries in the session JSONL
    /// (metadata header lines are not counted).
    pub total: u64,
}

// ── ADR-021: StreamingStateMap types ───────────────────────────────────────

/// An incomplete line: a message currently being streamed by the LLM
/// but not yet flushed to the JSONL file.
///
/// The frontend reads this via `read_messages_since()` to get the
/// in-progress content without waiting for a natural flush boundary.
#[derive(Debug, Clone)]
pub struct StreamingLine {
    /// The line number this will become in JSONL (0-based; 0 = metadata).
    pub line_number: usize,
    /// Role: "assistant" | "thought".
    pub role: String,
    /// ADR-035 M2: stable message id assigned at streaming line creation.
    /// Carried by every `stream_delta` push and the final `record_complete`
    /// event so the frontend can match the active buffer to the finalized
    /// record. Also written to JSONL via `append_message_with_id` so the
    /// persisted entry shares the same id.
    pub message_id: String,
    /// Current accumulated content (grows with each Delta).
    pub accumulated_content: String,
    /// ISO 8601 timestamp when streaming started.
    pub started_at: String,
    /// Unix epoch milliseconds when streaming started (for timing metadata
    /// in metadata: {startTime, endTime} written to JSONL on flush).
    pub started_at_ms: i64,
}

/// Delta portion of a streaming line returned to the frontend.
///
/// Only carries the *new* content since `char_offset`, not the full
/// accumulated content. This keeps poll responses small.
#[derive(Debug, Clone, Serialize)]
pub struct StreamingLineDelta {
    /// The line number this streaming line will become in JSONL.
    pub line: usize,
    /// Role: "assistant" | "thought".
    pub role: String,
    /// Only the new content since the requested `char_offset`.
    pub content: String,
    /// Current total character length of the accumulated content.
    /// The frontend uses this as the next `line_char_offset`.
    pub char_offset: usize,
}

/// Result of `read_messages_since()`.
#[derive(Debug, Clone)]
pub struct ReadMessagesSinceResult {
    /// New complete lines from JSONL (after `line_number`).
    pub messages: Vec<ConversationEntry>,
    /// Incomplete streaming line delta, if one exists for this session.
    pub streaming: Option<StreamingLineDelta>,
    /// Total lines in the JSONL file (including metadata line 0).
    pub total_lines: usize,
}

// ── Backend-managed delivery cursor (ADR-025) ───────────────────────────

/// Per-session delivery cursor — the backend tracks how much data has been
/// delivered to the frontend.  The frontend never sends coordinates; it
/// simply polls with `incremental=true` and the backend uses this cursor
/// to determine what to return.
///
/// * `line_number` — number of complete JSONL lines already delivered.
/// * `char_offset` — number of characters already delivered from the
///   current in-progress streaming line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeliveryCursor {
    pub line_number: usize,
    pub char_offset: usize,
}

/// Result of `read_messages_since_cursor()`.
#[derive(Debug, Clone)]
pub struct ReadMessagesSinceCursorResult {
    /// New complete lines from JSONL (batch-limited by `limit`).
    pub messages: Vec<ConversationEntry>,
    /// Incomplete streaming line delta, if one exists for this session.
    pub streaming: Option<StreamingLineDelta>,
    /// Total lines in the JSONL file.
    pub total_lines: usize,
    /// Whether more undelivered complete lines remain (batch catch-up signal).
    pub has_more: bool,
    /// The updated cursor after this read — caller should persist this.
    pub new_cursor: DeliveryCursor,
}

/// Shared map from SessionId to the current incomplete streaming line.
///
/// Written by AgentLoop on each Delta, read by the HTTP handler on poll.
/// Wrapped in `Arc<RwLock>` for concurrent access across tokio tasks.
pub type StreamingStateMap = Arc<RwLock<HashMap<String, StreamingLine>>>;

/// Find the most recently active session.
///
/// ADR-082 §4 step 3: reads the session-meta store (SQLite).
///
/// Deliberately unscoped (ADR-076 §决策 4): this feeds the runtime's
/// per-agent "resume my latest conversation" cache, written at startup before
/// any caller exists. Scope-enforcing here would be wrong for that caller —
/// cold start has no user. The HTTP surface that exposes the cache
/// (`GET /sessions/latest`) is where ownership is checked: it answers 404
/// rather than hand a caller a session id it may not read. The filtered
/// listing lives in [`scan_sessions_async`].
pub fn find_latest_session(conversations_dir: &Path) -> Option<String> {
    session_meta_store(conversations_dir)
        .ok()?
        .find_latest()
        .ok()
        .flatten()
        .map(|m| m.session_id)
}

/// Asynchronously scan all sessions from the session-meta store.
///
/// Returns a paginated list of `SessionInfo` sorted by `last_active_at`
/// descending (newest first). ADR-082 §4 step 3: the store is SQLite.
///
/// ADR-076 §决策 4: `scope` is applied *before* pagination (see
/// [`build_session_page`]), so the page slice, `total`, and the token totals
/// all describe only what the caller may read.
///
/// ADR-028: in addition to the page slice, the join handle now also
/// returns `(agent_total_input, agent_total_output, agent_total_cache_read,
/// agent_total_cache_write)` summed across every session on disk
/// (full-scan totals, not just the current page). The caller uses these
/// to bootstrap [`AgentCore`]'s in-process counters via atomic-max merge,
/// and to forward them to the Desktop App as a fallback data source for
/// the agent-total token display.
///
/// ADR-066: the third and fourth tuple elements carry prompt-cache
/// aggregates (Provider-reported).  Old v3 meta files without cache
/// fields contribute `0` to the running totals — same "宁可 miss 也不
/// 估计" policy used elsewhere.
// ADR-066: the four-element token tuple `(input, output, cache_read,
// cache_write)` was widened from the two-element version originally
// added in ADR-028.  Clippy's `type_complexity` lint still flags the
// full return type of `scan_sessions_async` as borderline; the
// intentional escape hatch below preserves the function signature so
// callers can keep tuple-destructuring the result without a custom
// struct.
#[allow(clippy::type_complexity)]
pub fn scan_sessions_async(
    conversations_dir: PathBuf,
    page: Option<u32>,
    size: Option<u32>,
    scope: SessionScope,
) -> tokio::task::JoinHandle<(Vec<SessionInfo>, usize, (u64, u64, u64, u64))> {
    tokio::task::spawn_blocking(move || {
        // ADR-082 §4 step 3: the rows come from the session-meta store
        // (SQLite). `scan_sessions_from_meta` is that store's full-scan form
        // and keeps the `(session_id, SessionMeta)` shape the page builder
        // already takes.
        //
        // The read filter and the slice both stay on this side of the store
        // boundary on purpose: `SessionMeta::is_readable_by` is the *single*
        // definition of the ADR-076 read rule. An equivalent SQL predicate
        // would be a second copy to keep in sync, and a drift between them is
        // a data leak, not a cosmetic bug.
        //
        // ponytail: full scan, then filter + slice in Rust. `total` and the
        // token aggregates are full-scan by contract anyway (ADR-028 /
        // ADR-066), so this is the O(n) the dashboard already pays. Ceiling:
        // every session row is materialised per list request (≈23 ms at 2000
        // rows). Upgrade path: push the predicate into SQL past ~10k sessions
        // per workspace — keeping `is_readable_by` as the reference
        // implementation the SQL would have to match.
        let rows = scan_sessions_from_meta(&conversations_dir);
        build_session_page(&rows, page, size, &scope)
    })
}

/// Slice one page out of an already-scanned row set, aggregating token
/// totals across every readable row.
///
/// Split out of [`scan_sessions_async`] so the body reads the same whether
/// the rows came fresh from disk or out of [`META_INDEX`].
#[allow(clippy::type_complexity)]
fn build_session_page(
    rows: &[(String, SessionMeta)],
    page: Option<u32>,
    size: Option<u32>,
    scope: &SessionScope,
) -> (Vec<SessionInfo>, usize, (u64, u64, u64, u64)) {
    // ADR-076 §决策 4: drop sessions `scope` may not read *before*
    // paginating, so `total` / `total_pages` describe what the caller
    // can actually see. Filtering after the slice would hand back
    // short pages and a page count pointing at invisible rows.
    let sessions: Vec<&(String, SessionMeta)> = rows
        .iter()
        .filter(|(_, meta)| meta.is_readable_by(scope))
        .collect();

    // ADR-028 / ADR-066: full-scan aggregate. Walk every meta file
    // (not just the current page) so a single scan can rebuild the
    // baseline even when `size` is small (e.g. title-only fetches).
    // `None` sessions and sessions with `prompt_tokens == 0`
    // (Provider fallback) are skipped on the input side; the output
    // side is always accumulated (matches
    // `ConversationSession::accumulate_llm_usage`).
    //
    // Cache totals sum directly — `accumulate_llm_usage` already
    // maintains the invariant that `total_cache_read` is only
    // incremented alongside `total_input`, so a session with
    // `total_input == 0` cannot have `total_cache_read > 0`.  We
    // do not need to re-check the zero-skip here.
    //
    // `total_cache_write` is always summed (Anthropic charges 1.25×
    // for `cache_creation_input_tokens` regardless of the regular
    // input count).
    let (agent_total_input, agent_total_output, agent_total_cache_read, agent_total_cache_write) =
        sessions.iter().fold(
            (0u64, 0u64, 0u64, 0u64),
            |(acc_in, acc_out, acc_cr, acc_cw), (_, meta)| {
                let t = meta.tokens.as_ref();
                (
                    acc_in.saturating_add(t.map(|t| t.total_input).unwrap_or(0)),
                    acc_out.saturating_add(t.map(|t| t.total_output).unwrap_or(0)),
                    acc_cr.saturating_add(t.map(|t| t.total_cache_read).unwrap_or(0)),
                    acc_cw.saturating_add(t.map(|t| t.total_cache_write).unwrap_or(0)),
                )
            },
        );

    let total = sessions.len();
    let page = page.unwrap_or(1).max(1) as usize;
    let size = size.unwrap_or(20).max(1) as usize;
    let start = (page - 1) * size;
    let end = (start + size).min(total);

    let infos = sessions[start..end]
        .iter()
        .map(|(sid, meta)| SessionInfo {
            session_id: sid.clone(),
            created_at: meta.created_at.clone(),
            last_active_at: meta.last_active_at.clone(),
            message_count: meta.message_count as u32,
            title: meta.title.clone(),
            corrupted: meta.corrupted,
            model: meta.model.clone(),
            provider: meta.provider.clone(),
            account_id: meta.account_id.clone(),
            workspace_id: meta.workspace_id.clone(),
            visibility: meta.visibility,
            initiator: meta.initiator.clone(),
            can_write: meta.is_writable_by(scope),
        })
        .collect();

    (
        infos,
        total,
        (
            agent_total_input,
            agent_total_output,
            agent_total_cache_read,
            agent_total_cache_write,
        ),
    )
}

/// Read messages from a JSONL file with offset-based pagination.
///
/// - `offset`: how many of the most recent raw entries to skip over.
///   `0` returns the latest `limit` entries; larger values return
///   older windows. The caller (frontend) moves this number back and
///   forth to scroll — there is **no `direction` parameter**, the
///   direction is encoded entirely in the offset arithmetic.
/// - `limit`: maximum number of **raw entries** to return. A raw entry
///   is one JSONL line (one thought / tool_call / tool_result /
///   user / assistant row). A single displayed "explore group" may
///   count as multiple raw entries depending on how many
///   thoughts/tool_calls/tool_results it contains. `display group`
///   collapsing (think + tool_call + tool_result → 1 chip) is a
///   **pure-frontend UI abstraction** — see `displayMessages` in
///   `ChatPanel.tsx`. The backend never reasons about groups.
///
/// # Why raw entries and not groups?
///
/// `offset` and `limit` are both measured in raw entries so they share
/// a single dimension and arithmetic `next_offset = prev_offset +
/// prev_limit` is always correct. Group-aware pagination (slicing by
/// display groups) would require every protocol hop to recompute group
/// boundaries — a hidden coupling that reintroduces the partial-cut
/// bug family (a mega-explore group straddling a window boundary).
///
/// Implementation: a per-path line index (see the "Log index" section) maps
/// the requested window to byte spans, and only those spans are read with
/// seek + positional reads. The index is reconciled incrementally — the one
/// full byte pass happens at first touch per session per process (open
/// calibration), never per request.
///
/// Returns messages in chronological order (oldest → newest within
/// the page).
pub fn read_messages_paginated(
    path: &Path,
    offset: u64,
    limit: u32,
    from_tail: bool,
) -> Result<PaginatedMessages> {
    // ADR-024: no metadata header line on new files; data lives in a
    // sidecar (see `<sid>.json`). A legacy v1 header line at line 0 is
    // classified (and stripped at open calibration) so it never enters
    // pagination; unlike the old reader we do NOT re-test every line for
    // the markers, so a data line whose content mentions `"version"` /
    // `"session_id"` now correctly counts toward `total`.
    //
    // ADR-050: pagination uses **forward** (oldest-end) offsets by default.
    //   `offset = 0`  → the OLDEST `limit` entries.
    //   `offset = K`  → entries `[K, K+limit)` clamped to `total`.
    //   `from_tail = true` → force the window to the LAST `limit` entries
    //     (overrides `offset`); used by the initial-load code path which
    //     doesn't know `total` yet.
    let file_len = std::fs::metadata(path)?.len();
    if file_len == 0 {
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            offset,
            limit: 0,
            total: 0,
        });
    }

    let index = log_index_for(path);
    let mut idx = index.lock().unwrap();
    // Incremental tail scan; a full pass only happens on first touch.
    reconcile_log(path, &mut idx)?;

    let total = idx.total_data;

    // If `offset` skips past the end, the page is empty (harmless boundary).
    // `from_tail` is rejected for empty files — total=0 means there's nothing
    // at the tail either.
    if total == 0 {
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            offset,
            limit: 0,
            total: 0,
        });
    }

    // Compute the effective [start_idx, end_idx) window.
    //
    // Forward semantics:
    //   start_idx = offset.min(total)
    //   end_idx   = (offset + limit).min(total)
    //
    // `from_tail` overrides: clamp `start_idx` so `end_idx` lands on the
    // last `limit` (or fewer) entries.
    let (effective_offset, start_idx, end_idx) = if from_tail {
        // Walk backwards from the tail by `limit` entries (or all of them
        // when the file holds fewer than `limit`).
        let tail_start = total.saturating_sub(limit as u64);
        let echo_offset = tail_start; // What the caller "should have asked for"
        (echo_offset, tail_start, total)
    } else {
        let start = offset.min(total);
        let end = (offset + limit as u64).min(total);
        (offset, start, end)
    };

    if start_idx >= end_idx {
        return Ok(PaginatedMessages {
            messages: Vec::new(),
            offset: effective_offset,
            limit: 0,
            total,
        });
    }

    // Map the data-coordinate window onto byte spans and read ONLY those.
    let mut messages: Vec<ConversationEntry> = Vec::new();
    let mut file = std::fs::File::open(path)?;
    for span in idx.spans.iter() {
        let Some(di) = span.data_index else { continue };
        if di < start_idx {
            continue;
        }
        if di >= end_idx {
            break;
        }
        let Some(text) = read_log_span_text(&mut file, span)? else {
            continue;
        };
        match serde_json::from_str::<ConversationEntry>(&text) {
            Ok(e) => messages.push(e),
            Err(e) => tracing::warn!("Skipping invalid JSONL line: {}", e),
        }
    }
    drop(idx); // release the index before the (allocating) return path

    let actual_limit = messages.len() as u32;

    Ok(PaginatedMessages {
        messages,
        offset: effective_offset,
        limit: actual_limit,
        total,
    })
}

// ── ADR-021: Incremental read with line-number coordinates ─────────────────

/// Read the first persisted `user` entry of a session's JSONL log.
///
/// Session-title generation uses this instead of the current turn's message:
/// the title must always describe the conversation's FIRST user turn. After a
/// Stop the async title task may still be running, and a follow-up message
/// typed into the same session must never become the title — neither through
/// a second generation task nor through a session restored from disk (a
/// rebuilt session has no in-memory title cache).
///
/// Internal runtime artifacts (output-budget nudges and the like) are
/// skipped — they are not user input. `None` when the log has no user row yet.
pub fn read_first_user_message(path: &Path) -> Option<String> {
    // The first user row sits at the head of the log; a couple of pages cover
    // the session-init / system rows that may precede it. Pages are only
    // walked when a log is unusually header-heavy.
    const PAGE: u32 = 16;
    const MAX_PAGES: u8 = 32;
    let mut offset = 0u64;
    for _ in 0..MAX_PAGES {
        let page = read_messages_paginated(path, offset, PAGE, false).ok()?;
        if let Some(entry) = page
            .messages
            .into_iter()
            .find(|e| e.role == "user" && !e.is_internal())
        {
            return Some(entry.content);
        }
        if page.limit == 0 {
            return None;
        }
        offset += u64::from(page.limit);
        if offset >= page.total {
            return None;
        }
    }
    None
}

/// Read messages from a JSONL file since a given line-number coordinate.
///
/// ADR-021: This is the Runtime-side handler for incremental poll requests.
/// It returns:
/// - `messages`: new complete lines from JSONL with index >= `line_number`
/// - `streaming`: delta of the in-progress streaming line (if any)
/// - `total_lines`: current total line count in the JSONL file
///
/// # Arguments
/// - `path`: Path to the session JSONL file.
/// - `line_number`: Number of complete lines already read by the frontend
///   (a COUNT, not an index). The function returns lines with index >= line_number.
/// - `line_char_offset`: Number of characters already read from the streaming
///   line. The function returns only new characters after this offset.
/// - `streaming_lines`: Shared StreamingStateMap for in-progress lines.
/// - `session_id`: Session ID to look up in `streaming_lines`.
///
/// # Clamping
/// If `line_number` exceeds `total_lines` (e.g., JSONL was externally
/// truncated), it is clamped to `total_lines`. Similarly, if the streaming
/// line's `char_offset` is less than the requested `line_char_offset`
/// (should not happen in normal operation), the full content is returned.
pub fn read_messages_since(
    path: &Path,
    line_number: usize,
    line_char_offset: usize,
    streaming_lines: &StreamingStateMap,
    session_id: &str,
    cached_total_lines: usize,
) -> Result<ReadMessagesSinceResult> {
    // ADR-022: Use `cached_total_lines` (committed_lines from the writer
    // thread) as the authoritative count.  Unlike the old `total_lines`
    // which was incremented before the async disk write, `committed_lines`
    // is only incremented AFTER the write — so it never lies about what's
    // actually on disk. On cold paths (counter not yet populated) the log
    // index provides the count — first touch reconciles once per session
    // per process, later calls only scan appended bytes.
    let total_lines = if cached_total_lines > 0 {
        cached_total_lines
    } else {
        physical_line_count(path)
    };

    // Clamp line_number to total_lines (defensive against external truncation)
    let line_number = line_number.min(total_lines);

    // Read new complete lines from JSONL (lines with index >= line_number).
    // line_number is a COUNT (number of lines already read by the frontend),
    // so the first unread line is at index line_number (0-based). Like the
    // old reader this is unbounded above: lines the writer committed after
    // `total_lines` was sampled are still delivered in this pass.
    let mut messages: Vec<ConversationEntry> = Vec::new();
    let index = log_index_for(path);
    if let Ok(mut idx) = index.lock()
        && reconcile_log(path, &mut idx).is_ok()
        && line_number < idx.spans.len()
        && let Ok(mut file) = std::fs::File::open(path)
    {
        for span in &idx.spans[line_number..] {
            let Ok(Some(text)) = read_log_span_text(&mut file, span) else {
                continue;
            };
            if let Ok(entry) = serde_json::from_str::<ConversationEntry>(&text) {
                messages.push(entry);
            }
        }
    }

    // Read streaming line delta — clone under read lock, compute delta outside lock
    // to minimize write-lock contention from concurrent Delta appends.
    let streaming = {
        let map = streaming_lines.read().unwrap();
        map.get(session_id).map(|sl| StreamingLine {
            line_number: sl.line_number,
            role: sl.role.clone(),
            message_id: sl.message_id.clone(),
            accumulated_content: sl.accumulated_content.clone(),
            started_at: sl.started_at.clone(),
            started_at_ms: sl.started_at_ms,
        })
    };
    let streaming = streaming.map(|sl| {
        let current_len = sl.accumulated_content.chars().count();
        // ADR-022: Detect stale `line_char_offset` from a previous streaming
        // line that has since been flushed to JSONL (role transition triggers
        // flush + new line). Applying a stale offset here would skip the
        // beginning of this fresh line and permanently drop its opening
        // characters.
        //
        // Case 1: `line_number < sl.line_number` — the frontend's line number
        //   is behind, so the offset belongs to a previous (now flushed) line.
        // Case 2: `line_char_offset > current_len` — the offset exceeds the
        //   current content length, which is impossible for this line (the
        //   offset is always set to `current_len` in our response). It must
        //   be from a previous (longer) line that was flushed.
        // In both cases, reset to 0 to return the full content of this line.
        let effective_offset = if line_number < sl.line_number || line_char_offset > current_len {
            0
        } else {
            line_char_offset
        };
        let delta_content: String = sl
            .accumulated_content
            .chars()
            .skip(effective_offset)
            .collect();
        StreamingLineDelta {
            line: sl.line_number,
            role: sl.role,
            content: delta_content,
            char_offset: current_len,
        }
    });

    Ok(ReadMessagesSinceResult {
        messages,
        streaming,
        total_lines,
    })
}

/// Backend-cursor variant of [`read_messages_since`].
///
/// Instead of receiving `line_number` / `line_char_offset` from the frontend,
/// this function takes a [`DeliveryCursor`] that the backend maintains
/// per-session.  It returns at most `limit` complete JSONL lines, plus the
/// streaming delta, plus a `has_more` flag for batch catch-up.
///
/// ## Batch delivery
///
/// When the cursor lags far behind `total_lines` (e.g. the session was in
/// the background for minutes), this function returns at most `limit` lines
/// and sets `has_more = true`.  The caller should immediately poll again
/// until `has_more` is false.
///
/// ## Streaming delta
///
/// The streaming delta is always returned (even during batch catch-up) so
/// the frontend can update the streaming placeholder incrementally.
///
/// ## Stale char_offset detection
///
/// If `cursor.line_number < streaming.line_number`, the `char_offset` belongs
/// to a previous (now flushed) streaming line and is reset to 0 — same logic
/// as [`read_messages_since`].
pub fn read_messages_since_cursor(
    path: &Path,
    cursor: DeliveryCursor,
    limit: usize,
    streaming_lines: &StreamingStateMap,
    session_id: &str,
    cached_total_lines: usize,
) -> Result<ReadMessagesSinceCursorResult> {
    let total_lines = if cached_total_lines > 0 {
        cached_total_lines
    } else {
        physical_line_count(path)
    };

    // Clamp cursor.line_number to total_lines (defensive against truncation).
    let start = cursor.line_number.min(total_lines);
    let end = (start + limit).min(total_lines);

    // Read complete lines [start..end) via the log index (seek per span,
    // no head-skip enumeration).
    let mut messages: Vec<ConversationEntry> = Vec::new();
    if start < end {
        let index = log_index_for(path);
        if let Ok(mut idx) = index.lock()
            && reconcile_log(path, &mut idx).is_ok()
            && let Ok(mut file) = std::fs::File::open(path)
        {
            let upper = end.min(idx.spans.len());
            for span in &idx.spans[start.min(upper)..upper] {
                let Ok(Some(text)) = read_log_span_text(&mut file, span) else {
                    continue;
                };
                if let Ok(entry) = serde_json::from_str::<ConversationEntry>(&text) {
                    messages.push(entry);
                }
            }
        }
    }

    let new_line_number = end;
    let has_more = new_line_number < total_lines;

    // Read streaming line delta.
    //
    // Only return streaming delta when the cursor has caught up to the
    // streaming line (new_line_number >= streaming.line_number).  During
    // batch catch-up (cursor behind streaming line), the delta is not
    // useful — the frontend is processing complete lines and the streaming
    // content will be delivered in full once the cursor catches up.
    let streaming = {
        let map = streaming_lines.read().unwrap();
        map.get(session_id).map(|sl| StreamingLine {
            line_number: sl.line_number,
            role: sl.role.clone(),
            message_id: sl.message_id.clone(),
            accumulated_content: sl.accumulated_content.clone(),
            started_at: sl.started_at.clone(),
            started_at_ms: sl.started_at_ms,
        })
    };
    let streaming = streaming.and_then(|sl| {
        // Only return delta if cursor has caught up to this streaming line.
        if new_line_number < sl.line_number {
            return None;
        }
        let current_len = sl.accumulated_content.chars().count();
        // If cursor was behind the streaming line before reading complete
        // lines, the char_offset belongs to a previous (flushed) streaming
        // line and must be reset to 0.
        let effective_offset =
            if cursor.line_number < sl.line_number || cursor.char_offset > current_len {
                0
            } else {
                cursor.char_offset
            };
        let delta_content: String = sl
            .accumulated_content
            .chars()
            .skip(effective_offset)
            .collect();
        Some(StreamingLineDelta {
            line: sl.line_number,
            role: sl.role,
            content: delta_content,
            char_offset: current_len,
        })
    });

    let new_char_offset = streaming.as_ref().map(|s| s.char_offset).unwrap_or(0);

    Ok(ReadMessagesSinceCursorResult {
        messages,
        streaming,
        total_lines,
        has_more,
        new_cursor: DeliveryCursor {
            line_number: new_line_number,
            char_offset: new_char_offset,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn meta_header_detector_never_matches_a_real_entry() {
        // The v1 metadata header carries both marker tokens and neither of the
        // fields a real `ConversationEntry` always serializes.
        assert!(looks_like_meta_header(br#"{"version":1,"session_id":"sess-1"}"#));
        // A genuine first message whose *body* happens to embed both marker
        // tokens must NOT be classified as a header: `strip_legacy_header_line`
        // physically rewrites the file, so a false positive would delete a real
        // row (the data-loss guard for ADR-096 open calibration).
        let real = r#"{"id":"x","ts":"2026-01-01T00:00:00.000Z","role":"assistant","content":"{\"version\":1,\"session_id\":\"sess-1\"}","metadata":null}"#;
        assert!(!looks_like_meta_header(real.as_bytes()));
    }

    #[test]
    fn test_generate_session_id() {
        let id = generate_session_id();
        // Format: YYYYMMDD_HHMMSS_xxxxxx (6-char short UUID)
        let parts: Vec<&str> = id.split('_').collect();
        assert_eq!(
            parts.len(),
            3,
            "Session ID should have 3 parts separated by underscores"
        );
        assert_eq!(parts[0].len(), 8, "Date part should be 8 chars (YYYYMMDD)");
        assert_eq!(parts[1].len(), 6, "Time part should be 6 chars (HHMMSS)");
        assert_eq!(parts[2].len(), 6, "Short UUID should be 6 chars");
        assert!(
            parts[0].chars().all(|c| c.is_ascii_digit()),
            "Date should be digits"
        );
        assert!(
            parts[1].chars().all(|c| c.is_ascii_digit()),
            "Time should be digits"
        );
    }

    #[test]
    fn test_conversation_writer_basic() {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = generate_session_id();
        let agent_id = "com.test.agent";

        // Create session and write messages
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            &session_id,
            SessionConfig {
                agent_id: agent_id.to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "Hello", None);
        session.append_message(
            "assistant",
            "Hi there!",
            Some(serde_json::json!({"model": "test-model"})),
        );
        session.append_message("tool_call", r#"{"path": "test.txt"}"#, None);

        // Give writer thread time to process
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Close session
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });

        // Verify file contents
        let file_path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 3, "ADR-024: 3 data lines, no metadata header");

        // First line is user message
        let entry: ConversationEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry.role, "user");
        assert_eq!(entry.content, "Hello");
        assert!(entry.metadata.is_none());

        // Second line is assistant message
        let entry: ConversationEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry.role, "assistant");
        assert_eq!(entry.content, "Hi there!");
        assert_eq!(
            entry.metadata,
            Some(serde_json::json!({"model": "test-model"}))
        );

        // Third line is tool_call
        let entry: ConversationEntry = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(entry.role, "tool_call");
        assert_eq!(entry.content, r#"{"path": "test.txt"}"#);

        // Verify the meta row exists in the session-meta store (SQLite).
        let meta = read_session_meta(&work_dir.join("conversations"), &session_id)
            .expect("Per-session meta row must exist");
        assert_eq!(meta.version, CONVERSATION_FORMAT_VERSION);
        assert_eq!(meta.session_id, session_id);
        assert_eq!(meta.agent_id, agent_id);
    }

    /// ADR-076 §决策 4: `initiator` is an additive, optional field — meta
    /// files written before the account system load as `None` (legacy /
    /// local mode), and a session with an owner round-trips.
    #[test]
    fn session_meta_initiator_is_backward_compatible() {
        let legacy = r#"{"version":1,"session_id":"s-1","agent_id":"a","created_at":"t",
            "message_count":0,"last_active_at":"t","corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.initiator, None);

        let mut owned = meta.clone();
        owned.initiator = Some("u-alice".into());
        let json = serde_json::to_string(&owned).unwrap();
        assert!(json.contains("u-alice"));
        let back: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.initiator.as_deref(), Some("u-alice"));

        // An unowned session must not carry the key at all (the disk
        // format stays byte-identical to pre-ADR-076 for local mode).
        assert!(!serde_json::to_string(&meta).unwrap().contains("initiator"));

        // A sidecar file written by the `user_id` era still loads: the
        // rename is a vocabulary change, not a data migration.
        let old_name = r#"{"version":1,"session_id":"s-2","agent_id":"a","created_at":"t",
            "message_count":0,"last_active_at":"t","corrupted":false,"user_id":"u-bob"}"#;
        let meta: SessionMeta = serde_json::from_str(old_name).unwrap();
        assert_eq!(meta.initiator.as_deref(), Some("u-bob"));
    }

    /// ADR-076 §决策 4: `visibility` is absent-on-disk by default and
    /// **absent means public** — upgrading must not retroactively hide
    /// every pre-existing session from its user.
    #[test]
    fn session_meta_visibility_is_absent_by_default_and_means_public() {
        let legacy = r#"{"version":1,"session_id":"s-1","agent_id":"a","created_at":"t",
            "message_count":0,"last_active_at":"t","corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(legacy).unwrap();
        assert_eq!(meta.visibility, None);
        assert!(!meta.visibility.is_some_and(SessionVisibility::is_private));

        // Explicitly private round-trips, and explicitly public is
        // serializable too.
        for (v, wire) in [
            (SessionVisibility::Private, "\"private\""),
            (SessionVisibility::Public, "\"public\""),
        ] {
            let mut m = meta.clone();
            m.visibility = Some(v);
            let json = serde_json::to_string(&m).unwrap();
            assert!(json.contains(wire), "{json} should contain {wire}");
            assert_eq!(
                serde_json::from_str::<SessionMeta>(&json).unwrap().visibility,
                Some(v)
            );
        }

        // Unset visibility leaves no key behind — the disk format stays
        // byte-identical to pre-ADR-076.
        assert!(!serde_json::to_string(&meta).unwrap().contains("visibility"));
    }

    /// `set_visibility` persists, is idempotent, and clearing it restores
    /// the public default (ADR-076 §决策 4).
    #[test]
    fn set_visibility_persists_and_clears_back_to_public() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-meta-visibility-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, _cfg, _state) = ConversationSession::new(
            &dir,
            "visibility",
            SessionConfig {
                agent_id: "a".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            10,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();

        assert!(!session.is_private(), "default must be public");

        session.set_visibility(Some(SessionVisibility::Private));
        assert!(session.is_private());
        assert!(session.build_meta().visibility.is_some_and(|v| v.is_private()));

        // Persisted, not just in memory.
        let on_disk = read_session_meta(&dir.join("conversations"), "visibility").unwrap();
        assert_eq!(on_disk.visibility, Some(SessionVisibility::Private));

        // Clearing restores the public default.
        session.set_visibility(None);
        assert!(!session.is_private());
        let on_disk = read_session_meta(&dir.join("conversations"), "visibility").unwrap();
        assert_eq!(on_disk.visibility, None);
    }

    /// `set_initiator` is the single writer and is immutable after the
    /// first call (ADR-076 §决策 4).
    #[test]
    fn set_initiator_is_write_once() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-meta-owner-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (session, _cfg, _state) = ConversationSession::new(
            &dir,
            "owned",
            SessionConfig {
                agent_id: "a".into(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            10,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();

        session.set_initiator(Some("u-alice".into()));
        assert_eq!(session.initiator().as_deref(), Some("u-alice"));
        session.set_initiator(Some("u-bob".into()));
        assert_eq!(session.initiator().as_deref(), Some("u-alice"));
        assert_eq!(session.build_meta().initiator.as_deref(), Some("u-alice"));
    }

    /// Seed a session-meta row plus an empty JSONL so the prune path has
    /// something to archive.  Avoids spinning up a `ConversationSession` (and
    /// its background writer thread) for what is a pure store-layout test.
    fn seed_session(conv: &Path, sid: &str, owner: Option<&str>, last_active_at: &str) {
        std::fs::create_dir_all(conv).unwrap();
        let meta = SessionMeta {
            version: 1,
            session_id: sid.to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-09-01T00:00:00Z".to_string(),
            initiator: owner.map(str::to_string),
            visibility: None,
            title: None,
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 0,
            last_active_at: last_active_at.to_string(),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        chat_session_id: None,
            model_usage: None,
        };
        std::fs::write(conv.join(format!("{}.jsonl", sid)), "").unwrap();
        write_session_meta(conv, &meta).unwrap();
    }

    #[test]
    fn prune_excess_sessions_is_per_owner_not_per_agent() {
        // ADR-076 §决策 4: the cap is per **owner**. Before the account
        // system an agent had exactly one user, so a global cap was the
        // same thing; with a shared agent a global cap lets one account's
        // activity archive another account's history.
        //
        // Alice's sessions are deliberately the OLDEST files on disk: the
        // pre-ADR-076 global cap would prune hers first, which is exactly
        // the bug this test pins down.
        let temp_dir = TempDir::new().unwrap();
        let conv = temp_dir.path().join("conversations");

        for i in 0..2 {
            seed_session(
                &conv,
                &format!("alice_{i}"),
                Some("u-alice"),
                &format!("2026-09-01T00:00:0{i}Z"),
            );
        }
        for i in 0..3 {
            seed_session(
                &conv,
                &format!("bob_{i}"),
                Some("u-bob"),
                &format!("2026-09-02T00:00:0{i}Z"),
            );
        }
        for i in 0..3 {
            seed_session(
                &conv,
                &format!("legacy_{i}"),
                None,
                &format!("2026-09-03T00:00:0{i}Z"),
            );
        }

        // 8 sessions total, cap 2 → only the two over-cap buckets shed one
        // session each. A global cap would have pruned 6.
        assert_eq!(prune_excess_sessions(&conv, 2), 2);

        // Alice is at the cap → untouched, even though she owns the oldest
        // files. This is the assertion the old implementation failed.
        for i in 0..2 {
            assert!(conv.join(format!("alice_{i}.jsonl")).exists());
            assert!(session_meta_exists(&conv, &format!("alice_{i}")));
        }

        // Bob is over the cap → exactly his oldest session is archived.
        assert!(conv.join("bob_0.jsonl.archive").exists());
        assert!(!conv.join("bob_0.jsonl").exists());
        assert!(!session_meta_exists(&conv, "bob_0"));
        for i in 1..3 {
            assert!(conv.join(format!("bob_{i}.jsonl")).exists());
            assert!(session_meta_exists(&conv, &format!("bob_{i}")));
        }

        // Ownerless sessions (pre-ADR-076 / local mode) form one bucket and
        // keep the pre-existing semantics.
        assert!(conv.join("legacy_0.jsonl.archive").exists());
        assert!(!session_meta_exists(&conv, "legacy_0"));
        assert!(conv.join("legacy_2.jsonl").exists());

        // Nothing is over a cap of 100.
        assert_eq!(prune_excess_sessions(&conv, 100), 0);
    }

    /// `delete_session_meta` must drop the store row, so the next listing no
    /// longer advertises a session whose history is gone.
    #[test]
    fn delete_session_meta_drops_the_session_from_the_listing() {
        let temp_dir = TempDir::new().unwrap();
        let conv = temp_dir.path().join("conversations");
        seed_session(&conv, "s-keep", None, "2026-01-01T00:00:00Z");
        seed_session(&conv, "s-drop", None, "2026-01-02T00:00:00Z");
        assert_eq!(scan_sessions_from_meta(&conv).len(), 2);

        delete_session_meta(&conv, "s-drop").unwrap();

        let ids: Vec<String> = scan_sessions_from_meta(&conv)
            .into_iter()
            .map(|(sid, _)| sid)
            .collect();
        assert_eq!(ids, vec!["s-keep".to_string()]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "must not live inside")]
    fn doubled_conversations_dir_trips_the_session_meta_guard() {
        // ADR-076 regression: `POST /sessions/{sid}/open` passed an already
        // joined `…/conversations` dir to a helper that joined it again, so
        // the store opened (and filled) `…/conversations/memory/private.sqlite`
        // — a second, empty DB next to the JSONL logs.
        let temp_dir = TempDir::new().unwrap();
        let conv = temp_dir.path().join("conversations");
        assert_eq!(
            session_meta_db_path(&conv),
            temp_dir.path().join("memory").join("private.sqlite")
        );
        session_meta_db_path(&conv.join("conversations"));
    }

    #[test]
    fn write_meta_skips_when_jsonl_deleted() {
        // Regression: `ConversationSession::write_meta` used to rewrite the
        // session meta unconditionally. A detached distill task holding an
        // `Arc<ConversationSession>` could therefore re-create a meta row for
        // a session whose JSONL had already been deleted by
        // `SessionManager::delete_session`. On the next startup the orphan row
        // was scanned as the "latest" session and resume failed because the
        // JSONL was gone. Fix: `write_meta` returns early when the JSONL file
        // no longer exists.
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = generate_session_id();

        let (session, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            &session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();

        let conversations_dir = work_dir.join("conversations");
        let jsonl_path = conversations_dir.join(format!("{}.jsonl", session_id));

        // Pre-condition: the JSONL exists and the meta row is present.
        assert!(jsonl_path.exists());
        assert!(session_meta_exists(&conversations_dir, &session_id));

        // Shut down the writer so the JSONL handle is released (required
        // to delete the file on Windows), then simulate the delete path:
        // `SessionManager::delete_session` removes BOTH the JSONL and the
        // meta row. The guard must then prevent `write_meta` from
        // re-creating the orphan row.
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });
        std::fs::remove_file(&jsonl_path).unwrap();
        delete_session_meta(&conversations_dir, &session_id).unwrap();
        assert!(!jsonl_path.exists());
        assert!(!session_meta_exists(&conversations_dir, &session_id));

        // The guard must prevent the meta row from being re-created.
        session.write_meta();
        assert!(
            !session_meta_exists(&conversations_dir, &session_id),
            "write_meta must not re-create a meta row once its JSONL is gone"
        );
    }

    #[test]
    fn test_find_latest_session() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // ADR-082 §4 step 3: find_latest_session reads the SQLite meta store.
        let base = chrono::Utc::now();
        let ids = vec![
            (
                "20260503_100000_aaaaaa",
                (base - chrono::Duration::hours(3))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
            (
                "20260503_120000_bbbbbb",
                base.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), // newest
            ),
            (
                "20260503_110000_cccccc",
                (base - chrono::Duration::hours(1))
                    .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            ),
        ];
        for (id, ts) in &ids {
            let meta = SessionMeta {
                version: CONVERSATION_FORMAT_VERSION,
                session_id: id.to_string(),
                agent_id: "com.test".to_string(),
                created_at: ts.clone(),
                initiator: None,
                visibility: None,
                title: None,
                workspace_id: None,
                model: None,
                provider: None,
                account_id: None,
                reasoning_effort: None,
                temperature: None,
                context_window: None,
                todos: None,
                message_count: 0,
                last_active_at: ts.clone(),
                tokens: None,
                llm_call_counter: None,
            perf: None,
                model_ratio: None,
                last_compaction_offset: None,
                corrupted: false,
            chat_session_id: None,
                model_usage: None,
            };
            write_session_meta(&conv_dir, &meta).unwrap();
        }

        let latest = find_latest_session(&conv_dir);
        assert_eq!(latest, Some("20260503_120000_bbbbbb".to_string()));
    }

    #[test]
    fn test_read_messages_paginated() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        let session_id = "20260503_100000_test01";
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));

        // ADR-024: write 5 messages directly (no metadata header).
        {
            let mut file = std::fs::File::create(&file_path).unwrap();

            for i in 0..5 {
                let entry = ConversationEntry {
                    id: format!("msg-{}", i),
                    ts: chrono::Utc::now().to_rfc3339(),
                    role: if i % 2 == 0 { "user" } else { "assistant" }.to_string(),
                    content: format!("Message {}", i),
                    metadata: None,
                    kind: None,
                };
                serde_json::to_writer(&mut file, &entry).unwrap();
                writeln!(file).unwrap();
            }
        }

        // Page 1: offset=0, limit=10 → first 5 (full conversation).
        let page = read_messages_paginated(&file_path, 0, 10, false).unwrap();
        assert_eq!(page.offset, 0);
        assert_eq!(page.limit, 5);
        assert_eq!(page.total, 5);
        assert_eq!(page.messages.len(), 5);
        assert_eq!(page.messages[0].content, "Message 0");
        assert_eq!(page.messages[4].content, "Message 4");

        // Page 2: offset=0, limit=2 → oldest 2 messages (Message 0, 1).
        let page = read_messages_paginated(&file_path, 0, 2, false).unwrap();
        assert_eq!(page.offset, 0);
        assert_eq!(page.limit, 2);
        assert_eq!(page.total, 5);
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.messages[0].content, "Message 0");
        assert_eq!(page.messages[1].content, "Message 1");

        // Page 3: offset=2, limit=2 → entries [2, 4) (Message 2, 3).
        let page2 = read_messages_paginated(&file_path, 2, 2, false).unwrap();
        assert_eq!(page2.offset, 2);
        assert_eq!(page2.limit, 2);
        assert_eq!(page2.total, 5);
        assert_eq!(page2.messages.len(), 2);
        assert_eq!(page2.messages[0].content, "Message 2");
        assert_eq!(page2.messages[1].content, "Message 3");

        // Page 4: offset=4, limit=2 → entries [4, 6) clamped to total → Message 4 only.
        let page3 = read_messages_paginated(&file_path, 4, 2, false).unwrap();
        assert_eq!(page3.offset, 4);
        assert_eq!(page3.limit, 1);
        assert_eq!(page3.total, 5);
        assert_eq!(page3.messages.len(), 1);
        assert_eq!(page3.messages[0].content, "Message 4");

        // Page 5: offset past the end → empty page, offset/total still meaningful.
        let out_of_range = read_messages_paginated(&file_path, 10, 2, false).unwrap();
        assert_eq!(out_of_range.offset, 10);
        assert_eq!(out_of_range.limit, 0);
        assert_eq!(out_of_range.total, 5);
        assert!(out_of_range.messages.is_empty());

        // Page 6: offset=0, limit=3 → first 3 messages (Message 0, 1, 2).
        let first3 = read_messages_paginated(&file_path, 0, 3, false).unwrap();
        assert_eq!(first3.offset, 0);
        assert_eq!(first3.limit, 3);
        assert_eq!(first3.total, 5);
        assert_eq!(first3.messages[0].content, "Message 0");
        assert_eq!(first3.messages[1].content, "Message 1");
        assert_eq!(first3.messages[2].content, "Message 2");

        // from_tail=true, limit=3 → latest 3 messages (Message 2, 3, 4).
        // Mirrors the pre-ADR-050 "latest 3" page for regression coverage.
        let tail3 = read_messages_paginated(&file_path, 0, 3, true).unwrap();
        assert_eq!(tail3.offset, 2);
        assert_eq!(tail3.limit, 3);
        assert_eq!(tail3.total, 5);
        assert_eq!(tail3.messages[0].content, "Message 2");
        assert_eq!(tail3.messages[1].content, "Message 3");
        assert_eq!(tail3.messages[2].content, "Message 4");

        // from_tail=true with limit > total → full conversation.
        let tail_all = read_messages_paginated(&file_path, 0, 10, true).unwrap();
        assert_eq!(tail_all.offset, 0);
        assert_eq!(tail_all.limit, 5);
        assert_eq!(tail_all.messages[0].content, "Message 0");
        assert_eq!(tail_all.messages[4].content, "Message 4");

        // Symmetric scroll-newer step: from offset=2 (got Message 2,3), ask
        // for offset=4 to load Message 4.
        let next = read_messages_paginated(&file_path, 4, 2, false).unwrap();
        assert_eq!(next.messages[0].content, "Message 4");

        // Empty file → empty page with zeroed totals.
        std::fs::write(&file_path, "").unwrap();
        let empty = read_messages_paginated(&file_path, 0, 10, false).unwrap();
        assert_eq!(empty.offset, 0);
        assert_eq!(empty.limit, 0);
        assert_eq!(empty.total, 0);
        assert!(empty.messages.is_empty());
    }

    /// §6: the seek/`data_index` windowing must be a faithful re-implementation
    /// of the old forward-parse reader. On a 10k-line log every data window
    /// equals the matching slice of one full sequential read, and a `from_tail`
    /// window equals the last `limit` entries with the correct echoed offset.
    #[test]
    fn read_messages_paginated_forward_tail_equivalence_large() {
        let dir = TempDir::new().unwrap();
        const N: u64 = 10_000;
        let entries: Vec<ConversationEntry> = (0..N)
            .map(|i| make_entry(&format!("msg-{i}"), "user", &format!("Message {i}")))
            .collect();
        let path = write_test_jsonl(&dir, "big", &entries);

        // Ground truth: one full forward read (bounded by total, not the log).
        let all = read_messages_paginated(&path, 0, N as u32, false).unwrap();
        assert_eq!(all.total, N);
        assert_eq!(all.messages.len() as u64, N);
        let contents: Vec<String> = all.messages.iter().map(|m| m.content.clone()).collect();

        // Forward windows at assorted (offset, limit) equal the ground slice.
        for (off, lim) in [
            (0u64, 1u32),
            (0, 100),
            (9_999, 1),
            (4_990, 20),
            (9_000, 2_000), // clamps to the last 1_000
            (12_000, 50),   // past the end -> empty
        ] {
            let page = read_messages_paginated(&path, off, lim, false).unwrap();
            let start = off.min(N) as usize;
            let end = (off + lim as u64).min(N) as usize;
            let got: Vec<String> = page.messages.iter().map(|m| m.content.clone()).collect();
            assert_eq!(got, contents[start..end], "forward off={off} lim={lim}");
            assert_eq!(page.total, N);
        }

        // `from_tail` windows equal the last `lim` entries; the echoed offset
        // is the forward offset the caller "should have asked for".
        for lim in [1u32, 20, 500, 10_000, 20_000] {
            let page = read_messages_paginated(&path, 0, lim, true).unwrap();
            let want_start = N.saturating_sub(lim as u64) as usize;
            let got: Vec<String> = page.messages.iter().map(|m| m.content.clone()).collect();
            assert_eq!(got, contents[want_start..], "from_tail lim={lim}");
            assert_eq!(page.offset as usize, want_start);
            assert_eq!(page.total, N);
        }
    }

    /// §7: the incremental tail reader resolves line-number coordinates against
    /// the byte-offset index rather than re-scanning from the head. The cold
    /// path (writer counter absent) reconciles once via the log index; the warm
    /// path uses the caller's cached count. Both return the identical trailing
    /// window, and a coordinate past the end clamps to empty.
    #[test]
    fn read_messages_since_uses_byte_cursor_cold_and_warm() {
        let dir = TempDir::new().unwrap();
        const N: usize = 50;
        let entries: Vec<ConversationEntry> = (0..N)
            .map(|i| make_entry(&format!("m{i}"), "user", &format!("Message {i}")))
            .collect();
        let path = write_test_jsonl(&dir, "tail", &entries);
        let empty_map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        for line_number in [0usize, 1, 20, 49, 50, 100] {
            // Warm (cached count present) and cold (cached = 0) must agree.
            let warm = read_messages_since(&path, line_number, 0, &empty_map, "tail", N).unwrap();
            let cold = read_messages_since(&path, line_number, 0, &empty_map, "tail", 0).unwrap();
            assert_eq!(warm.total_lines, N, "warm physical count");
            assert_eq!(cold.total_lines, N, "cold physical count via log index");

            let want: Vec<String> = (line_number.min(N)..N)
                .map(|i| format!("Message {i}"))
                .collect();
            let warm_got: Vec<String> = warm.messages.iter().map(|m| m.content.clone()).collect();
            assert_eq!(warm_got, want, "warm window from line {line_number}");
            let cold_got: Vec<String> = cold.messages.iter().map(|m| m.content.clone()).collect();
            assert_eq!(cold_got, want, "cold window from line {line_number}");
            assert!(warm.streaming.is_none(), "no active streaming line");
        }
    }

    #[test]
    fn test_session_resume() {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = "20260503_100000_resume";
        let agent_id = "com.test.resume";

        // Create initial session
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            session_id,
            SessionConfig {
                agent_id: agent_id.to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)), // unlimited in tests
        )
        .unwrap();
        session.append_message("user", "First message", None);
        std::thread::sleep(std::time::Duration::from_millis(50));

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            session.close().await.unwrap();
        });

        // Resume session
        let (resumed, _config_rx2, _state_rx2) =
            ConversationSession::resume(work_dir, session_id, Arc::new(AtomicUsize::new(0)))
                .unwrap();
        assert_eq!(resumed.session_id(), session_id);
        assert_eq!(resumed.agent_id(), agent_id);

        resumed.append_message("assistant", "Resumed response", None);
        std::thread::sleep(std::time::Duration::from_millis(50));

        rt.block_on(async {
            resumed.close().await.unwrap();
        });

        // Verify file has both messages (ADR-024: no metadata header)
        let file_path = work_dir
            .join("conversations")
            .join(format!("{}.jsonl", session_id));
        let content = std::fs::read_to_string(&file_path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 2, "ADR-024: 2 data lines, no metadata header");

        let entry1: ConversationEntry = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(entry1.content, "First message");

        let entry2: ConversationEntry = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(entry2.content, "Resumed response");
    }

    #[test]
    fn test_scan_sessions_includes_corrupted() {
        let temp_dir = TempDir::new().unwrap();
        let conv_dir = temp_dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();

        // ADR-082 §4 step 3: sessions live in the SQLite meta store.
        // Create a valid session with a meta row.
        let valid_id = "20260503_100000_valid";
        let valid_meta = SessionMeta {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: valid_id.to_string(),
            agent_id: "com.test".to_string(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            initiator: None,
            visibility: None,
            title: Some("Valid".to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 0,
            last_active_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        chat_session_id: None,
            model_usage: None,
        };
        write_session_meta(&conv_dir, &valid_meta).unwrap();

        // Create a corrupted session (corrupted flag set).
        let corrupt_id = "20260503_110000_corrupt";
        let corrupt_meta = SessionMeta {
            session_id: corrupt_id.to_string(),
            corrupted: true,
            title: None,
            ..valid_meta.clone()
        };
        write_session_meta(&conv_dir, &corrupt_meta).unwrap();

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (sessions, _total, _agent_totals) =
            rt.block_on(async { scan_sessions_async(conv_dir, None, None, SessionScope::Unfiltered).await.unwrap() });

        assert_eq!(
            sessions.len(),
            2,
            "Should find both valid and corrupted sessions"
        );

        let valid_session = sessions.iter().find(|s| s.session_id == valid_id).unwrap();
        assert!(!valid_session.corrupted);

        let corrupt_session = sessions
            .iter()
            .find(|s| s.session_id == corrupt_id)
            .unwrap();
        assert!(corrupt_session.corrupted);
        // ADR-024: corrupted sessions report whatever the meta file says;
        // title may be None (no recovery inference from JSONL header).
        assert_eq!(corrupt_session.title, None);
    }

    #[test]
    fn test_session_meta_serde_backward_compatible() {
        // ADR-024: SessionMeta uses `last_active_at` (new field) instead of
        // `updated_at`.  Old fields like `message_count` use `u64` with
        // `#[serde(default)]`.  Verify missing fields deserialize correctly.
        let old_json = r#"{"version":2,"session_id":"test","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert!(!meta.corrupted);
        assert_eq!(meta.session_id, "test");
    }

    // ── ADR-027: SessionTokens accumulation tests ─────────────────

    #[test]
    fn test_accumulate_llm_usage_basic() {
        // Two reliable calls accumulate correctly.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_basic", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1_000,
                completion_tokens: 200,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 3_500,
                completion_tokens: 450,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                tokens.last_input, 3_500,
                "last_input is the most recent raw value"
            );
            assert_eq!(tokens.last_output, 450);
            assert_eq!(tokens.total_input, 4_500);
            assert_eq!(tokens.total_output, 650);
        });
    }

    #[test]
    fn test_bump_llm_call_counter_monotonic_and_persists() {
        // Per-session lifetime LLM-call counter ("Iterations" display):
        // 1-based, monotonic, persisted to meta.json, and a resumed
        // session starts from the persisted value — so the count survives
        // Continue-after-max_iterations and process restarts.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_iter", cfg, 0, committed.clone()).unwrap();

            // None before the first LLM call.
            assert_eq!(session.llm_call_counter(), None);
            // 1-based and monotonic — never reset by Continue.
            assert_eq!(session.bump_llm_call_counter(), 1);
            assert_eq!(session.bump_llm_call_counter(), 2);
            assert_eq!(session.llm_call_counter(), Some(2));

            // Persisted to meta.json on disk.
            let meta = read_session_meta(&dir.join("conversations"), "tok_acc_iter").unwrap();
            assert_eq!(meta.llm_call_counter, Some(2));

            // Resumed session loads the persisted counter and keeps growing.
            session.close().await.unwrap();
            let (resumed, _config_rx, _state_rx) =
                ConversationSession::resume(&dir, "tok_acc_iter", committed).unwrap();
            assert_eq!(resumed.llm_call_counter(), Some(2));
            assert_eq!(resumed.bump_llm_call_counter(), 3);
        });
    }

    #[test]
    fn test_record_llm_round_folds_usage_perf_and_counter_once() {
        // One LLM round = one meta write carrying all three accumulators.
        // A call that measured only TTFT must not dilute the speed mean —
        // each field keeps its own sample count.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "perf_round", cfg, 0, committed.clone()).unwrap();

            assert_eq!(
                session.record_llm_round(
                    &UsageInfo {
                        prompt_tokens: 1_000,
                        completion_tokens: 200,
                        total_tokens: 1_200,
                        ..Default::default()
                    },
                    Some(100.0),
                    Some(300.0),
                    &ModelUsageAttribution::default(),
                ),
                1
            );
            // A TTFT-only round: speed mean stays on its own single sample.
            assert_eq!(
                session.record_llm_round(
                    &UsageInfo {
                        prompt_tokens: 2_500,
                        completion_tokens: 0,
                        total_tokens: 2_500,
                        ..Default::default()
                    },
                    None,
                    Some(140.0),
                    &ModelUsageAttribution::default(),
                ),
                2
            );

            let tokens = session.tokens().expect("tokens recorded");
            assert_eq!(tokens.total_input, 3_500);
            assert_eq!(tokens.total_output, 200);
            let perf = session.perf().expect("perf recorded");
            assert_eq!(perf.avg_tps(), Some(100.0), "TTFT-only round dilutes speed");
            assert_eq!(perf.avg_ttft_ms(), Some(220.0));
            assert_eq!(session.llm_call_counter(), Some(2));

            // A round with no perf observation still counts the call.
            assert_eq!(
                session.record_llm_round(
                    &UsageInfo {
                        prompt_tokens: 0,
                        completion_tokens: 50,
                        total_tokens: 50,
                        ..Default::default()
                    },
                    None,
                    None,
                    &ModelUsageAttribution::default(),
                ),
                3
            );

            session.close().await.unwrap();
            let meta = read_session_meta(&dir.join("conversations"), "perf_round").expect("meta");
            assert_eq!(meta.llm_call_counter, Some(3));
            let disk = meta.perf.expect("perf persisted");
            assert_eq!(disk.tps_samples, 1);
            assert_eq!(disk.ttft_ms_samples, 2);

            let (resumed, _config_rx, _state_rx) =
                ConversationSession::resume(&dir, "perf_round", committed).unwrap();
            let rperf = resumed.perf().expect("perf survives resume");
            assert_eq!(rperf.avg_tps(), Some(100.0));
            assert_eq!(rperf.avg_ttft_ms(), Some(220.0));
            assert_eq!(resumed.llm_call_counter(), Some(3));
        });
    }

    #[test]
    fn test_accumulate_llm_usage_skips_input_when_zero() {
        // Provider fallback (prompt_tokens == 0) must NOT pollute total_input
        // but MUST still record last_input as raw zero (ADR-027 honesty).
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_zero", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 0, // Provider fallback
                completion_tokens: 50,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 2_000,
                completion_tokens: 80,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                tokens.last_input, 2_000,
                "last_input reflects the most recent raw Provider value"
            );
            assert_eq!(
                tokens.total_input, 3_000,
                "total_input skipped the prompt_tokens=0 call (1000 + 2000)"
            );
            assert_eq!(
                tokens.total_output, 230,
                "total_output accumulates every call (100 + 50 + 80)"
            );
        });
    }

    #[test]
    fn test_accumulate_llm_usage_saturating_overflow() {
        // saturating_add guards against pathological Provider counters.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_overflow", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: u64::MAX,
                completion_tokens: u64::MAX,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 1,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                tokens.last_input, 1,
                "last_input is the most recent raw value, not the sum"
            );
            assert_eq!(
                tokens.total_input,
                u64::MAX,
                "saturating_add caps total_input at u64::MAX"
            );
            assert_eq!(tokens.total_output, u64::MAX);
        });
    }

    #[test]
    fn test_session_tokens_serde_roundtrip() {
        let t = SessionTokens {
            last_input: 100,
            last_output: 20,
            total_input: 500,
            total_output: 80,
            // ADR-066: cache fields round-trip alongside the legacy 4.
            last_cache_read: 40,
            last_cache_write: 10,
            total_cache_read: 200,
            total_cache_write: 50,
        };
        let json = serde_json::to_string(&t).unwrap();
        let parsed: SessionTokens = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, t);
    }

    /// ADR-066: a v3 meta file written by an older Runtime has no cache
    /// fields.  Deserializing it must succeed with all cache counters at
    /// 0 (matches the "宁可 miss 也不估计" policy — the absence of cache
    /// data is treated as "0 reported", not as a sentinel).
    #[test]
    fn test_session_tokens_serde_backward_compat_without_cache_fields() {
        let legacy_json = r#"{
            "last_input": 100,
            "last_output": 20,
            "total_input": 500,
            "total_output": 80
        }"#;
        let parsed: SessionTokens = serde_json::from_str(legacy_json)
            .expect("old v3 meta without cache fields must still parse");
        assert_eq!(parsed.last_input, 100);
        assert_eq!(parsed.last_output, 20);
        assert_eq!(parsed.total_input, 500);
        assert_eq!(parsed.total_output, 80);
        assert_eq!(parsed.last_cache_read, 0);
        assert_eq!(parsed.last_cache_write, 0);
        assert_eq!(parsed.total_cache_read, 0);
        assert_eq!(parsed.total_cache_write, 0);
    }

    #[test]
    fn test_session_tokens_default_is_all_zero() {
        let t = SessionTokens::default();
        assert_eq!(t.last_input, 0);
        assert_eq!(t.last_output, 0);
        assert_eq!(t.total_input, 0);
        assert_eq!(t.total_output, 0);
        // ADR-066: cache fields are also 0 in a default-constructed
        // SessionTokens (no LLM call recorded yet).
        assert_eq!(t.last_cache_read, 0);
        assert_eq!(t.last_cache_write, 0);
        assert_eq!(t.total_cache_read, 0);
        assert_eq!(t.total_cache_write, 0);
    }

    /// Regression: a compaction summary LLM call must NOT overwrite
    /// `last_input` with the summary's `prompt_tokens`. The summary LLM
    /// was given the full pre-compaction history as input, so its prompt
    /// size is unrepresentative of the post-compaction session state.
    /// Overwriting would inflate the displayed context usage until the
    /// next main-dialog LLM call recalibrates it.
    #[test]
    fn test_accumulate_compaction_usage_preserves_last_input() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_compaction_preserve", cfg, 0, committed)
                    .unwrap();

            // Simulate a previous main-dialog LLM call: 10_000 input, 500 output.
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 10_000,
                completion_tokens: 500,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            let before = session.tokens().expect("tokens should be set");
            assert_eq!(before.last_input, 10_000);
            assert_eq!(before.last_output, 500);
            assert_eq!(before.total_input, 10_000);
            assert_eq!(before.total_output, 500);

            // Simulate the compaction summary LLM call: the summary was
            // generated from the full pre-compaction history, so its
            // prompt_tokens are large (much larger than the post-compaction
            // session state). Its completion_tokens count the summary output.
            let summary_prompt_tokens = 80_000;
            let summary_completion_tokens = 1_200;
            session.accumulate_compaction_usage(&UsageInfo {
                prompt_tokens: summary_prompt_tokens,
                completion_tokens: summary_completion_tokens,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let after = session.tokens().expect("tokens should be set");
            // last_input / last_output MUST be preserved — the compaction
            // LLM's prompt is not a representative measurement of the
            // current session's input size.
            assert_eq!(
                after.last_input, 10_000,
                "accumulate_compaction_usage must preserve last_input"
            );
            assert_eq!(
                after.last_output, 500,
                "accumulate_compaction_usage must preserve last_output"
            );
            // Cumulative totals MUST still absorb the summary LLM's tokens
            // for billing/metrics parity with main-dialog calls.
            assert_eq!(
                after.total_input,
                10_000 + summary_prompt_tokens,
                "accumulate_compaction_usage must accumulate total_input"
            );
            assert_eq!(
                after.total_output,
                500 + summary_completion_tokens,
                "accumulate_compaction_usage must accumulate total_output"
            );
        });
    }

    /// Regression: `set_history_anchor` must overwrite `last_input` to the
    /// post-compaction history size so `emit_session_state()` reports the
    /// new (smaller) `usage_percent` immediately. `last_output` resets to 0
    /// because no completion event is associated with the compaction event
    /// itself; the summary's completion tokens were already counted into
    /// `total_output` by `accumulate_compaction_usage`.
    #[test]
    fn test_set_history_anchor_overwrites_last_input() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_history_anchor", cfg, 0, committed).unwrap();

            // Pre-anchor state: simulate a previous main-dialog call (last_input
            // = 10_000) followed by a compaction summary call that preserves
            // last_input.
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 10_000,
                completion_tokens: 500,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_compaction_usage(&UsageInfo {
                prompt_tokens: 80_000,
                completion_tokens: 1_200,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            // Anchor to the post-compaction history size. Typical numbers:
            // pre-compaction history ~80K tokens, summary trims it to ~20K.
            let post_compaction_tokens: u64 = 20_000;
            session.set_history_anchor(post_compaction_tokens);

            let after = session.tokens().expect("tokens should be set");
            assert_eq!(
                after.last_input, post_compaction_tokens,
                "set_history_anchor must overwrite last_input with the post-compaction size"
            );
            assert_eq!(
                after.last_output, 0,
                "set_history_anchor must reset last_output (no completion event)"
            );
            // Cumulative totals must NOT change — anchor only updates the
            // display anchor.
            assert_eq!(after.total_input, 10_000 + 80_000);
            assert_eq!(after.total_output, 500 + 1_200);
        });
    }

    /// Regression: ordering matters — `set_history_anchor` must run AFTER
    /// `accumulate_compaction_usage` so the final `last_input` is the
    /// post-compaction history size (heuristic), not the summary LLM's
    /// pre-compaction prompt size. Simulates the exact ordering in
    /// `AgentLoop::compact_history_if_needed`.
    #[test]
    fn test_compaction_pipeline_ordering() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_compaction_order", cfg, 0, committed).unwrap();

            // 1. Last main-dialog LLM call before compaction.
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 144_000,
                completion_tokens: 600,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            // 2. Compaction summary LLM call (preserves last_input=144_000).
            session.accumulate_compaction_usage(&UsageInfo {
                prompt_tokens: 144_000,
                completion_tokens: 1_500,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            // 3. History replace_middle_with_summary — token_count() drops
            //    to ~20K. Anchor to that.
            session.set_history_anchor(20_000);

            // 4. Next user message → main-dialog LLM call.
            //    This overwrites last_input with the real API value.
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 23_500,
                completion_tokens: 800,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let final_tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                final_tokens.last_input, 23_500,
                "final last_input must come from the most recent main-dialog call"
            );
            assert_eq!(final_tokens.last_output, 800);
            // Cumulative totals include all four events.
            assert_eq!(final_tokens.total_input, 144_000 + 144_000 + 23_500);
            assert_eq!(final_tokens.total_output, 600 + 1_500 + 800);
        });
    }

    #[test]
    fn test_model_usage_folds_attribution_and_poisons_cost() {
        // Per-model detail folding mirrors the ADR-027/066 zero-skip gate,
        // attributes by provider|model (missing → "unknown|unknown"), and
        // poisons a bucket's cost on the first priceless contribution.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "mu_fold", cfg, 0, committed.clone()).unwrap();

            // Round 1: attributed, priced call.
            session.record_llm_round(
                &UsageInfo {
                    prompt_tokens: 1_000,
                    completion_tokens: 200,
                    cache_read_tokens: 400,
                    cache_write_tokens: 60,
                    ..Default::default()
                },
                None,
                None,
                &ModelUsageAttribution {
                    provider_id: Some("p1"),
                    model_id: Some("m1"),
                    cost_delta: Some(0.01),
                },
            );
            // Round 2: same bucket, price still known → cost accumulates.
            session.accumulate_llm_usage(
                &UsageInfo {
                    prompt_tokens: 500,
                    completion_tokens: 100,
                    ..Default::default()
                },
                &ModelUsageAttribution {
                    provider_id: Some("p1"),
                    model_id: Some("m1"),
                    cost_delta: Some(0.02),
                },
            );
            // Round 3: provider fallback (prompt = 0) without a price — the
            // zero-skip gate drops input/cache_read, and the missing price
            // poisons the bucket cost (Some(0.03) → None).
            session.accumulate_llm_usage(
                &UsageInfo {
                    prompt_tokens: 0,
                    completion_tokens: 50,
                    cache_read_tokens: 999,
                    ..Default::default()
                },
                &ModelUsageAttribution {
                    provider_id: Some("p1"),
                    model_id: Some("m1"),
                    cost_delta: None,
                },
            );
            // Round 4: unattributed compaction call → "unknown|unknown".
            session.accumulate_compaction_usage(
                &UsageInfo {
                    prompt_tokens: 8_000,
                    completion_tokens: 900,
                    ..Default::default()
                },
                &ModelUsageAttribution::default(),
            );

            let meta = read_session_meta(&dir.join("conversations"), "mu_fold").unwrap();
            let usage = meta.model_usage.expect("detail map persisted");
            let p1m1 = &usage["p1|m1"];
            assert_eq!(p1m1.input, 1_500);
            assert_eq!(p1m1.output, 350);
            assert_eq!(p1m1.cache_read, 400, "round-3 cache_read skipped (prompt=0)");
            assert_eq!(p1m1.cache_write, 60);
            assert_eq!(p1m1.cost, None, "a priceless contribution poisons the bucket cost");
            let unk = &usage["unknown|unknown"];
            assert_eq!(unk.input, 8_000);
            assert_eq!(unk.output, 900);
            assert_eq!(unk.cost, None);

            // Σ detail == tokens totals — the store's diff invariant.
            let t = meta.tokens.expect("totals set");
            assert_eq!(p1m1.input + unk.input, t.total_input);
            assert_eq!(p1m1.output + unk.output, t.total_output);
            assert_eq!(p1m1.cache_read + unk.cache_read, t.total_cache_read);
            assert_eq!(p1m1.cache_write + unk.cache_write, t.total_cache_write);

            // Resume hydrates the map; the next attributed round keeps
            // folding into the existing buckets.
            session.close().await.unwrap();
            let (resumed, _config_rx, _state_rx) =
                ConversationSession::resume(&dir, "mu_fold", committed).unwrap();
            resumed.accumulate_llm_usage(
                &UsageInfo {
                    prompt_tokens: 100,
                    completion_tokens: 10,
                    ..Default::default()
                },
                &ModelUsageAttribution {
                    provider_id: Some("p1"),
                    model_id: Some("m1"),
                    cost_delta: None,
                },
            );
            let after = resumed.build_meta();
            let usage = after.model_usage.expect("map survives resume");
            assert_eq!(usage["p1|m1"].input, 1_600);
            assert_eq!(usage["p1|m1"].cost, None, "poison stays poisoned");
        });
    }

    #[test]
    fn test_estimate_call_cost_requires_prices_and_gates_zero_prompt() {
        let priced = acowork_core::protocol::ModelCostInfo {
            input_per_million: Some(3.0),
            output_per_million: Some(15.0),
        };
        // ADR-027 zero-skip mirror: prompt_tokens == 0 bills no input.
        let cost = estimate_call_cost(
            Some(&priced),
            &UsageInfo { prompt_tokens: 0, completion_tokens: 1_000, ..Default::default() },
        )
        .unwrap();
        assert!((cost - 0.015).abs() < 1e-12);
        let cost = estimate_call_cost(
            Some(&priced),
            &UsageInfo { prompt_tokens: 1_000_000, completion_tokens: 1_000_000, ..Default::default() },
        )
        .unwrap();
        assert!((cost - 18.0).abs() < 1e-12);
        // Half-priced or missing tables fold as "unknown", never 0.
        let half = acowork_core::protocol::ModelCostInfo {
            input_per_million: Some(3.0),
            output_per_million: None,
        };
        assert!(estimate_call_cost(Some(&half), &UsageInfo::default()).is_none());
        assert!(estimate_call_cost(None, &UsageInfo::default()).is_none());
    }

    #[test]
    fn test_session_meta_parses_without_model_usage() {
        // Meta written before the per-model breakdown existed must still
        // deserialize (serde default → None), the same contract as `tokens`.
        let old_json = r#"{"version":3,"session_id":"pre_mu","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert_eq!(meta.model_usage, None);
    }

    #[test]
    fn test_session_meta_tokens_field_default_to_none() {
        // ADR-027: backward compatibility — pre-ADR-027 meta files have no
        // `tokens` field; they should deserialize with `tokens: None`.
        let old_json = r#"{"version":2,"session_id":"pre027","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert_eq!(meta.tokens, None);
    }

    #[test]
    fn test_session_meta_serde_roundtrip() {
        // ADR-024: full round-trip with SessionMeta fields.
        let meta = SessionMeta {
            version: CONVERSATION_FORMAT_VERSION,
            session_id: "roundtrip_test".to_string(),
            agent_id: "com.test".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            initiator: None,
            visibility: None,
            title: Some("Test session".to_string()),
            workspace_id: None,
            model: Some("gpt-4".to_string()),
            provider: Some("openai".to_string()),
            account_id: None,
            reasoning_effort: None,
            temperature: Some(0.7),
            context_window: None,
            todos: None,
            message_count: 5,
            last_active_at: "2026-01-01T00:00:00Z".to_string(),
            tokens: Some(SessionTokens {
                last_input: 45_000,
                last_output: 1_200,
                total_input: 120_000,
                total_output: 3_400,
                last_cache_read: 8_000,
                last_cache_write: 1_500,
                total_cache_read: 22_000,
                total_cache_write: 4_000,
            }),
            llm_call_counter: Some(7),
            perf: Some(SessionPerf {
                tps_sum: 402.5,
                tps_samples: 7,
                ttft_ms_sum: 2170.0,
                ttft_ms_samples: 7,
            }),
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        chat_session_id: None,
            model_usage: None,
        };
        let json = serde_json::to_string(&meta).unwrap();
        let parsed: SessionMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.llm_call_counter, Some(7));
        // The accumulator round-trips as sum + count, and the derived means
        // are what the UI reads.
        assert_eq!(parsed.perf.as_ref().and_then(|p| p.avg_tps()), Some(57.5));
        assert_eq!(
            parsed.perf.as_ref().and_then(|p| p.avg_ttft_ms()),
            Some(310.0)
        );
        assert_eq!(
            parsed.tokens,
            Some(SessionTokens {
                last_input: 45_000,
                last_output: 1_200,
                total_input: 120_000,
                total_output: 3_400,
                last_cache_read: 8_000,
                last_cache_write: 1_500,
                total_cache_read: 22_000,
                total_cache_write: 4_000,
            })
        );
        assert_eq!(parsed.last_compaction_offset, None);
        assert_eq!(parsed.version, CONVERSATION_FORMAT_VERSION);
        assert_eq!(parsed.model.as_deref(), Some("gpt-4"));
    }

    // ── ADR-066: SessionTokens cache accumulation tests ────────────

    /// Two reliable calls accumulate cache_read + cache_write via
    /// saturating_add.  Per-turn (`last_cache_*`) tracks the most
    /// recent call's raw Provider values.
    #[test]
    fn test_accumulate_llm_usage_accumulates_cache_tokens() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_cache_basic", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1_000,
                completion_tokens: 200,
                cache_read_tokens: 400,  // Anthropic-style cache hit
                cache_write_tokens: 200, // Anthropic-style cache write
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 3_500,
                completion_tokens: 450,
                cache_read_tokens: 1_200,
                cache_write_tokens: 0,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session
                .tokens()
                .expect("tokens should be set after 2 calls");
            // Per-turn: most recent raw Provider value.
            assert_eq!(tokens.last_cache_read, 1_200);
            assert_eq!(tokens.last_cache_write, 0);
            // Cumulative: saturating_add.
            assert_eq!(tokens.total_cache_read, 1_600);
            assert_eq!(tokens.total_cache_write, 200);
            // Sanity: in/out unchanged.
            assert_eq!(tokens.total_input, 4_500);
            assert_eq!(tokens.total_output, 650);
        });
    }

    /// Provider-fallback semantics: a zero-input call must NOT add to
    /// `total_cache_read` (the same zero-skip gate as `total_input`),
    /// but `total_cache_write` IS always accumulated — Anthropic cache
    /// writes are real, billable events independent of the regular
    /// input count.
    #[test]
    fn test_accumulate_llm_usage_cache_zero_input_skip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_cache_zero", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1_000,
                completion_tokens: 100,
                cache_read_tokens: 300,
                cache_write_tokens: 50,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            // Provider fallback: prompt=0, but a (bogus) cache_write
            // is still recorded as-is.
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 0,
                completion_tokens: 50,
                cache_read_tokens: 999, // should be skipped
                cache_write_tokens: 30, // should be added
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 2_000,
                completion_tokens: 80,
                cache_read_tokens: 800,
                cache_write_tokens: 0,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session.tokens().expect("tokens should be set");
            // cache_read: 300 + 800 = 1_100 (999 skipped due to zero-input gate)
            assert_eq!(
                tokens.total_cache_read, 1_100,
                "cache_read follows the same zero-skip gate as total_input"
            );
            // cache_write: 50 + 30 + 0 = 80 (always accumulated)
            assert_eq!(
                tokens.total_cache_write, 80,
                "cache_write is always accumulated regardless of input"
            );
        });
    }

    /// Saturating overflow on cache counters — mirror of the existing
    /// overflow test for in/out.  Near-u64::MAX `cache_write_tokens`
    /// must not panic.
    #[test]
    fn test_accumulate_llm_usage_cache_saturating_overflow() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_cache_overflow", cfg, 0, committed)
                    .unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 0,
                cache_read_tokens: u64::MAX,
                cache_write_tokens: u64::MAX,
                ..Default::default()
            }, &ModelUsageAttribution::default());
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 1,
                completion_tokens: 0,
                cache_read_tokens: 5,
                cache_write_tokens: 7,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session.tokens().expect("tokens should be set");
            assert_eq!(
                tokens.total_cache_read,
                u64::MAX,
                "saturating_add caps total_cache_read at u64::MAX"
            );
            assert_eq!(
                tokens.total_cache_write,
                u64::MAX,
                "saturating_add caps total_cache_write at u64::MAX"
            );
        });
    }

    /// `accumulate_compaction_usage` must NOT pollute the per-turn
    /// `last_cache_*` snapshot — the compaction LLM was given the
    /// full pre-compaction history, so its cache figures are
    /// unrepresentative of the post-compaction state.
    ///
    /// Cumulative `total_cache_*` are still accumulated for parity
    /// with in/out (the tokens really were spent on a meta-call).
    #[test]
    fn test_accumulate_compaction_usage_preserves_last_cache_but_accumulates_total() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_acc_compaction_cache", cfg, 0, committed)
                    .unwrap();

            // Main-dialog LLM call establishes the per-turn baseline.
            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 10_000,
                completion_tokens: 500,
                cache_read_tokens: 4_000,
                cache_write_tokens: 800,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            // Compaction LLM call: huge prompt, big cache hit — must NOT
            // overwrite `last_cache_*` (otherwise the next push would
            // show inflated cache numbers until the next main call).
            session.accumulate_compaction_usage(&UsageInfo {
                prompt_tokens: 144_000,
                completion_tokens: 2_000,
                cache_read_tokens: 100_000,
                cache_write_tokens: 0,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            let tokens = session.tokens().expect("tokens should be set");
            // last_cache_* preserved from the pre-compaction call.
            assert_eq!(tokens.last_cache_read, 4_000);
            assert_eq!(tokens.last_cache_write, 800);
            // total_cache_* accumulated.
            assert_eq!(tokens.total_cache_read, 104_000);
            assert_eq!(tokens.total_cache_write, 800);
        });
    }

    /// `set_history_anchor` must NOT reset cumulative cache totals
    /// (they are Provider-reported, not local history estimates), and
    /// must zero `last_cache_*` in lock-step with `last_output = 0`
    /// (there is no fresh per-turn cache breakdown until the next
    /// `accumulate_llm_usage` call).
    #[test]
    fn test_set_history_anchor_preserves_total_cache_zeros_last_cache() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let dir = temp_dir.path().to_path_buf();
            let cfg = SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            };
            let committed = Arc::new(AtomicUsize::new(0));
            let (session, _config_rx, _state_rx) =
                ConversationSession::new(&dir, "tok_anchor_cache", cfg, 0, committed).unwrap();

            session.accumulate_llm_usage(&UsageInfo {
                prompt_tokens: 10_000,
                completion_tokens: 500,
                cache_read_tokens: 4_000,
                cache_write_tokens: 800,
                ..Default::default()
            }, &ModelUsageAttribution::default());

            // Anchor to post-compaction history size.
            session.set_history_anchor(3_500);

            let tokens = session.tokens().expect("tokens should be set");
            // total_cache_* preserved.
            assert_eq!(tokens.total_cache_read, 4_000);
            assert_eq!(tokens.total_cache_write, 800);
            // last_cache_* zeroed — there is no "current call" snapshot.
            assert_eq!(tokens.last_cache_read, 0);
            assert_eq!(tokens.last_cache_write, 0);
            // last_input still overwritten by the anchor (existing behavior).
            assert_eq!(tokens.last_input, 3_500);
            assert_eq!(tokens.last_output, 0);
        });
    }

    /// `scan_sessions_async` returns 4-tuple agent totals.  Two sessions
    /// with non-zero cache totals must aggregate correctly into the
    /// `(input, output, cache_read, cache_write)` tuple.  Cache fields
    /// default to 0 in legacy meta files (older Runtime), so old
    /// sessions contribute 0 — not undefined.
    #[test]
    fn test_scan_sessions_async_aggregates_cache_totals() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let temp_dir = TempDir::new().unwrap();
        rt.block_on(async {
            let conv_dir = temp_dir.path().join("conversations");
            std::fs::create_dir_all(&conv_dir).unwrap();

            let base = SessionMeta {
                version: 3,
                session_id: String::new(),
                agent_id: "com.test".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                initiator: None,
                visibility: None,
                title: None,
                workspace_id: None,
                model: None,
                provider: None,
                account_id: None,
                reasoning_effort: None,
                temperature: None,
                context_window: None,
                todos: None,
                message_count: 5,
                last_active_at: "2026-01-02T00:00:00Z".to_string(),
                tokens: None,
                llm_call_counter: None,
            perf: None,
                model_ratio: None,
                last_compaction_offset: None,
                corrupted: false,
            chat_session_id: None,
                model_usage: None,
            };

            // Session A: full cache breakdown.
            let meta_a = SessionMeta {
                session_id: "scan_cache_a".to_string(),
                tokens: Some(SessionTokens {
                    last_input: 1000,
                    last_output: 200,
                    total_input: 1000,
                    total_output: 200,
                    last_cache_read: 400,
                    last_cache_write: 100,
                    total_cache_read: 400,
                    total_cache_write: 100,
                }),
                ..base.clone()
            };
            write_session_meta(&conv_dir, &meta_a).unwrap();

            // Session B: legacy meta (cache fields default to 0).
            let meta_b = SessionMeta {
                session_id: "scan_cache_b".to_string(),
                created_at: "2026-01-03T00:00:00Z".to_string(),
                last_active_at: "2026-01-04T00:00:00Z".to_string(),
                message_count: 3,
                tokens: Some(SessionTokens {
                    last_input: 500,
                    last_output: 100,
                    total_input: 500,
                    total_output: 100,
                    last_cache_read: 0,
                    last_cache_write: 0,
                    total_cache_read: 0,
                    total_cache_write: 0,
                }),
                ..base.clone()
            };
            write_session_meta(&conv_dir, &meta_b).unwrap();

            let join = scan_sessions_async(conv_dir, None, None, SessionScope::Unfiltered);
            let (sessions, _total, (agent_in, agent_out, agent_cr, agent_cw)) = join.await.unwrap();

            assert_eq!(sessions.len(), 2);
            assert_eq!(
                agent_in, 1500,
                "total_input aggregated across both sessions"
            );
            assert_eq!(agent_out, 300, "total_output aggregated");
            assert_eq!(
                agent_cr, 400,
                "total_cache_read aggregated; legacy session contributes 0"
            );
            assert_eq!(
                agent_cw, 100,
                "total_cache_write aggregated; legacy session contributes 0"
            );
        });
    }

    #[test]
    fn test_session_meta_fields_default_to_none() {
        // ADR-024: old JSON without optional fields defaults correctly.
        let old_json = r#"{"version":2,"session_id":"old","agent_id":"com.test","created_at":"2026-01-01T00:00:00Z","last_active_at":"2026-01-01T00:00:00Z","message_count":0,"corrupted":false}"#;
        let meta: SessionMeta = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            meta.tokens, None,
            "old JSON without tokens field should default to None"
        );
        assert_eq!(meta.model, None);
        assert_eq!(meta.provider, None);
        assert_eq!(
            meta.todos, None,
            "pre-ADR-060 JSON without todos field should default to None"
        );
    }

    #[test]
    fn test_todos_persist_across_resume() {
        // ADR-060 §6.1: the todo snapshot survives a session restart —
        // `set_todos` writes meta immediately (metadata-mutation policy,
        // no cooldown swallow even right after `new`), and `resume`
        // hydrates it back into the session.
        let temp_dir = TempDir::new().unwrap();
        let sid = "20260503_090000_todos";
        let committed = Arc::new(AtomicUsize::new(0));

        let (session, _cfg_rx, _state_rx) = ConversationSession::new(
            temp_dir.path(),
            sid,
            SessionConfig {
                agent_id: "com.test".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            committed,
        )
        .unwrap();

        let items = vec![
            TodoItem {
                id: "t1".to_string(),
                content: "First task".to_string(),
                status: crate::agent::session_state::TodoStatus::InProgress,
            },
            TodoItem {
                id: "t2".to_string(),
                content: "Second task".to_string(),
                status: crate::agent::session_state::TodoStatus::Pending,
            },
        ];
        session.set_todos(&items);

        // Restart: a fresh session resumes from the persisted meta file.
        let (resumed, _cfg_rx, _state_rx) =
            ConversationSession::resume(temp_dir.path(), sid, Arc::new(AtomicUsize::new(0)))
                .unwrap();
        assert_eq!(
            resumed.todos(),
            Some(items),
            "todos must be restored from meta after session restart"
        );

        // Clearing the list also persists: an emptied todo list resumes as None.
        resumed.set_todos(&[]);
        let (resumed2, _cfg_rx, _state_rx) =
            ConversationSession::resume(temp_dir.path(), sid, Arc::new(AtomicUsize::new(0)))
                .unwrap();
        assert_eq!(
            resumed2.todos(),
            None,
            "emptied todo list must persist as None"
        );
    }

    // ── raw-entry pagination tests ───────────────────────────────
    //
    // Both `offset` and `limit` are in raw-entry units (one JSONL line
    // each).  Display-group collapsing (think + tool_call + tool_result
    // → 1 chip) is a **frontend UI abstraction** and is not tested
    // here.  See `displayMessages` in `ChatPanel.tsx`.

    /// Helper: write a JSONL file with entries (ADR-024: no metadata header).
    fn write_test_jsonl(dir: &TempDir, session_id: &str, entries: &[ConversationEntry]) -> PathBuf {
        let conv_dir = dir.path().join("conversations");
        std::fs::create_dir_all(&conv_dir).unwrap();
        let file_path = conv_dir.join(format!("{}.jsonl", session_id));
        let mut file = std::fs::File::create(&file_path).unwrap();
        // ADR-024: no metadata header — entries start at line 0.
        for e in entries {
            serde_json::to_writer(&mut file, e).unwrap();
            writeln!(file).unwrap();
        }
        file_path
    }

    fn make_entry(id: &str, role: &str, content: &str) -> ConversationEntry {
        ConversationEntry {
            id: id.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            role: role.to_string(),
            content: content.to_string(),
            metadata: None,
            kind: None,
        }
    }

    #[test]
    fn read_first_user_message_skips_non_user_and_internal_rows() {
        // Title generation must describe the FIRST real user turn: session-init
        // system rows and internal runtime nudges are not user input.
        let dir = TempDir::new().unwrap();
        let mut nudge = make_entry("nudge", "user", "budget nudge");
        nudge.metadata = Some(serde_json::json!({ "internal": true }));
        let entries = vec![
            make_entry("sys", "system", "session init"),
            nudge,
            make_entry("u1", "user", "第一条用户消息"),
            make_entry("a1", "assistant", "reply"),
            make_entry("u2", "user", "第二条用户消息"),
        ];
        let path = write_test_jsonl(&dir, "first-user", &entries);

        assert_eq!(
            read_first_user_message(&path).as_deref(),
            Some("第一条用户消息"),
            "must return the first real user row, not the internal nudge"
        );
    }

    #[test]
    fn read_first_user_message_returns_none_without_a_user_row() {
        let dir = TempDir::new().unwrap();
        let path = write_test_jsonl(&dir, "no-user", &[make_entry("a1", "assistant", "reply")]);
        assert_eq!(read_first_user_message(&path), None);

        // A missing file (brand-new session) is a clean `None`, not an error.
        assert_eq!(
            read_first_user_message(&dir.path().join("conversations").join("missing.jsonl")),
            None
        );
    }

    #[test]
    fn is_internal_only_for_internal_marker() {
        let mut e = make_entry("1", "user", "hello");
        assert!(!e.is_internal(), "no metadata → not internal");

        e.metadata = Some(serde_json::json!({ "tool_name": "echo" }));
        assert!(!e.is_internal(), "unrelated metadata → not internal");

        e.metadata = Some(serde_json::json!({ "internal": false }));
        assert!(!e.is_internal(), "explicit false → not internal");

        e.metadata = Some(serde_json::json!({ "internal": true }));
        assert!(e.is_internal(), "internal:true → internal");

        // The marker is the exact shape `append_internal_message` writes.
        assert_eq!(META_KEY_INTERNAL, "internal");
        let mut meta = serde_json::Map::new();
        meta.insert(META_KEY_INTERNAL.to_string(), serde_json::Value::Bool(true));
        e.metadata = Some(serde_json::Value::Object(meta));
        assert!(e.is_internal());
    }

    #[test]
    fn raw_limit_returns_first_n_raw_entries() {
        // 10 raw entries; limit=3 → first 3 entries (chronological order).
        // ADR-050: forward semantics — offset=0 anchors at the OLDEST end.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (1..=10)
            .map(|i| {
                make_entry(
                    &i.to_string(),
                    if i % 2 == 0 { "assistant" } else { "user" },
                    &format!("m{}", i),
                )
            })
            .collect();
        let path = write_test_jsonl(&dir, "sess-raw-limit", &entries);

        let page = read_messages_paginated(&path, 0, 3, false).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.total, 10);
        assert_eq!(page.messages[0].content, "m1");
        assert_eq!(page.messages[1].content, "m2");
        assert_eq!(page.messages[2].content, "m3");
    }

    #[test]
    fn raw_from_tail_returns_last_n_raw_entries() {
        // 10 raw entries; from_tail=true, limit=3 → last 3 entries.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (1..=10)
            .map(|i| {
                make_entry(
                    &i.to_string(),
                    if i % 2 == 0 { "assistant" } else { "user" },
                    &format!("m{}", i),
                )
            })
            .collect();
        let path = write_test_jsonl(&dir, "sess-raw-tail", &entries);

        let page = read_messages_paginated(&path, 0, 3, true).unwrap();
        assert_eq!(page.messages.len(), 3);
        assert_eq!(page.total, 10);
        // from_tail returns the last 3 entries: m8, m9, m10.
        assert_eq!(page.offset, 7, "tail offset = total - limit");
        assert_eq!(page.messages[0].content, "m8");
        assert_eq!(page.messages[1].content, "m9");
        assert_eq!(page.messages[2].content, "m10");
    }

    #[test]
    fn raw_offset_pagination_arithmetic() {
        // ADR-050: forward semantics — page_n = read(offset = n * limit, limit)
        // returns entries [n*limit, (n+1)*limit).  Every page exposes
        // exactly `limit` raw entries (no group boundary alignment), and
        // the union of all pages covers the whole file.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (1..=12)
            .map(|i| make_entry(&i.to_string(), "user", &format!("e{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-paginate", &entries);

        let p0 = read_messages_paginated(&path, 0, 5, false).unwrap();
        assert_eq!(p0.messages.len(), 5);
        assert_eq!(p0.messages.first().unwrap().content, "e1");
        assert_eq!(p0.messages.last().unwrap().content, "e5");

        let p1 = read_messages_paginated(&path, 5, 5, false).unwrap();
        assert_eq!(p1.messages.len(), 5);
        assert_eq!(p1.messages.first().unwrap().content, "e6");
        assert_eq!(p1.messages.last().unwrap().content, "e10");

        let p2 = read_messages_paginated(&path, 10, 5, false).unwrap();
        assert_eq!(p2.messages.len(), 2, "tail page only has 2 entries left");
        assert_eq!(p2.messages.first().unwrap().content, "e11");
        assert_eq!(p2.messages.last().unwrap().content, "e12");

        // Union of pages = whole file. Sort with a numeric-aware comparator
        // so the assertion reads the natural order (`e1..e12`), not the
        // lexicographic order (`e1, e10, e11, e12, e2, ...`).
        let mut all: Vec<&str> = p0
            .messages
            .iter()
            .chain(&p1.messages)
            .chain(&p2.messages)
            .map(|e| e.content.as_str())
            .collect();
        all.sort_by_key(|s| s.trim_start_matches('e').parse::<u32>().unwrap());
        let expected: Vec<String> = (1..=12).map(|i| format!("e{}", i)).collect();
        assert_eq!(all, expected.iter().map(|s| s.as_str()).collect::<Vec<_>>());
    }

    #[test]
    fn raw_offset_past_total_returns_empty() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
        ];
        let path = write_test_jsonl(&dir, "sess-empty-page", &entries);

        let page = read_messages_paginated(&path, 100, 5, false).unwrap();
        assert_eq!(page.messages.len(), 0);
        assert_eq!(page.total, 2);
        assert_eq!(page.limit, 0);
    }

    #[test]
    fn raw_limit_exceeds_total_returns_all() {
        // limit=50 on a 4-entry file must return all 4 entries —
        // the frontend default of 50 should always comfortably cover
        // any new session.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
            make_entry("3", "user", "u2"),
            make_entry("4", "assistant", "a2"),
        ];
        let path = write_test_jsonl(&dir, "sess-under-limit", &entries);

        let page = read_messages_paginated(&path, 0, 50, false).unwrap();
        assert_eq!(page.messages.len(), 4);
        assert_eq!(page.total, 4);
        assert_eq!(page.limit, 4);
    }

    #[test]
    fn raw_pagination_does_not_hide_user_messages_in_tool_heavy_session() {
        // Regression: with `limit=50` raw entries (frontend default),
        // a session of 1 user + 60 tool rows + 1 assistant = 62 raw
        // entries must surface the user message (which lives at idx 0,
        // well within the default page).  We verify two regimes:
        //   (a) Forward offset=0, limit=50: returns the FIRST 50 entries
        //       (idx 0..50) — user message MUST be in there.
        //   (b) Forward offset=50, limit=50: returns entries [50..62) —
        //       idx 50..61 (the tail 12 entries), user message MUST NOT.
        let dir = TempDir::new().unwrap();
        let mut entries = vec![make_entry("1", "user", "user-msg")];
        for i in 0..20 {
            entries.push(make_entry(
                &format!("t{}", i * 3 + 2),
                "thought",
                &format!("think-{}", i),
            ));
            entries.push(make_entry(
                &format!("c{}", i * 3 + 3),
                "tool_call",
                &format!("call-{}", i),
            ));
            entries.push(make_entry(
                &format!("r{}", i * 3 + 4),
                "tool_result",
                &format!("result-{}", i),
            ));
        }
        entries.push(make_entry("last", "assistant", "final-reply"));
        assert_eq!(entries.len(), 62);
        let path = write_test_jsonl(&dir, "sess-tool-heavy", &entries);

        // Page 1: first 50 raw entries (idx 0..50).  Includes user-msg.
        let page1 = read_messages_paginated(&path, 0, 50, false).unwrap();
        assert_eq!(page1.messages.len(), 50);
        assert_eq!(page1.total, 62);
        assert!(
            page1
                .messages
                .iter()
                .any(|m| m.role == "user" && m.content == "user-msg"),
            "page 1 (offset=0, limit=50) MUST include the user message at idx 0"
        );

        // Page 2: forward offset=50 → entries [50..62), idx 50..61 (12 entries).
        let page2 = read_messages_paginated(&path, 50, 50, false).unwrap();
        assert_eq!(page2.messages.len(), 12);
        assert!(
            !page2.messages.iter().any(|m| m.content == "user-msg"),
            "page 2 (idx 50..62) must NOT include the user message (it's at idx 0)"
        );
        // The last entry of the tail is the final assistant reply.
        assert_eq!(
            page2.messages.last().unwrap().content,
            "final-reply",
            "page 2 ends on the final assistant reply"
        );
    }

    fn make_compaction_entry(id: &str, summary: &str) -> ConversationEntry {
        ConversationEntry {
            id: id.to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            role: "system".to_string(),
            content: summary.to_string(),
            metadata: Some(serde_json::json!({
                "compacted_from_id": "first-id",
                "compacted_to_id": "last-id",
                "level": 1,
                "model": "test-model",
                "before_tokens": 1000u64,
                "after_tokens": 200u64,
            })),
            kind: Some(ENTRY_KIND_COMPACTION.to_string()),
        }
    }

    #[test]
    fn pagination_shows_full_history_with_compaction() {
        // 4 pre-compaction entries + compaction marker + 2 post-compaction entries.
        // Display path must show ALL entries — compaction boundary is only
        // enforced on the context path (restorer), not the display path.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "old-u1"),
            make_entry("2", "assistant", "old-a1"),
            make_entry("3", "user", "old-u2"),
            make_entry("4", "assistant", "old-a2"),
            make_compaction_entry("5", "<summary>compacted old-u1..old-a2</summary>"),
            make_entry("6", "user", "new-u3"),
            make_entry("7", "assistant", "new-a3"),
        ];
        let path = write_test_jsonl(&dir, "sess-compaction", &entries);

        // limit large enough to span the entire file in raw entries
        let page = read_messages_paginated(&path, 0, 50, false).unwrap();
        // Expect: all 7 entries (4 pre-compaction + compaction + 2 post-compaction)
        assert_eq!(
            page.messages.len(),
            7,
            "display path must show full history"
        );
        assert_eq!(page.total, 7);

        // Pre-compaction content must appear.
        assert!(
            page.messages.iter().any(|m| m.content == "old-u1"),
            "pre-compaction history must be visible in display path"
        );

        // Compaction marker must appear at the correct position (index 4).
        assert_eq!(
            page.messages[4].kind.as_deref(),
            Some(ENTRY_KIND_COMPACTION),
            "compaction marker must be at index 4"
        );

        // Post-compaction content must appear.
        assert_eq!(page.messages[5].content, "new-u3");
        assert_eq!(page.messages[6].content, "new-a3");
    }

    #[test]
    fn pagination_cursor_walks_forward_through_compaction_boundary() {
        // ADR-050 forward semantics: pagination cursor walks from the
        // oldest end forward (`offset += limit`).  Tight limit so the first
        // page does NOT include the tail of the conversation; the second
        // page (offset=2, limit=50) covers the remaining 5 entries,
        // crossing the compaction boundary.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "old-u1"),
            make_entry("2", "assistant", "old-a1"),
            make_compaction_entry("3", "<summary>...</summary>"),
            make_entry("4", "user", "new-u2"),
            make_entry("5", "assistant", "new-a2"),
            make_entry("6", "user", "new-u3"),
            make_entry("7", "assistant", "new-a3"),
        ];
        let path = write_test_jsonl(&dir, "sess-cap-boundary", &entries);

        // Page 1: oldest 2 raw entries (old-u1, old-a1).
        let page1 = read_messages_paginated(&path, 0, 2, false).unwrap();
        assert_eq!(page1.messages.len(), 2);
        assert_eq!(page1.messages[0].content, "old-u1");
        assert_eq!(page1.messages[1].content, "old-a1");

        // Page 2: forward offset=2 → entries [2..7) = 5 entries (compaction,
        // new-u2..new-a3), crossing the compaction boundary.
        let page2 = read_messages_paginated(&path, 2, 50, false).unwrap();
        assert_eq!(page2.messages.len(), 5, "5 entries after the first 2");
        assert!(
            page2
                .messages
                .iter()
                .any(|m| m.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)),
            "page 2 must include the compaction marker"
        );
        assert!(
            page2.messages.iter().any(|m| m.content.starts_with("new-")),
            "page 2 must include post-compaction history"
        );
        // page2 covers everything from idx 2 to the end: union with page1
        // = the whole file.
        assert_eq!(page2.total, 7);
        assert_eq!(page2.offset + page2.limit as u64, page2.total);
    }

    #[test]
    fn forward_pagination_with_stale_cursor() {
        // Forward pagination with a stale cursor pointing at offset 0
        // (below the data section start). The cursor is clamped to 0,
        // and all entries including pre-compaction history are returned.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "old-u1"),
            make_entry("2", "assistant", "old-a1"),
            make_compaction_entry("3", "<summary>...</summary>"),
            make_entry("4", "user", "new-u2"),
            make_entry("5", "assistant", "new-a2"),
        ];
        let path = write_test_jsonl(&dir, "sess-forward-clamp", &entries);

        // offset=0 anchors at the OLDEST entry (clamped).
        let page = read_messages_paginated(&path, 0, 50, false).unwrap();
        // Expect: all 5 entries (old-u1, old-a1, compaction, new-u2, new-a2)
        assert_eq!(
            page.messages.len(),
            5,
            "forward pagination must show full history"
        );
        assert!(
            page.messages
                .iter()
                .any(|m| m.kind.as_deref() == Some(ENTRY_KIND_COMPACTION)),
            "compaction marker must appear"
        );
        assert!(
            page.messages.iter().any(|m| m.content.starts_with("old-")),
            "pre-compaction history must be visible in forward pagination"
        );
    }

    #[test]
    fn pagination_without_compaction_is_unchanged() {
        // Regression: existing behavior (no compaction in file) must be preserved.
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "u1"),
            make_entry("2", "assistant", "a1"),
            make_entry("3", "user", "u2"),
            make_entry("4", "assistant", "a2"),
        ];
        let path = write_test_jsonl(&dir, "sess-no-compaction", &entries);

        let page = read_messages_paginated(&path, 0, 50, false).unwrap();
        assert_eq!(page.messages.len(), 4);
        assert_eq!(page.total, 4);
    }

    /// Build a StreamingStateMap with a single streaming line for `session_id`.
    fn make_streaming_map(
        session_id: &str,
        line_number: usize,
        role: &str,
        content: &str,
    ) -> StreamingStateMap {
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        map.write().unwrap().insert(
            session_id.to_string(),
            StreamingLine {
                line_number,
                role: role.to_string(),
                message_id: uuid::Uuid::new_v4().to_string(),
                accumulated_content: content.to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                started_at_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
        map
    }

    /// ADR-022: streaming delta uses the requested `line_char_offset` when the
    /// frontend is already caught up to this streaming line's `line_number`.
    #[test]
    fn test_read_since_streaming_same_line_uses_offset() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-same-line", &entries);
        // ADR-024: no metadata header — line 0 = user. total_lines = 1.
        // Streaming line will become line 1. Frontend already saw line 1 chars 0..3.
        let map = make_streaming_map("sess-same-line", 1, "assistant", "hello world");

        // Frontend line_number=1 (caught up), offset=3 → expect "lo world".
        let result = read_messages_since(&path, 1, 3, &map, "sess-same-line", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 1);
        assert_eq!(streaming.content, "lo world");
        assert_eq!(streaming.char_offset, "hello world".chars().count());
    }

    /// ADR-022 regression: when a role transition flushed the previous
    /// streaming line to JSONL and started a NEW streaming line at a higher
    /// line_number, the frontend's stale `line_char_offset` (from the old
    /// line) must NOT be applied to the new line — otherwise the opening
    /// characters of the new line are silently dropped.
    #[test]
    fn test_read_since_streaming_new_line_ignores_stale_offset() {
        let dir = TempDir::new().unwrap();
        // ADR-024: line 0 = user, line 1 = assistant. Frontend last saw up to line 1.
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous assistant text"),
        ];
        let path = write_test_jsonl(&dir, "sess-new-line", &entries);
        // New streaming line is a thought that will become line 2.
        let map = make_streaming_map("sess-new-line", 2, "thought", "reasoning...");

        // Frontend sends line_number=1 (has NOT yet seen line 2) with a stale
        // offset of 10 that belonged to the old assistant line. The new thought
        // line must be returned in full, not skipped by 10 chars.
        let result = read_messages_since(&path, 1, 10, &map, "sess-new-line", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 2);
        assert_eq!(
            streaming.content, "reasoning...",
            "stale offset from a flushed line must not truncate the new streaming line"
        );
        assert_eq!(streaming.char_offset, "reasoning...".chars().count());
    }

    /// ADR-022 regression: when the frontend's `line_number` matches the
    /// streaming line's `line_number` but `line_char_offset` exceeds the
    /// current content length (stale offset from a previous, longer streaming
    /// line that was flushed), the offset must be reset to 0 — not clamped
    /// to `current_len` (which would return an empty delta and lose content).
    #[test]
    fn test_read_since_streaming_stale_offset_exceeds_content() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous long assistant text"),
        ];
        let path = write_test_jsonl(&dir, "sess-stale-exceeds", &entries);
        // ADR-024: line 0 = user, line 1 = assistant. Streaming line = 2.
        let map = make_streaming_map("sess-stale-exceeds", 2, "assistant", "Hello");

        // Frontend sends line_number=2 (matches streaming line) with a stale
        // offset of 100 that belonged to the previous (longer) assistant line.
        let result = read_messages_since(&path, 2, 100, &map, "sess-stale-exceeds", 0).unwrap();
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 2);
        assert_eq!(
            streaming.content, "Hello",
            "stale offset exceeding current content length must return full content"
        );
        assert_eq!(streaming.char_offset, "Hello".chars().count());
    }

    // ── read_messages_since_cursor tests (ADR-025) ──────────────────

    #[test]
    fn test_cursor_basic_no_streaming() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hello"),
            make_entry("2", "assistant", "world"),
            make_entry("3", "user", "again"),
        ];
        let path = write_test_jsonl(&dir, "sess-cursor-basic", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Cursor at 0, limit 50 → all 3 messages, has_more=false
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            50,
            &map,
            "sess-cursor-basic",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 3);
        assert!(!result.has_more);
        assert_eq!(result.new_cursor.line_number, 3);
        assert_eq!(result.new_cursor.char_offset, 0);
        assert!(result.streaming.is_none());
    }

    #[test]
    fn test_cursor_batch_limit() {
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..10)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-cursor-batch", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Cursor at 0, limit 3 → first 3 messages, has_more=true
        let r1 = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            3,
            &map,
            "sess-cursor-batch",
            0,
        )
        .unwrap();
        assert_eq!(r1.messages.len(), 3);
        assert!(r1.has_more);
        assert_eq!(r1.new_cursor.line_number, 3);
        assert_eq!(r1.messages[0].content, "msg0");
        assert_eq!(r1.messages[2].content, "msg2");

        // Cursor at 3, limit 3 → next 3 messages, has_more=true
        let r2 = read_messages_since_cursor(&path, r1.new_cursor, 3, &map, "sess-cursor-batch", 0)
            .unwrap();
        assert_eq!(r2.messages.len(), 3);
        assert!(r2.has_more);
        assert_eq!(r2.new_cursor.line_number, 6);
        assert_eq!(r2.messages[0].content, "msg3");

        // Cursor at 6, limit 3 → next 3, has_more=true
        let r3 = read_messages_since_cursor(&path, r2.new_cursor, 3, &map, "sess-cursor-batch", 0)
            .unwrap();
        assert_eq!(r3.messages.len(), 3);
        assert!(r3.has_more);
        assert_eq!(r3.new_cursor.line_number, 9);

        // Cursor at 9, limit 3 → last 1 message, has_more=false
        let r4 = read_messages_since_cursor(&path, r3.new_cursor, 3, &map, "sess-cursor-batch", 0)
            .unwrap();
        assert_eq!(r4.messages.len(), 1);
        assert!(!r4.has_more);
        assert_eq!(r4.new_cursor.line_number, 10);
        assert_eq!(r4.messages[0].content, "msg9");
    }

    #[test]
    fn test_cursor_streaming_same_line_uses_offset() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-same", &entries);
        // total_lines=1, streaming line at 1 with "hello world"
        let map = make_streaming_map("sess-cursor-same", 1, "assistant", "hello world");

        // Cursor at line 1 (caught up), char_offset 3 → delta "lo world"
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 1,
                char_offset: 3,
            },
            50,
            &map,
            "sess-cursor-same",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 0); // no new complete lines
        assert!(!result.has_more);
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.line, 1);
        assert_eq!(streaming.content, "lo world");
        assert_eq!(streaming.char_offset, "hello world".chars().count());
        // Cursor advances char_offset to full length
        assert_eq!(result.new_cursor.line_number, 1);
        assert_eq!(result.new_cursor.char_offset, "hello world".chars().count());
    }

    #[test]
    fn test_cursor_streaming_new_line_resets_offset() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous text"),
        ];
        let path = write_test_jsonl(&dir, "sess-cursor-newline", &entries);
        // Streaming line at 2 (new), content "reasoning..."
        let map = make_streaming_map("sess-cursor-newline", 2, "thought", "reasoning...");

        // Cursor at line 1 (behind streaming line 2), stale char_offset 10
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 1,
                char_offset: 10,
            },
            50,
            &map,
            "sess-cursor-newline",
            0,
        )
        .unwrap();
        // Should return line 1 (the complete assistant message)
        assert_eq!(result.messages.len(), 1);
        assert_eq!(result.messages[0].content, "previous text");
        // Streaming delta should be full "reasoning..." (stale offset reset)
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.content, "reasoning...");
        // Cursor advances to line 2, char_offset = full streaming content length
        assert_eq!(result.new_cursor.line_number, 2);
        assert_eq!(
            result.new_cursor.char_offset,
            "reasoning...".chars().count()
        );
    }

    #[test]
    fn test_cursor_streaming_stale_offset_exceeds_content() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hi"),
            make_entry("2", "assistant", "previous long text"),
        ];
        let path = write_test_jsonl(&dir, "sess-cursor-stale", &entries);
        // Streaming line at 2, short content "Hello"
        let map = make_streaming_map("sess-cursor-stale", 2, "assistant", "Hello");

        // Cursor at line 2 (matches streaming line), stale char_offset 100
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 2,
                char_offset: 100,
            },
            50,
            &map,
            "sess-cursor-stale",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 0); // no new complete lines
        let streaming = result.streaming.expect("streaming line expected");
        assert_eq!(streaming.content, "Hello");
        assert_eq!(streaming.char_offset, "Hello".chars().count());
    }

    #[test]
    fn test_cursor_batch_with_streaming_delta() {
        // Background session scenario: 100 complete lines pending + streaming.
        // Streaming delta is NOT returned during batch catch-up (cursor behind
        // streaming line). It's only returned in the final batch when the
        // cursor catches up to the streaming line.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..100)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-cursor-bg", &entries);
        // Streaming line at 100, content "streaming content"
        let map = make_streaming_map("sess-cursor-bg", 100, "assistant", "streaming content");

        // Cursor at 0 (way behind), limit 50 — batch 1
        let r1 = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            50,
            &map,
            "sess-cursor-bg",
            0,
        )
        .unwrap();
        assert_eq!(r1.messages.len(), 50);
        assert!(r1.has_more);
        assert_eq!(r1.new_cursor.line_number, 50);
        // Streaming delta NOT returned (cursor at 50 < streaming at 100)
        assert!(
            r1.streaming.is_none(),
            "no streaming delta during batch catch-up"
        );
        assert_eq!(r1.new_cursor.char_offset, 0);

        // Batch 2: cursor at 50, limit 50 — catches up to line 100
        let r2 = read_messages_since_cursor(&path, r1.new_cursor, 50, &map, "sess-cursor-bg", 0)
            .unwrap();
        assert_eq!(r2.messages.len(), 50);
        assert!(!r2.has_more);
        assert_eq!(r2.new_cursor.line_number, 100);
        // Streaming delta IS returned now (cursor at 100 == streaming at 100)
        let s2 = r2
            .streaming
            .as_ref()
            .expect("streaming should be returned when caught up");
        assert_eq!(
            s2.content, "streaming content",
            "full content on first caught-up poll"
        );
        assert_eq!(s2.char_offset, "streaming content".chars().count());
        assert_eq!(
            r2.new_cursor.char_offset,
            "streaming content".chars().count()
        );

        // Batch 3: cursor at 100, no new complete lines, streaming delta only
        let r3 = read_messages_since_cursor(&path, r2.new_cursor, 50, &map, "sess-cursor-bg", 0)
            .unwrap();
        assert_eq!(r3.messages.len(), 0);
        assert!(!r3.has_more);
        let s3 = r3
            .streaming
            .as_ref()
            .expect("streaming delta on caught-up poll");
        assert_eq!(s3.content, "", "no new streaming chars");
    }

    #[test]
    fn test_cursor_at_total_lines_no_messages() {
        // Cursor already caught up, no streaming → empty result
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-caughtup", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 1,
                char_offset: 0,
            },
            50,
            &map,
            "sess-cursor-caughtup",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
        assert_eq!(result.new_cursor.line_number, 1);
        assert!(result.streaming.is_none());
    }

    #[test]
    fn test_cursor_clamped_to_total_lines() {
        // Cursor exceeds total_lines (defensive: JSONL was truncated)
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-clamp", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 999,
                char_offset: 0,
            },
            50,
            &map,
            "sess-cursor-clamp",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
        // Cursor is clamped to total_lines (1)
        assert_eq!(result.new_cursor.line_number, 1);
    }

    #[test]
    fn test_cursor_uses_cached_total_lines() {
        // When cached_total_lines > 0, it takes precedence over file scan.
        // In production, committed_lines is always accurate (ADR-022: incremented
        // AFTER write).  This test verifies the precedence logic only.
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..3)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "sess-cursor-cached", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // File has 3 lines, cache says 3 — should return all 3.
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            50,
            &map,
            "sess-cursor-cached",
            3,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 3);
        assert!(!result.has_more);
        assert_eq!(result.new_cursor.line_number, 3);
        assert_eq!(result.total_lines, 3);

        // File has 3 lines, cache says 0 — should fall back to file scan.
        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            50,
            &map,
            "sess-cursor-cached",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 3);
        assert_eq!(result.total_lines, 3);
    }

    #[test]
    fn test_cursor_empty_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("conversations").join("empty.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            50,
            &map,
            "empty",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(!result.has_more);
        assert_eq!(result.total_lines, 0);
        assert_eq!(result.new_cursor.line_number, 0);
    }

    #[test]
    fn test_cursor_limit_zero() {
        // Edge case: limit=0 → no messages, but has_more may still be true
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "sess-cursor-lim0", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        let result = read_messages_since_cursor(
            &path,
            DeliveryCursor {
                line_number: 0,
                char_offset: 0,
            },
            0,
            &map,
            "sess-cursor-lim0",
            0,
        )
        .unwrap();
        assert_eq!(result.messages.len(), 0);
        assert!(result.has_more); // 0 < 1
        assert_eq!(result.new_cursor.line_number, 0);
    }

    // ── End-to-end delivery flow tests (ADR-025) ─────────────────────
    //
    // These tests simulate the full delivery flow that SessionManager +
    // cli.rs perform: full load (reset cursor) → incremental poll →
    // batch catch-up → streaming flush → final poll.  They exercise
    // read_messages_since_cursor with realistic cursor management,
    // covering all branches and edge cases.

    /// Helper: append entries to an existing JSONL file (simulates writer
    /// thread flushing new lines while session is in background).
    fn append_test_jsonl(path: &Path, entries: &[ConversationEntry]) {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        for e in entries {
            serde_json::to_writer(&mut file, e).unwrap();
            writeln!(file).unwrap();
        }
    }

    /// E2E: Full lifecycle — full load resets cursor, then incremental
    /// polls deliver new data.
    #[test]
    fn e2e_full_load_then_incremental() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "hello"),
            make_entry("2", "assistant", "world"),
        ];
        let path = write_test_jsonl(&dir, "e2e-lifecycle", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-lifecycle";

        // Step 1: Full load — read all messages via pagination.
        // SessionManager.reset_delivery_cursor(sid, total_lines=2)
        let mut cursor = DeliveryCursor {
            line_number: 2,
            char_offset: 0,
        };

        // Step 2: Incremental poll — no new data, cursor already at end.
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
        cursor = r.new_cursor;
        assert_eq!(cursor.line_number, 2);

        // Step 3: Writer appends a new line (simulates flush).
        append_test_jsonl(&path, &[make_entry("3", "user", "new message")]);

        // Step 4: Incremental poll — should return the new line.
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].content, "new message");
        assert!(!r.has_more);
        cursor = r.new_cursor;
        assert_eq!(cursor.line_number, 3);
    }

    /// E2E: Background session — 100 lines accumulate while "away",
    /// then batch catch-up delivers them in batches.
    #[test]
    fn e2e_background_session_batch_catchup() {
        let dir = TempDir::new().unwrap();
        // Initial 2 lines (already delivered via full load)
        let entries = vec![
            make_entry("1", "user", "initial"),
            make_entry("2", "assistant", "response"),
        ];
        let path = write_test_jsonl(&dir, "e2e-bg", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-bg";

        // Full load done — cursor at 2
        let mut cursor = DeliveryCursor {
            line_number: 2,
            char_offset: 0,
        };

        // Session goes to background. Writer appends 100 more lines.
        let new_entries: Vec<ConversationEntry> = (0..100)
            .map(|i| make_entry(&format!("bg{}", i), "user", &format!("bg-msg{}", i)))
            .collect();
        append_test_jsonl(&path, &new_entries);

        // User returns — incremental poll with limit=50
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r1.messages.len(), 50);
        assert!(r1.has_more);
        assert_eq!(r1.messages[0].content, "bg-msg0");
        assert_eq!(r1.messages[49].content, "bg-msg49");
        cursor = r1.new_cursor;
        assert_eq!(cursor.line_number, 52);

        // Immediate re-poll (batch catch-up)
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 50);
        assert!(!r2.has_more); // 102 == 102, all complete lines delivered
        assert_eq!(r2.messages[0].content, "bg-msg50");
        assert_eq!(r2.messages[49].content, "bg-msg99");
        cursor = r2.new_cursor;
        assert_eq!(cursor.line_number, 102);

        // Final poll — no more complete lines
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 0);
        assert!(!r3.has_more);
        assert_eq!(r3.new_cursor.line_number, 102);
    }

    /// E2E: Streaming line lifecycle — delta delivery, flush, new line.
    #[test]
    fn e2e_streaming_flush_lifecycle() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "hi")];
        let path = write_test_jsonl(&dir, "e2e-stream", &entries);
        let sid = "e2e-stream";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 1
        let mut cursor = DeliveryCursor {
            line_number: 1,
            char_offset: 0,
        };

        // Streaming line starts at line 1, content "Hello"
        {
            let mut m = map.write().unwrap();
            m.insert(
                sid.to_string(),
                StreamingLine {
                    line_number: 1,
                    role: "assistant".to_string(),
                    message_id: uuid::Uuid::new_v4().to_string(),
                    accumulated_content: "Hello".to_string(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    started_at_ms: 0,
                },
            );
        }

        // Poll 1: streaming delta "Hello" (full content, cursor caught up)
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r1.messages.len(), 0); // no new complete lines
        let s1 = r1.streaming.as_ref().expect("streaming delta");
        assert_eq!(s1.content, "Hello");
        assert_eq!(s1.char_offset, 5);
        cursor = r1.new_cursor;
        assert_eq!(cursor.line_number, 1);
        assert_eq!(cursor.char_offset, 5);

        // More content arrives: " world" appended
        {
            let mut m = map.write().unwrap();
            m.get_mut(sid).unwrap().accumulated_content = "Hello world".to_string();
        }

        // Poll 2: streaming delta " world" (incremental)
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 0);
        let s2 = r2.streaming.as_ref().expect("streaming delta");
        assert_eq!(s2.content, " world");
        assert_eq!(s2.char_offset, 11);
        cursor = r2.new_cursor;
        assert_eq!(cursor.char_offset, 11);

        // Flush: write "Hello world" to JSONL, remove streaming line
        append_test_jsonl(&path, &[make_entry("2", "assistant", "Hello world")]);
        map.write().unwrap().remove(sid);

        // New streaming line starts at line 2, content "Next"
        {
            let mut m = map.write().unwrap();
            m.insert(
                sid.to_string(),
                StreamingLine {
                    line_number: 2,
                    role: "thought".to_string(),
                    message_id: uuid::Uuid::new_v4().to_string(),
                    accumulated_content: "Next".to_string(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    started_at_ms: 0,
                },
            );
        }

        // Poll 3: flushed line + new streaming delta
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 1);
        assert_eq!(r3.messages[0].content, "Hello world");
        let s3 = r3.streaming.as_ref().expect("new streaming delta");
        assert_eq!(s3.content, "Next", "full content of new streaming line");
        assert_eq!(s3.line, 2);
        cursor = r3.new_cursor;
        assert_eq!(cursor.line_number, 2); // advanced past flushed line
        assert_eq!(cursor.char_offset, 4); // "Next" = 4 chars
    }

    /// E2E: Full load resets cursor — incremental poll after full load
    /// returns nothing (all existing lines already delivered).
    #[test]
    fn e2e_full_load_resets_cursor() {
        let dir = TempDir::new().unwrap();
        let entries: Vec<ConversationEntry> = (0..10)
            .map(|i| make_entry(&format!("m{}", i), "user", &format!("msg{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "e2e-reset", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-reset";

        // Simulate: cursor was at 3 (some incremental polls happened)
        let cursor_before = DeliveryCursor {
            line_number: 3,
            char_offset: 0,
        };

        // Full load happens — reset_delivery_cursor(sid, total_lines=10)
        let cursor = DeliveryCursor {
            line_number: 10,
            char_offset: 0,
        };

        // Incremental poll — should return nothing (cursor at end)
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);

        // Verify the reset actually happened (cursor was 3, now 10)
        assert_ne!(cursor_before.line_number, cursor.line_number);
        assert_eq!(cursor.line_number, 10);
    }

    /// E2E: Batch catch-up with streaming — streaming delta is NOT
    /// returned during catch-up, only when cursor catches up.
    #[test]
    fn e2e_batch_catchup_with_streaming() {
        let dir = TempDir::new().unwrap();
        // 5 initial lines (delivered via full load)
        let entries: Vec<ConversationEntry> = (0..5)
            .map(|i| make_entry(&format!("i{}", i), "user", &format!("init{}", i)))
            .collect();
        let path = write_test_jsonl(&dir, "e2e-batch-stream", &entries);
        let sid = "e2e-batch-stream";

        // 10 more lines accumulate while "away"
        let new_entries: Vec<ConversationEntry> = (0..10)
            .map(|i| make_entry(&format!("n{}", i), "user", &format!("new{}", i)))
            .collect();
        append_test_jsonl(&path, &new_entries);

        // Streaming line at 15 (after all complete lines)
        let map = make_streaming_map(sid, 15, "assistant", "streaming content");

        // Cursor at 5 (after initial full load), limit=3
        let mut cursor = DeliveryCursor {
            line_number: 5,
            char_offset: 0,
        };

        // Batch 1: lines 5-7, has_more=true, NO streaming delta
        let r1 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r1.messages.len(), 3);
        assert!(r1.has_more);
        assert!(r1.streaming.is_none(), "no streaming during batch catch-up");
        cursor = r1.new_cursor;

        // Batch 2: lines 8-10, has_more=true, NO streaming delta
        let r2 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 3);
        assert!(r2.has_more);
        assert!(r2.streaming.is_none());
        cursor = r2.new_cursor;

        // Batch 3: lines 11-13, has_more=true, NO streaming delta
        let r3 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 3);
        assert!(r3.has_more);
        assert!(r3.streaming.is_none());
        cursor = r3.new_cursor;

        // Batch 4: lines 14, has_more=false (14 < 15), cursor at 14 < 15
        // Wait: 5 initial + 10 new = 15 total. cursor was 5, batches: 5→8→11→14→15
        // Batch 4: lines 14, has_more = 14 < 15 = true? No, 14+3=15=min(15,17)=15
        // So batch 4: lines[14..15) = 1 line, cursor→15, has_more=15<15=false
        let r4 = read_messages_since_cursor(&path, cursor, 3, &map, sid, 0).unwrap();
        assert_eq!(r4.messages.len(), 1);
        assert!(!r4.has_more);
        // Cursor at 15 == streaming.line_number(15) → streaming delta returned!
        let s4 = r4
            .streaming
            .as_ref()
            .expect("streaming delta when caught up");
        assert_eq!(s4.content, "streaming content");
        assert_eq!(s4.line, 15);
        cursor = r4.new_cursor;
        assert_eq!(cursor.line_number, 15);
        assert_eq!(cursor.char_offset, "streaming content".chars().count());
    }

    /// E2E: Multiple incremental polls with growing streaming content.
    /// Verifies char_offset advances correctly across polls.
    #[test]
    fn e2e_multiple_polls_streaming_growth() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("1", "user", "start")];
        let path = write_test_jsonl(&dir, "e2e-growth", &entries);
        let sid = "e2e-growth";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 1
        let mut cursor = DeliveryCursor {
            line_number: 1,
            char_offset: 0,
        };

        // Streaming line at 1, initially empty
        {
            map.write().unwrap().insert(
                sid.to_string(),
                StreamingLine {
                    line_number: 1,
                    role: "assistant".to_string(),
                    message_id: uuid::Uuid::new_v4().to_string(),
                    accumulated_content: String::new(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    started_at_ms: 0,
                },
            );
        }

        // Poll 1: streaming exists but empty → delta is "", char_offset=0
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s1 = r1.streaming.as_ref().expect("streaming");
        assert_eq!(s1.content, "");
        assert_eq!(s1.char_offset, 0);
        cursor = r1.new_cursor;
        assert_eq!(cursor.char_offset, 0);

        // Content grows to "abc"
        map.write()
            .unwrap()
            .get_mut(sid)
            .unwrap()
            .accumulated_content = "abc".to_string();

        // Poll 2: delta = "abc" (from offset 0)
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s2 = r2.streaming.as_ref().expect("streaming");
        assert_eq!(s2.content, "abc");
        assert_eq!(s2.char_offset, 3);
        cursor = r2.new_cursor;
        assert_eq!(cursor.char_offset, 3);

        // Content grows to "abcdef"
        map.write()
            .unwrap()
            .get_mut(sid)
            .unwrap()
            .accumulated_content = "abcdef".to_string();

        // Poll 3: delta = "def" (from offset 3)
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s3 = r3.streaming.as_ref().expect("streaming");
        assert_eq!(s3.content, "def");
        assert_eq!(s3.char_offset, 6);
        cursor = r3.new_cursor;
        assert_eq!(cursor.char_offset, 6);
    }

    /// E2E: done event final poll — after all streaming is flushed,
    /// the final poll delivers the last flushed line.
    #[test]
    fn e2e_done_event_final_poll() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "question"),
            make_entry("2", "assistant", "partial"),
        ];
        let path = write_test_jsonl(&dir, "e2e-done", &entries);
        let sid = "e2e-done";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 2
        let mut cursor = DeliveryCursor {
            line_number: 2,
            char_offset: 0,
        };

        // Streaming line at 2, content "final answer"
        {
            map.write().unwrap().insert(
                sid.to_string(),
                StreamingLine {
                    line_number: 2,
                    role: "assistant".to_string(),
                    message_id: uuid::Uuid::new_v4().to_string(),
                    accumulated_content: "final answer".to_string(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    started_at_ms: 0,
                },
            );
        }

        // Poll 1: streaming delta "final answer"
        let r1 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        let s1 = r1.streaming.as_ref().expect("streaming");
        assert_eq!(s1.content, "final answer");
        cursor = r1.new_cursor;

        // done event: flush streaming, remove from map
        append_test_jsonl(&path, &[make_entry("3", "assistant", "final answer")]);
        map.write().unwrap().remove(sid);

        // Final poll (done handler): should deliver the flushed line
        let r2 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r2.messages.len(), 1);
        assert_eq!(r2.messages[0].content, "final answer");
        assert!(!r2.has_more);
        assert!(r2.streaming.is_none(), "no streaming after flush");
        cursor = r2.new_cursor;
        assert_eq!(cursor.line_number, 3);

        // Another poll — nothing left
        let r3 = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r3.messages.len(), 0);
        assert!(!r3.has_more);
        assert!(r3.streaming.is_none());
    }

    /// E2E: compacting_ended one-shot poll — compaction record written
    /// to JSONL, one-shot incremental poll delivers it.
    #[test]
    fn e2e_compacting_ended_oneshot_poll() {
        let dir = TempDir::new().unwrap();
        let entries = vec![
            make_entry("1", "user", "msg1"),
            make_entry("2", "assistant", "reply1"),
        ];
        let path = write_test_jsonl(&dir, "e2e-compact", &entries);
        let sid = "e2e-compact";
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));

        // Full load — cursor at 2
        let mut cursor = DeliveryCursor {
            line_number: 2,
            char_offset: 0,
        };

        // Compaction happens: compaction record written to JSONL
        append_test_jsonl(
            &path,
            &[make_compaction_entry("3", "<summary>compacted</summary>")],
        );

        // compacting_ended: one-shot incremental poll
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].kind.as_deref(), Some(ENTRY_KIND_COMPACTION));
        assert!(!r.has_more);
        cursor = r.new_cursor;
        assert_eq!(cursor.line_number, 3);
    }

    /// E2E: Cursor survives multiple write-then-poll cycles.
    /// Verifies cursor advancement is cumulative and correct.
    #[test]
    fn e2e_cumulative_cursor_advancement() {
        let dir = TempDir::new().unwrap();
        let entries = vec![make_entry("0", "user", "base")];
        let path = write_test_jsonl(&dir, "e2e-cumulative", &entries);
        let map: StreamingStateMap = Arc::new(RwLock::new(HashMap::new()));
        let sid = "e2e-cumulative";

        // Full load — cursor at 1
        let mut cursor = DeliveryCursor {
            line_number: 1,
            char_offset: 0,
        };
        let mut total_delivered = 0;

        // 5 rounds of: write 3 lines → poll → verify
        for round in 0..5 {
            let round_entries: Vec<ConversationEntry> = (0..3)
                .map(|i| {
                    make_entry(
                        &format!("r{}c{}", round, i),
                        "user",
                        &format!("round{}-msg{}", round, i),
                    )
                })
                .collect();
            append_test_jsonl(&path, &round_entries);

            let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
            assert_eq!(
                r.messages.len(),
                3,
                "round {} should deliver 3 messages",
                round
            );
            assert!(!r.has_more);
            total_delivered += r.messages.len();
            cursor = r.new_cursor;
        }

        assert_eq!(total_delivered, 15);
        // 1 initial + 15 new = 16 total lines
        assert_eq!(cursor.line_number, 16);

        // Final poll — nothing left
        let r = read_messages_since_cursor(&path, cursor, 50, &map, sid, 0).unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
    }

    // ── ADR-047 tests ──────────────────────────────────────────────────

    /// Helper: create a ConversationSession for testing.
    fn make_test_session() -> ConversationSession {
        let temp_dir = TempDir::new().unwrap();
        let work_dir = temp_dir.path();
        let session_id = generate_session_id();
        let (session, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            &session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();
        // Leak the temp_dir so it stays alive for the session's lifetime.
        std::mem::forget(temp_dir);
        session
    }

    /// ADR-047 acceptance #2: apply_config writes to the meta store immediately.
    #[test]
    fn test_apply_config_persists_model_to_meta() {
        let session = make_test_session();

        let delta = crate::agent::session_config::SessionConfigDelta {
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        session.apply_config(&delta);

        // Verify in-memory state
        let snapshot = session.config_snapshot();
        assert_eq!(snapshot.model.as_deref(), Some("gpt-4o"));
        assert_eq!(snapshot.provider.as_deref(), Some("openai"));

        // Verify the persisted meta row reflects the new values.
        let meta = read_session_meta(&session.conversations_dir, &session.session_id).unwrap();
        assert_eq!(meta.model.as_deref(), Some("gpt-4o"));
        assert_eq!(meta.provider.as_deref(), Some("openai"));
    }

    /// ADR-047 acceptance #2: apply_config writes temperature to the meta store.
    #[test]
    fn test_apply_config_persists_temperature_to_meta() {
        let session = make_test_session();

        let delta = crate::agent::session_config::SessionConfigDelta {
            temperature: Some(0.7),
            ..Default::default()
        };
        session.apply_config(&delta);

        let snapshot = session.config_snapshot();
        assert_eq!(snapshot.temperature, Some(0.7));

        let meta = read_session_meta(&session.conversations_dir, &session.session_id).unwrap();
        assert_eq!(meta.temperature, Some(0.7));
    }

    /// ADR-047 acceptance #4: config_version increments after apply_config.
    #[test]
    fn test_config_version_increments() {
        let session = make_test_session();
        let v0 = session.config_version();
        assert_eq!(v0, 0);

        session.apply_config(&crate::agent::session_config::SessionConfigDelta {
            model: Some("test-model".to_string()),
            ..Default::default()
        });
        let v1 = session.config_version();
        assert_eq!(v1, 1);

        // Empty delta (all None) should NOT increment version
        session.apply_config(&crate::agent::session_config::SessionConfigDelta::default());
        let v2 = session.config_version();
        assert_eq!(v2, 1, "empty delta must not increment config_version");

        // Another change increments again
        session.apply_config(&crate::agent::session_config::SessionConfigDelta {
            temperature: Some(0.5),
            ..Default::default()
        });
        let v3 = session.config_version();
        assert_eq!(v3, 2);
    }

    /// ADR-047 acceptance #3: apply_config triggers config_change_tx.
    #[test]
    fn test_apply_config_notifies_config_change() {
        let (session, mut config_rx) = {
            let temp_dir = TempDir::new().unwrap();
            let work_dir = temp_dir.path();
            let session_id = generate_session_id();
            let (s, config_rx, _state_rx) = ConversationSession::new(
                work_dir,
                &session_id,
                SessionConfig {
                    agent_id: "com.test.agent".to_string(),
                    workspace_id: None,
                    model: None,
                    provider: None,
                },
                0,
                Arc::new(AtomicUsize::new(0)),
            )
            .unwrap();
            std::mem::forget(temp_dir);
            (s, config_rx)
        };

        session.apply_config(&crate::agent::session_config::SessionConfigDelta {
            model: Some("notify-test".to_string()),
            ..Default::default()
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        let change = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(1), config_rx.recv()).await
        });
        assert!(
            change.is_ok(),
            "config_change_tx should have sent a notification"
        );
        let change = change.unwrap().unwrap();
        assert_eq!(change.snapshot.model_id, "notify-test");
    }

    /// ADR-047 acceptance #1: config_snapshot returns current state after multiple apply_config calls.
    #[test]
    fn test_config_snapshot_reflects_all_fields() {
        let session = make_test_session();

        session.apply_config(&crate::agent::session_config::SessionConfigDelta {
            model: Some("claude-3".to_string()),
            ..Default::default()
        });
        session.apply_config(&crate::agent::session_config::SessionConfigDelta {
            workspace_id: Some("ws-123".to_string()),
            ..Default::default()
        });
        session.apply_config(&crate::agent::session_config::SessionConfigDelta {
            reasoning_effort: Some("high".to_string()),
            temperature: Some(0.3),
            title: Some("Test Title".to_string()),
            ..Default::default()
        });

        let snapshot = session.config_snapshot();
        assert_eq!(snapshot.model.as_deref(), Some("claude-3"));
        assert_eq!(snapshot.workspace_id.as_deref(), Some("ws-123"));
        assert_eq!(snapshot.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(snapshot.temperature, Some(0.3));
        assert_eq!(snapshot.title.as_deref(), Some("Test Title"));
    }

    /// ADR-047 acceptance #5: SessionConfigDelta supports partial construction.
    #[test]
    fn test_session_config_delta_partial_construction() {
        let d1 = crate::agent::session_config::SessionConfigDelta {
            model: Some("m1".to_string()),
            ..Default::default()
        };
        assert!(d1.model.is_some());
        assert!(d1.provider.is_none());
        assert!(d1.temperature.is_none());

        let d2 = crate::agent::session_config::SessionConfigDelta {
            temperature: Some(0.8),
            ..Default::default()
        };
        assert!(d2.model.is_none());
        assert!(d2.temperature.is_some());

        let d3 = crate::agent::session_config::SessionConfigDelta::default();
        assert!(d3.model.is_none());
    }

    /// ADR-076 §决策 4: the `x-user-id` header is the whole identity
    /// contract with the Gateway. Absent must mean *unfiltered* (local
    /// mode), never "anonymous, deny everything" — the Gateway simply
    /// does not inject the header when the account system is off.
    #[test]
    fn session_scope_from_header_value() {
        let cases = [
            (None, SessionScope::Unfiltered),
            (Some(""), SessionScope::Unfiltered), // empty == absent
            (Some("*"), SessionScope::Unfiltered), // administrator
            (Some("u-alice"), SessionScope::user("u-alice")),
        ];
        for (input, expected) in cases {
            assert_eq!(
                SessionScope::from_header_value(input),
                expected,
                "input = {input:?}"
            );
        }
        // The unfiltered scope has no user id, so `create` records no owner.
        assert_eq!(SessionScope::Unfiltered.caller_uid(), None);
        assert_eq!(SessionScope::user("u-1").caller_uid(), Some("u-1"));
    }

    /// The read/write rules are the whole point of the visibility switch:
    /// public = visible to all, private = owner only, and **visible is not
    /// the same as writable**.
    #[test]
    fn visibility_and_ownership_gate_read_and_write() {
        let alice = SessionScope::user("u-alice");
        let bob = SessionScope::user("u-bob");
        // `Unfiltered` is local mode: no accounts, so no second identity to
        // protect a session from. The administrator case is the separate
        // test below — it reads everything but writes only its own.
        let local = SessionScope::Unfiltered;

        let meta = |owner: Option<&str>, visibility: Option<SessionVisibility>| SessionMeta {
            version: 1,
            session_id: "s".into(),
            agent_id: "a".into(),
            created_at: "t".into(),
            initiator: owner.map(str::to_string),
            visibility,
            title: None,
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 0,
            last_active_at: "t".into(),
            tokens: None,
            corrupted: false,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
        chat_session_id: None,
            model_usage: None,
        };

        // (owner, visibility) -> (alice reads, bob reads, alice writes, bob writes)
        let cases = [
            // Unset visibility defaults to public.
            (Some("u-alice"), None, true, true, true, false),
            // Explicit public behaves identically.
            (
                Some("u-alice"),
                Some(SessionVisibility::Public),
                true,
                true,
                true,
                false,
            ),
            // Private is owner-only, for read AND write.
            (Some("u-alice"), Some(SessionVisibility::Private), true, false, true, false),
            // Legacy / local-mode sessions have no owner: readable by all
            // and still writable by all, so nobody is locked out of their
            // own pre-account history.
            (None, None, true, true, true, true),
            (None, Some(SessionVisibility::Public), true, true, true, true),
            // ...but an ownerless session marked private is *unclaimed*:
            // nobody but an administrator gets it, in either direction.
            // This is the agent's cold-start session — see
            // `is_readable_by`. Collapsing it into the row above is how
            // two accounts end up sharing one conversation.
            (None, Some(SessionVisibility::Private), false, false, false, false),
        ];

        for (owner, visibility, r_alice, r_bob, w_alice, w_bob) in cases {
            let m = meta(owner, visibility);
            let label = format!("owner={owner:?} visibility={visibility:?}");
            assert_eq!(m.is_readable_by(&alice), r_alice, "alice read: {label}");
            assert_eq!(m.is_readable_by(&bob), r_bob, "bob read: {label}");
            assert_eq!(m.is_writable_by(&alice), w_alice, "alice write: {label}");
            assert_eq!(m.is_writable_by(&bob), w_bob, "bob write: {label}");
            // Local mode has no accounts at all, so nothing is blocked.
            assert!(m.is_readable_by(&local), "local read: {label}");
            assert!(m.is_writable_by(&local), "local write: {label}");
        }
    }

    /// Minimal `SessionMeta` for the scope tests: everything optional is
    /// absent, so each test sets only the fields it is about.
    fn bare_session_meta() -> SessionMeta {
        SessionMeta {
            version: 1,
            session_id: "s".into(),
            agent_id: "a".into(),
            created_at: "t".into(),
            initiator: None,
            visibility: None,
            chat_session_id: None,
            title: None,
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 0,
            last_active_at: "t".into(),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
            model_usage: None,
        }
    }

    /// ADR-076 §决策 4: an administrator reads everything (support
    /// inspection, `as_user`) but is **not** exempt from the single-initiator
    /// rule on writes. Letting admins write someone else's execution session
    /// would put a second person inside a conversation that is defined as
    /// having exactly one.
    #[test]
    fn admin_reads_everything_but_writes_only_its_own() {
        let admin = SessionScope::admin("u-admin");
        let alice = SessionScope::user("u-alice");

        let owned_by_alice = SessionMeta {
            initiator: Some("u-alice".into()),
            visibility: Some(SessionVisibility::Private),
            ..bare_session_meta()
        };
        assert!(owned_by_alice.is_readable_by(&admin), "admin reads others");
        assert!(
            !owned_by_alice.is_writable_by(&admin),
            "admin must not write another initiator's session"
        );

        let owned_by_admin = SessionMeta {
            initiator: Some("u-admin".into()),
            ..owned_by_alice.clone()
        };
        assert!(owned_by_admin.is_writable_by(&admin), "own session writable");
        assert!(
            !owned_by_admin.is_writable_by(&alice),
            "and the reverse holds too"
        );

        // The reverse holds for a plain account: the admin's private session
        // is neither readable nor writable for alice — `as_user` is an
        // ordinary user scope, it buys no write.
        let as_alice = SessionScope::user("u-alice");
        assert!(!owned_by_admin.is_readable_by(&as_alice));
        assert!(!owned_by_admin.is_writable_by(&as_alice));
    }

    /// The header pair is what tells the two admin-ish scopes apart.
    #[test]
    fn admin_header_scope_is_distinct_from_local_mode() {
        let admin = SessionScope::from_headers(Some("*"), Some("u-admin"), Some("cs-1"));
        assert_eq!(
            admin,
            SessionScope::Admin {
                uid: "u-admin".into(),
                chats: vec!["cs-1".into()],
            }
        );
        assert_eq!(admin.caller_uid(), Some("u-admin"));
        // No admin header → local mode, unchanged.
        assert_eq!(
            SessionScope::from_headers(Some("*"), None, None),
            SessionScope::Unfiltered
        );
        // `as_user` arrives as a plain user, admin header absent.
        assert_eq!(
            SessionScope::from_headers(Some("u-bob"), None, None),
            SessionScope::user("u-bob")
        );
    }

    /// Design 27 §8.2: `Members` visibility resolves against the caller's
    /// Chat membership — the bound session's participants read the group
    /// execution history, everyone else gets a hidden 404, and the binding
    /// never grants write (participants' own messages enter through the
    /// agent's delivery path, not a direct session write).
    #[test]
    fn members_visibility_gates_read_on_chat_membership() {
        fn session_meta_fixture() -> SessionMeta {
            SessionMeta {
                version: 1,
                session_id: "s".into(),
                agent_id: "a".into(),
                created_at: "t".into(),
                initiator: None,
                visibility: None,
                chat_session_id: None,
                title: None,
                workspace_id: None,
                model: None,
                provider: None,
                account_id: None,
                reasoning_effort: None,
                temperature: None,
                context_window: None,
                todos: None,
                message_count: 0,
                last_active_at: "t".into(),
                tokens: None,
                llm_call_counter: None,
                perf: None,
                model_ratio: None,
                last_compaction_offset: None,
                corrupted: false,
                model_usage: None,
            }
        }
        let owner = SessionScope::user("u-alice");
        let member = SessionScope::User {
            uid: "u-bob".into(),
            chats: vec!["cs-group-1".into()],
        };
        let outsider = SessionScope::User {
            uid: "u-carol".into(),
            chats: vec!["cs-other".into()],
        };
        // §8.3: a removed participant — the binding is no longer in their
        // scope set. This is the exact shape the Gateway injects after
        // `remove_participant`, and it must read as revoked.
        let revoked = SessionScope::user("u-bob");

        let bound = || SessionMeta {
            chat_session_id: Some("cs-group-1".into()),
            visibility: Some(SessionVisibility::Members),
            initiator: Some("u-alice".into()),
            ..session_meta_fixture()
        };
        let m = bound();
        assert!(m.is_readable_by(&owner), "owner reads own session");
        assert!(m.is_readable_by(&member), "chat participant reads");
        assert!(!m.is_readable_by(&outsider), "non-participant hidden");
        assert!(!m.is_readable_by(&revoked), "removed participant revoked");
        assert!(m.is_readable_by(&SessionScope::Unfiltered), "admin reads");

        // Write stays owner-only: membership is a read grant (§8.2), not
        // an ownership transfer.
        assert!(m.is_writable_by(&owner));
        assert!(!m.is_writable_by(&member));
        assert!(!m.is_writable_by(&outsider));

        // Ownerless + Members (a binding without an attributable creator)
        // is readable through the chat path only.
        let unowned = SessionMeta {
            initiator: None,
            ..bound()
        };
        assert!(unowned.is_readable_by(&member));
        assert!(!unowned.is_readable_by(&owner));
        assert!(!unowned.is_writable_by(&member), "membership never writes");

        // `Members` without a binding is inert: nothing to match against,
        // so only owner / admin reach it.
        let unbound = SessionMeta {
            chat_session_id: None,
            ..bound()
        };
        assert!(unbound.is_readable_by(&owner));
        assert!(!unbound.is_readable_by(&member));
    }

    /// The listing filter must run **before** pagination. Filtering the
    /// returned page instead would hand back short pages and a page count
    /// pointing at rows the caller cannot see.
    #[tokio::test]
    async fn scan_filters_by_scope_before_paginating() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-scan-scope-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let conv = dir.join("conversations");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&conv).unwrap();

        let meta = |sid: &str, owner: Option<&str>, v: Option<SessionVisibility>, at: &str| {
            SessionMeta {
                version: 1,
                session_id: sid.into(),
                agent_id: "a".into(),
                created_at: at.into(),
                initiator: owner.map(str::to_string),
                visibility: v,
                title: Some(sid.into()),
                workspace_id: None,
                model: None,
                provider: None,
                account_id: None,
                reasoning_effort: None,
                temperature: None,
                context_window: None,
                todos: None,
                message_count: 0,
                last_active_at: at.into(),
                tokens: None,
                corrupted: false,
                llm_call_counter: None,
            perf: None,
                model_ratio: None,
                last_compaction_offset: None,
            chat_session_id: None,
                model_usage: None,
            }
        };

        // Newest first (last_active_at desc).
        write_session_meta(
            &conv,
            &meta("s-legacy", None, None, "2026-01-03T00:00:00Z"),
        )
        .unwrap();
        write_session_meta(
            &conv,
            &meta(
                "s-bob-private",
                Some("u-bob"),
                Some(SessionVisibility::Private),
                "2026-01-02T00:00:00Z",
            ),
        )
        .unwrap();
        write_session_meta(
            &conv,
            &meta("s-alice", Some("u-alice"), None, "2026-01-01T00:00:00Z"),
        )
        .unwrap();

        let scan = |scope: SessionScope| {
            let conv = conv.clone();
            async move {
                let (sessions, total, _) =
                    scan_sessions_async(conv, Some(1), Some(2), scope).await.unwrap();
                (
                    sessions.into_iter().map(|s| s.session_id).collect::<Vec<_>>(),
                    total,
                )
            }
        };

        // Alice: legacy + her own. Bob's private session is gone, and
        // `total` already reflects that (2, not 3) even though the page
        // size is 2 — the filter runs before pagination.
        let (ids, total) = scan(SessionScope::user("u-alice")).await;
        assert_eq!(ids, vec!["s-legacy", "s-alice"]);
        assert_eq!(total, 2, "total must count the filtered set, not all rows");

        // Bob: everything except Alice's *private* sessions — her session
        // here is public (the default), so he can read it. Privacy is
        // per-session, not a blanket "only show me my own".
        let (ids, total) = scan(SessionScope::user("u-bob")).await;
        assert_eq!(ids, vec!["s-legacy", "s-bob-private"]);
        assert_eq!(total, 3);

        // Unfiltered (admin / local mode): everything, first page only.
        let (ids, total) = scan(SessionScope::Unfiltered).await;
        assert_eq!(ids, vec!["s-legacy", "s-bob-private"]);
        assert_eq!(total, 3, "unfiltered total counts every session");

        // Visibility travels with the row so the client can render the
        // share toggle without a second request.
        let (sessions, _, _) =
            scan_sessions_async(conv.clone(), None, None, SessionScope::Unfiltered)
                .await
                .unwrap();
        assert_eq!(sessions[1].visibility, Some(SessionVisibility::Private));
        assert_eq!(sessions[0].visibility, None);

        // `can_write` travels with the row: it is the frontend's only signal
        // for disabling the session write controls, and it is NOT derivable
        // from visibility — a public session is readable by every account
        // but writable only by its owner.
        let (sessions, _, _) = scan_sessions_async(
            conv.clone(),
            None,
            None,
            SessionScope::user("u-bob"),
        )
        .await
        .unwrap();
        let by_id = |id: &str| {
            sessions
                .iter()
                .find(|s| s.session_id == id)
                .unwrap_or_else(|| panic!("{id} missing from bob's scan"))
        };
        assert!(
            !by_id("s-alice").can_write,
            "alice's public session is readable by bob but not writable by him"
        );
        assert!(by_id("s-bob-private").can_write, "bob owns his session");
        assert!(
            by_id("s-legacy").can_write,
            "ownerless (pre-ADR-076) sessions stay writable by any signed-in account"
        );

        // Admin (Unfiltered) may write everything, including sessions it
        // does not own — this is why the frontend cannot use
        // "owner == me" as its disable condition.
        let (sessions, _, _) =
            scan_sessions_async(conv.clone(), None, None, SessionScope::Unfiltered)
                .await
                .unwrap();
        assert!(
            sessions.iter().all(|s| s.can_write),
            "admin/local may write every session: {:?}",
            sessions
                .iter()
                .map(|s| (&s.session_id, s.can_write))
                .collect::<Vec<_>>()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

}

