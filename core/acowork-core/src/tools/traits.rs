//! Tool trait and related types

use std::sync::{Arc, Mutex};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;
use crate::providers::traits::ContentPart;

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
    /// Non-text result payloads (images, documents).
    ///
    /// The `content` string stays the *only* thing the 32 KB
    /// [`crate`] output cap and the transcript persist; `content_parts`
    /// is the channel for tools whose real output is multimodal bytes
    /// (the `image_read` builtin, MCP `ImageContent`, future PDF page
    /// renders). The agent loop appends them as a user-role message
    /// immediately after the tool result; provider-native
    /// `tool_result` image blocks cover only 2 of the 5 supported
    /// protocols, so they are deliberately not the transport here.
    /// `OutputBoundedTool` truncates `content` only; parts pass
    /// through untouched by design (a truncated base64 payload is
    /// corrupt, not smaller).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_parts: Option<Vec<ContentPart>>,
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
#[derive(Debug, Clone)]
pub struct ToolContext {
    /// Whether the active model accepts image input; unknown capabilities default to true.
    pub supports_image_input: bool,
    /// Shared per-turn image_read accounting owned by the session loop.
    pub image_read_budget: Option<Arc<Mutex<ImageReadBudget>>>,
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
    /// ADR-076 decision 4 (rev.): the session's initiator, a human account id
    /// for a human-started session, an agent instance id for an
    /// agent-started one (the two are indistinguishable by format, so the
    /// consumer that needs the distinction resolves it against the installed
    /// agent table). `intent_send` forwards it as the intent's `on_behalf_of`
    /// so the Gateway can keep the originating human aware of the
    /// agent-to-agent session the current agent is about to start; the
    /// Gateway drops it when it names an agent rather than a human.
    /// `None` for a turn that belongs to no execution session (the chat
    /// router).
    pub initiator: Option<String>,
}

impl Default for ToolContext {
    fn default() -> Self {
        Self { supports_image_input: true, image_read_budget: None, work_dir: None,
            session_id: None, current_user_message: None, chat_session_id: None, initiator: None }
    }
}

/// Accounting for successful image_read results in a single user turn.
#[derive(Debug, Default)]
pub struct ImageReadBudget {
    pub images: usize,
    pub base64_bytes: usize,
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

        // Verify serialization uses "parameters" instead of "input_schema"
        assert!(
            serialized.get("parameters").is_some(),
            "Should have 'parameters' field"
        );
        assert!(
            serialized.get("input_schema").is_none(),
            "Should NOT have 'input_schema' field"
        );

        // Verify the parameters content is correct
        let params = serialized.get("parameters").unwrap();
        assert!(params.get("properties").unwrap().get("command").is_some());

        println!(
            "Serialized JSON: {}",
            serde_json::to_string_pretty(&serialized).unwrap()
        );
    }
}