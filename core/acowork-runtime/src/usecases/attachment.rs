//! Attachment storage use case (ADR-046).
//!
//! Owns all read/write operations on the unified attachment blob store at
//! `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>`. The
//! companion implementation [`crate::usecases::RuntimeAttachmentService`]
//! is the single audit point for doc-id computation, dedup, atomic
//! persistence, and the `document_id` charset guard (used to defang
//! path-traversal probes).
//!
//! ## Why this is a trait
//!
//! ADR-040: HTTP handlers depend on UseCase traits, never on concrete
//! functions or `std::fs` calls directly. Without this trait, the new
//! `POST /sessions/{sid}/files` and `GET /files/{doc_id}` handlers would
//! re-introduce the "direct-call" antipattern flagged by the
//! MCP-config fix (see [`crate::usecases::AgentToolsService`] for
//! the established pattern).
//!
//! ## What lives here
//!
//! | HTTP endpoint | Trait method | On-disk location |
//! |---|---|---|
//! | `POST /sessions/{sid}/files` (multipart) | [`AttachmentService::upload_file`] | `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>` |
//! | `GET  /files/{document_id}`            | [`AttachmentService::read_file`]   | `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>` (legacy: bare `<document_id>` / `<document_id>.<ext>`) |
//!
//! `DELETE /files/{document_id}` is intentionally **out of scope** —
//! ADR-046 §5 defers file lifecycle management (cleanup, dedup
//! compaction, expiry) to a future document-management feature.
//! Adding `delete_file` now would be YAGNI.
//!
//! ## Wiring
//!
//! The implementation only needs the boot-time `work_dir` (sync, no
//! async resource dependency), so it publishes into the late-bind slot
//! alongside `agent_tools` / `agent_config` in `session_init.rs`
//! Phase B. Pre-Phase-B HTTP probes receive `503 Service Unavailable`
//! from the slot-first handler pattern, matching every other
//! `*Service` handler in this crate.

use async_trait::async_trait;

/// Single-file ceiling for the **single-request** upload route.
///
/// ADR-122 D6: the 50 MiB `MAX_UPLOAD_BYTES` constant is gone — one
/// configurable limit (`acowork_core::upload::max_file_bytes`, default 1 GiB)
/// now governs a file of any size, and the chunked route is how a client
/// actually sends one that big. The multipart route keeps the same limit
/// rather than a stricter one so an older client is not silently broken; in
/// practice the desktop sends anything over one chunk
/// (`acowork_core::upload::CHUNK_BYTES`) through the chunked route (D9).
pub fn max_upload_bytes() -> u64 {
    acowork_core::upload::max_file_bytes()
}

/// Errors a caller of [`AttachmentService`] can receive.
///
/// Each variant maps deterministically to an HTTP status:
/// - [`AttachmentError::ServiceUnavailable`] → 503
/// - [`AttachmentError::PayloadTooLarge`]    → 413
/// - [`AttachmentError::InvalidDocumentId`]  → 400
/// - [`AttachmentError::Persistence`]        → 500
/// - [`AttachmentError::NotFound`]           → 404
#[derive(Debug, thiserror::Error)]
pub enum AttachmentError {
    /// The late-bind slot has not been populated yet (Phase B has not
    /// run). HTTP 503 — the runtime is not ready.
    #[error("attachment service not ready")]
    ServiceUnavailable,

    /// Upload exceeded [`max_upload_bytes`]. HTTP 413.
    #[error("upload too large: {size} bytes (limit {limit})")]
    PayloadTooLarge { size: u64, limit: u64 },

    /// `document_id` is malformed (charset / length violation). HTTP
    /// 400 — protects against path traversal on the read path.
    #[error("invalid document_id: {0:?}")]
    InvalidDocumentId(String),

    /// I/O failure reading or writing the blob store. HTTP 500.
    #[error("attachment persistence error: {0}")]
    Persistence(String),

    /// The blob for `document_id` does not exist on disk. HTTP 404.
    #[error("attachment not found: {0}")]
    NotFound(String),
}

// ── Request / response DTOs ────────────────────────────────────────────────

