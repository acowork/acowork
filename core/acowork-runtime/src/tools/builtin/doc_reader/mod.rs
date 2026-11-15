//! Document reader sub-modules — format-specific text extraction.
//!
//! Each module handles one document format and exposes a single
//! `extract_text(path, options) -> Result<String>` function.
//!
//! | Format    | Crate        | Strategy                          |
//! |-----------|-------------|-----------------------------------|
//! | PDF       | `pdf-extract` | Font-rendered text extraction    |
//! | DOCX      | `zip`+`quick-xml` | XML text extraction          |
//! | PPTX      | `zip`+`quick-xml` | Slide text extraction        |
//! | XLSX      | `calamine`   | Sheet / row iteration             |
//! | Text      | (std)        | Strict UTF-8 read with NUL sniff  |
//!
//! `text` is the fallback for everything else: Markdown, source code,
//! logs, config files — any UTF-8 file. See [`text`] for the safety
//! gates (size cap, NUL sniff, strict decoding).

pub mod docx;
pub mod pdf;
pub mod pptx;
pub mod text;
pub mod xlsx;

use std::path::Path;

/// Options controlling text extraction behaviour.
#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// Optional start page (1-based, PDF / DOCX / PPTX).
    pub start_page: Option<usize>,
    /// Optional end page (inclusive, PDF / DOCX / PPTX).
    pub end_page: Option<usize>,
    /// Whether to render tables as Markdown (DOCX / XLSX).
    pub include_tables: bool,
    /// Optional 1-based start line for plain-text fallback (`.md`, source
    /// code, logs, etc.).  Ignored by PDF/DOCX/PPTX/XLSX — those use
    /// `start_page`/`end_page` for paging.
    ///
    /// Mirrors [`crate::tools::builtin::file_read::FileReadTool`]
    /// semantics: 1-based, inclusive on both ends.  When set together
    /// with [`end_line`](Self::end_line), returns only that line range.
    /// When unset, the entire file is read and truncated to
    /// [`crate::tools::output::MAX_OUTPUT_BYTES`] (32 KB) by the plain-text
    /// fallback path.
    pub start_line: Option<usize>,
    /// Optional inclusive end line for plain-text fallback.
    /// See [`start_line`](Self::start_line).
    pub end_line: Option<usize>,
}

/// Detect the document format from a file extension.
pub fn detect_format(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("pdf") => Some("pdf"),
        Some("docx") => Some("docx"),
        Some("pptx") => Some("pptx"),
        Some("xlsx") => Some("xlsx"),
        _ => None,
    }
}

// ── Tool trait implementation ───────────────────────────────────────────

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Maximum file size for document reading (50 MB).
const MAX_DOC_SIZE_BYTES: u64 = 50 * 1024 * 1024;

/// Built-in document reader tool.
///
/// Reads PDF, DOCX, PPTX, and XLSX files and extracts their text content.
/// Falls back to UTF-8 plain-text extraction for every other file type
/// (Markdown, source code, logs, ...).
pub struct DocReaderTool;

impl Default for DocReaderTool {
    fn default() -> Self {
        Self::new()
    }
}

impl DocReaderTool {
    pub fn new() -> Self {
        Self
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "doc_reader".to_string(),
            description: "Read and extract text from documents (PDF, DOCX, PPTX, XLSX) and \
                 UTF-8 plain-text files (Markdown, source code, logs, ...). \
                 Use this tool to ingest file content for analysis. \
                 Accepts both relative paths (within workspace) and absolute paths. \
                 Returns plain text with structural markers (page/slide/sheet headers, \
                 optional Markdown tables). \
                 For plain-text files, use start_line / end_line to read a specific \
                 line range (1-based, inclusive) — files larger than 32 KB must be \
                 paged this way. For PDF/DOCX/PPTX/XLSX, use start_page / end_page."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the document file (relative or absolute)"
                    },
                    "start_page": {
                        "type": "integer",
                        "description": "Optional 1-based start page/slide/sheet (default: 1). Used by PDF/DOCX/PPTX/XLSX only; ignored by plain-text fallback."
                    },
                    "end_page": {
                        "type": "integer",
                        "description": "Optional inclusive end page/slide/sheet. Used by PDF/DOCX/PPTX/XLSX only; ignored by plain-text fallback."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Optional 1-based start line for plain-text files (Markdown, source, logs, ...). Ignored by PDF/DOCX/PPTX/XLSX. When set with end_line, reads only that line range (max 100 lines per call)."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Optional inclusive end line for plain-text files. Ignored by PDF/DOCX/PPTX/XLSX. Must be >= start_line."
                    },
                    "include_tables": {
                        "type": "boolean",
                        "description": "Render tables as Markdown (DOCX/XLSX, default: false)"
                    }
                },
                "required": ["path"]
            }),
        }
    }
}

