//! User↔user chat persistence (ADR-076 §决策 8; storage & read/write
//! model restructured by ADR-092).
//!
//! One canonical directory per pair, so a conversation has exactly one
//! location and is never duplicated:
//!
//! ```text
//! {data_dir}/users/{min(a,b)}/chats/{max(a,b)}/
//!   conversation.json   participants, last_active_at, unread, version,
//!                       message_count (derived physical-line counter)
//!   messages.jsonl      append-only messages, each carrying a thread-local
//!                       monotonic `seq` (ADR-092 decision 1)
//!   files/{id}          attachment blob (ADR-076 §决策 9)
//!   files/{id}.json     its metadata
//! {data_dir}/users/{uid}/chats.index.json  per-user thread-summary cache
//!                       (ADR-092 decision 5; rebuildable, directory is truth)
//! ```
//!
//! `a`/`b` are compared as byte strings; the order is arbitrary but stable,
//! and `chat_id` is the wire form `min__max`. User ids are UUIDv4
//! (ADR-076 §决策 1), so `__` can never appear inside an id.
//!
//! Attachments live under the pair directory, not per user: a file is
//! addressed by its conversation and only ever read by its two participants,
//! so a second namespace would add a lookup without adding a boundary.
//!
//! The on-disk name is an opaque id, not the uploaded filename — the ADR's
//! sketch (`files/{message_id}_{filename}`) presumes a message id that does
//! not exist yet at upload time, and splicing a user string into a path
//! invites both traversal and name collisions. Metadata is a sidecar, and
//! the blob is written **first**: a crash between the two leaves an orphan
//! blob nobody can reference, never a metadata row pointing at nothing.
//!
//! ponytail: orphan blobs are never reaped. Finite ceiling (a failed upload
//! after a successful blob write), and uploads are authenticated; add a
//! sweep keyed on "blob with no sidecar, older than N days" if a deployment
//! ever cares to reclaim the space.
//!
//! Only reachable under `AUTH_MODE=multi_user` (ADR-076 §决策 12) — nothing
//! here runs, and `data_dir/users/` is never created, in `local` mode.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Separator inside `chat_id` (`min__max`).
pub const SEP: &str = "__";

/// `last_message_preview` length, in characters (ADR-076 §决策 8).
const PREVIEW_CHARS: usize = 80;

/// Upload ceilings, in bytes (ADR-076 §决策 9: images 25 MB, documents
/// 100 MB, no virus scan).
pub const MAX_IMAGE_BYTES: u64 = 25 * 1024 * 1024;
pub const MAX_DOCUMENT_BYTES: u64 = 100 * 1024 * 1024;

/// Cap on the stored filename, in characters. Cosmetic only — the name never
/// reaches a path — but it does reach a response header.
const MAX_FILENAME_CHARS: usize = 200;

/// The size ceiling for a given mime type.
pub fn limit_for(mime: &str) -> u64 {
    if mime.starts_with("image/") {
        MAX_IMAGE_BYTES
    } else {
        MAX_DOCUMENT_BYTES
    }
}

/// Longest edge of a stored image thumbnail, in pixels. 320 px renders
/// sharp in the 160 px chat tile at 2× and keeps a 25 MB screenshot under
/// ~15 KB of JPEG — small enough to inline in `messages.jsonl`.
const THUMB_MAX_EDGE: u32 = 320;

/// Text lines kept in a stored text preview, and the byte ceiling on how much
/// of the file is read to find them. Reading at most 4 KiB means a 100 MB log
/// costs the same as a 2 KiB one; [`THUMB_TEXT_BYTES`] is comfortably more
/// than [`THUMB_TEXT_LINES`] lines of realistic source (80 cols → 320 B/line).
const THUMB_TEXT_LINES: usize = 8;
const THUMB_TEXT_BYTES: usize = 4 * 1024;

/// Extensions whose bytes we are willing to read as UTF-8 text for a preview.
///
/// Mirrors the Desktop's own `isTextReadablePath` whitelist
/// ([monacoLanguage.ts](../../../apps/acowork-desktop/src/lib/monacoLanguage.ts))
/// — a binary with a text-ish extension would be previewed as mojibake here
/// and again as gibberish in the Monaco tab there, so the two lists have to
/// move together. Deliberately an allow-list: anything unlisted (pdf, zip,
/// sqlite, docx) is opaque and gets no preview.
fn is_text_previewable(filename: &str) -> bool {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    const TEXT_EXTS: &[&str] = &[
        "txt",
        "text",
        "log",
        "csv",
        "tsv",
        "env",
        "conf",
        "cfg",
        "properties",
        "patch",
        "diff",
        "md",
        "markdown",
        "json",
        "jsonc",
        "yaml",
        "yml",
        "toml",
        "ini",
        "xml",
        "html",
        "htm",
        "css",
        "js",
        "jsx",
        "ts",
        "tsx",
        "py",
        "rs",
        "go",
        "java",
        "kt",
        "c",
        "h",
        "cpp",
        "hpp",
        "cs",
        "rb",
        "php",
        "sh",
        "bash",
        "zsh",
        "ps1",
        "sql",
        "vue",
        "svelte",
    ];
    TEXT_EXTS.contains(&ext.as_str())
}

/// Build the thumbnail for an uploaded blob and write it beside the blob,
/// returning the token that locates it (`"jpg"` / `"txt"`), or `None` when
/// there is nothing useful to show.
///
/// Both arms are total functions: any failure (an image format the decoder
/// rejects, a file that is not valid UTF-8) yields `None` and the client
/// falls back to the plain file chip. Nothing here may fail the **upload** —
/// a preview is a nicety, the blob is the message.
///
/// `dir` is the conversation directory; the caller owns the write order and
/// the cleanup (see [`store_attachment`]).
fn generate_thumb(
    dir: &Path,
    id: &str,
    bytes: &[u8],
    mime: &str,
    filename: &str,
) -> Option<String> {
    if mime.starts_with("image/") {
        let out = thumb_image(bytes)?;
        fs::write(attachment_thumb_path(dir, id, "jpg"), out).ok()?;
        return Some("jpg".to_string());
    }
    if is_text_previewable(filename) {
        let out = thumb_text(bytes)?;
        fs::write(attachment_thumb_path(dir, id, "txt"), out).ok()?;
        return Some("txt".to_string());
    }
    None
}

/// Downscale to JPEG bytes.
///
/// Same `thumbnail` → JPEG shape as [acowork-pm](../../acowork-pm)'s
/// `generate_thumb`, so both services' thumbnails look the same.
fn thumb_image(bytes: &[u8]) -> Option<Vec<u8>> {
    use image::GenericImageView;
    use std::io::Cursor;

    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = img.dimensions();
    let thumb = if w.max(h) > THUMB_MAX_EDGE {
        img.thumbnail(THUMB_MAX_EDGE, THUMB_MAX_EDGE)
    } else {
        img
    };
    let mut out = Vec::new();
    thumb
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
        .ok()?;
    Some(out)
}

/// The first few non-blank lines, as bytes ready to write.
///
/// Only the first [`THUMB_TEXT_BYTES`] of the blob are read, and only the
/// first [`THUMB_TEXT_LINES`] non-blank lines kept, so a file with a licence
/// header still shows real content. `from_utf8_lossy` on a *truncated*
/// buffer can split a multi-byte char, so the tail bytes that fail to decode
/// become U+FFFD rather than rejecting the whole file.
fn thumb_text(bytes: &[u8]) -> Option<Vec<u8>> {
    let head = &bytes[..bytes.len().min(THUMB_TEXT_BYTES)];
    let text = String::from_utf8_lossy(head);
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(THUMB_TEXT_LINES)
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(lines.join("\n").into_bytes())
}

/// One attachment, as stored in `files/{id}.json` and inside a message.
///
/// Every field is written by [`store_attachment`] and read back verbatim by
/// [`append_message`] — a client only ever sends the `id`, so it cannot claim
/// a size, a mime type, or a name it did not actually upload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Attachment {
    pub id: String,
    pub filename: String,
    pub mime: String,
    pub size: u64,
    /// Which thumbnail sidecar sits next to the blob: `"jpg"` or `"txt"`.
    ///
    /// An image gets a downscaled JPEG; a text file gets its opening lines
    /// as plain text. Both are produced **once, at upload** and served from
    /// `files/{id}.thumb.{ext}` via the download route's `?thumb=1`, so a
    /// client rendering a thread never fetches a whole blob just to decide
    /// what a row looks like — a 25 MB screenshot and a 5 MB log both cost
    /// the same few KB.
    ///
    /// A short token rather than a path on purpose: the handler rebuilds the
    /// location from the validated id, so nothing a client can influence
    /// reaches a path (see [`thumb_sidecar`]).
    ///
    /// `None` when generation was skipped or failed: an image whose format
    /// the decoder rejects, or a binary with no text to show. Clients must
    /// treat it as "no preview" and fall back to the file chip — never as an
    /// error. `skip_serializing_if` keeps it off the wire (and out of
    /// `messages.jsonl`) in the common case of no preview, so a thread of
    /// binaries is no heavier than before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumb: Option<String>,
}

