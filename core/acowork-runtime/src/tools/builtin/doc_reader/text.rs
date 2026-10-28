//! Plain-text fallback extraction for the `doc_reader` tool.
//!
//! DOCX/PPTX/XLSX/PDF have dedicated extractors; every other file type
//! (Markdown, source code, logs, config files, ...) lands here. This is
//! what makes ADR-046 uploads of arbitrary text files readable by the
//! LLM: the blob store collapses unknown extensions to `.bin` on disk,
//! and `detect_format` returns `None` for `.bin` — so without this
//! fallback the agent could upload a `.md` but never read it back.
//!
//! ## Safety gates
//!
//! Reading is strict rather than lossy:
//! - **Size cap (whole-file path)** — Whole-file reads enforce
//!   `crate::tools::output::MAX_OUTPUT_BYTES` (32 KB) as a fail-fast gate
//!   on the result, so we don't pull megabytes off disk just to truncate
//!   them at the tool-output boundary. Bigger files must be read
//!   incrementally (shell `head` / `grep`, workspace `read_file`, or via
//!   the paged path below).
//! - **Paged path** — When `start_line` / `end_line` are provided in
//!   [`super::ExtractOptions`], no whole-file size cap is enforced (the
//!   whole point of paging is to read a subset of a larger file). The
//!   line range is validated against
//!   [`crate::tools::builtin::MAX_LINES_PER_CALL`] (100), matching
//!   `file_read`'s contract.
//! - **NUL-byte sniff** — binary files (PNG, ZIP, EXE, ...) frequently
//!   happen to be valid UTF-8; a NUL byte is a reliable cheap signal
//!   that the file is not human-readable text.
//! - **Strict UTF-8** — `String::from_utf8` rejects invalid sequences
//!   (e.g. UTF-16, GBK, arbitrary binary) instead of silently replacing
//!   them with U+FFFD like `from_utf8_lossy` would.

use std::path::Path;

use super::ExtractOptions;

/// True if the bytes contain a NUL byte — a strong signal the file is
/// binary (or UTF-16/UTF-32 text) rather than UTF-8 text.
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.contains(&0)
}

/// Extract text from a plain-text file.
///
/// Behaviour depends on `opts.start_line` / `opts.end_line`:
/// - **Both `Some`** → paged read: validate, slice the requested 1-based
///   inclusive line range, return it. No whole-file size cap (the page
///   range itself is bounded by [`crate::tools::builtin::MAX_LINES_PER_CALL`]).
/// - **One `Some`** → `Err` (both required, or neither).
/// - **Both `None`** → whole-file read: enforce the
///   [`crate::tools::output::MAX_OUTPUT_BYTES`] fail-fast gate, then
///   return everything (with a final output-truncate safety net).
///
/// Returns `Err` with an actionable message when the file is binary, not
/// valid UTF-8, oversize (whole-file path), out of range (paged path),
/// or has invalid range parameters.
pub fn extract_text(path: &Path, opts: &ExtractOptions) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Failed to read text file: {e}"))?;

    if looks_binary(&bytes) {
        return Err(
            "File appears to be binary (contains NUL bytes); only UTF-8 text is supported."
                .to_string(),
        );
    }

    let text = String::from_utf8(bytes).map_err(|e| {
        format!(
            "File is not valid UTF-8 text (invalid byte at offset {}): {e}",
            e.utf8_error().valid_up_to()
        )
    })?;

    match (opts.start_line, opts.end_line) {
        (Some(_), None) | (None, Some(_)) => Err(
            "Both 'start_line' and 'end_line' must be provided together (or neither). \
             For paging, pass both parameters; for whole-file reads, omit both."
                .to_string(),
        ),
        (Some(start), Some(end)) => extract_paged(&text, start, end),
        (None, None) => extract_whole(&text),
    }
}

