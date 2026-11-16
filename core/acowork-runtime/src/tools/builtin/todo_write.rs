//! Todo list management tool — LLM creates and manages a structured task list
//!
//! The actual todo state lives in `SessionState.todos`. This tool's `execute()`
//! is a no-op placeholder; the real logic is intercepted in `AgentLoop` which
//! parses the parameters, updates `SessionState.todos`, and injects the formatted
//! list into `ContextBuilder` so the LLM sees current TODOs in every system prompt.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Todo list management tool.
///
/// Registered as a built-in tool so the LLM knows about it and can call it,
/// but the actual todo list mutation happens via AgentLoop interception.
pub struct TodoWriteTool;

impl TodoWriteTool {
    /// Create a new TodoWriteTool instance.
    pub fn new() -> Self {
        Self
    }

    /// Return the static ToolSpec for `todo_write`.
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "todo_write".to_string(),
            description:
                "Create and manage a structured TODO list for your current working session. \
                 Update the list in real time as you work: write it before starting, mark each \
                 item in_progress when you begin it, and mark it completed as soon as you finish — \
                 Do not batch updates at the end of the TODO. \
                 Only one TODO list exists per session — each call replaces or merges into the current list."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The TODO items to set. Each item must have: id (unique string), content (description), status (one of: pending, in_progress, completed).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Unique identifier for this TODO item (e.g. a short slug like 'add-login')"
                                },
                                "content": {
                                    "type": "string",
                                    "description": "Human-readable TODO description"
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Current status: pending (not started), in_progress (working on it), completed (done)"
                                }
                            },
                            "required": ["id", "content", "status"]
                        }
                    },
                    "merge": {
                        "type": "boolean",
                        "description": "If true, merge with existing TODOs by id (update matching ids, add new ones). If false, replace the entire list. Default: false.",
                        "default": false
                    }
                },
                "required": ["todos"]
            }),
        }
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoWriteTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    /// Placeholder execution — the real logic is handled by AgentLoop
    /// interception in `loop_.rs`. This path should not be reached during
    /// normal operation; it returns a descriptive error if it is.
    async fn execute(
        &self,
        _params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        Ok(ToolResult {
            content_parts: None,
            ok: false,
            content: String::new(),
            error: Some(
                "todo_write must be handled by AgentLoop directly, not through Tool::execute"
                    .to_string(),
            ),
            token_usage: None,
        })
    }
}
