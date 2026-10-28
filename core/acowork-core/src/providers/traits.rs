//! Provider trait and chat message types

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Classification of LLM provider error conditions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ProviderErrorType {
    /// HTTP 429 — rate limited by the provider (retryable after backoff).
    RateLimited,
    /// HTTP 402 — payment required / insufficient quota (NOT retryable).
    PaymentRequired,
    /// HTTP 401/403 — authentication or authorization failure.
    Unauthorized,
    /// HTTP 5xx — transient server-side failure.
    ServerError,
    /// HTTP 4xx (other) — client-side request error.
    ClientError,
    /// Network-level failure (timeout, DNS, connection reset, etc.).
    NetworkError,
    /// Context window / token limit exceeded.
    /// Not directly retryable — requires history trimming before retry.
    ContextOverflow,
    /// Stream decode error (mid-stream data corruption, h2 frame error, etc.).
    /// Retryable — re-issuing the same request may succeed.
    StreamDecodeError,
    /// Stream silence — no data received within per-chunk read timeout.
    /// Retryable — the provider may have been temporarily overloaded.
    StreamTimeout,
    /// The endpoint does not exist for this provider/model (ADR-103 D8).
    /// Responses-shaped example: `POST /responses` answers 404 naming
    /// `responses`/unknown endpoint. NOT retryable: re-issuing the same
    /// request cannot succeed, the protocol choice itself is wrong.
    EndpointUnavailable,
    /// The provider rejects a request field it does not implement
    /// (ADR-103 D8). Carries the field name as a machine-readable prefix in
    /// [`ProviderError::message`], see [`ProviderError::unsupported_field`].
    /// NOT retryable as-is; only a trim-then-retry may succeed (ADR-103 S7).
    UnsupportedField,
    /// The request is internally inconsistent (image placement, empty
    /// `input`, illegal tool name, over-window). NOT retryable and MUST NOT
    /// trigger field trimming — the caller has to change the content
    /// (ADR-103 D8).
    ContentIntegrity,
    /// The server reported a terminal failure inside the stream
    /// (`response.failed`). Partial output may already have been delivered,
    /// so this is never retried blindly (ADR-103 D8).
    InBandFailure,
    /// The stream ended without the protocol's terminal event. Partial
    /// output may already have reached the caller — never retried blindly
    /// (ADR-103 D8).
    TruncatedStream,
    /// Unclassified error.
    Unknown,
}

/// Structured error type for LLM provider failures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    /// Raw error message (includes API response body, status code, etc.).
    /// Shown in the frontend "Details" expandable section.
    pub message: String,
    /// User-friendly error summary, generated from `error_type`.
    /// Shown by default in the frontend. Empty string means "use `message`".
    #[serde(default)]
    pub user_message: String,
    /// HTTP status code, if the error originated from an HTTP response.
    pub status_code: Option<u16>,
    /// Classified error type.
    pub error_type: ProviderErrorType,
    /// Whether the request may succeed if retried.
    pub retryable: bool,
    /// Server-suggested wait time in milliseconds (from HTTP Retry-After header).
    /// When present, the retry loop should prefer this over the backoff strategy.
    #[serde(default)]
    pub retry_after_ms: Option<u64>,
}

