//! Retrieval policy loading (ADR-095 D3, design 28 §3.2).
//!
//! Owns **configuration装载 only** — it reads the agent's `search` section,
//! applies defaults, and produces the [`FusionPolicy`] values the engine
//! consumes. The policy *types* live in `acowork-core::retrieval` because
//! `acowork-memory`, `acowork-sqlite` and the evaluation harness all need the
//! same vocabulary; this module is the one place that decides what the values
//! are for this agent.
//!
//! Weights are rank-fusion weights, not probabilities. Nothing here is
//! calibrated yet: the numbers are the ADR-specified starting points, and
//! ADR-095 D6 requires the Phase-0 evaluation set before any of them changes.

use acowork_core::retrieval::FusionPolicy;
use serde::{Deserialize, Serialize};

use crate::search::analyzer::{AnalyzerConfig, AnalyzerMode, CjkStrategy};

/// The agent's `search` configuration section (`agent_config.json`).
///
/// Every field is optional: an agent written before ADR-095 has no `search`
/// section at all and must keep today's behavior exactly, which is what
/// [`Default`] encodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SearchPolicyConfig {
    /// `off` | `shadow` | `on`. Default `off` — the analyzer is inert until it
    /// is deliberately enabled, so shipping the code does not change retrieval.
    #[serde(default)]
    pub analyzer_mode: Option<AnalyzerMode>,
    /// CJK term strategy for the analyzer. Default `keep` (zero regression).
    #[serde(default)]
    pub cjk_strategy: Option<CjkStrategy>,
    /// Memory-scope fusion weights. `None` = [`FusionPolicy::SEMANTIC_4TO1`].
    ///
    /// Exposed so the Phase-4 threshold sweep can vary knobs without a code
    /// change; the values are **not** authorized for production use until the
    /// evaluation set supports them (D6).
    #[serde(default)]
    pub memory: Option<PolicyOverrides>,
    /// Conversation-scope fusion weights. `None` =
    /// [`FusionPolicy::CONVERSATION_BASELINE`].
    #[serde(default)]
    pub conversation: Option<PolicyOverrides>,
}

/// Per-scope overrides of the three knobs that calibration is expected to move.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PolicyOverrides {
    #[serde(default)]
    pub vector_weight: Option<f64>,
    #[serde(default)]
    pub text_weight: Option<f64>,
    /// Cosine floor for the vector source. Absent = the scope's own default
    /// (memory: none; conversation: 0.3). Setting this for memory before the
    /// evaluation set justifies it is explicitly out of scope (ADR-095 §2.2).
    #[serde(default)]
    pub min_cosine: Option<f32>,
    #[serde(default)]
    pub pool_factor: Option<usize>,
}

impl SearchPolicyConfig {
    /// The analyzer mode in effect. `None` = `Off`.
    pub fn analyzer_mode(&self) -> AnalyzerMode {
        self.analyzer_mode.unwrap_or(AnalyzerMode::Off)
    }

    /// Analyzer bounds for this agent, with the configured CJK strategy.
    pub fn analyzer_config(&self) -> AnalyzerConfig {
        AnalyzerConfig {
            cjk: self.cjk_strategy.unwrap_or_default(),
            ..AnalyzerConfig::default()
        }
    }

    /// Fusion policy for the memory scope of global search.
    ///
    /// Starts from `SEMANTIC_4TO1`, the same vector:text tendency
    /// `memory_recall` has run in production (D3), with **no cosine floor** —
    /// D6 forbids inventing one before it is measured.
    pub fn memory_policy(&self) -> FusionPolicy {
        self.apply(FusionPolicy::SEMANTIC_4TO1, self.memory)
    }

    /// Fusion policy for the conversation scope.
    ///
    /// Starts from `CONVERSATION_BASELINE` (equal weight + the existing 0.3
    /// floor), because ADR-095 §9.3 leaves the conversation weights to
    /// evaluation — message text has a different FTS distribution from memory
    /// content, so the 4:1 ratio is not assumed to transfer.
    pub fn conversation_policy(&self) -> FusionPolicy {
        self.apply(FusionPolicy::CONVERSATION_BASELINE, self.conversation)
    }

    fn apply(&self, base: FusionPolicy, o: Option<PolicyOverrides>) -> FusionPolicy {
        let Some(o) = o else { return base };
        FusionPolicy {
            vector_weight: o.vector_weight.unwrap_or(base.vector_weight),
            text_weight: o.text_weight.unwrap_or(base.text_weight),
            min_cosine: o.min_cosine.or(base.min_cosine),
            pool_factor: o.pool_factor.unwrap_or(base.pool_factor),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_section_is_the_current_production_behavior() {
        // The rollback guarantee: an agent with no `search` config must not
        // have its retrieval changed by this code existing.
        let c = SearchPolicyConfig::default();
        assert_eq!(c.analyzer_mode(), AnalyzerMode::Off);
        assert_eq!(c.memory_policy(), FusionPolicy::SEMANTIC_4TO1);
        assert_eq!(c.conversation_policy(), FusionPolicy::CONVERSATION_BASELINE);
    }

    #[test]
    fn memory_policy_carries_no_cosine_floor() {
        // ADR-095 §2.2 / D6: no memory cosine floor until calibrated.
        assert_eq!(SearchPolicyConfig::default().memory_policy().min_cosine, None);
    }

    #[test]
    fn conversation_policy_keeps_the_existing_floor() {
        assert_eq!(
            SearchPolicyConfig::default().conversation_policy().min_cosine,
            Some(0.3)
        );
    }

    #[test]
    fn overrides_are_per_field_and_do_not_disturb_the_rest() {
        let c = SearchPolicyConfig {
            memory: Some(PolicyOverrides {
                vector_weight: Some(0.5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let p = c.memory_policy();
        assert_eq!(p.vector_weight, 0.5);
        assert_eq!(p.text_weight, FusionPolicy::SEMANTIC_4TO1.text_weight);
        assert_eq!(p.min_cosine, None);
        // The other scope is untouched.
        assert_eq!(c.conversation_policy(), FusionPolicy::CONVERSATION_BASELINE);
    }

    #[test]
    fn json_shape_is_all_optional() {
        // `{}` and a missing section must parse identically — old
        // `agent_config.json` files have neither key.
        let empty: SearchPolicyConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, SearchPolicyConfig::default());
        let full: SearchPolicyConfig =
            serde_json::from_str(r#"{"analyzer_mode":"shadow","cjk_strategy":"trigram"}"#).unwrap();
        assert_eq!(full.analyzer_mode(), AnalyzerMode::Shadow);
        assert_eq!(full.analyzer_config().cjk, CjkStrategy::Trigram);
    }

    #[test]
    fn unknown_analyzer_mode_is_a_parse_error_not_a_silent_default() {
        // A typo must not silently disable the analyzer.
        let bad: Result<SearchPolicyConfig, _> =
            serde_json::from_str(r#"{"analyzer_mode":"enabled"}"#);
        assert!(bad.is_err());
    }
}
