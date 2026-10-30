//! Provider modules

pub mod aliases;
pub mod error_patterns;
pub mod mock;
pub mod responses_errors;
pub mod traits;
pub mod url_resolve;

pub use aliases::{canonical_provider_id, vault_key_candidates};
pub use error_patterns::{
    classify_stream_error, is_balance_exhausted, is_context_overflow, is_minimax_balance_code,
    is_retryable, is_stream_decode_error,
};
pub use mock::{MockProvider, MockResponse};
pub use responses_errors::{classify_http_error, in_band_failure, truncated_stream};
pub use traits::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, FunctionCall, ImageUrlPart, MessageRole,
    Provider, ProviderError, ProviderErrorType, ReasoningEffort, StreamError, StreamEvent,
    ToolCall, UsageInfo,
};
