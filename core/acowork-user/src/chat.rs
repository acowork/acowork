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

use std::fs;
use std::path::{Path, PathBuf};

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

fn attachment_path(dir: &Path, id: &str) -> PathBuf {
    files_dir(dir).join(id)
}

/// `pub` so the download handler can stream the blob straight from disk
/// without round-tripping the bytes through the metadata reader.
/// The `files/` directory of one conversation or session. `dir` is the
/// owning directory (`chat_sessions::session_dir` since Wave 6.2; the old
/// pair directory before it) — attachment storage is directory-scoped.
fn files_dir(dir: &Path) -> PathBuf {
    dir.join("files")
}

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
pub(crate) fn attachment_thumb_path(dir: &Path, id: &str, ext: &str) -> PathBuf {
    files_dir(dir).join(format!("{id}.thumb.{ext}"))
}

// ── Attachments (ADR-076 §决策 9) ──────────────────────────────────────

/// Write `bytes` as an attachment of the pair `chat_id`, as uploaded by
/// `from`.
pub(crate) fn attachment_exists_at(dir: &Path, id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok() && attachment_meta_path(dir, id).is_file()
}

/// Dir-level [`load_attachment_meta`]: sidecar + blob path from `dir`.
pub(crate) fn load_attachment_meta_at(
    dir: &Path,
    id: &str,
) -> Result<Option<(Attachment, PathBuf)>, String> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Ok(None);
    }
    let meta = attachment_meta_path(dir, id);
    let raw = match fs::read_to_string(&meta) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", meta.display())),
    };
    let attachment: Attachment =
        serde_json::from_str(&raw).map_err(|e| format!("{}: {e}", meta.display()))?;
    Ok(Some((attachment, attachment_blob_path(dir, id))))
}

/// Dir-level [`store_attachment`] body: everything except the participant
/// check. Writes blob first, sidecar second (same crash rule).
pub(crate) fn store_attachment_at(
    dir: &Path,
    filename: &str,
    mime: &str,
    bytes: &[u8],
) -> Result<Attachment, String> {
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

    let fdir = files_dir(dir);
    fs::create_dir_all(&fdir).map_err(|e| format!("{}: {e}", fdir.display()))?;

    // Blob first, metadata second — see the module doc for why.
    let blob = attachment_path(dir, &attachment.id);
    fs::write(&blob, bytes).map_err(|e| format!("{}: {e}", blob.display()))?;

    // Thumbnail third, from the bytes already in hand so nothing is read
    // back off disk. A failure here is not fatal — the sidecar is simply
    // absent and `thumb` stays `None`, which the client already handles.
    let thumb = generate_thumb(dir, &attachment.id, bytes, &attachment.mime, &attachment.filename);
    let attachment = Attachment { thumb, ..attachment };

    let meta = attachment_meta_path(dir, &attachment.id);
    let json = serde_json::to_string(&attachment).map_err(|e| format!("serialize: {e}"))?;
    if let Err(e) = fs::write(&meta, &json) {
        // Do not leave a blob we cannot describe — nor a thumbnail the
        // missing sidecar no longer points at.
        let _ = fs::remove_file(&blob);
        if let Some(ext) = attachment.thumb.as_deref() {
            let _ = fs::remove_file(attachment_thumb_path(dir, &attachment.id, ext));
        }
        return Err(format!("{}: {e}", meta.display()));
    }
    Ok(attachment)
}

