//! Codebase tool — LSP-powered code intelligence for Agent Runtime.
//!
//! Connects to the LSP Relay's `/api/codebase/rpc` JSON-RPC endpoint to
//! perform code intelligence operations: go-to-definition, find references,
//! hover information, workspace symbol search, and diagnostics.
//!
//! ## Architecture
//!
//! ```text
//! Agent Runtime (codebase tool)
//!     │
//!     │ POST /api/codebase/rpc  (JSON-RPC over HTTP)
//!     ▼
//! acowork-lsp-relay
//!     │
//!     │ stdin/stdout (LSP protocol)
//!     ▼
//! Language Server (rust-analyzer, pyright, etc.)
//! ```
//!
//! The tool is conditionally registered: only when the Gateway reports
//! a running LSP Relay via `AgentHelloConfig.lsp_relay_endpoint`.
//!
//! ## Output contract
//!
//! LSP responses are **not** bounded by the language server — a single
//! `references` query on `Display::fmt` / `Iterator::next` can return
//! thousands of `Location` objects, and `symbol` on a fuzzy query like
//! `Mod` matches every symbol in the workspace. Without truncation a
//! single tool call can produce several megabytes of JSON, which is
//! exactly the failure mode that triggered the 128 KB → 32 KB cap on
//! the global tool output budget.
//!
//! Three-layer defence, mirroring the [shell head+tail] pattern:
//! 1. **Per-action array cap** — `references` / `symbol` arrays are
//!    sliced to [`output::MAX_RESULT_COUNT`] (1000) entries before
//!    serialisation; a `[... N more results omitted]` marker tells the
//!    LLM the truncation happened and how to narrow the query.
//! 2. **`definition` single-match** — a definition query that resolves
//!    to multiple `Location`s keeps only the first (most servers rank
//!    the primary definition first); a marker tells the LLM there were
//!    more.
//! 3. **Global 32 KB hard cap** — every result, regardless of action,
//!    is funnelled through [`output::truncate_output`] as the last
//!    safety net. The marker carries an actionable re-query hint.
//!
//! [shell head+tail]: crate::tools::output::truncate_head_tail_output

use acowork_core::timeout_config::constants;
use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::tools::output;

/// Timeout for individual LSP requests via the relay.
const REQUEST_TIMEOUT: std::time::Duration = constants::LSP_REQUEST;

/// The codebase tool — proxies LSP requests to the LSP Relay.
pub struct CodebaseTool {
    /// LSP Relay HTTP endpoint (e.g. "http://127.0.0.1:19878").
    relay_endpoint: String,
    /// HTTP client with timeout.
    client: reqwest::Client,
}

impl CodebaseTool {
    /// Create a new codebase tool connected to the given LSP Relay endpoint.
    pub fn new(relay_endpoint: String) -> Self {
        Self {
            relay_endpoint,
            client: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("Failed to build codebase HTTP client"),
        }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "codebase".to_string(),
            description: "Query the codebase using LSP (Language Server Protocol). \
                Supports: definition (go to definition), references (find all references), \
                hover (type info and documentation), symbol (workspace-wide symbol search), \
                diagnostic (get diagnostics for a file). \
                Requires a language server to be installed for the target language. \
                OUTPUT CONTRACT: results over {max} entries are truncated with a marker; \
                the entire response is hard-capped at 32 KB — narrow your query if truncated."
                .replace("{max}", &output::MAX_RESULT_COUNT.to_string()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["definition", "references", "hover", "symbol", "diagnostic"],
                        "description": "The code intelligence action to perform"
                    },
                    "language": {
                        "type": "string",
                        "description": "Language id (e.g. 'rust', 'python', 'typescript')"
                    },
                    "file": {
                        "type": "string",
                        "description": "Relative file path within the workspace (e.g. 'core/acowork-runtime/src/main.rs')"
                    },
                    "line": {
                        "type": "integer",
                        "description": "1-based line number (required for definition, references, hover)"
                    },
                    "character": {
                        "type": "integer",
                        "description": "1-based character/column number (required for definition, references, hover)"
                    },
                    "query": {
                        "type": "string",
                        "description": "Search query for workspace/symbol (required for symbol action)"
                    }
                },
                "required": ["action", "language"]
            }),
        }
    }
}