impl Attachment {
    /// Message `kind` implied by the payload: ADR-076 §决策 9 keeps only
    /// `image` and `document` (no voice / video / reaction — YAGNI).
    pub fn kind(&self) -> &'static str {
        if self.mime.starts_with("image/") {
            "image"
        } else {
            "document"
        }
    }
}

/// Collapse a client-supplied `Content-Type` to a bare, safe `type/subtype`.
///
/// The value is stored and later echoed in a response header, so an unvalidated
/// one would be a header-injection primitive. Anything that is not a
/// parameter-free token pair becomes `application/octet-stream`.
fn normalize_mime(raw: &str) -> String {
    fn is_token_char(c: char) -> bool {
        c.is_ascii_alphanumeric() || "!#$&^_.+-".contains(c)
    }
    let bare = raw.split(';').next().unwrap_or("").trim();
    let valid = bare.len() <= 64
        && bare
            .split_once('/')
            .map(|(t, s)| {
                !t.is_empty()
                    && !s.is_empty()
                    && t.chars().all(is_token_char)
                    && s.chars().all(is_token_char)
            })
            .unwrap_or(false);
    if valid {
        bare.to_ascii_lowercase()
    } else {
        "application/octet-stream".to_string()
    }
}

/// Last path segment, free of control characters, quotes and backslashes.
///
/// Path separators are stripped because multipart clients are allowed to send
/// them in `file_name`; the quotes and control characters because this string
/// is interpolated into `Content-Disposition`.
fn safe_filename(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || c == '"' || c == '\\' {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    let capped: String = cleaned.chars().take(MAX_FILENAME_CHARS).collect();
    if capped.is_empty() {
        "file".to_string()
    } else {
        capped
    }
}

/// RFC 6266/5987 `Content-Disposition`, carrying the real name twice: an
/// ASCII-mangled `filename=` for old clients and a percent-encoded
/// `filename*=` (UTF-8) so CJK names survive the round trip.
pub fn content_disposition(filename: &str) -> String {
    let ascii: String = filename
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut pct = String::new();
    for byte in filename.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || "!#$&+-.^_`|~".contains(c) {
            pct.push(c);
        } else {
            pct.push_str(&format!("%{byte:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{pct}")
}

/// One `messages.jsonl` line.
///
/// `from` is forced by the API to the caller's own id — it is never taken
/// from a request body, so a client cannot forge a sender.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// Thread-local monotonic sequence number (ADR-092 decision 1), assigned
    /// by `append_message` as `message_count + 1`, starting at 1.
    ///
    /// This is the client's stable identity for suffix-diff refresh: it never
    /// changes once written, and a page's messages are strictly increasing in
    /// it. History rows written before this field existed deserialize as `0`
    /// (`#[serde(default)]`) and are backfilled to their 1-based physical line
    /// number on read — legal because history lines append in non-decreasing
    /// `ts` order, so the line number is itself a valid monotonic sequence.
    /// Corrupt lines still occupy a physical number, so a `seq` gap is a
    /// permanent, legitimate hole (the client treats it as a safe resync).
    #[serde(default)]
    pub seq: u64,
    pub ts: i64,
    pub from: String,
    /// `"text"` / `"image"` / `"document"`, derived from the attachments.
    #[serde(default = "default_kind")]
    pub kind: String,
    pub body: String,
    /// Resolved server-side from attachment ids (ADR-076 §决策 9).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

fn default_kind() -> String {
    "text".to_string()
}

/// `conversation.json` — conversation metadata, rewritten on every change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Conversation {
    pub chat_id: String,
    /// Exactly two ids, byte-sorted `[min, max]`.
    pub participants: Vec<String>,
    pub created_at: i64,
    pub last_active_at: i64,
    #[serde(default)]
    pub last_message_preview: String,
    /// `user_id -> unread count`. Keyed by id rather than by `a`/`b` so a
    /// participant's count cannot drift when the pair order is recomputed.
    #[serde(default)]
    pub unread: BTreeMap<String, u32>,
    #[serde(default)]
    pub version: u64,
    /// Derived count of **physical lines** in `messages.jsonl` (ADR-092
    /// decision 3). The read path's `total` and the next `seq` both come from
    /// it, so neither needs to scan the file. It is never a second truth: the
    /// JSONL is authoritative, and this is recomputed from it whenever a load
    /// path finds it `0` while the file is non-empty, or an append-time tail
    /// check finds it behind the last durable `seq`.
    #[serde(default)]
    pub message_count: u64,
}

impl Conversation {
    fn new(chat_id: String, lo: &str, hi: &str, now: i64) -> Self {
        Self {
            chat_id,
            participants: vec![lo.to_string(), hi.to_string()],
            created_at: now,
            last_active_at: now,
            last_message_preview: String::new(),
            unread: BTreeMap::new(),
            version: 1,
            message_count: 0,
        }
    }

    pub fn unread_for(&self, user_id: &str) -> u32 {
        self.unread.get(user_id).copied().unwrap_or(0)
    }

    /// The other participant, as seen by `user_id`.
    pub fn peer_of(&self, user_id: &str) -> Option<&str> {
        self.participants
            .iter()
            .find(|p| p.as_str() != user_id)
            .map(String::as_str)
    }
}

// ── Naming ─────────────────────────────────────────────────────────────

/// Byte-sorted pair `(min, max)`.
pub fn ordered<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Canonical `chat_id` for a pair — order-independent.
pub fn chat_id(a: &str, b: &str) -> String {
    let (lo, hi) = ordered(a, b);
    format!("{lo}{SEP}{hi}")
}

/// Decode a wire `chat_id`. `None` for anything non-canonical, which callers
/// surface as 404 rather than leaking whether an id exists.
pub fn parse_chat_id(id: &str) -> Option<(String, String)> {
    let (lo, hi) = id.split_once(SEP)?;
    if lo.is_empty() || hi.is_empty() || lo >= hi {
        return None;
    }
    Some((lo.to_string(), hi.to_string()))
}

/// Directory holding one conversation.
pub fn pair_dir(data_dir: &Path, lo: &str, hi: &str) -> PathBuf {
    data_dir.join("users").join(lo).join("chats").join(hi)
}

fn dir_for(data_dir: &Path, chat_id: &str) -> Option<PathBuf> {
    let (lo, hi) = parse_chat_id(chat_id)?;
    Some(pair_dir(data_dir, &lo, &hi))
}

fn conversation_path(dir: &Path) -> PathBuf {
    dir.join("conversation.json")
}

fn messages_path(dir: &Path) -> PathBuf {
    dir.join("messages.jsonl")
}

fn files_dir(dir: &Path) -> PathBuf {
    dir.join("files")
}

/// Attachment blob. `id` is always a UUID (checked at every trust boundary),
/// so it cannot climb out of `files/`.
fn attachment_path(dir: &Path, id: &str) -> PathBuf {
    files_dir(dir).join(id)
}

/// `pub` so the download handler can stream the blob straight from disk
/// without round-tripping the bytes through the metadata reader.
pub fn attachment_blob_path(dir: &Path, id: &str) -> PathBuf {
    attachment_path(dir, id)
}

fn attachment_meta_path(dir: &Path, id: &str) -> PathBuf {
    files_dir(dir).join(format!("{id}.json"))
}

/// Thumbnail sidecar: `files/{id}.thumb.{ext}`.
///
/// A sibling of the blob and of the `.json`, not a separate tree: the
/// thumbnail shares the attachment's lifetime exactly, so no sweep can leave
/// one behind after the other is gone, and the download handler reaches it
/// with the same id validation as the blob.
fn attachment_thumb_path(dir: &Path, id: &str, ext: &str) -> PathBuf {
    files_dir(dir).join(format!("{id}.thumb.{ext}"))
}

/// Resolve the `thumb` recorded on an attachment to its sidecar, rejecting
/// anything this module would not have written.
///
/// Takes `data_dir` + `chat_id` like every other public reader here
/// ([`load_attachment_meta`], [`load`]) rather than a conversation directory,
/// so the caller cannot hand it the wrong level of the tree and end up
/// probing `data_dir/files/` — a directory that exists in nobody's
/// deployment.
///
/// The extension is a closed set rather than a free string, so a hand-edited
/// sidecar cannot steer the path out of `files/` the way a stored filename
/// could. The id is already UUID-validated by the caller.
pub fn thumb_sidecar(
    data_dir: &Path,
    chat_id: &str,
    id: &str,
    thumb: Option<&str>,
) -> Option<PathBuf> {
    let ext = match thumb? {
        "jpg" => "jpg",
        "txt" => "txt",
        _ => return None,
    };
    Some(attachment_thumb_path(&dir_for(data_dir, chat_id)?, id, ext))
}

// ── Concurrency (ADR-092 decision 3) ───────────────────────────────────

/// Per-thread write locks, keyed by the pair directory. Every read-modify-
/// write of a `conversation.json` (append, mark_read, the read path's
/// `message_count` init) runs under the lock for its own directory, so
/// concurrent sends cannot lose an unread count.
///
/// Registry entries live for the process lifetime; the pair count is bounded
/// by the fleet, so they are never reaped (ponytail: if a deployment ever
/// needs it, evict alongside pair-directory deletion).
static THREAD_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-user index locks (ADR-092 decision 5), keyed by user id. A second,
/// coarser lock because two different threads (A-B, A-C) hold *different*
/// thread locks yet both rewrite `users/A/chats.index.json`. Lock ordering is
/// always thread → user, a one-way hierarchy, so no deadlock cycle exists.
static USER_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// A poisoned lock means a previous holder panicked mid-write; the tmp+rename
/// writes never leave a torn file, so the map/registry itself is the only
/// thing that could be poisoned and reclaiming the guard is safe.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The mutex guarding one pair directory, creating it on first use.
fn thread_lock(dir: &Path) -> Arc<Mutex<()>> {
    let mut reg = lock(&THREAD_LOCKS);
    let e = reg
        .entry(dir.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())));
    Arc::clone(e)
}

