//! OpenAI Responses API provider (ADR-103 D4/D5/D6).
//!
//! A standalone `Provider` implementation for `POST {base_url}/responses`,
//! deliberately NOT folded into `openai.rs`: the two wire formats must not
//! share cache keys, URL construction, or the compat-fallback chain
//! (ADR-103 D4). Protocol-agnostic layers (`Provider` trait, `StreamEvent`,
//! `ReliableProvider`, agent loop) are untouched by this provider shape.
//!
//! ## Slice status (ADR-103 §7)
//!
//! - S1: routing chain complete (Gateway resolves → MQTT delivers →
//!   Runtime constructs → this provider is reached).
//! - S2 (this slice): D5 request mapping + non-streaming `chat()` + usage
//!   mapping (D6 non-stream half + D6.5).
//! - S3: streaming SSE (`D6` streaming half) — `chat_stream()` still fails
//!   with a non-retryable endpoint-unavailable error, so a mis-routed
//!   session surfaces as a clear provider error instead of a silent
//!   fallback to `/chat/completions` (ADR-103 D8 MUST NOT list).
//! - S4: URL resolution refinement (`D7`) + full error classification (`D8`).

use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::Serialize;

use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall, MessageContent,
    MessageRole, Provider, ProviderError, ReasoningEffort, StreamEvent, ToolCall, UsageInfo,
    parse_data_uri,
};
use acowork_core::tools::schema::sanitize_tool_schema;

/// Responses-API-shaped provider. S2: non-streaming `chat()` is live;
/// `chat_stream()` is the S3 placeholder (visible failure, never a silent
/// fallback).
pub struct ResponsesProvider {
    /// Resolved `{base}` — the endpoint is `{base}/responses` (D7, S4).
    base_url: String,
    api_key: Option<String>,
    http_client: Client,
    #[allow(dead_code)] // consumed by the S3 streaming implementation
    request_timeout: Duration,
}

impl ResponsesProvider {
    /// Construct with explicit base URL and timeouts, mirroring the
    /// `OpenAIProvider::with_base_url_and_timeouts` shape so the router
    /// branch stays uniform.
    pub fn with_base_url_and_timeouts(
        base_url: Option<&str>,
        api_key: Option<&str>,
        request_timeout: Duration,
        connect_timeout: Duration,
        _stream_read_timeout: Duration,
    ) -> Self {
        let http_client = Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .build()
            .expect("Failed to build HTTP client");
        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            api_key: api_key.map(ToString::to_string),
            http_client,
            request_timeout,
        }
    }

    /// The S3 placeholder failure point: streaming is not implemented yet.
    /// Non-retryable with actionable user text (D8 wording family).
    fn streaming_not_implemented(&self) -> acowork_core::AcoworkError {
        tracing::warn!(
            base_url = %self.base_url,
            "ResponsesProvider.chat_stream invoked before ADR-103 S3: streaming wire mapping not implemented yet"
        );
        acowork_core::AcoworkError::Provider(ProviderError {
            message: format!(
                "Responses API streaming endpoint unavailable: the SSE wire mapping is not \
                 implemented in this build (ADR-103 S3 pending). Endpoint: {}/responses",
                self.base_url
            ),
            user_message: "该 provider 的 Responses 流式协议尚未生效，请在模型设置里把协议切回 Chat \
                           Completions，或等待该版本完成 Responses 流式支持。"
                .to_string(),
            status_code: Some(404),
            error_type: acowork_core::providers::traits::ProviderErrorType::ClientError,
            retryable: false,
            retry_after_ms: None,
        })
    }
}

// ── D5: request mapping (domain → Responses wire) ─────────────────────

/// One `input_text` / `input_image` / `output_text` content block.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct ContentItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// `input_image` only: the URL or `data:` URI verbatim (D5 — MUST NOT
    /// stuff a URL string into a base64 field).
    #[serde(rename = "image_url", skip_serializing_if = "Option::is_none")]
    image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

/// A `message` input item.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct MessageItem {
    #[serde(rename = "type")]
    kind: String,
    role: String,
    content: Vec<ContentItem>,
}

/// A `function_call` input item (assistant tool call, D5 row 6).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct FunctionCallItem {
    #[serde(rename = "type")]
    kind: String,
    call_id: String,
    name: String,
    arguments: String,
}

