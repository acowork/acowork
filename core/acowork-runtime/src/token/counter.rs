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
//! - Document (PDF) token estimation per page

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

/// Anthropic scales the long side down to this before tokenizing: pixels above
/// it are delivered to nobody and billed to nobody.
pub const ANTHROPIC_IMAGE_MAX_SIDE: u32 = 1568;
/// Anthropic bills roughly one token per 750 *delivered* pixels.
const ANTHROPIC_PIXELS_PER_TOKEN: u64 = 750;

/// OpenAI's first step: scale the image down to fit this square.
pub const OPENAI_IMAGE_FIT_BOX: u32 = 2048;
/// OpenAI's second step: scale the *shortest* side to this — up **or** down.
/// The upscale is why a 300×300 thumbnail costs four tiles, not one.
pub const OPENAI_IMAGE_SHORT_SIDE: u32 = 768;
const OPENAI_IMAGE_TILE: u32 = 512;
const OPENAI_BASE_TOKENS: u64 = 85;
const OPENAI_TOKENS_PER_TILE: u64 = 170;

/// Gemini cuts an image into 768×768 tiles of 258 tokens each (one tile for
/// anything that fits inside a single tile).
pub const GEMINI_IMAGE_TILE: u32 = 768;
const GEMINI_TOKENS_PER_TILE: u64 = 258;

/// The dimensions the provider actually tokenizes, after its documented
/// downscale / upscale steps.
///
/// This is the number that has to feed the estimate: the raster we hand to
/// the wire is *not* what the provider bills. Pricing the pre-delivery raster
/// overstated a 4000×3000 `detail=original` image 6.5× on Anthropic (12.0M px
/// against the 1.84M px actually seen) and overstated a 1568-wide screenshot
/// 2.8× on OpenAI (which scales the shortest side to 768 before counting
/// 512-px tiles). A phantom overestimate like that is not cosmetic: the
/// compaction line is a percentage of the projection, so it fires on a
/// context that was never full and pays a full cache miss plus a lossy
/// summary for it.
pub fn provider_input_dimensions(
    protocol_type: &ProtocolType,
    width: u32,
    height: u32,
    detail: Option<&str>,
) -> (u32, u32) {
    let (w, h) = (width.max(1), height.max(1));

    match protocol_type {
        // ADR-103 §3: the Responses shape bills images with the same
        // tokenization as Chat Completions. `Unknown` is the read-side
        // tolerance value (falls back to OpenAI behavior, D3).
        ProtocolType::OpenAI | ProtocolType::Responses | ProtocolType::Unknown => {
            if detail == Some("low") {
                // Flat rate: the dimensions do not participate in the bill.
                return (w, h);
            }
            let (w, h) = fit_within(w, h, OPENAI_IMAGE_FIT_BOX);
            let (w, h) = scale_short_side_to(w, h, OPENAI_IMAGE_SHORT_SIDE);
            // The documented second step overshoots the fit box whenever the
            // aspect ratio is narrower than `2048:768` (≈2.67:1). Clamping
            // back keeps a paper-thin image from claiming an absurd share of
            // the budget on an unverifiable extrapolation of the formula.
            fit_within(w, h, OPENAI_IMAGE_FIT_BOX)
        }
        ProtocolType::Anthropic => scale_long_side_to(w, h, ANTHROPIC_IMAGE_MAX_SIDE),
        // No resize step is documented for the tile schemes: tiles are
        // counted on the delivered pixels.
        ProtocolType::Google | ProtocolType::Ollama => (w, h),
    }
}