/// Whole-file path: enforce the size cap, return everything.
fn extract_whole(text: &str) -> Result<String, String> {
    if text.len() > crate::tools::output::MAX_OUTPUT_BYTES {
        return Err(format!(
            "Text file too large: {} bytes (limit: {} bytes). \
             Read it incrementally instead (e.g. `head`, `grep`, workspace `read_file`, \
             or pass start_line/end_line to this tool to page through the file).",
            text.len(),
            crate::tools::output::MAX_OUTPUT_BYTES
        ));
    }

    // No truncate_output here: the OutputBoundedTool wrapper enforces
    // the 32 KB hard cap as the last safety net. doc_reader has its own
    // fail-fast above (Err if file > cap), which is a different
    // decision — "this file is too big, ask the LLM to narrow" rather
    // than "slice this output to fit".
    Ok(text.to_string())
}

/// Paged path: slice the requested 1-based inclusive line range.
fn extract_paged(text: &str, start: usize, end: usize) -> Result<String, String> {
    // Tolerance (mirrors file_read): start_line=0 is a 0-based
    // off-by-one, and a range exactly one line over the cap is the
    // "up to 100 lines" misread — clamp both instead of burning a
    // round-trip on the model.
    let start = start.max(1);
    if end < start {
        return Err(format!("end_line ({end}) must be >= start_line ({start})"));
    }
    let requested = end - start + 1;
    let end = if requested == crate::tools::builtin::MAX_LINES_PER_CALL + 1 {
        start + super::super::MAX_LINES_PER_CALL - 1
    } else if requested > super::super::MAX_LINES_PER_CALL {
        return Err(format!(
            "Range too large: {requested} lines requested, max {} per call. \
             Paginate across multiple calls (e.g. start_line: {s}, end_line: {e1}, \
             then start_line: {e1p1}, end_line: {e2}...)",
            super::super::MAX_LINES_PER_CALL,
            s = start,
            e1 = start + super::super::MAX_LINES_PER_CALL - 1,
            e1p1 = start + super::super::MAX_LINES_PER_CALL,
            e2 = end,
        ));
    } else {
        end
    };

    let ranges = line_byte_ranges(text);
    let total = ranges.len();
    if start > total {
        return Err(format!(
            "start_line ({start}) exceeds file length ({total} lines). \
             File has {total} lines total; adjust start_line."
        ));
    }

    // Inclusive 1-based → exclusive 0-based half-open.
    let s_idx = start - 1;
    let e_idx = (end - 1).min(total - 1);
    let byte_start = ranges[s_idx].0;
    // Include the trailing '\n' of the last requested line so the output
    // preserves the line structure the LLM expects. When the requested
    // range covers every line in the file we fall through to the `else`
    // branch, which additionally checks whether the file actually ends in
    // '\n' (rather than truncating mid-character to include it).
    let byte_end = if e_idx + 1 < total {
        ranges[e_idx + 1].0
    } else {
        let pos = ranges[e_idx].1;
        if pos < text.len() && text.as_bytes()[pos] == b'\n' {
            pos + 1
        } else {
            pos
        }
    };

    let selected = &text[byte_start..byte_end];

    // No truncate_output here: the OutputBoundedTool wrapper enforces
    // the 32 KB hard cap as the last safety net. The paged path
    // (100-line max via MAX_LINES_PER_CALL) plus that wrapper is two
    // independent bounds; either one alone would still be safe.
    Ok(selected.to_string())
}