/// A `function_call_output` input item (role=Tool message, D5 row 7).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct FunctionCallOutputItem {
    #[serde(rename = "type")]
    kind: String,
    call_id: String,
    /// String output; content-block arrays when a tool produced images
    /// (supported by DeepSeek/MiniMax — S2 emits the plain string form).
    output: serde_json::Value,
}

/// The `input` array element. `untagged` keeps the explicit `type`
/// discriminators in the serialized JSON while the Rust side stays typed.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(untagged)]
enum InputItem {
    Message(MessageItem),
    FunctionCall(FunctionCallItem),
    FunctionCallOutput(FunctionCallOutputItem),
}

/// Flat tool spec: `{type:"function", name, description, parameters}` —
/// the Responses shape (D5 row 8; NOT the nested completions wrapper).
#[derive(Debug, Clone, Serialize, PartialEq)]
struct FlatTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    description: String,
    parameters: serde_json::Value,
}

/// `reasoning: { effort }` (D5 row 10). `Auto` omits the whole block.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct ReasoningParam {
    effort: String,
}

/// The serialized request body for `POST {base}/responses`.
#[derive(Debug, Clone, Serialize, PartialEq)]
struct NativeResponsesRequest {
    model: String,
    /// Merged system (+developer) instructions — the stable prefix half of
    /// ADR-060 semantics (D5 rows 1–2).
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    input: Vec<InputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningParam>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<FlatTool>>,
    stream: bool,
    /// v1 is stateless: never server-side stored (D5 YAGNI row).
    store: bool,
}

/// Map `reasoning_effort` to the Responses `reasoning.effort` string.
/// `Auto` → `None` (field omitted entirely); `Off` → `"none"` (MiniMax
/// may 400 on that — D8/S7 trim territory, not compensated client-side).
fn reasoning_effort_str(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::Auto => None,
        ReasoningEffort::Off => Some("none"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::Max => Some("max"),
    }
}

/// Flatten a domain tool definition to the Responses shape, accepting both
/// the flat `ToolSpec` form (`{name, description, parameters}` — what
/// `rebuild_context_tool_definitions` produces today) and the nested
/// OpenAI wrapper (`{type:"function", function:{…}}` — what the D5 table
/// and older fixtures describe). One converter, shape-drift-proof.
fn flatten_tool(tool: &serde_json::Value) -> FlatTool {
    let inner = tool.get("function").filter(|v| v.is_object()).unwrap_or(tool);
    let name = inner
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let description = inner
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let parameters = match inner.get("parameters") {
        Some(p) if p.is_object() => sanitize_tool_schema(p),
        _ => {
            tracing::warn!(
                tool_name = %name,
                "Responses convert_tools: missing/non-object parameters, using default schema"
            );
            serde_json::json!({"type": "object", "properties": {}})
        }
    };
    FlatTool {
        kind: "function".to_string(),
        name,
        description,
        parameters,
    }
}

/// Turn one domain message's content into Responses content blocks.
///
/// `assistant` images are dropped with a warn (D5 MUST — DeepSeek returns
/// 400 for images on `system`/`assistant`); `user` images map to
/// `input_image` with the URL/data-URI verbatim. A `data:` URI that does
/// not parse is skipped with a warn instead of being sent malformed.
fn content_blocks(msg: &ChatMessage, role: &str) -> Vec<ContentItem> {
    let mut blocks = Vec::new();
    let is_assistant = role == "assistant";
    match msg.message_content() {
        MessageContent::Text(text) => {
            if !text.is_empty() {
                blocks.push(text_block(role, text));
            }
        }
        MessageContent::Parts { leading_text, parts } => {
            if let Some(text) = leading_text.filter(|t| !t.is_empty()) {
                blocks.push(text_block(role, text));
            }
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        if !text.is_empty() {
                            blocks.push(text_block(role, text));
                        }
                    }
                    ContentPart::ImageUrl { image_url } => {
                        if is_assistant {
                            tracing::warn!(
                                "Responses map_request: dropping image from assistant message (D5: providers reject assistant-role images)"
                            );
                            continue;
                        }
                        if image_url.url.starts_with("data:")
                            && parse_data_uri(&image_url.url).is_none()
                        {
                            tracing::warn!(
                                url_prefix = %image_url.url.char_indices().take_while(|(i, _)| *i < 64).map(|(_, c)| c).collect::<String>(),
                                "Responses map_request: malformed base64 data URI, skipping image"
                            );
                            continue;
                        }
                        blocks.push(ContentItem {
                            kind: "input_image".to_string(),
                            text: None,
                            image_url: Some(image_url.url.clone()),
                            detail: image_url.detail.clone().or_else(|| Some("auto".to_string())),
                        });
                    }
                }
            }
        }
    }
    blocks
}