/// Parameters for [`AttachmentService::upload_file`].
///
/// All fields except `bytes` are required for JSONL persistence: the
/// frontend reads `metadata` to render the chip / thumbnail and the
/// LLM uses the file via tools. `width` / `height` are optional so a
/// non-desktop client can omit them — the renderer falls back to
/// `<img onLoad>` natural sizing (ADR-046 §2.5).
#[derive(Debug, Clone)]
pub struct UploadFileParams {
    /// Filename as supplied by the user (UI hint only; not used as a
    /// filesystem path component).
    pub filename: String,
    /// Lowercase extension without the dot (e.g. "pdf", "png").
    pub format: String,
    /// Raw bytes of the file.
    pub bytes: Vec<u8>,
    /// `width` for image uploads (image files only); `None` for
    /// documents or when the client cannot measure.
    pub width: Option<u32>,
    /// `height` for image uploads; same optionality rules as `width`.
    pub height: Option<u32>,
}

/// Parameters for [`AttachmentService::adopt_staged_file`] (ADR-122).
///
/// The chunked route already merged the chunks into a staging file and
/// verified its digest, so this carries a **path**, not bytes: re-reading a
/// 1 GiB blob into a `Vec` to reuse [`AttachmentService::upload_file`] would
/// undo exactly what the chunked channel exists to avoid.
pub struct StagedUploadParams {
    /// Filename the client declared (UI hint / stem source only).
    pub filename: String,
    /// Lowercase extension without the dot (e.g. "pdf", "png").
    pub format: String,
    /// Merged, digest-verified file. Consumed by the adoption (renamed, or
    /// removed on a dedup hit) — the caller must not use it afterwards.
    pub staged: std::path::PathBuf,
    /// Verified `sha256` of `staged`, lowercase hex. The `document_id` is
    /// derived from it, so no second read pass is needed.
    pub sha256: String,
    /// Verified byte length of `staged`.
    pub size: u64,
    /// `width` / `height` for image uploads; same optionality rules as
    /// [`UploadFileParams`].
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Response for [`AttachmentService::upload_file`].
///
/// Mirrors the `AttachedItem::FileUpload` / `ImageUpload` wire format
/// (camelCase via serde) so the desktop can hand it straight to its
/// upload command without re-marshalling.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadedFileResponse {
    pub document_id: String,
    pub filename: String,
    pub format: String,
    pub size_bytes: u64,
    /// Echo of the optional image dimensions, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

// ── Trait ──────────────────────────────────────────────────────────────────

/// UseCase trait for attachment blob storage (ADR-046).
///
/// Implementations MUST:
/// - place uploaded files at
///   `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>`
///   (no session subdirectory — JSONL is the per-session index),
/// - validate that `document_id` matches `[0-9a-f]{12}-[0-9a-f]{1,4}`
///   before any filesystem access,
/// - persist atomically (write-tmp-rename),
/// - dedup by content hash (same bytes ⇒ same `document_id` ⇒
///   any existing file with that id is returned as-is).
#[async_trait]
pub trait AttachmentService: Send + Sync {
    /// `POST /sessions/{sid}/files` — store a new upload.
    ///
    /// Returns the persisted blob's metadata. The HTTP handler is
    /// expected to forward the response to the desktop verbatim. The
    /// implementation is responsible for:
    /// - size validation ([`max_upload_bytes`]),
    /// - content-hash doc-id computation,
    /// - atomic write-tmp-rename to
    ///   `<work_dir>/files/<sanitized_stem>_<document_id>.<safe_ext>`
    ///   (using [`crate::usecases::attachment::on_disk_name`] so the
    ///   path string the LLM / `doc_reader` receive matches the file
    ///   that actually lands),
    /// - dedup detection (no re-write when any on-disk entry already
    ///   addresses the same `document_id` — the first-uploaded name
    ///   wins).
    async fn upload_file(
        &self,
        params: UploadFileParams,
    ) -> Result<UploadedFileResponse, AttachmentError>;

    /// `GET /files/{document_id}` — read a previously-uploaded blob.
    ///
    /// Returns the raw bytes. The on-disk file extension is the
    /// whitelisted `safe_extension(format)` chosen at upload time, but
    /// this trait cannot tell callers the MIME type — the HTTP layer /
    /// frontend supplies it from the `AttachmentMeta` they already
    /// have on hand when issuing the read.
    async fn read_file(&self, document_id: &str) -> Result<Vec<u8>, AttachmentError>;

