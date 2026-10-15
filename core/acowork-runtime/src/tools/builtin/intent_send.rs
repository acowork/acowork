//! Intent send tool — deliver an Intent to another Agent via the Gateway
//! (intent-delivery-tech-debt-plan D1/D3/D6).
//!
//! Before the tech-debt plan this tool published a raw MQTT
//! `ControlCommand` to the target's control topic — no online check, no
//! permission check, self-declared `from`, and an `async` parameter that
//! changed only the reply text. It now POSTs to the Gateway's single
//! delivery entry (`POST /api/intents`), which:
//!
//! - verifies the sender identity against the session (D4 — the node
//!   token this call carries ties it to the machine hosting the sender),
//! - checks both manifests' `intent:send` / `intent:receive`
//!   declarations (D8),
//! - wake-then-delivers an offline target through its hosting node and
//!   waits for the retained AgentStatus flip (D1/D2),
//! - records the delivery fact in the mailbox and returns the real
//!   outcome (D3).
//!
//! Delivery is synchronous by design: the tool result says *delivered*
//! or *failed with a reason* — there is no fire-and-forget mode to
//! promise (the old `async` parameter was a lie and is gone, D6).
//! "Delivered" means the target Runtime accepted the Intent; the
//! target's *answer* comes back later through the conversation channel
//! (design doc 27), not as this call's return value.

