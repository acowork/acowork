//! Session config module (ADR-047).
//!
//! Decouples session config persistence from the LLM inference loop.
//! - `delta::SessionConfigDelta` -- partial config update, the single payload
//!   for all config mutations.
//! - `delta::SessionConfigSnapshot` -- read-only snapshot of current config.
//! - `llm_effects` -- deferred LLM-side effects applied at turn boundaries.
//!
//! ADR-074: `is_valid_context_window` / `resolve_effective_context_window`
//! are the single backend resolution point for the per-session context
//! window chain (session meta → agent_config → manifest → DEFAULT →
//! min(model window)).

pub mod delta;
pub mod llm_effects;

pub use delta::{SessionConfigDelta, SessionConfigSnapshot};

/// Lower bound of the valid context-window range (ADR-074 D6). Pure
/// anti-footgun floor; no per-model floor is enforced (values smaller
/// than the model's max output are naturally handled by min / trim).
pub const CONTEXT_WINDOW_FLOOR: u64 = 8_192;

/// Upper bound of the valid context-window range (ADR-074 D6). 4M tokens
/// is larger than any current model window — purely guards against a
/// hand-edited meta writing `u64::MAX` and producing astronomical budgets.
pub const CONTEXT_WINDOW_CEILING: u64 = 4_194_304;

/// Value validity — the single interpretation point for "invalid".
///
/// `0`, a missing value and an out-of-range value are ALL "invalid" and
/// mutually synonymous (ADR-074 §1.3): they mean "no override, fall to
/// the next layer". ADR-026's old `0 = 无限制` ("no limit") is abolished —
/// an unbounded budget is not a legal configuration (§6).
///
/// Used by BOTH the HTTP 400 validation (`put_session_config`) and the
/// resolution chain — the two call sites must never diverge (§3.2).
pub(crate) fn is_valid_context_window(n: u64) -> bool {
    (CONTEXT_WINDOW_FLOOR..=CONTEXT_WINDOW_CEILING).contains(&n)
}

/// Resolve the effective context window cap for a session (ADR-074 §3).
///
/// # Resolution chain (each layer takes the FIRST valid value)
///
/// ```text
/// Layer 0 (highest)  session meta.json context_window   (per-session override)
/// Layer 1            agent_config.json.context_window   (per-agent setting)
/// Layer 2            manifest.llm.context_window        (package author default)
/// Layer 3            DEFAULT_CONTEXT_WINDOW = 200_000   (hardcoded fallback
///                                                        = de-facto ceiling)
/// then: effective = min(resolved, model context_window)
/// ```
///
/// Invalid values (`missing / null / 0 / out-of-range`) at any layer fall
/// through to the next one. Out-of-range persisted values (old meta /
/// hand-edited files) are treated as invalid — never clamped, never an
/// error. The result is computed fresh on every call (never cached) so
/// agent-layer changes always reach sessions without an override, and is
/// never written back anywhere (§3.2).
pub(crate) fn resolve_effective_context_window(
    session_override: Option<u64>, // Layer 0 — from ConversationSession
    agent_override: Option<u64>,   // Layer 1 — AgentCore.context_window_override
    manifest_window: Option<u64>,  // Layer 2 — AgentCore.manifest_context_window
    model_caps: Option<&acowork_core::ModelCapabilitiesInfo>,
) -> u64 {
    let resolved = session_override
        .filter(|n| is_valid_context_window(*n))
        .or_else(|| agent_override.filter(|n| is_valid_context_window(*n)))
        .or_else(|| manifest_window.filter(|n| is_valid_context_window(*n)))
        .unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW);
    model_caps
        .map(|caps| resolved.min(caps.context_window))
        .unwrap_or(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::ModelCapabilitiesInfo;

    fn caps(window: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window: window,
            max_output_tokens: 8_192,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
            reasoning_efforts: None,
        }
    }

    // ADR-074 §8 test matrix A: exhaustive jump-layer + fallback coverage.
    #[test]
    fn layer3_fallback_then_min_with_model() {
        // session/agent/manifest all absent → 200K default, min(model 128K)
        let got = resolve_effective_context_window(None, None, None, Some(&caps(128_000)));
        assert_eq!(got, 128_000);
    }

    #[test]
    fn invalid_layer1_falls_to_layer2() {
        // agent Some(0) is invalid (old "no limit" sentinel) → fall to manifest 64K
        let got = resolve_effective_context_window(None, Some(0), Some(64_000), None);
        assert_eq!(got, 64_000);
    }

    #[test]
    fn top_layer_wins() {
        let got = resolve_effective_context_window(Some(96_000), Some(32_000), Some(64_000), None);
        assert_eq!(got, 96_000);
    }

    #[test]
    fn clear_is_invalid() {
        // session Some(0) = "cleared" = invalid → fall to agent 32K
        let got = resolve_effective_context_window(Some(0), Some(32_000), None, None);
        assert_eq!(got, 32_000);
    }

    #[test]
    fn cleared_session_falls_to_default_then_min_with_model() {
        // session 0 + no agent/manifest → 200K default, min(model 1M) = 200K
        let got = resolve_effective_context_window(Some(0), None, None, Some(&caps(1_000_000)));
        assert_eq!(got, crate::config::DEFAULT_CONTEXT_WINDOW);
    }

    #[test]
    fn out_of_range_layer0_is_invalid() {
        // session 5M is above CEILING → invalid → fall to agent 32K
        let got = resolve_effective_context_window(Some(5_000_000), Some(32_000), None, None);
        assert_eq!(got, 32_000);
    }

    #[test]
    fn no_model_caps_no_panic() {
        let got = resolve_effective_context_window(Some(96_000), Some(32_000), Some(64_000), None);
        assert_eq!(got, 96_000);
    }

    #[test]
    fn smaller_model_window_wins() {
        // model window 8K < resolved 96K → min = 8K
        let got =
            resolve_effective_context_window(Some(96_000), Some(32_000), None, Some(&caps(8_000)));
        assert_eq!(got, 8_000);
    }

    #[test]
    fn out_of_range_agent_layer_falls_to_manifest() {
        let got = resolve_effective_context_window(None, Some(10), Some(64_000), None);
        assert_eq!(got, 64_000);
    }

    #[test]
    fn below_floor_is_invalid() {
        let got = resolve_effective_context_window(Some(1_000), Some(32_000), None, None);
        assert_eq!(got, 32_000);
    }

    #[test]
    fn validity_bounds() {
        assert!(!is_valid_context_window(0));
        assert!(!is_valid_context_window(CONTEXT_WINDOW_FLOOR - 1));
        assert!(is_valid_context_window(CONTEXT_WINDOW_FLOOR));
        assert!(is_valid_context_window(CONTEXT_WINDOW_CEILING));
        assert!(!is_valid_context_window(CONTEXT_WINDOW_CEILING + 1));
        assert!(!is_valid_context_window(u64::MAX));
    }
}