    /// `POST /sessions/{sid}/uploads/{upload_id}/complete` — adopt an
    /// already-verified staging file (ADR-122 §3).
    ///
    /// Same landing rules as [`AttachmentService::upload_file`] (atomic
    /// placement, content-derived `document_id`, dedup), but the bytes are
    /// never held in memory: the `document_id` comes from the digest the
    /// chunked store already verified while streaming the merge.
    async fn adopt_staged_file(
        &self,
        params: StagedUploadParams,
    ) -> Result<UploadedFileResponse, AttachmentError>;
}

// ── On-disk naming contract ────────────────────────────────────────────────
//
// The three functions below are the **single source of truth** for how an
// attachment blob lands on disk and how a `document_id` is matched back to
// it. They are pure (no I/O, no async) so they are shared between:
//   - the write path (`attachment_impl::RuntimeAttachmentService::write_blob_atomic`),
//   - the read path (`attachment_impl::RuntimeAttachmentService::read_file`),
//   - the LLM hint builder (`session_task::build_attachment_hint`).
//
// Keeping them in the trait module (rather than scattered across callers)
// prevents the three call sites from drifting on disk-name format —
// the most common bug class here would be `doc_reader` failing because
// the hint string it received doesn't match the actual on-disk name.

/// Maximum length of the sanitised filename stem embedded in the
/// on-disk name.
///
/// Leaves room for the appended `_<document_id>.<ext>` (~22 chars) and
/// the `.<ext>.tmp` rename suffix used by `write_blob_atomic`. 200 chars
/// is well under Windows' 255-char component limit on every platform
/// we ship to.
pub const MAX_STEM_LEN: usize = 200;

/// Whitelist mapping a user-supplied `format` (lowercased, no leading
/// dot — e.g. `"jpg"`, `"pdf"`) to a safe on-disk extension suffix.
///
/// The mapping is whitelisted rather than pass-through so a hostile or
/// accidental format string (e.g. `"exe"`, `"html"`, `"./.."`) never
/// lands as a Finder / shell-trusted extension. Anything not on the
/// whitelist collapses to `"bin"` — the file is still readable, just
/// not double-click-previewable.
///
/// Must stay in lock-step with `doc_reader`'s `detect_format` whitelist
/// (`tools/builtin/doc_reader/mod.rs`). A `docx` blob landing as
/// `.bin` would make `doc_reader` reject it as "Unsupported document
/// format" and the LLM gets a clean error instead of the user's bytes.
pub fn safe_extension(format: &str) -> &'static str {
    match format.to_ascii_lowercase().as_str() {
        "pdf" => "pdf",
        "docx" => "docx",
        "pptx" => "pptx",
        "xlsx" => "xlsx",
        "png" => "png",
        "jpg" | "jpeg" => "jpg",
        "gif" => "gif",
        "webp" => "webp",
        _ => "bin",
    }
}