/// The mutex guarding one user's index file, creating it on first use.
fn user_lock(uid: &str) -> Arc<Mutex<()>> {
    let mut reg = lock(&USER_LOCKS);
    let e = reg
        .entry(uid.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())));
    Arc::clone(e)
}

// ── Physical-line readers (ADR-092 decision 4) ─────────────────────────

/// Initial reverse-block size for a tail read, and the growth factor per
/// round (ADR-092 decision 4: 64 KiB start, ×8 until enough lines or BOF).
const TAIL_BLOCK: usize = 64 * 1024;
const TAIL_GROW: usize = 8;

/// Count physical lines without parsing them. `BufRead::lines` yields one
/// segment per `\n`-terminated line and treats a missing final newline as a
/// line too, matching the physical-line basis every `seq` / `message_count`
/// computation uses (ADR-092 decision 1 v2).
fn count_physical(path: &Path) -> Result<usize, String> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    Ok(BufReader::new(file).lines().count())
}

/// Read the window of raw lines the page request names, without touching the
/// rest of the file: skip `offset` complete lines counting back from EOF, then
/// take up to `limit` of them (chronological).
///
/// Returns `(window, total_lines)`. `total_lines` is `Some` only when the scan
/// reached the file head — in that case the whole file was read and the count
/// is exact. When it stopped early it is `None` and the caller supplies the
/// total from `message_count`.
///
/// A block boundary can cut a multi-byte UTF-8 char (CJK bodies are the norm),
/// so the leading partial segment of a mid-file window is dropped and only
/// whole `\n`-delimited lines are returned; the caller parses complete bytes.
fn read_window_lines(
    path: &Path,
    offset: usize,
    limit: usize,
) -> Result<(Vec<Vec<u8>>, Option<usize>), String> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), Some(0))),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let size = file
        .metadata()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len() as usize;
    if size == 0 {
        return Ok((Vec::new(), Some(0)));
    }

    // Accumulate suffix chunks, newest first, until we hold enough line
    // terminators to bound `offset + limit` whole lines (one extra newline
    // bounds the topmost line's start) or we reach the head.
    let need = offset + limit + 1;
    let mut chunks: Vec<Vec<u8>> = Vec::new(); // newest-first
    let mut pos = size;
    let mut block = TAIL_BLOCK;
    let mut nl = 0usize;
    let mut hit_bof = false;
    while pos > 0 {
        let start = pos.saturating_sub(block);
        let mut buf = vec![0u8; pos - start];
        file.seek(SeekFrom::Start(start as u64))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        file.read_exact(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        nl += buf.iter().filter(|b| **b == b'\n').count();
        chunks.push(buf);
        pos = start;
        if pos == 0 {
            hit_bof = true;
            break;
        }
        if nl >= need {
            break;
        }
        block = block.saturating_mul(TAIL_GROW);
    }

    // Reassemble in chronological order (chunks are newest-first).
    let mut buf = Vec::with_capacity(size - pos);
    for c in chunks.iter().rev() {
        buf.extend_from_slice(c);
    }
    // `buf` is the bytes [pos, size). Drop the leading partial line unless we
    // started at the file head.
    let body = if hit_bof {
        buf.as_slice()
    } else {
        match buf.iter().position(|b| *b == b'\n') {
            Some(i) => &buf[i + 1..],
            None => &[][..], // no newline in window: everything is a partial line
        }
    };

    let mut lines: Vec<Vec<u8>> = body.split(|b| *b == b'\n').map(|s| s.to_vec()).collect();
    // A trailing newline leaves one empty artifact segment after the last
    // terminator — it is not a message line.
    if lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    let total = if hit_bof { Some(lines.len()) } else { None };

    // Slice the window counting back from the (local) tail.
    let n = lines.len();
    if offset >= n {
        return Ok((Vec::new(), total));
    }
    let end = n - offset;
    let start = end.saturating_sub(limit);
    Ok((lines[start..end].to_vec(), total))
}

/// The `seq` of the last parseable line, read from a 4 KiB tail block, or `0`
/// when the file is empty/all-corrupt/all-legacy. The append-time self-heal
/// (ADR-092 decision 3 v2) uses this to reconcile `message_count` after a
/// crash that wrote a JSONL line but failed to save metadata.
fn tail_last_seq(path: &Path) -> u64 {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => return 0,
    };
    let size = match file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return 0,
    };
    if size == 0 {
        return 0;
    }
    let start = size.saturating_sub(TAIL_BLOCK);
    let mut buf = vec![0u8; size - start];
    if file
        .seek(SeekFrom::Start(start as u64))
        .and_then(|_| file.read_exact(&mut buf))
        .is_err()
    {
        return 0;
    }
    // Walk the block's lines back-to-front; the first one that parses and
    // carries a nonzero `seq` is the last durable sequence number. A partial
    // leading segment (mid-file block start) parses as corrupt and is skipped.
    for line in buf.split(|b| *b == b'\n').rev() {
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        if let Ok(m) = serde_json::from_slice::<ChatMessage>(line)
            && m.seq != 0
        {
            return m.seq;
        }
    }
    0
}

// ── Reads ──────────────────────────────────────────────────────────────

/// Load one conversation by `chat_id`.
pub fn load(data_dir: &Path, chat_id: &str) -> Result<Conversation, String> {
    let dir = dir_for(data_dir, chat_id).ok_or_else(|| "invalid chat_id".to_string())?;
    let path = conversation_path(&dir);
    let raw = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", path.display()))
}

/// One thread's summary as cached in a participant's `chats.index.json`
/// (ADR-092 decision 5). It is the owner's own view of one conversation:
/// `peer_user_id` and `unread_count` are relative to the index's owner.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ChatIndexEntry {
    chat_id: String,
    peer_user_id: String,
    last_active_at: i64,
    last_message_preview: String,
    unread_count: u32,
}

fn index_path(data_dir: &Path, uid: &str) -> PathBuf {
    data_dir.join("users").join(uid).join("chats.index.json")
}

/// Read one user's index. `None` means "missing or corrupt" — both are the
/// signal to rebuild from the directory, since the cache is disposable and
/// the directory is the only truth.
fn read_index(data_dir: &Path, uid: &str) -> Option<Vec<ChatIndexEntry>> {
    let path = index_path(data_dir, uid);
    let Ok(raw) = fs::read_to_string(&path) else {
        return None;
    };
    match serde_json::from_str::<Vec<ChatIndexEntry>>(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "chats index unreadable; will rebuild from directory");
            None
        }
    }
}

