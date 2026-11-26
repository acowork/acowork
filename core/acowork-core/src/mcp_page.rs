//! Pagination policy shared by the PM and Doc MCP surfaces.
//!
//! ## Why this exists
//!
//! An MCP tool result lands **in the caller's context window**, not in a
//! browser tab. A list that answers "everything matching" is therefore not
//! generous, it is destructive: one `pm_list_tasks(limit=50)` used to return
//! each task's 20000-character `description` plus its 20000-character
//! submission result, and a single call compacted the agent's context away.
//!
//! So every MCP list tool follows one policy, enforced here rather than
//! re-typed per tool:
//!
//! - a page is at most [`MAX_PAGE`] rows;
//! - the envelope always says `total` / `offset` / `limit` / `returned` /
//!   `has_more`, so a caller can page instead of guessing;
//! - a capped page says so out loud (`truncated` + `notice`), because a silent
//!   truncation is read by an LLM as "that is all of them".
//!
//! Rows themselves are summaries. Full records (task description, submission
//! result, document content) belong to the single-object read tools, which
//! page over text instead ([`text_chunk`]).

use serde_json::{json, Value};

/// Hard ceiling on rows per page, for every MCP list tool, with no override.
///
/// Not a knob: a list tool whose caller can ask for 500 rows is a list tool
/// that will eventually be asked for 500 rows. Callers may still ask for up
/// to this; the *default* (when they omit `limit`) is the cheaper
/// [`DEFAULT_PAGE`].
pub const MAX_PAGE: usize = 20;

/// Rows returned when the caller omits `limit`.
///
/// Cheaper than [`MAX_PAGE`] so a lazy one-line `pm_list_tasks()` does not
/// spend the whole window on a page it may never scroll through; the LLM pages
/// forward (or asks for more, up to [`MAX_PAGE`]) when it actually needs it.
pub const DEFAULT_PAGE: usize = 10;

/// Maximum characters of document / submission text returned in one read.
///
/// Sized so a single read chunk is comfortably cheaper than a context window;
/// longer text is read with `offset`, which the response reports.
pub const MAX_TEXT_CHUNK: usize = 20_000;

/// Effective page size: the caller's request clamped into `1..=MAX_PAGE`
/// (an omitted request gets [`DEFAULT_PAGE`]).
pub fn page_limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

/// Effective text window: the caller's request clamped into
/// `1..=MAX_TEXT_CHUNK` (an omitted request gets a full window).
///
/// The sibling of [`page_limit`] for the single-document read tools; both
/// defaults live here so no handler restates `MAX_TEXT_CHUNK` as a
/// `#[serde(default = ...)]` constant of its own.
pub fn text_limit(requested: Option<usize>) -> usize {
    requested.unwrap_or(MAX_TEXT_CHUNK).clamp(1, MAX_TEXT_CHUNK)
}

/// Stamp the uniform page envelope onto an already-built response object and
/// store `items` under `key`.
///
/// `total` is `None` when the source cannot count its matches (ranked search);
/// the page is then assumed to be truncated whenever it came back full, which
/// errs toward "ask again with offset" instead of "you have seen everything".
pub fn paginate(
    mut base: Value,
    key: &str,
    items: Vec<Value>,
    total: Option<usize>,
    offset: usize,
    limit: usize,
) -> Value {
    let returned = items.len();
    let has_more = match total {
        Some(total) => offset + returned < total,
        None => returned == limit,
    };
    base["offset"] = json!(offset);
    base["limit"] = json!(limit);
    base["returned"] = json!(returned);
    base["has_more"] = json!(has_more);
    base["total"] = json!(total);
    base[key] = Value::Array(items);
    if has_more {
        base["truncated"] = json!(true);
        base["notice"] = json!(format!(
            "Truncated: this page shows {returned} row(s); more remain. Call again with offset={} for the next page (up to {MAX_PAGE} rows each), or narrow the query.",
            offset + returned
        ));
    }
    base
}

/// A character window over `content`, for the single-document read tools.
///
/// Character offsets, not bytes: a byte slice of UTF-8 text panics on a
/// boundary, and half a Chinese character is not a useful answer anyway.
///
/// Returns `(chunk, total_chars, has_more)`.
pub fn text_chunk(content: &str, offset: usize, max_chars: usize) -> (&str, usize, bool) {
    let total = content.chars().count();
    let start = offset.min(total);
    let take = max_chars.max(1);
    let start_byte = content
        .char_indices()
        .nth(start)
        .map(|(i, _)| i)
        .unwrap_or(content.len());
    let end_byte = content[start_byte..]
        .char_indices()
        .nth(take)
        .map(|(i, _)| start_byte + i)
        .unwrap_or(content.len());
    (&content[start_byte..end_byte], total, end_byte < content.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_capped_and_never_zero() {
        assert_eq!(page_limit(None), DEFAULT_PAGE);
        assert_eq!(page_limit(Some(1000)), MAX_PAGE);
        assert_eq!(page_limit(Some(25)), MAX_PAGE);
        assert_eq!(page_limit(Some(1)), 1);
        assert_eq!(page_limit(Some(0)), 1);
    }

    #[test]
    fn text_window_is_capped_and_never_zero() {
        assert_eq!(text_limit(None), MAX_TEXT_CHUNK);
        assert_eq!(text_limit(Some(usize::MAX)), MAX_TEXT_CHUNK);
        assert_eq!(text_limit(Some(0)), 1);
    }

    #[test]
    fn a_full_page_announces_the_truncation() {
        let items: Vec<Value> = (0..MAX_PAGE).map(|i| json!(i)).collect();
        let v = paginate(json!({ "path": "" }), "items", items, Some(57), 0, MAX_PAGE);
        assert_eq!(v["has_more"], true);
        assert_eq!(v["truncated"], true);
        assert_eq!(v["total"], 57);
        assert!(
            v["notice"].as_str().unwrap().contains("offset=20"),
            "the notice must tell the caller where to resume: {v}"
        );
        // The tool's own fields survive the envelope.
        assert_eq!(v["path"], "");
        assert_eq!(v["items"].as_array().unwrap().len(), MAX_PAGE);
    }

    #[test]
    fn the_last_page_is_not_marked_truncated() {
        let v = paginate(json!({}), "items", vec![json!(0)], Some(21), 20, 1);
        assert_eq!(v["has_more"], false);
        assert!(v.get("truncated").is_none());
        assert!(v.get("notice").is_none());
    }

    #[test]
    fn text_chunk_respects_character_boundaries() {
        // 4 Chinese characters = 12 bytes; a byte-offset slice at 2 would panic.
        let content = "文档内容ABC";
        let (chunk, total, has_more) = text_chunk(content, 2, 2);
        assert_eq!(chunk, "内容");
        assert_eq!(total, 7);
        assert!(has_more);
        let (tail, _, has_more) = text_chunk(content, 6, 100);
        assert_eq!(tail, "C");
        assert!(!has_more);
        // Past the end is an empty tail, not a panic.
        assert_eq!(text_chunk(content, 99, 10).0, "");
    }
}
