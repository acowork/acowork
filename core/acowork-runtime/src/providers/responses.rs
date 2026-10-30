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
//! - S2: D5 request mapping + non-streaming `chat()` + usage mapping
//!   (D6 non-stream half + D6.5).
//! - S3: streaming SSE (`D6` streaming half) — a pure, I/O-free state
//!   machine (`SseState`) consumes `data:` payloads and the driver closes the
//!   stream on the terminal event, never on `[DONE]`.
//! - S4 (this slice): D8 error classification — HTTP failures go through
//!   [`acowork_core::providers::classify_http_error`] (404 endpoint /
//!   400 field / 400 content integrity), in-band and truncated streams use
//!   the shared `InBandFailure` / `TruncatedStream` classes. Nothing here
//!   re-sends a request to `/chat/completions`.
//! - D7 base-URL resolution and the shape-qualified compat-cache namespace
//!   live in `acowork_core::providers::url_resolve` / `providers::compat` and
//!   are consumed by `router.rs` and `openai.rs` respectively.

use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::Stream;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Serialize;
use tokio::sync::mpsc;

use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall, MessageContent,
    MessageRole, Provider, ProviderError, ProviderErrorType, ReasoningEffort, StreamError,
    StreamEvent, ToolCall, UsageInfo, parse_data_uri,
};
use acowork_core::providers::responses_errors::TRIMMABLE_FIELDS;
use acowork_core::tools::schema::sanitize_tool_schema;
use std::sync::Arc;

use crate::providers::compat::ErrorClass;
use crate::providers::responses_compat::{ResponsesCompatCache, ResponsesTrim, trim_key};

