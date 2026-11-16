//! Tool compression (ADR-052) was retired. The `context_abandon` tool is
//! no longer registered with the LLM; its source file survives in this
//! module as dead code for future reference.
//!
//! `context_abandon` (originally an LLM-initiated "compress tool
//! result" tool) is kept here so the design history is preserved and
//! the in-place replacement mechanics can be revisited if a future
//! ADR re-introduces automated tool-result compression. Its sibling
//! `context_retrieve` — and the `context_compression` module that bound
//! the two sides — have been deleted (ADR-061 §10/§12).
#![allow(dead_code)]

//! ADR-052 context_abandon tool - replace a tool result with a placeholder.
//!
//! **DEPRECATED (ADR-061 §10.2)**: this tool is no longer registered.
//! LLM-autonomous tool compression is closed — the 8-level plan schedules
//! tool retention at runtime, and autonomous in-place replacement would
//! invalidate the ADR-060 Block B prompt cache. The implementation is kept
//! for backward compatibility and potential future reuse; nothing drains
//! its queue, so calls become a no-op that returns a deprecation notice.
//!
//! ## Historical execution model
//!
//! This tool did **not** directly modify `HistoryManager` (tools have no
//! access to it). Instead, it pushed the `tool_call_id` into an
//! `abandon_queue`; the agent loop drained the queue on the next iteration
//! and called `HistoryManager::abandon_tool_result()` to perform the
//! in-place replacement. (The symmetric `retrieve_queue` belonged to
//! `context_retrieve`, deleted along with it — see ADR-061 §12.)
//!
//! ## Parameters
//!
//! - `tool_call_id` (string, required) - the `tool_call_id` of the tool
//!   result to abandon. This is the same id that appears in tool results
//!   and in compressed placeholders.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tracing;

/// ADR-052 context_abandon tool for replacing tool results with placeholders.
///
/// DEPRECATED (ADR-061 §10.2): no longer registered with the LLM. The
/// queue is internal-only so the type still compiles and behaves
/// harmlessly if instantiated by legacy code paths.
pub struct ContextAbandonTool {
    /// Internal queue for abandon requests. Nothing drains it since
    /// ADR-061 closed LLM-autonomous tool compression.
    abandon_queue: Arc<Mutex<VecDeque<String>>>,
}

impl ContextAbandonTool {
    pub fn new() -> Self {
        Self {
            abandon_queue: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

impl Default for ContextAbandonTool {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextAbandonTool {
    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "context_abandon".to_string(),
            description:
                "Replace a tool result with a compact placeholder to free up context window space. \
                 The placeholder is NOT recallable: the original content is no longer part of the \
                 conversation history, so re-run the tool if you need the result again. Call this \
                 when a tool result is no longer needed for your current reasoning - e.g., after \
                 you've extracted the relevant information from a large file_read or \
                 content_search output."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool_call_id": {
                        "type": "string",
                        "description": "The tool_call_id of the tool result to abandon. \
                                        This is the same id that appears in tool results and \
                                        in compressed placeholders."
                    }
                },
                "required": ["tool_call_id"]
            }),
        }
    }
}

#[async_trait]
impl Tool for ContextAbandonTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        let tool_call_id = match params.get("tool_call_id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => id.to_string(),
            Some(_) => {
                tracing::warn!("context_abandon called with empty tool_call_id");
                return Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some("tool_call_id must be a non-empty string".to_string()),
                    token_usage: None,
                });
            }
            None => {
                tracing::warn!("context_abandon called without tool_call_id parameter");
                return Ok(ToolResult {
                    content_parts: None,
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'tool_call_id'".to_string()),
                    token_usage: None,
                });
            }
        };

        // Push the tool_call_id into the internal queue. Nothing drains it
        // since ADR-061 closed LLM-autonomous tool compression (§10.2) —
        // the response below is the deprecation surface.
        self.abandon_queue
            .lock()
            .unwrap()
            .push_back(tool_call_id.clone());

        tracing::warn!(
            tool_call_id = %tool_call_id,
            "context_abandon: deprecated tool invoked (ADR-061 §10.2) — queued but never drained"
        );

        Ok(ToolResult {
            content_parts: None,
            ok: true,
            content: format!(
                "Tool result '{}' will be replaced with a placeholder.",
                tool_call_id
            ),
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_missing_param() {
        let tool = ContextAbandonTool::new();
        let params = serde_json::json!({});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("Missing required parameter"));
    }

    #[test]
    fn test_tool_empty_param() {
        let tool = ContextAbandonTool::new();
        let params = serde_json::json!({"tool_call_id": ""});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("non-empty"));
    }

    #[test]
    fn test_abandon_queue_pushed() {
        let tool = ContextAbandonTool::new();
        let params = serde_json::json!({"tool_call_id": "toolu_abc"});
        let result = futures::executor::block_on(tool.execute(params, None)).unwrap();

        assert!(result.ok);
        assert!(result.content.contains("toolu_abc"));
        assert!(result.content.contains("placeholder"));

        // The internal queue should have the entry (deprecated path —
        // nothing drains it, but the push keeps the type's contract).
        let q = tool.abandon_queue.lock().unwrap();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0], "toolu_abc");
    }

    #[test]
    fn test_tool_name_and_spec() {
        let tool = ContextAbandonTool::new();
        let spec = tool.spec();
        assert_eq!(spec.name, "context_abandon");
        assert!(spec.description.contains("placeholder"));
        // The retired sibling tool must not be advertised: placeholders are
        // not recallable (ADR-061 §10.1 v3), and `context_retrieve` itself
        // has been deleted.
        assert!(!spec.description.contains("context_retrieve"));
    }
}
