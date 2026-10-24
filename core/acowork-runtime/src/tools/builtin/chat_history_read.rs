//! `chat_history_read` — read the Chat session history an Intent envelope
//! points at (design doc 27 §5.3.2, §7.3, §9).
//!
//! An agent @-mentioned in a group chat does **not** automatically receive
//! the whole group history (§5.2): the envelope carries the
//! `source_chat_session_id`, and the agent pulls only what it needs with
//! this tool. The read goes through the Gateway (`GET
//! /api/intents/chat-history`), which verifies the caller's node identity
//! and the user service enforces participation plus the hard caps — the
//! tool layer documents them, the server enforces them (§9).

use acowork_core::error::Result as CoreResult;
use acowork_core::tools::traits::{Tool, ToolContext, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use super::intent_send::IntentSendConfig;

/// Mirrors the Gateway hard caps (design 27 §5.6). The server is the
/// authority; these only shape the tool's own error text.
const MAX_COUNT: usize = 50;
const DEFAULT_COUNT: usize = 20;

pub struct ChatHistoryReadTool {
    config: IntentSendConfig,
    client: reqwest::Client,
}

impl ChatHistoryReadTool {
    pub fn new(config: IntentSendConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .expect("reqwest client build"),
        }
    }

    fn fail(content: impl Into<String>) -> ToolResult {
        ToolResult {
            ok: false,
            content: String::new(),
            error: Some(content.into()),
            token_usage: None,
        }
    }
}

#[async_trait]
impl Tool for ChatHistoryReadTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "chat_history_read".to_string(),
            description: "Read a Chat session's message log. Every execution session is bound to the Chat session it was started from (1:1 user-agent chat, agent-agent chat, or group chat). Your task brief already comes from that Chat — do NOT read chat history to \"get context\": it mostly belongs to other tasks and will dilute yours. Legitimate uses: (1) the task itself is about the chat log (summarize a discussion, find an earlier message); (2) your brief references a message you were not given — read one small window (count <= 5) to recover it. Returns up to 50 messages per call, newest window first; page older with `offset` (design 27 §5.3.2).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "chat_session_id": {
                        "type": "string",
                        "description": "Optional. A different Chat session id to read. Omit to read the chat this session is bound to."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "How many newest messages to skip before the window (paging toward older). Default 0."
                    },
                    "count": {
                        "type": "integer",
                        "description": "Messages to return, 1-50 (server hard cap). Default 20."
                    }
                },
                "required": []
            }),
        }
    }

    async fn execute(&self, _params: Value, _work_dir: Option<&str>) -> CoreResult<ToolResult> {
        Ok(Self::fail(
            "chat_history_read must be dispatched via execute_with_context (no session context reached the tool)",
        ))
    }

    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> CoreResult<ToolResult> {
        // Resolve the target Chat session: explicit parameter first, then
        // the session's persistent chat binding (from ToolContext). The agent
        // does not need to know internal ids — the runtime binding is enough.
        let param_id = params
            .get("chat_session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let session_id = match param_id.or_else(|| ctx.chat_session_id.clone()) {
            Some(ref s) if !s.is_empty() => s.clone(),
            _ => {
                return Ok(Self::fail(
                    "No chat_session_id provided and this session is not bound to a Chat session — pass a chat_session_id explicitly if you have one.",
                ));
            }
        };
        let offset = params.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let count = params
            .get("count")
            .and_then(|v| v.as_u64())
            .map(|c| c as usize)
            .unwrap_or(DEFAULT_COUNT)
            .clamp(1, MAX_COUNT);

        let url = format!(
            "http://{}:{}/api/intents/chat-history",
            self.config.gateway_host, acowork_core::defaults::GATEWAY_HTTP_PORT
        );
        let mut req = self.client.get(&url).query(&[
            ("sender", self.config.instance_id.as_str()),
            ("session_id", session_id.as_str()),
            ("offset", &offset.to_string()),
            ("count", &count.to_string()),
        ]);
        if !self.config.node_token.is_empty() {
            req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, &self.config.node_token);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return Ok(Self::fail(format!("chat history request failed: {e}"))),
        };
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return Ok(Self::fail(format!("chat history is not JSON (HTTP {status}): {e}"))),
        };
        if !status.is_success() {
            let detail = body
                .get("message")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("Gateway rejected the chat history read");
            return Ok(Self::fail(format!("chat history not available (HTTP {status}): {detail}")));
        }
        Ok(ToolResult {
            ok: true,
            content: body.to_string(),
            error: None,
            token_usage: None,
        })
    }
}