/// Write one user's index atomically (tmp+rename), so `list_for`'s lock-free
/// read never sees a torn file.
fn write_index(data_dir: &Path, uid: &str, entries: &[ChatIndexEntry]) -> Result<(), String> {
    let path = index_path(data_dir, uid);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string(entries).map_err(|e| format!("serialize index: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// Project one conversation into an index entry as seen by `uid`.
fn entry_for(convo: &Conversation, uid: &str) -> Option<ChatIndexEntry> {
    Some(ChatIndexEntry {
        chat_id: convo.chat_id.clone(),
        peer_user_id: convo.peer_of(uid)?.to_string(),
        last_active_at: convo.last_active_at,
        last_message_preview: convo.last_message_preview.clone(),
        unread_count: convo.unread_for(uid),
    })
}

/// Rebuild a `Conversation` from an index entry for `user_id`, enough for the
/// list endpoint (which only asks for the peer and the caller's own unread).
fn convo_from_entry(user_id: &str, e: ChatIndexEntry) -> Option<Conversation> {
    let (lo, hi) = parse_chat_id(&e.chat_id)?;
    let mut unread = BTreeMap::new();
    unread.insert(user_id.to_string(), e.unread_count);
    Some(Conversation {
        chat_id: e.chat_id,
        participants: vec![lo, hi],
        created_at: e.last_active_at,
        last_active_at: e.last_active_at,
        last_message_preview: e.last_message_preview,
        unread,
        version: 0,
        message_count: 0,
    })
}

/// Refresh both participants' index files after a conversation change
/// (ADR-092 decision 5). Called under the thread lock; takes each owner's
/// user lock (thread → user ordering). If an index is missing it is left for
/// `list_for` to rebuild from the directory — a partial write here would
/// silently drop the threads that were never touched, so we only ever upsert
/// into an index that already exists and parses.
fn update_index(data_dir: &Path, convo: &Conversation) {
    for uid in &convo.participants {
        let Some(entry) = entry_for(convo, uid) else {
            continue;
        };
        let ulock = user_lock(uid);
        let _g = lock(&ulock);
        let Some(mut entries) = read_index(data_dir, uid) else {
            continue;
        };
        entries.retain(|e| e.chat_id != entry.chat_id);
        entries.push(entry);
        if let Err(e) = write_index(data_dir, uid, &entries) {
            tracing::warn!(user = %uid, error = %e, "chats index update failed (list will rebuild)");
        }
    }
}

/// The authoritative, index-free scan: walk every pair directory and collect
/// the conversations `user_id` participates in. O(fleet); used to rebuild.
fn scan_all_for(data_dir: &Path, user_id: &str) -> Vec<Conversation> {
    let users = data_dir.join("users");
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&users) else {
        return out; // no `users/` yet: no conversations
    };
    for entry in entries.flatten() {
        let chats = entry.path().join("chats");
        let Ok(peers) = fs::read_dir(&chats) else {
            continue;
        };
        for peer in peers.flatten() {
            let path = conversation_path(&peer.path());
            let Ok(raw) = fs::read_to_string(&path) else {
                continue; // peer dir without metadata: skip, never fail the list
            };
            match serde_json::from_str::<Conversation>(&raw) {
                Ok(c) if c.participants.iter().any(|p| p == user_id) => out.push(c),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "skipping unreadable conversation")
                }
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
    out
}

/// Every conversation `user_id` participates in, newest first.
///
/// Reads the caller's own `chats.index.json` — O(self), not O(fleet) (ADR-092
/// decision 5). The pair directory lives under the *smaller* id, so half of a
/// user's threads are not under `users/{user_id}/`; maintaining the per-user
/// index on every change is what removes that asymmetry from the read path.
/// A missing or corrupt index falls back to a full directory scan and rebuilds
/// the cache — the directory stays the only truth.
pub fn list_for(data_dir: &Path, user_id: &str) -> Result<Vec<Conversation>, String> {
    list_for_with_scan(data_dir, user_id, scan_all_for)
}

/// Resolve a user's conversation list, rebuilding a missing/corrupt index from
/// the authoritative pair directories. The scanner is injectable so the
/// scan/write ordering can be tested deterministically under concurrency.
fn list_for_with_scan(
    data_dir: &Path,
    user_id: &str,
    scan: impl FnOnce(&Path, &str) -> Vec<Conversation>,
) -> Result<Vec<Conversation>, String> {
    if let Some(entries) = read_index(data_dir, user_id) {
        let mut out: Vec<Conversation> = entries
            .into_iter()
            .filter_map(|e| convo_from_entry(user_id, e))
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
        return Ok(out);
    }

    // Rebuild is one user-index transaction. Taking the user lock before the
    // authoritative scan prevents this stale-snapshot race: a concurrent
    // append could otherwise update a healthy index after our scan, then have
    // its update overwritten by this rebuild. Recheck after locking because a
    // competing list call may have completed the rebuild while we waited.
    let ulock = user_lock(user_id);
    let _g = lock(&ulock);
    if let Some(entries) = read_index(data_dir, user_id) {
        let mut out: Vec<Conversation> = entries
            .into_iter()
            .filter_map(|e| convo_from_entry(user_id, e))
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.last_active_at));
        return Ok(out);
    }

    // A user with no threads still gets an (empty) index file. The scan occurs
    // under the same lock as all index upserts, so an append that races the scan
    // either appears in the snapshot or applies its upsert after the rebuild.
    let convos = scan(data_dir, user_id);
    let entries: Vec<ChatIndexEntry> = convos
        .iter()
        .filter_map(|c| entry_for(c, user_id))
        .collect();
    if let Err(e) = write_index(data_dir, user_id, &entries) {
        tracing::warn!(user = %user_id, error = %e, "chats index rebuild failed");
    }
    Ok(convos)
}

/// Read a page of messages, newest-page-first: `offset` counts *back* from
/// the tail (0 = the most recent message), matching ADR-076 §决策 8.
///
/// Returns `(page_in_chronological_order, total_messages)`.
///
/// Bounded read (ADR-092 decision 4): only the bytes around the requested
/// window are touched — no whole-file parse on the hot path. `total` is the
/// physical-line count from `message_count` (O(1)); the first read of a thread
/// whose counter was never initialized pays one O(N) scan to seed it, then it
/// is O(1). `seq` is backfilled to the physical line number for legacy rows.
pub fn read_messages(
    data_dir: &Path,
    chat_id: &str,
    offset: usize,
    limit: usize,
) -> Result<(Vec<ChatMessage>, usize), String> {
    let dir = dir_for(data_dir, chat_id).ok_or_else(|| "invalid chat_id".to_string())?;
    let path = messages_path(&dir);

    // Fast path reads metadata lock-free: `save` is tmp+rename, so a
    // concurrent writer only ever swaps the whole file, never tears it.
    let known = load(data_dir, chat_id)
        .ok()
        .map(|c| c.message_count)
        .unwrap_or(0);

    let (window, scanned_total) = read_window_lines(&path, offset, limit.max(1))?;

    // Resolve the physical total. Prefer the counter; fall back to a scan we
    // already paid for (head reached), and only then to an explicit count.
    let total: usize = if known > 0 {
        known as usize
    } else if let Some(t) = scanned_total {
        t
    } else if window.is_empty() && offset == 0 {
        0
    } else {
        count_physical(&path)?
    };

    // Seed the counter the first time a non-empty thread is found with it
    // unset (ADR-092 decision 3 v2). Done under the thread lock because it
    // writes `conversation.json`; a missing conversation is left alone.
    if known == 0 && total > 0 {
        let tlock = thread_lock(&dir);
        let _g = lock(&tlock);
        if let Ok(mut convo) = load(data_dir, chat_id)
            && convo.message_count == 0
        {
            convo.message_count = total as u64;
            save(&dir, &convo)?;
        }
    }

    let mut out: Vec<ChatMessage> = Vec::with_capacity(window.len());
    let mut prev_seq: u64 = 0;
    // The window is the `window.len()` physical lines ending at number
    // `total - offset`; walk them forward, numbering each and backfilling seq.
    for (i, raw) in window.iter().enumerate() {
        let phys = total.saturating_sub(offset + window.len() - 1 - i) as u64;
        if raw.iter().all(|b| b.is_ascii_whitespace()) {
            continue; // blank physical line: occupies a number, yields no message
        }
        match serde_json::from_slice::<ChatMessage>(raw) {
            Ok(mut m) => {
                if m.seq == 0 {
                    m.seq = phys; // legacy row: its line number is a valid seq
                }
                if m.seq <= prev_seq {
                    // Non-monotonic on a settled file means disk corruption.
                    // Warn only — never renumber (ADR-092 decision 3 v2).
                    tracing::warn!(
                        path = %path.display(),
                        line = phys,
                        seq = m.seq,
                        prev = prev_seq,
                        "message seq is not monotonic (possible corruption)"
                    );
                }
                prev_seq = m.seq;
                out.push(m);
            }
            // One corrupt line must not brick the history (ADR-076 §决策 8).
            Err(e) => {
                tracing::warn!(path = %path.display(), line = phys, error = %e, "skipping corrupt message line")
            }
        }
    }
    Ok((out, total))
}

// ── Attachments (ADR-076 §决策 9) ──────────────────────────────────────

/// Write `bytes` as an attachment of the pair `chat_id`, as uploaded by
/// `from`.
///
/// `from` is checked against the pair itself rather than against
/// `conversation.json`: the conversation file only appears with the first
/// message, and uploading before sending is the normal flow. `chat_id` is
/// canonical, so being one of its two halves *is* participation.
///
/// `filename` and `mime` are presented by the client; both are sanitised here
/// and the result is authoritative from this point on.
pub fn store_attachment(
    data_dir: &Path,
    chat_id: &str,
    from: &str,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<Attachment, String> {
    let (lo, hi) = parse_chat_id(chat_id).ok_or_else(|| "invalid chat_id".to_string())?;
    if from != lo && from != hi {
        return Err("not a participant".to_string());
    }
    if bytes.is_empty() {
        return Err("attachment is empty".to_string());
    }
    let mime = normalize_mime(mime);
    let limit = limit_for(&mime);
    if bytes.len() as u64 > limit {
        return Err(format!("attachment exceeds {limit} bytes"));
    }

    let attachment = Attachment {
        id: uuid::Uuid::new_v4().to_string(),
        filename: safe_filename(filename),
        mime: mime.clone(),
        size: bytes.len() as u64,
        thumb: None,
    };

    let dir = pair_dir(data_dir, &lo, &hi);
    let fdir = files_dir(&dir);
    fs::create_dir_all(&fdir).map_err(|e| format!("{}: {e}", fdir.display()))?;

    // Blob first, metadata second — see the module doc for why.
    let blob = attachment_path(&dir, &attachment.id);
    fs::write(&blob, bytes).map_err(|e| format!("{}: {e}", blob.display()))?;

    // Thumbnail third, from the bytes already in hand so nothing is read
    // back off disk. A failure here is not fatal — the sidecar is simply
    // absent and `thumb` stays `None`, which the client already handles.
    let thumb = generate_thumb(
        &dir,
        &attachment.id,
        bytes,
        &attachment.mime,
        &attachment.filename,
    );
    let attachment = Attachment {
        thumb,
        ..attachment
    };

    let meta = attachment_meta_path(&dir, &attachment.id);
    let json = serde_json::to_string(&attachment).map_err(|e| format!("serialize: {e}"))?;
    if let Err(e) = fs::write(&meta, &json) {
        // Do not leave a blob we cannot describe — nor a thumbnail the
        // missing sidecar no longer points at.
        let _ = fs::remove_file(&blob);
        if let Some(ext) = attachment.thumb.as_deref() {
            let _ = fs::remove_file(attachment_thumb_path(&dir, &attachment.id, ext));
        }
        return Err(format!("{}: {e}", meta.display()));
    }
    Ok(attachment)
}

/// Read one attachment back: metadata plus bytes.
///
/// `Ok(None)` covers both "no such attachment" and "not a well-formed id" —
/// the caller answers 404 either way, so a probe cannot tell them apart and
/// learn which attachments a conversation holds.
///
/// `id` comes off a URL path, so it is validated as a UUID before it is
/// joined into a filename — that single check is what keeps `../` out.
pub fn load_attachment(
    data_dir: &Path,
    chat_id: &str,
    id: &str,
) -> Result<Option<(Attachment, Vec<u8>)>, String> {
    let dir = dir_for(data_dir, chat_id).ok_or_else(|| "invalid chat_id".to_string())?;
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let meta = attachment_meta_path(&dir, id);
    let raw = match fs::read_to_string(&meta) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", meta.display())),
    };
    let attachment: Attachment =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", meta.display()))?;
    let blob = attachment_path(&dir, id);
    match fs::read(&blob) {
        Ok(bytes) => Ok(Some((attachment, bytes))),
        // Metadata without a blob: the window between the two writes, or a
        // manual delete. Either way there is nothing to serve.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", blob.display())),
    }
}

/// Read attachment metadata without loading the blob, returning the on-disk
/// path so the caller can stream the bytes through `tokio::fs::File`. The
/// download handler prefers this over `load_attachment` so a 100 MiB
/// attachment doesn't pin 100 MiB in the request worker until the response
/// body is built — the whole point of streaming is to let the kernel move
/// data socket → socket without us holding it. Returns the same 404-vs-422
/// shape as `load_attachment`.
pub fn load_attachment_meta(
    data_dir: &Path,
    chat_id: &str,
    id: &str,
) -> Result<Option<(Attachment, PathBuf)>, String> {
    let dir = dir_for(data_dir, chat_id).ok_or_else(|| "invalid chat_id".to_string())?;
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let meta = attachment_meta_path(&dir, id);
    let raw = match fs::read_to_string(&meta) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", meta.display())),
    };
    let attachment: Attachment =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", meta.display()))?;
    Ok(Some((attachment, attachment_blob_path(&dir, id))))
}