impl ProviderError {
    /// Generate a user-friendly error summary from `error_type` and metadata.
    ///
    /// This is the single source of truth for user-facing error text.
    /// The raw API response body is kept separately in `message` for the
    /// frontend "Details" expandable section.
    pub fn compute_user_message(error_type: &ProviderErrorType, retry_after_ms: Option<u64>) -> String {
        match error_type {
            ProviderErrorType::RateLimited => {
                if let Some(ms) = retry_after_ms {
                    format!("Rate limited. Retry in ~{}s.", ms / 1000)
                } else {
                    "Rate limited. Please retry shortly.".to_string()
                }
            }
            ProviderErrorType::PaymentRequired => {
                "Insufficient balance. Please top up or switch provider.".to_string()
            }
            ProviderErrorType::Unauthorized => {
                "Invalid API key. Check provider settings.".to_string()
            }
            ProviderErrorType::ServerError => {
                "Server error. Please retry.".to_string()
            }
            ProviderErrorType::NetworkError => {
                "Network error. Check connection.".to_string()
            }
            ProviderErrorType::ContextOverflow => {
                "Context too long. History compressed.".to_string()
            }
            ProviderErrorType::StreamDecodeError => {
                "Stream error. Retrying.".to_string()
            }
            ProviderErrorType::StreamTimeout => {
                "Response timeout.".to_string()
            }
            ProviderErrorType::EndpointUnavailable => {
                "This provider does not support the selected protocol. Switch the model                  protocol back to Chat Completions in model settings."
                    .to_string()
            }
            ProviderErrorType::UnsupportedField => {
                "This provider rejects a request field the model settings enable.                  Adjust the model settings."
                    .to_string()
            }
            ProviderErrorType::ContentIntegrity => {
                "The request content was rejected by the provider. Check the message and                  attachments."
                    .to_string()
            }
            ProviderErrorType::InBandFailure => {
                "The model reported a failure while answering.".to_string()
            }
            ProviderErrorType::TruncatedStream => {
                "The answer was cut off before it finished.".to_string()
            }
            ProviderErrorType::ClientError => {
                "Invalid request. Check model/tool config.".to_string()
            }
            ProviderErrorType::Unknown => {
                "Unexpected error. See details.".to_string()
            }
        }
    }

    /// Build a `ProviderError` from an HTTP status code and message.
    /// Automatically infers `error_type`, `retryable`, and `user_message`.
    pub fn from_status_code(status: u16, message: String) -> Self {
        let (error_type, retryable) = match status {
            402 => (ProviderErrorType::PaymentRequired, false),
            429 => (ProviderErrorType::RateLimited, true),
            401 | 403 => (ProviderErrorType::Unauthorized, false),
            500..=599 => (ProviderErrorType::ServerError, true),
            400..=499 => (ProviderErrorType::ClientError, false),
            _ => (ProviderErrorType::Unknown, false),
        };
        Self {
            user_message: Self::compute_user_message(&error_type, None),
            message,
            status_code: Some(status),
            error_type,
            retryable,
            retry_after_ms: None,
        }
    }

    /// Machine-readable prefix used to carry the offending field name inside
    /// [`ProviderError::message`] (ADR-103 D8 `UnsupportedField{field}`).
    ///
    /// `ProviderError` has a deliberately narrow shape shared by every
    /// provider, so the field travels in the message behind this marker
    /// instead of widening the struct. [`ProviderError::unsupported_field`]
    /// is the only allowed reader.
    pub const UNSUPPORTED_FIELD_MARKER: &'static str = "unsupported field `";