/// Responses-API-shaped provider. S2 delivered non-streaming `chat()`; S3
/// delivers `chat_stream()` (D6 SSE state machine, terminal-event driven).
pub struct ResponsesProvider {
    /// Resolved `{base}` — the endpoint is `{base}/responses` (D7, S4).
    base_url: String,
    api_key: Option<String>,
    http_client: Client,
    stream_read_timeout: Duration,
    /// Provider id used as the cache-key namespace. `None` (standalone mode,
    /// unit tests) disables the D8 trim cache entirely — a trim learned under
    /// no namespace could be read back by the wrong provider.
    cache_namespace: Option<String>,
    /// ADR-103 D8/S7 trim evidence for this provider's `/responses` traffic.
    compat_cache: Option<Arc<ResponsesCompatCache>>,
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
        stream_read_timeout: Duration,
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
            stream_read_timeout,
            cache_namespace: None,
            compat_cache: None,
        }
    }

    /// Install the D8 trim cache. Called once from
    /// `create_provider_with_wiring` with the same provider id the Gateway
    /// uses, so a `(provider, model)` entry learned here is addressed the same
    /// way as the probe verdict (ADR-103 D10).
    pub fn set_compat_wiring(&mut self, provider_id: String, cache: Arc<ResponsesCompatCache>) {
        self.cache_namespace = Some(provider_id);
        self.compat_cache = Some(cache);
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
///
/// `Auto` and `Off` both → `None` (field omitted entirely): "not controlling"
/// must not send `effort: "none"`, because MiniMax M3.1-Flash answers HTTP
/// 400 for that value (the D8 trim path would then silently drop the whole
/// `reasoning` block, which is not what the user asked for). The Responses
/// shape accepts the full level range: minimal / low / medium / high / xhigh /
/// max.
fn reasoning_effort_str(effort: &ReasoningEffort) -> Option<&'static str> {
    match effort {
        ReasoningEffort::Auto => None,
        ReasoningEffort::Off => None,
        ReasoningEffort::Minimal => Some("minimal"),
        ReasoningEffort::Low => Some("low"),
        ReasoningEffort::Medium => Some("medium"),
        ReasoningEffort::High => Some("high"),
        ReasoningEffort::XHigh => Some("xhigh"),
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

// -- D6: streaming SSE -------------------------------------------------

/// What one decoded `data:` payload asks the streaming driver to do.
#[derive(Debug)]
enum SseAction {
    /// Known-noise or validation-only event -- nothing observable.
    Ignore,
    /// Non-terminal events to forward downstream.
    Emit(Vec<StreamEvent>),
    /// Terminal event (`completed` / `incomplete` / `failed`): forward these
    /// and close the stream. This is the ONLY successful way a stream ends
    /// (ADR-103 D6.1 -- `[DONE]` is never the terminator).
    Finish(Vec<StreamEvent>),
}

/// Lifecycle / validation-only events: parsed then dropped, never re-emitted.
///
/// Two families live here (D6.7):
/// - validation-only (`*.done`, `content_part.*`, `output_item.done`): the
///   text already reached the loop as deltas; re-emitting would duplicate.
/// - lifecycle noise (`created`, `in_progress`, `queued`).
const KNOWN_NOISE_EVENTS: &[&str] = &[
    "response.created",
    "response.in_progress",
    "response.queued",
    "response.content_part.added",
    "response.content_part.done",
    "response.output_text.done",
    "response.reasoning_text.done",
    "response.reasoning_summary_text.done",
    "response.reasoning_summary_part.added",
    "response.reasoning_summary_part.done",
    "response.function_call_arguments.done",
];

/// Tool-shaped event families v1 does not consume: noisy by design and
/// unrelated to chat / tool-call streaming. Ignored without a warning.
const KNOWN_NOISE_PREFIXES: &[&str] = &[
    "response.file_search_call.",
    "response.web_search_call.",
    "response.code_interpreter_call.",
    "response.image_generation_call.",
    "response.mcp_call.",
    "response.mcp_list_tools.",
    "response.audio.",
    "response.refusal.",
];

fn is_known_noise(event_type: &str) -> bool {
    KNOWN_NOISE_EVENTS.contains(&event_type)
        || KNOWN_NOISE_PREFIXES
            .iter()
            .any(|prefix| event_type.starts_with(prefix))
}

/// Pure, I/O-free Responses SSE state machine (ADR-103 D6).
///
/// Testable boundary: feed `data:` payloads in wire order, assert the
/// emitted `StreamEvent`s. Deliberately stateless with respect to
/// `sequence_number` -- D6.6 forbids reordering or de-duplication on it, so
/// events are consumed strictly in arrival order.
struct SseState {
    /// Set once a terminal event was observed (success or failure).
    terminated: bool,
    /// Whether any `function_call` item appeared (drives `finish_reason`).
    saw_function_call: bool,
    /// `output_index` -> assigned tool-call index, fixed at item-add time so
    /// argument chunks cannot drift to another index.
    tool_index_by_output: HashMap<u64, u64>,
    next_tool_index: u64,
    /// Usage is authoritative only in the terminal event's `response`.
    usage: Option<UsageInfo>,
    /// D6.7: warn about unknown event types once per stream, never fail.
    warned_unknown: bool,
}

impl SseState {
    fn new() -> Self {
        Self {
            terminated: false,
            saw_function_call: false,
            tool_index_by_output: HashMap::new(),
            next_tool_index: 0,
            usage: None,
            warned_unknown: false,
        }
    }

    /// Resolve (or first-time assign) the tool-call index for an
    /// `output_index`. Assignment order defines the downstream index, which
    /// keeps parallel tool calls stable.
    fn assign_tool_index(&mut self, output_index: u64) -> u64 {
        if let Some(existing) = self.tool_index_by_output.get(&output_index) {
            return *existing;
        }
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.tool_index_by_output.insert(output_index, index);
        index
    }

    /// D6.5: usage comes from the terminal event's `response.usage` only.
    fn absorb_terminal_usage(&mut self, value: &serde_json::Value) {
        self.usage = map_usage(value.get("response").and_then(|r| r.get("usage")));
    }

    fn finished_event(&self, finish_reason: &str) -> StreamEvent {
        StreamEvent::Finished(ChatResponse {
            content: String::new(),
            tool_calls: None,
            usage: self.usage.clone(),
            finish_reason: Some(finish_reason.to_string()),
            ..Default::default()
        })
    }

    /// Dispatch one JSON `data:` payload (D6.2: the type is read from the
    /// payload, never from the `event:` line).
    fn on_data(&mut self, payload: &str) -> Result<SseAction, StreamError> {
        let value: serde_json::Value = serde_json::from_str(payload).map_err(|e| StreamError {
            message: format!("Responses SSE payload is not valid JSON: {e}"),
            // Mid-stream corruption; D8 refines whether a retry is safe once
            // output has already been emitted (S4).
            error_type: ProviderErrorType::StreamDecodeError,
            retryable: true,
            status_code: None,
        })?;

        let event_type = value.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            // -- content deltas --
            "response.output_text.delta" => {
                let delta = value.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    Ok(SseAction::Ignore)
                } else {
                    Ok(SseAction::Emit(vec![StreamEvent::Content(delta.to_string())]))
                }
            }
            // D6.3: `reasoning_text` (native) and `reasoning_summary_text`
            // (summary-mode models) are both reasoning deltas; one is not a
            // fallback for the other.
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let delta = value.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    Ok(SseAction::Ignore)
                } else {
                    Ok(SseAction::Emit(vec![StreamEvent::ReasoningContent(
                        delta.to_string(),
                    )]))
                }
            }

            // -- tool calls --
            // D6.4: `call_id` / `name` are known only from `output_item.added`;
            // the argument deltas carry no identity at all.
            "response.output_item.added" => {
                let item = value.get("item");
                let item_type = item
                    .and_then(|i| i.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");
                if item_type != "function_call" {
                    return Ok(SseAction::Ignore);
                }
                let output_index = value
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                self.assign_tool_index(output_index);
                self.saw_function_call = true;
                let call_id = item
                    .and_then(|i| i.get("call_id"))
                    .or_else(|| item.and_then(|i| i.get("id")))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .and_then(|i| i.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                Ok(SseAction::Emit(vec![StreamEvent::ToolCallStart(ToolCall {
                    id: call_id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        // Arguments arrive as deltas, never on the added item.
                        arguments: String::new(),
                    },
                })]))
            }
            "response.function_call_arguments.delta" => {
                let delta = value.get("delta").and_then(|d| d.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Ok(SseAction::Ignore);
                }
                let output_index = value
                    .get("output_index")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                if !self.tool_index_by_output.contains_key(&output_index) {
                    // A chunk without a preceding `output_item.added` (lost or
                    // reordered event). Keep the arguments flowing; the loop
                    // reconciles by index.
                    tracing::warn!(
                        output_index,
                        "Responses function_call argument chunk arrived without output_item.added -- assigning a fresh tool index"
                    );
                }
                let index = self.assign_tool_index(output_index);
                Ok(SseAction::Emit(vec![StreamEvent::ToolCallChunk {
                    index,
                    arguments: delta.to_string(),
                }]))
            }
            // Validation only: the `added` item already opened the call and
            // the arguments already streamed as deltas.
            "response.output_item.done" => {
                if let Some(item) = value
                    .get("item")
                    .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("function_call"))
                {
                    let call_id = item.get("call_id").and_then(|c| c.as_str()).unwrap_or("?");
                    let args = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
                    if !args.trim().is_empty()
                        && serde_json::from_str::<serde_json::Value>(args).is_err()
                    {
                        tracing::warn!(
                            call_id,
                            "Responses function_call arguments are not valid JSON"
                        );
                    }
                }
                Ok(SseAction::Ignore)
            }

            // -- terminal events (D6.1: these alone end the stream) --
            "response.completed" => {
                self.absorb_terminal_usage(&value);
                self.terminated = true;
                let finish_reason = if self.saw_function_call {
                    "tool_calls"
                } else {
                    "stop"
                };
                Ok(SseAction::Finish(vec![self.finished_event(finish_reason)]))
            }
            "response.incomplete" => {
                self.absorb_terminal_usage(&value);
                let reason = value
                    .get("response")
                    .and_then(|r| r.get("incomplete_details"))
                    .and_then(|d| d.get("reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("max_output_tokens");
                let finish_reason = if reason == "content_filter" {
                    "content_filter"
                } else {
                    "length"
                };
                tracing::warn!(
                    reason = %reason,
                    finish_reason,
                    "Responses stream ended incomplete -- truncation is surfaced through finish_reason, never as a silently empty answer (ADR-103 section 5)"
                );
                self.terminated = true;
                Ok(SseAction::Finish(vec![self.finished_event(finish_reason)]))
            }
            "response.failed" | "error" => {
                let err = value
                    .get("response")
                    .and_then(|r| r.get("error"))
                    .or_else(|| value.get("error"));
                let message = err
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("Responses API stream reported a terminal failure");
                let code = err
                    .and_then(|e| e.get("code"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                // D8: deltas may already have reached the loop, so an in-band
                // failure is never retried blindly.
                let stream_err = acowork_core::providers::in_band_failure(code, message);
                tracing::warn!(code = %code, message = %message, "Responses stream failed in-band");
                self.terminated = true;
                Ok(SseAction::Finish(vec![StreamEvent::Error(stream_err)]))
            }

            other => {
                if is_known_noise(other) {
                    tracing::trace!(event_type = other, "Responses SSE known-noise event ignored");
                } else if !self.warned_unknown {
                    self.warned_unknown = true;
                    tracing::warn!(
                        event_type = other,
                        "Unrecognized Responses SSE event type -- ignored (warned once per stream, ADR-103 D6.7)"
                    );
                }
                Ok(SseAction::Ignore)
            }
        }
    }
}

/// Per-stream counters for the single summary log line (no per-chunk noise).
#[derive(Default)]
struct SseCounters {
    payloads: u64,
    content_chars: u64,
    reasoning_chars: u64,
    tool_calls: u64,
}

/// Feed one raw SSE line to the state machine.
///
/// Returns `false` when the driver must stop reading: terminal event
/// emitted, mid-stream decode failure emitted, or the consumer hung up.
async fn dispatch_sse_line(
    tx: &mpsc::Sender<Option<StreamEvent>>,
    state: &mut SseState,
    counters: &mut SseCounters,
    line: &str,
) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    // `event:` lines are optional and NOT authoritative (D6.2); comments,
    // `id:` and `retry:` carry nothing v1 needs.
    let Some(payload) = line.strip_prefix("data:") else {
        return true;
    };
    let payload = payload.trim();
    if payload.is_empty() {
        return true;
    }
    if payload == "[DONE]" {
        // Accepted, never required (D6.1). Termination is decided by the
        // terminal event or by the EOF truncation check.
        tracing::debug!(
            "Responses SSE [DONE] marker ignored -- termination is terminal-event driven"
        );
        return true;
    }

    counters.payloads += 1;
    match state.on_data(payload) {
        Ok(SseAction::Ignore) => true,
        Ok(SseAction::Emit(events)) => {
            for event in events {
                match &event {
                    StreamEvent::Content(text) => {
                        counters.content_chars += text.chars().count() as u64;
                    }
                    StreamEvent::ReasoningContent(text) => {
                        counters.reasoning_chars += text.chars().count() as u64;
                    }
                    StreamEvent::ToolCallStart(_) => counters.tool_calls += 1,
                    _ => {}
                }
                if tx.send(Some(event)).await.is_err() {
                    return false; // consumer dropped
                }
            }
            true
        }
        Ok(SseAction::Finish(events)) => {
            for event in events {
                let _ = tx.send(Some(event)).await;
            }
            let _ = tx.send(None).await;
            tracing::debug!(
                payloads = counters.payloads,
                content_chars = counters.content_chars,
                reasoning_chars = counters.reasoning_chars,
                tool_calls = counters.tool_calls,
                "Responses SSE stream closed by terminal event"
            );
            false
        }
        Err(err) => {
            let _ = tx.send(Some(StreamEvent::Error(err))).await;
            let _ = tx.send(None).await;
            false
        }
    }
}

/// `mpsc::Receiver<Option<StreamEvent>>` -> `Stream`. `None` terminates.
struct ChannelStream {
    rx: mpsc::Receiver<Option<StreamEvent>>,
}

impl Stream for ChannelStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(Some(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(None)) | Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl ResponsesProvider {
    /// ADR-103 D8 — classify a failed HTTP response through the shared
    /// Responses table (core owns the classes), keeping the platform-wide
    /// `Retry-After` handling.
    ///
    /// Nothing here may re-send the request to `/chat/completions` (D8/D9).
    fn classify_failure(
        &self,
        status: u16,
        body: String,
        headers: &reqwest::header::HeaderMap,
        model: &str,
    ) -> ProviderError {
        let mut error = acowork_core::providers::classify_http_error(status, &body);
        if let Some(retry_after_ms) = crate::providers::parse_retry_after_header(headers) {
            error.retry_after_ms = Some(retry_after_ms);
            error.refresh_user_message();
        }
        match &error.error_type {
            ProviderErrorType::EndpointUnavailable => tracing::warn!(
                base_url = %self.base_url,
                model,
                status,
                "provider does not serve POST /responses — the model protocol must be switched back to Chat Completions (ADR-103 D8)"
            ),
            ProviderErrorType::UnsupportedField => tracing::warn!(
                base_url = %self.base_url,
                model,
                field = error.unsupported_field().unwrap_or(""),
                "provider rejected a Responses request field (ADR-103 D8; trimming is the S7 whitelist)"
            ),
            _ => {}
        }
        error
    }

    /// Bridge a `text/event-stream` body into `StreamEvent`s.
    ///
    /// I/O shell only: line buffering, per-chunk read timeout, and the
    /// terminal / truncation decision. All protocol semantics live in
    /// [`SseState`], so they are testable without a socket.
    fn sse_to_stream(
        response: reqwest::Response,
        stream_read_timeout: Duration,
    ) -> Box<dyn Stream<Item = StreamEvent> + Send> {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut state = SseState::new();
            let mut counters = SseCounters::default();

            loop {
                let chunk = match tokio::time::timeout(stream_read_timeout, stream.next()).await {
                    Ok(Some(Ok(chunk))) => chunk,
                    Ok(Some(Err(e))) => {
                        let err = acowork_core::providers::classify_stream_error(&format!(
                            "Responses SSE transport error: {e}"
                        ));
                        let _ = tx.send(Some(StreamEvent::Error(err))).await;
                        return; // MUST NOT emit Finished
                    }
                    Ok(None) => break, // EOF -- the terminal check below decides
                    Err(_) => {
                        tracing::warn!(
                            timeout_secs = stream_read_timeout.as_secs(),
                            "Responses SSE silence: no data within the read timeout"
                        );
                        let _ = tx
                            .send(Some(StreamEvent::Error(StreamError::stream_timeout(
                                stream_read_timeout.as_secs(),
                            ))))
                            .await;
                        return;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = buffer.find('\n') {
                    let line: String = buffer.drain(..=pos).collect();
                    if !dispatch_sse_line(&tx, &mut state, &mut counters, &line).await {
                        return;
                    }
                }
            }

            // EOF with a trailing line that had no newline terminator.
            if !buffer.is_empty() {
                let tail = std::mem::take(&mut buffer);
                if !dispatch_sse_line(&tx, &mut state, &mut counters, &tail).await {
                    return;
                }
            }

            if !state.terminated {
                // D6.1: no terminal event => truncated. `[DONE]` is not a
                // terminator, so a stream that only sent `[DONE]` lands here.
                tracing::warn!(
                    payloads = counters.payloads,
                    content_chars = counters.content_chars,
                    reasoning_chars = counters.reasoning_chars,
                    tool_calls = counters.tool_calls,
                    "Responses SSE ended without a terminal event -- reporting a truncated stream"
                );
                let _ = tx
                    .send(Some(StreamEvent::Error(
                        acowork_core::providers::truncated_stream(
                            counters.content_chars,
                            counters.tool_calls,
                        ),
                    )))
                    .await;
            }
        });

        Box::new(ChannelStream { rx })
    }
}

// ── D8 / S7: the `reasoning` trim (single-field, evidence-gated) ──────

/// One HTTP send that already accounts for the D8 trim.
struct TrimmedSend {
    response: reqwest::Response,
    /// What was actually dropped from the body (`omit[]` for a verbatim send).
    applied: ResponsesTrim,
}

impl ResponsesProvider {
    /// Cache address for one model, or `None` when no cache/namespace is
    /// installed (standalone mode + unit tests stay cache-free by design).
    fn compat_key(&self, model: &str) -> Option<String> {
        let namespace = self.cache_namespace.as_deref()?;
        self.compat_cache.as_ref()?;
        Some(trim_key(namespace, model))
    }

    /// One POST to `{base}/responses`. No trim logic here — this is the I/O
    /// primitive [`Self::send_with_trim`] re-runs at most twice.
    async fn post_native(
        &self,
        native: &NativeResponsesRequest,
        stream: bool,
    ) -> Result<reqwest::Response, ProviderError> {
        let url = format!("{}/responses", self.base_url);
        let mut req_builder = self.http_client.post(&url);
        if let Some(ref api_key) = self.api_key {
            req_builder = req_builder.bearer_auth(api_key);
        }
        if stream {
            req_builder = req_builder.header("Accept", "text/event-stream");
        }
        req_builder.json(native).send().await.map_err(|e| {
            ProviderError::network(if stream {
                format!("Responses API streaming request failed: {e}")
            } else {
                format!("Responses API request failed: {e}")
            })
        })
    }

    /// Send `request` with the D8 trim discipline.
    ///
    /// Deliberately **not** a fallback chain (D8 "明确不做"): at most two
    /// requests are ever sent, and only fields on the whitelist
    /// ([`TRIMMABLE_FIELDS`]) may be dropped. The decision to drop one is
    /// evidence-gated through the shared [`CompatCache`](super::compat::CompatCache)
    /// policy — three confirmations promote a candidate to durable, a plain
    /// success resets pending evidence, and a fast-path failure invalidates.
    ///
    /// Returns the response plus the trim that was applied, so callers can log
    /// `stripped_fields` without re-deriving what happened.
    async fn send_with_trim(
        &self,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<TrimmedSend, ProviderError> {
        let mut native = map_request(request)?;
        native.stream = stream;

        tracing::debug!(
            url = %format!("{}/responses", self.base_url),
            model = %native.model,
            input_items = native.input.len(),
            has_tools = native.tools.is_some(),
            stream,
            "Responses request mapped (ADR-103 D5)"
        );

        // Nothing to learn when the body carries no trimmable field at all:
        // a 400 cannot be about a field we never sent.
        if native.reasoning.is_none() && native.temperature.is_none() {
            let response = self.post_native(&native, stream).await?;
            if response.status().is_success() {
                return Ok(TrimmedSend {
                    response,
                    applied: ResponsesTrim::none(),
                });
            }
            return Err(self.reject(response, &native.model).await);
        }

        let key = self.compat_key(&native.model);
        let mut applied = ResponsesTrim::none();

        // ── Fast path: a durable trim says this provider rejects the field ──
        let mut cached_hit = false;
        if let Some(key) = key.as_deref()
            && let Some(cache) = self.compat_cache.as_ref()
            && let Some(learned) = cache.get(key)
        {
            if learned.omit_reasoning && native.reasoning.is_some() {
                native.reasoning = None;
                applied.omit_reasoning = true;
            }
            if learned.omit_temperature && native.temperature.is_some() {
                native.temperature = None;
                applied.omit_temperature = true;
            }
            cached_hit = !applied.is_none();
            if cached_hit {
                tracing::warn!(
                    key,
                    model = %native.model,
                    stripped = ?applied.stripped_fields(),
                    "Responses fast path applied a learned trim (ADR-103 D8) - the request omits those fields"
                );
            }
        }

        let response = self.post_native(&native, stream).await?;
        let model = native.model.clone();
        if response.status().is_success() {
            if let Some(key) = key.as_deref()
                && let Some(cache) = self.compat_cache.as_ref()
            {
                if !applied.is_none() {
                    cache.record_fallback_success(key, applied.clone(), ErrorClass::RequestSchema);
                } else {
                    // Verbatim success: contradicting evidence, the accident
                    // window (if any) is over.
                    cache.record_plain_success(key);
                }
            }
            return Ok(TrimmedSend { response, applied });
        }

        let error = self.reject(response, &model).await;

        // A rejected cached trim means the provider changed under us: drop the
        // evidence and report. Re-sending the *fuller* body would only repeat
        // the same 400.
        if cached_hit {
            if let Some(key) = key.as_deref()
                && let Some(cache) = self.compat_cache.as_ref()
            {
                cache.invalidate(key, "learned Responses trim rejected on fast path");
            }
            tracing::warn!(
                model = %model,
                status = error.status_code,
                "Responses fast-path trim no longer works - cache invalidated (ADR-103 D8)"
            );
            return Err(error);
        }

        // ── Retry: only a whitelisted field, only once (D8) ────────────────
        let trimmable = error
            .unsupported_field()
            .filter(|field| TRIMMABLE_FIELDS.contains(field));
        let Some(field) = trimmable else {
            return Err(error);
        };
        // Drop exactly the field the endpoint named. A field that is not in
        // the body cannot be the 400's cause; re-sending would repeat it.
        match field {
            "reasoning" if native.reasoning.is_some() => {
                native.reasoning = None;
                applied.omit_reasoning = true;
            }
            "temperature" if native.temperature.is_some() => {
                native.temperature = None;
                applied.omit_temperature = true;
            }
            _ => return Err(error),
        }
        tracing::warn!(
            model = %model,
            field,
            status = error.status_code,
            "provider rejected `{field}` - retrying once with the field omitted (ADR-103 D8 trim)"
        );

        let retry = self.post_native(&native, stream).await?;
        if retry.status().is_success() {
            if let Some(key) = key.as_deref()
                && let Some(cache) = self.compat_cache.as_ref()
            {
                cache.record_fallback_success(key, applied.clone(), ErrorClass::RequestSchema);
            }
            tracing::warn!(
                model = %model,
                stripped = ?applied.stripped_fields(),
                "Responses request succeeded only after the D8 trim - this (provider, model) is DEGRADED: the stripped fields' settings are not applied"
            );
            return Ok(TrimmedSend {
                response: retry,
                applied,
            });
        }
        Err(self.reject(retry, &model).await)
    }

    /// Consume a non-2xx response into a classified error.
    async fn reject(&self, response: reqwest::Response, model: &str) -> ProviderError {
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        self.classify_failure(status.as_u16(), body, &headers, model)
    }
}

// ── Provider trait ────────────────────────────────────────────────────

#[async_trait]
impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
    }

    async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
        // D8 trim (at most one retry) lives in `send_with_trim`; this method
        // only parses the winning response.
        let TrimmedSend { response, applied } = self
            .send_with_trim(&request, false)
            .await
            .map_err(acowork_core::AcoworkError::Provider)?;
        if !applied.is_none() {
            tracing::debug!(stripped = ?applied.stripped_fields(), "Responses chat parsed a trimmed response");
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
        request: ChatRequest,
    ) -> acowork_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        // The trim decision happens before the stream is handed over: a 400
        // listing `reasoning` is a plain HTTP error, so it is resolved (or
        // surfaced) here rather than mid-stream (D6: no in-band retry).
        let TrimmedSend { response, applied } = self
            .send_with_trim(&request, true)
            .await
            .map_err(acowork_core::AcoworkError::Provider)?;
        if !applied.is_none() {
            tracing::debug!(stripped = ?applied.stripped_fields(), "Responses stream opened on a trimmed request");
        }

        Ok(Self::sse_to_stream(response, self.stream_read_timeout))
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

    /// `Auto` and `Off` both omit the `reasoning` block: `Off` means "do not
    /// control reasoning". Sending `effort: "none"` would be wrong — MiniMax
    /// M3.1-Flash answers HTTP 400 for it, and the D8 trim would then drop
    /// the whole block (which is not what "off" asked for either).
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
        assert!(
            map_request(&off).unwrap().reasoning.is_none(),
            "Off omits the field (never sends `none`)"
        );
        let high = ChatRequest {
            reasoning_effort: Some(ReasoningEffort::High),
            ..base.clone()
        };
        assert_eq!(map_request(&high).unwrap().reasoning.unwrap().effort, "high");
    }

    /// The Responses shape accepts the full level range (minimal / xhigh
    /// included) — unlike the completions shape.
    #[test]
    fn reasoning_effort_mapping_covers_the_full_level_range() {
        let base = req(vec![ChatMessage::user("hi")]);
        for (effort, wire) in [
            (ReasoningEffort::Minimal, "minimal"),
            (ReasoningEffort::Low, "low"),
            (ReasoningEffort::Medium, "medium"),
            (ReasoningEffort::High, "high"),
            (ReasoningEffort::XHigh, "xhigh"),
            (ReasoningEffort::Max, "max"),
        ] {
            let req = ChatRequest {
                reasoning_effort: Some(effort),
                ..base.clone()
            };
            assert_eq!(map_request(&req).unwrap().reasoning.unwrap().effort, wire);
        }
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

// ── D6 streaming SSE (ADR-103 hard requirements 1–7) ──────────────

    fn sse_frame(payload: serde_json::Value) -> String {
        format!("data: {payload}\n\n")
    }

    /// Flatten a state-machine action into the events downstream would see.
    fn emitted(action: SseAction) -> Vec<StreamEvent> {
        match action {
            SseAction::Emit(events) | SseAction::Finish(events) => events,
            SseAction::Ignore => Vec::new(),
        }
    }

    fn feed(state: &mut SseState, payload: serde_json::Value) -> Vec<StreamEvent> {
        emitted(
            state
                .on_data(&payload.to_string())
                .expect("payload decodes"),
        )
    }

    fn content_of(events: &[StreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                StreamEvent::Content(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    async fn drain(stream: Box<dyn Stream<Item = StreamEvent> + Send>) -> Vec<StreamEvent> {
        // `Pin<Box<..>>` is `Unpin`, so `next()` is available without a
        // projection macro.
        let mut stream: Pin<Box<dyn Stream<Item = StreamEvent> + Send>> = Box::into_pin(stream);
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    /// Run a full `chat_stream` round-trip against a fake SSE server.
    async fn stream_against(body: String) -> Vec<StreamEvent> {
        use mockito::Server;
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/responses")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(body)
            .create_async()
            .await;
        let provider = provider(server.url().as_str());
        let stream = provider
            .chat_stream(chat_request_with(vec![ChatMessage::user("hi")]))
            .await
            .expect("stream must open");
        drain(stream).await
    }

    /// D6.1 — the terminal event alone ends the stream.
    #[tokio::test]
    async fn completed_event_ends_the_stream_without_a_done_marker() {
        let body = format!(
            "{}{}{}",
            sse_frame(serde_json::json!({"type":"response.output_text.delta","delta":"he"})),
            sse_frame(serde_json::json!({"type":"response.output_text.delta","delta":"llo"})),
            sse_frame(serde_json::json!({
                "type":"response.completed",
                "response":{"status":"completed","output":[],
                            "usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}
            })),
        );
        assert!(!body.contains("[DONE]"), "the fixture must not rely on [DONE]");

        let events = stream_against(body).await;
        assert_eq!(content_of(&events), "hello");
        match events.last().expect("non-empty stream") {
            StreamEvent::Finished(response) => {
                assert_eq!(response.finish_reason.as_deref(), Some("stop"));
                assert_eq!(response.usage.as_ref().expect("usage mapped").total_tokens, 15);
            }
            other => panic!("terminal event must yield Finished, got {other:?}"),
        }
        assert!(
            !events.iter().any(|e| matches!(e, StreamEvent::Error(_))),
            "a completed stream carries no error"
        );
    }

    /// D6.1 — EOF with no terminal event is a truncation, never a Finish.
    #[tokio::test]
    async fn eof_without_terminal_event_is_truncation_never_finish() {
        let events = stream_against(sse_frame(
            serde_json::json!({"type":"response.output_text.delta","delta":"partial"}),
        ))
        .await;

        assert_eq!(content_of(&events), "partial");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, StreamEvent::Finished(_))),
            "no terminal event means no Finished: {events:?}"
        );
        match events.last().expect("non-empty stream") {
            StreamEvent::Error(err) => {
                assert_eq!(err.error_type, ProviderErrorType::TruncatedStream);
                assert!(
                    !err.retryable,
                    "a truncated stream is never retried blindly (output already reached the loop)"
                );
                assert!(
                    err.message.contains("without a terminal event"),
                    "{}",
                    err.message
                );
            }
            other => panic!("expected a truncation Error, got {other:?}"),
        }
    }

    /// D6.1 — `[DONE]` is accepted but is not a terminator.
    #[tokio::test]
    async fn done_marker_alone_does_not_terminate_the_stream() {
        let body = format!(
            "{}data: [DONE]\n\n",
            sse_frame(serde_json::json!({"type":"response.output_text.delta","delta":"x"}))
        );
        let events = stream_against(body).await;
        assert_eq!(content_of(&events), "x");
        assert!(!events
            .iter()
            .any(|e| matches!(e, StreamEvent::Finished(_))));
        assert!(matches!(events.last(), Some(StreamEvent::Error(_))));
    }

    /// D6.2 — the event type is read from the JSON payload, not `event:`.
    #[tokio::test]
    async fn event_type_comes_from_the_payload_not_the_event_line() {
        let body = concat!(
            "event: response.output_text.done\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"live\"}\n\n",
            "event: response.failed\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
        )
        .to_string();

        let events = stream_against(body).await;
        assert_eq!(content_of(&events), "live");
        assert!(
            matches!(events.last(), Some(StreamEvent::Finished(_))),
            "the payload type wins over a misleading `event:` line: {events:?}"
        );
    }

    /// D6.3 — both reasoning aliases emit reasoning, neither is a fallback.
    #[test]
    fn both_reasoning_aliases_emit_reasoning_content() {
        let mut state = SseState::new();
        let native = feed(
            &mut state,
            serde_json::json!({"type":"response.reasoning_text.delta","delta":"native"}),
        );
        let summary = feed(
            &mut state,
            serde_json::json!({"type":"response.reasoning_summary_text.delta","delta":"summary"}),
        );

        match (native.as_slice(), summary.as_slice()) {
            (
                [StreamEvent::ReasoningContent(native_text)],
                [StreamEvent::ReasoningContent(summary_text)],
            ) => {
                assert_eq!(native_text, "native");
                assert_eq!(summary_text, "summary");
            }
            other => panic!("expected two ReasoningContent events, got {other:?}"),
        }
    }

    /// D6.4 — identity comes from `output_item.added`; chunks map by
    /// `output_index`, not by arrival order.
    #[test]
    fn tool_call_identity_comes_from_output_item_added() {
        let mut state = SseState::new();
        let first = feed(
            &mut state,
            serde_json::json!({"type":"response.output_item.added","output_index":0,
                "item":{"type":"function_call","call_id":"call_a","name":"read_file"}}),
        );
        let second = feed(
            &mut state,
            serde_json::json!({"type":"response.output_item.added","output_index":1,
                "item":{"type":"function_call","call_id":"call_b","name":"write_file"}}),
        );

        match (first.as_slice(), second.as_slice()) {
            ([StreamEvent::ToolCallStart(a)], [StreamEvent::ToolCallStart(b)]) => {
                assert_eq!((a.id.as_str(), a.function.name.as_str()), ("call_a", "read_file"));
                assert!(
                    a.function.arguments.is_empty(),
                    "arguments never arrive on the added item"
                );
                assert_eq!((b.id.as_str(), b.function.name.as_str()), ("call_b", "write_file"));
            }
            other => panic!("expected two ToolCallStart events, got {other:?}"),
        }

        // Chunks arrive out of order relative to the item order.
        let chunk_b = feed(
            &mut state,
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"{}"}),
        );
        let chunk_a = feed(
            &mut state,
            serde_json::json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{}"}),
        );
        assert!(matches!(
            chunk_b.as_slice(),
            [StreamEvent::ToolCallChunk { index: 1, .. }]
        ));
        assert!(matches!(
            chunk_a.as_slice(),
            [StreamEvent::ToolCallChunk { index: 0, .. }]
        ));

        // `output_item.done` restates the arguments — validation only.
        let done = feed(
            &mut state,
            serde_json::json!({"type":"response.output_item.done","output_index":0,
                "item":{"type":"function_call","call_id":"call_a","name":"read_file","arguments":"{}"}}),
        );
        assert!(
            done.is_empty(),
            "output_item.done must never re-emit: {done:?}"
        );

        let terminal = feed(
            &mut state,
            serde_json::json!({"type":"response.completed","response":{}}),
        );
        assert!(matches!(
            terminal.as_slice(),
            [StreamEvent::Finished(r)] if r.finish_reason.as_deref() == Some("tool_calls")
        ));
    }

    /// D6.5 — usage is authoritative only on the terminal event.
    #[test]
    fn usage_is_only_authoritative_on_the_terminal_event() {
        let mut state = SseState::new();
        feed(
            &mut state,
            serde_json::json!({"type":"response.output_text.delta","delta":"x",
                "usage":{"input_tokens":1,"output_tokens":2}}),
        );
        assert!(
            state.usage.is_none(),
            "per-event usage fragments are ignored"
        );

        let terminal = feed(
            &mut state,
            serde_json::json!({"type":"response.completed","response":{"usage":{
                "prompt_tokens":7,"completion_tokens":3,"total_tokens":10,
                "prompt_tokens_details":{"cached_tokens":4},
                "output_tokens_details":{"reasoning_tokens":2}}}}),
        );
        match terminal.as_slice() {
            [StreamEvent::Finished(response)] => {
                let usage = response.usage.as_ref().expect("usage mapped");
                assert_eq!(usage.prompt_tokens, 7);
                assert_eq!(usage.completion_tokens, 3);
                assert_eq!(usage.total_tokens, 10);
                assert_eq!(usage.cache_read_tokens, 4);
                assert_eq!(usage.reasoning_tokens, 2);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    /// D6.6 — `sequence_number` never reorders and never de-duplicates.
    #[test]
    fn sequence_number_is_ignored_for_ordering_and_dedup() {
        let mut state = SseState::new();
        let mut events = Vec::new();
        for (sequence_number, delta) in [(5u64, "a"), (3, "b"), (5, "c")] {
            events.extend(feed(
                &mut state,
                serde_json::json!({"type":"response.output_text.delta",
                    "sequence_number":sequence_number,"delta":delta}),
            ));
        }
        assert_eq!(
            content_of(&events),
            "abc",
            "arrival order is preserved and a repeated sequence_number is not a duplicate"
        );
    }

    /// D6.7 — validation-only / known-noise events never re-emit text.
    #[test]
    fn validation_only_and_noise_events_never_reemit_text() {
        let mut state = SseState::new();
        for payload in [
            serde_json::json!({"type":"response.output_text.done","text":"hello"}),
            serde_json::json!({"type":"response.content_part.done",
                "part":{"type":"output_text","text":"hello"}}),
            serde_json::json!({"type":"response.reasoning_text.done","text":"hm"}),
            serde_json::json!({"type":"response.output_item.done","item":{"type":"message"}}),
            serde_json::json!({"type":"response.created","response":{}}),
            serde_json::json!({"type":"response.file_search_call.searching"}),
        ] {
            assert!(
                feed(&mut state, payload.clone()).is_empty(),
                "event must be silent: {payload}"
            );
        }
    }

    /// D6.7 — unknown types are warned about once and never fail the stream.
    #[test]
    fn unknown_event_types_warn_once_and_never_fail() {
        let mut state = SseState::new();
        assert!(feed(&mut state, serde_json::json!({"type":"response.brand_new"})).is_empty());
        assert!(state.warned_unknown, "the first unknown type is warned about");
        assert!(
            feed(&mut state, serde_json::json!({"type":"response.another_new"})).is_empty(),
            "a second unknown type is still ignored, not an error"
        );
    }

    #[test]
    fn failed_event_is_a_terminal_error_not_a_finish() {
        let mut state = SseState::new();
        let action = state
            .on_data(
                &serde_json::json!({"type":"response.failed","response":{"error":
                    {"code":"server_error","message":"upstream exploded"}}})
                .to_string(),
            )
            .expect("payload decodes");

        match emitted(action).as_slice() {
            [StreamEvent::Error(err)] => {
                assert_eq!(err.error_type, ProviderErrorType::InBandFailure);
                assert_eq!(err.message, "server_error: upstream exploded");
                assert!(
                    !err.retryable,
                    "an in-band failure after deltas is never retried blindly"
                );
            }
            other => panic!("expected a single Error, got {other:?}"),
        }
        assert!(state.terminated);
    }

    #[test]
    fn incomplete_max_output_tokens_surfaces_as_visible_length() {
        let mut state = SseState::new();
        let events = feed(
            &mut state,
            serde_json::json!({"type":"response.incomplete","response":{
                "status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},
                "usage":{"input_tokens":3,"output_tokens":64,"total_tokens":67}}}),
        );
        match events.as_slice() {
            [StreamEvent::Finished(response)] => {
                assert_eq!(
                    response.finish_reason.as_deref(),
                    Some("length"),
                    "truncation must be visible, never a silently empty answer"
                );
                assert_eq!(response.usage.as_ref().expect("usage").total_tokens, 67);
            }
            other => panic!("expected Finished, got {other:?}"),
        }
    }

    #[test]
    fn incomplete_content_filter_is_visible_in_the_stream_finish() {
        let mut state = SseState::new();
        let events = feed(
            &mut state,
            serde_json::json!({"type":"response.incomplete","response":{
                "incomplete_details":{"reason":"content_filter"}}}),
        );
        assert!(matches!(
            events.as_slice(),
            [StreamEvent::Finished(r)] if r.finish_reason.as_deref() == Some("content_filter")
        ));
    }

    #[test]
    fn malformed_payload_is_a_decode_error() {
        let mut state = SseState::new();
        let err = state
            .on_data("{not json")
            .expect_err("a malformed payload must not be silently dropped");
        assert!(matches!(err.error_type, ProviderErrorType::StreamDecodeError));
    }

    /// The streaming request must carry `stream: true`.
    #[tokio::test]
    async fn chat_stream_requests_the_streaming_wire_form() {
        use mockito::Server;
        let mut server = Server::new_async().await;
        let mock = server
            .mock("POST", "/responses")
            .match_body(mockito::Matcher::Regex("\"stream\":true".to_string()))
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse_frame(
                serde_json::json!({"type":"response.completed","response":{}}),
            ))
            .expect(1)
            .create_async()
            .await;

        let provider = provider(server.url().as_str());
        let events = drain(
            provider
                .chat_stream(chat_request_with(vec![ChatMessage::user("hi")]))
                .await
                .expect("stream opens"),
        )
        .await;

        assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
        mock.assert();
    }

// ── D8 error classification (S4) ─────────────────────────────────

    async fn chat_error_against(
        status: usize,
        body: &'static str,
        retry_after: Option<&'static str>,
    ) -> acowork_core::ProviderError {
        use mockito::Server;
        let mut server = Server::new_async().await;
        let mut mock = server.mock("POST", "/responses").with_status(status);
        if let Some(value) = retry_after {
            mock = mock.with_header("retry-after", value);
        }
        let _m = mock.with_body(body).create_async().await;
        // ADR-103 D8/D9 — a failed Responses request MUST NOT be re-sent to
        // the completions endpoint. `expect(0)` makes any such retry a test
        // failure instead of a silent second semantic.
        let completions_guard = server
            .mock("POST", "/chat/completions")
            .expect(0)
            .create_async()
            .await;
        let provider = provider(server.url().as_str());
        let error = match provider
            .chat(chat_request_with(vec![ChatMessage::user("hi")]))
            .await
        {
            Err(acowork_core::AcoworkError::Provider(err)) => err,
            other => panic!("expected a provider error, got {other:?}"),
        };
        completions_guard.assert();
        error
    }

    #[tokio::test]
    async fn endpoint_unavailable_404_is_classified_and_actionable() {
        let err = chat_error_against(
            404,
            r#"{"error":{"message":"Invalid URL (POST /v1/responses)"}}"#,
            None,
        )
        .await;

        assert_eq!(err.error_type, ProviderErrorType::EndpointUnavailable);
        assert!(!err.retryable, "a wrong protocol must never be retried");
        assert_eq!(err.status_code, Some(404));
        assert!(
            err.user_message.contains("Chat Completions"),
            "text must name the way back: {}",
            err.user_message
        );
        assert!(
            err.message.contains("/v1/responses"),
            "the provider body stays in the details"
        );
    }

    #[tokio::test]
    async fn endpoint_unavailable_is_classified_on_the_streaming_path_too() {
        use mockito::Server;
        let mut server = Server::new_async().await;
        let _m = server
            .mock("POST", "/responses")
            .with_status(404)
            .with_body(r#"{"error":{"message":"unknown endpoint"}}"#)
            .create_async()
            .await;
        let provider = provider(server.url().as_str());

        let error = provider
            .chat_stream(chat_request_with(vec![ChatMessage::user("hi")]))
            .await
            .err()
            .expect("a 404 must fail before any stream opens");
        let acowork_core::AcoworkError::Provider(err) = error else {
            panic!("expected a provider error, got {error:?}");
        };
        assert_eq!(err.error_type, ProviderErrorType::EndpointUnavailable);
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn unsupported_field_400_keeps_the_field_name_for_the_trim_whitelist() {
        let err = chat_error_against(
            400,
            r#"{"error":{"message":"Unsupported parameter: 'reasoning'"}}"#,
            None,
        )
        .await;

        assert_eq!(err.error_type, ProviderErrorType::UnsupportedField);
        assert_eq!(
            err.unsupported_field(),
            Some("reasoning"),
            "S7 needs the field identity, not just the class"
        );
        assert!(!err.retryable, "the untrimmed request is never retried");
    }

    #[tokio::test]
    async fn content_integrity_400_stays_visible_and_is_never_trimmable() {
        let err = chat_error_against(400, "invalid image placement in input[1]", None).await;

        assert_eq!(err.error_type, ProviderErrorType::ContentIntegrity);
        assert_eq!(err.unsupported_field(), None);
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn retry_after_header_survives_the_responses_classification() {
        let err = chat_error_against(429, "slow down", Some("7")).await;
        assert_eq!(err.error_type, ProviderErrorType::RateLimited);
        assert_eq!(
            err.retry_after_ms,
            Some(7000),
            "S4 must not lose the platform-wide Retry-After handling"
        );
    }
}
/// ADR-103 D8/S7 — provider-level behaviour of the single-field trim.
///
/// These exercise the whole path (HTTP → classify → trim → cache), which is
/// the only place the "no silent behaviour change" rules can actually be
/// checked: the cache policy itself is covered in
/// [`super::responses_compat`].
#[cfg(test)]
mod d8_trim_tests {
    use super::*;
    use crate::providers::compat::COMPAT_CONFIRM_REQUIRED;
    use crate::providers::responses_compat::{ResponsesTrim, load, trim_key};
    use mockito::{Matcher, Server};
    use std::path::PathBuf;

    const PROVIDER: &str = "minimax";
    const MODEL: &str = "m3.1-flash";

    fn work_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "acowork_resp_trim_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }

    fn trimmed_provider(base: &str, work: &std::path::Path) -> ResponsesProvider {
        let mut provider = ResponsesProvider::with_base_url_and_timeouts(
            Some(base),
            Some("sk-test"),
            Duration::from_secs(10),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        provider.set_compat_wiring(PROVIDER.to_string(), load(work));
        provider
    }

    fn key() -> String {
        trim_key(PROVIDER, MODEL)
    }

    fn request(effort: ReasoningEffort) -> ChatRequest {
        ChatRequest {
            model: MODEL.to_string(),
            messages: vec![ChatMessage::user("hi")],
            temperature: None,
            max_tokens: Some(16),
            tools: None,
            reasoning_effort: Some(effort),
            thinking_mode: None,
        }
    }

    fn ok_body() -> String {
        serde_json::json!({
            "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}]
        })
        .to_string()
    }

    /// MiniMax M3.1-Flash wording: a 400 that blames the `reasoning` parameter.
    const REASONING_400: &str = r#"{"error":{"code":"invalid_parameter","message":"invalid parameter: `reasoning` for model m3.1-flash","param":"reasoning","type":"invalid_request_error"}}"#;

    /// The exact body the retry must carry: the original mapping minus
    /// `reasoning`. Comparing whole JSON bodies (not a substring) is what pins
    /// "only that one field is dropped".
    fn body_without_reasoning() -> serde_json::Value {
        let mut native = map_request(&request(ReasoningEffort::Low)).unwrap();
        native.reasoning = None;
        serde_json::to_value(&native).unwrap()
    }

    fn provider_error(err: acowork_core::AcoworkError) -> ProviderError {
        match err {
            acowork_core::AcoworkError::Provider(e) => e,
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn reasoning_400_retries_once_without_the_field() {
        let mut server = Server::new_async().await;
        let rejected = server
            .mock("POST", "/responses")
            .match_body(Matcher::Regex("\"reasoning\"".to_string()))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(REASONING_400)
            .expect(1)
            .create_async()
            .await;
        let accepted = server
            .mock("POST", "/responses")
            .match_body(Matcher::Json(body_without_reasoning()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ok_body())
            .expect(1)
            .create_async()
            .await;

        let work = work_dir("once");
        let provider = trimmed_provider(server.url().as_str(), &work);
        let response = provider
            .chat(request(ReasoningEffort::Low))
            .await
            .expect("the trimmed retry must succeed");

        assert_eq!(response.content, "ok");
        rejected.assert();
        accepted.assert();

        // One success is evidence, not policy (the 2026-09-05 rule).
        let cache = provider.compat_cache.as_ref().unwrap();
        assert!(
            cache.get(&key()).is_none(),
            "a single trim success must stay a candidate"
        );
    }

    #[tokio::test]
    async fn repeated_trims_promote_to_a_fast_path_that_sends_one_request() {
        let mut server = Server::new_async().await;
        let confirmations = COMPAT_CONFIRM_REQUIRED as usize;
        let rejected = server
            .mock("POST", "/responses")
            .match_body(Matcher::Regex("\"reasoning\"".to_string()))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(REASONING_400)
            .expect(confirmations)
            .create_async()
            .await;
        let accepted = server
            .mock("POST", "/responses")
            .match_body(Matcher::Json(body_without_reasoning()))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ok_body())
            .expect(confirmations + 1)
            .create_async()
            .await;

        let work = work_dir("promote");
        let provider = trimmed_provider(server.url().as_str(), &work);
        for _ in 0..confirmations {
            provider
                .chat(request(ReasoningEffort::Low))
                .await
                .expect("trimmed retry succeeds");
        }
        let cache = provider.compat_cache.as_ref().unwrap();
        assert_eq!(cache.get(&key()), Some(ResponsesTrim::omit_reasoning()));

        // The promoted trim must skip the rejected request entirely: one send.
        provider
            .chat(request(ReasoningEffort::Low))
            .await
            .expect("fast path succeeds");
        rejected.assert();
        accepted.assert();
    }

    #[tokio::test]
    async fn non_whitelisted_field_400_is_never_trimmed() {
        // `top_p` is a recognized field but NOT trimmable (D8: dropping it
        // silently changes model behaviour without an endpoint-side rejection
        // of the value we actually sent to justify it).
        let mut server = Server::new_async().await;
        let only = server
            .mock("POST", "/responses")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"unsupported parameter: top_p"}}"#)
            .expect(1)
            .create_async()
            .await;

        let work = work_dir("notrim");
        let provider = trimmed_provider(server.url().as_str(), &work);
        let err = provider_error(
            provider
                .chat(request(ReasoningEffort::Low))
                .await
                .expect_err("must surface, not trim"),
        );

        assert_eq!(err.error_type, ProviderErrorType::UnsupportedField);
        assert_eq!(err.unsupported_field(), Some("top_p"));
        only.assert();
        assert!(provider.compat_cache.as_ref().unwrap().get(&key()).is_none());
    }

    #[tokio::test]
    async fn temperature_400_retries_once_without_the_field() {
        // Reasoning models answer 400 for `temperature` outright ("not
        // supported with this model") while the Runtime's fallback chain
        // injects DEFAULT_TEMPERATURE. The trim path must drop exactly that
        // field and retry once (ADR-103 D8 whitelist, revised).
        let mut req = request(ReasoningEffort::Low);
        req.temperature = Some(0.7);
        let mut native = map_request(&req).unwrap();
        native.temperature = None;
        let body_without_temperature = serde_json::to_value(&native).unwrap();

        let mut server = Server::new_async().await;
        let rejected = server
            .mock("POST", "/responses")
            .match_body(Matcher::Regex("\"temperature\"".to_string()))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model.","type":"invalid_request_error","param":"temperature"}}"#)
            .expect(1)
            .create_async()
            .await;
        let accepted = server
            .mock("POST", "/responses")
            .match_body(Matcher::Json(body_without_temperature))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ok_body())
            .expect(1)
            .create_async()
            .await;

        let work = work_dir("temp_trim");
        let provider = trimmed_provider(server.url().as_str(), &work);
        let response = provider
            .chat(req)
            .await
            .expect("the temperature-trimmed retry must succeed");

        assert_eq!(response.content, "ok");
        rejected.assert();
        accepted.assert();

        // One success is evidence, not policy (the 2026-09-05 rule): the
        // temperature trim must stay a candidate, not a durable entry.
        let cache = provider.compat_cache.as_ref().unwrap();
        assert!(
            cache.get(&key()).is_none(),
            "a single trim success must stay a candidate"
        );
    }

    #[tokio::test]
    async fn repeated_temperature_trims_promote_to_a_fast_path() {
        // After enough confirmations the temperature trim hardens into the
        // durable fast path: subsequent chats send ONE request, already
        // without the field.
        fn temp_request() -> ChatRequest {
            let mut req = request(ReasoningEffort::Low);
            req.temperature = Some(0.7);
            req
        }
        let mut native = map_request(&temp_request()).unwrap();
        native.temperature = None;
        let accepted_body = serde_json::to_value(&native).unwrap();

        let mut server = Server::new_async().await;
        let confirmations = COMPAT_CONFIRM_REQUIRED as usize;
        let rejected = server
            .mock("POST", "/responses")
            .match_body(Matcher::Regex("\"temperature\"".to_string()))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"Unsupported parameter: 'temperature' is not supported with this model.","type":"invalid_request_error","param":"temperature"}}"#)
            .expect(confirmations)
            .create_async()
            .await;
        let accepted = server
            .mock("POST", "/responses")
            .match_body(Matcher::Json(accepted_body))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ok_body())
            .expect(confirmations + 1)
            .create_async()
            .await;

        let work = work_dir("temp_promote");
        let provider = trimmed_provider(server.url().as_str(), &work);
        for _ in 0..confirmations {
            provider
                .chat(temp_request())
                .await
                .expect("trimmed retry succeeds");
        }
        let cache = provider.compat_cache.as_ref().unwrap();
        assert_eq!(cache.get(&key()), Some(ResponsesTrim::omit_temperature()));

        // The promoted trim must skip the rejected request entirely: one send.
        provider
            .chat(temp_request())
            .await
            .expect("fast path succeeds");
        rejected.assert();
        accepted.assert();
    }

    #[tokio::test]
    async fn content_integrity_400_is_never_trimmed() {
        // DeepSeek's "reasoning_content must be passed back" is a content
        // defect; trimming would mask it (2026-09-05 regression).
        let mut server = Server::new_async().await;
        let only = server
            .mock("POST", "/responses")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(
                r#"{"error":{"message":"The reasoning_content in the thinking mode must be passed back to the API"}}"#,
            )
            .expect(1)
            .create_async()
            .await;

        let work = work_dir("content");
        let provider = trimmed_provider(server.url().as_str(), &work);
        let err = provider_error(
            provider
                .chat(request(ReasoningEffort::Low))
                .await
                .expect_err("content defects are not trim candidates"),
        );
        assert_eq!(err.error_type, ProviderErrorType::ContentIntegrity);
        only.assert();
        assert!(provider.compat_cache.as_ref().unwrap().get(&key()).is_none());
    }

    #[tokio::test]
    async fn a_failing_fast_path_invalidates_the_learned_trim() {
        let mut server = Server::new_async().await;
        // Even with the trim applied the provider rejects: the evidence is
        // stale, so the cache must be dropped (and no third request sent).
        let rejected = server
            .mock("POST", "/responses")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(REASONING_400)
            .expect(1)
            .create_async()
            .await;

        let work = work_dir("invalidate");
        let provider = trimmed_provider(server.url().as_str(), &work);
        let cache = provider.compat_cache.as_ref().unwrap().clone();
        for _ in 0..COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(
                key().as_str(),
                ResponsesTrim::omit_reasoning(),
                crate::providers::compat::ErrorClass::RequestSchema,
            );
        }
        assert!(cache.get(&key()).is_some(), "seeded durable trim");

        provider
            .chat(request(ReasoningEffort::Low))
            .await
            .expect_err("the fast path fails");
        rejected.assert();
        assert!(
            cache.get(&key()).is_none(),
            "a rejected learned trim must be invalidated, not silently kept"
        );
    }

    #[tokio::test]
    async fn a_request_without_a_reasoning_block_never_touches_the_cache() {
        // `Auto` omits the field, so there is nothing to trim and nothing to
        // learn: the cache keeps its evidence and only one request is sent.
        let mut server = Server::new_async().await;
        let native = map_request(&request(ReasoningEffort::Auto)).unwrap();
        assert!(native.reasoning.is_none(), "Auto omits the block (D5)");
        let body = serde_json::to_value(&native).unwrap();
        let accepted = server
            .mock("POST", "/responses")
            .match_body(Matcher::Json(body))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(ok_body())
            .expect(1)
            .create_async()
            .await;

        let work = work_dir("auto");
        let provider = trimmed_provider(server.url().as_str(), &work);
        let cache = provider.compat_cache.as_ref().unwrap().clone();
        for _ in 0..COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(
                key().as_str(),
                ResponsesTrim::omit_reasoning(),
                crate::providers::compat::ErrorClass::RequestSchema,
            );
        }

        provider
            .chat(request(ReasoningEffort::Auto))
            .await
            .expect("verbatim send");
        accepted.assert();
        assert!(
            cache.get(&key()).is_some(),
            "an Auto request carries no reasoning block, so it is not contradicting evidence"
        );
    }
}