/// Whether `id` addresses an attachment of this conversation.
///
/// `append_message` resolves ids itself (so the invariant holds wherever it
/// is called from); this exists only so the API can answer a made-up id with
/// a 4xx instead of letting it read as a server fault.
pub fn attachment_exists(data_dir: &Path, chat_id: &str, id: &str) -> bool {
    dir_for(data_dir, chat_id)
        .map(|dir| uuid::Uuid::parse_str(id).is_ok() && attachment_meta_path(&dir, id).is_file())
        .unwrap_or(false)
}

/// Resolve client-supplied ids to the metadata actually on disk.
///
/// A message can only reference attachments of its own conversation, and only
/// ones that really exist — the caller sends ids, never names or sizes.
fn resolve_attachments(dir: &Path, ids: &[String]) -> Result<Vec<Attachment>, String> {
    ids.iter()
        .map(|id| {
            if uuid::Uuid::parse_str(id).is_err() {
                return Err("invalid attachment id".to_string());
            }
            let meta = attachment_meta_path(dir, id);
            let raw = fs::read_to_string(&meta).map_err(|_| "unknown attachment".to_string())?;
            serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", meta.display()))
        })
        .collect()
}

// ── Writes ─────────────────────────────────────────────────────────────

/// Whether `path` exists, is non-empty, and does NOT end in a newline — the
/// signature of a torn final line left by a crash mid-append. `append_message`
/// prepends a newline before its own record so a torn row is not spliced into
/// (which would lose two rows at once).
fn needs_leading_newline(path: &Path) -> bool {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let len = match f.metadata() {
        Ok(m) => m.len(),
        Err(_) => return false,
    };
    if len == 0 {
        return false;
    }
    if f.seek(SeekFrom::End(-1)).is_err() {
        return false;
    }
    let mut b = [0u8; 1];
    match f.read_exact(&mut b) {
        Ok(()) => b[0] != b'\n',
        Err(_) => false,
    }
}

/// Append a message and bump the recipient's unread count.
///
/// The conversation is created on first write. `attachment_ids` are resolved
/// against this conversation's `files/`; `body` may be empty only when there
/// is something else to show. Returns the stored message (with the
/// server-assigned `ts` and `seq`).
pub fn append_message(
    data_dir: &Path,
    from: &str,
    to: &str,
    body: &str,
    attachment_ids: &[String],
    now: i64,
) -> Result<ChatMessage, String> {
    if from == to {
        return Err("cannot message yourself".to_string());
    }
    let (lo, hi) = ordered(from, to);
    let dir = pair_dir(data_dir, lo, hi);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

    let chat_id = format!("{lo}{SEP}{hi}");
    // The whole load → assign seq → append → save → index sequence is one
    // atomic read-modify-write, so it holds the pair's thread lock for its
    // duration (ADR-092 decision 3). This is what stops concurrent sends from
    // losing an unread count or racing to the same `seq`.
    let tlock = thread_lock(&dir);
    let _g = lock(&tlock);

    let mut convo = match load(data_dir, &chat_id) {
        Ok(c) => c,
        Err(_) => Conversation::new(chat_id.clone(), lo, hi, now),
    };
    let path = messages_path(&dir);

    // Reconcile the counter BEFORE allocating a seq, so a crash window (JSONL
    // line durable, metadata save failed) can never hand out a used seq:
    // - first sight of a non-empty thread with the counter unset → seed it from
    //   the physical line count, or the first post-upgrade message would take
    //   seq 1 and collide with history line 1;
    // - else trust the last durable seq if it has run ahead of the counter
    //   (the append-time tail check — ADR-092 decision 3 v2).
    if convo.message_count == 0 {
        convo.message_count = count_physical(&path)? as u64;
    }
    let last_seq = tail_last_seq(&path);
    if last_seq >= convo.message_count {
        convo.message_count = last_seq;
    }
    let seq = convo.message_count + 1;

    let attachments = resolve_attachments(&dir, attachment_ids)?;
    let kind = attachments
        .first()
        .map(Attachment::kind)
        .unwrap_or("text")
        .to_string();

    let message = ChatMessage {
        seq,
        ts: now,
        from: from.to_string(),
        kind,
        body: body.to_string(),
        attachments,
    };
    let mut line =
        serde_json::to_string(&message).map_err(|e| format!("serialize message: {e}"))?;
    line.push('\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if needs_leading_newline(&path) {
        file.write_all(b"\n")
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    file.write_all(line.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))?;

    convo.message_count = seq;
    convo.last_active_at = now;
    convo.last_message_preview = preview(&message);
    convo.version += 1;
    *convo.unread.entry(to.to_string()).or_insert(0) += 1;
    save(&dir, &convo)?;
    update_index(data_dir, &convo);
    Ok(message)
}

/// Clear `user_id`'s unread counter for a conversation.
pub fn mark_read(data_dir: &Path, chat_id: &str, user_id: &str) -> Result<Conversation, String> {
    let dir = dir_for(data_dir, chat_id).ok_or_else(|| "invalid chat_id".to_string())?;
    // Same read-modify-write discipline as `append_message` (ADR-092 decision 3).
    let tlock = thread_lock(&dir);
    let _g = lock(&tlock);
    let mut convo = load(data_dir, chat_id)?;
    if !convo.participants.iter().any(|p| p == user_id) {
        return Err("not a participant".to_string());
    }
    if convo.unread_for(user_id) != 0 {
        convo.unread.insert(user_id.to_string(), 0);
        convo.version += 1;
        save(&dir, &convo)?;
        update_index(data_dir, &convo);
    }
    Ok(convo)
}