/// Estimate the token count one image part will cost the provider.
///
/// The bill is `f(delivered dimensions)`, never `f(file dimensions)`:
/// [`provider_input_dimensions`] supplies the former, so the answer matches
/// what `usage.prompt_tokens` reports for the same image (within a few
/// percent, which `calibrate_from_usage` absorbs on the next turn).
///
/// When width/height are unknown (None), a 512×512 default is assumed — the
/// same best-effort rule the document arm uses. Callers that know the
/// delivered size (every path through `image_read`) must pass it.
pub fn estimate_image_tokens(
    protocol_type: &ProtocolType,
    width: Option<u32>,
    height: Option<u32>,
    detail: Option<&str>,
) -> u64 {
    let w = width.unwrap_or(512);
    let h = height.unwrap_or(512);
    let (dw, dh) = provider_input_dimensions(protocol_type, w, h, detail);
    let (dw, dh) = (u64::from(dw), u64::from(dh));

    match protocol_type {
        ProtocolType::OpenAI | ProtocolType::Responses | ProtocolType::Unknown => {
            if detail == Some("low") {
                return OPENAI_BASE_TOKENS;
            }
            let tiles = dw.div_ceil(u64::from(OPENAI_IMAGE_TILE))
                * dh.div_ceil(u64::from(OPENAI_IMAGE_TILE));
            OPENAI_BASE_TOKENS + OPENAI_TOKENS_PER_TILE * tiles
        }
        ProtocolType::Anthropic => (dw * dh) / ANTHROPIC_PIXELS_PER_TOKEN,
        ProtocolType::Google => {
            GEMINI_TOKENS_PER_TILE
                * dw.div_ceil(u64::from(GEMINI_IMAGE_TILE))
                * dh.div_ceil(u64::from(GEMINI_IMAGE_TILE))
        }
        // Unverified legacy fallback: no published tiling scheme to model, so
        // the pixel-to-token ratio stays as it was.
        ProtocolType::Ollama => (dw * dh) / 258,
    }
}

/// Scale uniformly so the long side becomes at most `max_side` (downscale only).
fn scale_long_side_to(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_side {
        return (w, h);
    }
    scale_uniform(w, h, f64::from(max_side) / f64::from(long))
}

/// Scale uniformly so the image fits inside a `box_side` square (downscale only).
fn fit_within(w: u32, h: u32, box_side: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= box_side {
        return (w, h);
    }
    scale_uniform(w, h, f64::from(box_side) / f64::from(long))
}

/// Scale uniformly so the shortest side becomes `target` (up or down).
fn scale_short_side_to(w: u32, h: u32, target: u32) -> (u32, u32) {
    let short = w.min(h);
    if short == target {
        return (w, h);
    }
    scale_uniform(w, h, f64::from(target) / f64::from(short))
}

/// Apply one uniform scale factor, rounding and never collapsing to zero.
fn scale_uniform(w: u32, h: u32, factor: f64) -> (u32, u32) {
    (
        ((f64::from(w) * factor).round() as u32).max(1),
        ((f64::from(h) * factor).round() as u32).max(1),
    )
}

// ── Document Token Estimation ──────────────────────────────────────

/// Tokens one inlined PDF page is estimated to cost.
///
/// Anthropic publishes a figure for PDF pages and bills by it; the other houses
/// that accept file input publish nothing comparable. One number for all of
/// them is the deliberate choice: this estimate feeds the context meter and the
/// compaction decision, where being roughly right everywhere beats being
/// exactly right on one provider.
pub const TOKENS_PER_PDF_PAGE: u64 = 1_500;