    /// Build the `UnsupportedField` classification for a provider that
    /// rejected a specific request field (ADR-103 D8).
    ///
    /// `field` is the wire name the provider complained about (`reasoning`,
    /// `tool_choice`, `text.format`, `temperature`, `top_p`), `detail` is the
    /// provider's own wording, kept for the details panel.
    pub fn unsupported_field_error(field: &str, detail: String) -> Self {
        Self {
            message: format!("{}{}`, {}", Self::UNSUPPORTED_FIELD_MARKER, field, detail),
            user_message: Self::compute_user_message(&ProviderErrorType::UnsupportedField, None),
            status_code: Some(400),
            error_type: ProviderErrorType::UnsupportedField,
            // A retry with the field trimmed may succeed; the untrimmed
            // request never will (ADR-103 D8/S7).
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// The field name this error rejected, if it was classified as
    /// `UnsupportedField` (ADR-103 D8, consumed by the S7 trim whitelist).
    pub fn unsupported_field(&self) -> Option<&str> {
        if self.error_type != ProviderErrorType::UnsupportedField {
            return None;
        }
        let rest = self.message.strip_prefix(Self::UNSUPPORTED_FIELD_MARKER)?;
        rest.split('`').next().filter(|field| !field.is_empty())
    }

    /// Convenience constructor for network-level errors.
    pub fn network(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::NetworkError, None),
            message,
            status_code: None,
            error_type: ProviderErrorType::NetworkError,
            retryable: true,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for unclassified errors.
    pub fn unknown(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::Unknown, None),
            message,
            status_code: None,
            error_type: ProviderErrorType::Unknown,
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for authentication/authorization errors.
    pub fn unauthorized(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::Unauthorized, None),
            message,
            status_code: Some(401),
            error_type: ProviderErrorType::Unauthorized,
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for rate-limited errors.
    pub fn rate_limited(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::RateLimited, None),
            message,
            status_code: Some(429),
            error_type: ProviderErrorType::RateLimited,
            retryable: true,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for server-side errors.
    pub fn server_error(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::ServerError, None),
            message,
            status_code: Some(500),
            error_type: ProviderErrorType::ServerError,
            retryable: true,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for payment required / insufficient quota errors.
    /// These are NOT retryable — the user needs to add funds or upgrade their plan.
    pub fn payment_required(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::PaymentRequired, None),
            message,
            status_code: Some(402),
            error_type: ProviderErrorType::PaymentRequired,
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for context window / token limit overflow errors.
    /// Not directly retryable — requires history trimming before retry.
    pub fn context_overflow(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::ContextOverflow, None),
            message,
            status_code: None,
            error_type: ProviderErrorType::ContextOverflow,
            retryable: false,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for stream decode errors (mid-stream data corruption).
    /// These are retryable — re-issuing the same request may succeed.
    pub fn stream_decode(message: String) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::StreamDecodeError, None),
            message,
            status_code: None,
            error_type: ProviderErrorType::StreamDecodeError,
            retryable: true,
            retry_after_ms: None,
        }
    }

    /// Convenience constructor for stream silence / read timeout errors.
    /// These are retryable — the provider may have been temporarily overloaded.
    pub fn stream_timeout(timeout_secs: u64) -> Self {
        Self {
            user_message: Self::compute_user_message(&ProviderErrorType::StreamTimeout, None),
            message: format!("Stream timeout: no data received for {}s", timeout_secs),
            status_code: None,
            error_type: ProviderErrorType::StreamTimeout,
            retryable: true,
            retry_after_ms: None,
        }
    }

    /// Refresh `user_message` after `retry_after_ms` is set (e.g. from HTTP headers).
    /// Call this after mutating `retry_after_ms` to keep the user message in sync.
    pub fn refresh_user_message(&mut self) {
        self.user_message = Self::compute_user_message(&self.error_type, self.retry_after_ms);
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.status_code {
            write!(
                f,
                "[{}] {} (retryable={})",
                code, self.message, self.retryable
            )
        } else {
            write!(f, "{} (retryable={})", self.message, self.retryable)
        }
    }
}

impl std::error::Error for ProviderError {}

/// Structured error for stream events.
///
/// Unlike `ProviderError` (which represents call-level failures before a stream
/// is established), `StreamError` represents failures that occur *during* an
/// active SSE stream. It carries the same classification metadata so that
/// upstream consumers (ReliableProvider, AgentLoop) can make informed retry
/// decisions without string matching.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamError {
    /// Human-readable error message.
    pub message: String,
    /// Classified error type.
    pub error_type: ProviderErrorType,
    /// Whether re-issuing the same request may succeed.
    pub retryable: bool,
    /// HTTP status code, if the error originated from an HTTP response.
    pub status_code: Option<u16>,
}

impl StreamError {
    /// Create a `StreamError` from a `ProviderError`, preserving all metadata.
    pub fn from_provider_error(err: &ProviderError) -> Self {
        Self {
            message: err.message.clone(),
            error_type: err.error_type.clone(),
            retryable: err.retryable,
            status_code: err.status_code,
        }
    }