/// What the conversation list shows for this message: the body, or the first
/// attachment's name when there is no body (an image with no caption still
/// needs a row in the list). Truncated on a char boundary so a multi-byte
/// string cannot panic.
fn preview(message: &ChatMessage) -> String {
    let source = if message.body.trim().is_empty() {
        match message.attachments.first() {
            Some(a) if a.kind() == "image" => format!("[image] {}", a.filename),
            Some(a) => format!("[file] {}", a.filename),
            None => String::new(),
        }
    } else {
        message.body.clone()
    };
    let trimmed = source.trim();
    if trimmed.chars().count() <= PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let mut s: String = trimmed.chars().take(PREVIEW_CHARS).collect();
    s.push('…');
    s
}

/// tmp + rename, same durability story as the account store.
fn save(dir: &Path, convo: &Conversation) -> Result<(), String> {
    let path = conversation_path(dir);
    let json =
        serde_json::to_string_pretty(convo).map_err(|e| format!("serialize conversation: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| format!("{}: {e}", tmp.display()))?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-chat-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
    const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
    /// A third account, for the cross-conversation test.
    const C: &str = "cccccccc-0000-4000-8000-000000000003";

    #[test]
    fn attachment_writes_are_confined_to_participants() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        assert!(store_attachment(&dir, &id, C, "x.png", "image/png", b"hi").is_err());
        // A non-canonical pair is not a conversation at all.
        assert!(
            store_attachment(&dir, &format!("{B}{SEP}{A}"), A, "x", "image/png", b"hi").is_err()
        );
        assert!(store_attachment(&dir, &id, A, "x.png", "image/png", b"").is_err());
    }

    #[test]
    fn an_image_gets_a_jpeg_sidecar_and_a_text_file_gets_its_opening_lines() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        let conv = pair_dir(&dir, A, B);

        // A real 2×2 PNG — the generator decodes for real, so a fake header
        // would just prove that `image` refuses junk.
        let png = image::ImageBuffer::from_fn(2, 2, |_, _| image::Rgba([255, 0, 0, 255]));
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(png)
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .unwrap();

        let img = store_attachment(&dir, &id, A, "shot.png", "image/png", &png_bytes).unwrap();
        assert_eq!(img.thumb.as_deref(), Some("jpg"));
        let sidecar = thumb_sidecar(&dir, &id, &img.id, img.thumb.as_deref()).unwrap();
        assert!(sidecar.exists(), "image sidecar must exist on disk");
        // JPEG magic — proves the bytes are a real thumbnail, not the PNG
        // copied next to itself.
        let jpeg = fs::read(&sidecar).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);

        let text = store_attachment(
            &dir,
            &id,
            A,
            "notes.md",
            "text/markdown",
            b"\n\n# Title\n\nfirst real line\nsecond line\n",
        )
        .unwrap();
        assert_eq!(text.thumb.as_deref(), Some("txt"));
        let sidecar = thumb_sidecar(&dir, &id, &text.id, text.thumb.as_deref()).unwrap();
        // Leading blank lines are dropped so the preview shows content.
        assert_eq!(
            fs::read_to_string(&sidecar).unwrap(),
            "# Title\nfirst real line\nsecond line"
        );

        // A binary gets no sidecar at all — never an empty one, and never a
        // hand-edited `thumb` pointing outside `files/`.
        let bin = store_attachment(
            &dir,
            &id,
            A,
            "app.exe",
            "application/octet-stream",
            b"MZ\x90\x00",
        )
        .unwrap();
        assert_eq!(bin.thumb, None);
        assert!(thumb_sidecar(&dir, &id, &bin.id, Some("jpg")).is_some());
        assert!(!attachment_thumb_path(&conv, &bin.id, "jpg").exists());
    }

    #[test]
    fn a_forged_thumb_token_cannot_escape_the_files_directory() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        let conv = pair_dir(&dir, A, B);
        // A hand-edited sidecar carrying a traversal or an unknown extension
        // must resolve to `None`, not to a path built from the string.
        for forged in ["../../secrets", "jpg/../../..", "png", "", "JPG"] {
            assert!(
                thumb_sidecar(&dir, &id, A, Some(forged)).is_none(),
                "{forged:?} must not resolve to a sidecar"
            );
        }
        // Only the two tokens the generator writes are accepted, and each
        // stays inside that conversation's own `files/`.
        for good in ["jpg", "txt"] {
            let p = thumb_sidecar(&dir, &id, A, Some(good)).unwrap();
            assert!(p.starts_with(files_dir(&conv)));
            assert_eq!(p.file_name().unwrap(), format!("{A}.thumb.{good}").as_str());
        }
        // A `chat_id` that does not parse resolves to `None` rather than a
        // path at the data root.
        assert!(thumb_sidecar(&dir, "not-a-chat-id", A, Some("jpg")).is_none());
    }

    #[test]
    fn an_attachment_cannot_be_borrowed_by_another_conversation() {
        let dir = temp_dir();
        let ab = chat_id(A, B);
        let stored = store_attachment(&dir, &ab, A, "secret.png", "image/png", b"png").unwrap();

        // Same uploader, different peer: the id resolves inside `A-B/files/`
        // only, so `A-C` must refuse it rather than link another chat's blob.
        let ac = chat_id(A, C);
        assert!(!attachment_exists(&dir, &ac, &stored.id));
        assert!(append_message(&dir, A, C, "look", std::slice::from_ref(&stored.id), 1).is_err());
        // And the conversation it does belong to accepts it.
        let msg = append_message(&dir, A, B, "look", std::slice::from_ref(&stored.id), 1).unwrap();
        assert_eq!(msg.kind, "image");
        assert_eq!(msg.attachments[0].filename, "secret.png");
    }

    #[test]
    fn stored_metadata_is_the_gateways_own_record() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        // A path and a quote come from the client's `file_name`; neither may
        // survive into storage, because both end up in a response header.
        let a = store_attachment(
            &dir,
            &id,
            A,
            "../../etc/pa\"sswd.html",
            "text/html; charset=utf-8",
            b"bytes",
        )
        .unwrap();
        assert_eq!(a.filename, "pa_sswd.html");
        // Parameters are dropped; the bare type is kept because it is a valid
        // token pair. Serving it is made safe at the other end (attachment +
        // nosniff), not by refusing to remember it.
        assert_eq!(a.mime, "text/html");
        assert_eq!(a.kind(), "document");
        assert_eq!(a.size, 5);

        let (read, bytes) = load_attachment(&dir, &id, &a.id).unwrap().unwrap();
        assert_eq!(read, a);
        assert_eq!(bytes, b"bytes");
    }

    #[test]
    fn an_injectable_mime_becomes_octet_stream() {
        assert_eq!(
            normalize_mime("image/png\r\nX-Evil: 1"),
            "application/octet-stream"
        );
        assert_eq!(normalize_mime("nope"), "application/octet-stream");
        assert_eq!(normalize_mime(""), "application/octet-stream");
        assert_eq!(normalize_mime("image/svg+xml"), "image/svg+xml");
        assert_eq!(normalize_mime("IMAGE/PNG"), "image/png");
    }

    #[test]
    fn attachment_paths_cannot_be_traversed_or_probed() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        store_attachment(&dir, &id, A, "x.png", "image/png", b"png").unwrap();
        // A well-formed pair whose conversation.json exists, so the only
        // thing rejecting these is the id check itself.
        append_message(&dir, A, B, "hi", &[], 1).unwrap();
        for probe in [
            "../conversation.json",
            "..",
            "00000000-0000-4000-8000-000000000000",
            "",
        ] {
            assert!(
                load_attachment(&dir, &id, probe).unwrap().is_none(),
                "probe {probe:?} should resolve to nothing"
            );
        }
    }

    #[test]
    fn ceilings_split_on_the_mime_they_were_uploaded_as() {
        assert_eq!(limit_for("image/png"), MAX_IMAGE_BYTES);
        assert_eq!(limit_for("application/pdf"), MAX_DOCUMENT_BYTES);
        const { assert!(MAX_IMAGE_BYTES < MAX_DOCUMENT_BYTES) };

        let dir = temp_dir();
        let id = chat_id(A, B);
        let too_big = vec![0u8; MAX_IMAGE_BYTES as usize + 1];
        assert!(store_attachment(&dir, &id, A, "big.png", "image/png", &too_big).is_err());
        // The same bytes are a fine *document* — the ceiling follows the
        // declared type, which is why the type is validated first.
        assert!(store_attachment(&dir, &id, A, "big.bin", "application/zip", &too_big).is_ok());
    }

    #[test]
    fn content_disposition_survives_a_cjk_filename() {
        let d = content_disposition("设计稿 v2.png");
        // Header-safe ASCII fallback, and the real name percent-encoded.
        assert!(!d.contains('\u{8bbe}'));
        assert!(d.contains("filename*=UTF-8''%E8%AE%BE%E8%AE%A1%E7%A8%BF%20v2.png"));
        assert!(d.starts_with("attachment; filename=\""));
        // The stored name already has quotes and controls stripped, but the
        // formatter must not be the only thing standing between them and a
        // response header.
        assert!(!content_disposition("a\"b\nc").contains("a\"b"));
    }

    #[test]
    fn a_bodyless_message_previews_its_attachment() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        let a = store_attachment(&dir, &id, A, "shot.png", "image/png", b"png").unwrap();
        let msg = append_message(&dir, A, B, "   ", &[a.id], 1).unwrap();
        assert_eq!(msg.body, "   ");
        let convo = load(&dir, &id).unwrap();
        assert_eq!(convo.last_message_preview, "[image] shot.png");
    }

    #[test]
    fn chat_id_is_order_independent() {
        assert_eq!(chat_id(A, B), chat_id(B, A));
        assert_eq!(chat_id(A, B), format!("{A}{SEP}{B}"));
        assert_eq!(parse_chat_id(&chat_id(B, A)), Some((A.into(), B.into())));
        // Non-canonical / malformed ids decode to `None` (callers 404).
        assert_eq!(parse_chat_id(&format!("{B}{SEP}{A}")), None);
        assert_eq!(parse_chat_id(A), None);
        assert_eq!(parse_chat_id(SEP), None);
    }

    #[test]
    fn append_then_read_round_trips_and_bumps_unread() {
        let dir = temp_dir();
        let id = chat_id(A, B);

        append_message(&dir, A, B, "hello", &[], 100).unwrap();
        append_message(&dir, B, A, "hi there", &[], 200).unwrap();

        let (page, total) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(total, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].from, A);
        assert_eq!(page[1].body, "hi there");
        assert_eq!(page[0].kind, "text");

        let convo = load(&dir, &id).unwrap();
        assert_eq!(convo.participants, vec![A.to_string(), B.to_string()]);
        assert_eq!(convo.unread_for(B), 1); // A's message is unread for B
        assert_eq!(convo.unread_for(A), 1); // B's reply is unread for A
        assert_eq!(convo.last_active_at, 200);
        assert_eq!(convo.last_message_preview, "hi there");

        mark_read(&dir, &id, B).unwrap();
        let convo = load(&dir, &id).unwrap();
        assert_eq!(convo.unread_for(B), 0);
        assert_eq!(convo.unread_for(A), 1); // untouched
    }

    #[test]
    fn both_participants_see_the_same_conversation() {
        let dir = temp_dir();
        append_message(&dir, A, B, "one", &[], 10).unwrap();

        for me in [A, B] {
            let list = list_for(&dir, me).unwrap();
            assert_eq!(list.len(), 1, "{me} should see one conversation");
            assert_eq!(list[0].chat_id, chat_id(A, B));
            let peer = list[0].peer_of(me).unwrap();
            assert_eq!(peer, if me == A { B } else { A });
        }
        // A third party sees nothing.
        assert!(
            list_for(&dir, "cccccccc-0000-4000-8000-000000000003")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paging_walks_back_from_the_tail() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        for i in 0..10 {
            append_message(&dir, A, B, &format!("m{i}"), &[], 100 + i).unwrap();
        }

        let (page, total) = read_messages(&dir, &id, 0, 3).unwrap();
        assert_eq!(total, 10);
        assert_eq!(
            page.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            vec!["m7", "m8", "m9"]
        );

        let (page, _) = read_messages(&dir, &id, 8, 5).unwrap();
        assert_eq!(
            page.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            vec!["m0", "m1"]
        );

        // Paging past the head is empty, not an error.
        let (page, _) = read_messages(&dir, &id, 99, 5).unwrap();
        assert!(page.is_empty());
    }

    #[test]
    fn corrupt_line_is_skipped_not_fatal() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        append_message(&dir, A, B, "first", &[], 1).unwrap();
        let path = messages_path(&pair_dir(&dir, A, B));
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{ not json\n").unwrap();
        drop(f);
        append_message(&dir, B, A, "third", &[], 3).unwrap();

        let (page, total) = read_messages(&dir, &id, 0, 50).unwrap();
        // The corrupt line yields no message and carries no `seq`, so the tail
        // check reconciles `message_count` to the last durable `seq`: the
        // corrupt row is skipped, `total` counts the two real messages, and the
        // surviving rows keep dense monotonic `seq` (ADR-092 decision 3).
        assert_eq!(total, 2);
        assert_eq!(page.len(), 2);
        assert_eq!(page[1].body, "third");
        assert_eq!(page[0].seq, 1);
        assert_eq!(page[1].seq, 2);
    }

    #[test]
    fn preview_truncates_on_char_boundaries() {
        let dir = temp_dir();
        let long = "话".repeat(200);
        let msg = append_message(&dir, A, B, &long, &[], 1).unwrap();
        assert_eq!(msg.body, long); // stored in full
        let convo = load(&dir, &chat_id(A, B)).unwrap();
        assert_eq!(
            convo.last_message_preview.chars().count(),
            PREVIEW_CHARS + 1
        );
        assert!(convo.last_message_preview.ends_with('…'));
    }

    #[test]
    fn self_message_is_rejected() {
        let dir = temp_dir();
        assert!(append_message(&dir, A, A, "hey me", &[], 1).is_err());
        assert!(load(&dir, &chat_id(A, A)).is_err());
    }

    #[test]
    fn mark_read_requires_participation() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        append_message(&dir, A, B, "hi", &[], 1).unwrap();
        assert!(mark_read(&dir, &id, "cccccccc-0000-4000-8000-000000000003").is_err());
        assert!(load(&dir, "not-a-chat-id").is_err());
    }

    // ── ADR-092: seq, bounded reads, self-heal, index ──────────────────

    #[test]
    fn seq_is_monotonic_and_echoed_on_send_and_read() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        let m1 = append_message(&dir, A, B, "one", &[], 1).unwrap();
        let m2 = append_message(&dir, B, A, "two", &[], 2).unwrap();
        assert_eq!(m1.seq, 1, "the send echo carries the assigned seq");
        assert_eq!(m2.seq, 2);

        let (page, total) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(total, 2);
        assert_eq!(page.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn a_legacy_seq_zero_row_is_backfilled_to_its_physical_line() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        // Two rows written before `seq` existed: no field, deserialize as 0.
        fs::create_dir_all(pair_dir(&dir, A, B)).unwrap();
        let path = messages_path(&pair_dir(&dir, A, B));
        fs::write(
            &path,
            b"{\"ts\":1,\"from\":\"a\",\"kind\":\"text\",\"body\":\"old1\"}\n{\"ts\":2,\"from\":\"b\",\"kind\":\"text\",\"body\":\"old2\"}\n",
        )
        .unwrap();
        let mut convo = Conversation::new(id.clone(), A, B, 2);
        convo.message_count = 2; // seed meta as the migration path would
        save(&pair_dir(&dir, A, B), &convo).unwrap();

        let (page, _) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(page.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 2]);

        // The next append must not reuse a seq: it lands on 3, not 1.
        let m = append_message(&dir, A, B, "new", &[], 3).unwrap();
        assert_eq!(m.seq, 3);
    }

    #[test]
    fn upgrading_a_thread_with_history_seeds_the_counter_before_allocating() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        // History exists but `conversation.json` predates `message_count` (= 0).
        fs::create_dir_all(pair_dir(&dir, A, B)).unwrap();
        let path = messages_path(&pair_dir(&dir, A, B));
        fs::write(&path, "").unwrap();
        for i in 0..5 {
            let line = serde_json::to_string(&ChatMessage {
                seq: 0,
                ts: i,
                from: A.to_string(),
                kind: "text".into(),
                body: format!("h{i}"),
                attachments: vec![],
            })
            .unwrap();
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(format!("{line}\n").as_bytes())
                .unwrap();
        }
        let mut convo = Conversation::new(id.clone(), A, B, 5);
        convo.message_count = 0; // never initialized
        save(&pair_dir(&dir, A, B), &convo).unwrap();

        let m = append_message(&dir, B, A, "post-upgrade", &[], 6).unwrap();
        assert_eq!(
            m.seq, 6,
            "the append path must seed from physical lines first"
        );
        let (_, total) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(total, 6);
    }

    #[test]
    fn a_crash_that_outran_the_counter_never_reuses_a_seq() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        append_message(&dir, A, B, "one", &[], 1).unwrap();
        // Simulate the crash window: a JSONL line durable with seq 2, but
        // metadata saved at message_count 1 (save of the 2nd failed).
        let path = messages_path(&pair_dir(&dir, A, B));
        let stray = serde_json::to_string(&ChatMessage {
            seq: 2,
            ts: 2,
            from: B.to_string(),
            kind: "text".into(),
            body: "stray".into(),
            attachments: vec![],
        })
        .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{stray}\n").as_bytes())
            .unwrap();
        let mut convo = load(&dir, &id).unwrap();
        convo.message_count = 1; // the regressed counter
        save(&pair_dir(&dir, A, B), &convo).unwrap();

        // The tail check reconciles to last_seq (2) and allocates 3, not 2.
        let m = append_message(&dir, A, B, "three", &[], 3).unwrap();
        assert_eq!(m.seq, 3, "duplicate seq must not reach disk");
        let (page, total) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(total, 3);
        assert_eq!(
            page.iter().map(|x| x.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn a_torn_final_line_is_terminated_before_the_next_append() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        append_message(&dir, A, B, "one", &[], 1).unwrap();
        // A crash mid-write leaves a half line with no trailing newline.
        let path = messages_path(&pair_dir(&dir, A, B));
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"ts\":2,\"from\":\"a\",\"body\":\"torn")
            .unwrap();

        append_message(&dir, B, A, "two", &[], 3).unwrap();
        // The torn row is isolated as its own corrupt physical line (closed
        // with a newline before the new record is written), so the pre-existing
        // message and the new one both survive; the torn row yields no message.
        let (page, total) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(total, 2, "the torn row is not a message");
        assert_eq!(
            page.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
        assert_eq!(page[1].seq, 2);
    }

    #[test]
    fn a_wide_cjk_thread_reads_correctly_across_block_boundaries() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        // Multi-byte bodies big enough that the tail 64 KiB window starts in
        // the middle of a UTF-8 sequence on some row boundary.
        let cjk = "设计稿评审意见汇总".repeat(20); // ~240 bytes of 3-byte chars
        let mut n: usize = 0;
        while n < 600 {
            append_message(
                &dir,
                if n.is_multiple_of(2) { A } else { B },
                if n.is_multiple_of(2) { B } else { A },
                &cjk,
                &[],
                n as i64,
            )
            .unwrap();
            n += 1;
        }
        // The file is comfortably past 64 KiB, so the hot path is a partial read.
        let (page, total) = read_messages(&dir, &id, 0, 50).unwrap();
        assert_eq!(total, n);
        assert_eq!(page.len(), 50);
        assert_eq!(page.last().unwrap().seq, n as u64);
        // Every returned body decoded whole — a split multi-byte char would
        // have failed to parse and shortened the page.
        assert!(page.iter().all(|m| m.body == cjk));

        // Deep paging (`offset` counts back from the tail): 550 back over 600
        // lands on the oldest 50, so those are `seq` 1..=50, still whole rows.
        let (older, _) = read_messages(&dir, &id, 550, 50).unwrap();
        assert_eq!(older.len(), 50);
        assert_eq!(older.first().unwrap().seq, 1);
        assert_eq!(older.last().unwrap().seq, 50);
        assert!(older.iter().all(|m| m.body == cjk));
    }

    #[test]
    fn concurrent_sends_lose_no_unread_and_no_seq() {
        let dir = temp_dir();
        let id = chat_id(A, B);
        // First message seeds `conversation.json` so all threads take the lock.
        append_message(&dir, A, B, "seed", &[], 0).unwrap();

        let mut handles = Vec::new();
        for i in 0..16 {
            let d = dir.clone();
            handles.push(std::thread::spawn(move || {
                let (from, to) = if i % 2 == 0 { (A, B) } else { (B, A) };
                append_message(&d, from, to, &format!("m{i}"), &[], i)
                    .unwrap()
                    .seq
            }));
        }
        let mut seqs: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        seqs.sort_unstable();
        seqs.dedup();
        assert_eq!(seqs.len(), 16, "every append got a distinct seq");
        assert_eq!(seqs[0], 2, "seqs continue after the seed, never restart");

        let convo = load(&dir, &id).unwrap();
        assert_eq!(
            convo.message_count, 17,
            "counter equals the physical line count"
        );
        // The seed and the 8 even-i sends are A→B (B receives 9); the 8 odd-i
        // sends are B→A (A receives 8). 9 + 8 = 17 = message_count, so the
        // thread lock lost no concurrent unread bump.
        assert_eq!(convo.unread_for(B), 9);
        assert_eq!(convo.unread_for(A), 8);
        let (_, total) = read_messages(&dir, &id, 0, 100).unwrap();
        assert_eq!(total, 17);
    }

    #[test]
    fn list_for_serves_from_the_index_and_updates_both_participants() {
        let dir = temp_dir();
        // A-B and A-C: two threads, both must appear in A's index.
        append_message(&dir, A, B, "ab", &[], 10).unwrap();
        append_message(&dir, A, C, "ac", &[], 20).unwrap();
        // First call rebuilds from the directory and seeds the index files.
        assert_eq!(list_for(&dir, A).unwrap().len(), 2);
        assert!(index_path(&dir, A).exists(), "the index is materialized");

        // A second send updates the existing index incrementally (no rebuild).
        append_message(&dir, B, A, "ab2", &[], 30).unwrap();
        let list = list_for(&dir, A).unwrap();
        assert_eq!(list.len(), 2);
        let ab = list.iter().find(|c| c.chat_id == chat_id(A, B)).unwrap();
        assert_eq!(ab.unread_for(A), 1, "A has one unread (the B→A reply)");
        assert_eq!(
            ab.last_active_at, 30,
            "the index row advanced with the send"
        );
    }

    #[test]
    fn a_rebuild_cannot_erase_an_append_that_races_its_scan() {
        use std::sync::Mutex as StdMutex;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;
        use std::time::{Duration, Instant};

        let dir = temp_dir();
        append_message(&dir, A, B, "existing", &[], 10).unwrap();
        let index = index_path(&dir, A);
        let _ = fs::remove_file(&index);

        // Return a deliberately stale but valid directory snapshot while an
        // append races the rebuild. On the old scan-before-lock path, the append
        // sees no index and skips its upsert, so the rebuild loses the thread.
        // With the lock covering scan + rebuild, the append waits at its index
        // upsert and repairs the rebuilt index immediately after lock release.
        let (append_done_tx, append_done_rx) = std::sync::mpsc::channel();
        let index_committed = Arc::new(AtomicBool::new(false));
        let index_committed_out = Arc::clone(&index_committed);
        let index_committed_check = Arc::clone(&index_committed);
        let append_thread: Arc<StdMutex<Option<std::thread::JoinHandle<()>>>> =
            Arc::new(StdMutex::new(None));
        let append_thread_out = Arc::clone(&append_thread);
        let scan_dir = dir.clone();
        let listed = list_for_with_scan(&dir, A, move |data_dir, user_id| {
            let stale_snapshot = scan_all_for(data_dir, user_id);
            let append_dir = scan_dir.clone();
            let done = append_done_tx.clone();
            let index_committed = Arc::clone(&index_committed_out);
            let handle = thread::spawn(move || {
                append_message(&append_dir, A, C, "racing", &[], 20).unwrap();
                index_committed.store(true, Ordering::SeqCst);
                let _ = done.send(());
            });
            *append_thread_out.lock().unwrap() = Some(handle);

            // The metadata write proves append passed its thread lock. Whether
            // its index upsert completes before this scan returns depends on
            // whether the user lock correctly covers the rebuild transaction.
            let created = pair_dir(data_dir, A, C);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !conversation_path(&created).exists() {
                assert!(
                    Instant::now() < deadline,
                    "concurrent append did not commit metadata"
                );
                thread::sleep(Duration::from_millis(1));
            }
            assert!(
                !append_done_rx
                    .recv_timeout(Duration::from_millis(100))
                    .is_ok(),
                "append's index upsert completed while rebuild held the user lock"
            );
            assert!(
                !index_committed_check.load(Ordering::SeqCst),
                "append's index upsert must remain blocked until rebuild releases the lock"
            );
            stale_snapshot
        })
        .unwrap();

        append_thread
            .lock()
            .unwrap()
            .take()
            .expect("scanner spawned the concurrent append")
            .join()
            .unwrap();

        assert_eq!(
            listed.len(),
            1,
            "the scan snapshot predates the racing append"
        );
        let cached = list_for(&dir, A).unwrap();
        assert_eq!(
            cached.len(),
            2,
            "the append must not be lost from the rebuilt index"
        );
        assert!(cached.iter().any(|c| c.chat_id == chat_id(A, C)));
    }

    #[test]
    fn a_corrupt_index_rebuilds_from_the_directory() {
        let dir = temp_dir();
        append_message(&dir, A, B, "hi", &[], 10).unwrap();
        assert_eq!(list_for(&dir, A).unwrap().len(), 1, "seeds the index");
        // Hand-corrupt the cache — the directory is still the truth.
        fs::write(index_path(&dir, A), b"}{ not json").unwrap();

        let list = list_for(&dir, A).unwrap();
        assert_eq!(
            list.len(),
            1,
            "a corrupt index falls back to a directory scan"
        );
        assert_eq!(list[0].chat_id, chat_id(A, B));
        // And the rebuild rewrites a healthy index, so the next read is O(self).
        assert_eq!(list_for(&dir, A).unwrap().len(), 1);
    }
}
