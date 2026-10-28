//! OpenAI Responses API provider (ADR-103 D4).
//!
//! A standalone `Provider` implementation for `POST {base_url}/responses`,
//! deliberately NOT folded into `openai.rs`: the two wire formats must not
//! share cache keys, URL construction, or the compat-fallback chain
//! (ADR-103 D4). Protocol-agnostic layers (`Provider` trait, `StreamEvent`,
//! `ReliableProvider`, agent loop) are untouched by this provider shape.
//!
//! ## Slice status (ADR-103 §7)
//!
//! This is the **S1 skeleton**: the routing chain is complete
//! (Gateway resolves → MQTT delivers → Runtime constructs → this provider
//! is reached), but the wire mapping is not implemented yet. Every call
//! fails with a non-retryable `EndpointUnavailable`-class error, so a
//! mis-routed session surfaces as a clear provider error instead of a
//! silent fallback to `/chat/completions` (ADR-103 D8 MUST NOT list).
//!
//! - S2: request mapping (`D5`) + non-streaming `chat()` + usage mapping
//! - S3: streaming SSE (`D6`)
//! - S4: URL resolution (`D7`) + full error classification (`D8`)

use std::time::Duration;

use async_trait::async_trait;

use acowork_core::providers::traits::{
    ChatMessage, ChatRequest, ChatResponse, Provider, ProviderError,
};

/// Responses-API-shaped provider. S1: routing skeleton only, all calls fail.
pub struct ResponsesProvider {
    /// Resolved `{base}` — the endpoint is `{base}/responses` (D7, S4).
    base_url: String,
    api_key: Option<String>,
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
        _connect_timeout: Duration,
        _stream_read_timeout: Duration,
    ) -> Self {
        Self {
            base_url: base_url
                .map(|u| u.trim_end_matches('/').to_string())
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            api_key: api_key.map(ToString::to_string),
            request_timeout,
        }
    }

    /// The single failure point of the S1 skeleton: an `EndpointUnavailable`
    /// class error — non-retryable, with actionable user text (D8 wording).
    fn endpoint_unavailable(&self) -> acowork_core::AcoworkError {
        tracing::warn!(
            base_url = %self.base_url,
            has_api_key = self.api_key.is_some(),
            request_timeout_secs = self.request_timeout.as_secs(),
            "ResponsesProvider (S1 skeleton) invoked: Responses API wire mapping not implemented yet"
        );
        acowork_core::AcoworkError::Provider(ProviderError {
            message: format!(
                "Responses API endpoint unavailable: the Responses provider shape ({}/responses) \
                 is not implemented in this build (ADR-103 S1 skeleton).",
                self.base_url
            ),
            user_message: "该 provider 的 Responses 协议尚未生效，请在模型设置里把协议切回 Chat Completions，\
                           或等待该版本完成 Responses 支持。"
                .to_string(),
            status_code: Some(404),
            error_type: acowork_core::providers::traits::ProviderErrorType::ClientError,
            retryable: false,
            retry_after_ms: None,
        })
    }
}

#[async_trait]
impl Provider for ResponsesProvider {
    fn name(&self) -> &str {
        "openai-responses"
    }

    async fn chat(&self, _request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
        Err(self.endpoint_unavailable())
    }

    async fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> acowork_core::error::Result<Box<dyn futures_core::Stream<Item = acowork_core::providers::traits::StreamEvent> + Send>>
    {
        Err(self.endpoint_unavailable())
    }

    async fn chat_token_count(
        &self,
        _messages: &[ChatMessage],
    ) -> acowork_core::error::Result<u64> {
        Err(self.endpoint_unavailable())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_stable_identifier() {
        let p = ResponsesProvider::with_base_url_and_timeouts(
            None,
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        );
        assert_eq!(p.name(), "openai-responses");
    }

    fn ping_request(model: &str) -> ChatRequest {
        ChatRequest {
            model: model.to_string(),
            messages: vec![ChatMessage::user("ping".to_string())],
            temperature: None,
            max_tokens: None,
            tools: None,
            reasoning_effort: None,
            thinking_mode: None,
        }
    }

    fn skeleton() -> ResponsesProvider {
        ResponsesProvider::with_base_url_and_timeouts(
            Some("https://api.deepseek.com/v1/"),
            Some("sk-test"),
            Duration::from_secs(600),
            Duration::from_secs(10),
            Duration::from_secs(45),
        )
    }

    #[test]
    fn base_url_is_trimmed_of_trailing_slash() {
        let p = skeleton();
        assert_eq!(p.base_url, "https://api.deepseek.com/v1");
    }

    #[tokio::test]
    async fn chat_fails_non_retryable_endpoint_unavailable() {
        let p = skeleton();
        let request = ping_request("deepseek-chat");
        let err = p.chat(request).await.expect_err("S1 skeleton must fail");
        match err {
            acowork_core::AcoworkError::Provider(pe) => {
                assert_eq!(pe.status_code, Some(404));
                assert!(!pe.retryable, "endpoint-unavailable must not be retried");
                assert!(pe.message.contains("responses"), "message names the endpoint");
            }
            other => panic!("expected Provider error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chat_stream_and_token_count_fail_the_same_way() {
        let p = skeleton();
        let request = ping_request("m");
        assert!(p.chat_stream(request).await.is_err());
        assert!(p.chat_token_count(&[ChatMessage::user("x".to_string())]).await.is_err());
    }
}