    /// Convenience constructor for stream decode errors (mid-stream data corruption).
    /// These are retryable — re-issuing the same request may succeed.
    pub fn stream_decode(message: String) -> Self {
        Self {
            message,
            error_type: ProviderErrorType::StreamDecodeError,
            retryable: true,
            status_code: None,
        }
    }

    /// Convenience constructor for stream silence / read timeout errors.
    /// These are retryable — the provider may have been temporarily overloaded.
    pub fn stream_timeout(timeout_secs: u64) -> Self {
        Self {
            message: format!("Stream timeout: no data received for {}s", timeout_secs),
            error_type: ProviderErrorType::StreamTimeout,
            retryable: true,
            status_code: None,
        }
    }

    /// Convenience constructor for context window / token limit overflow errors.
    /// Not directly retryable — requires history trimming before retry.
    pub fn context_overflow(message: String) -> Self {
        Self {
            message,
            error_type: ProviderErrorType::ContextOverflow,
            retryable: false,
            status_code: None,
        }
    }
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(code) = self.status_code {
            write!(
                f,
                "[{}] {} (type={:?}, retryable={})",
                code, self.message, self.error_type, self.retryable
            )
        } else {
            write!(
                f,
                "{} (type={:?}, retryable={})",
                self.message, self.error_type, self.retryable
            )
        }
    }
}

impl std::error::Error for StreamError {}

/// Chat message role
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

/// Content part for multimodal messages.
///
/// When a message contains only text, the `content` field (String) is used for
/// backward compatibility. `content_parts` carries the *non-text* payloads: a
/// caller that derives images server-side (the chat path since ADR-046) puts
/// only images in it and leaves the user's own words in `content`. Providers
/// must therefore serialise BOTH — see [`ChatMessage::message_content`], which
/// owns that rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentPart {
    /// Plain text content
    #[serde(rename = "text")]
    Text { text: String },
    /// Image URL (data:image/...;base64,... or https://...)
    #[serde(rename = "image_url")]
    ImageUrl { image_url: ImageUrlPart },
}

/// Image URL details for a ContentPart
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrlPart {
    /// The image URL or data URI (e.g. "data:image/png;base64,...")
    pub url: String,
    /// Optional detail level: "auto", "low", "high" (OpenAI convention)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Image width in pixels (for token estimation).
    /// When absent, a default of 512 is used for estimation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Image height in pixels (for token estimation).
    /// When absent, a default of 512 is used for estimation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
}

/// `(media_type, base64_payload)` for a `data:` URI, `None` for anything else
/// (a remote `https://` URL has no inlinable bytes).
///
/// This is the reading every provider needs before it can put an image on the
/// wire: Anthropic wants media type and payload as separate fields, Ollama
/// wants the bare payload in its `images[]` array. Free function rather than a
/// method so callers holding a `&str` keep their lifetime.
pub fn parse_data_uri(uri: &str) -> Option<(&str, &str)> {
    let rest = uri.strip_prefix("data:")?;
    let semi = rest.find(';')?;
    let media_type = &rest[..semi];
    let after_semi = &rest[semi + 1..];
    let comma = after_semi.find(',')?;
    if !after_semi[..comma].contains("base64") {
        return None;
    }
    Some((media_type, &after_semi[comma + 1..]))
}

impl ImageUrlPart {
    /// See [`parse_data_uri`].
    pub fn data_uri_parts(&self) -> Option<(&str, &str)> {
        parse_data_uri(&self.url)
    }

    /// The base64 payload alone, or `None` when the URL is not a base64 data
    /// URI.
    pub fn base64_payload(&self) -> Option<&str> {
        self.data_uri_parts().map(|(_, data)| data)
    }
}

impl ContentPart {
    /// Create a text content part
    pub fn text(text: impl Into<String>) -> Self {
        ContentPart::Text { text: text.into() }
    }