#[async_trait]
impl Tool for CodebaseTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let action = params["action"].as_str().unwrap_or("");
        let language = params["language"].as_str().unwrap_or("");
        let file = params["file"].as_str().unwrap_or("");
        let line = params["line"].as_u64();
        let character = params["character"].as_u64();
        let query = params["query"].as_str().unwrap_or("");

        if language.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Missing 'language' parameter".to_string()),
                token_usage: None,
            });
        }

        // Resolve workspace root from work_dir (the agent's working directory).
        let workspace_root = work_dir.unwrap_or(".");

        // Build the LSP request based on the action.
        let (method, lsp_params) = match action {
            "definition" => {
                if file.is_empty() || line.is_none() || character.is_none() {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "Action 'definition' requires 'file', 'line', and 'character'"
                                .to_string(),
                        ),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/definition",
                    serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line.unwrap() - 1, "character": character.unwrap() - 1 }
                    }),
                )
            }
            "references" => {
                if file.is_empty() || line.is_none() || character.is_none() {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "Action 'references' requires 'file', 'line', and 'character'"
                                .to_string(),
                        ),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/references",
                    serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line.unwrap() - 1, "character": character.unwrap() - 1 },
                        "context": { "includeDeclaration": true }
                    }),
                )
            }
            "hover" => {
                if file.is_empty() || line.is_none() || character.is_none() {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(
                            "Action 'hover' requires 'file', 'line', and 'character'".to_string(),
                        ),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/hover",
                    serde_json::json!({
                        "textDocument": { "uri": uri },
                        "position": { "line": line.unwrap() - 1, "character": character.unwrap() - 1 }
                    }),
                )
            }
            "symbol" => {
                if query.is_empty() {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some("Action 'symbol' requires 'query'".to_string()),
                        token_usage: None,
                    });
                }
                ("workspace/symbol", serde_json::json!({ "query": query }))
            }
            "diagnostic" => {
                if file.is_empty() {
                    return Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some("Action 'diagnostic' requires 'file'".to_string()),
                        token_usage: None,
                    });
                }
                let uri = build_file_uri(workspace_root, file);
                (
                    "textDocument/diagnostic",
                    serde_json::json!({
                        "textDocument": { "uri": uri }
                    }),
                )
            }
            _ => {
                return Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Unknown action '{}'. Supported: definition, references, hover, symbol, diagnostic",
                        action
                    )),
                    token_usage: None,
                });
            }
        };

        // Call the LSP Relay.
        let url = format!("{}/api/codebase/rpc", self.relay_endpoint);
        let request_body = serde_json::json!({
            "language": language,
            "workspace_root": workspace_root,
            "method": method,
            "params": lsp_params,
            "expect_response": true,
        });

        match self.client.post(&url).json(&request_body).send().await {
            Ok(resp) => {
                let body: Value = resp.json().await.unwrap_or_default();
                let success = body["success"].as_bool().unwrap_or(false);
                if success {
                    let result = body.get("result").cloned().unwrap_or(Value::Null);
                    // Two-layer output protection (see module docs):
                    //   1. action-specific array/scalar shaping — gives the
                    //      LLM a structured `__truncated__: N omitted` marker
                    //      it can act on (narrow query, different file, etc.)
                    //   2. global 32 KB hard cap is the OutputBoundedTool
                    //      wrapper's job — applied AFTER this method returns,
                    //      so we don't have to remember it per tool.
                    let shaped = shape_lsp_result(action, result);
                    let content = serde_json::to_string_pretty(&shaped).unwrap_or_default();
                    Ok(ToolResult {
                        content_parts: None,
                        ok: true,
                        content,
                        error: None,
                        token_usage: None,
                    })
                } else {
                    let error_msg = body["error"]
                        .as_str()
                        .unwrap_or("Unknown LSP error")
                        .to_string();
                    Ok(ToolResult {
                        content_parts: None,
                        ok: false,
                        content: String::new(),
                        error: Some(error_msg),
                        token_usage: None,
                    })
                }
            }
            Err(e) => Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!("Failed to reach LSP Relay: {e}")),
                token_usage: None,
            }),
        }
    }
}