fn text_block(role: &str, text: &str) -> ContentItem {
    ContentItem {
        kind: if role == "assistant" {
            "output_text".to_string()
        } else {
            "input_text".to_string()
        },
        text: Some(text.to_string()),
        image_url: None,
        detail: None,
    }
}

/// D5 mapping: `ChatRequest` → `NativeResponsesRequest`.
///
/// Errors (non-retryable `ClientError`) instead of sending a request the
/// schema layer rejects: empty `input` (MiniMax requires it) and a
/// conversation that produced no user-visible items at all.
fn map_request(request: &ChatRequest) -> Result<NativeResponsesRequest, ProviderError> {
    let mut instructions: Vec<&str> = Vec::new();
    let mut input: Vec<InputItem> = Vec::new();

    for msg in &request.messages {
        match msg.role {
            // D5 rows 1–2: every system message merges into `instructions`
            // (`\n\n` join keeps ADR-060's stable-prefix semantics). The
            // domain has no `developer` role — if one is ever added, it
            // merges here too (D5 v1 ruling).
            MessageRole::System => {
                if !msg.content.trim().is_empty() {
                    instructions.push(msg.content.trim());
                }
            }
            MessageRole::User => {
                let blocks = content_blocks(msg, "user");
                if !blocks.is_empty() {
                    input.push(InputItem::Message(MessageItem {
                        kind: "message".to_string(),
                        role: "user".to_string(),
                        content: blocks,
                    }));
                }
            }
            MessageRole::Assistant => {
                // D5 row 6: one assistant message may yield a `message`
                // item (text) PLUS N `function_call` items.
                let blocks = content_blocks(msg, "assistant");
                if !blocks.is_empty() {
                    input.push(InputItem::Message(MessageItem {
                        kind: "message".to_string(),
                        role: "assistant".to_string(),
                        content: blocks,
                    }));
                }
                for tc in msg.tool_calls.as_deref().unwrap_or_default() {
                    input.push(InputItem::FunctionCall(FunctionCallItem {
                        kind: "function_call".to_string(),
                        call_id: tc.id.clone(),
                        name: tc.function.name.clone(),
                        arguments: tc.function.arguments.clone(),
                    }));
                }
            }
            MessageRole::Tool => {
                // D5 row 7: role=Tool + tool_call_id → function_call_output.
                let call_id = msg.tool_call_id.clone().unwrap_or_default();
                if call_id.is_empty() {
                    tracing::warn!(
                        "Responses map_request: tool message without tool_call_id; emitting empty call_id (provider will reject — surfaced as API error)"
                    );
                }
                input.push(InputItem::FunctionCallOutput(FunctionCallOutputItem {
                    kind: "function_call_output".to_string(),
                    call_id,
                    output: serde_json::Value::String(msg.content.clone()),
                }));
            }
        }
    }

    // D5 MUST: `input` is never empty (schema-level required on MiniMax
    // gateways; a request without user-visible content is a routing bug).
    if input.is_empty() {
        return Err(ProviderError {
            message: "Responses map_request: conversation produced an empty `input` array \
                      (only instructions were present)"
                .to_string(),
            user_message: "对话历史中没有可发送的用户消息，请重新发起对话。".to_string(),
            status_code: None,
            error_type: acowork_core::providers::traits::ProviderErrorType::ClientError,
            retryable: false,
            retry_after_ms: None,
        });
    }

    let reasoning = request
        .reasoning_effort
        .as_ref()
        .and_then(reasoning_effort_str)
        .map(|effort| ReasoningParam {
            effort: effort.to_string(),
        });

    // D5 row 12: `thinking_mode` is Anthropic-only — deliberately NOT sent.

    Ok(NativeResponsesRequest {
        model: request.model.clone(),
        instructions: (!instructions.is_empty()).then(|| instructions.join("\n\n")),
        input,
        max_output_tokens: request.max_tokens,
        temperature: request.temperature,
        reasoning,
        tools: request
            .tools
            .as_deref()
            .map(|items| items.iter().map(flatten_tool).collect()),
        stream: false,
        store: false,
    })
}

