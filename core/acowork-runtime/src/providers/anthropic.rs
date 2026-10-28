//! Anthropic Claude Provider
//!
//! Supports the Anthropic Messages API (claude-sonnet-4, claude-haiku, etc.)
//! with streaming, tool_use, and structured error handling.

use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::mpsc;

/// Per-chunk read timeout (45s) — used by backwards-compatible constructors.
const DEFAULT_STREAM_READ_TIMEOUT: Duration = Duration::from_secs(45);

use acowork_core::providers::traits::{
    parse_data_uri, ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall,
    MessageContent, MessageRole, Provider, StreamEvent, ToolCall, UsageInfo,
};

// ── Provider struct ──────────────────────────────────────────────────────

/// Anthropic Claude provider
pub struct AnthropicProvider {
    base_url: String,
    api_key: Option<String>,
    http_client: Client,
    stream_read_timeout: Duration,
}

impl AnthropicProvider {
    /// Create a new Anthropic provider with default base URL and default timeouts
    pub fn new(api_key: Option<&str>) -> Self {
        Self::with_base_url(None, api_key)
    }

    /// Create provider with custom base URL and default timeouts
    pub fn with_base_url(base_url: Option<&str>, api_key: Option<&str>) -> Self {
        Self::with_base_url_and_timeouts(
            base_url,
            api_key,
            Duration::from_secs(120),
            Duration::from_secs(10),
            DEFAULT_STREAM_READ_TIMEOUT,
        )
    }

    /// Create provider with fully configurable timeouts
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
                .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string()),
            api_key: api_key.map(ToString::to_string),
            http_client,
            stream_read_timeout,
        }
    }

    /// Set API key after construction (e.g., from Vault KeyRelease)
    pub fn set_api_key(&mut self, key: String) {
        self.api_key = Some(key);
    }
}

// ── Anthropic API types ──────────────────────────────────────────────────

/// Anthropic Messages API request
#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    messages: Vec<AnthropicMessage>,
    /// ADR-060: upgraded from `Option<String>` to a content-block array so
    /// the static kernel (Block A) can carry a `cache_control` breakpoint.
    /// The API accepts both forms; the array form is the documented one.
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<AnthropicSystemBlock>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicToolSpec>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    /// Extended / adaptive thinking configuration for reasoning-capable models.
    /// When thinking is enabled, temperature must be 1.0 (Anthropic API requirement).
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<AnthropicThinking>,
    /// Output configuration for adaptive thinking (effort level).
    /// Only used with `thinking: { type: "adaptive" }`.
    #[serde(skip_serializing_if = "Option::is_none")]
    output_config: Option<AnthropicOutputConfig>,
}

/// Anthropic cache control directive (prompt caching, ADR-060).
///
/// Maps to the `cache_control` field on a message or system content block:
/// `{ "type": "ephemeral", "ttl": "1h" }`. Applies to the Anthropic
/// ephemeral prompt cache breakpoints placed at Block A (system) and
/// Block C (todo snapshot).
///
/// ## TTL choice: 1h vs default 5m
///
/// Anthropic defaults `ephemeral` to a 5-minute TTL. In long-running
/// sessions with periodic compaction, a 5-minute window means the
/// Block A cache frequently invalidates between user turns (especially
/// when the user pauses to think or read), forcing re-writes that
/// double the `cache_creation_input_tokens` cost.
///
/// With `ttl: "1h"`, Block A stays warm across the entire realistic
/// shape of a working session. Trade-off: per-write token cost is ~2x
/// the 5m rate, but break-even is reached at round 2 of cache reuse,
/// which is the common case.
///
/// ponytail: ceiling — fixed 1h hard-coded. If we ever need per-tenant
/// TTLs (e.g. cost-sensitive enterprise users on 5m, power users on
/// 1h), promote `ttl` to a config field on `ChatRequest` and let the
/// session loop override.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct AnthropicCacheControl {
    #[serde(rename = "type")]
    cache_type: String,
    /// Anthropic cache TTL: "5m" (default) or "1h". Defaulted to "1h"
    /// here to keep Block A warm across realistic working sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<String>,
}

impl AnthropicCacheControl {
    /// Default ephemeral breakpoint: 1h TTL.
    fn ephemeral() -> Self {
        Self {
            cache_type: "ephemeral".to_string(),
            ttl: Some("1h".to_string()),
        }
    }
}

/// System content block (ADR-060): allows cache_control on the top-level
/// `system` field, which the plain-string form cannot express.
#[derive(Debug, Serialize)]
struct AnthropicSystemBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

impl AnthropicSystemBlock {
    fn text(text: String) -> Self {
        Self {
            block_type: "text".to_string(),
            text,
            cache_control: None,
        }
    }
}

/// Anthropic thinking configuration block.
///
/// Two modes are supported:
/// - **Extended** (older models): `{ type: "enabled", budget_tokens: N }`
/// - **Adaptive** (newer models, Opus 4.6+): `{ type: "adaptive" }` + `output_config.effort`
///
/// API constraints:
/// - When `thinking` is present, `temperature` must be 1.0 or omitted.
/// - `budget_tokens` must be >= 1024 and < `max_tokens` (extended mode only).
/// - Adaptive-only models (Opus 4.7+, Mythos 5, Fable 5) reject `enabled`/`disabled`.
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum AnthropicThinking {
    #[serde(rename = "enabled")]
    Enabled { budget_tokens: u32 },
    #[serde(rename = "disabled")]
    Disabled,
    #[serde(rename = "adaptive")]
    Adaptive,
}

/// Output configuration for Anthropic adaptive thinking.
/// Controls how much thinking the model does (effort level).
#[derive(Debug, Serialize)]
struct AnthropicOutputConfig {
    effort: String,
}

/// Map `ReasoningEffort` to Anthropic's `thinking` + `output_config` blocks.
///
/// Supports two modes controlled by `thinking_mode`:
/// - **"extended"** (default): `{ type: "enabled", budget_tokens: N }`
/// - **"adaptive"**: `{ type: "adaptive" }` + `output_config: { effort: "..." }`
///
/// Extended mode budget allocation:
/// - `None` → no thinking block (model uses its own default behavior)
/// - `Off` → disabled
/// - `Low` → 2048 tokens
/// - `Medium` → 8000 tokens
/// - `High` → min(16000, max_output/2)
/// - `Max` → min(31999, max_output - 1)
///
/// Adaptive mode effort mapping:
/// - `None` → no thinking block (model uses adaptive default)
/// - `Off` → disabled
/// - `Low` → "low", `Medium` → "medium", `High` → "high", `Max` → "max"
///
/// Returns `(thinking_config, output_config, adjusted_temperature)`.
/// When thinking is enabled, temperature is forced to `None` (Anthropic requires 1.0).
fn map_anthropic_thinking(
    effort: Option<&acowork_core::providers::ReasoningEffort>,
    thinking_mode: Option<&str>,
    max_output: u32,
    original_temperature: Option<f64>,
) -> (
    Option<AnthropicThinking>,
    Option<AnthropicOutputConfig>,
    Option<f64>,
) {
    use acowork_core::providers::ReasoningEffort;
    let is_adaptive = thinking_mode == Some("adaptive");

    // None: reasoning_effort not set at all — omit thinking block entirely
    // so the model uses its own built-in default behavior.
    // Auto: user chose to let the model decide — same behavior as None (omit block).
    // Off: user explicitly chose to disable thinking.
    let effort = match effort {
        None | Some(ReasoningEffort::Auto) => {
            // No override or Auto — let the model decide its default behavior.
            // For both extended and adaptive models, omitting thinking = model default.
            return (None, None, original_temperature);
        }
        Some(ReasoningEffort::Off) => {
            // Explicitly disable thinking (both extended and adaptive).
            return (
                Some(AnthropicThinking::Disabled),
                None,
                original_temperature,
            );
        }
        Some(e) => e,
    };

    if is_adaptive {
        // Adaptive thinking: map effort to output_config.effort string
        let effort_str = match effort {
            ReasoningEffort::Low => "low",
            ReasoningEffort::Medium => "medium",
            ReasoningEffort::High => "high",
            ReasoningEffort::Max => "max",
            _ => "medium",
        };
        return (
            Some(AnthropicThinking::Adaptive),
            Some(AnthropicOutputConfig {
                effort: effort_str.to_string(),
            }),
            None, // temperature must be omitted
        );
    }

    // Extended thinking: compute budget_tokens
    let budget = match effort {
        ReasoningEffort::Low => 2048,
        ReasoningEffort::Medium => 8000,
        ReasoningEffort::High => std::cmp::min(16000, max_output.saturating_div(2)),
        ReasoningEffort::Max => std::cmp::min(31999, max_output.saturating_sub(1)),
        _ => unreachable!(),
    };
    // Ensure budget_tokens >= 1024 (Anthropic minimum)
    let budget = std::cmp::max(budget, 1024);
    // Ensure budget_tokens < max_tokens (Anthropic requirement)
    let budget = std::cmp::min(budget, max_output.saturating_sub(1));

    // When thinking is enabled, temperature must be omitted (defaults to 1.0)
    (
        Some(AnthropicThinking::Enabled {
            budget_tokens: budget,
        }),
        None,
        None,
    )
}

