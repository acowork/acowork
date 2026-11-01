//! Error pattern matching for LLM provider errors.
//!
//! Provides centralized pattern matching for classifying raw error strings
//! into structured types (`StreamError`, `ProviderErrorType`). This is the
//! single source of truth for "is this a context overflow?", "is this a
//! stream decode error?", etc.
//!
//! All upstream consumers (ReliableProvider, AgentLoop, etc.) should use
//! these functions instead of implementing their own string matching.

use crate::error::AcoworkError;
use crate::providers::traits::{ProviderErrorType, StreamError};

/// Check if a 404 body means "this endpoint does not exist here".
///
/// **Protocol-agnostic** on purpose: the same body shape is produced by every
/// common HTTP stack when a route is missing ("404 page not found" is Go's
/// `net/http` default, and MiniMax's Anthropic endpoint answers exactly that
/// for `POST /chat/completions` and `POST /responses`).
///
/// ADR-103 D8 already had a Responses-specific version of this check
/// (`responses_errors::looks_like_missing_endpoint`); it only matched bodies
/// that *name* the missing route, so a generic router 404 fell through to
/// [`ProviderError::from_status_code`] → `ClientError`, and the user was told
/// "Invalid request. Check model/tool config." — pointing at the wrong thing
/// entirely when the real fault is the protocol/URL pairing (the model was
/// routed to a wire protocol its endpoint does not serve).
///
/// The two are kept in one place on purpose: the Responses-specific hints
/// (`responses`, `unknown endpoint`, …) are a superset of the generic ones,
/// and a divergent pair is how the same 404 ends up with two different
/// classifications depending on which provider implementation saw it.
pub fn is_missing_endpoint_body(body: &str) -> bool {
    let lower = body.to_lowercase();
    const HINTS: &[&str] = &[
        // Generic router 404s ("404 page not found" — Go's net/http default).
        "404 page not found",
        "page not found",
        // Explicit route-missing wording.
        "unknown endpoint",
        "endpoint not found",
        "no such endpoint",
        "unknown route",
        "route not found",
        "not_found_error",
        "cannot post",
        "cannot get",
    ];
    HINTS.iter().any(|hint| lower.contains(hint))
}

