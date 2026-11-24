//! The doc-service deliverable FACT stamped onto a `tool_result`'s metadata.
//!
//! ## Why the Runtime extracts this (and files it does not)
//!
//! A file deliverable is declared by the producing tool through the `produced`
//! sink, so the Runtime never inspects a tool's text ([`super::turn_deliverables`]
//! stays tool-agnostic — ADR-119). A doc-service document is different: it is
//! produced by an *external* MCP tool (`doc_write` / `doc_move`) that cannot use
//! the sink, and its fact (`doc_id`, `path`, `name`, `version`) lives only in the
//! tool's result envelope.
//!
//! That envelope rides the `tool_result` **content**, which the live MQTT path
//! truncates to five lines (ADR-035 D9.2). A pretty-printed envelope is longer
//! than five lines, so the live frontend receives unparseable JSON and could not
//! derive the document until a disk restore. The fix is the same one the file
//! facts use: ride the **metadata**, which is never truncated. This module
//! extracts the small doc fact from the full result at persist time so it can be
//! stamped onto `metadata.doc` and survive the live channel — making the live
//! banner identical to the restored one.
//!
//! ## Isolation
//!
//! Doc recognition is inherently tool-specific (only the doc service writes
//! docs), so it lives HERE — a separate adapter — and never in the tool-agnostic
//! file path. This mirrors the frontend's `deliverableDocs.ts` adapter. The
//! ADR-119 red line scans `turn_deliverables.rs` (the general file rules), not
//! this doc-specific adapter, exactly as it does not scan `deliverableDocs.ts`.

use serde_json::{json, Value};

/// Which doc-service operation a tool name denotes, regardless of server name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DocToolKind {
    Write,
    Move,
}

fn doc_tool_kind(tool_name: &str) -> Option<DocToolKind> {
    if !tool_name.starts_with("mcp_") {
        return None;
    }
    if tool_name.ends_with("__doc_write") {
        return Some(DocToolKind::Write);
    }
    if tool_name.ends_with("__doc_move") {
        return Some(DocToolKind::Move);
    }
    None
}

/// Unwrap the MCP `CallToolResult` envelope to the inner result object.
///
/// The wrapper serializes a text-only result as
/// `{"content":[{"type":"text","text":"<inner JSON>"}],...}`. Tolerant: a
/// non-JSON or malformed result yields `None` rather than panicking.
fn unwrap_mcp_result(content: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(content).ok()?;
    let obj = parsed.as_object()?;
    if let Some(blocks) = obj.get("content").and_then(|v| v.as_array()) {
        let mut texts: Vec<&str> = Vec::new();
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) == Some("text")
                && let Some(text) = block.get("text").and_then(|v| v.as_str())
            {
                texts.push(text);
            }
        }
        if texts.is_empty() {
            return None;
        }
        let inner: Value = serde_json::from_str(&texts.join("\n")).ok()?;
        return if inner.is_object() { Some(inner) } else { None };
    }
    // Already the inner result object (a wrapper may pass it through).
    Some(parsed)
}