/// Shape the raw LSP `result` JSON before serialisation.
///
/// Each action has different worst-case output shape:
/// - `references` → `Location[]` (can be thousands; cap to MAX_RESULT_COUNT)
/// - `symbol`     → `SymbolInformation[]` / `WorkspaceSymbol[]` (cap similarly)
/// - `definition` → `Location | Location[]` (keep only the first Location;
///   multi-result definition is rare and the primary result is always first
///   in LSP spec, so taking [0] is the principled choice)
/// - `hover`      → `Hover` (small; pass through)
/// - `diagnostic` → `Diagnostic[]` (cap similarly — a freshly-built project
///   can produce thousands of warnings)
fn shape_lsp_result(action: &str, result: Value) -> Value {
    let cap = output::MAX_RESULT_COUNT;
    match action {
        "references" | "symbol" | "diagnostic" => cap_array(result, cap),
        "definition" => keep_first_location(result),
        _ => result,
    }
}

/// If `value` is a JSON array longer than `cap`, slice it to `cap` and
/// attach a `__truncated__` annotation the caller can surface as a marker.
///
/// Pure function — no I/O, no logging. The marker is attached as a
/// sibling field rather than a wrapper object so `to_string_pretty`
/// produces output the LLM can still parse naturally.
fn cap_array(value: Value, cap: usize) -> Value {
    let Some(arr) = value.as_array() else {
        return value;
    };
    let total = arr.len();
    if total <= cap {
        return value;
    }
    let kept: Vec<Value> = arr.iter().take(cap).cloned().collect();
    let omitted = total - cap;
    serde_json::json!({
        "__truncated__": true,
        "__total__": total,
        "__kept__": cap,
        "__omitted__": omitted,
        "items": kept,
    })
}

/// Reduce a `Location | Location[]` to a single `Location`. If the input
/// is already a single object (the common case), it's returned as-is.
/// If it's an array, the first entry is kept and a sibling marker is
/// attached so the LLM knows more results existed.
fn keep_first_location(value: Value) -> Value {
    match value {
        Value::Array(arr) => {
            if arr.is_empty() {
                return Value::Null;
            }
            let total = arr.len();
            let first = arr.into_iter().next().unwrap_or(Value::Null);
            if total == 1 {
                first
            } else {
                serde_json::json!({
                    "__truncated__": true,
                    "__total__": total,
                    "__omitted__": total - 1,
                    "location": first,
                })
            }
        }
        other => other,
    }
}