/// Byte ranges `(start, end)` for each line in `text`, where `start` is
/// the byte offset of the first character of the line and `end` is the
/// byte offset of `'\n'` (or `text.len()` for the last line if it has no
/// trailing newline).
///
/// Matches `str::lines()` semantics: a trailing newline does **not**
/// introduce an extra empty line.
fn line_byte_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\n' {
            ranges.push((start, i));
            start = i + 1;
        }
        i += 1;
    }
    if start < text.len() {
        // Trailing content without a closing newline.
        ranges.push((start, text.len()));
    }
    ranges
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
    fn reads_utf8_markdown() {
        let (_dir, p) = with_temp_file(b"# Title\n\nhello **world**\n", "notes.md");
        let text = extract_text(&p, &ExtractOptions::default()).expect("md should extract");
        assert!(text.contains("# Title"));
        assert!(text.contains("hello **world**"));
    }

    #[test]
    fn reads_extensionless_source_file() {
        let (_dir, p) = with_temp_file(b"fn main() {}\n", "Makefile");
        let text = extract_text(&p, &ExtractOptions::default())
            .expect("extensionless text should extract");
        assert_eq!(text, "fn main() {}\n");
    }

    #[test]
    fn rejects_nul_bytes_as_binary() {
        let (_dir, p) = with_temp_file(b"PNG\x00\x01\x02binary-ish", "fake.png");
        let err =
            extract_text(&p, &ExtractOptions::default()).expect_err("NUL bytes must be rejected");
        assert!(err.contains("binary"), "got: {err}");
    }

    #[test]
    fn rejects_invalid_utf8() {
        // 0xFF is never valid UTF-8.
        let (_dir, p) = with_temp_file(&[0xFF, 0xFE, 0x00, 0x41], "utf16-le.txt");
        let err = extract_text(&p, &ExtractOptions::default())
            .expect_err("invalid UTF-8 must be rejected");
        assert!(err.contains("UTF-8"), "got: {err}");
    }

    #[test]
    fn rejects_oversized_text() {
        let big = vec![b'a'; crate::tools::output::MAX_OUTPUT_BYTES + 1];
        let (_dir, p) = with_temp_file(&big, "big.log");
        let err = extract_text(&p, &ExtractOptions::default())
            .expect_err("oversized text must be rejected");
        assert!(err.contains("too large"), "got: {err}");
    }

    // ── paging path ────────────────────────────────────────────────

    /// Build a multi-line temp file from the given lines (each line gets
    /// `'\n'` appended). Returns `(dir, path)`.
    fn with_temp_lines(lines: &[&str], name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let mut buf = Vec::new();
        for line in lines {
            buf.extend_from_slice(line.as_bytes());
            buf.push(b'\n');
        }
        with_temp_file(&buf, name)
    }

    #[test]
    fn paging_returns_only_requested_lines() {
        let (_dir, p) =
            with_temp_lines(&["line 1", "line 2", "line 3", "line 4", "line 5"], "x.txt");
        let opts = ExtractOptions {
            start_line: Some(2),
            end_line: Some(4),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("paged read should succeed");
        assert_eq!(text, "line 2\nline 3\nline 4\n");
    }

    #[test]
    fn paging_single_line() {
        let (_dir, p) = with_temp_lines(&["a", "b", "c", "d", "e"], "x.txt");
        let opts = ExtractOptions {
            start_line: Some(3),
            end_line: Some(3),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("single-line page should succeed");
        assert_eq!(text, "c\n");
    }

    #[test]
    fn paging_clamps_end_to_file_length() {
        // file has 3 lines, request end=10 → return all 3
        let (_dir, p) = with_temp_lines(&["a", "b", "c"], "x.txt");
        let opts = ExtractOptions {
            start_line: Some(1),
            end_line: Some(10),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("clamped page should succeed");
        assert_eq!(text, "a\nb\nc\n");
    }

    #[test]
    fn paging_rejects_start_exceeds_file_length() {
        let (_dir, p) = with_temp_lines(&["a", "b", "c"], "x.txt");
        let opts = ExtractOptions {
            start_line: Some(100),
            end_line: Some(105),
            ..Default::default()
        };
        let err = extract_text(&p, &opts).expect_err("oversized start_line must error");
        assert!(err.contains("exceeds file length"), "got: {err}");
        assert!(err.contains("3 lines"), "got: {err}");
    }

    #[test]
    fn paging_clamps_start_line_zero_to_one_based() {
        // start_line=0 is a 0-based off-by-one, not a real error.
        let (_dir, p) = with_temp_lines(&["a", "b", "c"], "x.txt");
        let opts = ExtractOptions {
            start_line: Some(0),
            end_line: Some(2),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("start_line=0 must be clamped");
        assert_eq!(text, "a\nb\n");
    }

    #[test]
    fn paging_clamps_range_one_over_max() {
        // "up to 100 lines" misread as 101 → clamp to the capped window.
        let lines: Vec<String> = (0..120).map(|i| format!("L{i}")).collect();
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, p) = with_temp_lines(&line_refs, "big.txt");
        let opts = ExtractOptions {
            start_line: Some(1),
            end_line: Some(crate::tools::builtin::MAX_LINES_PER_CALL + 1),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("one line over max must be clamped");
        assert_eq!(text.lines().count(), crate::tools::builtin::MAX_LINES_PER_CALL);
    }

    #[test]
    fn paging_rejects_end_lt_start() {
        let (_dir, p) = with_temp_lines(&["a", "b", "c"], "x.txt");
        let opts = ExtractOptions {
            start_line: Some(3),
            end_line: Some(1),
            ..Default::default()
        };
        let err = extract_text(&p, &opts).expect_err("end<start must error");
        assert!(err.contains("end_line"), "got: {err}");
    }

    #[test]
    fn paging_rejects_range_larger_than_max() {
        // 请求 500 行（> MAX_LINES_PER_CALL=100）必须报错
        let mut lines: Vec<String> = (0..500).map(|i| format!("L{i}")).collect();
        lines.push(String::new()); // sentinel
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, p) = with_temp_lines(&line_refs, "big.txt");
        let opts = ExtractOptions {
            start_line: Some(1),
            end_line: Some(500),
            ..Default::default()
        };
        let err = extract_text(&p, &opts).expect_err("range > MAX_LINES_PER_CALL must error");
        assert!(err.contains("Range too large"), "got: {err}");
        assert!(err.contains("100"), "got: {err}");
    }

    #[test]
    fn paging_requires_both_params() {
        let (_dir, p) = with_temp_lines(&["a", "b", "c"], "x.txt");
        // Only start_line
        let opts = ExtractOptions {
            start_line: Some(1),
            end_line: None,
            ..Default::default()
        };
        extract_text(&p, &opts).expect_err("only start_line must error");
        // Only end_line
        let opts = ExtractOptions {
            start_line: None,
            end_line: Some(2),
            ..Default::default()
        };
        extract_text(&p, &opts).expect_err("only end_line must error");
    }

    #[test]
    fn paging_handles_trailing_newline() {
        // "a\nb\n" 是 2 行（不是 3 行）— 尾随换行不引入空行
        let (_dir, p) = with_temp_file(b"a\nb\n", "trailing.txt");
        let opts = ExtractOptions {
            start_line: Some(2),
            end_line: Some(2),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("trailing-newline page should succeed");
        assert_eq!(text, "b\n");
    }

    #[test]
    fn paging_handles_no_trailing_newline() {
        // "a\nb"（无尾随换行）也是 2 行
        let (_dir, p) = with_temp_file(b"a\nb", "no-trailing.txt");
        let opts = ExtractOptions {
            start_line: Some(2),
            end_line: Some(2),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("no-trailing-newline page should succeed");
        assert_eq!(text, "b");
    }

    #[test]
    fn paging_works_on_file_larger_than_output_cap() {
        // 这是分页路径的核心价值：文件 > 32 KB 也能读，只要分页
        let big_line = "x".repeat(200); // 每行 200 字节
        let lines: Vec<String> = (0..500).map(|_| big_line.clone()).collect(); // 500 行 × 200 = 100 KB
        let line_refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let (_dir, p) = with_temp_lines(&line_refs, "huge.txt");
        let opts = ExtractOptions {
            start_line: Some(1),
            end_line: Some(3), // 只读前 3 行 = 600 字节
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("paged read of huge file should succeed");
        assert!(text.starts_with(&"x".repeat(200)));
        assert!(text.contains("\n"));
    }

    #[test]
    fn paging_utf8_lines_are_byte_safe() {
        // CJK 字符（3 字节）+ emoji（4 字节）混合
        let lines = [
            "中文第一行",
            "😀 emoji line",
            "中文第三行",
            "line four",
            "末行",
        ];
        let (_dir, p) = with_temp_lines(&lines, "cjk.txt");
        let opts = ExtractOptions {
            start_line: Some(2),
            end_line: Some(4),
            ..Default::default()
        };
        let text = extract_text(&p, &opts).expect("CJK paging should succeed");
        assert!(text.contains("😀 emoji line"));
        assert!(text.contains("中文第三行"));
        assert!(text.contains("line four"));
        // 不应包含被截断的字符（之前的 panic 触发点）
        assert!(text.is_char_boundary(text.len()));
    }
}
