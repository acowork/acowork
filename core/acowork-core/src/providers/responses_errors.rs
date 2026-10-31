//! ADR-103 D8 — Responses-shaped error classification.
//!
//! Layering (ADR-016): classification lives in `acowork-core`, retry
//! orchestration belongs to `ReliableProvider`, presentation to the frontend.
//! The Gateway probe (ADR-103 D10) and the Runtime provider therefore share
//! this module instead of re-deriving the same table twice.
//!
//! **MUST NOT**: nothing here may ever cause the caller to re-send a request
//! to `/chat/completions`. A wrong-shape retry is "retry with a different
//! semantic", which cannot be attributed when it fails (ADR-103 D8/D9).
//!
//! Classification table (ADR-103 D8):
//!
//! | signal | class | handling |
//! |---|---|---|
//! | 404 naming `responses` / unknown endpoint | [`ProviderErrorType::EndpointUnavailable`] | no retry, actionable text |
//! | 400 naming a request field | [`ProviderErrorType::UnsupportedField`] | trim whitelist (S7) |
//! | 400 content-integrity | [`ProviderErrorType::ContentIntegrity`] | no trim, no retry |
//! | 400 over-window | [`ProviderErrorType::ContextOverflow`] | history trimming |
//! | 401/403 · 429 · 5xx | existing platform classes | unchanged |
//! | SSE `response.failed` | [`ProviderErrorType::InBandFailure`] | `StreamEvent::Error` |
//! | stream end without terminal event | [`ProviderErrorType::TruncatedStream`] | `StreamEvent::Error`, no retry |

use super::traits::{ProviderError, ProviderErrorType, StreamError};

/// Request fields the trim path is allowed to drop when the provider rejects
/// them (ADR-103 D8, revised: the whitelist is `reasoning` + `temperature`).
///
/// Widening this list requires an ADR change — every surviving entry is a
/// silent behaviour change for the user, which is why the table is narrow.
///
/// `temperature` joined `reasoning` because reasoning models reject the
/// parameter outright ("'temperature' is not supported with this model"),
/// while the Runtime's fallback chain injects `DEFAULT_TEMPERATURE` even
/// when the user never set one. For such a model the platform default is not
/// user intent, and refusing to drop it made the model unusable with an
/// error the user could not act on. An endpoint that accepts temperature is
/// unaffected: the trim is evidence-gated per `(provider, model)` and only
/// ever applies after that pair answered 400 naming the field.
pub const TRIMMABLE_FIELDS: &[&str] = &["reasoning", "temperature"];

/// Every field whose 400 mention is recognized as "this provider does not
/// implement that field". Only [`TRIMMABLE_FIELDS`] may be trimmed; the rest
/// are surfaced to the user so the model settings can be adjusted (D8:
/// `tool_choice` / `text.format` / `top_p` change model behaviour when
/// dropped without an endpoint-side rejection to justify it, so they are
/// never compensated client-side).
pub const RECOGNIZED_FIELDS: &[&str] = &[
    "reasoning",
    "tool_choice",
    "text.format",
    "temperature",
    "top_p",
];

/// Body hints that a 404 means "this endpoint does not exist here".
fn looks_like_missing_endpoint(body_lower: &str) -> bool {
    body_lower.contains("responses")
        || body_lower.contains("unknown endpoint")
        || body_lower.contains("endpoint not found")
        || body_lower.contains("no such endpoint")
        || body_lower.contains("unknown route")
        || body_lower.contains("not_found_error")
}

/// Body hints that a 400 is "the content itself is inconsistent".
///
/// `must be passed back` / `reasoning_content` cover the DeepSeek family:
/// "The reasoning_content in the thinking mode must be passed back to the API"
/// is a *content* defect. Trimming `reasoning` would have masked it — that is
/// the 2026-09-05 incident this list exists to prevent (ADR-103 D8).
fn looks_like_content_integrity(body_lower: &str) -> bool {
    const HINTS: &[&str] = &[
        "image",
        "empty",
        "tool name",
        "function name",
        "invalid tool",
        "invalid function",
        "tool_call_id",
        "unsupported content",
        "content type",
        "must be passed back",
        "reasoning_content",
    ];
    HINTS.iter().any(|hint| body_lower.contains(hint))
}

/// Body hints that a 400 is a context-window overflow.
fn looks_like_overflow(body_lower: &str) -> bool {
    const HINTS: &[&str] = &[
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "too many tokens",
        "too long",
        "token limit",
    ];
    HINTS.iter().any(|hint| body_lower.contains(hint))
}

/// The recognized request field a provider body blames, if any.
///
/// Matching is a case-insensitive substring search over [`RECOGNIZED_FIELDS`],
/// longest name first so `text.format` wins over a bare `text` mention — but
/// only **whole-identifier** mentions count (`[a-z0-9_]` boundaries).
///
/// The boundary rule is what keeps `reasoning_content must be passed back`
/// (a content defect) from being read as "the `reasoning` parameter is
/// unsupported", which would put a real defect on the D8 trim path.
pub fn field_named_in_body(body: &str) -> Option<&'static str> {
    let body_lower = body.to_lowercase();
    RECOGNIZED_FIELDS
        .iter()
        .copied()
        .find(|field| mentions_identifier(&body_lower, &field.to_lowercase()))
}