/// Build a `file://` URI from a workspace root and relative file path.
fn build_file_uri(workspace_root: &str, file: &str) -> String {
    let root = workspace_root.trim_end_matches('/');
    let file = file.trim_start_matches('/');
    format!("file://{}/{}", root, file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_file_uri() {
        let uri = build_file_uri("/home/user/project", "src/main.rs");
        assert_eq!(uri, "file:///home/user/project/src/main.rs");
    }

    #[test]
    fn test_build_file_uri_trailing_slash() {
        let uri = build_file_uri("/home/user/project/", "/src/main.rs");
        assert_eq!(uri, "file:///home/user/project/src/main.rs");
    }

    #[test]
    fn test_spec_value() {
        let spec = CodebaseTool::spec_value();
        assert_eq!(spec.name, "codebase");
        assert!(spec.description.contains("LSP"));
    }

    // ── cap_array unit tests ───────────────────────────────────────────
    //
    // cap_array is the workhorse for `references` / `symbol` /
    // `diagnostic` — three LSP actions that can return thousands of
    // entries. Every test below pins down a behavioural contract; if
    // the function ever drifts, one of these catches it before the
    // LLM sees a multi-megabyte tool result.

    /// Build an array of N `{uri, line}` location-like objects — a
    /// realistic shape for `references` results.
    fn make_locations(n: usize) -> Value {
        let items: Vec<Value> = (0..n)
            .map(|i| {
                serde_json::json!({
                    "uri": format!("file:///repo/src/file_{i}.rs"),
                    "range": { "start": { "line": i, "character": 0 } }
                })
            })
            .collect();
        Value::Array(items)
    }

    #[test]
    fn cap_array_empty_array_returns_empty() {
        // Edge case: zero results — must stay empty (no marker, since
        // nothing was truncated).
        let v = cap_array(Value::Array(vec![]), 1000);
        assert_eq!(v, Value::Array(vec![]));
    }

    #[test]
    fn cap_array_below_cap_returns_unchanged() {
        // 5 items with cap=10 — all 5 must come back verbatim with no
        // truncation marker (LLM should not see "truncated" if nothing
        // was dropped).
        let input = make_locations(5);
        let out = cap_array(input.clone(), 10);
        assert_eq!(out, input);
    }

    #[test]
    fn cap_array_at_cap_returns_unchanged() {
        // Boundary: len == cap. Strict `<=` means we DON'T add a
        // marker. (Off-by-one error here would flip the behaviour.)
        let input = make_locations(1000);
        let out = cap_array(input.clone(), 1000);
        assert_eq!(out, input);
    }

    #[test]
    fn cap_array_one_over_cap_truncates_with_marker() {
        // Boundary inverse: len == cap + 1 → truncate to cap items,
        // omitted = 1, marker present.
        let input = make_locations(1001);
        let out = cap_array(input, 1000);
        let obj = out.as_object().expect("truncated output must be object");
        assert_eq!(obj["__truncated__"], Value::Bool(true));
        assert_eq!(obj["__total__"], Value::Number(1001u64.into()));
        assert_eq!(obj["__kept__"], Value::Number(1000u64.into()));
        assert_eq!(obj["__omitted__"], Value::Number(1u64.into()));
        let items = obj["items"].as_array().expect("items array");
        assert_eq!(items.len(), 1000);
        // First and last kept items must match the input shape.
        assert!(items[0]["uri"].as_str().unwrap().contains("file_0.rs"));
        assert!(items[999]["uri"].as_str().unwrap().contains("file_999.rs"));
    }

    #[test]
    fn cap_array_well_over_cap_truncates_with_correct_omitted_count() {
        // Realistic case: 1500 references, cap=1000 → 500 omitted.
        let input = make_locations(1500);
        let out = cap_array(input, 1000);
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__total__"], Value::Number(1500u64.into()));
        assert_eq!(obj["__kept__"], Value::Number(1000u64.into()));
        assert_eq!(obj["__omitted__"], Value::Number(500u64.into()));
        assert_eq!(obj["items"].as_array().unwrap().len(), 1000);
    }

    #[test]
    fn cap_array_preserves_full_first_cap_items_in_order() {
        // Ordering contract: a partial result is only useful if the LLM
        // can trust that items[0..cap] are the same as the LSP server's
        // first cap items. We verify by tagging each item with its
        // index and checking the kept slice matches.
        let input = make_locations(1500);
        let out = cap_array(input, 1000);
        let items = out["items"].as_array().unwrap();
        for (i, item) in items.iter().enumerate().take(1000) {
            let expected_uri = format!("file:///repo/src/file_{i}.rs");
            assert_eq!(
                item["uri"],
                Value::String(expected_uri),
                "item {i} order drifted"
            );
        }
    }

    #[test]
    fn cap_array_non_array_object_passes_through() {
        // cap_array only mutates arrays. If the LSP server returned an
        // unexpected object (protocol bug, version mismatch), pass it
        // through untouched — better than crashing the tool call.
        let input = serde_json::json!({ "unexpected": "shape", "count": 42 });
        let out = cap_array(input.clone(), 1000);
        assert_eq!(out, input);
    }

    #[test]
    fn cap_array_non_array_null_passes_through() {
        // LSP sometimes returns null for "no result" — pass through.
        let out = cap_array(Value::Null, 1000);
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn cap_array_non_array_string_passes_through() {
        // Defensive: malformed LSP response that returns a string where
        // an array is expected. Pass through rather than crash.
        let out = cap_array(Value::String("oops".into()), 1000);
        assert_eq!(out, Value::String("oops".into()));
    }

    #[test]
    fn cap_array_cap_zero_makes_everything_omitted() {
        // Degenerate cap=0: every item is over the cap. Result is the
        // marker object with empty items and omitted = total. Pins
        // down the saturating behaviour so a future refactor can't
        // accidentally treat cap=0 as "no cap".
        let input = make_locations(3);
        let out = cap_array(input, 0);
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__truncated__"], Value::Bool(true));
        assert_eq!(obj["__total__"], Value::Number(3u64.into()));
        assert_eq!(obj["__kept__"], Value::Number(0u64.into()));
        assert_eq!(obj["__omitted__"], Value::Number(3u64.into()));
        assert_eq!(obj["items"].as_array().unwrap().len(), 0);
    }

    // ── keep_first_location unit tests ────────────────────────────────

    #[test]
    fn keep_first_location_single_object_passes_through() {
        // The common case for definition: LSP returns a single
        // Location object. No mutation, no marker (nothing dropped).
        let loc = serde_json::json!({
            "uri": "file:///repo/src/lib.rs",
            "range": { "start": { "line": 42, "character": 5 } }
        });
        let out = keep_first_location(loc.clone());
        assert_eq!(out, loc);
    }

    #[test]
    fn keep_first_location_array_of_one_unwraps_silently() {
        // LSP sometimes wraps a single Location in an array of length
        // 1. Unwrap to the bare Location — no marker, since total==1
        // means nothing was actually dropped.
        let loc = serde_json::json!({
            "uri": "file:///repo/src/lib.rs",
            "range": { "start": { "line": 42, "character": 5 } }
        });
        let input = Value::Array(vec![loc.clone()]);
        let out = keep_first_location(input);
        assert_eq!(out, loc);
    }

    #[test]
    fn keep_first_location_array_of_many_takes_first_with_marker() {
        // Multi-result definition: keep only [0], add marker so the
        // LLM knows there were alternatives.
        let a = serde_json::json!({"uri": "file:///a.rs", "range": {}});
        let b = serde_json::json!({"uri": "file:///b.rs", "range": {}});
        let c = serde_json::json!({"uri": "file:///c.rs", "range": {}});
        let out = keep_first_location(Value::Array(vec![a.clone(), b, c]));
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__truncated__"], Value::Bool(true));
        assert_eq!(obj["__total__"], Value::Number(3u64.into()));
        assert_eq!(obj["__omitted__"], Value::Number(2u64.into()));
        assert_eq!(obj["location"], a);
    }

    #[test]
    fn keep_first_location_array_of_two_takes_first_with_marker() {
        // Boundary: exactly 2 — must add marker (omitted=1). If we
        // ever change the threshold to "drop marker when omitted<=N"
        // this test catches the unintended behaviour.
        let a = serde_json::json!({"uri": "file:///a.rs", "range": {}});
        let b = serde_json::json!({"uri": "file:///b.rs", "range": {}});
        let out = keep_first_location(Value::Array(vec![a.clone(), b]));
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__omitted__"], Value::Number(1u64.into()));
        assert_eq!(obj["location"], a);
    }

    #[test]
    fn keep_first_location_empty_array_returns_null() {
        // LSP returned [] for definition. shape_lsp_result treats this
        // as "no definition found" — caller converts to Null. We test
        // the helper directly: empty array → Null.
        let out = keep_first_location(Value::Array(vec![]));
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn keep_first_location_non_location_value_passes_through() {
        // Defensive: if LSP returns a string (shouldn't happen but
        // versions vary), pass through. Better than crashing the tool.
        let cases = vec![
            Value::Null,
            Value::String("str".into()),
            Value::Number(42.into()),
            Value::Bool(true),
        ];
        for v in cases {
            assert_eq!(keep_first_location(v.clone()), v, "must passthrough {v:?}");
        }
    }

    // ── shape_lsp_result dispatch tests ───────────────────────────────

    #[test]
    fn shape_references_below_cap_passthrough() {
        let input = make_locations(5);
        let out = shape_lsp_result("references", input.clone());
        assert_eq!(out, input);
    }

    #[test]
    fn shape_references_over_cap_truncates() {
        let input = make_locations(1500);
        let out = shape_lsp_result("references", input);
        let obj = out.as_object().expect("truncated result must be object");
        assert_eq!(obj["__omitted__"], Value::Number(500u64.into()));
    }

    #[test]
    fn shape_symbol_over_cap_truncates() {
        // Workspace symbol queries are the worst offender — a fuzzy
        // query like "Mod" can match every module-level symbol.
        let input = make_locations(2000);
        let out = shape_lsp_result("symbol", input);
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__total__"], Value::Number(2000u64.into()));
        assert_eq!(obj["__kept__"], Value::Number(1000u64.into()));
        assert_eq!(obj["__omitted__"], Value::Number(1000u64.into()));
    }

    #[test]
    fn shape_diagnostic_over_cap_truncates() {
        // A freshly-built Rust project can produce thousands of
        // warnings — clippy alone routinely hits 500+ items. Cap it.
        let input = make_locations(1200);
        let out = shape_lsp_result("diagnostic", input);
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__omitted__"], Value::Number(200u64.into()));
    }

    #[test]
    fn shape_definition_single_object_passes_through() {
        let loc = serde_json::json!({"uri": "file:///x.rs", "range": {}});
        let out = shape_lsp_result("definition", loc.clone());
        assert_eq!(out, loc);
    }

    #[test]
    fn shape_definition_array_unwraps_to_first_with_marker() {
        let a = serde_json::json!({"uri": "file:///a.rs", "range": {}});
        let b = serde_json::json!({"uri": "file:///b.rs", "range": {}});
        let out = shape_lsp_result("definition", Value::Array(vec![a.clone(), b]));
        let obj = out.as_object().unwrap();
        assert_eq!(obj["__truncated__"], Value::Bool(true));
        assert_eq!(obj["location"], a);
    }

    #[test]
    fn shape_definition_empty_array_yields_null() {
        let out = shape_lsp_result("definition", Value::Array(vec![]));
        assert_eq!(out, Value::Null);
    }

    #[test]
    fn shape_hover_passes_through_any_value() {
        // Hover returns small markdown — no truncation, pass through
        // whatever shape LSP gives us.
        let hover = serde_json::json!({
            "contents": { "kind": "markdown", "value": "fn main() {}" },
            "range": { "start": { "line": 1, "character": 0 } }
        });
        let out = shape_lsp_result("hover", hover.clone());
        assert_eq!(out, hover);
    }

    #[test]
    fn shape_unknown_action_passes_through() {
        // Forward compatibility: a future action we don't know about
        // must pass through, not crash. (Tests _ => result branch.)
        let v = serde_json::json!({ "future": "action result" });
        let out = shape_lsp_result("renameSymbol", v.clone());
        assert_eq!(out, v);
    }

    #[test]
    fn shape_dispatch_is_action_based_not_value_based() {
        // Regression guard: a non-array value passed to a "cap_array"
        // action must NOT be wrapped in a marker object — it must
        // pass through. (cap_array's own non-array branch handles this,
        // but shape_lsp_result must not add wrapper behaviour of its
        // own.)
        let v = serde_json::json!({"not": "an array"});
        let out = shape_lsp_result("references", v.clone());
        assert_eq!(out, v, "non-array must passthrough references action");
    }

    // ── shape_lsp_result cross-action contract ────────────────────────

    #[test]
    fn shape_cap_is_consistent_across_all_array_actions() {
        // All three array actions must use the same cap
        // (MAX_RESULT_COUNT). If we ever add a per-action cap, this
        // catches the drift.
        let big = make_locations(output::MAX_RESULT_COUNT + 50);
        for action in ["references", "symbol", "diagnostic"] {
            let out = shape_lsp_result(action, big.clone());
            let obj = out.as_object().unwrap();
            assert_eq!(
                obj["__kept__"],
                Value::Number(output::MAX_RESULT_COUNT.into()),
                "action {action} must use MAX_RESULT_COUNT cap"
            );
            assert_eq!(
                obj["__omitted__"],
                Value::Number(50u64.into()),
                "action {action} must compute omitted correctly"
            );
        }
    }
}