// ── D6 (non-stream half) + D6.5: response mapping ─────────────────────

/// Map a Responses JSON body (`POST /responses` non-streaming result) into
/// the domain `ChatResponse`.
///
/// `status` → `finish_reason` per the D6 table; `incomplete` +
/// `max_output_tokens` is a VISIBLE truncation (warn + `length`), never a
/// quiet empty answer. `failed` returns `Err` (same error family as
/// `from_http_parts`, ADR-016; full Responses-specific classification is
/// S4/D8).
fn map_response(body: serde_json::Value) -> Result<ChatResponse, ProviderError> {
    let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("");

    if status == "failed" {
        let code = body
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let message = body
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("in-band failure");
        return Err(ProviderError {
            message: format!("Responses API in-band failure: [{code}] {message}"),
            user_message: format!("模型返回失败：{message}"),
            status_code: None,
            error_type: acowork_core::providers::traits::ProviderErrorType::ServerError,
            retryable: false,
            retry_after_ms: None,
        });
    }

    let mut content = String::new();
    let mut reasoning_content = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    let output = body.get("output").and_then(|v| v.as_array());
    for item in output.into_iter().flatten() {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message" => {
                for part in item.get("content").and_then(|v| v.as_array()).into_iter().flatten() {
                    if part.get("type").and_then(|v| v.as_str()) == Some("output_text")
                        && let Some(t) = part.get("text").and_then(|v| v.as_str())
                    {
                        if !content.is_empty() {
                            content.push('\n');
                        }
                        content.push_str(t);
                    }
                }
            }
            "reasoning" => {
                // `content[].reasoning_text` and the `summary[]` fallback.
                for key in ["content", "summary"] {
                    for part in item.get(key).and_then(|v| v.as_array()).into_iter().flatten() {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            if !reasoning_content.is_empty() {
                                reasoning_content.push('\n');
                            }
                            reasoning_content.push_str(t);
                        }
                    }
                }
            }
            "function_call" => {
                tool_calls.push(ToolCall {
                    id: item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: item
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        arguments: item
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    },
                });
            }
            other => {
                tracing::debug!(item_type = %other, "Responses map_response: ignoring unknown output item");
            }
        }
    }

    // D6 status → finish_reason table.
    let finish_reason = if status == "incomplete" {
        let reason = body
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match reason {
            "content_filter" => "content_filter",
            // MUST be visible: the budget was eaten by the chain of thought
            // and there is no message body. Never a silent empty answer.
            _ => {
                tracing::warn!(
                    incomplete_reason = %reason,
                    "Responses API returned status=incomplete (max_output_tokens): output truncated by reasoning budget"
                );
                "length"
            }
        }
    } else if !tool_calls.is_empty() {
        "tool_calls"
    } else {
        "stop"
    };

    Ok(ChatResponse {
        content,
        reasoning_content: (!reasoning_content.is_empty()).then_some(reasoning_content),
        tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
        usage: map_usage(body.get("usage")),
        reasoning_started_at: None,
        reasoning_finished_at: None,
        finish_reason: Some(finish_reason.to_string()),
    })
}

/// D6.5 usage mapping with the `openai.rs:277` dual-naming tolerance
/// (`prompt_tokens.or(input_tokens)`): the Responses API names the fields
/// `input_*`/`output_*`, some gateways emit the completions names.
fn map_usage(usage: Option<&serde_json::Value>) -> Option<UsageInfo> {
    let u = usage?;
    let num = |keys: [&str; 2]| -> u64 {
        keys.iter()
            .find_map(|k| u.get(*k).and_then(|v| v.as_u64()))
            .unwrap_or(0)
    };
    let prompt = num(["input_tokens", "prompt_tokens"]);
    let completion = num(["output_tokens", "completion_tokens"]);
    if prompt == 0 && completion == 0 && u.get("total_tokens").is_none() {
        return None; // no usage block at all — keep None, downstream totals tolerate it
    }
    let total = u
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(prompt + completion);
    let cache_read = u
        .get("input_tokens_details")
        .or_else(|| u.get("prompt_tokens_details"))
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let reasoning = u
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    Some(UsageInfo {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cache_read_tokens: cache_read,
        cache_write_tokens: 0,
        reasoning_tokens: reasoning,
    })
}