/// Read one attachment back: metadata plus bytes.
///
/// A message can only reference attachments of its own conversation, and only
/// ones that really exist — the caller sends ids, never names or sizes.
pub(crate) fn resolve_attachments(dir: &Path, ids: &[String]) -> Result<Vec<Attachment>, String> {
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

/// What the conversation list shows for this message: the body, or the first
/// attachment's name when there is no body (an image with no caption still
/// needs a row in the list). Truncated on a char boundary so a multi-byte
/// string cannot panic.
pub(crate) fn preview(message: &ChatMessage) -> String {
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

    #[test]
    fn an_image_gets_a_jpeg_sidecar_and_a_text_file_gets_its_opening_lines() {
        let dir = temp_dir();

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

        let img = store_attachment_at(&dir, "shot.png", "image/png", &png_bytes).unwrap();
        assert_eq!(img.thumb.as_deref(), Some("jpg"));
        let sidecar = attachment_thumb_path(&dir, &img.id, "jpg");
        assert!(sidecar.exists(), "image sidecar must exist on disk");
        // JPEG magic — proves the bytes are a real thumbnail, not the PNG
        // copied next to itself.
        let jpeg = fs::read(&sidecar).unwrap();
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF]);

        let text = store_attachment_at(
            &dir,
            "notes.md",
            "text/markdown",
            b"\n\n# Title\n\nfirst real line\nsecond line\n",
        )
        .unwrap();
        assert_eq!(text.thumb.as_deref(), Some("txt"));
        // Leading blank lines are dropped so the preview shows content.
        assert_eq!(
            fs::read_to_string(attachment_thumb_path(&dir, &text.id, "txt")).unwrap(),
            "# Title\nfirst real line\nsecond line"
        );

        // A binary gets no sidecar at all — never an empty one.
        let bin = store_attachment_at(&dir, "app.exe", "application/octet-stream", b"MZ\x90\x00")
            .unwrap();
        assert_eq!(bin.thumb, None);
        assert!(!attachment_thumb_path(&dir, &bin.id, "jpg").exists());
    }

    #[test]
    fn an_attachment_cannot_be_borrowed_across_directories() {
        let (d1, d2) = (temp_dir(), temp_dir());
        let stored = store_attachment_at(&d1, "secret.png", "image/png", b"png").unwrap();
        assert!(attachment_exists_at(&d1, &stored.id));
        // The id resolves inside `d1/files/` only: another session must
        // refuse it rather than link someone else's blob.
        assert!(!attachment_exists_at(&d2, &stored.id));
        assert!(load_attachment_meta_at(&d2, &stored.id).unwrap().is_none());
    }

    #[test]
    fn stored_metadata_is_the_services_own_record() {
        let dir = temp_dir();
        // A path and a quote come from the client's `file_name`; neither may
        // survive into storage, because both end up in a response header.
        let a = store_attachment_at(
            &dir,
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

        let (read, blob) = load_attachment_meta_at(&dir, &a.id).unwrap().unwrap();
        assert_eq!(read, a);
        assert_eq!(fs::read(&blob).unwrap(), b"bytes");
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
        store_attachment_at(&dir, "x.png", "image/png", b"png").unwrap();
        // Only a UUID-shaped id reaches the filesystem; everything else
        // resolves to nothing (the API answers 404).
        for probe in [
            "../conversation.json",
            "..",
            "00000000-0000-4000-8000-000000000000",
            "",
        ] {
            assert!(
                load_attachment_meta_at(&dir, probe).unwrap().is_none(),
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
        let too_big = vec![0u8; MAX_IMAGE_BYTES as usize + 1];
        assert!(store_attachment_at(&dir, "big.png", "image/png", &too_big).is_err());
        // The same bytes are a fine *document* — the ceiling follows the
        // declared type, which is why the type is validated first.
        assert!(store_attachment_at(&dir, "big.bin", "application/zip", &too_big).is_ok());
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
        assert!(!content_disposition("a\"b\r\nX: 1").contains("\r\n"));
    }

    #[test]
    fn a_bodyless_message_previews_its_attachment() {
        let att = Attachment {
            id: "1".into(),
            filename: "shot.png".into(),
            mime: "image/png".into(),
            size: 1,
            thumb: None,
        };
        let doc = Attachment {
            filename: "spec.pdf".into(),
            mime: "application/pdf".into(),
            ..att.clone()
        };
        let img_row = ChatMessage {
            seq: 1,
            ts: 0,
            from: A.into(),
            kind: "image".into(),
            body: String::new(),
            attachments: vec![att],
        };
        assert_eq!(preview(&img_row), "[image] shot.png");
        let doc_row = ChatMessage {
            kind: "document".into(),
            attachments: vec![doc],
            ..img_row
        };
        assert_eq!(preview(&doc_row), "[file] spec.pdf");
    }

    #[test]
    fn preview_truncates_on_char_boundaries() {
        let long = "话".repeat(200);
        let row = ChatMessage {
            seq: 1,
            ts: 0,
            from: A.into(),
            kind: "text".into(),
            body: long,
            attachments: Vec::new(),
        };
        let p = preview(&row);
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
        assert!(p.ends_with('…'));
    }

    #[test]
    fn chat_id_is_order_independent() {
        assert_eq!(chat_id(A, B), chat_id(B, A));
        assert_eq!(chat_id(A, B), format!("{A}{SEP}{B}"));
        assert_eq!(parse_chat_id(&chat_id(B, A)), Some((A.into(), B.into())));
        // Non-canonical / malformed ids decode to None (callers 404).
        assert_eq!(parse_chat_id(&format!("{B}{SEP}{A}")), None);
        assert_eq!(parse_chat_id(A), None);
        assert_eq!(parse_chat_id(SEP), None);
    }
}
