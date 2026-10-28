//! Unified Token Counter
//!
//! Session-scoped chars/token ratio for token estimation
//! (`tokens ≈ chars / ratio`). The ratio is the last value calibrated from
//! LLM API feedback (`ratio = input_chars / prompt_tokens`, same source,
//! no cross-tokenizer error) — there is no per-model table: multi-language
//! input has no stable per-model constant, only a live value with bounded
//! local error that only propagates to the next turn's new input, never
//! to already-counted history.
//!
//! Also provides:
//! - Full-field ChatMessage counting (role, name, tool_calls)
//! - Image token estimation per protocol type

use acowork_core::protocol::ProtocolType;
use acowork_core::providers::traits::ChatMessage;

/// Default chars/token ratio used when no calibration exists yet.
/// Empirically observed across major LLM families for natural-language text;
/// CJK-heavy content converges to ~2.0-2.5 after the first calibration.
pub const DEFAULT_RATIO: f64 = 3.5;

/// Minimum and maximum valid chars/token ratios.
const RATIO_MIN: f64 = 1.0;
const RATIO_MAX: f64 = 10.0;

// ── Image Token Estimation ──────────────────────────────────────────────

/// Estimate token count for an image based on protocol type.
///
/// Different LLM providers use different image tokenization strategies.
/// When width/height are unknown (None), a conservative default of 512×512 is used.
pub fn estimate_image_tokens(
    protocol_type: &ProtocolType,
    width: Option<u32>,
    height: Option<u32>,
    detail: Option<&str>,
) -> u64 {
    // Default to 512×512 when dimensions are unknown (conservative estimate).
    let w = width.unwrap_or(512) as u64;
    let h = height.unwrap_or(512) as u64;

    match protocol_type {
        // ADR-103 §3: the Responses shape bills images with the same
        // tokenization formula as Chat Completions. `Unknown` is the
        // read-side tolerance value (falls back to OpenAI behavior, D3).
        ProtocolType::OpenAI | ProtocolType::Responses | ProtocolType::Unknown => {
            // OpenAI: "low" detail uses fixed 85 tokens.
            // "high"/"auto" tiles the image at 512×512.
            if detail == Some("low") {
                return 85;
            }
            let tiles_w = w.div_ceil(512);
            let tiles_h = h.div_ceil(512);
            85 + 170 * tiles_w * tiles_h
        }
        ProtocolType::Anthropic => {
            // Anthropic: approximately 1 token per 750 pixels.
            (w * h) / 750
        }
        ProtocolType::Google => {
            // Google Gemini: approximately 1 token per 258 pixels.
            (w * h) / 258
        }
        ProtocolType::Ollama => {
            // Ollama models typically don't support vision.
            // Use conservative estimate for any vision-capable Ollama models.
            (w * h) / 258
        }
    }
}

// ── Token Counter ───────────────────────────────────────────────────────

/// Unified token counter with a single session chars/token ratio.
///
/// All token estimation uses `chars / ratio`. The ratio is the last value
/// calibrated from LLM API feedback (or the default 3.5 before the first
/// calibration) and is shared by every counting path in the session.
pub struct TokenCounter {
    /// Session chars/token ratio (last calibrated value, default 3.5)
    ratio: f64,
    /// Whether the ratio has been calibrated from real API feedback.
    calibrated: bool,
}

impl TokenCounter {
    /// Create a new token counter with the default ratio 3.5 until
    /// calibrated from real API feedback.
    pub fn new() -> Self {
        Self {
            ratio: DEFAULT_RATIO,
            calibrated: false,
        }
    }

    /// Replace the session ratio with a fresh calibration sample
    /// (`chars / prompt_tokens`). Clamped to the valid range.
    pub fn set_ratio(&mut self, sample: f64) {
        let clamped = sample.clamp(RATIO_MIN, RATIO_MAX);
        if clamped != sample {
            tracing::warn!(
                raw_sample = %sample,
                clamped = %clamped,
                "Ratio sample out of realistic range, clamping"
            );
        }
        self.ratio = clamped;
        self.calibrated = true;
    }

    /// Current chars/token ratio (default 3.5 until calibrated).
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Whether the ratio has been calibrated from real API feedback.
    pub fn is_calibrated(&self) -> bool {
        self.calibrated
    }