// ── Provider trait ────────────────────────────────────────────────────

#[async_trait]
impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
    }

    async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
        let native = map_request(&request).map_err(acowork_core::AcoworkError::Provider)?;

        tracing::debug!(
            url = %format!("{}/responses", self.base_url),
            model = %native.model,
            input_items = native.input.len(),
            has_tools = native.tools.is_some(),
            "Responses chat request mapped (ADR-103 D5)"
        );

        let url = format!("{}/responses", self.base_url);
        let mut req_builder = self.http_client.post(&url);
        if let Some(ref api_key) = self.api_key {
            req_builder = req_builder.bearer_auth(api_key);
        }
        let response = req_builder.json(&native).send().await.map_err(|e| {
            acowork_core::AcoworkError::Provider(ProviderError::network(format!(
                "Responses API request failed: {e}"
            )))
        })?;

        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.text().await.unwrap_or_default();
            // S2 keeps the shared classification family; Responses-specific
            // 404/400 refinement is S4 (D7/D8).
            let err = crate::providers::from_http_parts(
                status.as_u16(),
                format!("Responses API error: {status} - {body}"),
                &headers,
            );
            return Err(acowork_core::AcoworkError::Provider(err));
        }

        let body: serde_json::Value = response.json().await.map_err(|e| {
            acowork_core::AcoworkError::Provider(ProviderError::unknown(format!(
                "Failed to parse Responses API JSON: {e}"
            )))
        })?;
        map_response(body).map_err(acowork_core::AcoworkError::Provider)
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> acowork_core::error::Result<
        Box<dyn futures_core::Stream<Item = StreamEvent> + Send>,
    > {
        Err(self.streaming_not_implemented())
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> acowork_core::error::Result<u64> {
        // Same local approximation as the OpenAI-compatible provider
        // (~4 chars/token); precise counting requires tiktoken and is
        // protocol-agnostic (token/counter.rs owns the calibrated path).
        let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
        Ok((total_chars as f64 / 4.0).ceil() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(base: &str) -> ResponsesProvider {
        ResponsesProvider::with_base_url_and_timeouts(
            Some(base),
            Some("sk-test"),
            Duration::from_secs(5),
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
    }

    fn req(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5".to_string(),
            messages,
            temperature: None,
            max_tokens: None,
            tools: None,
            reasoning_effort: None,
            thinking_mode: None,
        }
    }

    #[test]
    fn name_is_stable_identifier() {
        assert_eq!(provider("https://x.invalid/v1").name(), "openai-responses");
    }

    #[tokio::test]
    async fn stream_endpoint_fails_visibly_until_s3() {
        // The S1 routing chain still ends here for streaming: a mis-routed
        // session must NOT silently fall back to /chat/completions.
        let p = provider("https://x.invalid/v1");
        let err = p
            .chat_stream(req(vec![ChatMessage::user("hi")]))
            .await
            .err()
            .expect("streaming is an S3 stub");
        let msg = format!("{err:?}");
        assert!(msg.contains("S3"), "error text must name the pending slice: {msg}");
    }

    // ── D5 request mapping ────────────────────────────────────────────

    #[test]
    fn system_messages_merge_into_instructions() {
        let r = req(vec![
            ChatMessage::system("A"),
            ChatMessage::system("B"),
            ChatMessage::user("hello"),
        ]);
        let native = map_request(&r).unwrap();
        assert_eq!(native.instructions.as_deref(), Some("A\n\nB"));
        assert_eq!(native.input.len(), 1);
        assert!(!native.store);
        assert!(!native.stream);
    }

    #[test]
    fn user_text_and_image_map_to_input_blocks() {
        let mut m = ChatMessage::user("look".to_string());
        m.content_parts = Some(vec![ContentPart::ImageUrl {
            image_url: acowork_core::providers::traits::ImageUrlPart {
                url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                detail: Some("high".to_string()),
                width: None,
                height: None,
            },
        }]);
        let native = map_request(&req(vec![m])).unwrap();
        let InputItem::Message(msg) = &native.input[0] else {
            panic!("expected message item")
        };
        assert_eq!(msg.content.len(), 2);
        assert_eq!(msg.content[0].kind, "input_text");
        assert_eq!(msg.content[1].kind, "input_image");
        // URL verbatim — never re-encoded into a base64 field (D5 MUST).
        assert_eq!(
            msg.content[1].image_url.as_deref(),
            Some("data:image/png;base64,iVBORw0KGgo=")
        );
        assert_eq!(msg.content[1].detail.as_deref(), Some("high"));
    }

    #[test]
    fn assistant_images_are_dropped_with_warn() {
        let mut m = ChatMessage::assistant("noted".to_string());
        m.content_parts = Some(vec![ContentPart::ImageUrl {
            image_url: acowork_core::providers::traits::ImageUrlPart {
                url: "https://x/y.png".to_string(),
                detail: None,
                width: None,
                height: None,
            },
        }]);
        let native = map_request(&req(vec![ChatMessage::user("q"), m])).unwrap();
        let InputItem::Message(msg) = &native.input[1] else {
            panic!("expected message")
        };
        assert_eq!(msg.content.len(), 1, "image dropped");
        assert_eq!(msg.content[0].kind, "output_text");
    }

    #[test]
    fn assistant_tool_calls_become_function_call_items() {
        let mut m = ChatMessage::assistant(String::new());
        m.tool_calls = Some(vec![ToolCall {
            id: "call_1".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: "{\"path\":\"a\"}".to_string(),
            },
        }]);
        let mut out = ChatMessage {
            role: MessageRole::Tool,
            content: "42".to_string(),
            tool_call_id: Some("call_1".to_string()),
            ..Default::default()
        };
        out.name = Some("read_file".to_string());
        let native = map_request(&req(vec![ChatMessage::user("go"), m, out])).unwrap();
        assert!(matches!(native.input[1], InputItem::FunctionCall(_)));
        let InputItem::FunctionCall(fc) = &native.input[1] else {
            unreachable!()
        };
        assert_eq!((fc.call_id.as_str(), fc.name.as_str()), ("call_1", "read_file"));
        let InputItem::FunctionCallOutput(o) = &native.input[2] else {
            panic!("expected function_call_output, got {:?}", native.input[2])
        };
        assert_eq!(o.call_id, "call_1");
        assert_eq!(o.output, serde_json::json!("42"));
    }

    #[test]
    fn tools_flatten_from_both_shapes() {
        let flat = serde_json::json!({"name": "n1", "description": "d", "parameters": {"type": "object"}});
        let nested = serde_json::json!({"type": "function", "function": {"name": "n2", "description": "d2", "parameters": {"type": "object"}}});
        let r = ChatRequest {
            tools: Some(vec![flat, nested]),
            ..req(vec![ChatMessage::user("hi")])
        };
        let native = map_request(&r).unwrap();
        let tools = native.tools.unwrap();
        assert_eq!(tools[0].name, "n1");
        assert_eq!(tools[1].name, "n2");
        let json = serde_json::to_string(&tools[0]).unwrap();
        assert!(json.contains("\"type\":\"function\""));
        // No nested wrapper: the spec fields sit at top level next to `type`.
        assert!(json.contains("\"name\":\"n1\""), "flat name at top level: {json}");
    }

    #[test]
    fn reasoning_effort_mapping() {
        let base = req(vec![ChatMessage::user("hi")]);
        let auto = ChatRequest {
            reasoning_effort: Some(ReasoningEffort::Auto),
            ..base.clone()
        };
        assert!(map_request(&auto).unwrap().reasoning.is_none(), "Auto omits the field");
        let off = ChatRequest {
            reasoning_effort: Some(ReasoningEffort::Off),
            ..base.clone()
        };
        assert_eq!(map_request(&off).unwrap().reasoning.unwrap().effort, "none");
        let high = ChatRequest {
            reasoning_effort: Some(ReasoningEffort::High),
            ..base
        };
        assert_eq!(map_request(&high).unwrap().reasoning.unwrap().effort, "high");
    }

    #[test]
    fn empty_input_is_an_error_not_a_request() {
        // D5 MUST: instructions-only conversation must never hit the wire
        // (MiniMax schema requires `input`).
        let r = req(vec![ChatMessage::system("only instructions")]);
        let err = map_request(&r).unwrap_err();
        assert!(!err.retryable);
        assert!(err.message.contains("empty `input`"));
    }

    #[test]
    fn thinking_mode_is_never_sent() {
        let r = ChatRequest {
            thinking_mode: Some("extended".to_string()),
            ..req(vec![ChatMessage::user("hi")])
        };
        let json = serde_json::to_string(&map_request(&r).unwrap()).unwrap();
        assert!(!json.contains("thinking"), "D5 row 12: Anthropic-only field");
    }

    #[test]
    fn max_tokens_maps_to_max_output_tokens() {
        let r = ChatRequest {
            max_tokens: Some(1024),
            ..req(vec![ChatMessage::user("hi")])
        };
        let native = map_request(&r).unwrap();
        assert_eq!(native.max_output_tokens, Some(1024));
        let json = serde_json::to_string(&native).unwrap();
        assert!(json.contains("\"max_output_tokens\":1024"));
        assert!(!json.contains("max_tokens\""));
    }

    // ── D6/D6.5 response mapping ──────────────────────────────────────

    fn text_body() -> serde_json::Value {
        serde_json::json!({
            "id": "resp_1", "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "part1"},
                {"type": "output_text", "text": "part2"}
            ]}],
            "usage": {
                "input_tokens": 9, "output_tokens": 50, "total_tokens": 59,
                "input_tokens_details": {"cached_tokens": 4},
                "output_tokens_details": {"reasoning_tokens": 30}
            }
        })
    }

    #[test]
    fn completed_text_maps_to_stop_with_full_usage() {
        let resp = map_response(text_body()).unwrap();
        assert_eq!(resp.content, "part1\npart2");
        assert_eq!(resp.finish_reason.as_deref(), Some("stop"));
        let u = resp.usage.unwrap();
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.total_tokens),
            (9, 50, 59)
        );
        // ADR-066 accumulation chain must not lose these fields:
        assert_eq!(u.cache_read_tokens, 4);
        assert_eq!(u.reasoning_tokens, 30);
    }

    #[test]
    fn usage_tolerates_completions_naming() {
        // Dual-naming policy (openai.rs:277 precedent).
        let body = serde_json::json!({
            "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "x"}]}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10,
                       "prompt_tokens_details": {"cached_tokens": 2}}
        });
        let u = map_response(body).unwrap().usage.unwrap();
        assert_eq!((u.prompt_tokens, u.completion_tokens, u.cache_read_tokens), (7, 3, 2));
    }

    #[test]
    fn reasoning_item_maps_to_reasoning_content() {
        let body = serde_json::json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "thinking..."}]},
                {"type": "message", "content": [{"type": "output_text", "text": "answer"}]}
            ]
        });
        let resp = map_response(body).unwrap();
        assert_eq!(resp.reasoning_content.as_deref(), Some("thinking..."));
        assert_eq!(resp.content, "answer");
    }

    #[test]
    fn function_call_item_maps_to_tool_calls() {
        let body = serde_json::json!({
            "status": "completed",
            "output": [{"type": "function_call", "call_id": "c1", "name": "dispatch", "arguments": "{}"}]
        });
        let resp = map_response(body).unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
        let tcs = resp.tool_calls.unwrap();
        assert_eq!((tcs[0].id.as_str(), tcs[0].function.name.as_str()), ("c1", "dispatch"));
    }

    #[test]
    fn incomplete_max_tokens_is_visible_length_not_empty_answer() {
        let body = serde_json::json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [],
            "usage": {"input_tokens": 10, "output_tokens": 1024, "total_tokens": 1034}
        });
        let resp = map_response(body).unwrap();
        assert_eq!(resp.finish_reason.as_deref(), Some("length"));
        assert!(resp.content.is_empty());
        assert!(resp.usage.is_some());
    }

    #[test]
    fn incomplete_content_filter_maps_through() {
        let body = serde_json::json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "content_filter"},
            "output": []
        });
        assert_eq!(
            map_response(body).unwrap().finish_reason.as_deref(),
            Some("content_filter")
        );
    }

    #[test]
    fn failed_status_is_an_error() {
        let body = serde_json::json!({
            "status": "failed",
            "error": {"code": "server_error", "message": "boom"}
        });
        let err = map_response(body).unwrap_err();
        assert!(!err.retryable, "in-band failure is not blindly retried");
        assert!(err.message.contains("boom"));
    }

    #[test]
    fn absent_usage_block_stays_none() {
        let body = serde_json::json!({
            "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "hi"}]}]
        });
        assert!(map_response(body).unwrap().usage.is_none());
    }

    // ── S2 acceptance: fake server, three cases ───────────────────────

    fn chat_request_with(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            messages,
            ..req(vec![])
        }
    }

    #[tokio::test]
    async fn fake_server_text_tool_reasoning_three_cases() {
        use mockito::Server;
        let mut server = Server::new_async().await;

        // Case 1: plain text.
        let m1 = server
            .mock("POST", "/responses")
            .match_header("authorization", "Bearer sk-test")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(text_body().to_string())
            .expect(1)
            .create_async()
            .await;

        let p = provider(server.url().as_str());
        let resp = p
            .chat(chat_request_with(vec![
                ChatMessage::system("be terse"),
                ChatMessage::user("hi"),
            ]))
            .await
            .expect("text case");
        assert_eq!(resp.content, "part1\npart2");
        assert_eq!(resp.usage.unwrap().total_tokens, 59);
        m1.assert();

        // Case 2: tool call round-trip (request carries function_call_output).
        let m2 = server
            .mock("POST", "/responses")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "status": "completed",
                    "output": [{"type": "function_call", "call_id": "c9", "name": "read_file", "arguments": "{\"path\":\"x\"}"}]
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let mut assistant = ChatMessage::assistant(String::new());
        assistant.tool_calls = Some(vec![ToolCall {
            id: "c9".to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: "read_file".to_string(),
                arguments: "{}".to_string(),
            },
        }]);
        let tool_out = ChatMessage {
            role: MessageRole::Tool,
            content: "file body".to_string(),
            tool_call_id: Some("c9".to_string()),
            ..Default::default()
        };
        let resp = p
            .chat(chat_request_with(vec![
                ChatMessage::user("read x"),
                assistant,
                tool_out,
            ]))
            .await
            .expect("tool case");
        assert_eq!(resp.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(resp.tool_calls.unwrap()[0].function.name, "read_file");
        m2.assert();

        // Case 3: reasoning model.
        let m3 = server
            .mock("POST", "/responses")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                serde_json::json!({
                    "status": "completed",
                    "output": [
                        {"type": "reasoning", "content": [{"type": "reasoning_text", "text": "hmm"}]},
                        {"type": "message", "content": [{"type": "output_text", "text": "42"}]}
                    ],
                    "usage": {"input_tokens": 5, "output_tokens": 15, "total_tokens": 20,
                               "output_tokens_details": {"reasoning_tokens": 10}}
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let resp = p
            .chat(chat_request_with(vec![ChatMessage::user("answer")]))
            .await
            .expect("reasoning case");
        assert_eq!(resp.reasoning_content.as_deref(), Some("hmm"));
        assert_eq!(resp.usage.unwrap().reasoning_tokens, 10);
        m3.assert();
    }

    #[tokio::test]
    async fn fake_server_error_status_uses_shared_classification() {
        use mockito::Server;
        let mut server = Server::new_async().await;
        let m = server
            .mock("POST", "/responses")
            .with_status(429)
            .with_body("rate limited")
            .create_async()
            .await;
        let p = provider(server.url().as_str());
        let err = p
            .chat(chat_request_with(vec![ChatMessage::user("hi")]))
            .await
            .expect_err("429 must surface");
        let s = format!("{err:?}");
        assert!(s.contains("429") || s.to_lowercase().contains("rate"), "{s}");
        m.assert();
    }

    #[tokio::test]
    async fn instructions_only_never_reaches_the_wire() {
        use mockito::Server;
        let mut server = Server::new_async().await;
        let m = server
            .mock("POST", "/responses")
            .expect(0)
            .create_async()
            .await;
        let p = provider(server.url().as_str());
        let err = p
            .chat(chat_request_with(vec![ChatMessage::system("s")]))
            .await
            .expect_err("empty input must error");
        assert!(format!("{err:?}").contains("empty `input`"));
        m.assert();
    }
}