/// Anthropic message format
#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    /// ADR-060: cache breakpoint hint (ephemeral). Not set on tool_result
    /// messages — Anthropic forbids cache_control on tool_result blocks.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<AnthropicCacheControl>,
}

/// Anthropic tool specification
#[derive(Debug, Serialize, Deserialize)]
struct AnthropicToolSpec {
    name: String,
    description: String,
    #[serde(rename = "input_schema")]
    input_schema: serde_json::Value,
}

/// Anthropic Messages API response
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
    #[serde(default)]
    usage: Option<AnthropicUsage>,
    #[serde(default)]
    stop_reason: Option<String>,
}

/// Anthropic content block
#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
    /// Extended-thinking text, carried by `type: "thinking"` blocks.
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

/// Anthropic usage information
#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
}

/// Anthropic error response
#[derive(Debug, Deserialize)]
struct AnthropicError {
    error: AnthropicErrorDetail,
}

#[derive(Debug, Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    error_type: String,
    message: String,
}

// ── Streaming SSE types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamEventRaw {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    content_block: Option<ContentBlockStart>,
    #[serde(default)]
    message: Option<AnthropicResponse>,
    /// Usage info included in message_delta event
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StreamDelta {
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
    /// Extended-thinking text, carried by `thinking_delta` events.
    #[serde(default)]
    thinking: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct ContentBlockStart {
    #[serde(rename = "type")]
    block_type: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<serde_json::Value>,
}

// ── Conversion helpers ──────────────────────────────────────────────────

/// Build Anthropic content array from a ChatMessage.
///
/// When `content_parts` is present, each ContentPart is mapped to the
/// corresponding Anthropic content block type:
/// - `ContentPart::Text` → `{"type": "text", "text": "..."}`
/// - `ContentPart::ImageUrl` → `{"type": "image", "source": {"type": "base64", ...}}`
///
/// When `content_parts` is None, falls back to a single text block from `content`.
fn build_anthropic_content(msg: &ChatMessage) -> serde_json::Value {
    let text_block = |text: &str| serde_json::json!({ "type": "text", "text": text });
    let mut blocks: Vec<serde_json::Value> = Vec::new();
    // `message_content` decides whether the message's own text joins the part
    // list; the previous "parts, or else content" branch dropped the text on
    // every image turn, because the derived parts carry images only.
    match msg.message_content() {
        MessageContent::Text(text) => blocks.push(text_block(text)),
        MessageContent::Parts { leading_text, parts } => {
            if let Some(text) = leading_text {
                blocks.push(text_block(text));
            }
            for p in parts {
                match p {
                    ContentPart::Text { text } => blocks.push(text_block(text)),
                    ContentPart::ImageUrl { image_url } => {
                        // Anthropic expects: {"type":"image","source":{"type":"base64","media_type":"image/png","data":"..."}}
                        // Parse the data URI to extract media_type and base64 data
                        let (media_type, data) = image_source_fields(&image_url.url);
                        blocks.push(serde_json::json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": data
                            }
                        }));
                    }
                }
            }
        }
    }
    serde_json::Value::Array(blocks)
}

/// Parse a data URI into (media_type, base64_data).
/// E.g. "data:image/png;base64,iVBOR..." → ("image/png", "iVBOR...")
/// Anthropic's `(media_type, base64_data)` for an image URL.
///
/// The parsing is the shared [`parse_data_uri`]; what is Anthropic-specific is
/// the tolerance: a payload that is already bare base64 is sent as `image/png`
/// rather than failing the turn.
fn image_source_fields(uri: &str) -> (&str, &str) {
    match parse_data_uri(uri) {
        Some(parts) => parts,
        None => ("image/png", uri),
    }
}

/// Convert our ChatMessage list to Anthropic format.
/// Anthropic requires system messages to be passed separately,
/// and tool messages to use `tool_result` content blocks.
///
/// ADR-060: multiple System messages are appended (never overwrite) into
/// the top-level `system` content-block array; `cache_control` hints are
/// mapped to message-level `cache_control` (user/assistant only).
fn convert_messages(
    messages: &[ChatMessage],
) -> (Vec<AnthropicMessage>, Option<Vec<AnthropicSystemBlock>>) {
    let mut system_blocks: Vec<AnthropicSystemBlock> = Vec::new();
    let mut converted = Vec::new();

    for msg in messages {
        match msg.role {
            MessageRole::System => {
                // Anthropic uses a top-level `system` field. Multiple system
                // messages are appended (Block A is the only system message
                // today, but append is the safe superset of the old
                // overwrite-with-last semantics).
                let mut block = AnthropicSystemBlock::text(msg.content.clone());
                if msg.cache_control
                    == Some(acowork_core::providers::traits::CacheControl::Ephemeral)
                {
                    block.cache_control = Some(AnthropicCacheControl::ephemeral());
                }
                system_blocks.push(block);
            }
            MessageRole::User => {
                // ADR-060: Block C (todo snapshot) is a User message carrying
                // the ephemeral cache breakpoint. Never set cache_control on
                // tool_result blocks (Anthropic API rejects it).
                let is_tool_result = msg.tool_call_id.is_some()
                    || (msg.tool_call_id.is_none() && msg.content.starts_with("{\"tool_call_id\""));
                let cache_control = if is_tool_result {
                    None
                } else if msg.cache_control
                    == Some(acowork_core::providers::traits::CacheControl::Ephemeral)
                {
                    Some(AnthropicCacheControl::ephemeral())
                } else {
                    None
                };
                converted.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(build_anthropic_content(msg)),
                    cache_control,
                });
            }
            MessageRole::Assistant => {
                // The signed thinking block is not regenerated here (see the
                // `ponytail:` note in `parse_anthropic_sse_line`). Surface the
                // drop in logs so the gap is observable the moment a native
                // Claude tool turn with thinking enabled actually hits it.
                if msg.reasoning_content.is_some() {
                    tracing::debug!(
                        "anthropic: assistant reasoning_content not replayed on this turn \
                         (signed thinking replay unimplemented; see parse_anthropic_sse_line)"
                    );
                }

                let mut content_blocks: Vec<serde_json::Value> = Vec::new();

                // Add text content if present
                if !msg.content.is_empty() {
                    content_blocks.push(serde_json::json!({
                        "type": "text",
                        "text": msg.content
                    }));
                }

                // Add tool_use blocks if present
                if let Some(ref tool_calls) = msg.tool_calls {
                    for tc in tool_calls {
                        let input: serde_json::Value = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Object(Default::default()));
                        content_blocks.push(serde_json::json!({
                            "type": "tool_use",
                            "id": tc.id,
                            "name": tc.function.name,
                            "input": input
                        }));
                    }
                }

                let cache_control = if msg.cache_control
                    == Some(acowork_core::providers::traits::CacheControl::Ephemeral)
                {
                    Some(AnthropicCacheControl::ephemeral())
                } else {
                    None
                };
                converted.push(AnthropicMessage {
                    role: "assistant".to_string(),
                    content: if content_blocks.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::Array(content_blocks))
                    },
                    cache_control,
                });
            }
            MessageRole::Tool => {
                // Anthropic tool results: user message with tool_result content block
                // Prefer dedicated tool_call_id field; fall back to content JSON for legacy
                let tool_call_id = msg.tool_call_id.clone().unwrap_or_else(|| {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                        value
                            .get("tool_call_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string()
                    } else {
                        "unknown".to_string()
                    }
                });
                let result_content = if msg.tool_call_id.is_some() {
                    // New format: content is the actual result
                    msg.content.clone()
                } else if let Ok(value) = serde_json::from_str::<serde_json::Value>(&msg.content) {
                    // Legacy format: content JSON contains tool_call_id and content
                    value
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&msg.content)
                        .to_string()
                } else {
                    msg.content.clone()
                };

                converted.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: Some(serde_json::Value::Array(vec![serde_json::json!({
                        "type": "tool_result",
                        "tool_use_id": tool_call_id,
                        "content": result_content
                    })])),
                    cache_control: None,
                });
            }
        }
    }

    (
        converted,
        if system_blocks.is_empty() {
            None
        } else {
            Some(system_blocks)
        },
    )
}