    // ── Text counting ───────────────────────────────────────────────────

    /// Count tokens for a single text string using the session ratio.
    ///
    /// `tokens = ceil(text.len() / ratio)`.
    pub fn count_text(&self, text: &str) -> u64 {
        if text.is_empty() {
            return 0;
        }
        (text.len() as f64 / self.ratio).ceil() as u64
    }

    // ── Message counting ────────────────────────────────────────────────

    /// Count tokens for a full ChatMessage (including role, name, tool_calls overhead).
    /// When `protocol_type` is provided, image content parts are included in the count.
    pub fn count_message(
        &self,
        message: &ChatMessage,
        protocol_type: Option<&ProtocolType>,
    ) -> u64 {
        let mut tokens = 0u64;

        // Role overhead: ~1 token for role marker
        tokens += 1;

        // Name overhead: ~1 token per 4 chars + 1 for the name field
        if let Some(ref name) = message.name {
            tokens += self.count_text(name) + 1;
        }

        // Content tokens, through the same accessor the providers use.
        // Counting "the parts, else the content" valued every image turn's text
        // at zero tokens — the derived part list holds images only and the
        // user's message lives in `content` — which then skewed the context
        // budget and compaction against turns that were longer than measured.
        match message.message_content() {
            acowork_core::providers::traits::MessageContent::Text(text) => {
                tokens += self.count_text(text);
            }
            acowork_core::providers::traits::MessageContent::Parts {
                leading_text,
                parts,
            } => {
                if let Some(text) = leading_text {
                    tokens += self.count_text(text);
                }
                for part in parts {
                    match part {
                        acowork_core::providers::traits::ContentPart::Text { text } => {
                            tokens += self.count_text(text);
                        }
                        acowork_core::providers::traits::ContentPart::ImageUrl { image_url } => {
                            if let Some(pt) = protocol_type {
                                tokens += estimate_image_tokens(
                                    pt,
                                    image_url.width,
                                    image_url.height,
                                    image_url.detail.as_deref(),
                                );
                            }
                            // If protocol_type is unknown, skip image tokens (best-effort)
                        }
                    }
                }
            }
        }

        // Tool calls overhead
        if let Some(ref tool_calls) = message.tool_calls {
            for tc in tool_calls {
                // Each tool call has overhead: id + type + function wrapper ~4 tokens
                tokens += 4;
                // Function name
                tokens += self.count_text(&tc.function.name);
                // Function arguments
                tokens += self.count_text(&tc.function.arguments);
            }
        }

        // Message boundary token (varies by API but typically 1)
        tokens += 1;

        tokens
    }
}