use acowork_core::agent_instance_id::AgentInstanceId;
use acowork_core::defaults::GATEWAY_HTTP_PORT;
use acowork_core::tools::traits::{Tool, ToolContext, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

/// Gateway HTTP delivery endpoint config for the tool.
#[derive(Debug, Clone)]
pub struct IntentSendConfig {
    /// This agent's INSTANCE identity (UUID v4) — the `sender` the
    /// Gateway verifies against the caller's session (D4).
    pub instance_id: String,
    /// Gateway HTTP host (same source as the global-resources pull).
    pub gateway_host: String,
    /// Node token presented as `X-ACowork-Node-Token` (machine
    /// identity; empty = unenrolled local mode).
    pub node_token: String,
}

pub struct IntentSendTool {
    config: IntentSendConfig,
    client: reqwest::Client,
}

impl IntentSendTool {
    pub fn new(config: IntentSendConfig) -> Self {
        Self {
            config,
            // Timeout: delivery waits for a cold-start wake (up to the
            // Gateway's INTENT_DEFAULT) plus slack; the Gateway owns the
            // real deadline, the client must not cut it short first.
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .expect("reqwest client build"),
        }
    }

    fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "intent_send".to_string(),
            description: "Send an Intent message to another Agent via Gateway routing. The Gateway delivers it (waking the target if it is offline) and returns the real delivery outcome. Both agents must declare the matching intent:send / intent:receive permission. Delivery is not a reply: the target's answer arrives later as a new turn in your session, not in this result.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Target agent INSTANCE identity (UUID v4, generated at install time by the Gateway). Look it up via the agent list — the package `agent_id` (e.g. 'com.example.calendar') does NOT address a runtime on the wire (ADR-073)."
                    },
                    "action": {
                        "type": "string",
                        "description": "Intent action name (e.g. 'schedule', 'query', 'notify')"
                    },
                    "params": {
                        "type": "object",
                        "description": "Intent payload (key-value data for the target Agent)"
                    },
                    "repo": {
                        "type": "string",
                        "description": "Optional git URL of a repository the task is about (design doc 27 §4.1). Format-checked by the Gateway; the target works only inside its own authorized directories."
                    },
                    "session_config": {
                        "type": "object",
                        "description": "Optional initial-config suggestion for the target's execution session, e.g. {\"model\": \"...\", \"provider\": \"...\"} (design doc 27 §4). Only a suggestion: the target applies it only when creating a new session and only within its own allowed model set; fields you omit use the target's defaults. Omit unless you have a concrete reason."
                    },
                    "conversation_id": {
                        "type": "string",
                        "description": "Optional conversation coordinate. Omit on the FIRST request (the Gateway assigns one and returns it below); pass the value from the returned message/turn metadata on every FOLLOW-UP in the same conversation, so the target answers in the same execution session and the per-conversation request budget is enforced."
                    }
                },
                "required": ["target", "action"]
            }),
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
impl Tool for IntentSendTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(&self, _params: Value, _work_dir: Option<&str>) -> acowork_core::error::Result<ToolResult> {
        // Routed through execute_with_context (needs the session id).
        Ok(Self::fail("intent_send requires session context"))
    }

    async fn execute_with_context(
        &self,
        params: Value,
        ctx: &ToolContext,
    ) -> acowork_core::error::Result<ToolResult> {
        let target = match params.get("target").and_then(|v| v.as_str()) {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => return Ok(Self::fail("Missing required parameter 'target'")),
        };
        let action = match params.get("action").and_then(|v| v.as_str()) {
            Some(a) if !a.trim().is_empty() => a.trim().to_string(),
            _ => return Ok(Self::fail("Missing required parameter 'action'")),
        };

        // ADR-073: target is the INSTANCE identity (UUID v4), not the
        // package `agent_id` — a package id cannot address a runtime on
        // the wire. Reject early so the LLM sees the contract explicitly.
        if AgentInstanceId::from_string(target.clone()).is_err() {
            return Ok(Self::fail(format!(
                "Invalid target '{target}'. Must be the target agent's INSTANCE UUID (ADR-073) — \
                 find it in the agent list; the package `agent_id` is display-only and \
                 cannot address a runtime on the wire."
            )));
        }

        let intent_params = params
            .get("params")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let repo = params.get("repo").and_then(|v| v.as_str()).map(String::from);
        // Design 27 §4: optional session-config suggestion — forwarded
        // verbatim; shape-checked by the Gateway, clamped by the responder.
        let session_config = params.get("session_config").cloned();
        // Design 27 §6.2: the initiator reuses one conversation_id across
        // follow-ups; the Gateway assigns it when absent and reports it
        // back so the model can copy it into the next request.
        let conversation_id = params
            .get("conversation_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(String::from);

        let url = format!(
            "http://{}:{}/api/intents",
            self.config.gateway_host, GATEWAY_HTTP_PORT
        );
        let body = serde_json::json!({
            "sender": self.config.instance_id,
            "sender_session_id": ctx.session_id,
            "target": target,
            "action": action,
            "params": intent_params,
            "repo": repo,
            "session_config": session_config,
            "conversation_id": conversation_id,
        });

        let mut req = self.client.post(&url).json(&body);
        if !self.config.node_token.is_empty() {
            req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, &self.config.node_token);
        }

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(Self::fail(format!(
                    "Intent delivery request to Gateway failed: {e}"
                )));
            }
        };

        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                return Ok(Self::fail(format!(
                    "Intent delivery response is not JSON (HTTP {status}): {e}"
                )));
            }
        };
        // Gateway-level rejection (identity / permission / validation):
        // `ApiError` shape `{ "error": ..., "code": ... }` or the
        // `not_authorized` `{ "code": "not_authorized", "message": ... }`
        // variant.
        if !status.is_success() {
            let detail = body
                .get("message")
                .or_else(|| body.get("error"))
                .and_then(|v| v.as_str())
                .unwrap_or("Gateway rejected the intent");
            return Ok(Self::fail(format!("Intent not accepted (HTTP {status}): {detail}")));
        }

        // D3: the response carries the real delivery outcome.
        let message_id = body
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let conversation_id = body
            .get("conversation_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match body.get("status").and_then(|v| v.as_str()) {
            Some("delivered") => Ok(ToolResult {
                ok: true,
                content: format!(
                    "Intent delivered to '{target}' action='{action}' (message_id: {message_id}, conversation_id: {conversation_id}). \
                     The target accepted it; the reply arrives later automatically as a new turn in this session — do not poll. \
                     For a follow-up in the same conversation pass conversation_id: \"{conversation_id}\"."
                ),
                error: None,
                token_usage: None,
            }),
            _ => {
                let code = body
                    .get("error_code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("delivery_unavailable");
                let err = body
                    .get("error")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown delivery failure");
                Ok(Self::fail(format!(
                    "Intent delivery failed ({code}): {err} (message_id: {message_id})"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::tools::traits::ToolContext;

    fn test_tool() -> IntentSendTool {
        IntentSendTool::new(IntentSendConfig {
            instance_id: "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c".to_string(),
            gateway_host: "127.0.0.1".to_string(),
            node_token: String::new(),
        })
    }

    #[test]
    fn test_intent_send_spec() {
        let spec = IntentSendTool::spec_value();
        assert_eq!(spec.name, "intent_send");
        assert!(spec.input_schema["properties"]["target"].is_object());
        assert!(spec.input_schema["properties"]["action"].is_object());
        // D6: the fake `async` parameter is gone from the contract.
        assert!(spec.input_schema["properties"].get("async").is_none());
    }

    #[tokio::test]
    async fn test_intent_send_missing_target() {
        let tool = test_tool();
        let result = tool
            .execute_with_context(
                serde_json::json!({ "action": "schedule" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'target'")
        );
    }

    #[tokio::test]
    async fn test_intent_send_missing_action() {
        let tool = test_tool();
        let result = tool
            .execute_with_context(
                serde_json::json!({ "target": "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c" }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!result.ok);
        assert!(
            result
                .error
                .unwrap()
                .contains("Missing required parameter 'action'")
        );
    }

    /// ADR-073 guard: a package id must be rejected before anything
    /// reaches the Gateway.
    #[tokio::test]
    async fn test_intent_send_rejects_package_id_target() {
        let tool = test_tool();
        let result = tool
            .execute_with_context(
                serde_json::json!({
                    "target": "com.example.calendar",
                    "action": "schedule"
                }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!result.ok);
        let err = result.error.expect("should have error message");
        assert!(err.contains("INSTANCE UUID"), "error: {err}");
    }

    /// No Gateway listening: the tool reports the transport failure
    /// instead of pretending the Intent was sent. Port 0 is invalid for
    /// a TCP connect, so the request fails without depending on any
    /// locally bound service.
    #[tokio::test]
    async fn test_intent_send_reports_gateway_unreachable() {
        let tool = IntentSendTool::new(IntentSendConfig {
            instance_id: "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c".to_string(),
            gateway_host: "localhost.invalid.acowork.test".to_string(),
            node_token: String::new(),
        });
        let result = tool
            .execute_with_context(
                serde_json::json!({
                    "target": "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c",
                    "action": "schedule",
                    "params": { "time": "10:00" }
                }),
                &ToolContext::default(),
            )
            .await
            .unwrap();
        assert!(!result.ok, "intent_send without Gateway should return ok: false");
        let err = result.error.expect("should have error message");
        assert!(
            err.contains("Intent delivery request to Gateway failed"),
            "error should explain the transport failure, got: {err}"
        );
    }
}
