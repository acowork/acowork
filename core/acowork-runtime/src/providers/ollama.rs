//! Ollama Provider
//!
//! Supports local Ollama API for running open-source models.
//! Default endpoint: http://localhost:11434

use async_trait::async_trait;
use futures_core::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall, MessageContent, MessageRole,
    Provider, StreamEvent, ToolCall, UsageInfo,
};

/// Ollama provider implementation
pub struct OllamaProvider {
    base_url: String,
    http_client: Client,
}

impl OllamaProvider {
    /// Create a new Ollama provider with default base URL and default timeouts
    pub fn new() -> Self {
        Self::with_base_url(None)
    }

    /// Create provider with custom base URL and default timeouts
    pub fn with_base_url(base_url: Option<&str>) -> Self {
        Self::with_base_url_and_timeouts(base_url, Duration::from_secs(300), Duration::from_secs(5))
    }

    /// Create provider with fully configurable timeouts
    pub fn with_base_url_and_timeouts(
        base_url: Option<&str>,
        request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        let http_client = Client::builder()
            .timeout(request_timeout)
            .connect_timeout(connect_timeout)
            .build()
            .expect("Failed to build HTTP client");

        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "http://localhost:11434".to_string()),
            http_client,
        }
    }
}

impl Default for OllamaProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ── Ollama API types ────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct OllamaChatRequest {
    model: String,
    messages: Vec<OllamaMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    options: Option<OllamaOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OllamaToolSpec>>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct OllamaOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(Debug, Serialize)]