    /// Create an image_url content part from a data URI or URL
    pub fn image_url(url: impl Into<String>) -> Self {
        ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: url.into(),
                detail: None,
                width: None,
                height: None,
            },
        }
    }

    /// Create an image_url content part with a detail level
    pub fn image_url_with_detail(url: impl Into<String>, detail: impl Into<String>) -> Self {
        ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: url.into(),
                detail: Some(detail.into()),
                width: None,
                height: None,
            },
        }
    }
}

/// Cache boundary hint for prompt caching (ADR-060).
///
/// Carried on `ChatMessage` so the provider layer can emit the
/// provider-specific cache control directive:
/// - **Anthropic**: maps to `cache_control: { type: "ephemeral" }` on the
///   message (user/assistant) or on the system content-block.
/// - **OpenAI / Ollama**: ignored — block position alone determines cache.
///
/// ADR-060: Block A (static kernel) and Block C (dynamic todo snapshot)
/// carry `Ephemeral` breakpoints; Block B (append-only history) carries
/// `None` so a stable prefix stays byte-identical across turns.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CacheControl {
    /// Mark this message as a cache breakpoint (Anthropic ephemeral cache).
    Ephemeral,
    /// Reserved for future long-lived cache use; currently unused.
    Persistent,
}

/// A provider's view of one message's content: either a plain string, or a
/// part list that may need the message's own text prepended to it.
///
/// Produced by [`ChatMessage::message_content`]; see there for why text and
/// parts are not alternatives.
#[derive(Debug, Clone, Copy)]
pub enum MessageContent<'a> {
    /// No multimodal payload — serialise the string.
    Text(&'a str),
    /// Parts present — `leading_text` is the message's `content` when the list
    /// does not already carry a text block.
    Parts {
        leading_text: Option<&'a str>,
        parts: &'a [ContentPart],
    },
}

/// Chat message in conversation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatMessage {
    #[serde(default)]
    pub role: MessageRole,
    /// The message's text. Serialised on its own when there are no
    /// `content_parts`, and as a leading text block when there are — see
    /// [`ChatMessage::message_content`].
    #[serde(default)]
    pub content: String,
    /// Multimodal content parts — when present, providers should serialize
    /// content as an array of ContentPart objects instead of a plain string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_parts: Option<Vec<ContentPart>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Tool call ID — required for role=Tool messages to match the corresponding
    /// assistant tool_call. Maps to the `tool_call_id` field in the OpenAI API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Provider-specific cache boundary hint (ADR-060).
    /// `None` = no explicit cache control on this message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

impl ChatMessage {
    /// Create a user message
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::User,
            content: content.into(),
            ..Default::default()
        }
    }

    /// Create a user message with multimodal content parts.
    /// The `content` field is set to a human-readable summary for logging/debugging.
    pub fn user_multimodal(text_content: impl Into<String>, parts: Vec<ContentPart>) -> Self {
        Self {
            role: MessageRole::User,
            content: text_content.into(),
            content_parts: Some(parts),
            ..Default::default()
        }
    }

    /// What to put on the wire for this message's content.
    ///
    /// The rule lives here rather than in each provider because the two
    /// representations are easy to get wrong independently: `content_parts`
    /// holds the non-text payloads, and the text of the message is in
    /// `content`. Serialising only the parts — which is what a
    /// "prefer `content_parts` over `content`" reading gives you — drops the
    /// user's words on every image turn, which is exactly the bug this
    /// accessor closes.
    ///
    /// Borrowed, not cloned: a part carries base64 image data, so building a
    /// merged `Vec` per request would copy the payload for no reason.
    pub fn message_content(&self) -> MessageContent<'_> {
        match self.content_parts.as_deref() {
            None => MessageContent::Text(&self.content),
            Some(parts) => {
                // A caller that supplied its own text part owns its position:
                // caption-before-image is deliberate for some vision models.
                let has_text = parts
                    .iter()
                    .any(|p| matches!(p, ContentPart::Text { .. }));
                let leading_text = if has_text || self.content.trim().is_empty() {
                    None
                } else {
                    Some(self.content.as_str())
                };
                MessageContent::Parts {
                    leading_text,
                    parts,
                }
            }
        }
    }

    /// Create an assistant message
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            ..Default::default()
        }
    }

    /// Create an assistant message with reasoning content (DeepSeek thinking mode)
    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        reasoning: impl Into<String>,
    ) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            reasoning_content: Some(reasoning.into()),
            ..Default::default()
        }
    }

    /// Create an assistant message with tool calls
    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls: if tool_calls.is_empty() {
                None
            } else {
                Some(tool_calls)
            },
            ..Default::default()
        }
    }

    /// Create a tool result message
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            ..Default::default()
        }
    }

    /// Create a system message
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::System,
            content: content.into(),
            ..Default::default()
        }
    }
}

