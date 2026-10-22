//! ADR-061 context compression - shared constants and budget validation.
//!
//! The 8-level degradation strategy (replacing "keep last N rounds") is
//! driven by these constants. Source of truth: docs/adr/zh/ADR-061 §19
//! (2026-08-30 finalized revision).

use acowork_core::protocol::ModelCapabilitiesInfo;

use crate::error::RuntimeError;

/// Minimum compression ratio for levels 1-7 (the target bar).
///
/// A level is only selected when it removes >= `MIN_COMPRESSION_RATIO` of
/// the history tokens — below that the cache invalidation is not worth the
/// summary cost (ADR-061 §3.3 break-even analysis). Level 8 is exempt
/// (see below).
///
/// Default is 0.90 = "compress until at most 10% remains" (e.g. 200K →
/// 20K). This is the **default** for the per-agent
/// `AgentConfig::compression_ratio_threshold` (agent_config.json / Agent
/// Setup panel); a user override replaces it at plan time.
pub(crate) const MIN_COMPRESSION_RATIO: f64 = 0.90;

/// Maximum output budget for the LLM compaction summary.
///
/// Replaces the former hardcoded `2048` in `compact_via_llm`. With the
/// mandatory two-section summary (`<summary>` / `<user_intent>`,
/// ADR-061 §8.1; `<triples>` was removed in ADR-057) 2K is too tight;
/// 4K leaves room for the full user-intent list without truncating
/// `</user_intent>`.
pub(crate) const SUMMARY_TOKEN_BUDGET: u64 = 4_096;

/// Model rejection line: agents require an effective input budget of at
/// least 64K to run the 8-level compression loop meaningfully.
///
/// Below this, level 8 (system + summary + current user) dominates the
/// context and the mechanism degenerates (ADR-061 §19.3). 128K/200K/1M
/// mainstream models all pass (128K - 16K output = 112K >= 64K).
pub(crate) const MIN_BUDGET_FOR_AGENT: u64 = 65_536;

/// Output-token reserve used when validating a model's effective input
/// budget (mirrors `max_output_tokens_limit`'s default of 16K = 1024×16).
pub(crate) const DEFAULT_OUTPUT_RESERVE: u64 = 16_384;

/// Share of the usable input budget that is actually available for content,
/// the rest being reserved for the model's next response, tool-call metadata
/// and message-serialization overhead.
///
/// Previously an inline `0.85` in `trim_tool_results_for_context`. Named and
/// shared now because skill recall computes its headroom the same way, and
/// the two MUST agree on what "free space" means or the two admission-control
/// paths silently fight over the same bytes.
pub(crate) const CONTEXT_SAFETY_FACTOR: f64 = 0.85;

// ── ADR-094 follow-up: how much skill body may survive a compaction ────
//
// Both terms are evaluated against the *post-compression* headroom and the
// smaller wins. Rationale for the two-sided form:
//
//   * the adaptive term is the useful one when the window is tight — it
//     scales the recall down instead of letting preserved SKILL.md bodies
//     push the session back over the compaction line;
//   * the ceiling is the useful one when the window is roomy. Right after a
//     default compaction the history sits near 10% of the budget, so "half
//     of what is free" would be ~37% of the whole window spent on skill
//     bodies. Capacity is not the objective here — *stability* is: every
//     byte promised at compression N can be retracted at compression N+1
//     when the adaptive term tightens, and a retracted round cannot come
//     back (see `HistoryManager::apply_skill_recall_budget`). Keeping the
//     preserved set small and steady is what makes the recall predictable.
//
// `MIN_COMPRESSION_RATIO` is only the per-agent *default*
// (`AgentConfig::compression_ratio_threshold` overrides it), so a user who
// configures a weak ratio really can leave 70% of the window occupied —
// which is precisely the case the adaptive term exists for. Do not reason
// about this budget as if history always ends up at 10%.

/// Fraction of the free post-compression space that skill recall may claim.
pub(crate) const SKILL_RECALL_FREE_SHARE: f64 = 0.5;

/// Hard ceiling on skill recall, as a fraction of the usable input budget,
/// regardless of how much space is free.
pub(crate) const SKILL_RECALL_WINDOW_CEILING: f64 = 0.15;

/// Skill-recall token budget given the usable budget and the tokens still
/// occupied after compression.
///
/// `min(SKILL_RECALL_FREE_SHARE × free, SKILL_RECALL_WINDOW_CEILING ×
/// usable)`. Pure and side-effect-free so the policy is unit-testable without
/// driving a whole compaction.
pub(crate) fn skill_recall_budget_tokens(usable_budget: u64, occupied_tokens: u64) -> u64 {
    let safe = (usable_budget as f64 * CONTEXT_SAFETY_FACTOR) as u64;
    let free = safe.saturating_sub(occupied_tokens);
    let adaptive = (free as f64 * SKILL_RECALL_FREE_SHARE) as u64;
    let ceiling = (usable_budget as f64 * SKILL_RECALL_WINDOW_CEILING) as u64;
    adaptive.min(ceiling)
}

/// Validate that a model's effective input budget clears the agent
/// rejection line.
///
/// Called on `model_switch` (hard rejection) and session init (warning
/// only — see ADR-061 §13.4 implementation note).
pub(crate) fn validate_model_budget(
    caps: &ModelCapabilitiesInfo,
) -> std::result::Result<(), RuntimeError> {
    if caps.effective_input_budget(DEFAULT_OUTPUT_RESERVE) < MIN_BUDGET_FOR_AGENT {
        return Err(RuntimeError::UnsupportedModel(format!(
            "Model context window too small for agent loop: effective input budget {} < MIN_BUDGET_FOR_AGENT {}",
            caps.effective_input_budget(DEFAULT_OUTPUT_RESERVE),
            MIN_BUDGET_FOR_AGENT
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Post-compression the context is usually roomy, so the adaptive half of
    /// the free space is larger than the window share — the ceiling binds.
    /// This is the common case the design discussion called "大概率不会触发".
    #[test]
    fn skill_recall_budget_is_capped_by_the_window_share_when_roomy() {
        let usable = 100_000;
        let budget = skill_recall_budget_tokens(usable, 10_000);
        assert_eq!(
            budget,
            (usable as f64 * SKILL_RECALL_WINDOW_CEILING) as u64,
            "at 10% occupied, 0.5 x free (37.5k) exceeds the ceiling (15k) → ceiling wins"
        );
    }

    /// Once the context is more than ~55% of the safe window full the adaptive
    /// term takes over and the recall budget shrinks with the remaining space.
    #[test]
    fn skill_recall_budget_shrinks_adaptively_when_tight() {
        let usable = 100_000;
        let safe = (usable as f64 * CONTEXT_SAFETY_FACTOR) as u64;
        let occupied = safe - 4_000; // only 4k genuinely free
        let budget = skill_recall_budget_tokens(usable, occupied);
        assert_eq!(
            budget,
            (4_000_f64 * SKILL_RECALL_FREE_SHARE) as u64,
            "tight context: half of the free space, not the ceiling"
        );
        assert!(
            budget < skill_recall_budget_tokens(usable, 10_000),
            "a tighter context must buy a smaller recall"
        );
    }

    #[test]
    fn skill_recall_budget_is_zero_when_nothing_is_free() {
        let usable = 100_000;
        let safe = (usable as f64 * CONTEXT_SAFETY_FACTOR) as u64;
        assert_eq!(skill_recall_budget_tokens(usable, safe), 0);
        assert_eq!(
            skill_recall_budget_tokens(usable, safe + 50_000),
            0,
            "saturating: over-full context must not underflow"
        );
    }
}