/// Estimate the token cost of an inlined document.
///
/// `None` page count counts as nothing — the same best-effort rule that makes
/// an image with no known protocol type uncounted. It is a real understatement,
/// which is why the derivation that produces document parts refuses to inline a
/// PDF whose page tree it cannot read rather than rely on this arm.
pub fn estimate_document_tokens(page_count: Option<u32>) -> u64 {
    page_count.map_or(0, |pages| TOKENS_PER_PDF_PAGE * u64::from(pages))
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
                        // Not gated on `protocol_type` the way an image is: a
                        // document's cost does not depend on a tiling scheme,
                        // and the meter has to see this weight on every path.
                        acowork_core::providers::traits::ContentPart::Document { document } => {
                            tokens += estimate_document_tokens(document.page_count);
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

    /// A 30-page PDF is not a rounding error: it is roughly 45k tokens, and the
    /// context meter and the compaction decision only see that weight if the
    /// document part is counted.
    #[test]
    fn document_turn_is_counted_by_its_page_count() {
        use acowork_core::providers::traits::{ChatMessage, ContentPart};
        let counter = TokenCounter::new();
        let text = "what does page 12 of this say?";

        let pages = |count: Option<u32>| {
            counter.count_message(
                &ChatMessage::user_multimodal(
                    text,
                    vec![ContentPart::document("JVBERi0=", "deck.pdf", count)],
                ),
                None,
            )
        };
        let text_only = counter.count_message(&ChatMessage::user(text), None);
        assert_eq!(
            pages(Some(30)) - text_only,
            30 * TOKENS_PER_PDF_PAGE,
            "the page count is the whole bill"
        );
        // An unknown page count is not charged — the same best-effort rule the
        // image path follows when no protocol type is known.
        assert_eq!(pages(None), text_only);
        assert_eq!(estimate_document_tokens(None), 0);
        // Independent of `protocol_type`, which the image arm needs.
        assert_eq!(
            counter.count_message(
                &ChatMessage::user_multimodal(
                    text,
                    vec![ContentPart::document("JVBERi0=", "deck.pdf", Some(4))]
                ),
                Some(&ProtocolType::Anthropic),
            ),
            pages(Some(4))
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
            estimate_image_tokens(
                &ProtocolType::Responses,
                Some(1024),
                Some(1024),
                Some("high")
            ),
            85 + 170 * 4
        );
        assert_eq!(
            estimate_image_tokens(
                &ProtocolType::Responses,
                Some(1024),
                Some(1024),
                Some("low")
            ),
            85
        );
    }
}
// ── Image preprocessing model: the estimate must match what is billed ──
//
// Regression guard for the phantom-overestimate bug: the old formulas were
// applied to the *pre-delivery* raster, so a 4000×3000 `detail=original`
// image was priced 6.5× its real cost on Anthropic and a 1568-wide screenshot
// 2.8× its real cost on OpenAI. That inflated the projection, fired compaction
// early, and cost a full prefix-cache miss plus a lossy summary each time.
#[cfg(test)]
mod image_preprocessing_model_tests {
    use super::{estimate_image_tokens, provider_input_dimensions};
    use acowork_core::protocol::ProtocolType;

    #[test]
    fn anthropic_sees_the_long_side_capped_raster() {
        // 12.0M px delivered as 1568×1176 → 1.84M px seen by the provider.
        assert_eq!(
            provider_input_dimensions(&ProtocolType::Anthropic, 4000, 3000, Some("original")),
            (1568, 1176)
        );
        // 1568×1176 = 1_843_968 px / 750 ≈ 2_458 tokens — not 16_000.
        assert_eq!(
            estimate_image_tokens(
                &ProtocolType::Anthropic,
                Some(4000),
                Some(3000),
                Some("original")
            ),
            2_458
        );
        // A raster already inside the ceiling is untouched.
        assert_eq!(
            estimate_image_tokens(
                &ProtocolType::Anthropic,
                Some(1568),
                Some(1176),
                Some("high")
            ),
            2_458
        );
    }

    #[test]
    fn openai_scales_the_short_side_before_counting_tiles() {
        // fit 2048 → 2048×1536, short side 1536 → 768 → 1024×768 → 2×2 tiles.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::OpenAI, Some(4000), Some(3000), Some("high")),
            85 + 170 * 4
        );
        // A 1568-wide screenshot is not 12 tiles once the shortest side is
        // scaled: it is 1024×768, the same 4 tiles.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::OpenAI, Some(1568), Some(1176), Some("high")),
            85 + 170 * 4
        );
        // The upscale direction: 300×300 becomes 768×768 → 4 tiles, so the old
        // single-tile answer understated it.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::OpenAI, Some(300), Some(300), Some("high")),
            85 + 170 * 4
        );
        // `low` stays a flat rate whatever the size.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::OpenAI, Some(4000), Some(3000), Some("low")),
            85
        );
    }

    #[test]
    fn a_paper_thin_image_cannot_claim_an_absurd_share() {
        // The documented short-side upscale would take 8000×10 to 614400×768.
        // Clamping back to the fit box keeps the bill at 4 tiles instead of
        // 2400 — an unverifiable extrapolation must not trigger compaction.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::OpenAI, Some(8000), Some(10), Some("high")),
            85 + 170 * 4
        );
    }

    #[test]
    fn gemini_prices_whole_tiles() {
        // 1568×1176 → 3×2 tiles of 258 tokens.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::Google, Some(1568), Some(1176), Some("high")),
            258 * 6
        );
        // Anything inside one tile is one tile — not 300*300/258 ≈ 348.
        assert_eq!(
            estimate_image_tokens(&ProtocolType::Google, Some(300), Some(300), Some("high")),
            258
        );
    }

    #[test]
    fn unknown_dimensions_stay_inside_one_delivery() {
        // No dimensions means "assume 512×512", and the answer must be in the
        // range a real 512×512 image costs on each protocol — not a
        // conservative number that happens to look safe but distorts the meter.
        for protocol in [
            ProtocolType::OpenAI,
            ProtocolType::Anthropic,
            ProtocolType::Google,
        ] {
            let unknown = estimate_image_tokens(&protocol, None, None, Some("high"));
            let known = estimate_image_tokens(&protocol, Some(512), Some(512), Some("high"));
            assert_eq!(unknown, known, "{protocol:?}");
        }
    }
}