/// Tool call from LLM
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// Function call details
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Reasoning effort level for thinking-capable models.
///
/// Maps to provider-specific parameters:
/// - OpenAI-compatible: `reasoning_effort` field ("low", "medium", "high")
/// - Anthropic: `thinking` block with `budget_tokens`
/// - Google: `thinkingConfig` with `thinkingBudget`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    /// Let the model decide its own reasoning depth — no parameter is sent to the LLM.
    /// Displayed in the frontend; the control is shown and "Auto" is selected by default.
    #[default]
    Auto,
    /// Disable thinking/reasoning entirely.
    Off,
    /// Light reasoning — fast responses with minimal thinking.
    Low,
    /// Balanced reasoning — good trade-off between speed and depth.
    Medium,
    /// Deep reasoning — thorough analysis, slower responses.
    High,
    /// Maximum reasoning depth — exhaustive thinking for complex problems.
    Max,
}

impl ReasoningEffort {
    /// Parse from a string slice (case-insensitive).
    /// Returns `None` if the string does not match any known variant.
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "off" | "none" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "max" | "maximum" => Some(Self::Max),
            _ => None,
        }
    }

    /// Convert to the OpenAI-compatible API string value.
    /// Returns `None` for `Auto` (no parameter sent) and `Off` (omit field).
    pub fn to_openai_str(&self) -> Option<&'static str> {
        match self {
            // Auto and Off both result in omitting the field; provider uses its own default.
            Self::Auto => None,
            Self::Off => None,
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High => Some("high"),
            // Most OpenAI-compatible APIs cap at "high".
            Self::Max => Some("high"),
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Off => write!(f, "off"),
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
            Self::Max => write!(f, "max"),
        }
    }
}

/// Chat request to LLM provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub tools: Option<Vec<serde_json::Value>>,
    /// Reasoning/thinking effort level (None = use provider default, no override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    /// Anthropic-specific thinking mode: "extended" (budget_tokens) or "adaptive".
    /// When `None`, the provider decides based on the model (defaults to "extended").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_mode: Option<String>,
}