struct OllamaMessage {
    role: String,
    content: String,
    /// Base64 payloads, no `data:` wrapper — the shape Ollama's chat API
    /// accepts. Absent for text-only messages, so their payloads are unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolSpec {
    #[serde(rename = "type")]
    kind: String,
    function: OllamaToolFunctionSpec,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolFunctionSpec {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaToolCall {
    function: OllamaFunctionCall,
}

#[derive(Debug, Serialize, Deserialize)]
struct OllamaFunctionCall {
    name: String,
    arguments: serde_json::Value,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OllamaChatResponse {
    message: OllamaResponseMessage,
    #[serde(default)]
    total_duration: Option<u64>,
    #[serde(default)]
    eval_count: Option<u64>,
    #[serde(default)]
    prompt_eval_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct OllamaResponseMessage {
    role: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

// ── Conversion helpers ──────────────────────────────────────────────────

fn convert_messages(messages: &[ChatMessage]) -> Vec<OllamaMessage> {
    messages
        .iter()
        .map(|m| {
            let role = match m.role {
                MessageRole::System => "system",
                MessageRole::User => "user",
                MessageRole::Assistant => "assistant",
                MessageRole::Tool => "tool",
            };

            // Ollama has one string per message plus an `images` array, so the
            // text is concatenated and the images lifted out. Reading only
            // `content` — as this did — silently dropped every image the user
            // attached; reading only the parts would have dropped their words.
            let mut texts: Vec<&str> = Vec::new();
            let mut images: Option<Vec<String>> = None;
            match m.message_content() {
                MessageContent::Text(text) => texts.push(text),
                MessageContent::Parts {
                    leading_text,
                    parts,
                } => {
                    if let Some(text) = leading_text {
                        texts.push(text);
                    }
                    let mut payloads = Vec::new();
                    for part in parts {
                        match part {
                            ContentPart::Text { text } => texts.push(text),
                            ContentPart::ImageUrl { image_url } => {
                                match image_url.base64_payload() {
                                    Some(payload) => payloads.push(payload.to_string()),
                                    None => tracing::warn!(
                                        url = %image_url.url,
                                        "ollama cannot inline a remote image URL, skipping it"
                                    ),
                                }
                            }
                        }
                    }
                    if !payloads.is_empty() {
                        images = Some(payloads);
                    }
                }
            }
            OllamaMessage {
                role: role.to_string(),
                content: texts.join("
"),
                images,
                tool_calls: None, // TODO: convert tool_calls
            }
        })
        .collect()
}

fn parse_response(msg: OllamaResponseMessage, resp: &OllamaChatResponse) -> ChatResponse {
    let content = msg.content.unwrap_or_default();

    let tool_calls = msg
        .tool_calls
        .unwrap_or_default()
        .into_iter()
        .map(|tc| ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            call_type: "function".to_string(),
            function: FunctionCall {
                name: tc.function.name,
                arguments: serde_json::to_string(&tc.function.arguments).unwrap_or_default(),
            },
        })
        .collect::<Vec<_>>();

    let usage = UsageInfo {
        prompt_tokens: resp.prompt_eval_count.unwrap_or(0),
        completion_tokens: resp.eval_count.unwrap_or(0),
        total_tokens: resp.prompt_eval_count.unwrap_or(0) + resp.eval_count.unwrap_or(0),
        ..Default::default()
    };

    ChatResponse {
        content,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        },
        usage: Some(usage),
        ..Default::default()
    }
}

// ── Provider trait implementation ───────────────────────────────────────

#[async_trait]
impl Provider for OllamaProvider {
    fn name(&self) -> &str {
        "ollama"
    }

    async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
        let ollama_request = OllamaChatRequest {
            model: request.model,
            messages: convert_messages(&request.messages),
            options: Some(OllamaOptions {
                temperature: request.temperature,
                num_predict: request.max_tokens,
            }),
            tools: None, // TODO: convert tools
            stream: false,
        };

        let url = format!("{}/api/chat", self.base_url);

        let response = self
            .http_client
            .post(&url)
            .json(&ollama_request)
            .send()
            .await
            .map_err(|e| {
                acowork_core::AcoworkError::Provider(acowork_core::ProviderError::network(format!(
                    "Ollama request failed: {e}"
                )))
            })?;

        if !response.status().is_success() {
            let err = crate::providers::from_http_response(response).await;
            return Err(acowork_core::AcoworkError::Provider(err));
        }

        let ollama_resp: OllamaChatResponse = response.json().await.map_err(|e| {
            acowork_core::AcoworkError::Provider(acowork_core::ProviderError::unknown(format!(
                "Failed to parse Ollama response: {e}"
            )))
        })?;

        let msg = ollama_resp.message;
        let prompt_eval = ollama_resp.prompt_eval_count;
        let eval = ollama_resp.eval_count;
        let dummy_resp = OllamaChatResponse {
            message: OllamaResponseMessage {
                role: String::new(),
                content: None,
                tool_calls: None,
            },
            total_duration: None,
            eval_count: eval,
            prompt_eval_count: prompt_eval,
        };
        Ok(parse_response(msg, &dummy_resp))
    }

    async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> acowork_core::error::Result<Box<dyn Stream<Item = StreamEvent> + Send>> {
        // Ollama streaming not yet implemented — fall back to non-streaming
        let response = self.chat(request).await?;
        Ok(Box::new(OnceStream {
            event: Some(StreamEvent::Finished(response)),
        }))
    }

    async fn chat_token_count(&self, messages: &[ChatMessage]) -> acowork_core::error::Result<u64> {
        let total_chars: usize = messages.iter().map(|m| m.content.len()).sum();
        Ok((total_chars as f64 / 4.0).ceil() as u64)
    }
}

/// Simple stream that yields a single event
struct OnceStream {
    event: Option<StreamEvent>,
}

impl Stream for OnceStream {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.event.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(url: &str) -> acowork_core::providers::traits::ContentPart {
        acowork_core::providers::traits::ContentPart::ImageUrl {
            image_url: acowork_core::providers::traits::ImageUrlPart {
                url: url.into(),
                detail: None,
                width: None,
                height: None,
            },
        }
    }

    /// Regression: this provider read `content` alone, so an attached image
    /// never reached the model. Ollama wants the payload bare in `images[]`.
    #[test]
    fn multimodal_message_sends_text_and_bare_base64_images() {
        let msg = ChatMessage::user_multimodal(
            "what does this screenshot show?",
            vec![image("data:image/png;base64,iVBORw0KGgo=")],
        );
        let json = serde_json::to_value(convert_messages(&[msg])).unwrap();
        assert_eq!(json[0]["content"], "what does this screenshot show?");
        assert_eq!(json[0]["images"][0], "iVBORw0KGgo=");
        // The wrapper must not survive: Ollama base64-decodes the string.
        assert!(!json[0]["images"][0]
            .as_str()
            .unwrap()
            .starts_with("data:"));
    }

    /// A text-only turn must keep the payload it had before images were wired
    /// up — an empty `images` array is a schema surprise, not a no-op.
    #[test]
    fn text_only_message_carries_no_images_key() {
        let json =
            serde_json::to_value(convert_messages(&[ChatMessage::user("hello")])).unwrap();
        assert!(
            json[0].get("images").is_none(),
            "no images key expected, got {json}"
        );
    }

    /// Ollama's chat API takes base64 only, so a remote URL cannot be inlined
    /// here. Dropping the image must not drop the message.
    #[test]
    fn remote_url_is_skipped_without_losing_the_text() {
        let msg = ChatMessage::user_multimodal("see the link", vec![image("https://x/y.png")]);
        let json = serde_json::to_value(convert_messages(&[msg])).unwrap();
        assert_eq!(json[0]["content"], "see the link");
        assert!(json[0].get("images").is_none(), "got {json}");
    }

    #[test]
    fn test_ollama_provider_creation() {
        let provider = OllamaProvider::new();
        assert_eq!(provider.name(), "ollama");
        assert_eq!(provider.base_url, "http://localhost:11434");
    }

    #[test]
    fn test_ollama_provider_custom_url() {
        let provider = OllamaProvider::with_base_url(Some("http://192.168.1.100:11434"));
        assert_eq!(provider.base_url, "http://192.168.1.100:11434");
    }

    #[test]
    fn test_convert_messages() {
        let messages = vec![ChatMessage {
            role: MessageRole::User,
            content: "Hello".to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            cache_control: None,
        }];
        let ollama_msgs = convert_messages(&messages);
        assert_eq!(ollama_msgs.len(), 1);
        assert_eq!(ollama_msgs[0].role, "user");
    }

    #[test]
    fn test_convert_messages_cache_control_ignored() {
        use acowork_core::providers::traits::CacheControl;

        // ADR-060 regression: Ollama wire format has no cache marker — a
        // `cache_control` flag on ChatMessage must be silently ignored
        // (no serialization impact), never rejected.
        let messages = vec![ChatMessage {
            role: MessageRole::System,
            content: "You are helpful.".to_string(),
            content_parts: None,
            reasoning_content: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
            cache_control: Some(CacheControl::Ephemeral),
        }];

        let ollama_msgs = convert_messages(&messages);
        let json = serde_json::to_value(&ollama_msgs[0]).unwrap();
        assert_eq!(json["role"], "system");
        assert_eq!(json["content"], "You are helpful.");
        assert!(
            json.get("cache_control").is_none(),
            "Ollama wire format must not carry cache_control"
        );
    }
}