/// Convert tools from our format to Anthropic format
fn convert_tools(tools: Option<&[serde_json::Value]>) -> Option<Vec<AnthropicToolSpec>> {
    tools.map(|items| {
        items
            .iter()
            .map(|tool| {
                let name = tool["name"].as_str().unwrap_or("unknown").to_string();
                tracing::debug!(
                    tool = %name,
                    has_parameters = tool.get("parameters").is_some(),
                    has_input_schema = tool.get("input_schema").is_some(),
                    tool_keys = ?tool.as_object().map(|o| o.keys().collect::<Vec<_>>()),
                    "Anthropic convert_tools field check"
                );
                AnthropicToolSpec {
                    name,
                    description: tool
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: tool
                        .get("parameters")
                        .cloned()
                        .unwrap_or(serde_json::json!({"type": "object", "properties": {}})),
                }
            })
            .collect()
    })
}

/// Parse Anthropic response into our ChatResponse
fn parse_response(resp: AnthropicResponse) -> ChatResponse {
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in resp.content {
        match block.block_type.as_str() {
            "text" => {
                if let Some(text) = block.text {
                    text_parts.push(text);
                }
            }
            // Extended-thinking block — surface as reasoning_content so the
            // loop can show a `thought` turn instead of silently dropping it.
            // Newer Claude models return the reasoning as an opaque `signature`
            // and leave `thinking` empty or whitespace-only; such blocks must
            // not become `Some("")`/`Some(" ")`, which would either short-circuit
            // the think-tag fallback in `persist_think_to_conversation` or
            // persist a blank thought record the UI has to filter.
            "thinking" => {
                if let Some(thinking) = block.thinking
                    && !thinking.trim().is_empty()
                {
                    reasoning_parts.push(thinking);
                }
            }
            "tool_use" => {
                tool_calls.push(ToolCall {
                    id: block.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: block.name.unwrap_or_default(),
                        arguments: block
                            .input
                            .map(|v| serde_json::to_string(&v).unwrap_or_default())
                            .unwrap_or_default(),
                    },
                });
            }
            _ => {}
        }
    }

    let usage_info = resp.usage.map(|u| {
        let input = u.input_tokens.unwrap_or(0);
        let output = u.output_tokens.unwrap_or(0);
        let cache_creation = u.cache_creation_input_tokens.unwrap_or(0);
        let cache_read = u.cache_read_input_tokens.unwrap_or(0);
        UsageInfo {
            prompt_tokens: input + cache_creation + cache_read,
            completion_tokens: output,
            total_tokens: input + cache_creation + cache_read + output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_creation,
            reasoning_tokens: 0, // Anthropic reasoning is in thinking blocks, not a separate field
        }
    });

    ChatResponse {
        content: text_parts.join(""),
        reasoning_content: if reasoning_parts.is_empty() {
            None
        } else {
            Some(reasoning_parts.join(""))
        },
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: usage_info,
        // Anthropic names the same condition differently; normalise to the
        // OpenAI-style vocabulary that `ChatResponse::finish_reason` carries.
        // Without this, a caller checking for `"length"` would never see an
        // Anthropic truncation, which the API reports as `"max_tokens"`.
        finish_reason: resp.stop_reason.as_deref().map(normalize_stop_reason),
        ..Default::default()
    }
}

/// Map an Anthropic `stop_reason` onto the OpenAI-style vocabulary used
/// internally (`"stop"` / `"length"` / `"tool_calls"`).
fn normalize_stop_reason(reason: &str) -> String {
    match reason {
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        other => other.to_string(),
    }
}