#[async_trait]
impl Tool for DocReaderTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let raw_path = params["path"].as_str().unwrap_or("");
        if raw_path.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Missing 'path' parameter".to_string()),
                token_usage: None,
            });
        }

        // Support both relative and absolute paths. Absolute paths (e.g. from
        // Gateway document upload) bypass work_dir join — see
        // `acowork_core::path_utils::resolve` for the full rule set.
        let full_path = acowork_core::path_utils::resolve(raw_path, work_dir);

        // Size check
        match tokio::fs::metadata(&full_path).await {
            Ok(meta) => {
                if meta.len() > MAX_DOC_SIZE_BYTES {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(format!(
                            "Document too large: {} bytes (limit: {MAX_DOC_SIZE_BYTES} bytes)",
                            meta.len()
                        )),
                        token_usage: None,
                    });
                }
            }
            Err(e) => {
                return Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some(format!("Failed to read file metadata: {e}")),
                    token_usage: None,
                });
            }
        }

        // Build extract options from params. Done *before* format detection
        // because the plain-text fallback (None branch below) needs them
        // for its `start_line`/`end_line` paging contract — the doc
        // extractor branches don't read these fields but the struct is
        // identical across paths.
        let opts = ExtractOptions {
            start_page: params["start_page"].as_u64().map(|v| v as usize),
            end_page: params["end_page"].as_u64().map(|v| v as usize),
            include_tables: params["include_tables"].as_bool().unwrap_or(false),
            start_line: params["start_line"].as_u64().map(|v| v as usize),
            end_line: params["end_line"].as_u64().map(|v| v as usize),
        };

        // Detect format. PDF/DOCX/PPTX/XLSX get their dedicated
        // extractor; anything else falls back to UTF-8 plain-text
        // extraction — this is what makes Markdown / source code / log
        // uploads readable (the blob store collapses unknown extensions
        // to `.bin`, and `safe_extension` never invents new suffixes).
        let format = match detect_format(&full_path) {
            Some(f) => f,
            None => {
                let full_path_clone = full_path.clone();
                let opts_clone = opts.clone();
                let raw = tokio::task::spawn_blocking(move || {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        text::extract_text(&full_path_clone, &opts_clone)
                    }))
                    .map_err(|panic_payload| {
                        let msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                            s.clone()
                        } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                            s.to_string()
                        } else {
                            "Unknown panic during text extraction".to_string()
                        };
                        format!("Text extraction panicked: {msg}")
                    })
                    .and_then(|r| r)
                })
                .await
                .map_err(|join_err| {
                    format!("Text extraction task cancelled or panicked: {join_err}")
                })
                .and_then(|r| r);

                return match raw {
                    Ok(text) => {
                        // The OutputBoundedTool wrapper is the single
                        // source of truth for the 32 KB hard cap. We
                        // deliberately don't write a `was_truncated` hint
                        // into `error` here — the wrapper's
                        // TRUNCATED_OUTPUT_MARKER carries the actionable
                        // guidance for the LLM, and only fires in the
                        // rare case where the inner tool didn't already
                        // truncate to fit.
                        Ok(ToolResult {
                            content_parts: None,
                            ok: true,
                            content: text,
                            error: None,
                            token_usage: None,
                        })
                    }
                    Err(e) => Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(format!(
                            "Unsupported document format: '{}'. Supported: pdf, docx, pptx, xlsx, \
                             or UTF-8 plain text (md, txt, source code, ...). {e}",
                            full_path
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("(none)")
                        )),
                        token_usage: None,
                    }),
                };
            }
        };

        // Dispatch to format-specific extraction on a blocking thread.
        //
        // PDF/DOCX/PPTX/XLSX extraction is inherently CPU-bound and may
        // block for seconds on large or complex documents (e.g. PDFs with
        // embedded fonts / tables).  Running this on a tokio worker thread
        // would starve other async tasks and, worse, a panic inside the
        // extraction crate (e.g. pdf_extract font rendering) would kill
        // the owning tokio task (SessionTask).
        //
        // spawn_blocking isolates the heavy work on a dedicated thread pool
        // and catch_unwind converts any internal panic into a clean error.
        let full_path_clone = full_path.clone();
        let opts_clone = opts.clone();
        let raw = tokio::task::spawn_blocking(move || {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match format {
                "pdf" => pdf::extract_text(&full_path_clone, &opts_clone),
                "docx" => docx::extract_text(&full_path_clone, &opts_clone),
                "pptx" => pptx::extract_text(&full_path_clone, &opts_clone),
                "xlsx" => xlsx::extract_text(&full_path_clone, &opts_clone),
                _ => unreachable!(),
            }))
            .map_err(|panic_payload| {
                let msg = if let Some(s) = panic_payload.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_payload.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "Unknown panic during document extraction".to_string()
                };
                format!("Document extraction panicked: {msg}")
            })
            .and_then(|r| r)
        })
        .await
        .map_err(|join_err| format!("Document extraction task cancelled or panicked: {join_err}"))
        .and_then(|r| r);

        match raw {
            Ok(text) => {
                // The OutputBoundedTool wrapper is the single source of
                // truth for the 32 KB hard cap — no per-tool truncate
                // here. The wrapper's TRUNCATED_OUTPUT_MARKER carries
                // the actionable guidance for the LLM.
                Ok(ToolResult {
                    content_parts: None,
                    ok: true,
                    content: text,
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(e),
                token_usage: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Create a temp file and return `(dir, path)`. Keeping `dir` alive
    /// for the rest of the test guarantees the file exists and that the
    /// directory is cleaned up on drop.
    fn with_temp_file(bytes: &[u8], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("create temp dir");
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(bytes).expect("write temp file");
        (dir, path)
    }

    #[test]
    fn detect_format_returns_none_for_text_extensions() {
        for name in ["notes.md", "main.rs", "Makefile", "app.json", "blob.bin"] {
            assert_eq!(detect_format(Path::new(name)), None, "{name}");
        }
    }

    #[test]
    fn detect_format_still_resolves_documents() {
        for name in ["a.pdf", "b.docx", "c.pptx", "d.xlsx"] {
            assert!(detect_format(Path::new(name)).is_some(), "{name}");
        }
    }

    #[tokio::test]
    async fn execute_reads_text_file_without_extension_whitelist() {
        // End-to-end fallback: a `.md` path (which after an upload lands
        // as `.bin` on disk) must be readable as plain text — this is
        // the contract that makes arbitrary text uploads usable.
        let (_dir, p) = with_temp_file(b"# Hello\n", "notes.md");
        let result = DocReaderTool::new()
            .execute(serde_json::json!({ "path": p.to_string_lossy() }), None)
            .await
            .expect("execute should not fail");
        assert!(result.ok, "text fallback must succeed: {:?}", result.error);
        assert_eq!(result.content, "# Hello\n");
    }

    #[tokio::test]
    async fn execute_rejects_binary_as_unreadable() {
        let (_dir, p) = with_temp_file(b"PK\x03\x04\x00\x00", "fake.zip");
        let result = DocReaderTool::new()
            .execute(serde_json::json!({ "path": p.to_string_lossy() }), None)
            .await
            .expect("execute should not fail");
        assert!(!result.ok);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("Unsupported document format"), "got: {err}");
    }

    // ── E2E: paging contract ─────────────────────────────────────────
    //
    // doc_reader plain-text fallback previously had no way to page.
    // These tests verify:
    //  - The LLM-visible schema exposes start_line / end_line.
    //  - The Tool::execute() path actually honours them end-to-end.
    //  - Whole-file reads of >32 KB files fail cleanly with a paging hint
    //    (the regression that motivated this work — without paging, large
    //    Markdown / log files were unreadable).
    //  - Paging a file larger than the 32 KB cap succeeds (the headline
    //    new feature working through the real entry point).

    #[test]
    fn spec_exposes_start_line_and_end_line_to_llm() {
        // The schema is what the LLM reads. If start_line / end_line
        // are missing or mis-typed, models will never learn to page.
        let spec = DocReaderTool::spec_value();
        let props = spec.input_schema["properties"]
            .as_object()
            .expect("properties object present");

        assert!(
            props.contains_key("start_line"),
            "missing start_line: {props:?}"
        );
        assert!(
            props.contains_key("end_line"),
            "missing end_line: {props:?}"
        );
        assert_eq!(
            props["start_line"]["type"],
            serde_json::Value::String("integer".to_string())
        );
        assert_eq!(
            props["end_line"]["type"],
            serde_json::Value::String("integer".to_string())
        );
        // Both are OPTIONAL (whole-file read is the default; paging is opt-in)
        let start_required = props["start_line"].get("type").is_some()
            && !spec.input_schema["required"]
                .as_array()
                .map(|a| a.iter().any(|v| v == "start_line"))
                .unwrap_or(false);
        assert!(
            start_required,
            "start_line must NOT be required (breaking change risk)"
        );
    }

    #[test]
    fn spec_still_exposes_start_page_and_end_page_for_documents() {
        // Regression guard: adding line params must not remove page params.
        let spec = DocReaderTool::spec_value();
        let props = spec.input_schema["properties"]
            .as_object()
            .expect("properties object present");
        assert!(props.contains_key("start_page"), "start_page must remain");
        assert!(props.contains_key("end_page"), "end_page must remain");
    }

    #[test]
    fn spec_description_teaches_line_paging_for_plain_text() {
        // If the description doesn't say "use start_line/end_line for
        // plain-text", models default to whole-file read and hit the
        // 32 KB cap on real-world files.
        let desc = DocReaderTool::spec_value().description;
        assert!(
            desc.contains("start_line") || desc.contains("line range"),
            "spec description must advertise line paging: {desc}"
        );
    }

    #[tokio::test]
    async fn execute_whole_file_over_32kb_errors_with_paging_hint() {
        // This is the regression that motivated the change: large
        // plain-text uploads were unreadable because the 128 KB cap
        // became 32 KB. The error message must teach the LLM how to
        // recover (pass start_line/end_line).
        let big = "x".repeat(crate::tools::output::MAX_OUTPUT_BYTES + 100);
        let (_dir, p) = with_temp_file(big.as_bytes(), "big.md");
        let result = DocReaderTool::new()
            .execute(serde_json::json!({ "path": p.to_string_lossy() }), None)
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.unwrap_or_default();
        assert!(err.contains("too large"), "got: {err}");
        assert!(
            err.contains("start_line") && err.contains("end_line"),
            "error must teach LLM the paging parameters; got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_paged_read_returns_only_requested_lines() {
        // Paging via the real LLM-style JSON params.
        let mut buf = Vec::new();
        for i in 1..=20 {
            buf.extend_from_slice(format!("line {i}\n").as_bytes());
        }
        let (_dir, p) = with_temp_file(&buf, "twenty.md");
        let result = DocReaderTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 5,
                    "end_line": 10
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok, "paged read should succeed: {:?}", result.error);
        // Use newline-terminated needles so substring collisions can't
        // fool us — "line 1\n" is NOT a prefix of "line 10\n" or "line 11\n".
        for i in 5..=10 {
            let needle = format!("line {i}\n");
            assert!(
                result.content.contains(&needle),
                "missing line {i}: {:?}",
                result.content
            );
        }
        for i in [1, 4, 11, 20] {
            let needle = format!("line {i}\n");
            assert!(
                !result.content.contains(&needle),
                "out-of-range line {i} leaked: {:?}",
                result.content
            );
        }
    }

    #[tokio::test]
    async fn execute_paged_read_works_on_file_larger_than_32kb() {
        // The headline new feature: a 100 KB Markdown file is readable
        // via paging even though whole-file read is rejected. This is
        // what the user actually wanted.
        let big = "x".repeat(100_000); // 100 KB ≫ 32 KB cap
        let (_dir, p) = with_temp_file(big.as_bytes(), "huge.md");
        let result = DocReaderTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 3
                }),
                None,
            )
            .await
            .unwrap();
        assert!(
            result.ok,
            "paged read of 100 KB file must succeed: {:?}",
            result.error
        );
        assert!(result.content.starts_with("xxx"));
    }

    #[tokio::test]
    async fn execute_paged_read_rejects_partial_params() {
        // Both or neither — same contract as the unit-level test, but
        // verified through Tool::execute() (where parameter names come
        // from JSON, not Rust types).
        let (_dir, p) = with_temp_file(b"line 1\nline 2\nline 3\n", "tiny.md");
        let tool = DocReaderTool::new();

        // Only start_line
        let result = tool
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(result.error.as_deref().unwrap().contains("start_line"));
        assert!(result.error.as_deref().unwrap().contains("end_line"));

        // Only end_line
        let result = tool
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "end_line": 2
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
    }

    #[tokio::test]
    async fn execute_paged_read_rejects_range_over_max() {
        // MAX_LINES_PER_CALL is 100; doc_reader's paging contract must
        // mirror file_read's cap (the constants are shared for a reason).
        let mut buf = Vec::new();
        for i in 1..=500 {
            buf.extend_from_slice(format!("L{i}\n").as_bytes());
        }
        let (_dir, p) = with_temp_file(&buf, "long.md");
        let result = DocReaderTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 1,
                    "end_line": 500
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.as_deref().unwrap();
        assert!(err.contains("Range too large"), "got: {err}");
        assert!(
            err.contains("100"),
            "must mention MAX_LINES_PER_CALL: {err}"
        );
        assert!(
            err.contains("Paginate"),
            "must offer paginate template: {err}"
        );
    }

    #[tokio::test]
    async fn execute_paged_read_rejects_start_beyond_file() {
        let (_dir, p) = with_temp_file(b"a\nb\nc\n", "short.md");
        let result = DocReaderTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 100,
                    "end_line": 105
                }),
                None,
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.as_deref().unwrap();
        assert!(err.contains("exceeds file length"), "got: {err}");
        assert!(err.contains("3 lines"), "must report actual length: {err}");
    }

    #[tokio::test]
    async fn execute_paged_read_preserves_utf8_cjk_content() {
        // CJK is the test bed for UTF-8 safety in the paging path.
        let lines = ["中文第一行", "中文第二行", "中文第三行", "中文第四行"];
        let mut buf = Vec::new();
        for l in &lines {
            buf.extend_from_slice(l.as_bytes());
            buf.push(b'\n');
        }
        let (_dir, p) = with_temp_file(&buf, "cjk.md");
        let result = DocReaderTool::new()
            .execute(
                serde_json::json!({
                    "path": p.to_string_lossy(),
                    "start_line": 2,
                    "end_line": 3
                }),
                None,
            )
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.content.contains("中文第二行"));
        assert!(result.content.contains("中文第三行"));
        assert!(!result.content.contains("中文第一行"));
        assert!(!result.content.contains("中文第四行"));
    }
}