/// Check if an error message indicates a context window / token limit overflow.
///
/// Covers patterns from Anthropic, OpenAI, MiniMax, DeepSeek, Ollama,
/// OpenRouter, llama.cpp, Mistral, and generic HTTP 413 responses.
pub fn is_context_overflow(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        // Anthropic
        "exceeds the context window",
        "exceeds the available context size",
        "context window of this model",
        // OpenAI
        "maximum context length",
        "context length exceeded",
        "too many tokens",
        "token limit exceeded",
        "prompt is too long",
        "input is too long",
        "prompt exceeds max length",
        // OpenRouter / DeepSeek / vLLM
        "max context length",
        // MiniMax / OpenAI API error code
        "context_length_exceeded",
        // HTTP 413
        "request entity too large",
        // Ollama
        "exceeded max context length",
        // Mistral
        "too large for model",
        // llama.cpp
        "exceeds the available context",
        // z.ai
        "model_context_window_exceeded",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error message indicates a stream decode / transport error.
///
/// These are typically caused by HTTP/2 frame errors, connection resets,
/// or mid-stream data corruption — all potentially recoverable by retry.
pub fn is_stream_decode_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        "error decoding response body",
        "error decoding",
        "connection reset",
        "broken pipe",
        "stream error",
        "h2 protocol error",
        "http2 frame error",
        "unexpected eof",
        "connection closed",
        "stream was reset",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error message indicates an authentication / authorization failure.
///
/// These are NOT retryable without user intervention (changing API key, etc.).
pub fn is_auth_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const PATTERNS: &[&str] = &[
        "unauthorized",
        "invalid api key",
        "incorrect api key",
        "missing api key",
        "api key not set",
        "authentication failed",
        "auth failed",
        "forbidden",
        "permission denied",
        "access denied",
        "invalid token",
    ];
    PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error message indicates a non-retryable rate limit
/// (business/quota exhaustion that retries cannot fix).
///
/// NOTE: Provider-specific business codes (e.g., MiniMax 1113/1311) should be
/// handled by the provider layer, not here. This function only matches
/// generic error message patterns.
pub fn is_non_retryable_rate_limit(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    const BUSINESS_PATTERNS: &[&str] = &[
        "plan does not include",
        "insufficient balance",
        "insufficient_quota",
        "quota exhausted",
        "out of credits",
        "no available package",
        "package not active",
        "model not available for your plan",
        "free usage limit",
        "token plan",
        "用量上限",
    ];
    BUSINESS_PATTERNS.iter().any(|p| lower.contains(p))
}

/// Check if an error indicates insufficient balance / quota exhaustion.
///
/// Combines:
/// 1. Generic business/quota exhaustion patterns (e.g. "insufficient_quota")
/// 2. MiniMax-specific business codes (1113, 1311)
///
/// These are NOT retryable — the user needs to add funds or switch provider.
pub fn is_balance_exhausted(error: &AcoworkError) -> bool {
    match error {
        AcoworkError::Provider(provider_err) => {
            // 1. Check generic patterns (e.g. "insufficient quota", "out of credits")
            is_non_retryable_rate_limit(&provider_err.message)
            // 2. Check MiniMax-specific business codes
            || is_minimax_balance_code(&provider_err.message)
        }
        _ => false,
    }
}

/// Check for MiniMax-specific balance exhaustion error codes.
///
/// These are provider-specific business codes that cannot be matched
/// by generic patterns alone.
pub fn is_minimax_balance_code(msg: &str) -> bool {
    msg.contains("1113") || msg.contains("1311")
}

/// Unified retryability check for `AcoworkError`.
///
/// This is the single source of truth for "should we retry this error?".
/// It reads the structured `retryable` flag on `ProviderError` (set at
/// construction time by `from_status_code` or convenience constructors).
pub fn is_retryable(error: &AcoworkError) -> bool {
    match error {
        AcoworkError::Provider(provider_err) => provider_err.retryable,
        AcoworkError::RateLimited(_) => true,
        // Network / transport I/O failures are retryable: a transient
        // disconnect (e.g. machine sleep, Wi-Fi drop) may be fixed by waiting.
        AcoworkError::Io(_) => true,
        _ => false,
    }
}

/// Classify a raw stream error string into a structured `StreamError`.
///
/// This is the unified entry point for converting unstructured error
/// strings (e.g. from SSE parsing failures) into structured types that
/// carry `error_type` and `retryable` metadata.
pub fn classify_stream_error(msg: &str) -> StreamError {
    if is_context_overflow(msg) {
        StreamError::context_overflow(msg.to_string())
    } else if is_stream_decode_error(msg) {
        StreamError::stream_decode(msg.to_string())
    } else if is_auth_error(msg) {
        StreamError {
            message: msg.to_string(),
            error_type: ProviderErrorType::Unauthorized,
            retryable: false,
            status_code: None,
        }
    } else if is_non_retryable_rate_limit(msg) {
        StreamError {
            message: msg.to_string(),
            error_type: ProviderErrorType::PaymentRequired,
            retryable: false,
            status_code: None,
        }
    } else {
        // Default: unknown errors are NOT retryable.
        // Treat conservatively — unknown errors could be permanent failures
        // (e.g. server-side rejection) and retrying would waste budget.
        StreamError {
            message: msg.to_string(),
            error_type: ProviderErrorType::Unknown,
            retryable: false,
            status_code: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AcoworkError;
    use crate::providers::traits::ProviderError;

    #[test]
    fn test_is_balance_exhausted_generic() {
        let err = AcoworkError::Provider(ProviderError::unknown(
            "insufficient_quota".to_string(),
        ));
        assert!(is_balance_exhausted(&err));

        let err = AcoworkError::Provider(ProviderError::unknown(
            "out of credits".to_string(),
        ));
        assert!(is_balance_exhausted(&err));
    }

    #[test]
    fn test_is_balance_exhausted_minimax_codes() {
        let err = AcoworkError::Provider(ProviderError::unknown(
            "Error code 1113: balance exhausted".to_string(),
        ));
        assert!(is_balance_exhausted(&err));

        let err = AcoworkError::Provider(ProviderError::unknown(
            "Code 1311: insufficient balance".to_string(),
        ));
        assert!(is_balance_exhausted(&err));
    }

    #[test]
    fn test_is_balance_exhausted_non_matching() {
        let err = AcoworkError::Provider(ProviderError::from_status_code(
            500,
            "500 internal error".to_string(),
        ));
        assert!(!is_balance_exhausted(&err));
    }

    #[test]
    fn test_is_balance_exhausted_cn_pattern() {
        let err = AcoworkError::Provider(ProviderError::unknown(
            "已达到 Token Plan 用量上限：请升级 Token Plan 套餐或购买积分补充用量。 (2056)".to_string(),
        ));
        assert!(is_balance_exhausted(&err));
    }

    #[test]
    fn test_is_retryable() {
        let err = AcoworkError::Provider(ProviderError::network("timeout".to_string()));
        assert!(is_retryable(&err));

        let err = AcoworkError::Provider(ProviderError::from_status_code(
            401,
            "401 unauthorized".to_string(),
        ));
        assert!(!is_retryable(&err));
    }

    #[test]
    fn test_context_overflow_detection() {
        assert!(is_context_overflow(
            "prompt exceeds the context window of this model"
        ));
        assert!(is_context_overflow("Prompt is too long: 150000 tokens"));
        assert!(is_context_overflow("context_length_exceeded"));
        assert!(is_context_overflow("Request entity too large"));
        assert!(!is_context_overflow("connection timed out"));
    }

    #[test]
    fn test_stream_decode_error_detection() {
        assert!(is_stream_decode_error("error decoding response body"));
        assert!(is_stream_decode_error(
            "Stream error: connection reset by peer"
        ));
        assert!(is_stream_decode_error("h2 protocol error"));
        assert!(!is_stream_decode_error("prompt is too long"));
    }

    #[test]
    fn test_auth_error_detection() {
        assert!(is_auth_error("Unauthorized: invalid api key"));
        assert!(is_auth_error("403 Forbidden"));
        assert!(!is_auth_error("connection reset"));
    }

    #[test]
    fn test_non_retryable_rate_limit_detection() {
        assert!(is_non_retryable_rate_limit("insufficient_quota"));
        assert!(is_non_retryable_rate_limit("quota exhausted"));
        assert!(is_non_retryable_rate_limit("out of credits"));
        assert!(!is_non_retryable_rate_limit("Too many requests"));
        // Note: MiniMax business codes (1113/1311) are handled by provider layer
        assert!(!is_non_retryable_rate_limit("Error code 1113"));
    }

    #[test]
    fn test_classify_stream_error_context_overflow() {
        let err = classify_stream_error("prompt exceeds the context window");
        assert_eq!(err.error_type, ProviderErrorType::ContextOverflow);
        assert!(!err.retryable);
    }

    #[test]
    fn test_classify_stream_error_decode() {
        let err = classify_stream_error("error decoding response body");
        assert_eq!(err.error_type, ProviderErrorType::StreamDecodeError);
        assert!(err.retryable);
    }

    #[test]
    fn test_classify_stream_error_auth() {
        let err = classify_stream_error("invalid api key provided");
        assert_eq!(err.error_type, ProviderErrorType::Unauthorized);
        assert!(!err.retryable);
    }

    #[test]
    fn test_classify_stream_error_unknown_non_retryable() {
        let err = classify_stream_error("some unknown transient error");
        assert_eq!(err.error_type, ProviderErrorType::Unknown);
        assert!(!err.retryable);
    }

    #[test]
    fn test_is_missing_endpoint_body_recognizes_generic_router_404() {
        // The exact body MiniMax's Anthropic endpoint answers for
        // `POST /chat/completions` — Go's net/http default.
        assert!(is_missing_endpoint_body("404 page not found"));
        assert!(is_missing_endpoint_body("404 Page Not Found"));
        assert!(is_missing_endpoint_body("page not found"));
        // Explicit route-missing wording.
        assert!(is_missing_endpoint_body("unknown endpoint"));
        assert!(is_missing_endpoint_body("endpoint not found"));
        assert!(is_missing_endpoint_body("no such endpoint"));
        assert!(is_missing_endpoint_body("unknown route"));
    }

    #[test]
    fn test_is_missing_endpoint_body_rejects_other_bodies() {
        // A 404 that names a *resource* (not a route) is not a protocol
        // fault — it must keep the generic classification.
        assert!(!is_missing_endpoint_body("model not found"));
        assert!(!is_missing_endpoint_body("{\"error\":\"invalid api key\"}"));
        assert!(!is_missing_endpoint_body(""));
        // A 400 body is never a missing endpoint.
        assert!(!is_missing_endpoint_body("invalid request: temperature not supported"));
    }
}