// ── Provider trait implementation ───────────────────────────────────────

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
        let (messages, system) = convert_messages(&request.messages);

        // Anthropic API requires at least one non-system message.
        // When context trimming removes all user/assistant messages,
        // only the system prompt remains (extracted to the `system` field),
        // leaving `messages` empty — which the API rejects with 400.
        // Insert a minimal user message as a safety net.
        let messages = if messages.is_empty() {
            tracing::warn!(
                model = %request.model,
                "Anthropic: no non-system messages after conversion, inserting placeholder"
            );
            vec![AnthropicMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String(
                    "[Context too large — all conversation history was trimmed. Please start a new conversation or reduce attached files.]".to_string()
                )),
                cache_control: None,
            }]
        } else {
            messages
        };

        let effective_max_tokens = request.max_tokens.unwrap_or(4096);
        let (thinking, output_config, temperature) = map_anthropic_thinking(
            request.reasoning_effort.as_ref(),
            request.thinking_mode.as_deref(),
            effective_max_tokens,
            request.temperature,
        );

        let anthropic_request = AnthropicRequest {
            model: request.model,
            messages,
            system,
            max_tokens: Some(effective_max_tokens),
            temperature,
            tools: convert_tools(request.tools.as_deref()),
            stream: None,
            thinking,
            output_config,
        };

        // Log request payload for debugging tool definitions
        tracing::debug!(
            request_len = serde_json::to_string(&anthropic_request).map(|s| s.len()).unwrap_or(0),
            model = %anthropic_request.model,
            has_tools = anthropic_request.tools.is_some(),
            "Anthropic chat request"
        );

        let url = format!("{}/messages", self.base_url);

        let api_key = self.api_key.as_ref().ok_or_else(|| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::unauthorized(
                "Anthropic API key is required".to_string(),
            ))
        })?;

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&anthropic_request)
            .send()
            .await
            .map_err(|e| {
                acowork_core::AcoworkError::Provider(acowork_core::ProviderError::network(format!(
                    "Anthropic request failed: {e}"
                )))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let headers = response.headers().clone();
            let body = response.text().await.unwrap_or_default();

            // Try to parse Anthropic error structure for better classification
            let message = if let Ok(err_resp) = serde_json::from_str::<AnthropicError>(&body) {
                format!(
                    "Anthropic API error [{}]: {}",
                    err_resp.error.error_type, err_resp.error.message
                )
            } else {
                format!("Anthropic API error: {status} — {body}")
            };

            let err = crate::providers::from_http_parts(status.as_u16(), message, &headers);
            return Err(acowork_core::AcoworkError::Provider(err));
        }

        let native_resp: AnthropicResponse = response.json().await.map_err(|e| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::unknown(format!(
                "Failed to parse Anthropic response: {e}"
            )))
        })?;

        Ok(parse_response(native_resp))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> acowork_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        let (messages, system) = convert_messages(&request.messages);

        // Anthropic API requires at least one non-system message.
        // When context trimming removes all user/assistant messages,
        // only the system prompt remains (extracted to the `system` field),
        // leaving `messages` empty — which the API rejects with 400.
        // Insert a minimal user message as a safety net.
        let messages = if messages.is_empty() {
            tracing::warn!(
                model = %request.model,
                "Anthropic: no non-system messages after conversion, inserting placeholder"
            );
            vec![AnthropicMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String(
                    "[Context too large — all conversation history was trimmed. Please start a new conversation or reduce attached files.]".to_string()
                )),
                cache_control: None,
            }]
        } else {
            messages
        };

        let effective_max_tokens = request.max_tokens.unwrap_or(4096);
        let (thinking, output_config, temperature) = map_anthropic_thinking(
            request.reasoning_effort.as_ref(),
            request.thinking_mode.as_deref(),
            effective_max_tokens,
            request.temperature,
        );

        let anthropic_request = AnthropicRequest {
            model: request.model,
            messages,
            system,
            max_tokens: Some(effective_max_tokens),
            temperature,
            tools: convert_tools(request.tools.as_deref()),
            stream: Some(true),
            thinking,
            output_config,
        };

        // Log request payload for debugging tool definitions
        tracing::debug!(
            request_len = serde_json::to_string(&anthropic_request).map(|s| s.len()).unwrap_or(0),
            model = %anthropic_request.model,
            has_tools = anthropic_request.tools.is_some(),
            "Anthropic chat_stream request"
        );

        let url = format!("{}/messages", self.base_url);

        let api_key = self.api_key.as_ref().ok_or_else(|| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::unauthorized(
                "Anthropic API key is required".to_string(),
            ))
        })?;

        let response = self
            .http_client
            .post(&url)
            .header("x-api-key", api_key.as_str())
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&anthropic_request)
            .send()
            .await
            .map_err(|e| {
                acowork_core::AcoworkError::Provider(acowork_core::ProviderError::network(format!(
                    "Anthropic streaming request failed: {e}"
                )))
            })?;

        if !response.status().is_success() {
            let err = crate::providers::from_http_response(response).await;
            return Err(acowork_core::AcoworkError::Provider(err));
        }

        let (tx, rx) = mpsc::channel(32);
        let stream_read_timeout = self.stream_read_timeout;
        tokio::spawn(async move {
            let mut stream = response.bytes_stream();
            let mut buffer = String::new();
            let mut pending_tool_id: Option<String> = None;
            let mut pending_tool_name: Option<String> = None;
            let mut accumulated_input_tokens: u64 = 0;
            let mut accumulated_cache_read_tokens: u64 = 0;
            let mut accumulated_cache_write_tokens: u64 = 0;
            let mut block_index_map: HashMap<u64, u64> = HashMap::new();

            use futures_util::StreamExt;
            loop {
                match tokio::time::timeout(stream_read_timeout, stream.next()).await {
                    Ok(Some(Ok(bytes))) => {
                        buffer.push_str(&String::from_utf8_lossy(&bytes));

                        while let Some(newline_pos) = buffer.find('\n') {
                            let line = buffer[..newline_pos].to_string();
                            buffer = buffer[newline_pos + 1..].to_string();

                            if let Some(event) = parse_anthropic_sse_line(
                                &line,
                                &mut pending_tool_id,
                                &mut pending_tool_name,
                                &mut accumulated_input_tokens,
                                &mut accumulated_cache_read_tokens,
                                &mut accumulated_cache_write_tokens,
                                &mut block_index_map,
                            ) && tx.send(Some(event)).await.is_err()
                            {
                                return; // receiver dropped
                            }
                        }
                    }
                    Ok(Some(Err(e))) => {
                        let stream_err = acowork_core::providers::classify_stream_error(&format!(
                            "Stream error: {e}"
                        ));
                        let _ = tx.send(Some(StreamEvent::Error(stream_err))).await;
                        return;
                    }
                    Ok(None) => {
                        // Stream ended normally
                        break;
                    }
                    Err(_) => {
                        // Stream silence — no data received within read timeout
                        tracing::warn!(
                            timeout_secs = stream_read_timeout.as_secs(),
                            "Stream silence detected, no data received within timeout"
                        );
                        let _ = tx
                            .send(Some(StreamEvent::Error(
                                acowork_core::providers::StreamError::stream_timeout(
                                    stream_read_timeout.as_secs(),
                                ),
                            )))
                            .await;
                        return;
                    }
                }
            }
            let _ = tx.send(None).await;
        });

        Ok(Box::new(ChannelStream { rx }))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> acowork_core::error::Result<u64> {
        // Heuristic estimation for Anthropic models:
        // Claude models use a similar tokenization to GPT models.
        // Approximate: English ~4 chars/token, CJK ~2 chars/token
        let mut total_chars: usize = 0;
        let mut cjk_chars: usize = 0;

        for msg in messages {
            total_chars += msg.content.len();
            cjk_chars += msg.content.chars().filter(|c| !c.is_ascii()).count();
        }

        let ascii_chars = total_chars - cjk_chars;
        let estimated_tokens =
            (ascii_chars as f64 / 4.0).ceil() as u64 + (cjk_chars as f64 / 2.0).ceil() as u64;

        Ok(estimated_tokens)
    }
}

// ── Streaming helpers ────────────────────────────────────────────────────

/// Channel-based stream for SSE events
struct ChannelStream {
    rx: mpsc::Receiver<Option<StreamEvent>>,
}