/// Sanitise a user-supplied filename into a single-component stem that
/// is safe to splice into the on-disk attachment name.
///
/// The transformation is purely textual; it does NOT touch the
/// filesystem. The output is intended to be combined with the
/// content-derived `document_id` and the [`safe_extension`] mapping by
/// [`on_disk_name`].
///
/// Rules:
/// 1. Take the **last path segment** only (`a/b/c.pdf` → `c.pdf`). Path
///    separators are stripped even though multipart form-data clients
///    can include them in the `file_name` part.
/// 2. Strip one trailing extension (`report.pdf` → `report`,
///    `v1.2.final.docx` → `v1.2.final`). Leading-dot names like
///    `.gitignore` are kept whole (their "extension" is the whole name).
/// 3. Replace each unsafe char (`/ \ : * ? " < > |` plus any Unicode
///    control char) with `_`.
/// 4. Strip trailing dots / spaces — Windows silently drops them, so
///    leaving them in would desync the on-disk name from the path
///    string handed to `doc_reader` and the LLM.
/// 5. Avoid the Windows reserved device names (`CON`, `PRN`, …) by
///    appending a trailing `_` to the stem.
/// 6. Cap stem length at [`MAX_STEM_LEN`] Unicode scalar values. Excess
///    chars are dropped from the tail.
/// 7. Fall back to `"file"` when the result is empty.
pub fn sanitize_stem(filename: &str) -> String {
    // (1) last path segment + trim.
    let last = filename.rsplit(['/', '\\']).next().unwrap_or("").trim();

    // (2) strip one trailing extension, but only when the LAST dot is
    // not at position 0 — a leading-dot name like `.gitignore` has
    // no real extension to strip (the dot is the hidden-file marker).
    let without_ext = match last.rfind('.') {
        Some(0) => last,
        Some(i) if i > 0 => &last[..i],
        None => last,
        // Unreachable: rfind returned Some(0) is handled above; this
        // branch exists only to keep the match exhaustive.
        _ => last,
    };

    // (3) replace unsafe chars.
    let mut cleaned: String = without_ext
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();

    // (4) trim trailing dots / spaces (Windows silently drops them).
    while cleaned.ends_with('.') || cleaned.ends_with(' ') {
        cleaned.pop();
    }

    // (5) dodge Windows reserved device names.
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.iter().any(|r| cleaned.eq_ignore_ascii_case(r)) {
        cleaned.push('_');
    }

    // (6) length cap.
    if cleaned.chars().count() > MAX_STEM_LEN {
        cleaned = cleaned.chars().take(MAX_STEM_LEN).collect();
    }

    // (7) empty fallback.
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned
    }
}

/// Build the canonical on-disk attachment filename.
///
/// Format: `<sanitized_stem>_<document_id>.<safe_ext>`, e.g.
/// `2024年度报告_ab12cd34ef56-7890.pdf`.
///
/// The `document_id` is the content-hash key that the rest of the
/// system (JSONL metadata, HTTP URLs, `doc_reader` path hints)
/// addresses blobs by; appending it AFTER the human-readable stem
/// keeps the workspace file tree readable while preserving the
/// existing dedup invariant
/// ("same content ⇒ same `document_id` ⇒ `read_file` resolves to one
/// blob via the matching predicate in [`name_matches_document_id`]").
pub fn on_disk_name(document_id: &str, format: &str, filename: &str) -> String {
    format!(
        "{}_{}.{}",
        sanitize_stem(filename),
        document_id,
        safe_extension(format),
    )
}