/// Extract the doc deliverable fact from a doc-service tool result, or `None`.
///
/// Returns `{doc_id, path, name, version}` for an *applied* `doc_write` or a
/// `doc_move`; `None` for any other tool, a failed/pending write, or a malformed
/// result. The shape mirrors the frontend `DeliverableDoc` (snake_case on the
/// wire, like `metadata.produced`).
pub fn doc_fact(tool_name: &str, content: &str) -> Option<Value> {
    let kind = doc_tool_kind(tool_name)?;
    let inner = unwrap_mcp_result(content)?;
    // doc_write carries a status; only an applied write landed a document.
    // doc_move has no status field and always names the moved document.
    if kind == DocToolKind::Write && inner.get("status").and_then(|v| v.as_str()) != Some("applied")
    {
        return None;
    }
    let doc_id = inner.get("doc_id").and_then(|v| v.as_str())?;
    let path = inner.get("path").and_then(|v| v.as_str())?;
    if doc_id.is_empty() || path.is_empty() {
        return None;
    }
    let name = inner
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(path);
    let version = inner.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    Some(json!({
        "doc_id": doc_id,
        "path": path,
        "name": name,
        "version": version,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(inner: &str) -> String {
        json!({"content":[{"type":"text","text":inner}],"isError":false}).to_string()
    }

    #[test]
    fn applied_write_yields_fact() {
        let content = envelope(
            r#"{"status":"applied","doc_id":"doc-abc","path":"研发/x.md","name":"x.md","version":1}"#,
        );
        let fact = doc_fact("mcp_doc__doc_write", &content).unwrap();
        assert_eq!(fact["doc_id"], "doc-abc");
        assert_eq!(fact["path"], "研发/x.md");
        assert_eq!(fact["name"], "x.md");
        assert_eq!(fact["version"], 1);
    }

    #[test]
    fn pretty_printed_envelope_parses() {
        // The persisted envelope is pretty-printed (multi-line); parsing must
        // not depend on it being compact.
        let inner = r#"{"status":"applied","doc_id":"doc-abc","path":"a/b.md","name":"b.md","version":2}"#;
        let content = serde_json::to_string_pretty(
            &json!({"content":[{"type":"text","text":inner}],"isError":false}),
        )
        .unwrap();
        assert!(content.lines().count() > 5);
        let fact = doc_fact("mcp_doc__doc_write", &content).unwrap();
        assert_eq!(fact["doc_id"], "doc-abc");
        assert_eq!(fact["version"], 2);
    }

    #[test]
    fn pending_review_is_not_a_deliverable() {
        let content = envelope(r#"{"status":"pending_review","request_id":"r-abc"}"#);
        assert!(doc_fact("mcp_doc__doc_write", &content).is_none());
    }

    #[test]
    fn move_yields_fact_without_status() {
        let content =
            envelope(r#"{"doc_id":"doc-abc","path":"研发/final.md","name":"final.md","version":3}"#);
        let fact = doc_fact("mcp_doc__doc_move", &content).unwrap();
        assert_eq!(fact["path"], "研发/final.md");
        assert_eq!(fact["version"], 3);
    }

    #[test]
    fn custom_server_name_recognized() {
        let content = envelope(
            r#"{"status":"applied","doc_id":"doc-x","path":"p.md","name":"p.md","version":1}"#,
        );
        assert!(doc_fact("mcp_mydocs__doc_write", &content).is_some());
    }

    #[test]
    fn unrelated_tool_yields_none() {
        let content = envelope(
            r#"{"status":"applied","doc_id":"doc-x","path":"p.md","name":"p.md","version":1}"#,
        );
        assert!(doc_fact("mcp_doc__doc_read", &content).is_none());
        assert!(doc_fact("file_write", &content).is_none());
        assert!(doc_fact("shell", &content).is_none());
    }

    #[test]
    fn malformed_or_truncated_content_yields_none() {
        assert!(doc_fact("mcp_doc__doc_write", "not json").is_none());
        // The live 5-line truncation of a pretty envelope: unparseable.
        let inner = r#"{"status":"applied","doc_id":"doc-x","path":"p.md","name":"p.md","version":1}"#;
        let pretty = serde_json::to_string_pretty(
            &json!({"content":[{"type":"text","text":inner}],"isError":false}),
        )
        .unwrap();
        let truncated = pretty.lines().take(5).collect::<Vec<_>>().join("\n") + "\n...(truncated)";
        assert!(doc_fact("mcp_doc__doc_write", &truncated).is_none());
    }

    #[test]
    fn missing_id_or_path_yields_none() {
        let no_id = envelope(r#"{"status":"applied","path":"p.md","version":1}"#);
        assert!(doc_fact("mcp_doc__doc_write", &no_id).is_none());
        let no_path = envelope(r#"{"status":"applied","doc_id":"doc-x","version":1}"#);
        assert!(doc_fact("mcp_doc__doc_write", &no_path).is_none());
    }

    #[test]
    fn name_defaults_to_path_and_version_to_zero() {
        let content = envelope(r#"{"status":"applied","doc_id":"doc-x","path":"a/b.md"}"#);
        let fact = doc_fact("mcp_doc__doc_write", &content).unwrap();
        assert_eq!(fact["name"], "a/b.md");
        assert_eq!(fact["version"], 0);
    }
}