impl Stream for ChannelStream {
    type Item = StreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.rx.poll_recv(cx) {
            Poll::Ready(Some(Some(event))) => Poll::Ready(Some(event)),
            Poll::Ready(Some(None)) => Poll::Ready(None),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Merge a `usage` block into the running prompt accounting.
///
/// Anthropic-compatible providers disagree on *which* stream event carries
/// prompt accounting: native Anthropic reports `input_tokens` plus the cache
/// tokens on `message_start` and only output tokens on `message_delta`, while
/// MiniMax-M3 sends an all-zero block on `message_start` and the real counts on
/// `message_delta`. Accounting therefore has to be event-agnostic: every usage
/// block is merged with the same rule, so no shape loses tokens.
///
/// The largest non-zero report wins. A zero report can never mask a value
/// another event already reported, a later report that is *smaller* (a provider
/// sending an increment on `message_delta` after a full count on
/// `message_start`) cannot shrink the total, and the cache counters always come
/// from the same report as the total so the two stay consistent.
///
/// Rationale and the SDK evidence for these wire shapes: ADR-027, section
/// "Usage merge across stream events".
fn merge_prompt_usage(
    usage: &AnthropicUsage,
    input_tokens: &mut u64,
    cache_read_tokens: &mut u64,
    cache_write_tokens: &mut u64,
) {
    let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
    let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);
    // Cache tokens are part of the prompt — they only appear as separate
    // fields so callers can tell cached from fresh input apart.
    let input = usage.input_tokens.unwrap_or(0) + cache_read + cache_write;
    if input > *input_tokens {
        *input_tokens = input;
        *cache_read_tokens = cache_read;
        *cache_write_tokens = cache_write;
    }
}

/// Parse a single Anthropic SSE line into a StreamEvent.
///
/// `block_index_map` maps Anthropic's global content_block index to a
/// tool-call-only sequential index (0, 1, 2…). This is needed because
/// Anthropic assigns index to *all* content blocks (text + tool_use),
/// while the downstream loop expects indices that align with the tool_calls
/// array (i.e. only tool_use blocks counted). Without this mapping, if a
/// text block appears before tool_use blocks, the indices would be off-by-one
/// or more, causing argument chunks to be routed to wrong buffers.
fn parse_anthropic_sse_line(
    line: &str,
    pending_tool_id: &mut Option<String>,
    pending_tool_name: &mut Option<String>,
    accumulated_input_tokens: &mut u64,
    accumulated_cache_read_tokens: &mut u64,
    accumulated_cache_write_tokens: &mut u64,
    block_index_map: &mut HashMap<u64, u64>,
) -> Option<StreamEvent> {
    let line = line.trim();
    if line.is_empty() || line == ":" {
        return None;
    }

    // Anthropic SSE format: "event: message_start" / "data: {...}"
    // We only process "data:" lines
    let data = line.strip_prefix("data: ")?;

    if data.is_empty() {
        return None;
    }

    let event: StreamEventRaw = serde_json::from_str(data).ok()?;

    match event.event_type.as_str() {
        "content_block_start" => {
            if let Some(block) = event.content_block
                && block.block_type.as_deref() == Some("tool_use")
            {
                let id = block.id.unwrap_or_default();
                let name = block.name.unwrap_or_default();
                *pending_tool_id = Some(id.clone());
                *pending_tool_name = Some(name.clone());
                // Map Anthropic's global content_block index to sequential tool-call index
                let tool_index = block_index_map.len() as u64;
                if let Some(content_block_index) = event.index {
                    block_index_map.insert(content_block_index, tool_index);
                }
                return Some(StreamEvent::ToolCallStart(ToolCall {
                    id,
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name,
                        arguments: String::new(),
                    },
                }));
            }
            None
        }
        "content_block_delta" => {
            if let Some(delta) = event.delta {
                // Extended-thinking delta. Carries the model's reasoning as
                // plain text (`{"type":"thinking_delta","thinking":"..."}`).
                // Forward it as ReasoningContent so the agent loop streams it
                // into the `thought` role (ADR-022) instead of dropping it —
                // this is what Anthropic-compatible providers (e.g. MiniMax)
                // send in place of the OpenAI `reasoning_content` field.
                //
                // ponytail: thinking is displayed but NOT replayed to the
                // endpoint on later turns — the signed block is never rebuilt in
                // `convert_messages`. Replay is only required by native Anthropic
                // *extended thinking + tool use*, i.e. only when the user
                // explicitly sets a reasoning effort (Auto/None sends no thinking
                // config at all), so it is unreachable on the default path and
                // entirely unvalidated on compat endpoints (MiniMax etc.).
                // It is deliberately not implemented here: the exact replay
                // contract (block position, signature validation) is unverified
                // (Anthropic docs are region-blocked), and a mis-shaped block
                // turns a working tool loop into a 400 — strictly worse than the
                // status quo. Upgrade path, gated on one probe: run a native
                // Claude tool call with thinking on; if it 400s on a missing
                // thinking block, capture `signature_delta` here and rebuild
                // `{type:"thinking",thinking,signature}` in `convert_messages`
                // before the `tool_use` block, native endpoints only.
                if let Some(thinking) = delta.thinking
                    && !thinking.is_empty()
                {
                    return Some(StreamEvent::ReasoningContent(thinking));
                }
                if let Some(text) = delta.text
                    && !text.is_empty()
                {
                    return Some(StreamEvent::Content(text));
                }
                if let Some(partial_json) = delta.partial_json
                    && !partial_json.is_empty()
                {
                    // Route argument chunks to the correct tool_call buffer by content block index.
                    // Anthropic assigns each content block a unique global index (text blocks
                    // also occupy indices), but loop_.rs expects sequential tool-call indices
                    // (0, 1, 2…). We translate via block_index_map so downstream stays aligned.
                    let content_block_index = event.index.unwrap_or(0);
                    let tool_index = block_index_map
                        .get(&content_block_index)
                        .copied()
                        .unwrap_or(0);
                    return Some(StreamEvent::ToolCallChunk {
                        index: tool_index,
                        arguments: partial_json,
                    });
                }
            }
            None
        }
        "message_stop" => {
            // Stream complete — will be handled by channel close (None)
            None
        }
        "message_delta" => {
            // Carries stop_reason, output tokens, and — on some
            // Anthropic-compatible providers — the prompt accounting that
            // native Anthropic puts on `message_start`.
            //
            // DIAG (2026-10-07): surface the server-reported stop reason on
            // the Finished response. It used to be dropped here, so every
            // streamed `finish_reason` stayed None and "model chose to end"
            // was indistinguishable from "server truncated the response".
            // Two MiniMax-M3.1-Flash-Preview runs ended mid-task exactly
            // this way (0 chars while billing 128 output tokens; another
            // cut at 43 chars mid-sentence) and looked identical in logs.
            let stop_reason = event
                .delta
                .as_ref()
                .and_then(|d| d.stop_reason.as_deref())
                .map(normalize_stop_reason);
            if let Some(usage) = event.usage {
                merge_prompt_usage(
                    &usage,
                    accumulated_input_tokens,
                    accumulated_cache_read_tokens,
                    accumulated_cache_write_tokens,
                );
                let input = *accumulated_input_tokens;
                let cache_read = *accumulated_cache_read_tokens;
                let cache_write = *accumulated_cache_write_tokens;
                let output = usage.output_tokens.unwrap_or(0);
                if input == 0 {
                    // No event of this response reported prompt tokens: ADR-027's
                    // zero-skip will hold the session's `total_input` unchanged.
                    tracing::warn!(
                        ?usage,
                        output_tokens = output,
                        "anthropic stream: provider reported no prompt tokens"
                    );
                }
                return Some(StreamEvent::Finished(ChatResponse {
                    content: String::new(),
                    tool_calls: None,
                    usage: Some(acowork_core::providers::traits::UsageInfo {
                        prompt_tokens: input,
                        completion_tokens: output,
                        total_tokens: input + output,
                        cache_read_tokens: cache_read,
                        cache_write_tokens: cache_write,
                        reasoning_tokens: 0,
                    }),
                    finish_reason: stop_reason,
                    ..Default::default()
                }));
            }
            // A stream that never reports a usage block leaves
            // `ChatResponse.usage == None`; the caller then falls back to its
            // local estimate instead of booking provider-reported numbers.
            tracing::warn!("anthropic stream: message_delta carried no usage block");
            None
        }
        "message_start" => {
            // Native Anthropic reports prompt accounting here, cache tokens
            // included. `merge_prompt_usage` keeps whichever event actually
            // carries the numbers, so a provider that zeroes this block out and
            // reports on `message_delta` instead still books its input.
            if let Some(usage) = event.message.as_ref().and_then(|msg| msg.usage.as_ref()) {
                merge_prompt_usage(
                    usage,
                    accumulated_input_tokens,
                    accumulated_cache_read_tokens,
                    accumulated_cache_write_tokens,
                );
            }
            None
        }
        "content_block_stop" | "ping" => {
            // No action needed for these event types
            None
        }
        "error" => Some(StreamEvent::Error(
            acowork_core::providers::classify_stream_error(&format!(
                "Anthropic stream error: {data}"
            )),
        )),
        _ => None,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression (chat image + text): the turn reached the model as the image
    /// only. Since ADR-046 the image parts are derived server-side, so
    /// `content_parts` carries no text block — and serialising the parts alone
    /// dropped the user's own words.
    #[test]
    fn multimodal_content_keeps_the_text_beside_derived_images() {
        use acowork_core::providers::traits::ImageUrlPart;
        let msg = ChatMessage::user_multimodal(
            "the delete button style is wrong",
            vec![ContentPart::ImageUrl {
                image_url: ImageUrlPart {
                    url: "data:image/png;base64,iVBORw0KGgo=".into(),
                    detail: None,
                    width: None,
                    height: None,
                },
            }],
        );
        let content = build_anthropic_content(&msg);
        let blocks = content.as_array().unwrap_or_else(|| panic!("not an array: {content}"));
        assert!(
            blocks
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "the delete button style is wrong"),
            "user text missing from {blocks:?}"
        );
        assert!(blocks.iter().any(|b| b["type"] == "image"));
    }

    #[test]
    fn test_provider_creation() {
        let provider = AnthropicProvider::new(Some("sk-ant-test"));
        assert_eq!(provider.name(), "anthropic");
        assert_eq!(provider.base_url, "https://api.anthropic.com/v1");

        let custom = AnthropicProvider::with_base_url(
            Some("https://custom-api.example.com"),
            Some("sk-ant-test"),
        );
        assert_eq!(custom.base_url, "https://custom-api.example.com");
    }

    #[test]
    fn test_set_api_key() {
        let mut provider = AnthropicProvider::new(None);
        assert!(provider.api_key.is_none());
        provider.set_api_key("sk-ant-new".to_string());
        assert_eq!(provider.api_key.as_deref(), Some("sk-ant-new"));
    }

    #[test]
    fn test_convert_messages_basic() {
        let messages = vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ];

