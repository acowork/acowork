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
use acowork_core::defaults::gateway_http_port;
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
            description: "Send an Intent message to another Agent via Gateway routing. The Gateway delivers it (waking the target if it is offline) and returns the real delivery outcome. Both agents must declare the matching intent:send / intent:receive permission. Delivery is not a reply: the target's answer arrives later as a new turn in your session, not in this result — and if your session is bound to a Chat, it is echoed there as well.".to_string(),
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
                    "attachments": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional workspace file paths to send WITH the intent (ADR-122). Each is uploaded to the platform's transfer area first and referenced by id; the receiver pulls the bytes into its own workspace before its turn starts. Absolute paths, or paths relative to your workspace."
                    },
                    "repo": {
                        "type": "string",
                        "description": "Optional git URL of a repository the task is about (design doc 27 §4.5.1). Format-checked by the Gateway; the target works only inside its own authorized directories."
                    },
                    "session_config": {
                        "type": "object",
                        "description": "Optional initial-config suggestion for the target's execution session, e.g. {\"model\": \"...\", \"provider\": \"...\"} (design doc 27 §4.5). Only a suggestion: the target applies it only when creating a new session and only within its own allowed model set; fields you omit use the target's defaults. Omit unless you have a concrete reason."
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

    /// Put one file into the platform's transfer area and return its id
    /// (ADR-122 §3 + §4.3).
    ///
    /// Two passes over the file on purpose: `create` needs the digest up
    /// front, and the digest has to be computed by streaming — reading a whole
    /// attachment into memory to hash it is exactly the failure mode the
    /// chunked route exists to avoid. The second pass streams the chunks.
    async fn upload_transfer(&self, path: &str, work_dir: &str) -> Result<String, String> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncReadExt;

        let resolved = {
            let p = std::path::Path::new(path);
            if p.is_absolute() || work_dir.is_empty() {
                p.to_path_buf()
            } else {
                std::path::Path::new(work_dir).join(p)
            }
        };
        let file = tokio::fs::File::open(&resolved)
            .await
            .map_err(|e| format!("{}: {e}", resolved.display()))?;
        let size = file
            .metadata()
            .await
            .map_err(|e| format!("{}: {e}", resolved.display()))?
            .len();
        if size == 0 {
            return Err("file is empty".to_string());
        }
        let name = resolved
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "attachment".to_string());

        // Pass 1: streamed digest.
        let mut reader = file;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; acowork_core::upload::CHUNK_BYTES as usize];
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| format!("read {}: {e}", resolved.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let sha256 = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();

        let base = format!(
            "http://{}:{}/api/transfers/uploads",
            self.config.gateway_host,
            gateway_http_port()
        );
        let sender = &self.config.instance_id;

        // Reserve an id.
        let mut req = self
            .client
            .post(format!("{base}?sender={sender}"))
            .json(&serde_json::json!({ "name": name, "size": size, "sha256": sha256 }));
        if !self.config.node_token.is_empty() {
            req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, &self.config.node_token);
        }
        let created = req.send().await.map_err(|e| format!("create: {e}"))?;
        if !created.status().is_success() {
            return Err(format!(
                "create refused ({}): {}",
                created.status(),
                created.text().await.unwrap_or_default()
            ));
        }
        let upload_id = created
            .json::<Value>()
            .await
            .ok()
            .and_then(|v| v.get("uploadId").and_then(|i| i.as_str()).map(str::to_string))
            .ok_or_else(|| "create answered no uploadId".to_string())?;

        // Pass 2: the chunks, in order — a resume here would need a HEAD, and
        // a fresh id has nothing to resume.
        let mut reader = tokio::fs::File::open(&resolved)
            .await
            .map_err(|e| format!("{}: {e}", resolved.display()))?;
        let mut offset: u64 = 0;
        loop {
            let n = reader
                .read(&mut buf)
                .await
                .map_err(|e| format!("read {}: {e}", resolved.display()))?;
            if n == 0 {
                break;
            }
            let mut req = self
                .client
                .put(format!("{base}/{upload_id}/{offset}?sender={sender}"))
                .body(buf[..n].to_vec());
            if !self.config.node_token.is_empty() {
                req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, &self.config.node_token);
            }
            let sent = req.send().await.map_err(|e| format!("chunk {offset}: {e}"))?;
            if !sent.status().is_success() {
                return Err(format!("chunk {offset} refused ({})", sent.status()));
            }
            offset += n as u64;
        }

        // Land it. The mime is what the receiving side types the file with;
        // an unknown extension stays octet-stream and the name carries the
        // meaning.
        let mime = mime_for(&name);
        let mut req = self.client.post(format!(
            "{base}/{upload_id}/complete?sender={sender}&mime={mime}"
        ));
        if !self.config.node_token.is_empty() {
            req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, &self.config.node_token);
        }
        let landed = req.send().await.map_err(|e| format!("complete: {e}"))?;
        if !landed.status().is_success() {
            return Err(format!(
                "complete refused ({}): {}",
                landed.status(),
                landed.text().await.unwrap_or_default()
            ));
        }
        Ok(upload_id)
    }

    fn fail(content: impl Into<String>) -> ToolResult {
        ToolResult {
            content_parts: None,
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
        Ok(Self::fail(
            "intent_send must be dispatched via execute_with_context (no session context reached the tool)",
        ))
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

        // ADR-122 §4.3 D11: files ride the transfer area, not the envelope.
        // Upload before sending so a reference the receiver cannot fetch
        // never leaves this call: the Gateway validates ownership and grants
        // the target access, both of which need the bytes to be there.
        let attachment_ids = match params.get("attachments").and_then(|v| v.as_array()) {
            Some(list) => {
                let work_dir = ctx.work_dir.clone().unwrap_or_default();
                let mut ids = Vec::with_capacity(list.len());
                for entry in list {
                    let Some(path) = entry.as_str().map(str::trim).filter(|p| !p.is_empty()) else {
                        return Ok(Self::fail("attachments must be non-empty file paths"));
                    };
                    match self.upload_transfer(path, &work_dir).await {
                        Ok(id) => ids.push(serde_json::json!({ "id": id })),
                        Err(e) => return Ok(Self::fail(format!("attachment {path}: {e}"))),
                    }
                }
                ids
            }
            None => Vec::new(),
        };

        let url = format!(
            "http://{}:{}/api/intents",
            self.config.gateway_host, gateway_http_port()
        );
        let body = serde_json::json!({
            "sender": self.config.instance_id,
            "sender_session_id": ctx.session_id,
            "target": target,
            "action": action,
            "params": intent_params,
            // ADR-122 D11: references only — the Gateway re-states the facts
            // from the transfer record, so nothing here is a claim.
            "attachments": attachment_ids,
            "repo": repo,
            "session_config": session_config,
            "conversation_id": conversation_id,
            // ADR-076 §决策 4 (rev.): forward this session's initiator as
            // the chain's originator. It is a human account id when a human
            // started the session and an agent instance id when another
            // agent did — the two are indistinguishable here (both UUID v4),
            // so the Gateway resolves it against the installed-agent table
            // and keeps it only when it names a human. That is what lets the
            // originating human stay aware of the agent-to-agent session
            // this call starts, even when they are not on either agent's
            // permission roster (a group-chat originator, a chain hop).
            "on_behalf_of": ctx.initiator,
            // ADR-076 §决策 4 (rev.) / design 27 §4.2.3: the chat surface
            // this execution is bound to, so a descendant intent's card /
            // delivery lands back on the human's original conversation
            // instead of a fresh agent-agent pair the human cannot see.
            // Empty when the session has no chat binding.
            "source_chat_session_id": ctx.chat_session_id,
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
                content_parts: None,
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

/// A small extension→mime map. Not a dependency: the transfer area needs a
/// type for `Content-Type` on the pull and nothing else, and every receiver
/// decides how to read a file by its own rules anyway.
fn mime_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        _ => "application/octet-stream",
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