/// Chat response from LLM provider
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatResponse {
    #[serde(default)]
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
    /// Unix timestamp ms when reasoning/think started (set by streaming loop)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_started_at: Option<i64>,
    /// Unix timestamp ms when reasoning/think finished (set by streaming loop)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_finished_at: Option<i64>,
    /// Finish reason from the LLM API (e.g. "stop", "length", "tool_calls").
    /// Captured from streaming `choices[].finish_reason` for diagnostics.
    /// Helps distinguish model-initiated stop vs token-limit truncation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// Usage information
///
/// `prompt_tokens` and `completion_tokens` are the **total** values reported
/// by the API -- they **already include** cache tokens and reasoning tokens.
/// The `cache_*` and `reasoning_tokens` fields are breakouts for cost
/// calculation only and do NOT affect the totals.
///
/// ## OpenAI protocol mapping
/// - `prompt_tokens` <- API `usage.prompt_tokens` (includes cached)
/// - `completion_tokens` <- API `usage.completion_tokens` (includes reasoning)
/// - `cache_read_tokens` <- API `usage.prompt_tokens_details.cached_tokens`
/// - `reasoning_tokens` <- API `usage.completion_tokens_details.reasoning_tokens`
///
/// ## Anthropic protocol mapping
/// - `prompt_tokens` <- `input_tokens + cache_creation + cache_read`
/// - `completion_tokens` <- `output_tokens`
/// - `cache_read_tokens` <- `cache_read_input_tokens`
/// - `cache_write_tokens` <- `cache_creation_input_tokens`
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UsageInfo {
    /// Total input tokens (including cache read/write).
    pub prompt_tokens: u64,
    /// Total output tokens (including reasoning).
    pub completion_tokens: u64,
    /// Total tokens (prompt + completion).
    pub total_tokens: u64,
    /// Tokens served from prompt cache.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Tokens written to prompt cache.
    #[serde(default)]
    pub cache_write_tokens: u64,
    /// Reasoning/thinking tokens.
    #[serde(default)]
    pub reasoning_tokens: u64,
}

/// Stream event for streaming responses
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text chunk
    Content(String),
    /// Reasoning content chunk (e.g. DeepSeek thinking mode)
    ReasoningContent(String),
    /// Tool call start
    ToolCallStart(ToolCall),
    /// Tool call argument chunk (index identifies which tool call, arguments is incremental JSON fragment)
    ToolCallChunk { index: u64, arguments: String },
    /// Stream finished
    Finished(ChatResponse),
    /// Error occurred during streaming. Carries structured metadata so that
    /// upstream consumers (ReliableProvider, AgentLoop) can make informed retry
    /// decisions without string matching.
    Error(StreamError),
}

/// Provider trait for LLM providers
#[async_trait]
pub trait Provider: Send + Sync {
    /// Get provider name
    fn name(&self) -> &str;

    /// Send a chat request and get response
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;

    /// Send a chat request with streaming response
    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<Box<dyn futures_core::Stream<Item = StreamEvent> + Send>>;

    /// Count tokens in a message (approximate)
    async fn chat_token_count(&self, messages: &[ChatMessage]) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_data_uri_splits_media_type_from_payload() {
        assert_eq!(
            parse_data_uri("data:image/png;base64,iVBORw0KGgo="),
            Some(("image/png", "iVBORw0KGgo="))
        );
    }

    /// A remote URL has no bytes to inline, and pretending otherwise would put
    /// the URL string into a base64 field.
    #[test]
    fn parse_data_uri_rejects_urls_and_non_base64() {
        assert_eq!(parse_data_uri("https://host/x.png"), None);
        assert_eq!(parse_data_uri("data:image/png;charset=utf-8,abc"), None);
        assert_eq!(parse_data_uri("iVBORw0KGgo="), None);
    }

    #[test]
    fn image_url_part_exposes_the_bare_payload() {
        let part = ImageUrlPart {
            url: "data:image/jpeg;base64,/9j/4AAQ".into(),
            detail: None,
            width: None,
            height: None,
        };
        assert_eq!(part.base64_payload(), Some("/9j/4AAQ"));
    }

    fn image_part() -> ContentPart {
        ContentPart::ImageUrl {
            image_url: ImageUrlPart {
                url: "data:image/png;base64,iVBORw0KGgo=".into(),
                detail: None,
                width: None,
                height: None,
            },
        }
    }

    /// The part list a chat image turn produces carries no text block: the
    /// images are derived server-side (ADR-046) and the user's words stay in
    /// `content`. A provider that serialises only the parts drops the message —
    /// this accessor is the single place that rule is stated.
    #[test]
    fn image_only_parts_still_carry_the_message_text() {
        let msg = ChatMessage::user_multimodal("look at this", vec![image_part()]);
        match msg.message_content() {
            MessageContent::Parts { leading_text, parts } => {
                assert_eq!(leading_text, Some("look at this"));
                assert_eq!(parts.len(), 1);
            }
            other => panic!("expected a part list, got {other:?}"),
        }
    }