impl Default for TokenCounter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::traits::{FunctionCall, ToolCall};

    /// An image turn carries its pictures in `content_parts` and its words in
    /// `content`. Counting one "or" the other billed every such turn as if the
    /// user had typed nothing, which is what fed a too-small number to the
    /// context budget and to compaction.
    #[test]
    fn image_turn_counts_its_text_and_its_image() {
        use acowork_core::providers::traits::{ChatMessage, ContentPart, ImageUrlPart};
        let counter = TokenCounter::new();
        let text = "the delete button style is wrong, copy the memory panel one";
        let msg = ChatMessage::user_multimodal(
            text,
            vec![ContentPart::ImageUrl {
                image_url: ImageUrlPart {
                    url: "data:image/png;base64,iVBORw0KGgo=".into(),
                    detail: None,
                    width: Some(800),
                    height: Some(600),
                },
            }],
        );
        let with_text = counter.count_message(&msg, None);
        let text_only = counter.count_message(&ChatMessage::user(text), None);
        assert!(
            with_text >= text_only,
            "image turn ({with_text}) must not cost less than the same text alone ({text_only})"
        );
    }


    #[test]
    fn test_count_text_default_ratio() {
        let counter = TokenCounter::new();
        let text = "Hello, how are you today?";
        let chars = text.len() as f64;
        let ratio = 3.5;
        let expected = (chars / ratio).ceil() as u64;
        let count = counter.count_text(text);
        assert_eq!(count, expected);
    }

    #[test]
    fn test_count_text_calibrated_ratio() {
        let mut counter = TokenCounter::new();
        counter.set_ratio(4.0);
        let text = "the quick brown fox jumps over the lazy dog";
        // 43 chars / 4.0 = 10.75 → ceil → 11
        let count = counter.count_text(text);
        assert_eq!(count, 11);
    }

    #[test]
    fn test_ratio_clamped_to_valid_range() {
        let mut counter = TokenCounter::new();
        counter.set_ratio(0.1); // below RATIO_MIN
        assert_eq!(counter.ratio(), 1.0);
        counter.set_ratio(99.0); // above RATIO_MAX
        assert_eq!(counter.ratio(), 10.0);
        assert!(counter.is_calibrated());
    }

    #[test]
    fn test_count_text_cjk() {
        let counter = TokenCounter::new();
        let text = "你好世界，今天天气不错";
        let count = counter.count_text(text);
        assert!(
            count >= 3,
            "Expected at least 3 tokens for CJK text, got {count}"
        );
    }

    #[test]
    fn test_count_text_mixed() {
        let counter = TokenCounter::new();
        let text = "Hello 你好 world 世界";
        let count = counter.count_text(text);
        assert!(count >= 3, "Expected at least 3 tokens, got {count}");
    }

    #[test]
    fn test_count_message_basic() {
        let counter = TokenCounter::new();
        let msg = ChatMessage::user("Hello world");
        let count = counter.count_message(&msg, None);
        // content tokens + role overhead + boundary
        assert!(count >= 3, "Expected at least 3 tokens, got {count}");
    }

    #[test]
    fn test_count_message_with_name() {
        let counter = TokenCounter::new();
        let msg = ChatMessage {
            role: acowork_core::providers::traits::MessageRole::User,
            content: "Hello".to_string(),
            name: Some("Alice".to_string()),
            ..Default::default()
        };
        let count_without_name = counter.count_text("Hello") + 2; // role + boundary
        let count_with_name = counter.count_message(&msg, None);
        assert!(
            count_with_name > count_without_name,
            "Named message should have more tokens"
        );
    }

    #[test]
    fn test_count_message_with_tool_calls() {
        let counter = TokenCounter::new();
        let msg = ChatMessage::assistant_with_tools(
            "",
            vec![ToolCall {
                id: "call_123".to_string(),
                call_type: "function".to_string(),
                function: FunctionCall {
                    name: "weather".to_string(),
                    arguments: r#"{"city":"Shanghai"}"#.to_string(),
                },
            }],
        );
        let count = counter.count_message(&msg, None);
        // Tool call overhead (4) + name + arguments + role + boundary
        assert!(
            count >= 6,
            "Expected at least 6 tokens for tool call message, got {count}"
        );
    }

    #[test]
    fn test_count_text_empty() {
        let counter = TokenCounter::new();
        assert_eq!(counter.count_text(""), 0);
    }

    #[test]
    fn test_count_text_single_char() {
        let counter = TokenCounter::new();
        let count = counter.count_text("a");
        assert!(count >= 1);
    }
}

// ── ADR-103 S1: Responses image-token formula ─────────────────────────

#[cfg(test)]
mod adr103_counter_tests {
    use super::estimate_image_tokens;
    use acowork_core::protocol::ProtocolType;

    #[test]
    fn responses_uses_the_openai_formula() {
        // §3: Responses bills images with the same tokenization as Chat
        // Completions — the two must never diverge.
        for (w, h, detail) in [
            (Some(1024u32), Some(1024u32), None),
            (Some(512), Some(512), Some("high")),
            (Some(2000), Some(300), Some("auto")),
            (None, None, Some("low")),
            (Some(100), Some(50), Some("low")),
        ] {
            let openai = estimate_image_tokens(&ProtocolType::OpenAI, w, h, detail);
            let responses = estimate_image_tokens(&ProtocolType::Responses, w, h, detail);
            let unknown = estimate_image_tokens(&ProtocolType::Unknown, w, h, detail);
            assert_eq!(openai, responses, "w={w:?} h={h:?} detail={detail:?}");
            assert_eq!(openai, unknown, "Unknown falls back to OpenAI behavior");
        }
        // Spot-check the canonical numbers: 1024x1024 high = 85 + 170*4.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::Responses, Some(1024), Some(1024), Some("high")),
            85 + 170 * 4
        );
        assert_eq!(
            estimate_image_tokens(&ProtocolType::Responses, Some(1024), Some(1024), Some("low")),
            85
        );
    }
}