/// Does `haystack` contain `needle` as a whole identifier?
fn mentions_identifier(haystack: &str, needle: &str) -> bool {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0;
    while let Some(pos) = haystack[from..].find(needle) {
        let start = from + pos;
        let end = start + needle.len();
        let before_ok = haystack[..start].chars().next_back().is_none_or(|c| !is_ident(c));
        let after_ok = haystack[end..].chars().next().is_none_or(|c| !is_ident(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.len();
    }
    false
}

/// Classify a non-2xx Responses HTTP response (ADR-103 D8).
///
/// 429/401/403/5xx keep the platform-wide mapping from
/// [`ProviderError::from_status_code`]; only the Responses-specific 404 and
/// 400 rows are refined here.
pub fn classify_http_error(status: u16, body: &str) -> ProviderError {
    let body_lower = body.to_lowercase();

    match status {
        404 if looks_like_missing_endpoint(&body_lower) => ProviderError {
            message: body.to_string(),
            user_message: "This provider does not serve the Responses API. Switch the model \
                           protocol back to Chat Completions in model settings."
                .to_string(),
            status_code: Some(404),
            error_type: ProviderErrorType::EndpointUnavailable,
            retryable: false,
            retry_after_ms: None,
        },
        400 => {
            if let Some(field) = field_named_in_body(body) {
                return ProviderError::unsupported_field_error(field, body.to_string());
            }
            if looks_like_overflow(&body_lower) {
                // ADR-103 D8 groups this under content integrity; the platform
                // already has a dedicated class that drives history trimming,
                // so the more specific one wins (both are non-trim/non-retry
                // at the provider level).
                return ProviderError::context_overflow(body.to_string());
            }
            if looks_like_content_integrity(&body_lower) {
                return ProviderError {
                    message: body.to_string(),
                    user_message: ProviderError::compute_user_message(
                        &ProviderErrorType::ContentIntegrity,
                        None,
                    ),
                    status_code: Some(400),
                    error_type: ProviderErrorType::ContentIntegrity,
                    retryable: false,
                    retry_after_ms: None,
                };
            }
            ProviderError::from_status_code(status, body.to_string())
        }
        _ => ProviderError::from_status_code(status, body.to_string()),
    }
}

/// Classify an in-band `response.failed` stream event (ADR-103 D8).
///
/// Partial output may already have been delivered, so the result is never
/// retryable — this is the same reason the ADR forbids waiting for `[DONE]`
/// and pretending a truncated stream succeeded.
pub fn in_band_failure(code: &str, message: &str) -> StreamError {
    let code = code.trim();
    StreamError {
        message: if code.is_empty() {
            message.to_string()
        } else {
            format!("{code}: {message}")
        },
        error_type: ProviderErrorType::InBandFailure,
        retryable: false,
        status_code: None,
    }
}

/// Classify "the stream ended without the terminal event" (ADR-103 D6.1/D8).
///
/// Carries what was already delivered so the message can say how much of the
/// answer exists, which is what makes the truncation visible to the user
/// instead of an empty-looking reply.
pub fn truncated_stream(content_chars: u64, tool_calls: u64) -> StreamError {
    StreamError {
        message: format!(
            "Responses stream ended without a terminal event ({content_chars} content chars, \
             {tool_calls} tool calls already received); the answer is incomplete"
        ),
        error_type: ProviderErrorType::TruncatedStream,
        retryable: false,
        status_code: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_endpoint_404_is_actionable_and_not_retryable() {
        let err = classify_http_error(
            404,
            r#"{"error":{"message":"Invalid URL (POST /v1/responses)"}}"#,
        );
        assert_eq!(err.error_type, ProviderErrorType::EndpointUnavailable);
        assert!(!err.retryable);
        assert_eq!(err.status_code, Some(404));
        assert!(
            err.user_message.contains("Responses API")
                && err.user_message.contains("Chat Completions"),
            "text must tell the user which way out exists: {}",
            err.user_message
        );
    }

    #[test]
    fn unknown_endpoint_wording_also_maps_to_endpoint_unavailable() {
        for body in [
            "unknown endpoint: /v1/responses",
            "Not Found",
            "{\"error\":{\"type\":\"not_found_error\",\"message\":\"no route\"}}",
        ] {
            let err = classify_http_error(404, body);
            if body.contains("responses") || body.to_lowercase().contains("unknown endpoint") {
                assert_eq!(
                    err.error_type,
                    ProviderErrorType::EndpointUnavailable,
                    "body: {body}"
                );
            }
        }
        // A bare "Not Found" carries no Responses hint: stays a client error.
        assert_eq!(
            classify_http_error(404, "Not Found").error_type,
            ProviderErrorType::ClientError
        );
    }

    #[test]
    fn each_recognized_field_maps_to_unsupported_field() {
        for field in RECOGNIZED_FIELDS {
            let body = format!("{{\"error\":{{\"message\":\"Unsupported parameter: '{field}'\"}}}}");
            let err = classify_http_error(400, &body);
            assert_eq!(
                err.error_type,
                ProviderErrorType::UnsupportedField,
                "field {field} must classify as UnsupportedField"
            );
            assert_eq!(err.unsupported_field(), Some(*field));
            assert!(!err.retryable, "the untrimmed request never succeeds");
        }
    }

    #[test]
    fn unsupported_field_round_trips_through_the_message_marker() {
        let err = ProviderError::unsupported_field_error(
            "reasoning",
            "reasoning.effort 'none' not supported".to_string(),
        );
        assert_eq!(err.unsupported_field(), Some("reasoning"));
        assert!(err.message.contains("reasoning.effort"));
        // A non-UnsupportedField error never reports a field.
        assert_eq!(
            ProviderError::from_status_code(400, "bad".to_string()).unsupported_field(),
            None
        );
    }

    #[test]
    fn content_integrity_400_never_reaches_the_trim_whitelist() {
        for body in [
            "invalid image placement in input[1]",
            "input is empty",
            "tool name 'x y' is invalid",
            "unsupported content type image/webp",
        ] {
            let err = classify_http_error(400, body);
            assert_eq!(
                err.error_type,
                ProviderErrorType::ContentIntegrity,
                "body: {body}"
            );
            assert!(!err.retryable);
            assert_eq!(err.unsupported_field(), None);
        }
    }

    #[test]
    fn reasoning_content_bodies_are_content_defects_not_trim_targets() {
        // The 2026-09-05 lesson, applied to the word "reasoning": a body that
        // complains about reasoning *content* must never be read as "the
        // `reasoning` parameter is unsupported" — otherwise the D8 trim would
        // mask a real defect by dropping the field.
        for body in [
            "The reasoning_content in the thinking mode must be passed back to the API",
            "request error: the message history must be passed back in order",
        ] {
            let err = classify_http_error(400, body);
            assert_eq!(
                err.error_type,
                ProviderErrorType::ContentIntegrity,
                "body: {body}"
            );
            assert_eq!(err.unsupported_field(), None, "body: {body}");
        }
        // A whole-identifier mention is still the parameter itself.
        for body in [
            "invalid parameter: `reasoning`",
            "reasoning.effort 'none' not supported",
            "{\"error\":{\"param\":\"reasoning\"}}",
        ] {
            let err = classify_http_error(400, body);
            assert_eq!(
                err.error_type,
                ProviderErrorType::UnsupportedField,
                "body: {body}"
            );
            assert_eq!(err.unsupported_field(), Some("reasoning"), "body: {body}");
        }
    }

    #[test]
    fn over_window_400_uses_the_existing_context_overflow_class() {
        let err = classify_http_error(400, "maximum context length is 128000 tokens");
        assert_eq!(err.error_type, ProviderErrorType::ContextOverflow);
        assert!(!err.retryable);
    }

    #[test]
    fn unclassified_statuses_keep_the_platform_mapping() {
        assert_eq!(
            classify_http_error(429, "slow down").error_type,
            ProviderErrorType::RateLimited
        );
        assert_eq!(
            classify_http_error(503, "unavailable").error_type,
            ProviderErrorType::ServerError
        );
        assert_eq!(
            classify_http_error(401, "bad key").error_type,
            ProviderErrorType::Unauthorized
        );
        assert_eq!(
            classify_http_error(400, "something else entirely").error_type,
            ProviderErrorType::ClientError
        );
    }

    #[test]
    fn in_band_failure_keeps_the_code_and_is_never_retryable() {
        let err = in_band_failure("server_error", "upstream exploded");
        assert_eq!(err.error_type, ProviderErrorType::InBandFailure);
        assert!(!err.retryable);
        assert_eq!(err.message, "server_error: upstream exploded");

        let bare = in_band_failure("", "no code");
        assert_eq!(bare.message, "no code");
    }

    #[test]
    fn truncated_stream_is_reported_as_its_own_class() {
        let err = truncated_stream(42, 1);
        assert_eq!(err.error_type, ProviderErrorType::TruncatedStream);
        assert!(!err.retryable, "partial output must not be re-run blindly");
        assert!(err.message.contains("42 content chars"));
        assert!(err.message.contains("1 tool calls"));
    }

    #[test]
    fn trim_whitelist_stays_a_subset_of_the_recognized_fields() {
        for field in TRIMMABLE_FIELDS {
            assert!(
                RECOGNIZED_FIELDS.contains(field),
                "{field} is trimmable but not recognized"
            );
        }
        assert_eq!(
            TRIMMABLE_FIELDS,
            &["reasoning", "temperature"],
            "ADR-103 D8 pins the whitelist; widening it requires an ADR revision"
        );
    }
}