/// Decide whether an on-disk filename addresses `document_id`.
///
/// Accepts three shapes so existing pre-change blobs stay readable
/// alongside new writes:
///   1. Legacy bare `<document_id>` (no extension).
///   2. Legacy suffixed `<document_id>.<ext>` (the previous default).
///   3. New `<sanitized_stem>_<document_id>.<ext>` (current default).
///
/// The `_` separator in shape (3) is what distinguishes the new
/// format from a pathologically-named `<document_id>.<ext>` (shape 2)
/// — both would have the same extension-stripped form if not for the
/// underscore.
pub fn name_matches_document_id(name: &str, document_id: &str) -> bool {
    // Strip one trailing extension to land on the "bare" name. Hidden
    // names like `.gitignore` (only dot is at index 0) are kept whole.
    let bare = match name.rfind('.') {
        Some(0) => name,
        Some(i) => &name[..i],
        None => name,
    };
    if bare == document_id {
        return true;
    }
    // New format: `<stem>_<id>[.<ext>]`. The stem itself may end in
    // underscores (e.g. `foo_`), producing `foo__<id>` on disk — we
    // don't try to distinguish "stem ends in _" from "single
    // separator" since `document_id` is content-derived (48+16 bits
    // of entropy) and dedup guarantees one match per id.
    bare.len() > document_id.len() + 1 && bare.ends_with(&format!("_{document_id}"))
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Whitelisted formats get a real extension; everything else gets `bin`.
    /// This guards against a hostile / accidental `"exe"` / `"html"` slipping
    /// through into a Finder-trusted extension.
    #[test]
    fn safe_extension_is_whitelisted() {
        for (input, expected) in [
            ("jpg", "jpg"),
            ("JPG", "jpg"),
            ("jpeg", "jpg"),
            ("png", "png"),
            ("gif", "gif"),
            ("webp", "webp"),
            ("pdf", "pdf"),
            ("PDF", "pdf"),
            ("docx", "docx"),
            ("DOCX", "docx"),
            ("pptx", "pptx"),
            ("PPTX", "pptx"),
            ("xlsx", "xlsx"),
            ("XLSX", "xlsx"),
            ("exe", "bin"),
            ("html", "bin"),
            ("", "bin"),
            ("../../etc/passwd", "bin"),
            ("./jsp", "bin"),
        ] {
            assert_eq!(
                safe_extension(input),
                expected,
                "format={input:?} → expected {expected:?}"
            );
        }
    }

    #[test]
    fn sanitize_stem_basic() {
        assert_eq!(sanitize_stem("report.pdf"), "report");
        assert_eq!(sanitize_stem("2024年度报告.pdf"), "2024年度报告");
        assert_eq!(sanitize_stem("v1.2.final.docx"), "v1.2.final");
        // No extension: kept as-is.
        assert_eq!(sanitize_stem("notes"), "notes");
        // Hidden file with no other extension → kept whole (the
        // leading dot is the "hidden" marker, not an extension).
        assert_eq!(sanitize_stem(".gitignore"), ".gitignore");
        // Hidden file with extension: the leading-dot stays and the
        // trailing extension is stripped, so the stem looks like a
        // hidden file with no extension.
        assert_eq!(sanitize_stem(".bashrc.txt"), ".bashrc");
    }

    #[test]
    fn sanitize_stem_path_components() {
        // Take the last segment only — multipart clients can send
        // paths here even though the protocol doesn't require them.
        assert_eq!(sanitize_stem("C:\\Users\\me\\report.pdf"), "report");
        assert_eq!(sanitize_stem("/var/tmp/notes.txt"), "notes");
        assert_eq!(sanitize_stem("a/b/c.docx"), "c");
        assert_eq!(sanitize_stem("a/b/c"), "c");
    }

    #[test]
    fn sanitize_stem_unsafe_chars() {
        // / \ : * ? " < > | replaced with _
        assert_eq!(sanitize_stem("a:b*c?.pdf"), "a_b_c_");
        assert_eq!(sanitize_stem("foo\"bar.pdf"), "foo_bar");
        assert_eq!(sanitize_stem("foo<bar>baz.pdf"), "foo_bar_baz");
        assert_eq!(sanitize_stem("foo|bar.pdf"), "foo_bar");
        // Control chars: NUL, BEL, US, DEL.
        assert_eq!(sanitize_stem("foo\x00\x07\x1f\x7f.pdf"), "foo____");
    }

    #[test]
    fn sanitize_stem_trailing_dots_and_spaces() {
        // Windows silently drops trailing dots and spaces, so the
        // on-disk name must match the path string we hand to
        // doc_reader — strip them.
        assert_eq!(sanitize_stem("report...   .pdf"), "report");
        assert_eq!(sanitize_stem("foo.  .pdf"), "foo");
        // Internal spaces are kept (only trailing matters on Windows);
        // here the trailing space (before `.pdf`) is stripped along
        // with the extension.
        assert_eq!(sanitize_stem("trailing space .pdf"), "trailing space");
    }

    #[test]
    fn sanitize_stem_windows_reserved_names() {
        for raw in ["CON", "PRN", "AUX", "NUL", "COM1", "LPT9"] {
            let stem = sanitize_stem(&format!("{raw}.pdf"));
            assert!(
                stem.ends_with('_') && !stem.eq_ignore_ascii_case(raw),
                "reserved {raw:?} must dodge collision: got {stem:?}"
            );
        }
        // Case-insensitive match.
        assert_eq!(sanitize_stem("con.pdf"), "con_");
        assert_eq!(sanitize_stem("nul.docx"), "nul_");
        // Names that merely *start with* a reserved token are fine.
        assert_eq!(sanitize_stem("CONV.pdf"), "CONV");
        assert_eq!(sanitize_stem("conf.pdf"), "conf");
    }

    #[test]
    fn sanitize_stem_length_cap() {
        let long = "a".repeat(MAX_STEM_LEN + 50);
        let out = sanitize_stem(&long);
        assert_eq!(out.chars().count(), MAX_STEM_LEN);
    }

    #[test]
    fn sanitize_stem_unicode_preserved() {
        // Chinese, emoji (as multi-byte UTF-8), and accented Latin
        // pass through unchanged — only the unsafe-char pass touches them.
        assert_eq!(sanitize_stem("年度报告.pdf"), "年度报告");
        assert_eq!(sanitize_stem("café.md"), "café");
        assert_eq!(sanitize_stem("🦀-notes.txt"), "🦀-notes");
    }

    #[test]
    fn sanitize_stem_fallback_for_empty() {
        assert_eq!(sanitize_stem(""), "file");
        assert_eq!(sanitize_stem("...."), "file");
        assert_eq!(sanitize_stem("..."), "file");
        assert_eq!(sanitize_stem(".."), "file");
        assert_eq!(sanitize_stem("   "), "file"); // whitespace-only trims to ""
    }

    /// Sanitise-then-re-sanitise must be a fixed point on the no-dot
    /// "clean stem" form: the sanitiser's output (a name with no
    /// extension to strip) fed back in must round-trip unchanged.
    /// Strings that LOOK like they could have an extension (e.g.
    /// `v1.2.final`) intentionally do NOT round-trip — the user-named
    /// "stem" never carries a known extension, so the second pass
    /// strips the trailing `.final` as if it were an extension. This
    /// is acceptable because the sanitiser is invoked once at upload
    /// time; re-sanitisation is never done on an already-cleaned stem.
    #[test]
    fn sanitize_stem_is_idempotent_on_dotless_stems() {
        for raw in [
            "report",
            "2024年度报告",
            "trailing_space_",
            "CON_",
            "café",
            "🦀-notes",
        ] {
            let once = sanitize_stem(raw);
            let twice = sanitize_stem(&once);
            assert_eq!(
                once, twice,
                "not idempotent for {raw:?}: {once:?} vs {twice:?}"
            );
        }
    }

    #[test]
    fn on_disk_name_composes_correctly() {
        assert_eq!(
            on_disk_name("ab12cd34ef56-7890", "pdf", "report.pdf"),
            "report_ab12cd34ef56-7890.pdf",
        );
        // Unicode stem preserved verbatim.
        assert_eq!(
            on_disk_name("ab12cd34ef56-7890", "docx", "年度报告.docx"),
            "年度报告_ab12cd34ef56-7890.docx",
        );
        // Non-whitelisted format collapses to .bin — original ext
        // ("exe") is dropped because safe_extension overrides it.
        assert_eq!(
            on_disk_name("ab12cd34ef56-7890", "exe", "evil.exe"),
            "evil_ab12cd34ef56-7890.bin",
        );
        // No extension in original filename → stem is the whole name.
        assert_eq!(
            on_disk_name("ab12cd34ef56-7890", "bin", "data"),
            "data_ab12cd34ef56-7890.bin",
        );
    }

    #[test]
    fn name_matches_document_id_accepts_legacy_shapes() {
        let id = "ab12cd34ef56-7890";
        // Legacy bare.
        assert!(name_matches_document_id(id, id));
        // Legacy suffixed (pre-change on-disk format).
        assert!(name_matches_document_id(&format!("{id}.pdf"), id));
        assert!(name_matches_document_id(&format!("{id}.docx"), id));
        assert!(name_matches_document_id(&format!("{id}.bin"), id));
    }

    #[test]
    fn name_matches_document_id_accepts_new_shape() {
        let id = "ab12cd34ef56-7890";
        // New default: <stem>_<id>.<ext>.
        assert!(name_matches_document_id(&format!("report_{id}.pdf"), id));
        assert!(name_matches_document_id(&format!("年度报告_{id}.docx"), id));
        // Stem ending in underscore → double-underscore separator; still matches.
        assert!(name_matches_document_id(&format!("foo__{id}.pdf"), id));
    }

    #[test]
    fn name_matches_document_id_rejects_unrelated_names() {
        let id = "ab12cd34ef56-7890";
        // Different id.
        assert!(!name_matches_document_id(
            "report_001122334455-6677.pdf",
            id,
        ));
        // Substring of the id (would be a security concern if it matched).
        assert!(!name_matches_document_id(&format!("{id}x.pdf"), id));
        // Bare name that contains but doesn't end with the id.
        assert!(!name_matches_document_id(&format!("{id}_suffix"), id));
        // Unrelated file.
        assert!(!name_matches_document_id("README.md", id));
        assert!(!name_matches_document_id("", id));
    }
}