        let (converted, system) = convert_messages(&messages);
        // ADR-060: system is now a content-block array (may carry cache_control).
        let blocks = system.expect("system blocks present");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].text, "You are helpful.");
        assert_eq!(blocks[0].cache_control, None);
        assert_eq!(converted.len(), 2); // system excluded from messages
        assert_eq!(converted[0].role, "user");
        assert_eq!(converted[1].role, "assistant");
    }

    #[test]
    fn test_convert_messages_multiple_system_append() {
        // ADR-060: multiple System messages must be appended, never overwrite.
        let messages = vec![
            ChatMessage::system("Kernel A"),
            ChatMessage::system("Kernel B"),
        ];
        let (converted, system) = convert_messages(&messages);
        assert!(converted.is_empty());
        let blocks = system.expect("system blocks present");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].text, "Kernel A");
        assert_eq!(blocks[1].text, "Kernel B");
    }

    #[test]
    fn test_convert_messages_cache_control_mapping() {
        use acowork_core::providers::traits::CacheControl;

        // Block A: system kernel with ephemeral breakpoint.
        let mut sys = ChatMessage::system("Static kernel");
        sys.cache_control = Some(CacheControl::Ephemeral);
        // Block C: todo snapshot (User role) with ephemeral breakpoint.
        let mut todo = ChatMessage::user("## Todo Task List\n- task1");
        todo.cache_control = Some(CacheControl::Ephemeral);
        // Block B tail: assistant message with ephemeral breakpoint.
        let mut asst = ChatMessage::assistant("Previous turn");
        asst.cache_control = Some(CacheControl::Ephemeral);
        // Plain user message: no breakpoint.
        let plain = ChatMessage::user("Plain hello");
        // Tool result: must never carry cache_control (Anthropic rejects it).
        let tool = ChatMessage {
            role: MessageRole::Tool,
            content: "result ok".to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: Some("toolu_1".to_string()),
            tool_calls: None,
            cache_control: Some(CacheControl::Ephemeral),
        };

        let (converted, system) = convert_messages(&[sys, todo, asst, plain, tool]);
        let blocks = system.expect("system blocks present");
        assert_eq!(
            blocks[0].cache_control.as_ref().unwrap().cache_type,
            "ephemeral"
        );

        assert_eq!(converted.len(), 4); // todo, asst, plain, tool
        assert_eq!(converted[0].role, "user");
        assert!(converted[0].cache_control.is_some()); // Block C breakpoint
        assert!(converted[1].cache_control.is_some()); // Block B tail breakpoint
        assert!(converted[2].cache_control.is_none()); // plain user
        assert!(converted[3].cache_control.is_none()); // tool_result must not carry it
        assert_eq!(
            converted[3].content.as_ref().unwrap()[0]["type"],
            "tool_result"
        );
    }

    #[test]
    fn test_convert_messages_cache_control_on_tool_result_suppressed() {
        use acowork_core::providers::traits::CacheControl;

        // Legacy tool_result shape (content JSON with tool_call_id, no
        // dedicated field) must also suppress cache_control.
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: r#"{"tool_call_id":"toolu_123","content":"Sunny, 25°C"}"#.to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            cache_control: Some(CacheControl::Ephemeral),
        }];

        let (converted, _system) = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        let msg = &converted[0];
        assert_eq!(msg.role, "user"); // Anthropic requires tool_result in user role
        assert!(msg.cache_control.is_none());
        let content = msg.content.as_ref().unwrap();
        let blocks = content.as_array().unwrap();
        assert!(blocks[0]["type"] == "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_123");
    }

    #[test]
    fn test_convert_messages_tool_iteration_block_c_d_layout() {
        use acowork_core::providers::traits::CacheControl;

        // ADR-060 §5.4: the real tool-loop wire shape after a
        // `todo_write` call — [assistant(tool_use), user(tool_result),
        // user(todo snapshot)]. Consecutive user messages are
        // accepted by Anthropic (auto-merged); the tool_result message
        // must NEVER carry cache_control, while the snapshot keeps its
        // ephemeral breakpoint.
        let asst = ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "toolu_01".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "todo_write".to_string(),
                    arguments: "{}".to_string(),
                },
            }],
        );
        let mut todo = ChatMessage::user("## Todo Task List\n- [ ] t1 (t1)");
        todo.cache_control = Some(CacheControl::Ephemeral);

        let (converted, _system) =
            convert_messages(&[asst, ChatMessage::tool("toolu_01", "todos saved"), todo]);
        assert_eq!(converted.len(), 3);
        assert_eq!(converted[0].role, "assistant");
        assert_eq!(converted[1].role, "user");
        assert_eq!(converted[2].role, "user");
        // tool_result: cache_control suppressed, content is a tool_result block.
        assert!(converted[1].cache_control.is_none());
        assert_eq!(
            converted[1].content.as_ref().unwrap()[0]["type"],
            "tool_result"
        );
        assert_eq!(
            converted[1].content.as_ref().unwrap()[0]["tool_use_id"],
            "toolu_01"
        );
        // Block C (todo snapshot) keeps its ephemeral breakpoint.
        assert!(converted[2].cache_control.is_some());
        assert_eq!(
            converted[2].cache_control.as_ref().unwrap().cache_type,
            "ephemeral"
        );
        // The todo text survives as a plain text block.
        assert_eq!(converted[2].content.as_ref().unwrap()[0]["type"], "text");
        assert!(
            converted[2].content.as_ref().unwrap()[0]["text"]
                .as_str()
                .unwrap()
                .contains("Todo Task List")
        );
    }

    #[test]
    fn test_convert_messages_with_tool_calls() {
        let messages = vec![ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "toolu_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "get_weather".to_string(),
                    arguments: r#"{"city":"Shanghai"}"#.to_string(),
                },
            }],
        )];

        let (converted, _system) = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        let msg = &converted[0];
        let content = msg.content.as_ref().unwrap();
        let blocks = content.as_array().unwrap();
        // Should have one tool_use block
        assert!(blocks.iter().any(|b| b["type"] == "tool_use"));
    }

    #[test]
    fn test_convert_messages_tool_result() {
        let messages = vec![ChatMessage {
            role: MessageRole::Tool,
            content: r#"{"tool_call_id":"toolu_123","content":"Sunny, 25°C"}"#.to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            cache_control: None,
        }];

        let (converted, _system) = convert_messages(&messages);
        assert_eq!(converted.len(), 1);
        let msg = &converted[0];
        assert_eq!(msg.role, "user"); // Anthropic requires tool_result in user role
        let content = msg.content.as_ref().unwrap();
        let blocks = content.as_array().unwrap();
        assert!(blocks[0]["type"] == "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_123");
    }

    #[test]
    fn test_convert_tools() {
        let tools = vec![serde_json::json!({
            "name": "calculator",
            "description": "Performs calculations",
            "parameters": {
                "type": "object",
                "properties": {
                    "expression": {"type": "string"}
                }
            }
        })];

        let converted = convert_tools(Some(&tools));
        assert!(converted.is_some());
        let tools = converted.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "calculator");
        assert_eq!(tools[0].description, "Performs calculations");
    }

    #[test]
    fn test_convert_tools_none() {
        let converted = convert_tools(None);
        assert!(converted.is_none());
    }

    #[test]
    fn test_parse_response_text_only() {
        let resp = AnthropicResponse {
            content: vec![AnthropicContentBlock {
                block_type: "text".to_string(),
                text: Some("Hello! How can I help?".to_string()),
                thinking: None,
                id: None,
                name: None,
                input: None,
            }],
            usage: Some(AnthropicUsage {
                input_tokens: Some(10),
                output_tokens: Some(8),
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
            }),
            stop_reason: Some("end_turn".to_string()),
        };

        let chat_resp = parse_response(resp);
        assert_eq!(chat_resp.content, "Hello! How can I help?");
        assert!(chat_resp.tool_calls.is_none());
        assert_eq!(chat_resp.usage.as_ref().unwrap().prompt_tokens, 10);
        assert_eq!(chat_resp.usage.as_ref().unwrap().completion_tokens, 8);
        assert_eq!(chat_resp.usage.as_ref().unwrap().total_tokens, 18);
    }

    #[test]
    fn test_parse_response_with_tool_use() {
        let resp = AnthropicResponse {
            content: vec![
                AnthropicContentBlock {
                    block_type: "text".to_string(),
                    text: Some("Let me check that.".to_string()),
                    thinking: None,
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicContentBlock {
                    block_type: "tool_use".to_string(),
                    text: None,
                    thinking: None,
                    id: Some("toolu_abc".to_string()),
                    name: Some("weather".to_string()),
                    input: Some(serde_json::json!({"city": "Shanghai"})),
                },
            ],
            usage: Some(AnthropicUsage {
                input_tokens: Some(20),
                output_tokens: Some(15),
                cache_creation_input_tokens: Some(5),
                cache_read_input_tokens: None,
            }),
            stop_reason: Some("tool_use".to_string()),
        };

        let chat_resp = parse_response(resp);
        assert_eq!(chat_resp.content, "Let me check that.");
        assert!(chat_resp.tool_calls.is_some());
        let tool_calls = chat_resp.tool_calls.unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "toolu_abc");
        assert_eq!(tool_calls[0].function.name, "weather");
        assert!(tool_calls[0].function.arguments.contains("Shanghai"));

        // Cache creation tokens should be included in prompt_tokens
        assert_eq!(chat_resp.usage.as_ref().unwrap().prompt_tokens, 25); // 20 + 5
        assert_eq!(chat_resp.usage.as_ref().unwrap().completion_tokens, 15);
    }

    #[test]
    fn test_parse_sse_content_delta() {
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();
        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(event.is_some());
        if let Some(StreamEvent::Content(text)) = event {
            assert_eq!(text, "Hello");
        } else {
            panic!("Expected Content event");
        }
    }

    #[test]
    fn test_parse_sse_thinking_delta() {
        // MiniMax-on-Anthropic sends extended thinking as `thinking_delta`
        // events, not the OpenAI `reasoning_content` field. The parser must
        // surface it as ReasoningContent so the loop shows a `thought` turn.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Let me think"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        let is_reasoning =
            matches!(event, Some(StreamEvent::ReasoningContent(ref t)) if t == "Let me think");
        assert!(is_reasoning, "thinking_delta must map to ReasoningContent, got {event:?}");

        // An empty thinking delta must not emit a spurious event.
        let empty = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(empty.is_none());
    }

    #[test]
    fn test_parse_sse_thinking_text_tool_index_isolation() {
        // thinking (block 0) + text (block 1) + tool_use (block 2) in one turn.
        // Reasoning and text deltas must NOT perturb the tool-call index map:
        // the single tool must still be routed to index 0, not 2.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        // Thinking split across two chunks.
        let t1 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step1 "}}"#,
            &mut tool_id, &mut tool_name, &mut input_tokens,
            &mut cache_read_tokens, &mut cache_write_tokens, &mut block_index_map,
        );
        let t2 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"step2"}}"#,
            &mut tool_id, &mut tool_name, &mut input_tokens,
            &mut cache_read_tokens, &mut cache_write_tokens, &mut block_index_map,
        );
        assert!(matches!(t1, Some(StreamEvent::ReasoningContent(ref t)) if t == "step1 "));
        assert!(matches!(t2, Some(StreamEvent::ReasoningContent(ref t)) if t == "step2"));

        // Text delta in a different content block.
        let txt = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Answer"}}"#,
            &mut tool_id, &mut tool_name, &mut input_tokens,
            &mut cache_read_tokens, &mut cache_write_tokens, &mut block_index_map,
        );
        assert!(matches!(txt, Some(StreamEvent::Content(ref t)) if t == "Answer"));

        // tool_use start at content_block index 2 -> registers as tool index 0.
        let start = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}"#,
            &mut tool_id, &mut tool_name, &mut input_tokens,
            &mut cache_read_tokens, &mut cache_write_tokens, &mut block_index_map,
        );
        assert!(matches!(start, Some(StreamEvent::ToolCallStart(ref tc)) if tc.id == "toolu_1"));

        // Its argument delta must carry tool index 0 despite content_block 2.
        let arg = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"a\":1}"}}"#,
            &mut tool_id, &mut tool_name, &mut input_tokens,
            &mut cache_read_tokens, &mut cache_write_tokens, &mut block_index_map,
        );
        let ok = matches!(arg, Some(StreamEvent::ToolCallChunk { index: 0, ref arguments }) if arguments == "{\"a\":1}");
        assert!(ok, "reasoning/text blocks must not shift tool index; got {arg:?}");
    }

    #[test]
    fn test_parse_response_extracts_thinking() {
        // Extended-thinking block alongside text must surface as
        // `reasoning_content`, not be dropped by the `_ => {}` arm.
        let resp = AnthropicResponse {
            content: vec![
                AnthropicContentBlock {
                    block_type: "thinking".to_string(),
                    text: None,
                    thinking: Some("weighing options".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicContentBlock {
                    block_type: "text".to_string(),
                    text: Some("Here is the answer.".to_string()),
                    thinking: None,
                    id: None,
                    name: None,
                    input: None,
                },
            ],
            usage: None,
            stop_reason: Some("end_turn".to_string()),
        };

        let chat_resp = parse_response(resp);
        assert_eq!(chat_resp.content, "Here is the answer.");
        assert_eq!(
            chat_resp.reasoning_content.as_deref(),
            Some("weighing options")
        );
    }

    #[test]
    fn test_parse_response_drops_blank_thinking_blocks() {
        // Newer Claude models return reasoning as an opaque `signature` and
        // leave `thinking` empty/whitespace. Such blocks must not surface as
        // `Some("")`/`Some(" ")` — that persists a blank thought record the UI
        // renders as an empty ThinkBlock, and short-circuits the think-tag
        // fallback in `persist_think_to_conversation`.
        let resp = AnthropicResponse {
            content: vec![
                AnthropicContentBlock {
                    block_type: "thinking".to_string(),
                    text: None,
                    thinking: Some(String::new()),
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicContentBlock {
                    block_type: "thinking".to_string(),
                    text: None,
                    thinking: Some("  \n ".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicContentBlock {
                    block_type: "text".to_string(),
                    text: Some("answer".to_string()),
                    thinking: None,
                    id: None,
                    name: None,
                    input: None,
                },
            ],
            usage: None,
            stop_reason: Some("end_turn".to_string()),
        };
        assert_eq!(parse_response(resp).reasoning_content, None);

        // Mixed real + blank blocks keep only the real content.
        let mixed = AnthropicResponse {
            content: vec![
                AnthropicContentBlock {
                    block_type: "thinking".to_string(),
                    text: None,
                    thinking: Some("  ".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
                AnthropicContentBlock {
                    block_type: "thinking".to_string(),
                    text: None,
                    thinking: Some("real reasoning".to_string()),
                    id: None,
                    name: None,
                    input: None,
                },
            ],
            usage: None,
            stop_reason: Some("end_turn".to_string()),
        };
        assert_eq!(
            parse_response(mixed).reasoning_content.as_deref(),
            Some("real reasoning")
        );
    }

    #[test]
    fn test_parse_sse_prompt_usage_reported_on_message_delta() {
        // MiniMax-M3 shape: `message_start` carries an all-zero usage block and
        // the real prompt counts only appear on `message_delta`. Reading prompt
        // accounting from `message_start` alone yields `prompt_tokens = 0`, and
        // ADR-027's zero-skip then freezes the session's `total_input`.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        let started = parse_anthropic_sse_line(
            r#"data: {"type":"message_start","message":{"id":"msg_1","content":[],"usage":{"input_tokens":0,"output_tokens":0}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(started.is_none());
        assert_eq!(input_tokens, 0, "all-zero message_start reports nothing");

        let finished = parse_anthropic_sse_line(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":628,"output_tokens":524,"cache_read_input_tokens":32020}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        let Some(StreamEvent::Finished(resp)) = finished else {
            panic!("Expected Finished event");
        };
        let usage = resp.usage.expect("usage must be reported");
        // Cached tokens are part of the prompt: 628 fresh + 32020 cached.
        assert_eq!(usage.prompt_tokens, 32648);
        assert_eq!(usage.cache_read_tokens, 32020);
        assert_eq!(usage.completion_tokens, 524);
        assert_eq!(usage.total_tokens, 32648 + 524);
        // The delta's stop_reason must survive into finish_reason — it was
        // previously dropped, leaving streamed responses indistinguishable
        // from truncated ones in post-mortem logs.
        assert_eq!(resp.finish_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn test_parse_sse_prompt_usage_from_message_start_survives_delta() {
        // Native Anthropic shape: `message_start` reports `input_tokens` plus
        // cache tokens, `message_delta` reports only `output_tokens`. The delta
        // must not wipe the prompt accounting, and a smaller increment-style
        // report on the delta must not shrink it either.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        let _ = parse_anthropic_sse_line(
            r#"data: {"type":"message_start","message":{"id":"msg_1","content":[],"usage":{"input_tokens":100,"output_tokens":1,"cache_creation_input_tokens":5,"cache_read_input_tokens":10}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert_eq!(input_tokens, 115);

        let finished = parse_anthropic_sse_line(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":7,"output_tokens":42}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        let Some(StreamEvent::Finished(resp)) = finished else {
            panic!("Expected Finished event");
        };
        let usage = resp.usage.expect("usage must be reported");
        assert_eq!(usage.prompt_tokens, 115, "smaller delta report must not shrink it");
        assert_eq!(usage.cache_read_tokens, 10);
        assert_eq!(usage.cache_write_tokens, 5);
        assert_eq!(usage.completion_tokens, 42);
    }

    #[test]
    fn test_parse_sse_message_delta_normalizes_stop_reason() {
        // The stop_reason carried by `message_delta` must reach
        // `ChatResponse.finish_reason` in the OpenAI vocabulary
        // (`tool_calls` / `length`, everything else passthrough), so
        // downstream logic — budget-exhaustion nudges, truncation
        // diagnostics — can rely on it.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        for (stop_reason, expected) in [
            ("tool_use", "tool_calls"),
            ("max_tokens", "length"),
            ("end_turn", "end_turn"),
        ] {
            let line = format!(
                r#"data: {{"type":"message_delta","delta":{{"stop_reason":"{stop_reason}"}},"usage":{{"input_tokens":10,"output_tokens":2}}}}"#
            );
            let event = parse_anthropic_sse_line(
                &line,
                &mut tool_id,
                &mut tool_name,
                &mut input_tokens,
                &mut cache_read_tokens,
                &mut cache_write_tokens,
                &mut block_index_map,
            );
            let Some(StreamEvent::Finished(resp)) = event else {
                panic!("expected a Finished event for stop_reason={stop_reason}");
            };
            assert_eq!(
                resp.finish_reason.as_deref(),
                Some(expected),
                "stop_reason={stop_reason} must normalize to {expected}"
            );
        }
    }

    #[test]
    fn test_parse_sse_tool_use_start() {
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();
        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_xyz","name":"calculator","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(event.is_some());
        if let Some(StreamEvent::ToolCallStart(tc)) = event {
            assert_eq!(tc.id, "toolu_xyz");
            assert_eq!(tc.function.name, "calculator");
        } else {
            panic!("Expected ToolCallStart event");
        }
        // content_block index 1 should map to tool-call index 0
        assert_eq!(block_index_map.get(&1), Some(&0u64));
    }

    #[test]
    fn test_parse_sse_tool_input_delta() {
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();
        // First register the tool_use at content_block index 1 -> tool-call index 0
        let _ = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_xyz","name":"calculator","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        // Now send a delta for content_block index 1 — should be mapped to tool-call index 0
        let event = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"expr\":"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(event.is_some());
        if let Some(StreamEvent::ToolCallChunk {
            index,
            arguments: chunk,
        }) = event
        {
            assert_eq!(
                index, 0,
                "index should be mapped to tool-call sequential index 0, not content_block index 1"
            );
            assert!(chunk.contains("expr"));
        } else {
            panic!("Expected ToolCallChunk event");
        }
    }

    #[test]
    fn test_parse_sse_multi_tool_call_index_routing() {
        // Simulate Anthropic streaming where a text block precedes tool_use blocks.
        // content_block index 0 = text (skipped), index 1 = first tool, index 2 = second tool.
        // The block_index_map should translate: 1->0, 2->1 so downstream gets
        // sequential tool-call indices (0, 1) matching the tool_calls array.
        let mut tool_id = None;
        let mut tool_name = None;
        let mut input_tokens = 0u64;
        let mut cache_read_tokens = 0u64;
        let mut cache_write_tokens = 0u64;
        let mut block_index_map: HashMap<u64, u64> = HashMap::new();

        // content_block_start for first tool (content_block index 1, e.g. after text block at index 0)
        let e1 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_001","name":"shell","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(
            matches!(e1, Some(StreamEvent::ToolCallStart(ref tc)) if tc.id == "toolu_001" && tc.function.name == "shell")
        );

        // content_block_start for second tool (content_block index 2)
        let e2 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_002","name":"read_file","input":{}}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        assert!(
            matches!(e2, Some(StreamEvent::ToolCallStart(ref tc)) if tc.id == "toolu_002" && tc.function.name == "read_file")
        );

        // Delta for first tool (content_block index 1) — should map to tool-call index 0
        let e3 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        if let Some(StreamEvent::ToolCallChunk { index, arguments }) = e3 {
            assert_eq!(
                index, 0,
                "first tool delta must have sequential tool-call index 0 (content_block 1 -> tool 0)"
            );
            assert!(arguments.contains("command"));
        } else {
            panic!("Expected ToolCallChunk for index 0");
        }

        // Delta for second tool (content_block index 2) — should map to tool-call index 1
        let e4 = parse_anthropic_sse_line(
            r#"data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
            &mut tool_id,
            &mut tool_name,
            &mut input_tokens,
            &mut cache_read_tokens,
            &mut cache_write_tokens,
            &mut block_index_map,
        );
        if let Some(StreamEvent::ToolCallChunk { index, arguments }) = e4 {
            assert_eq!(
                index, 1,
                "second tool delta must have sequential tool-call index 1 (content_block 2 -> tool 1)"
            );
            assert!(arguments.contains("path"));
        } else {
            panic!("Expected ToolCallChunk for index 1");
        }
    }

    #[test]
    fn test_token_count_estimation() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let provider = AnthropicProvider::new(Some("test-key"));

        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "Hello world".to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            cache_control: None,
        }];

        let count = rt.block_on(provider.chat_token_count(&messages)).unwrap();
        assert!(count > 0);
        // "Hello world" = 11 chars / 4 ≈ 3 tokens
        assert!((2..=5).contains(&count));
    }

    #[test]
    fn test_token_count_cjk() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let provider = AnthropicProvider::new(Some("test-key"));

        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "你好世界".to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            cache_control: None,
        }];

        let count = rt.block_on(provider.chat_token_count(&messages)).unwrap();
        // 4 CJK chars ≈ 4/2 = 2 tokens
        assert!(count >= 2);
    }

    /// ADR-060 + concurrency-cost fix: ephemeral breakpoints must carry
    /// `ttl: "1h"` so Block A stays warm across realistic working
    /// sessions (vs Anthropic's 5-minute default that invalidates
    /// between user turns).
    #[test]
    fn test_ephemeral_cache_control_emits_ttl_1h() {
        let cc = AnthropicCacheControl::ephemeral();
        let json = serde_json::to_value(&cc).unwrap();
        assert_eq!(json["type"], "ephemeral");
        assert_eq!(
            json["ttl"], "1h",
            "Anthropic ephemeral cache must use 1h TTL to keep \
             Block A warm across long-running concurrent sessions"
        );
    }

    /// Sanity: a TTL-less AnthropicCacheControl (e.g. a deserialized
    /// legacy payload from before the TTL change) must still serialize
    /// without the `ttl` field, not as `"ttl": null`.
    #[test]
    fn test_ephemeral_cache_control_omits_ttl_when_none() {
        let cc = AnthropicCacheControl {
            cache_type: "ephemeral".to_string(),
            ttl: None,
        };
        let json = serde_json::to_value(&cc).unwrap();
        assert!(json.get("ttl").is_none(), "ttl=None must skip serialization");
    }
}
