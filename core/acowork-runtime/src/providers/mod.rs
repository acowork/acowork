//! Provider module

pub mod anthropic;
pub mod compat;
pub mod ollama;
pub mod openai;
pub mod registry;
pub mod reliable;
pub mod responses;
pub mod router;

#[cfg(test)]
mod compat_e2e;

/// ADR-056: Whether a provider `base_url` points at a local machine —
/// i.e. the provider is self-hosted and callable **without** an API key.
///
/// Used by the distillation availability checks (`AgentCore` /
/// `available_cache`) to accept local providers (Ollama, LM Studio, …)
/// whose `api_key` is empty by design, while still rejecting cloud
/// providers whose key was revoked/never configured.
pub fn is_local_base_url(url: &str) -> bool {
    url.contains("localhost")
        || url.contains("127.0.0.1")
        || url.contains("0.0.0.0")
        || url.contains("[::1]")
}

/// Parse the HTTP `Retry-After` header value into milliseconds.
///
/// Supports two formats per RFC 7231:
/// - Seconds: `"120"` → `120_000` ms
/// - HTTP-date: `"Wed, 21 Oct 2015 07:28:00 GMT"` → ms until that date
///
/// Returns `None` if the header is absent, malformed, or the date is in the past.
pub fn parse_retry_after_header(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?;
    let value_str = value.to_str().ok()?;

    // Try parsing as seconds (integer)
    if let Ok(seconds) = value_str.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }

    // Try parsing as HTTP-date (RFC 2822 / IMF-fixdate)
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc2822(value_str) {
        let now = chrono::Utc::now();
        let wait = datetime.signed_duration_since(now);
        let ms = wait.num_milliseconds();
        if ms > 0 {
            return Some(ms as u64);
        }
    }

    None
}

/// Convert an HTTP error response into a structured `ProviderError`.
///
/// This is the **single entry point** for converting `reqwest::Response`
/// failures into `ProviderError`. All provider implementations (OpenAI,
/// Anthropic, Ollama) should use this instead of manually constructing
/// `ProviderError::from_status_code` + `parse_retry_after_header`.
///
/// Automatically fills `error_type`, `retryable`, `retry_after_ms`,
/// and `user_message`.
pub async fn from_http_response(response: reqwest::Response) -> acowork_core::ProviderError {
    let status = response.status().as_u16();
    let headers = response.headers().clone();

    // Read error body (best-effort; may be empty for some providers)
    let body = response.text().await.unwrap_or_default();
    let message = if body.is_empty() {
        format!(
            "HTTP {} {}",
            status,
            reqwest::StatusCode::from_u16(status)
                .ok()
                .and_then(|s| s.canonical_reason())
                .unwrap_or("Error")
        )
    } else {
        body
    };

    let mut error = acowork_core::ProviderError::from_status_code(status, message);

    // Parse Retry-After header for 429 / 503 responses
    if let Some(retry_after_ms) = parse_retry_after_header(&headers) {
        error.retry_after_ms = Some(retry_after_ms);
        error.refresh_user_message();
    }

    error
}

/// Convert pre-extracted HTTP error parts into a structured `ProviderError`.
///
/// Use this when the response body has already been consumed (e.g. for
/// diagnostics or JSON parsing). For the simple case where you just need
/// to convert a failed response, use [`from_http_response`] instead.
///
/// Automatically fills `error_type`, `retryable`, `retry_after_ms`,
/// and `user_message`.
pub fn from_http_parts(
    status: u16,
    message: String,
    headers: &reqwest::header::HeaderMap,
) -> acowork_core::ProviderError {
    let mut error = acowork_core::ProviderError::from_status_code(status, message);

    if let Some(retry_after_ms) = parse_retry_after_header(headers) {
        error.retry_after_ms = Some(retry_after_ms);
        error.refresh_user_message();
    }

    error
}
