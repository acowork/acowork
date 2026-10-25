//! Tool trait and related types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

/// Tool specification (name, description, input schema)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    /// Tool name (unique identifier)
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// JSON Schema for input parameters
    #[serde(rename = "parameters")]
    pub input_schema: Value,
}

/// Tool execution result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    /// Whether the execution was successful
    pub ok: bool,
    /// Result content (text or structured data)
    pub content: String,
    /// Optional error message
    #[serde(default)]
    pub error: Option<String>,
    /// Token usage statistics
    #[serde(default)]
    pub token_usage: Option<TokenUsage>,
}

/// Token usage statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Per-call execution context resolved by the agent loop.
///
/// Carries SESSION-SCOPED state to tools that need it (memory recall /
/// store). Such state must never live on the shared tool instance:
/// `AgentCore.all_tools` is an `Arc` reused by every concurrent SessionTask
/// of the agent, so instance-held session data is last-writer-wins across
/// sessions. The loop resolves this struct from its own per-session state at
/// each dispatch, mirroring how `work_dir` was already passed.
///
/// Owned fields (not `&str`) so the struct can be moved into parallel
/// tool-execution tasks.
#[derive(Debug, Clone, Default)]
pub struct ToolContext {
    /// Caller-resolved workspace directory (same semantics as
    /// [`Tool::execute`]'s `work_dir` argument).
    pub work_dir: Option<String>,
    /// ID of the session issuing the tool call.
    pub session_id: Option<String>,
    /// The current turn's user message for the session issuing the tool
    /// call. Vector-source memory recall uses it as full-sentence query
    /// text; absent for non-user-triggered executions (e.g. debug replay).
    pub current_user_message: Option<String>,
    /// Design 27 §5.3.2 / ADR-100 D9: the Chat session this execution session
    /// is bound to (persistent `chat_session_id` in conversation meta).
    /// Populated by the loop from the session's conversation binding so tools
    /// like `chat_history_read` can resolve the source chat without requiring
    /// the agent to carry internal ids in its text output.
    pub chat_session_id: Option<String>,
}

/// Core Tool trait that all tools must implement
#[async_trait]
pub trait Tool: Send + Sync {
    /// Get tool specification
    fn spec(&self) -> ToolSpec;

    /// Get tool name (convenience method)
    fn name(&self) -> String {
        self.spec().name.clone()
    }

    /// Execute the tool with given parameters.
    ///
    /// `work_dir` is the caller-resolved workspace directory path.
    /// Filesystem tools (file_read, file_write, etc.) use this as the
    /// base directory for relative path resolution, overriding any
    /// construction-time default. Non-filesystem tools may ignore it.
    ///
    /// Tools that need session-scoped state must override
    /// [`Self::execute_with_context`] instead of reading session data from
    /// shared instances.
    async fn execute(&self, params: Value, work_dir: Option<&str>) -> Result<ToolResult>;

    /// Session-aware execution entry point. The agent loop dispatches every
    /// tool call through this method.
    ///
    /// The default implementation delegates to [`Self::execute`] with
    /// `ctx.work_dir`, so stateless tools are unaffected. Tools that need
    /// `session_id` / `current_user_message` override this method (and keep
    /// [`Self::execute`] working for direct-call contexts such as tests).
    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> Result<ToolResult> {
        self.execute(params, ctx.work_dir.as_deref()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_spec_serialization_uses_parameters() {
        let spec = ToolSpec {
            name: "shell".to_string(),
            description: "test".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string"
                    }
                },
                "required": ["command"]
            }),
        };

        let serialized = serde_json::to_value(&spec).unwrap();

        // 验证序列化后使用 "parameters" 而不是 "input_schema"
        assert!(
            serialized.get("parameters").is_some(),
            "Should have 'parameters' field"
        );
        assert!(
            serialized.get("input_schema").is_none(),
            "Should NOT have 'input_schema' field"
        );

        // 验证 parameters 内容正确
        let params = serialized.get("parameters").unwrap();
        assert!(params.get("properties").unwrap().get("command").is_some());

        println!(
            "Serialized JSON: {}",
            serde_json::to_string_pretty(&serialized).unwrap()
        );
    }
}