    /// A caller that built its own text part chose where it goes; the accessor
    /// must not add a second copy of the message in front of it.
    #[test]
    fn a_caller_supplied_text_part_keeps_its_own_position() {
        let msg = ChatMessage::user_multimodal(
            "not a second copy",
            vec![ContentPart::Text {
                text: "caption first".into(),
            }, image_part()],
        );
        match msg.message_content() {
            MessageContent::Parts { leading_text, parts } => {
                assert_eq!(leading_text, None);
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected a part list, got {other:?}"),
        }
    }

    /// An image with no caption must not send an empty text block.
    #[test]
    fn blank_text_adds_no_empty_block() {
        let msg = ChatMessage::user_multimodal("   ", vec![image_part()]);
        match msg.message_content() {
            MessageContent::Parts { leading_text, parts } => {
                assert_eq!(leading_text, None);
                assert_eq!(parts.len(), 1);
            }
            other => panic!("expected a part list, got {other:?}"),
        }
    }

    #[test]
    fn no_parts_serialises_as_plain_text() {
        let msg = ChatMessage::user("hello");
        assert!(matches!(
            msg.message_content(),
            MessageContent::Text("hello")
        ));
    }

    #[test]
    fn test_provider_error_from_status_429() {
        let err = ProviderError::from_status_code(429, "Too many requests".to_string());
        assert_eq!(err.status_code, Some(429));
        assert_eq!(err.error_type, ProviderErrorType::RateLimited);
        assert!(err.retryable);
        assert_eq!(err.message, "Too many requests");
    }

    #[test]
    fn test_provider_error_from_status_401() {
        let err = ProviderError::from_status_code(401, "Unauthorized".to_string());
        assert_eq!(err.status_code, Some(401));
        assert_eq!(err.error_type, ProviderErrorType::Unauthorized);
        assert!(!err.retryable);
    }

    #[test]
    fn test_provider_error_from_status_500() {
        let err = ProviderError::from_status_code(500, "Internal server error".to_string());
        assert_eq!(err.status_code, Some(500));
        assert_eq!(err.error_type, ProviderErrorType::ServerError);
        assert!(err.retryable);
    }

    #[test]
    fn test_provider_error_from_status_400() {
        let err = ProviderError::from_status_code(400, "Bad request".to_string());
        assert_eq!(err.status_code, Some(400));
        assert_eq!(err.error_type, ProviderErrorType::ClientError);
        assert!(!err.retryable);
    }

    #[test]
    fn test_provider_error_network() {
        let err = ProviderError::network("Connection timed out".to_string());
        assert_eq!(err.status_code, None);
        assert_eq!(err.error_type, ProviderErrorType::NetworkError);
        assert!(err.retryable);
        assert_eq!(err.message, "Connection timed out");
    }

    #[test]
    fn test_provider_error_display() {
        let err = ProviderError::from_status_code(429, "Rate limited".to_string());
        let display = format!("{err}");
        assert!(display.contains("429"));
        assert!(display.contains("Rate limited"));
        assert!(display.contains("retryable=true"));

        let err_no_code = ProviderError::network("DNS error".to_string());
        let display_no_code = format!("{err_no_code}");
        assert!(display_no_code.contains("DNS error"));
        assert!(display_no_code.contains("retryable=true"));
    }

    #[test]
    fn test_provider_error_from_status_402() {
        let err = ProviderError::from_status_code(402, "Insufficient quota".to_string());
        assert_eq!(err.status_code, Some(402));
        assert_eq!(err.error_type, ProviderErrorType::PaymentRequired);
        assert!(!err.retryable);
        assert_eq!(err.message, "Insufficient quota");
    }

    #[test]
    fn test_provider_error_payment_required_constructor() {
        let err = ProviderError::payment_required("Billing limit reached".to_string());
        assert_eq!(err.status_code, Some(402));
        assert_eq!(err.error_type, ProviderErrorType::PaymentRequired);
        assert!(!err.retryable);
        assert_eq!(err.message, "Billing limit reached");
    }
}
