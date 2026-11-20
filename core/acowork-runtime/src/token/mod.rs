//! Token counting module
//!
//! Session-scoped chars/token ratio for token estimation:
//! `tokens ≈ chars / ratio`. The ratio is the last value calibrated from
//! LLM API feedback (`ratio = input_chars / prompt_tokens`, same source)
//! and is persisted in the session meta — not a per-model table, because
//! multi-language input has no stable per-model constant.
//!
//! # Unified API
//!
//! **All** token counting in ACowork MUST go through [`count_text`].
//! Do NOT use `content.len() / 4` or any other ad-hoc heuristic —
//! they cause the debug panel and status panel to show contradictory numbers.
pub mod counter;

pub use counter::{TokenCounter, estimate_image_tokens, provider_input_dimensions};

/// The single unified entry point for token counting in ACowork.
///
/// Uses the default chars/token ratio 3.5 (no session context is available
/// in this free function; session-scoped counting goes through
/// `HistoryManager` / `TokenCounter`):
/// - `tokens = ceil(chars / ratio)`
///
/// # Why a unified API matters
///
/// Before this function existed, token counting was scattered across:
/// - `content.len() / 4` in debug panel → overestimates Chinese text by ~2.9x
/// - `chars / 3.5` in context builder safety checks → inconsistent with debug
/// - `TokenCounter::count_text()` in history manager → the only correct path
///
/// Two different numbers displayed to the user for the same session is a UX bug.
/// This function ensures **one source of truth** for all token estimates.
pub fn count_text(text: &str) -> usize {
    TokenCounter::new().count_text(text) as usize
}
