//! Context management for the AgentLoop.
//!
//! Extracted from loop_.rs (ADR-014 Phase 1).
//! Contains all methods related to context window management:
//! - Token budget calculation
//! - History trimming (5-level compression)
//! - LLM-based context compaction
//! - Model resolution for compaction/distillation
//! - Runtime config application (affects context limits)

use std::sync::Arc;

use acowork_core::providers::traits::Provider;

use crate::agent::compression_constants::{
    CONTEXT_SAFETY_FACTOR, MIN_COMPRESSION_RATIO, skill_recall_budget_tokens,
};
use crate::agent::context::{
    ContextBuilder, count_chat_request_chars, patch_agent_totals, patch_perf, patch_session_totals,
};
use crate::agent::loop_::{AgentLoop, ChunkEvent, CompactionCancelReason};
use crate::agent::session::session_manager::RuntimeConfigOverrides;

// ── Context compression thresholds ─────────────────────────────────────
// All *compression* percentages below are relative to the **effective
// usable input budget** (`AgentCore::context_trim_budget`, the single
// denominator: the resolved total context cap minus the model's output
// reserve — e.g. 250K cap + 32K output = 218K input budget).
//
// This is deliberately SEPARATE from the UI-facing `usage_percent` pushed in
// ContextUsageInfo, which the frontend reads as "used (input + output) /
// window total" (250K). The two never move together — see
// `compute_context_usage` in context.rs.
//
// ADR-011 tier scheme (revised 2026-09: FIFO/emergency trim deleted by
// ADR-061, so the early tiers lost their original purpose):
//   WARN      → informational log only (70%). No action — the old 70%
//               pre-trim FORCED a full LLM summary before appending tool
//               results; with the 5-level plan (which folds/trims tool
//               results progressively and guarantees a fit at level 5) that
//               early compaction was premature and is removed.
//   COMPACT   → the single automatic LLM compaction line (90%). Fired
//               pre-request and before appending tool results, measured on
//               the PROJECTED next-request input tokens (last API
//               `prompt_tokens` anchor + growth since) instead of raw
//               history tokens.
//   CRITICAL  → post-compaction sanity log only (95%) — kept as
//               documentation of the deleted emergency-trim safety net.
pub(crate) const CONTEXT_WARN_PERCENT: f64 = 70.0;
pub(crate) const CONTEXT_COMPACT_PERCENT: f64 = 90.0;
pub(crate) const CONTEXT_CRITICAL_PERCENT: f64 = 95.0;

// ── ADR-056: Distillation resolution types ────────────────────────

/// ADR-056: Resolution tier for a chosen distillation target.
///
/// Logged on every compaction call so it's easy to verify in production
/// which fallback chain fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DistillTier {
    /// Level 1: `AgentCore.default_compact_models` — the user's ordered
    /// global picks across all configured providers (each expansion in the
    /// call-phase chain carries this tier).
    GlobalDefault,
    /// Level 2: `ProviderListItem.compact_model` of the session's current
    /// provider (the pre-ADR-056 behaviour).
    ProviderCompact,
    /// Level 3: the session's current chat model.
    CurrentChat,
}

/// ADR-056: Result of resolving the (provider, model) target for a
/// distillation / compaction call. Carries the tier so the caller can
/// log it and pick the right `Provider` instance (cross-provider
/// distillation requires `SessionCore::build_provider_for`, while
/// `ProviderCompact` / `CurrentChat` can reuse `self.core.provider`).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedDistill {
    pub provider_id: String,
    pub model_id: String,
    pub tier: DistillTier,
}

impl AgentLoop {
    /// Update the LLM provider at runtime (e.g., after a `ModelSwitch`
    /// message rebuilds the Provider from the global cache).
    ///
    /// The current provider_id is tracked in `SessionState.provider`,
    /// which the ModelSwitch handler updates before invoking this method.
    /// At distillation time, `resolve_distill_model` looks up the
    /// compact_model via `self.session.provider()`.
    pub fn update_provider(
        &mut self,
        new_provider: Arc<dyn Provider>,
        model: String,
        provider_id: Option<String>,
    ) {
        // `self.core` is an owned `AgentCore` (per-session clone), so we
        // can mutate it directly. The optional `provider_id` is kept as
        // a parameter for caller compatibility but is now persisted on
        // `SessionState.provider` instead of on `AgentCore`.
        self.core.update_provider(new_provider, model);
        let _ = provider_id;
    }

    /// Apply runtime config overrides from Gateway.
    pub fn apply_runtime_config(&mut self, overrides: &RuntimeConfigOverrides) {
        self.core.apply_runtime_config(overrides);
        // Sync the session's cached temperature so emit_session_state()
        // reflects the new override immediately.  Without this,
        // session.temperature() returns the old resolved value (always
        // Some), blocking the fallback chain from reaching the new
        // temperature_override.
        if overrides.temperature.is_some() {
            self.session.set_temperature(overrides.temperature);
        }

        // If context_window changed, push updated context_usage immediately
        // so the frontend status panel and context-usage popup reflect the
        // new cap without waiting for the next LLM response.
        if overrides.context_window.is_some() {
            let model_name = self.resolve_current_model(None);
            if let Some((caps, effective_window, effective_usable)) =
                self.effective_context_budget(&model_name)
            {
                let total_tokens = self.session.history.token_count();
                // UI percent denominator = window TOTAL (incl. output), not
                // the input budget — see compute_context_usage doc. The
                // compaction threshold lives on projected input vs
                // effective_usable and is NOT shown here.
                let percent = if effective_window > 0 {
                    ((total_tokens as f64 / effective_window as f64) * 100.0).min(100.0) as u8
                } else {
                    0
                };
                // Pull cumulative session totals (in/out + cache) so the frontend
                // status panel can render session-level Total Input /
                // Total Output / Cache Read / Cache Write alongside the
                // per-turn figures.  ADR-066 §2 routes all four cumulative
                // session fields through `patch_session_totals`.
                let session_tokens = self.session.conversation.as_ref().and_then(|c| c.tokens());
                let ctx_info = acowork_core::protocol::ContextUsageInfo {
                    context_window: effective_window,
                    input_tokens: total_tokens,
                    output_tokens: 0,
                    total_tokens,
                    max_input_tokens: caps.max_input_tokens,
                    usable_context: effective_usable,
                    usage_percent: percent,
                    // Cumulative session fields default to None —
                    // patched below via the shared helper.
                    total_input_tokens: None,
                    total_output_tokens: None,
                    // ADR-028 / ADR-066: agent-scoped cumulative tokens + cache
                    // totals are snapshotted from AtomicU64 counters below.
                    agent_total_input_tokens: None,
                    agent_total_output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    total_cache_read_tokens: None,
                    total_cache_write_tokens: None,
                    agent_total_cache_read_tokens: None,
                    agent_total_cache_write_tokens: None,
                    sections: None,
                    // No LLM call here — context_window override only.
                    iteration: None,
                    avg_tps: None,
                    avg_ttft_ms: None,
                };
                tracing::info!(
                    context_window = effective_window,
                    total_tokens,
                    usage_percent = percent,
                    "Pushing immediate context_usage after context_window config change"
                );
                // ADR-028 + ADR-066: snapshot the agent-scoped counters and
                // patch the session totals in one place so the three push
                // paths can never drift.
                let mut ctx_info = ctx_info;
                if let Some(t) = session_tokens {
                    patch_session_totals(&mut ctx_info, &t);
                }
                // ADR-028 / ADR-066: same centralised patcher as the live
                // LLM-call push, so the config-change and post-compaction
                // pushes never drift from the main path either.
                patch_perf(
                    &mut ctx_info,
                    self.session.conversation.as_ref().and_then(|c| c.perf()).as_ref(),
                );
                patch_agent_totals(&mut ctx_info, self.core.agent_token_totals());
                let _ = self
                    .session_core
                    .try_send_chunk(ChunkEvent::ContextUsage(ctx_info));
            }
        }
    }

    /// Get the context window budget for history trimming.
    ///
    /// Resolves the effective budget through the ADR-074 chain — session
    /// meta override (Layer 0) → agent_config (Layer 1) → manifest
    /// (Layer 2) → DEFAULT_CONTEXT_WINDOW (Layer 3) → min(model window) —
    /// then clamps to the model's actual context window capacity.
    /// Falls back to config.history_max_tokens when model capabilities
    /// are unavailable.
    pub(crate) fn context_trim_budget(&self, model_name: &str) -> u64 {
        let resolved = self.resolved_effective_context_window(model_name);
        self.core
            .context_trim_budget_with(Some(resolved), model_name)
    }

    /// ADR-074: resolve this session's effective context window through the
    /// single pure resolution point (§3.2). Layer 0 (session meta override)
    /// is read from `ConversationSession` — the same in-memory mirror of the
    /// persisted value — and computed fresh on every call (never cached), so
    /// agent-layer changes always reach sessions without an override.
    fn resolved_effective_context_window(&self, model_name: &str) -> u64 {
        let session_override = self.session.conversation().and_then(|c| c.context_window());
        let caps = self.core.get_model_capabilities(model_name);
        crate::agent::session_config::resolve_effective_context_window(
            session_override,
            self.core.context_window_override,
            self.core.manifest_context_window,
            caps.as_ref(),
        )
    }

    /// Resolve the effective (clamped) context budget for **display** and
    /// threshold checks in this file.
    ///
    /// Returns `(caps, effective_window, effective_usable)` if model
    /// capabilities are available, otherwise `None`.
    ///
    /// `effective_window` is the user-facing **total context length** shown
    /// in the UI: the ADR-074 resolved cap (session meta → agent_config →
    /// manifest → DEFAULT_CONTEXT_WINDOW) already clamped to the model
    /// window, so a 250K setting stays 250K on screen (never 218K). It is
    /// also the display percent denominator ("used incl. output / window
    /// total").
    /// `effective_usable` is the runtime input budget from
    /// [`Self::context_trim_budget`] (resolved cap − output reserve) — the
    /// single denominator for the compaction thresholds only.
    pub(crate) fn effective_context_budget(
        &self,
        model_name: &str,
    ) -> Option<(acowork_core::protocol::ModelCapabilitiesInfo, u64, u64)> {
        let caps = self.core.get_model_capabilities(model_name)?;
        // Display window follows the SAME ADR-074 resolution chain as the
        // trim budget (session meta → agent_config → manifest →
        // DEFAULT_CONTEXT_WINDOW → min(model window)), so the frontend
        // shows the session-effective "context length" (e.g. 96K) instead
        // of the model's raw window (1M) when a cap is configured.
        let effective_window = self.resolved_effective_context_window(model_name);
        let effective_usable = self
            .core
            .context_trim_budget_with(Some(effective_window), model_name);
        Some((caps, effective_window, effective_usable))
    }

    /// Projected size of the NEXT chat-request input, in API-counted tokens.
    ///
    /// `history.token_count()` is anchored to the last reliable API
    /// `prompt_tokens` (the whole request — history + system prompt + tool
    /// schemas — as the provider counts it, see
    /// [`Self::process_llm_response_usage`]), so it already is what the
    /// provider will count; newly appended messages are estimated on top
    /// as `chars / ratio`. No separate "overhead" term exists.
    pub(crate) fn projected_input_tokens(&self) -> u64 {
        self.session.history.token_count()
    }

    /// Projected input INCLUDING not-yet-appended tool results (measured
    /// before they land in history).
    pub(crate) fn projected_input_tokens_with(&self, pending_extra_tokens: u64) -> u64 {
        self.projected_input_tokens()
            .saturating_add(pending_extra_tokens)
    }

    /// Absolute token count at which compaction fires: `CONTEXT_COMPACT_PERCENT`
    /// (90%) of the single input budget. When this is crossed the agent runs
    /// the 5-level plan BEFORE the next LLM request.
    pub(crate) fn compact_threshold_tokens(&self, model_name: &str) -> u64 {
        let budget = self.context_trim_budget(model_name);
        ((budget as f64 * (CONTEXT_COMPACT_PERCENT / 100.0)) as u64).min(budget)
    }

    /// Whether the projected next-input is at/over the compaction line.
    pub(crate) fn projected_exceeds_compact_threshold(
        &self,
        model_name: &str,
        pending_extra_tokens: u64,
    ) -> bool {
        self.projected_input_tokens_with(pending_extra_tokens)
            >= self.compact_threshold_tokens(model_name)
    }

    /// Trim history to fit within the input budget.
    ///
    /// The budget comes from [`context_trim_budget`] — the resolved total
    /// context cap minus the model's output reserve (250K cap + 32K output
    /// → 218K input). `HistoryManager::max_tokens` is synced to the same
    /// value so downstream token accounting uses the correct threshold.
    ///
    /// ADR-061 §11.2: when history exceeds the budget, the ADR-061 v3
    /// 5-level compression plan is forced (level 5 is guaranteed to fit
    /// once the summary size is known — §19.5). Failure is explicit: the
    /// error propagates so the agent loop stops instead of continuing to
    /// grow the context (§11.3) — `history` is left untouched.
    ///
    /// When history is already within budget this is a no-op.
    pub(crate) async fn trim_history_to_budget(
        &mut self,
        model_name: &str,
    ) -> crate::error::Result<()> {
        let budget = self.context_trim_budget(model_name);

        // Sync HistoryManager::max_tokens to the actual model budget so
        // downstream token accounting uses the correct threshold. Without
        // this, max_tokens remains at the static config default (128K)
        // even after model switch, capabilities update, or
        // max_output_tokens change.
        self.session.history.set_max_tokens(budget);

        if self.session.history.token_count() > budget {
            self.compact_history_if_needed(model_name, true).await?;
        }
        Ok(())
    }

    /// Resolve the model to use for session distillation or compaction.
    ///
    /// Uses [`crate::token::count_text`] — the single unified token counting API.
    ///
    /// Priority order:
    /// 1. Provider's configured `compact_model` from provider_list (read from disk)
    /// 2. Current model (fallback when compact model unavailable or context too small)
    pub(crate) fn resolve_distill_model(&self, content_text: &str) -> ResolvedDistill {
        let current_model = self.resolve_current_model(None);
        let session_pid = self
            .session
            .provider()
            .map(str::to_string)
            .unwrap_or_default();

        let estimated_tokens = crate::token::count_text(content_text) as u64;

        // ── Level 1: global compact-model candidate list (ordered) ─────
        // Walk the user's list in priority order; the first candidate that
        // passes availability + context-window checks wins. Candidates that
        // fail are logged and skipped — the *call-phase* chain
        // (`resolve_distill_targets`) still includes the skipped-but-
        // available ones via `try_global_default_targets`.
        let globals: Vec<(String, String)> =
            self.core.default_compact_models.read().unwrap().clone();
        for (pid, mid) in &globals {
            if !self.core.is_compact_provider_available(pid) {
                tracing::warn!(
                    provider_id = %pid,
                    model_id = %mid,
                    "Global compact candidate provider unavailable, trying next"
                );
                continue;
            }
            if let Some(cap) = self.core.get_model_capabilities(mid) {
                if cap.context_window < estimated_tokens {
                    tracing::warn!(
                        provider_id = %pid,
                        model_id = %mid,
                        context_window = cap.context_window,
                        estimated_tokens,
                        "Global compact candidate context_window too small, trying next"
                    );
                    continue;
                }
                tracing::info!(
                    provider_id = %pid,
                    model_id = %mid,
                    context_window = cap.context_window,
                    estimated_tokens,
                    tier = "global_default",
                    "Using global default compact model for distillation"
                );
                return ResolvedDistill {
                    provider_id: pid.clone(),
                    model_id: mid.clone(),
                    tier: DistillTier::GlobalDefault,
                };
            } else {
                tracing::warn!(
                    provider_id = %pid,
                    model_id = %mid,
                    "Global compact candidate not found in capabilities, trying next"
                );
            }
        }

        // ── Level 2: session provider's compact_model ──────────────────
        // Clone the map value out to an owned String so the RwLock guard
        // temp drops with the let-chain scrutinee instead of being held
        // across the block body.
        if !session_pid.is_empty()
            && let Some(compact) = self
                .core
                .provider_compact_models
                .read()
                .unwrap()
                .get(&session_pid)
                .and_then(|cm| cm.clone())
        {
            if let Some(cap) = self.core.get_model_capabilities(&compact) {
                if cap.context_window < estimated_tokens {
                    tracing::warn!(
                        provider_id = %session_pid,
                        model_id = %compact,
                        context_window = cap.context_window,
                        estimated_tokens,
                        "Provider compact model context_window too small, falling back to Level 3"
                    );
                } else {
                    tracing::info!(
                        provider_id = %session_pid,
                        model_id = %compact,
                        context_window = cap.context_window,
                        estimated_tokens,
                        tier = "provider_compact",
                        "Using provider's compact model for distillation"
                    );
                    return ResolvedDistill {
                        provider_id: session_pid.clone(),
                        model_id: compact.clone(),
                        tier: DistillTier::ProviderCompact,
                    };
                }
            } else {
                tracing::warn!(
                    provider_id = %session_pid,
                    model_id = %compact,
                    "Provider compact model not found in capabilities, falling back to Level 3"
                );
            }
        }

        // ── Level 3: current chat model ────────────────────────────────
        tracing::info!(
            provider_id = %session_pid,
            model_id = %current_model,
            estimated_tokens,
            tier = "current_chat",
            "Compact model not available or insufficient, using current model for distillation"
        );
        ResolvedDistill {
            provider_id: session_pid,
            model_id: current_model,
            tier: DistillTier::CurrentChat,
        }
    }

    /// ADR-056 follow-up: Return the ordered list of usable distillation
    /// targets for [`Self::compact_history_if_needed`].
    ///
    /// Differences from [`Self::resolve_distill_model`]:
    /// - Returns **all** candidates the current state can reach, in priority
    ///   order: `[GlobalDefault…] (the whole ordered global list),
    ///   ProviderCompact?, CurrentChat`.
    /// - `CurrentChat` is always present as the last-resort fallback so the
    ///   caller can guarantee at least one `distill_provider()` target.
    /// - Does **not** log per-tier "Using X for distillation" messages; the
    ///   caller logs the chosen tier after the LLM call returns.
    ///
    /// This is the input for the *call-phase* fallback chain: when a
    /// higher tier's LLM call fails (network, 4xx, 5xx, parse error), the
    /// caller iterates through `targets` until one succeeds, instead of
    /// falling through to `trim_fifo` + `emergency_trim` (which would
    /// destroy early context unconditionally).
    pub(crate) fn resolve_distill_targets(&self) -> Vec<ResolvedDistill> {
        // Reuse `resolve_distill_model` for the top-priority target — it
        // already encodes the same selection logic as the single-tier
        // resolver, including context-window and capability checks.
        let top = self.resolve_distill_model("");
        let mut out: Vec<ResolvedDistill> = Vec::new();

        // Global tier expands to the user's whole ordered list (filtered by
        // provider availability), not just the single head. `top` always
        // leads the chain; the remaining global candidates follow in the
        // user's order.
        //
        // When `top` is NOT a global pick, the selection phase rejected
        // every global candidate it evaluated — provider unavailable, or
        // model missing from `capabilities` — and picked a lower tier
        // instead. `try_global_default_targets` filters on provider
        // availability only, so it can still return a candidate the
        // selection phase threw out. Appending those after `top` (rather
        // than prepending) keeps the chain consistent with the tier
        // `resolve_distill_model` reported, while still letting a later
        // call failure fall through to another user pick — strictly
        // better than skipping it outright.
        out.push(top.clone());
        out.extend(self.try_global_default_targets());

        // Provider-compact tier: appended unless `top` already chose it.
        if !matches!(top.tier, DistillTier::ProviderCompact)
            && let Some(t) = self.try_provider_compact_target()
        {
            out.push(t);
        }

        // CurrentChat is the universal last-resort: always appended unless
        // `resolve_distill_model` already chose it as the top tier. This
        // guarantees the caller has at least one fallback even when both
        // GlobalDefault and ProviderCompact are absent.
        if !matches!(top.tier, DistillTier::CurrentChat) {
            let current_model = self.resolve_current_model(None);
            let session_pid = self
                .session
                .provider()
                .map(str::to_string)
                .unwrap_or_default();
            out.push(ResolvedDistill {
                provider_id: session_pid,
                model_id: current_model,
                tier: DistillTier::CurrentChat,
            });
        }

        // De-duplicate by (provider_id, model_id) preserving order — guards
        // against the degenerate case where two tiers resolve to the same
        // (provider, model) pair (e.g. GlobalDefault == CurrentChat, or the
        // top global appearing again inside `globals`).
        let mut deduped: Vec<ResolvedDistill> = Vec::with_capacity(out.len());
        for r in out {
            if !deduped
                .iter()
                .any(|d| d.provider_id == r.provider_id && d.model_id == r.model_id)
            {
                deduped.push(r);
            }
        }
        deduped
    }

    /// Probe whether the global default compact model can serve as a
    /// distillation target right now. Mirrors the selection logic in
    /// [`Self::resolve_distill_model`] but returns `None` instead of
    /// falling through.
    fn try_global_default_targets(&self) -> Vec<ResolvedDistill> {
        let globals: Vec<(String, String)> =
            self.core.default_compact_models.read().unwrap().clone();
        globals
            .into_iter()
            .filter(|(pid, _mid)| self.core.is_compact_provider_available(pid))
            // No token-estimated context_window check here: if a global
            // candidate's context_window is too small for the actual
            // content, the LLM call will still succeed (just produce a
            // short summary); the *call-phase* fallback only kicks in on
            // call failure. The selection phase already rejected this
            // candidate via `resolve_distill_model` when context_window <
            // estimated_tokens, so re-checking here would only produce a
            // different answer than the top-tier resolver — keep them
            // aligned.
            .map(|(pid, mid)| ResolvedDistill {
                provider_id: pid,
                model_id: mid,
                tier: DistillTier::GlobalDefault,
            })
            .collect()
    }

    /// Probe whether the session provider's `compact_model` is usable.
    fn try_provider_compact_target(&self) -> Option<ResolvedDistill> {
        let session_pid = self
            .session
            .provider()
            .map(str::to_string)
            .unwrap_or_default();
        if session_pid.is_empty() {
            return None;
        }
        let compact = self
            .core
            .provider_compact_models
            .read()
            .unwrap()
            .get(&session_pid)
            .and_then(|cm| cm.clone())?;
        Some(ResolvedDistill {
            provider_id: session_pid,
            model_id: compact,
            tier: DistillTier::ProviderCompact,
        })
    }

    /// ADR-056: Pick the `Provider` instance to use for a distillation /
    /// compaction LLM call given a resolved target, and return the
    /// `(provider, model_id, tier)` triple the caller must actually use.
    ///
    /// Levels 2 & 3 reuse the session's current provider (which matches the
    /// resolved model). Level 1 may live on a *different* provider entirely,
    /// so it is rebuilt via `SessionCore::build_provider_for` with the
    /// target's base_url + api_key from the provider list / key vault.
    ///
    /// When the target provider cannot be rebuilt (removed from the list
    /// between resolve and call), both the provider **and** the model are
    /// demoted to the session's current chat model (Level 3) — never use a
    /// cross-provider model_id with the session provider, the API would
    /// reject it.
    pub(crate) fn distill_provider(
        &self,
        resolved: &ResolvedDistill,
    ) -> (Arc<dyn Provider>, String, DistillTier) {
        let session_pid = self
            .session
            .provider()
            .map(str::to_string)
            .unwrap_or_default();

        let same_provider = resolved.provider_id == session_pid || resolved.provider_id.is_empty();
        if same_provider {
            return (
                self.core.provider.clone(),
                resolved.model_id.clone(),
                resolved.tier,
            );
        }

        match self.session_core.build_provider_for(
            &resolved.provider_id,
            None,
            &self.core.config,
            &self.core.global_provider_list,
            &self.core.provider_key_vault,
            self.core.compat_cache.as_ref(),
        ) {
            Some(p) => {
                tracing::info!(
                    target_provider = %resolved.provider_id,
                    target_model = %resolved.model_id,
                    session_provider = %session_pid,
                    tier = ?resolved.tier,
                    "ADR-056: rebuilt provider for cross-provider distillation"
                );
                (p, resolved.model_id.clone(), resolved.tier)
            }
            None => {
                tracing::warn!(
                    target_provider = %resolved.provider_id,
                    target_model = %resolved.model_id,
                    "ADR-056: failed to rebuild provider for cross-provider distillation, demoting to session provider + current chat model"
                );
                (
                    self.core.provider.clone(),
                    self.resolve_current_model(None),
                    DistillTier::CurrentChat,
                )
            }
        }
    }

    /// Run LLM-based context compaction (5-level plan).
    ///
    /// ADR-061: FIFO / emergency trim are deleted; the 5-level plan
    /// (level-5 floor) covers every case and LLM failure is explicit —
    /// history is left untouched and the error is returned so callers stop
    /// the agent loop instead of continuing to grow an un-compactable
    /// context (2026-09-06 incident: the UI showed the error while the loop
    /// kept iterating).
    ///
    /// The single automatic trigger is 90% of the projected next-request
    /// input (see [`Self::compact_threshold_tokens`] /
    /// [`Self::projected_exceeds_compact_threshold`]); `force=true` bypasses
    /// the threshold for manual compression and absolute-budget guards.
    ///
    /// Returns `Ok(())` on success (or when no compaction was needed).
    pub(crate) async fn compact_history_if_needed(
        &mut self,
        model_name: &str,
        force: bool,
    ) -> crate::error::Result<()> {
        let budget = self.context_trim_budget(model_name);
        let current_tokens = self.session.history.token_count();

        if budget == 0 {
            return Ok(());
        }

        let usage_percent = (current_tokens as f64 / budget as f64) * 100.0;
        let projected = self.projected_input_tokens();

        // The single automatic compaction line: 90% of the input budget,
        // measured on the PROJECTED next-request input (history anchored to
        // the last API-counted prompt + estimated new messages).
        // `force=true` bypasses the threshold for manual compression and the
        // absolute-budget floor.
        if force || self.projected_exceeds_compact_threshold(model_name, 0) {
            tracing::info!(
                usage_percent = ?usage_percent,
                current_tokens,
                projected,
                threshold = self.compact_threshold_tokens(model_name),
                budget,
                force,
                "Triggering LLM compaction"
            );

            // Notify frontend that compaction has started (both manual and auto paths).
            let _ = self
                .session_core
                .try_send_chunk(ChunkEvent::CompactingStarted);

            // ADR-083: arm the per-compaction cancel slot. This must happen
            // BEFORE `CompactingStarted` is published, so the "cancel
            // compaction" button the frontend renders in response is already
            // bound to a live handle. `begin_compaction` swaps in a fresh
            // one-shot handle, so a `cancel()` that arrives before this point
            // targets the retired handle and is harmlessly discarded.
            let compaction_cancel = self.session_core.begin_compaction();

            // ADR-083: ONE end-to-end deadline shared by every distill tier
            // (§3.2). Established here, never re-armed per tier — that is the
            // whole point: without it, `provider_request_timeout_ms` (10 min)
            // × `max_attempts` (3) × N tiers had no upper bound.
            let deadline = tokio::time::Instant::now() + self.core.config.timeouts.compaction();

            // Build combined text from history for model-aware token counting.
            let combined_text: String =
                self.session
                    .history
                    .messages()
                    .iter()
                    .fold(String::new(), |mut acc, m| {
                        acc.push_str(&m.content);
                        acc.push('\n');
                        acc
                    });
            let _resolved_distill = self.resolve_distill_model(&combined_text);
            // Compaction prompt resolution chain:
            //   1. AgentCore.compaction_prompt — package-declared
            //      `prompts/summary.md` (per-agent summarization rules).
            //   2. Built-in COMPACTION_SYSTEM_PROMPT — universal fallback.
            // The main-dialog `system_prompt_override` is deliberately NOT
            // consulted: it overrides the dialog identity, while compaction
            // is a summarization task with its own directive (see
            // `AgentCore.compaction_prompt` doc).
            // ADR-063 §3.7.5: `compaction_prompt()` returns owned `Option<String>`
            // (cloned through the Arc<RwLock>). Bind the temporary to a
            // local before `.as_deref()` so the returned `&str` outlives
            // the statement — without the binding the temporary drops and
            // the `&str` would dangle by the next use.
            let override_prompt = self.core.compaction_prompt();
            let system_prompt = override_prompt
                .as_deref()
                .unwrap_or(crate::prompt::COMPACTION_SYSTEM_PROMPT);

            // ADR-056 follow-up — call-phase fallback: try the resolved
            // target first; on LLM call failure (network, 4xx, 5xx, parse
            // error), step down to the next tier in `resolve_distill_targets()`
            // order. The previous behaviour was a single attempt + unconditional
            // `trim_fifo` + `emergency_trim` on error — which silently destroys
            // early history when the global-default provider is down. The
            // three-tier fallback chain mirrors the chain the *selection* phase
            // already uses (ADR-056 §3.2), so the user only loses history when
            // **all three** distillation targets fail.
            //
            // (post-2026-09-10: the resolved memory_provider is no longer
            // consumed here — compact summaries are not auto-written to
            // Grafeo. Memory persistence is delegated to the agent via the
            // `memory_store` tool, driven by `prompts/system.md` guidance.)
            let targets = self.resolve_distill_targets();
            tracing::info!(
                tier = ?targets.first().map(|t| t.tier),
                provider_id = %targets.first().map(|t| t.provider_id.clone()).unwrap_or_default(),
                model_id = %targets.first().map(|t| t.model_id.clone()).unwrap_or_default(),
                fallback_targets = targets.len().saturating_sub(1),
                "Distillation target resolved (with call-phase fallback chain)"
            );

            // Track which target produced the successful result so the post-
            // success bookkeeping uses the right `compact_model` (which may
            // differ from the top-priority one if a lower tier won).
            let mut succeeded: Option<(
                ResolvedDistill,
                String,
                (String, acowork_core::providers::traits::UsageInfo),
            )> = None;
            let mut last_err: Option<crate::error::RuntimeError> = None;
            // ADR-083: set when the loop stopped short of a summary because
            // of a cancel or the shared deadline (not because a tier erred).
            let mut cancel_reason: Option<CompactionCancelReason> = None;

            /// Outcome of one distill tier under the ADR-083 guards.
            enum TierOutcome {
                Done((String, acowork_core::providers::traits::UsageInfo)),
                Errored(crate::error::RuntimeError),
                Cancelled,
                TimedOut,
            }

            for target in &targets {
                // Cancelled between tiers (e.g. while the previous tier was
                // tearing down) — stop before spending another tier's budget.
                if compaction_cancel.is_cancelled() {
                    cancel_reason = Some(CompactionCancelReason::UserCancelled);
                    break;
                }
                // Shared budget exhausted by earlier tiers → no more tiers.
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    cancel_reason = Some(CompactionCancelReason::Timeout);
                    break;
                }

                let (compact_provider, compact_model, _tier) = self.distill_provider(target);
                let call = self.session.history.compact_via_llm(
                    compact_provider.as_ref(),
                    &compact_model,
                    system_prompt,
                    self.session.identity_context(),
                );
                // ADR-083: race the LLM against the user's cancel and the
                // remaining shared budget. `biased` gives the cancel branch
                // priority on a dead heat — an explicit user request wins.
                let outcome = tokio::select! {
                    biased;
                    _ = compaction_cancel.cancelled() => TierOutcome::Cancelled,
                    r = tokio::time::timeout(remaining, call) => match r {
                        Ok(Ok(v)) => TierOutcome::Done(v),
                        Ok(Err(e)) => TierOutcome::Errored(e),
                        Err(_) => TierOutcome::TimedOut,
                    },
                };

                match outcome {
                    TierOutcome::Done((summary, usage)) => {
                        tracing::info!(
                            target_provider = %target.provider_id,
                            target_model = %target.model_id,
                            tier = ?target.tier,
                            summary_len = summary.len(),
                            "Compaction LLM call succeeded"
                        );
                        succeeded = Some((target.clone(), compact_model, (summary, usage)));
                        break;
                    }
                    TierOutcome::Errored(e) => {
                        tracing::warn!(
                            target_provider = %target.provider_id,
                            target_model = %target.model_id,
                            tier = ?target.tier,
                            error = %e,
                            "Compaction LLM call failed, trying next tier"
                        );
                        // LowQuality is a model-capability problem — stepping
                        // down the chain only gets cheaper/weaker, so discard
                        // instead of retrying (quality-over-nothing).
                        let non_retryable = matches!(
                            &e,
                            crate::error::RuntimeError::Summary(se) if !se.is_retryable()
                        );
                        last_err = Some(e);
                        if non_retryable {
                            break;
                        }
                    }
                    TierOutcome::Cancelled => {
                        tracing::warn!(
                            target_provider = %target.provider_id,
                            tier = ?target.tier,
                            "ADR-083: compaction cancelled by user — history untouched"
                        );
                        cancel_reason = Some(CompactionCancelReason::UserCancelled);
                        break;
                    }
                    TierOutcome::TimedOut => {
                        tracing::warn!(
                            target_provider = %target.provider_id,
                            tier = ?target.tier,
                            "ADR-083: compaction deadline expired — history untouched"
                        );
                        cancel_reason = Some(CompactionCancelReason::Timeout);
                        break;
                    }
                }
            }

            // Whether every distill tier failed (no summary produced).
            // Failure must abort the agent loop — see the tail of this fn.
            let compaction_failed = succeeded.is_none();

            match succeeded {
                Some((resolved_distill, compact_model, (summary, usage))) => {
                    // ADR-027 (revised for compaction): record raw Provider
                    // usage from the compaction summary call into the session
                    // token accumulator, but **do not** overwrite `last_input`
                    // with the summary LLM's `prompt_tokens`. The summary LLM
                    // was given the **full pre-compaction history** as input,
                    // so its prompt size is unrepresentative of the
                    // post-compaction session state — overwriting `last_input`
                    // would inflate the displayed context usage until the
                    // next main-dialog LLM call recalibrates it.
                    //
                    // `accumulate_compaction_usage` preserves `last_input` /
                    // `last_output` and only updates cumulative totals.
                    // `set_history_anchor` (called below after
                    // `replace_middle_with_summary`) re-anchors `last_input`
                    // to the post-compaction `history.token_count()`.
                    if let Some(ref conversation) = self.session.conversation {
                        // Per-model attribution: the distill target's provider
                        // wins; an empty `provider_id` means "session
                        // provider" (same convention as `distill_provider`).
                        // Cost comes from the compact model's price table —
                        // `None` (no caps / no price) folds as unknown.
                        let provider_id = if resolved_distill.provider_id.is_empty() {
                            conversation.provider()
                        } else {
                            Some(resolved_distill.provider_id.clone())
                        };
                        let compact_caps = self.get_model_capabilities(&compact_model);
                        conversation.accumulate_compaction_usage(
                            &usage,
                            &crate::conversation::ModelUsageAttribution {
                                provider_id: provider_id.as_deref(),
                                model_id: Some(&compact_model),
                                cost_delta: compact_caps.as_ref().and_then(|c| {
                                    crate::conversation::estimate_call_cost(c.cost.as_ref(), &usage)
                                }),
                            },
                        );
                    }
                    // ADR-028: also feed the agent-scoped counters so the
                    // agent-total line in Results Panel updates even if
                    // the session's persisted meta is still on disk.
                    self.core.accumulate_llm_usage(&usage);
                    let validated = crate::episode_distill::parse_and_validate_summary(
                        &summary,
                        Some(&self.session.history.user_intent_fallback_text()),
                    );
                    // ADR-061: summary first (input = full history), then
                    // pick the 5-level plan against the now-known summary
                    // size, then apply (§19.1). History is untouched when no
                    // plan fits the budget (§19.5).
                    //
                    // The marker text carries both the <summary> and the
                    // <user_intent> block (§8.2: user_intent stays inside
                    // the marker, not a separate message) so the next
                    // compaction's LLM sees the preserved user intents.
                    let marker_text = format!(
                        "<summary>{}</summary>\n<user_intent>{}</user_intent>",
                        validated.summary, validated.user_intent
                    );
                    // ADR-061 §19.3: the per-agent compression ratio threshold
                    // (Agent Setup panel, agent_config.json) replaces the
                    // built-in default; None → MIN_COMPRESSION_RATIO (0.90).
                    let min_ratio = self
                        .core
                        .compression_ratio_threshold
                        .unwrap_or(MIN_COMPRESSION_RATIO);
                    // ADR-060 v2 §5.4: the injected round must be persisted to
                    // JSONL AFTER `append_compaction_event` (so the synthetic
                    // rows land AFTER the compaction offset and the restorer
                    // picks them up on resume). Hoist it out of the apply
                    // match — the inner arm can only mutate it.
                    let mut injected_rounds_for_persistence: Vec<(
                        acowork_core::providers::traits::ChatMessage,
                        acowork_core::providers::traits::ChatMessage,
                    )> = Vec::new();
                    let (level, removed) = match self
                        .session
                        .history
                        .plan_compression(&marker_text, min_ratio)
                    {
                        Ok(plan) => {
                            let level = plan.level;

                            // ADR-060 v2 §5.4: snapshot the last todo_write
                            // round BEFORE `apply_compression` may remove it
                            // from history. We splice it back after the marker
                            // post-mutation if it is no longer in the
                            // retained tail.
                            let pending_todo_write_inject =
                                self.session.history.find_last_todo_write_round();

                            // Same reason, for `load_skill` bodies: the
                            // candidates must be read while they are still in
                            // `messages`. The *budget* cannot be — it is a
                            // function of the space compression frees, so it is
                            // computed after `apply_compression` below.
                            // Newest-first, one per distinct skill name.
                            let pending_skill_rounds =
                                self.session.history.find_latest_skill_rounds();

                            match self.session.history.apply_compression(
                                plan,
                                &marker_text,
                                min_ratio,
                            ) {
                                Ok(outcome) => {
                                    tracing::info!(
                                        level = outcome.level,
                                        original_tokens = outcome.original_tokens,
                                        new_tokens = outcome.new_tokens,
                                        compression_ratio = ?outcome.compression_ratio,
                                        removed_messages = outcome.removed_messages,
                                        "ADR-061: 5-level compression plan applied"
                                    );

                                    // ADR-060 v2 §5.4: idempotent — inject
                                    // skips if the round is already in the
                                    // retained tail (preserves cache hit +
                                    // avoids duplicate tool_call_id); splices
                                    // after the marker if removed. The
                                    // synthetic rows are persisted AFTER
                                    // `append_compaction_event` below (it
                                    // blocks until the compaction offset is
                                    // committed, so the subsequent
                                    // append_message_with_id commands land
                                    // AFTER it in the JSONL).
                                    //
                                    // `inject_round_after_marker`
                                    // returns `(tokens, injected)`: the
                                    // second element is `true` only when the
                                    // splice actually happened. We use it as
                                    // the JSONL persistence gate — skipping
                                    // the append_message_with_id calls when
                                    // the in-memory splice was skipped
                                    // prevents duplicate synthetic rows on
                                    // disk across consecutive compressions.
                                    if let Some((assistant, tool)) = pending_todo_write_inject {
                                        // ADR-060 v2 §5.4: idempotency is
                                        // enforced inside
                                        // `inject_round_after_marker`
                                        // by the `injected_round_call_ids`
                                        // gate. On the first compression of a
                                        // session with a todo_write round, it
                                        // splices and signals `injected=true`,
                                        // and the caller persists the synthetic
                                        // rows to JSONL below. On every
                                        // subsequent compression the gate
                                        // refuses re-injection — preventing
                                        // unbounded JSONL duplication.
                                        let (_, injected) = self
                                            .session
                                            .history
                                            .inject_round_after_marker(
                                                assistant.clone(),
                                                tool.clone(),
                                            );
                                        if injected {
                                            injected_rounds_for_persistence
                                                .push((assistant, tool));
                                        }
                                    }

                                    // ADR-094 follow-up: carry loaded SKILL.md
                                    // bodies through the compaction too, within
                                    // the adaptive-then-capped recall budget.
                                    //
                                    // The budget is measured on the
                                    // POST-compression token count — that is
                                    // the state we are spending against —
                                    // while `budget` is the usable input budget
                                    // for this model (`context_trim_budget`).
                                    let skill_budget = skill_recall_budget_tokens(
                                        budget,
                                        self.session.history.token_count(),
                                    );
                                    let skill_rounds_to_inject = self
                                        .session
                                        .history
                                        .apply_skill_recall_budget(
                                            &pending_skill_rounds,
                                            skill_budget,
                                        );
                                    if !pending_skill_rounds.is_empty() {
                                        tracing::info!(
                                            candidates = pending_skill_rounds.len(),
                                            preserved = skill_rounds_to_inject.len(),
                                            skill_budget,
                                            "skill recall: carrying load_skill rounds \
                                             through compaction"
                                        );
                                    }
                                    for (assistant, tool) in skill_rounds_to_inject {
                                        let (_, injected) = self
                                            .session
                                            .history
                                            .inject_round_after_marker(
                                                assistant.clone(),
                                                tool.clone(),
                                            );
                                        if injected {
                                            injected_rounds_for_persistence
                                                .push((assistant, tool));
                                        }
                                    }

                                    (level, outcome.removed_messages)
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        error = %e,
                                        "ADR-061: compression apply rejected, history untouched"
                                    );
                                    (level, 0)
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "ADR-061: no compression plan fits budget, history untouched"
                            );
                            (0, 0)
                        }
                    };

                    // Recompute usage after compaction (used both for the
                    // JSONL event payload and the stage-3 emergency check).
                    let new_tokens = self.session.history.token_count();
                    let new_usage = if budget > 0 {
                        (new_tokens as f64 / budget as f64) * 100.0
                    } else {
                        0.0
                    };

                    // Anchor `last_input` to the post-compaction history size
                    // so the next `emit_session_state()` push reports the new
                    // (smaller) `usage_percent` **immediately**, without
                    // waiting for the next main-dialog LLM call. The value is
                    // heuristic — it will be overwritten by the next
                    // `accumulate_llm_usage()` with the API's authoritative
                    // `prompt_tokens` — but the heuristic is good enough for
                    // the immediate StatusBar refresh and prevents the
                    // "stale 70% before, drops to 20% after next message"
                    // UX glitch.
                    //
                    // The anchor must run AFTER `replace_middle_with_summary`
                    // (so `history.token_count()` reflects the new state) and
                    // BEFORE `append_compaction_event` (so the JSONL event
                    // carries the post-anchor token state).
                    if let Some(ref conversation) = self.session.conversation {
                        conversation.set_history_anchor(new_tokens);

                        let meta = crate::conversation::CompactionEventMeta {
                            // Range tracking is best-effort; we don't currently
                            // know the precise from/to entry ids without
                            // threading them through. Leaving them empty is
                            // acceptable — the restorer only needs the event's
                            // *position* in the log, not the id range.
                            compacted_from_id: String::new(),
                            compacted_to_id: String::new(),
                            level,
                            model: compact_model.clone(),
                            before_tokens: current_tokens,
                            after_tokens: new_tokens,
                        };
                        // Sync handshake — the writer has seeked, written the
                        // compaction entry, and updated `last_compaction_offset`
                        // by the time this returns. The subsequent
                        // `append_message_with_id` calls for the injected
                        // round therefore land AFTER the compaction offset in
                        // the JSONL, so the restorer picks them up on resume.
                        conversation.append_compaction_event(&marker_text, meta);

                        // ADR-060 v2 §5.4: persist the injected round to JSONL
                        // so the canonical todo state survives a process
                        // restart. Without these rows the restorer would find
                        // only the compaction marker post-restart (everything
                        // pre-compact is skipped), and the next chat round
                        // would lose its todo reference.
                        //
                        // The format mirrors the normal chat flow's
                        // `tool_call` / `tool_result` rows (loop_tools.rs:1100)
                        // so the restorer can merge them identically on
                        // resume — `tool_call` rows fold into the previous
                        // assistant's `tool_calls` list (or synthesize an
                        // empty assistant if none exists), and `tool_result`
                        // rows become standalone Tool messages whose
                        // `tool_call_id` matches the assistant's tool_calls
                        // (verified by the orphan-tool sanitiser).
                        if !injected_rounds_for_persistence.is_empty() {
                            for (assistant, tool) in &injected_rounds_for_persistence {
                                if let Some(tool_calls) = &assistant.tool_calls {
                                    for tc in tool_calls {
                                        let call_meta = serde_json::json!({
                                            "tool_name": tc.function.name,
                                            "tool_call_id": tc.id,
                                        });
                                        let call_id =
                                            format!("inject-call-{}", uuid::Uuid::new_v4().simple());
                                        conversation.append_message_with_id(
                                            "tool_call",
                                            &tc.function.arguments,
                                            Some(call_meta),
                                            Some(call_id),
                                        );
                                    }
                                }
                                let result_meta = serde_json::json!({
                                    "tool_call_id": tool
                                        .tool_call_id
                                        .clone()
                                        .unwrap_or_default(),
                                    // Fall back to the caller's own tool name,
                                    // never a hardcoded one: this path now
                                    // carries both todo_write and load_skill
                                    // rounds, and a mislabelled row would make
                                    // the restorer fold the result into the
                                    // wrong assistant.
                                    "tool_name": tool
                                        .name
                                        .clone()
                                        .or_else(|| {
                                            assistant
                                                .tool_calls
                                                .as_ref()
                                                .and_then(|tcs| tcs.first())
                                                .map(|tc| tc.function.name.clone())
                                        })
                                        .unwrap_or_default(),
                                });
                                let result_id = format!(
                                    "inject-result-{}",
                                    uuid::Uuid::new_v4().simple()
                                );
                                conversation.append_message_with_id(
                                    "tool_result",
                                    &tool.content,
                                    Some(result_meta),
                                    Some(result_id),
                                );
                            }
                            injected_rounds_for_persistence.clear();

                            // Synchronous flush — blocks until the writer
                            // thread has physically committed the synthetic
                            // rows above. Without this, a process restart
                            // immediately after compaction could race the
                            // writer and lose the round. One flush for the
                            // whole batch: skill recall can persist N rounds,
                            // and each individual row is already ordered
                            // behind the compaction offset.
                            if let Err(e) = conversation.flush_pending().await {
                                tracing::warn!(
                                    error = %e,
                                    "ADR-060 v2: flush_pending after inject failed — restart may not see injected round"
                                );
                            }
                        }
                    }

                    // Compaction no longer writes the summary to Grafeo (was:
                    // `EpisodeDistiller::write_summary_to_provider`). Memory
                    // persistence is delegated to the agent itself — see
                    // `prompts/system.md` for the prompt guidance asking the
                    // LLM to call `memory_store` when a task is complete.
                    // The summary still replaces the in-memory history slice
                    // above, so the next LLM call sees a compact context.
                    tracing::debug!(
                        session_id = %self
                            .session
                            .conversation
                            .as_ref()
                            .map(|c| c.session_id())
                            .unwrap_or(""),
                        summary_len = summary.len(),
                        "compaction summary not auto-written to Grafeo; agent must call memory_store to persist"
                    );

                    // Mark session as compacted (zero new messages since compaction)
                    self.session.is_compacted = true;

                    // ADR-051 P3 maintenance tasks (generalization / history
                    // compression / relationship auto-generation) were all
                    // removed: semantic-layer promotion happens exclusively in
                    // the EpisodicDistiller background step (ADR-068
                    // single-producer rule, gated by [memory.distiller].
                    // enabled). Nothing runs inline after compaction.
                    tracing::info!(
                        removed,
                        summary_len = summary.len(),
                        before_tokens = current_tokens,
                        after_tokens = new_tokens,
                        before_usage = ?usage_percent,
                        after_usage = ?new_usage,
                        "LLM compaction completed"
                    );

                    // Stage 3: 95% → the 5-level plan already guarantees
                    // post-compaction size ≤ budget (apply validates
                    // projected ≤ budget; level 5 is the floor — §19.5).
                    // Usage ≥ CRITICAL here simply means the conversation
                    // is still large after a successful level-1..7 fit;
                    // emergency trim is deleted (ADR-061 §11) and the next
                    // 80% trigger will re-compact when needed.
                    if new_usage >= CONTEXT_CRITICAL_PERCENT {
                        tracing::warn!(
                            after_usage = ?new_usage,
                            "Post-compaction usage still >= CONTEXT_CRITICAL_PERCENT (within budget; no emergency trim)"
                        );
                    }
                }
                None => {
                    // ADR-061 §11.3: LLM unavailable → explicit failure.
                    // History is left untouched; the FIFO + emergency trim
                    // fallback is deleted. The error is RETURNED (not just
                    // logged) so the caller aborts the loop → session Idle,
                    // instead of continuing to grow an over-budget context
                    // while the UI already shows the failure.
                    tracing::error!(
                        error = %last_err.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                        target_count = targets.len(),
                        "ADR-061: LLM compaction failed — history untouched (no FIFO fallback)"
                    );
                }
            }

            // ADR-083: the end event now carries the outcome. Success keeps
            // `CompactingEnded`; every non-success path emits
            // `CompactionCancelled { reason }` so the frontend can reset its
            // dual-state button and pick the matching toast.
            let end_event = if compaction_failed {
                ChunkEvent::CompactionCancelled {
                    reason: cancel_reason.unwrap_or(CompactionCancelReason::Failed),
                }
            } else {
                ChunkEvent::CompactingEnded
            };
            let _ = self.session_core.try_send_chunk(end_event);

            // Re-emit the runtime session-state snapshot now that
            // `last_input` reflects the post-compaction history size (via
            // `set_history_anchor` above). Without this call, the
            // `SessionStateChanged` event published earlier in this flow
            // (by `accumulate_compaction_usage → notify_state_change`)
            // carries a stale, inflated `usage_percent` that contradicts
            // the standalone `ContextUsage` event below. Frontends that
            // key off `SessionStateChanged` (e.g. for the retained
            // `sessions/{sid}/state` topic) would otherwise show the old
            // percentage until the next main-dialog LLM call recalibrates.
            //
            // The standalone `ContextUsage(ctx_info)` push below carries
            // the same numeric value computed here from
            // `history.token_count()`, so the two channels stay consistent.
            self.emit_session_state();

            // Compute and send updated context usage after compaction.
            if let Some((caps, effective_window, effective_usable)) =
                self.effective_context_budget(model_name)
            {
                let total_tokens = self.session.history.token_count();
                // Same decoupling as compute_context_usage: display percent
                // over window TOTAL; thresholds use effective_usable (input).
                let usage_percent = if effective_window > 0 {
                    ((total_tokens as f64 / effective_window as f64) * 100.0).min(100.0) as u8
                } else {
                    0
                };
                // Pull cumulative session totals for the post-compaction
                // snapshot so the frontend status panel can update both
                // per-turn and cumulative fields in one push.
                let session_tokens = self.session.conversation.as_ref().and_then(|c| c.tokens());
                let ctx_info = acowork_core::protocol::ContextUsageInfo {
                    context_window: effective_window,
                    input_tokens: total_tokens,
                    output_tokens: 0,
                    total_tokens,
                    max_input_tokens: caps.max_input_tokens,
                    usable_context: effective_usable,
                    usage_percent,
                    // Cumulative session fields default to None —
                    // patched below via the shared helper.
                    total_input_tokens: None,
                    total_output_tokens: None,
                    // ADR-028 / ADR-066: agent-scoped cumulative tokens + cache
                    // totals are snapshotted from AtomicU64 counters below.
                    agent_total_input_tokens: None,
                    agent_total_output_tokens: None,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    total_cache_read_tokens: None,
                    total_cache_write_tokens: None,
                    agent_total_cache_read_tokens: None,
                    agent_total_cache_write_tokens: None,
                    sections: None,
                    // Post-compaction recompute push. The compaction's own
                    // summary LLM call goes through `compact_via_llm`, a
                    // separate path that does NOT bump the per-session
                    // `llm_call_counter` (only real dialog responses in
                    // `process_llm_response_usage` bump), so no count here —
                    // this push only reflects the new context shape.
                    iteration: None,
                    avg_tps: None,
                    avg_ttft_ms: None,
                };
                // ADR-028 + ADR-066: patch session + agent totals in one place.
                let mut ctx_info = ctx_info;
                if let Some(t) = session_tokens {
                    patch_session_totals(&mut ctx_info, &t);
                }
                // Session speed / latency averages: no LLM call happened on
                // this path, but the persisted values must survive the push
                // or the panel would blank out mid-session.
                patch_perf(
                    &mut ctx_info,
                    self.session.conversation.as_ref().and_then(|c| c.perf()).as_ref(),
                );
                patch_agent_totals(&mut ctx_info, self.core.agent_token_totals());
                let _ = self
                    .session_core
                    .try_send_chunk(ChunkEvent::ContextUsage(ctx_info));
            }

            // Failure tail: every distill tier failed (no summary produced).
            // The CompactingEnded / session-state / ContextUsage events above
            // already fired so the UI is consistent — now signal the loop to
            // STOP. The caller (pre-request / post-append / absolute-budget
            // guard) propagates this out of the iteration; `decide_retry_action`
            // treats `CompactionFailed` as non-retryable → GiveUp → Idle, so
            // the agent never keeps looping with an un-compactable context.
            if compaction_failed {
                let detail = last_err.as_ref().map(|e| e.to_string()).unwrap_or_default();
                // ADR-083: a user cancel / deadline expiry is not a "failure".
                // History is untouched and the session returns to Idle so the
                // user can immediately retry with another model. The distinct
                // `CompactionCancelled` variant lets the manual-path caller
                // suppress the generic error bubble — the
                // `CompactionCancelled` chunk event already told the UI.
                return Err(match cancel_reason {
                    Some(reason) => crate::error::RuntimeError::CompactionCancelled(
                        format!("{reason:?}"),
                    ),
                    None => crate::error::RuntimeError::CompactionFailed(detail),
                });
            }
        } else if usage_percent >= CONTEXT_CRITICAL_PERCENT {
            // NOTE: this branch is unreachable today — CONTEXT_COMPACT_PERCENT
            // (90) < CONTEXT_CRITICAL_PERCENT (95), so any usage ≥ 95% enters
            // the compaction branch above first. It is kept as documentation
            // of the deleted Stage-3 emergency-trim safety net (ADR-061 §11):
            // the 5-level plan replaces it, and explicit failure (returned
            // `Err(CompactionFailed)`) replaces silent FIFO trimming.
            tracing::error!(
                usage_percent = ?usage_percent,
                current_tokens,
                budget,
                "Unexpected critical usage without compaction — unreachable; no emergency trim (ADR-061)"
            );
        }

        Ok(())
    }

    // ── Sub-methods extracted from execute_single_iteration (ADR-014 Phase 1) ──

    /// ① Budget pre-check — validate that the estimated token count is
    /// within budget before proceeding with the LLM call.
    ///
    /// Returns `Err(BudgetExceeded)` if budget is denied, otherwise `Ok(())`.
    /// Warnings are appended to history as system messages.
    pub(crate) fn check_budget_and_warn(&mut self) -> crate::error::Result<()> {
        use crate::agent::budget_guard::BudgetCheckResult;
        use crate::agent::session_state::SessionStatus;
        use acowork_core::providers::traits::{ChatMessage, MessageRole};

        let estimated_tokens = self.session.history.estimate_total_tokens() + 500; // +500 for new response
        match self.session.budget_guard.check(estimated_tokens) {
            BudgetCheckResult::Allowed => {}
            BudgetCheckResult::Exceeded { reason, action } => {
                tracing::warn!(reason = %reason, action = %action, "Budget exceeded");
                match action.as_str() {
                    "deny" => {
                        self.transition_status(SessionStatus::Idle);
                        return Err(crate::error::RuntimeError::BudgetExceeded(reason));
                    }
                    "warn" => {
                        self.session.history.append(ChatMessage {
                            role: MessageRole::User,
                            content: format!("[System Warning] {reason}"),
                            name: Some("system".to_string()),
                            ..Default::default()
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// ②.5 Build the chat request from context + history, including MCP tool merge.
    ///
    /// This consolidates: todo injection, context build, logging, and MCP tool
    /// merge into a single method so that `execute_single_iteration` reads as
    /// a high-level orchestration.
    pub(crate) fn build_chat_request(
        &mut self,
        context_builder: &mut crate::agent::context::ContextBuilder,
        current_model: &str,
    ) -> acowork_core::providers::traits::ChatRequest {
        // ADR-060 v2: `set_todo_context` removed — todo state is carried by the
        // most recent real `todo_write` tool result in Block B history.
        let caps = self.get_model_capabilities(current_model);
        let max_output_limit = self.core.max_output_tokens_limit_for_model(current_model);

        // Resolve reasoning_effort: read from per-session state (initialized from
        // the three-level priority chain in build_initial_session_state).
        // None  = provider does not support thinking control (frontend hides control).
        // Some(Auto) = provider supports it but no parameter sent to LLM (model decides).
        // Some(Low/Medium/High/...) = explicit level sent to LLM.
        // The fallback to caps.default_reasoning_effort is kept for safety in case
        // the session state was not initialized yet (e.g. direct AgentLoop usage).
        let reasoning_effort = self.session.reasoning_effort().cloned().or_else(|| {
            caps.as_ref()
                .and_then(|c| c.default_reasoning_effort.as_deref())
                .and_then(acowork_core::providers::traits::ReasoningEffort::from_str_loose)
        });
        context_builder.set_reasoning_effort(reasoning_effort.clone());
        // Cache for emergency trim retry in call_llm_streaming_inner()
        // where context_builder is immutable.
        self.last_reasoning_effort = reasoning_effort;

        // Resolve thinking_mode (Anthropic: "extended" vs "adaptive")
        let thinking_mode = caps.as_ref().and_then(|c| c.thinking_mode.clone());
        context_builder.set_thinking_mode(thinking_mode.clone());
        self.last_thinking_mode = thinking_mode;

        // Resolve temperature via the per-agent chain:
        //   agent_config.json (Layer 1) → manifest default (Layer 2) → DEFAULT_TEMPERATURE (Layer 3).
        // Always set a concrete value on the builder so the ChatRequest reflects what
        // the model will actually receive, and so the value shown in the status panel
        // matches the request payload.
        let temperature = self
            .session
            .temperature()
            .or(self.core.temperature_override)
            .or(self.core.manifest_temperature)
            .unwrap_or(crate::config::DEFAULT_TEMPERATURE);
        context_builder.set_temperature(Some(temperature));

        let mut chat_request = context_builder.build(
            &self.core.manifest,
            &self.session.history,
            // ADR-060 §5.5 erratum: no Block D arg — the current user
            // message is carried by Block B's tail (history).
            caps.as_ref(),
            max_output_limit,
        );

        tracing::info!(
            request_messages_count = chat_request.messages.len(),
            request_model = %chat_request.model,
            request_max_tokens = ?chat_request.max_tokens,
            request_tools_count = chat_request.tools.as_ref().map(|t| t.len()).unwrap_or(0),
            history_tokens = self.session.history.token_count(),
            "Built chat request for LLM (after preemptive trim)"
        );

        // Inject MCP server tool definitions into the LLM request.
        //
        // `chat_request.tools` contains built-in tool definitions from
        // `tool_definitions` (set at startup, does not include MCP tools).
        // MCP server tools live separately in `self.core.mcp_tools` and must
        // be injected here before each LLM call.
        //
        // We iterate `self.core.mcp_tools` directly — NOT `self.core.all_tools`
        // filtered by name prefix — because `all_tools` mixes built-in tools
        // (mcp_install, mcp_uninstall) with MCP server tools, and filtering by
        // "mcp_" prefix would re-inject built-in tools and cause
        // "Tool names must be unique" 400 errors.
        if let Some(ref mut tools) = chat_request.tools
            && let Some(ref mcp_tools) = self.core.mcp_tools
        {
            for tool in mcp_tools {
                let spec = tool.spec();
                let val = serde_json::to_value(&spec).unwrap_or_default();
                tools.push(val);
            }
        }

        // Design 27 §6.5 R1 + §5.5.4 (plan 2b.3): two orthogonal gates on
        // the wire tool set. `intent_send` is hidden by ROLE — a responder
        // turn answers one request and hands the turn back (§7.1 keeps
        // ownership singular). `ask_user_question` is hidden by
        // ANSWERABILITY — only when the Gateway stamped the session as
        // having no human participant; a group-mention responder turn
        // still has humans to answer a card, so the tool stays (§5.5.4).
        // Unknown (`None`) fails open: keep the tool.
        {
            let hidden = hidden_tools_for_turn(
                self.pending_intent.is_some(),
                self.turn_surface.has_human,
            );
            if !hidden.is_empty()
                && let Some(tools) = chat_request.tools.as_mut()
            {
                tools.retain(|t| {
                    let name = t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .or_else(|| {
                            t.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                        })
                        .unwrap_or("");
                    !hidden.contains(&name)
                });
            }
        }

        // LLM-perspective tool dump: AFTER builtin + MCP merge.
        // This is the most authoritative view of what the LLM actually sees
        // in the next provider call. tool_names is the literal contents of
        // the tools array sent on the wire.
        if let Some(ref tools) = chat_request.tools {
            let tool_names: Vec<String> = tools
                .iter()
                .filter_map(|t| {
                    t.get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                        .or_else(|| {
                            t.get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                })
                .collect();
            tracing::info!(
                request_tools_count = tool_names.len(),
                request_tool_names = ?tool_names,
                "LLM tools schema (after builtin + MCP merge) -- exact wire payload"
            );
        }

        // Compute total input chars for next round's token ratio calibration.
        self.last_input_chars = count_chat_request_chars(&chat_request);

        // Inject transient tool results from the previous iteration
        // (ADR-032 C3a — preserved for future tools that need one-shot
        // injection). ADR-052 removed the `context_retrieve` transient
        // path; the field is now only populated by hypothetical future
        // transient tools. Empty vec naturally skips.
        if !self.pending_transient_tool_msgs.is_empty() {
            let count = self.pending_transient_tool_msgs.len();
            chat_request
                .messages
                .append(&mut self.pending_transient_tool_msgs);
            debug_assert!(self.pending_transient_tool_msgs.is_empty());
            tracing::debug!(count, "Injected transient tool results into chat request");
        }

        chat_request
    }

    /// ②.6 Pre-request circuit-breaking — the single automatic compaction
    /// trigger. Runs the ADR-061 v3 5-level plan when the PROJECTED
    /// next-request input tokens (history anchored to the last API
    /// `prompt_tokens` + estimated new messages) reach
    /// `CONTEXT_COMPACT_PERCENT` (90%) of the input budget.
    /// WARN (70%) remains informational only.
    ///
    /// ADR-061 §11.2: this routes through [`Self::compact_history_if_needed`]
    /// (force) — `emergency_trim` is deleted, and failure is returned so the
    /// iteration aborts (session → Idle) instead of continuing to grow.
    ///
    /// Returns `true` if the chat_request needs to be rebuilt after trimming.
    pub(crate) async fn check_context_overflow_and_trim(
        &mut self,
        current_model: &str,
    ) -> crate::error::Result<bool> {
        let usable = self.context_trim_budget(current_model);
        let warn_threshold = (usable as f64 * (CONTEXT_WARN_PERCENT / 100.0)) as u64;
        let current_tokens = self.session.history.token_count();
        let projected = self.projected_input_tokens();

        if self.projected_exceeds_compact_threshold(current_model, 0) {
            tracing::info!(
                projected,
                current_tokens,
                threshold = self.compact_threshold_tokens(current_model),
                usable_context = usable,
                "Projected next-request input at compaction threshold — running 5-level compression"
            );
            self.compact_history_if_needed(current_model, true).await?;
            Ok(true) // signal that request needs rebuild
        } else if current_tokens > warn_threshold {
            tracing::warn!(
                current_tokens,
                projected,
                warn_threshold,
                usable_context = usable,
                "Context usage approaching limit (informational)"
            );
            Ok(false)
        } else {
            Ok(false)
        }
    }

    /// ④ Process LLM response usage — update budget, calibrate token counter,
    /// emit context usage report, and trigger compaction if needed.
    ///
    /// This is the largest inline block extracted from `execute_single_iteration`.
    pub(crate) async fn process_llm_response_usage(
        &mut self,
        response: &acowork_core::providers::traits::ChatResponse,
        context_builder: &ContextBuilder,
        current_model: &str,
    ) {
        let local_estimate = self.session.history.token_count();

        if let Some(usage) = &response.usage {
            self.session
                .budget_guard
                .update_usage(usage.total_tokens, 0.0);

            // Diagnostic: log local token estimate vs API ground truth
            tracing::info!(
                model = %current_model,
                local_estimate,
                api_prompt_tokens = usage.prompt_tokens,
                api_completion_tokens = usage.completion_tokens,
                api_reasoning_tokens = usage.reasoning_tokens,
                api_content_tokens = usage.completion_tokens.saturating_sub(usage.reasoning_tokens),
                api_total_tokens = usage.total_tokens,
                "Context usage: local estimate vs API ground truth"
            );

            // Detect providers that return prompt_tokens=0 despite having
            // non-trivial context. Skip calibration to avoid corrupting
            // the internal token counter.
            let prompt_tokens_reliable = usage.prompt_tokens > 0;
            if prompt_tokens_reliable {
                self.session
                    .history
                    .calibrate_from_usage(usage.prompt_tokens, self.last_input_chars);

                // Persist the calibrated ratio into SessionState (UI) and
                // session meta (resume scene) so the next emit_session_state
                // checkpoint picks it up.
                if let Some(ratio) = self.session.history.model_ratio() {
                    self.session.set_model_ratio(ratio);
                    if let Some(conv) = self.session.conversation() {
                        conv.set_model_ratio(ratio);
                    }
                }
            } else {
                tracing::warn!(
                    local_estimate,
                    "API returned prompt_tokens=0 despite non-trivial context; \
                     skipping calibration and using local estimate"
                );
            }

            // Compute and emit context usage report
            let model_caps = self.get_model_capabilities(current_model);
            let max_output_limit = self.core.max_output_tokens_limit_for_model(current_model);
            tracing::debug!(
                has_chunk_tx = self.session_core.chunk_tx.is_some(),
                has_model_caps = model_caps.is_some(),
                has_usage = true,
                "ContextUsage: checking preconditions"
            );
            if let Some(caps) = model_caps {
                // ADR-074: pass the session-effective window (Layer 0
                // override over the per-agent chain, min model window) —
                // the usage push now reflects the per-session cap.
                let resolved = self.resolved_effective_context_window(current_model);
                let ctx_usage = if prompt_tokens_reliable {
                    crate::agent::context::compute_context_usage(
                        &caps,
                        usage,
                        max_output_limit,
                        Some(resolved),
                    )
                } else {
                    // Re-resolve via the helper: model_caps in scope and the
                    // helper agree (same source), so this is infallible here.
                    let (caps, effective_window, effective_usable) = self
                        .effective_context_budget(current_model)
                        .expect("model_caps already verified Some above");
                    // Display percent over window TOTAL (incl. output);
                    // effective_usable stays the compaction denominator only.
                    let percent = if effective_window > 0 {
                        ((local_estimate as f64 / effective_window as f64) * 100.0).min(100.0) as u8
                    } else {
                        0
                    };
                    acowork_core::protocol::ContextUsageInfo {
                        context_window: effective_window,
                        input_tokens: local_estimate,
                        output_tokens: usage.completion_tokens,
                        total_tokens: local_estimate + usage.completion_tokens,
                        max_input_tokens: caps.max_input_tokens,
                        usable_context: effective_usable,
                        usage_percent: percent,
                        total_input_tokens: None,
                        total_output_tokens: None,
                        // ADR-028 / ADR-066: agent-scoped cumulative tokens + cache totals
                        // are snapshotted from AtomicU64 counters immediately after
                        // construction; the snapshot step below populates them.
                        agent_total_input_tokens: None,
                        agent_total_output_tokens: None,
                        cache_read_tokens: None,
                        cache_write_tokens: None,
                        total_cache_read_tokens: None,
                        total_cache_write_tokens: None,
                        agent_total_cache_read_tokens: None,
                        agent_total_cache_write_tokens: None,
                        sections: None,
                        // Filled in just before the push below.
                        iteration: None,
                        // Same for the session speed / latency averages —
                        // recorded and patched just below.
                        avg_tps: None,
                        avg_ttft_ms: None,
                    }
                };
                tracing::debug!(
                    context_window = ctx_usage.context_window,
                    total_tokens = ctx_usage.total_tokens,
                    usage_percent = ctx_usage.usage_percent,
                    "ContextUsage: sending report"
                );

                // Persist the raw Provider-reported usage counts so the
                // frontend context-usage indicator can be restored on
                // session resume. ADR-027 deliberately uses `usage.*`
                // (raw Provider values) rather than `ctx_usage.*` so the
                // persisted snapshot faithfully reflects what the Provider
                // returned — even when `prompt_tokens_reliable == false`
                // and the frontend display falls back to the local
                // tokenizer estimate. The "宁可 miss 也不估计" policy
                // prefers a raw zero over a local estimate in the
                // accumulator.
                //
                // After accumulation, the SessionTokens cumulative totals
                // (`total_input`, `total_output`) reflect this LLM call,
                // so we patch the just-computed `ctx_usage` with them
                // before pushing to the frontend. This lets the status
                // panel show session-level cumulative figures alongside
                // per-turn input_tokens / output_tokens.
                let mut ctx_usage = ctx_usage;
                // The measurement belongs to *this* response. `take()` makes
                // the one-sample-per-round pairing explicit instead of
                // relying on every early-return path of
                // `call_llm_streaming_inner` happening to leave the slot
                // untouched (see the reset at the top of that method).
                let call_perf = self.last_call_perf.take();
                if let Some(ref conv) = self.session.conversation {
                    // One persisted write for the whole round-trip: token
                    // usage, this call's speed / TTFT sample, and the
                    // per-session call count. Folding them separately would
                    // issue three full-row meta upserts per LLM call on the
                    // runtime's hottest path.
                    //
                    // `tps` is `None` when the Provider reported no usable
                    // completion count, and the whole sample is `None` for a
                    // call that never produced output (cancelled before its
                    // first token) — `SessionPerf` folds those as "no
                    // measurement", never as 0.
                    ctx_usage.iteration = Some(conv.record_llm_round(
                        usage,
                        call_perf.and_then(|p| p.tps),
                        call_perf.map(|p| p.ttft_ms),
                        // Per-model attribution: session provider + dialog
                        // model; cost estimated from this model's price
                        // table (cache tokens unbilled — no cache unit
                        // prices in `ModelCostInfo`).
                        &crate::conversation::ModelUsageAttribution {
                            provider_id: conv.provider().as_deref(),
                            model_id: Some(current_model),
                            cost_delta: crate::conversation::estimate_call_cost(
                                caps.cost.as_ref(),
                                usage,
                            ),
                        },
                    ));
                    if let Some(t) = conv.tokens() {
                        // ADR-066 §2 / ADR-066 §7: "compute 只看 per-turn、
                        // session total 由 caller patch".  All four
                        // cumulative session fields (in/out + cache read/write)
                        // are patched through `patch_session_totals` so the
                        // three push sites can't drift apart.  See
                        // `context::patch_session_totals` for the rationale
                        // behind the helper.
                        patch_session_totals(&mut ctx_usage, &t);
                    }
                    // Read back AFTER the round was recorded so the push
                    // carries the averages including this call.
                    patch_perf(&mut ctx_usage, conv.perf().as_ref());
                }
                // ADR-028: feed the agent-scoped counters and snapshot them
                // into the push payload so the frontend's Results Panel can
                // show a "Agent total" line. Live in this path is the
                // primary data source; the session-list response carries a
                // fallback copy (see `handle_list_sessions`).
                //
                // ADR-066: agent-scoped cache totals snapshotted alongside
                // input/output via the 4-tuple return of `agent_token_totals`.
                self.core.accumulate_llm_usage(usage);
                patch_agent_totals(&mut ctx_usage, self.core.agent_token_totals());

                // ADR-067: populate the per-section byte sizes so the
                // input-box context-usage popup's 5-category breakdown
                // works without DevMode. The helper mirrors the section
                // list produced by the DevMode Debug Panel — same keys and
                // order, except for sections that are byte-size-only and
                // carry no single source string to render
                // (`skills_loaded`; the Debug Panel drops those), plus the
                // absence of token estimates / SHA-256 hashes.
                //
                // MCP tools come from `self.core.mcp_tools` (the exact set
                // injected into `ChatRequest.tools`) — NOT `all_tools`
                // filtered by the `mcp_` prefix, which would over-count by
                // including the built-in `mcp_install` / `mcp_uninstall`
                // tools (see `build_chat_request` for the same distinction).
                let mcp_tools: Vec<serde_json::Value> = self
                    .core
                    .mcp_tools
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|tool| serde_json::to_value(tool.spec()).ok())
                    .collect();
                ctx_usage.sections = Some(crate::agent::context::compute_section_sizes(
                    context_builder,
                    &self.session.history,
                    &mcp_tools,
                    current_model,
                ));

                // ADR-067: cache the typed sections directly so the retained
                // `session_state` snapshot (built by `emit_session_state`
                // from persisted tokens, which has no `ContextBuilder` to
                // recompute sections) does not clobber the breakdown with
                // an empty `sections`. The frontend also merges sections
                // from whichever source arrives last (defense in depth).
                if let Some(ref conv) = self.session.conversation
                    && let Some(sections) = ctx_usage.sections.as_ref()
                {
                    conv.cache_context_usage_sections(sections.clone());
                }

                // The per-session LLM-call count was already folded and
                // stamped onto `ctx_usage.iteration` by `record_llm_round`
                // above — one persisted write for the whole round-trip.
                // The counter lives on the conversation session and is
                // persisted in meta.json (`SessionMeta.llm_call_counter`),
                // so it is monotonic for the session's lifetime and
                // survives restarts — unlike the `max_iterations` per-burst
                // loop counter which resets on Continue (loop_.rs).

                if !self
                    .session_core
                    .try_send_chunk(ChunkEvent::ContextUsage(ctx_usage))
                {
                    tracing::debug!(
                        "ContextUsage: chunk channel full/closed or session_id missing"
                    );
                }
            } else {
                let available: Vec<String> = {
                    let list = self.core.global_provider_list.read().unwrap();
                    list.iter()
                        .flat_map(|p| p.models.iter().map(|m| m.id.clone()))
                        .collect()
                };
                let msg = format!(
                    "Model capabilities not found for '{}'. Available: {:?}. \
                     Check that the model name matches exactly (case-sensitive). \
                     Context usage display and compaction accuracy may be affected.",
                    current_model, available
                );
                tracing::warn!(
                    "ContextUsage: NOT sent — missing model capabilities for '{}'",
                    current_model
                );
                let _ = self.session_core.try_send_chunk(ChunkEvent::Error {
                    user_message: msg,
                    detail: String::new(),
                    error_type: "ContextOverflow".to_string(),
                    message_id: format!("caps-missing-{}", current_model),
                });
            }

            // ADR-067: no response tail auto-compaction. The single 90%
            // projected-input compaction line is enforced right before the
            // next LLM request (pre-request path), where the projected input
            // reflects the summary of history + this just-completed response.
        }
    }

    /// ⑥ Pre-append enforcement of the SINGLE 90% projected-input compaction
    /// line — runs the 5-level plan before tool results land in history when
    /// `projected input + estimated_result_tokens` already crosses the line.
    ///
    /// This is NOT a separate threshold: it uses the same
    /// [`Self::projected_exceeds_compact_threshold`] denominator and 90%
    /// cut-off as the pre-request check (the old 70%-of-usable pre-trim was
    /// removed — see file header). Compacting here instead of one iteration
    /// later keeps the tool results from ever being appended above the line.
    ///
    /// Returns `Ok(())` when no compaction was needed or it succeeded; on LLM
    /// failure the caller aborts the iteration (→ GiveUp → Idle) rather than
    /// swallowing the error and continuing to grow the context.
    pub(crate) async fn pre_trim_for_tool_results(
        &mut self,
        tool_results: &[String],
        current_model: &str,
    ) -> crate::error::Result<()> {
        let result_tokens_estimate: u64 = tool_results
            .iter()
            .map(|r| crate::token::count_text(r) as u64)
            .sum();
        if result_tokens_estimate > 0
            && self.projected_exceeds_compact_threshold(current_model, result_tokens_estimate)
        {
            tracing::info!(
                projected = self.projected_input_tokens(),
                result_tokens_estimate,
                threshold = self.compact_threshold_tokens(current_model),
                budget = self.context_trim_budget(current_model),
                "Projected input + pending tool results at compaction threshold — \
                 compacting before append"
            );
            self.compact_history_if_needed(current_model, true).await?;
        }
        Ok(())
    }

    /// ⑥.5 Context-aware tool result trimming — truncate individual tool
    /// results so they don't overflow the context window when appended.
    ///
    /// After [`pre_trim_for_tool_results`] trims history, this ensures the
    /// tool results themselves fit within the remaining context budget.
    /// Each result gets a proportional share of the remaining space, and
    /// truncated results carry a detailed marker so the LLM can adapt its
    /// strategy (narrower search, pagination, etc.).
    ///
    /// `exempt[i]` marks result `i` as un-cuttable — currently only
    /// `load_skill` bodies (ADR-094 follow-up). The equal-share rule is wrong
    /// for a SKILL.md: it is the procedure the model is mid-way through
    /// executing, and the share is set by how many *other* tools happened to
    /// run in the same turn, so a 3-call turn can amputate a perfectly
    /// reasonable skill body at a third of its size. The model cannot "re-run
    /// with narrower parameters" either — `load_skill` has no pagination.
    ///
    /// Exempt results are still counted in `n`, so the co-resident results get
    /// a correspondingly smaller share — but that is the *only* thing exempt
    /// status buys: it never buys extra space. The exempt results collectively
    /// draw on what is left of `remaining` after the equal-share split
    /// (see [`apply_result_budget`]), so the total the model is asked to accept
    /// still fits the budget this function exists to enforce. Charging the
    /// exempt result against `remaining` alone would have let N of them claim
    /// N × the free space.
    ///
    /// Exemption is therefore not unconditional: a result that exceeds the
    /// space still unclaimed is truncated, because a request that cannot be
    /// sent cannot compact, re-load the skill, or make progress in any way.
    ///
    /// Returns the number of results that were truncated.
    pub(crate) fn trim_tool_results_for_context(
        &self,
        tool_results: &mut [String],
        current_model: &str,
        exempt: &[bool],
    ) -> usize {
        let usable_budget = self.context_trim_budget(current_model);
        let current_tokens = self.session.history.token_count();

        // Safety margin: reserve 15% of the usable budget for the LLM's
        // next response (output tokens), tool call metadata, and message
        // serialisation overhead. Without this, the trimmed results may
        // fit the remaining space but the follow-up LLM response will
        // immediately overflow again.
        //
        // `CONTEXT_SAFETY_FACTOR` is shared with the skill-recall budget
        // (`compression_constants::skill_recall_budget_tokens`) on purpose:
        // both paths decide how much of the window counts as "free", and two
        // independent literals here would let them fight over the same bytes.
        let safe_budget = (usable_budget as f64 * CONTEXT_SAFETY_FACTOR) as u64;
        let remaining = safe_budget.saturating_sub(current_tokens);

        if remaining == 0 || tool_results.is_empty() {
            return 0;
        }

        // Count tokens per result
        let result_tokens: Vec<u64> = tool_results
            .iter()
            .map(|r| crate::token::count_text(r) as u64)
            .collect();
        let total_result_tokens: u64 = result_tokens.iter().sum();

        if total_result_tokens <= remaining {
            return 0; // Everything fits
        }

        let n = tool_results.len() as u64;
        // Minimum budget per result: 256 tokens ensures the LLM still
        // gets meaningful output. Below this, every result would be
        // reduced to just the truncation marker, which is unhelpful.
        let min_per_result: u64 = 256;
        let per_result_budget = (remaining / n).max(min_per_result);

        tracing::warn!(
            current_tokens,
            total_result_tokens,
            remaining,
            per_result_budget,
            result_count = n,
            usable_budget,
            "Tool results exceed remaining context budget — truncating"
        );

        apply_result_budget(tool_results, &result_tokens, remaining, per_result_budget, exempt)
    }
}

/// The pure trimming policy behind
/// [`AgentLoop::trim_tool_results_for_context`], split out so the three
/// branches (equal share / exempt pass-through / exempt-but-over-everything)
/// are testable without constructing an agent loop, a history, or a model.
///
/// `remaining` is the space free after the safety margin; `per_result_budget`
/// is that space split across all results. `exempt[i]` protects result `i` from
/// the split (see the caller's doc for why), but never from the budget itself.
///
/// Exempt results draw from one shared pool — `remaining` minus what the
/// equal-share split will hand the others — instead of each capping itself at
/// `remaining`. That is what keeps `Σ kept <= remaining`: with a per-result cap
/// of `remaining`, N exempt results in a single turn could each claim the whole
/// free space and the request this trim exists to make fit would go out over
/// budget. Building the pool up front also makes the outcome independent of the
/// order the results happen to arrive in.
fn apply_result_budget(
    tool_results: &mut [String],
    result_tokens: &[u64],
    remaining: u64,
    per_result_budget: u64,
    exempt: &[bool],
) -> usize {
    let n = tool_results.len() as u64;

    // Space left for the exempt results after the non-exempt ones take their
    // equal share. Both terms on the right are already-known inputs, so the
    // whole policy stays order-independent.
    let mut exempt_left: u64 = remaining;
    for (i, &tokens) in result_tokens.iter().enumerate() {
        if exempt.get(i).copied().unwrap_or(false) {
            continue;
        }
        exempt_left = exempt_left.saturating_sub(tokens.min(per_result_budget));
    }

    let mut truncated = 0;
    for (i, result) in tool_results.iter_mut().enumerate() {
        let orig_tokens = result_tokens[i];
        let is_exempt = exempt.get(i).copied().unwrap_or(false);

        // Exempt results skip the equal-share cut, but not the hard limit:
        // if this result alone exceeds what is still unclaimed, no amount of
        // trimming elsewhere makes room for it.
        let result_budget = if is_exempt {
            let budget = exempt_left;
            // Reserve what this result keeps (all of it when it fits) so a
            // later exempt result cannot claim the same bytes again.
            exempt_left -= orig_tokens.min(exempt_left);
            if orig_tokens <= budget {
                continue;
            }
            budget
        } else {
            if orig_tokens <= per_result_budget {
                continue;
            }
            per_result_budget
        };

        let original_bytes = result.len();
        // Convert token budget to char budget (≈ 4 chars/token for CJK, ≈ 3 for ASCII).
        // Use 3.5 as a middle-ground multiplier and add 10% padding for safety.
        let max_chars = ((result_budget as f64) * 3.5 * 1.1) as usize;

        // Find a UTF-8 safe cut point within budget.
        // Replaced ad-hoc loop with the project-wide `util::text::truncate_utf8`
        // so the same character-boundary guarantee lives in exactly one place.
        let safe = crate::util::text::truncate_utf8(result, max_chars.min(result.len()));
        let mut cut = safe.len();

        // Try to cut at a newline for readability
        if let Some(nl_pos) = safe.rfind('\n').filter(|&p| p > cut / 2) {
            cut = nl_pos;
        }

        let kept = &safe[..cut];
        let dropped_tokens = orig_tokens.saturating_sub(result_budget);

        let truncation_marker = if is_exempt {
            // The generic "re-run with narrower parameters" advice is
            // meaningless for a skill body, and silently presenting a
            // partial procedure as if it were complete is worse: the model
            // would follow the visible steps and never notice the missing
            // ones. Say exactly what happened.
            format!(
                "\n\n[SKILL BODY INCOMPLETE — the full SKILL.md needs ~{orig_tokens} tokens \
                 but only ~{result_budget} tokens of context budget are available for it at \
                 this point ({remaining} tokens in total are free, and the other results in \
                 this turn have already claimed their share), so ~{dropped_tokens} tokens \
                 were cut. Everything above is verbatim; the steps below this point are MISSING. \
                 Do NOT assume you know them. Work through what is shown, and re-run `load_skill` \
                 once the context has compacted if the remaining steps are needed.]",
            )
        } else {
            format!(
                "\n\n[RESULT TRUNCATED by context budget: original output was {} bytes \
                 (~{} tokens), but only {result_budget} tokens of context budget remain \
                 per result (total remaining: {remaining} tokens across {n} result(s)). \
                 ~{dropped_tokens} tokens of this output were dropped. \
                 SUGGESTION: re-run with narrower parameters (pipe through 'head -N' or \
                 'tail -N' for pagination, use tighter grep patterns to reduce matches, \
                 or search fewer files/directories) to get a complete result.]",
                original_bytes, orig_tokens,
            )
        };

        *result = format!("{kept}{truncation_marker}");
        truncated += 1;

        tracing::warn!(
            i,
            original_bytes,
            orig_tokens,
            kept_bytes = cut,
            result_budget,
            exempt = is_exempt,
            "Tool result #{i} truncated to fit context budget"
        );
    }

    truncated
}

/// Design 27 §6.5 R1 + §5.5.4 (plan 2b.3): the per-turn wire tool-set
/// removals. `intent_send` is gated by ROLE (responder turns answer and
/// hand back, they must not spawn a nested Intent); `ask_user_question`
/// is gated by ANSWERABILITY (only a session *known* to have no human
/// participant hides it — `None` fails open so older Gateways and
/// chat-less deliveries keep today's behaviour).
pub(crate) fn hidden_tools_for_turn(
    responder: bool,
    has_human: Option<bool>,
) -> &'static [&'static str] {
    const BOTH: [&str; 2] = ["intent_send", "ask_user_question"];
    const NO_HUMAN: [&str; 1] = ["ask_user_question"];
    const RESPONDER: [&str; 1] = ["intent_send"];
    match (responder, has_human) {
        (true, Some(false)) => &BOTH,
        (true, _) => &RESPONDER,
        (false, Some(false)) => &NO_HUMAN,
        (false, _) => &[],
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────
//
// ADR-056 §9.1: unit tests for `resolve_distill_model` three-tier fallback.
//
// We exercise the resolver end-to-end by constructing a real `AgentLoop`
// (per the test scaffolding already in `loop_.rs::tests`) and then
// mutating its `AgentCore` fields (`global_provider_list`,
// `provider_key_vault`, `provider_compact_models`, `default_compact_models`)
// to stage each scenario. The resolver is purely a synchronous,
// self-contained function on `AgentLoop`, so this approach covers every
// branch without mocking anything.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::agent_core::BuiltinToolEntry;
    use crate::agent::context::ContextBuilder;
    use crate::config::RuntimeConfig;
    use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry};
    use acowork_core::providers::mock::{MockProvider, MockResponse};
    use acowork_core::providers::traits::{
        ChatMessage, ChatRequest, ChatResponse, MessageRole, StreamEvent, UsageInfo,
    };
    use acowork_core::{AgentManifest, Budget};
    use std::sync::Arc;

    // ── Fixtures ────────────────────────────────────────────────────────

    fn minimal_manifest() -> AgentManifest {
        AgentManifest::from_toml(
            r#"
            agent_id = "com.test.compact"
            version = "1.0.0"
            name = "Compact Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "deepseek"
            model = "deepseek-v4-flash"
            "#,
        )
        .expect("manifest must parse")
    }

    fn minimal_config() -> RuntimeConfig {
        RuntimeConfig::default()
    }

    fn caps(context_window: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens: 4096,
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
        }
    }

    fn build_loop() -> AgentLoop {
        let provider = Arc::new(MockProvider::single_text("ok"));
        let tools: Vec<BuiltinToolEntry> = vec![];
        let budget = Budget {
            daily_tokens: Some(100_000),
            monthly_tokens: None,
            daily_cost_usd: Some(10.0),
            monthly_cost_usd: None,
            exceeded_action: "warn".to_string(),
        };
        let (loop_, _inbound_tx) = AgentLoop::new(
            minimal_config(),
            minimal_manifest(),
            provider,
            tools,
            budget,
            None,
            None,
        );
        loop_
    }

    /// Stage two providers in the AgentCore's global_provider_list:
    ///   `deepseek`     → deepseek-v4-flash (large), deepseek-v4-pro (large)
    ///   `ollama-local` → qwen2.5:0.5b (32K), llama3:8b (8K)
    ///
    /// Only `deepseek` gets an API key — `ollama-local` is a local
    /// provider with an empty key, mirroring the real-world setup where
    /// Ollama needs no key. `is_compact_provider_available` must
    /// accept it via the local base_url branch (ADR-056 §2.3).
    fn seed_providers(loop_: &AgentLoop) {
        let mut list = loop_.core.global_provider_list.write().unwrap();
        *list = vec![
            ProviderListItem {
                id: "deepseek".to_string(),
                base_url: "https://api.deepseek.com/v1".to_string(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                compact_model: Some("deepseek-v4-flash".to_string()),
                custom: false,
                models: vec![
                    ProviderModelEntry {
                        id: "deepseek-v4-flash".to_string(),
                        capabilities: caps(128_000),
                        max_output_tokens_limit: 32_768,
                    },
                    ProviderModelEntry {
                        id: "deepseek-v4-pro".to_string(),
                        capabilities: caps(128_000),
                        max_output_tokens_limit: 32_768,
                    },
                ],
            },
            ProviderListItem {
                id: "ollama-local".to_string(),
                base_url: "http://localhost:11434/v1".to_string(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                compact_model: None,
                custom: false,
                models: vec![
                    ProviderModelEntry {
                        id: "qwen2.5:0.5b".to_string(),
                        capabilities: caps(32_000),
                        max_output_tokens_limit: 16_384,
                    },
                    ProviderModelEntry {
                        id: "llama3:8b".to_string(),
                        capabilities: caps(8_000),
                        max_output_tokens_limit: 8_192,
                    },
                ],
            },
        ];
        drop(list);

        // Only the cloud provider (deepseek) gets a key; ollama-local stays
        // keyless (local provider, no key required).
        let mut keys = loop_.core.provider_key_vault.write().unwrap();
        keys.insert(
            "deepseek".to_string(),
            vec![acowork_core::protocol::ProviderKeyEntry {
                provider_id: "deepseek".to_string(),
                account_id: String::new(),
                api_key: "sk-test-deepseek".to_string(),
            }],
        );
    }

    fn set_session(loop_: &mut AgentLoop, provider: &str, model: &str) {
        loop_.session.provider = Some(provider.to_string());
        loop_.session.model = Some(model.to_string());
    }

    fn set_provider_compact(loop_: &mut AgentLoop, provider: &str, compact: Option<&str>) {
        loop_
            .core
            .provider_compact_models
            .write()
            .unwrap()
            .insert(provider.to_string(), compact.map(|s| s.to_string()));
    }

    // ── resolve_distill_model: three-tier fallback ─────────────────────

    #[test]
    fn build_chat_request_current_user_message_appears_once_in_block_b() {
        // ADR-060 v2 §5.5 + erratum: Block B is the full append-only history
        // — it INCLUDES the current user message at its tail. There is NO
        // Block D duplicate. Request order: A → B (Block C and Block D both
        // removed). The current user message must appear EXACTLY ONCE.
        let mut loop_ = build_loop();
        loop_
            .session
            .history
            .append(ChatMessage::user("First turn"));
        loop_
            .session
            .history
            .append(ChatMessage::assistant("First reply"));
        let current = ChatMessage::user("Second turn");
        loop_.session.history.append(current.clone());
        // ADR-060 v2: setting todos via SessionState no longer affects the
        // request layout (todo state lives in Block B history, surfaced by
        // the most recent `todo_write` tool result). Kept commented for
        // context — verify the request layout is independent of todo state.
        // loop_.session.update_todos(vec![TodoItem { ... }], false);

        let mut builder = ContextBuilder::new("Kernel".to_string());
        let request = loop_.build_chat_request(&mut builder, "test-model");

        // [0] Block A: system kernel.
        assert_eq!(request.messages[0].role, MessageRole::System);
        assert!(
            !request.messages[0].content.contains("Todo Task List"),
            "Block A must not contain a todo snapshot"
        );

        // Block B: history turns present, including the current user message.
        assert!(
            request
                .messages
                .iter()
                .any(|m| m.role == MessageRole::User && m.content == "Second turn"),
            "Block B must include the current user message"
        );

        // ADR-060 v2: NO Block C. The "## Todo Task List" header must not
        // appear in the request (todo state is carried by Block B's real
        // todo_write tool results, not a synthetic tail snapshot).
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.content.contains("Todo Task List")),
            "ADR-060 v2: no Block C — todo state lives in Block B"
        );

        // The current user message is at Block B's tail (last message) and
        // appears EXACTLY ONCE — no trailing Block D duplicate.
        let last = request.messages.last().unwrap();
        assert_eq!(last.role, MessageRole::User);
        assert_eq!(last.content, current.content);
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|m| m.content == "Second turn")
                .count(),
            1,
            "current user message appears exactly once (Block B tail, no Block D)"
        );
    }

    #[test]
    fn build_chat_request_no_duplicate_in_tool_iteration() {
        use acowork_core::providers::traits::{FunctionCall, ToolCall};

        // ADR-060 §5.5 erratum: tool-loop iterations end with the tool
        // result; the original user turn appears only once (in Block B).
        let mut loop_ = build_loop();
        loop_.session.history.append(ChatMessage::user("Turn"));
        // A REAL tool_call on the assistant turn keeps the tool result
        // from being classified as orphaned by sanitize_messages.
        loop_
            .session
            .history
            .append(ChatMessage::assistant_with_tools(
                "",
                vec![ToolCall {
                    id: "toolu_1".to_string(),
                    call_type: "function".to_string(),
                    function: FunctionCall {
                        name: "test_tool".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
            ));
        loop_
            .session
            .history
            .append(ChatMessage::tool("toolu_1", "ok"));

        let mut builder = ContextBuilder::new("Kernel".to_string());
        let request = loop_.build_chat_request(&mut builder, "test-model");

        let last = request.messages.last().unwrap();
        assert_eq!(
            last.role,
            MessageRole::Tool,
            "tool iteration: last message must be the tool result, no Block D"
        );
        assert_eq!(
            request
                .messages
                .iter()
                .filter(|m| m.content == "Turn")
                .count(),
            1,
            "no Block D means the user turn appears exactly once"
        );
    }

    #[tokio::test]
    async fn auto_inject_does_not_set_flag_without_provider() {
        // ADR-060 §6.3: `memory_retrieved_for_session` is set only on a
        // SUCCESSFUL first retrieval. With no memory provider the call
        // bails early and the flag must stay false — a transient provider
        // absence must not permanently starve the session.
        let mut loop_ = build_loop();
        assert!(loop_.core.memory_provider().is_none());
        let mut builder = ContextBuilder::new("Kernel".to_string());
        loop_
            .retrieve_and_inject_memories("hello", &mut builder)
            .await;
        assert!(
            !loop_.memory_retrieved_for_session,
            "no provider → no retrieval → flag stays false"
        );
        // Repeated calls keep retrying (flag still false).
        loop_
            .retrieve_and_inject_memories("hello again", &mut builder)
            .await;
        assert!(!loop_.memory_retrieved_for_session);
    }

    #[test]
    fn tier1_global_default_is_picked_when_set_and_available() {
        // ADR-056 §1, §3.2: Global default wins when present and valid.
        // Real-world flagship scenario: local Ollama model selected as the
        // cross-provider default while the session chats with deepseek —
        // ollama-local has NO API key here and must still be accepted via
        // the local base_url branch.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];

        let resolved = loop_.resolve_distill_model("hello world");
        assert_eq!(resolved.provider_id, "ollama-local");
        assert_eq!(resolved.model_id, "qwen2.5:0.5b");
        assert!(matches!(resolved.tier, DistillTier::GlobalDefault));
    }

    #[test]
    fn tier1_global_default_falls_through_when_provider_unavailable() {
        // ADR-056 §3.2 / §7: If the global provider has no API key,
        // fall back to Level 2 (provider.compact_model).
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        *loop_.core.default_compact_models.write().unwrap() = vec![("anthropic".to_string(), "claude-3-haiku".to_string())]; // not seeded

        let resolved = loop_.resolve_distill_model("hello world");
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
    }

    #[test]
    fn tier1_global_default_falls_through_when_context_too_small() {
        // ADR-056 §3.2 / §7: Global model exists but context_window < estimate.
        // The estimated token count from `count_text("hello world")` is tiny,
        // but we force the scenario by using a model with a tiny context_window
        // that is below the estimate when the input is large enough.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        // Pick the tiny 8K model as global default; the estimated tokens for
        // a long input will exceed 8K → fall back.
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "llama3:8b".to_string())];

        // 50 KB of text → ~13k+ tokens by char/4 estimate, well above 8K.
        let long_input = "x".repeat(50_000);
        let resolved = loop_.resolve_distill_model(&long_input);
        // Tier 1 fails (context too small) → Tier 2 fires.
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
    }

    #[test]
    fn tier1_falls_through_when_model_not_in_capabilities() {
        // ADR-056 §7: If global default points at a model_id that doesn't
        // exist in any provider's models[], warn and fall back to Tier 2.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));

        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "ghost-model-99".to_string())];

        let resolved = loop_.resolve_distill_model("hi");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
    }

    #[test]
    fn targets_list_does_not_promote_global_rejected_by_selection_phase() {
        // The selection phase (`resolve_distill_model`) rejects a global
        // candidate whose model_id is missing from every provider's
        // `models[]` (capabilities lookup fails) and falls through to
        // ProviderCompact. The call-phase chain must honour that verdict:
        // `try_global_default_targets` only filters on *provider*
        // availability, so without an explicit guard the rejected
        // candidate would be re-promoted to the head of the chain — the
        // chain would then contradict the tier `resolve_distill_model`
        // reported, and `compact_history_if_needed` would log a
        // GlobalDefault tier the selection phase never chose.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() =
            vec![("ollama-local".to_string(), "ghost-model-99".to_string())];

        // Selection phase rejects the ghost model and picks provider tier.
        let resolved = loop_.resolve_distill_model("hi");
        assert!(
            matches!(resolved.tier, DistillTier::ProviderCompact),
            "precondition: selection phase must reject the unknown model, got {resolved:?}"
        );

        let targets = loop_.resolve_distill_targets();
        assert!(
            !matches!(targets.first().map(|t| t.tier), Some(DistillTier::GlobalDefault)),
            "call-phase chain must not lead with a global the selection phase rejected, got {targets:?}"
        );
    }

    #[test]
    fn no_global_default_goes_straight_to_tier2_provider_compact() {
        // ADR-056 §3.2 base case: global default is None -> Tier 2 fires.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        assert!(loop_.core.default_compact_models.read().unwrap().is_empty());

        let resolved = loop_.resolve_distill_model("hi");
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-flash");
        assert!(matches!(resolved.tier, DistillTier::ProviderCompact));
    }

    #[test]
    fn tier1_and_tier2_both_fail_goes_to_tier3_current_chat() {
        // Both global default and provider.compact_model are unusable.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        // Provider compact missing entirely.
        set_provider_compact(&mut loop_, "deepseek", None);

        // Global default -> unknown provider, will fall through.
        *loop_.core.default_compact_models.write().unwrap() = vec![("anthropic".to_string(), "claude-3-haiku".to_string())];

        let resolved = loop_.resolve_distill_model("hi");
        assert_eq!(resolved.provider_id, "deepseek");
        assert_eq!(resolved.model_id, "deepseek-v4-pro");
        assert!(matches!(resolved.tier, DistillTier::CurrentChat));
    }

    #[test]
    fn tier3_falls_back_to_chat_model_when_session_provider_empty() {
        // Edge case: no session provider at all -> Tier 3 still resolves
        // to the current chat model with empty provider_id.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        loop_.session.provider = None;
        loop_.session.model = Some("deepseek-v4-pro".to_string());
        set_provider_compact(&mut loop_, "deepseek", None);
        *loop_.core.default_compact_models.write().unwrap() = Vec::new();

        let resolved = loop_.resolve_distill_model("hi");
        assert_eq!(resolved.provider_id, "");
        assert_eq!(resolved.model_id, "deepseek-v4-pro");
        assert!(matches!(resolved.tier, DistillTier::CurrentChat));
    }

    // ── helpers: is_compact_provider_available ────────────────

    #[test]
    fn default_compact_provider_available_local_without_key() {
        // ADR-056 §2.3: local provider (Ollama) needs no API key — the
        // availability check must accept it via the local base_url branch,
        // otherwise the flagship "chat=deepseek, distill=ollama" scenario
        // would never fire.
        let loop_ = build_loop();
        seed_providers(&loop_);
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];
        assert!(
            loop_.core.is_compact_provider_available("ollama-local"),
            "local provider without key must be usable"
        );
    }

    #[test]
    fn default_compact_provider_unavailable_cloud_without_key() {
        // ADR-056 §7: cloud provider whose key was revoked is NOT callable
        // → distillation falls back to Level 2.
        let loop_ = build_loop();
        seed_providers(&loop_);
        *loop_.core.default_compact_models.write().unwrap() = vec![("deepseek".to_string(), "deepseek-v4-flash".to_string())];
        assert!(loop_.core.is_compact_provider_available("deepseek"));

        // Revoke the key -> no longer available.
        loop_
            .core
            .provider_key_vault
            .write()
            .unwrap()
            .remove("deepseek");
        assert!(!loop_.core.is_compact_provider_available("deepseek"));
    }

    #[test]
    fn default_compact_provider_unavailable_when_not_set() {
        let loop_ = build_loop();
        assert!(!loop_.core.is_compact_provider_available("anthropic"));
    }

    // ── Distill resolver prefers compact model over chat model ───────
    //
    // Regression check for the bug fix in ADR-056 §5.2: the three-tier
    // chain must prefer the compact model over the chat model. We can
    // prove the resolver picks the compact model when one is available —
    // which means the chat model's own context_window never demotes the
    // resolver.

    #[test]
    fn resolved_distill_uses_compact_model_when_global_default_available() {
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        // Current chat model has small-ish context.
        set_session(&mut loop_, "deepseek", "deepseek-v4-flash");
        set_provider_compact(&mut loop_, "deepseek", None); // no Tier 2

        // Global default has ample context.
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];

        let resolved = loop_.resolve_distill_model("some long text");
        assert_eq!(resolved.provider_id, "ollama-local");
        assert_eq!(resolved.model_id, "qwen2.5:0.5b");
        assert!(matches!(resolved.tier, DistillTier::GlobalDefault));
    }

    // ── resolve_distill_targets: call-phase fallback chain ────────────

    #[test]
    fn targets_list_returns_three_tiers_in_priority_order() {
        // All three tiers are reachable → list = [GlobalDefault,
        // ProviderCompact, CurrentChat].
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];

        let targets = loop_.resolve_distill_targets();
        assert_eq!(targets.len(), 3, "expected three targets, got {targets:?}");
        assert!(matches!(targets[0].tier, DistillTier::GlobalDefault));
        assert!(matches!(targets[1].tier, DistillTier::ProviderCompact));
        assert!(matches!(targets[2].tier, DistillTier::CurrentChat));
        assert_eq!(targets[0].provider_id, "ollama-local");
        assert_eq!(targets[1].provider_id, "deepseek");
        assert_eq!(targets[1].model_id, "deepseek-v4-flash");
        assert_eq!(targets[2].model_id, "deepseek-v4-pro");
    }

    #[test]
    fn targets_list_expands_multiple_global_candidates_in_order() {
        // ADR-056 list follow-up: the whole global list participates in the
        // call-phase chain, in the user's order, ahead of the provider tier.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() = vec![
            ("ollama-local".to_string(), "qwen2.5:0.5b".to_string()),
            ("deepseek".to_string(), "deepseek-v4-flash".to_string()),
        ];

        let targets = loop_.resolve_distill_targets();
        // global#1, global#2 (== provider compact, deduped), CurrentChat.
        assert_eq!(targets.len(), 3, "expected three targets, got {targets:?}");
        assert_eq!(targets[0].provider_id, "ollama-local");
        assert_eq!(targets[0].model_id, "qwen2.5:0.5b");
        assert_eq!(targets[1].provider_id, "deepseek");
        assert_eq!(targets[1].model_id, "deepseek-v4-flash");
        assert_eq!(targets[2].tier, DistillTier::CurrentChat);
        assert_eq!(targets[2].model_id, "deepseek-v4-pro");
    }

    #[test]
    fn resolve_skips_first_global_candidate_when_provider_unavailable() {
        // First candidate lives on a keyless cloud provider (anthropic) →
        // selection must step down to the second candidate, not fall to
        // the provider tier.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() = vec![
            ("anthropic".to_string(), "claude-3-haiku".to_string()),
            ("ollama-local".to_string(), "qwen2.5:0.5b".to_string()),
        ];

        let resolved = loop_.resolve_distill_model("hello world");
        assert_eq!(resolved.provider_id, "ollama-local");
        assert_eq!(resolved.model_id, "qwen2.5:0.5b");
        assert!(matches!(resolved.tier, DistillTier::GlobalDefault));
    }

    #[test]
    fn resolve_skips_first_global_candidate_when_context_too_small() {
        // llama3:8b (8K window) is first but the content exceeds it →
        // step down to qwen2.5:0.5b (32K) while staying in the global tier.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        *loop_.core.default_compact_models.write().unwrap() = vec![
            ("ollama-local".to_string(), "llama3:8b".to_string()),
            ("ollama-local".to_string(), "qwen2.5:0.5b".to_string()),
        ];

        // 60K chars / 3.5 ratio ≈ 17K tokens: > llama3:8b's 8K window,
        // < qwen2.5:0.5b's 32K window.
        let big_text = "hello ".repeat(10_000);
        let resolved = loop_.resolve_distill_model(&big_text);
        assert_eq!(resolved.model_id, "qwen2.5:0.5b");
        assert!(matches!(resolved.tier, DistillTier::GlobalDefault));
    }

    #[test]
    fn targets_list_collapses_to_current_chat_when_only_tier3_is_available() {
        // No global default + no provider compact → only Level 3 is reachable.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", None);
        assert!(loop_.core.default_compact_models.read().unwrap().is_empty());

        let targets = loop_.resolve_distill_targets();
        assert_eq!(targets.len(), 1, "expected one target, got {targets:?}");
        assert!(matches!(targets[0].tier, DistillTier::CurrentChat));
        assert_eq!(targets[0].model_id, "deepseek-v4-pro");
    }

    #[test]
    fn targets_list_dedups_duplicate_provider_model_pairs() {
        // Edge case: all three tiers resolve to the same (provider, model)
        // (session pid = deepseek, chat = deepseek-v4-flash, provider compact
        // = deepseek-v4-flash, global default = deepseek-v4-flash). De-dup
        // must collapse the list to a single entry so the caller does not
        // retry the same broken target three times.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-flash");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() = vec![("deepseek".to_string(), "deepseek-v4-flash".to_string())];

        let targets = loop_.resolve_distill_targets();
        let unique: std::collections::HashSet<(String, String)> = targets
            .iter()
            .map(|t| (t.provider_id.clone(), t.model_id.clone()))
            .collect();
        assert_eq!(
            unique.len(),
            targets.len(),
            "duplicate (provider, model) pairs not allowed"
        );
        assert!(
            targets.len() <= 2,
            "expected at most 2 distinct targets (global default + maybe one other), got {targets:?}"
        );
        assert!(
            targets
                .iter()
                .any(|t| matches!(t.tier, DistillTier::GlobalDefault))
        );
    }

    // ── compact_history_if_needed: call-phase fallback integration ────

    /// Wrap a MockProvider so the FIRST call to chat() returns Error and
    /// subsequent calls return Text. The shared state (`Arc<Mutex>`) lets
    /// the integration test inspect the model names of every call.
    struct Tier1FailThenSucceedProvider {
        responses: std::sync::Arc<std::sync::Mutex<Vec<MockResponse>>>,
        call_log: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Tier1FailThenSucceedProvider {
        fn new(fail_count: usize, success_text: &str) -> Self {
            let mut responses = Vec::new();
            for i in 0..fail_count {
                responses.push(MockResponse::Error {
                    message: format!("provider failed at call {i}"),
                });
            }
            responses.push(MockResponse::Text {
                content: success_text.to_string(),
            });
            Self {
                responses: std::sync::Arc::new(std::sync::Mutex::new(responses)),
                call_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
        fn log(&self) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
            self.call_log.clone()
        }
        /// Build from an explicit response script — used by tests that need
        /// text responses of varying quality (e.g. a LowQuality first call).
        fn from_responses(responses: Vec<MockResponse>) -> Self {
            Self {
                responses: std::sync::Arc::new(std::sync::Mutex::new(responses)),
                call_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for Tier1FailThenSucceedProvider {
        fn name(&self) -> &str {
            "tier1-fail-then-succeed"
        }
        async fn chat(&self, request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
            self.call_log.lock().unwrap().push(request.model.clone());
            let mut resp = self.responses.lock().unwrap();
            let next = if resp.is_empty() {
                MockResponse::Text {
                    content: "default".to_string(),
                }
            } else {
                resp.remove(0)
            };
            match next {
                MockResponse::Text { content } => Ok(ChatResponse {
                    content,
                    usage: Some(UsageInfo {
                        prompt_tokens: 50,
                        completion_tokens: 25,
                        total_tokens: 75,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                MockResponse::Error { message } => Err(acowork_core::AcoworkError::Provider(
                    acowork_core::providers::ProviderError::unknown(message),
                )),
                _ => unimplemented!(),
            }
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<Box<dyn futures_core::Stream<Item = StreamEvent> + Send>>
        {
            unimplemented!()
        }
        async fn chat_token_count(
            &self,
            messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(messages.iter().map(|m| m.content.len() as u64 / 4).sum())
        }
    }

    /// Provider that ALWAYS errors (for the "all tiers fail" test).
    struct AlwaysFailProvider;

    #[async_trait::async_trait]
    impl Provider for AlwaysFailProvider {
        fn name(&self) -> &str {
            "always-fail"
        }
        async fn chat(&self, _request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
            Err(acowork_core::AcoworkError::Provider(
                acowork_core::providers::ProviderError::unknown("permanent failure".to_string()),
            ))
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<Box<dyn futures_core::Stream<Item = StreamEvent> + Send>>
        {
            unimplemented!()
        }
        async fn chat_token_count(
            &self,
            _messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn call_phase_fallback_skips_failing_tier_and_uses_next() {
        // Scenario: GlobalDefault = ollama-local (unreachable in tests),
        // so the resolver falls back to Tier 2 (ProviderCompact =
        // deepseek-v4-flash) then Tier 3 (CurrentChat = deepseek-v4-pro).
        // We install a session provider mock that fails on the FIRST call
        // (= Tier 2: deepseek-v4-flash) and succeeds thereafter (= Tier 3:
        // deepseek-v4-pro). The integration test asserts:
        //   - Tier 2 was attempted and failed
        //   - Tier 3 was attempted next and succeeded
        //   - History was compacted (compaction marker present)
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        // GlobalDefault points to ollama-local which is NOT seeded as a
        // provider with a key, so `resolve_distill_model` skips it at
        // selection time and starts at Tier 2.
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];

        let _targets = loop_.resolve_distill_targets();
        // The session_pid is deepseek; deepseek has an API key, so the
        // selection-time fallback should NOT exclude Tier 1 here.
        // To force the *call-phase* fallback, install a provider mock
        // that fails on the first call regardless of which tier it is.
        // Three targets → three attempts. First call fails, second succeeds.
        let fail_once = Tier1FailThenSucceedProvider::new(
            1,
            "<summary>a valid compact model summary output here</summary>",
        );
        let call_log = fail_once.log();
        loop_.core.update_provider(
            std::sync::Arc::new(fail_once) as std::sync::Arc<dyn Provider>,
            "deepseek-v4-pro".to_string(),
        );

        // Build a non-trivial history so compaction has something to do.
        for i in 0..10 {
            loop_.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("Message #{i}"),
                ..Default::default()
            });
            loop_.session.history.append(ChatMessage {
                role: MessageRole::Assistant,
                content: format!("Reply #{i}"),
                ..Default::default()
            });
        }

        let _ = loop_
            .compact_history_if_needed("deepseek-v4-pro", true)
            .await;

        let log = call_log.lock().unwrap();
        assert!(
            log.len() >= 2,
            "expected fallback to retry; actual call log: {log:?}"
        );

        // History was actually compacted (replace_middle_with_summary ran)
        let has_marker = loop_
            .session
            .history
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME));
        assert!(
            has_marker,
            "compaction marker should be present after fallback succeeded"
        );
    }

    #[tokio::test]
    async fn call_phase_fallback_when_all_tiers_fail_does_fifo_trim() {
        // When ALL tiers fail (provider mock errors forever), history must
        // fall back to trim_fifo + emergency_trim — the documented
        // last-resort behaviour. The user loses early context, but the
        // session can still continue (better than a panic or hanging
        // request).
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];

        loop_.core.update_provider(
            std::sync::Arc::new(AlwaysFailProvider) as std::sync::Arc<dyn Provider>,
            "deepseek-v4-pro".to_string(),
        );

        for i in 0..20 {
            loop_.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("Long message {i} ").repeat(50),
                ..Default::default()
            });
        }

        let tokens_before = loop_.session.history.token_count();
        let _ = loop_
            .compact_history_if_needed("deepseek-v4-pro", true)
            .await;
        let tokens_after = loop_.session.history.token_count();

        // No compaction marker — the trim path was taken instead.
        let has_marker = loop_
            .session
            .history
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME));
        assert!(
            !has_marker,
            "no compaction marker should exist when LLM compaction fully failed"
        );

        // History must not grow (FIFO+emergency path is the floor).
        assert!(
            tokens_after <= tokens_before,
            "FIFO path must not grow history: before={tokens_before} after={tokens_after}"
        );
    }

    #[tokio::test]
    async fn call_phase_fallback_stops_on_low_quality_without_stepping_down() {
        // LowQuality means the top tier's model cannot produce a usable
        // summary — stepping down the chain only gets cheaper/weaker, so
        // the chain must STOP (quality-over-nothing, P1), not retry with a
        // weaker model. Script: call 1 returns a placeholder summary
        // (< 20 chars → LowQuality), call 2 would return a valid summary.
        // If the chain wrongly stepped down, call 2 would succeed and
        // history would be compacted — the test asserts neither happens.
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        *loop_.core.default_compact_models.write().unwrap() = vec![("ollama-local".to_string(), "qwen2.5:0.5b".to_string())];

        let provider = Tier1FailThenSucceedProvider::from_responses(vec![
            MockResponse::Text {
                content: "<summary>ok</summary>".to_string(),
            },
            MockResponse::Text {
                content: "<summary>a valid compact model summary output here</summary>".to_string(),
            },
        ]);
        let call_log = provider.log();
        loop_.core.update_provider(
            std::sync::Arc::new(provider) as std::sync::Arc<dyn Provider>,
            "deepseek-v4-pro".to_string(),
        );

        for i in 0..10 {
            loop_.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("Message #{i}"),
                ..Default::default()
            });
            loop_.session.history.append(ChatMessage {
                role: MessageRole::Assistant,
                content: format!("Reply #{i}"),
                ..Default::default()
            });
        }

        let _ = loop_
            .compact_history_if_needed("deepseek-v4-pro", true)
            .await;

        // Only ONE LLM call: LowQuality breaks the chain instead of
        // stepping down to a weaker model.
        let log = call_log.lock().unwrap();
        assert_eq!(
            log.len(),
            1,
            "LowQuality must not step down the chain; call log: {log:?}"
        );

        // History untouched: no compaction marker, no raw replacement.
        let has_marker = loop_
            .session
            .history
            .messages()
            .iter()
            .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME));
        assert!(
            !has_marker,
            "low-quality compaction must leave history untouched"
        );
    }

    // ── ADR-083: compaction deadline + cancellation ────────────────────

    /// Provider whose `chat()` never returns — models the incident where the
    /// distill provider is unreachable/too slow. Counts calls so tests can
    /// assert the shared budget stops the tier chain.
    struct HangingProvider {
        calls: std::sync::Arc<std::sync::Mutex<usize>>,
    }

    impl HangingProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::Arc::new(std::sync::Mutex::new(0)),
            }
        }
        fn calls(&self) -> std::sync::Arc<std::sync::Mutex<usize>> {
            self.calls.clone()
        }
    }

    #[async_trait::async_trait]
    impl Provider for HangingProvider {
        fn name(&self) -> &str {
            "hanging"
        }
        async fn chat(&self, _request: ChatRequest) -> acowork_core::error::Result<ChatResponse> {
            *self.calls.lock().unwrap() += 1;
            // Never resolves — only the ADR-083 timeout/cancel can end this.
            std::future::pending::<()>().await;
            unreachable!()
        }
        async fn chat_stream(
            &self,
            _request: ChatRequest,
        ) -> acowork_core::error::Result<Box<dyn futures_core::Stream<Item = StreamEvent> + Send>>
        {
            unimplemented!()
        }
        async fn chat_token_count(
            &self,
            _messages: &[ChatMessage],
        ) -> acowork_core::error::Result<u64> {
            Ok(0)
        }
    }

    /// Seed a loop whose every distill tier resolves to `HangingProvider`,
    /// with a tiny ADR-083 deadline, and some history to compact.
    fn hanging_loop(deadline_ms: u64) -> (AgentLoop, std::sync::Arc<std::sync::Mutex<usize>>) {
        let mut loop_ = build_loop();
        seed_providers(&loop_);
        set_session(&mut loop_, "deepseek", "deepseek-v4-pro");
        set_provider_compact(&mut loop_, "deepseek", Some("deepseek-v4-flash"));
        // Aim every tier at the SESSION provider (`deepseek`) so all of them
        // resolve to the injected mock via `distill_provider`'s same-provider
        // fast path. (Pointing Tier 1 at `ollama-local` would make
        // `build_provider_for` construct a real Ollama client and hit the
        // network — the very thing this test needs to avoid.)
        *loop_.core.default_compact_models.write().unwrap() = vec![("deepseek".to_string(), "deepseek-v4-flash".to_string())];

        let hanging = HangingProvider::new();
        let calls = hanging.calls();
        loop_.core.update_provider(
            std::sync::Arc::new(hanging) as std::sync::Arc<dyn Provider>,
            "deepseek-v4-pro".to_string(),
        );
        // Bypass `validate()` (which floors ms fields at 1000) — the test
        // wants a sub-second deadline to keep the suite fast.
        loop_.core.config.timeouts.compaction_deadline_ms = deadline_ms;

        for i in 0..10 {
            loop_.session.history.append(ChatMessage {
                role: MessageRole::User,
                content: format!("Message #{i}"),
                ..Default::default()
            });
        }
        (loop_, calls)
    }

    #[tokio::test]
    async fn compaction_times_out_on_deadline_and_does_not_try_more_tiers() {
        let (mut loop_, calls) = hanging_loop(300);
        // Guard the premise: the chain must actually offer more than one
        // tier, otherwise "stopped the chain" would be vacuous.
        assert!(
            loop_.resolve_distill_targets().len() >= 2,
            "test needs a multi-tier distill chain"
        );
        let start = std::time::Instant::now();
        let result = loop_.compact_history_if_needed("deepseek-v4-pro", true).await;
        let elapsed = start.elapsed();

        // Bounded: the deadline (300ms) + slack, NOT 10 minutes.
        assert!(
            elapsed < std::time::Duration::from_secs(3),
            "compaction must be bounded by the deadline, took {elapsed:?}"
        );
        // Typed as a cancellation with the Timeout reason.
        match result {
            Err(crate::error::RuntimeError::CompactionCancelled(msg)) => assert!(
                msg.contains("Timeout"),
                "expected Timeout reason, got {msg:?}"
            ),
            other => panic!("expected CompactionCancelled(Timeout), got {other:?}"),
        }
        // Shared budget: the first tier consumed it, so no further tier ran.
        assert_eq!(*calls.lock().unwrap(), 1, "deadline must stop the tier chain");
        // History untouched — no compaction marker.
        assert!(
            !loop_
                .session
                .history
                .messages()
                .iter()
                .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME)),
            "timed-out compaction must leave history untouched"
        );
    }

    #[tokio::test]
    async fn compaction_is_cancelled_by_user_signal() {
        let (mut loop_, _calls) = hanging_loop(60_000);
        let slot = loop_.session_core.compaction_cancel_handle_arc();
        // Fire the cancel repeatedly: the compaction re-arms the slot with a
        // fresh handle at start, so a cancel that lands before the arm is
        // discarded (by design) and the next iteration hits the live handle.
        let canceller = tokio::spawn(async move {
            for _ in 0..200 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                slot.lock().cancel(crate::cancellation::CancellationReason::UserStop {
                    source: crate::cancellation::StopSource::Test,
                    reason: "test cancel compaction".to_string(),
                });
            }
        });

        let start = std::time::Instant::now();
        let result = loop_.compact_history_if_needed("deepseek-v4-pro", true).await;
        let elapsed = start.elapsed();
        canceller.abort();

        // Interrupted well before the (60s) deadline — the cancel path, not
        // the timeout path.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "cancel must interrupt promptly, took {elapsed:?}"
        );
        match result {
            Err(crate::error::RuntimeError::CompactionCancelled(msg)) => assert!(
                msg.contains("UserCancelled"),
                "expected UserCancelled reason, got {msg:?}"
            ),
            other => panic!("expected CompactionCancelled(UserCancelled), got {other:?}"),
        }
        assert!(
            !loop_
                .session
                .history
                .messages()
                .iter()
                .any(|m| m.name.as_deref() == Some(crate::agent::history::COMPACTION_SUMMARY_NAME)),
            "cancelled compaction must leave history untouched"
        );
    }

    // ─── Module D: load_skill exemption from equal-share result trimming ───

    fn big_text(len: usize) -> String {
        "x".repeat(len)
    }

    #[test]
    fn exempt_result_bigger_than_its_equal_share_is_left_whole() {
        // A SKILL.md that is far larger than the per-result share but still
        // fits the free space must NOT be amputated — the share is set by how
        // many unrelated tools ran alongside it.
        let skill = big_text(6_000);
        let sibling = "short output".to_string();
        let mut results = vec![skill.clone(), sibling.clone()];
        let tokens = [1_000u64, 100];
        let n = apply_result_budget(&mut results, &tokens, 2_000, 256, &[true, false]);
        assert_eq!(n, 0, "nothing to cut");
        assert_eq!(results[0], skill, "the skill body must survive verbatim");
        assert_eq!(results[1], sibling);
    }

    #[test]
    fn non_exempt_sibling_still_gets_the_equal_share_cut() {
        let skill = big_text(6_000);
        let noisy = big_text(6_000);
        let mut results = vec![skill.clone(), noisy];
        let tokens = [1_000u64, 1_000];
        // Same sizes, but now neither is exempt.
        let n = apply_result_budget(&mut results, &tokens, 2_000, 256, &[false, false]);
        assert_eq!(n, 2, "both exceed the 256-token share");
        assert!(results[0].contains("RESULT TRUNCATED"));
        assert!(results[1].contains("RESULT TRUNCATED"));
    }

    #[test]
    fn exempt_result_larger_than_all_free_space_is_still_cut_and_says_so() {
        // Exemption is not immunity: a body that cannot fit even if every
        // other result were dropped must be cut, or the request cannot be sent
        // at all — and a session that cannot send cannot compact or re-load.
        let skill = big_text(60_000);
        let mut results = vec![skill];
        let n = apply_result_budget(&mut results, &[20_000], 1_000, 256, &[true]);
        assert_eq!(n, 1);
        assert!(
            results[0].contains("SKILL BODY INCOMPLETE"),
            "the generic truncation marker tells the model to re-run with \
             narrower parameters, which load_skill does not support"
        );
        assert!(
            !results[0].contains("SUGGESTION: re-run with narrower parameters"),
            "the skill marker must not carry the pagination advice"
        );
        assert!(
            results[0].contains("re-run `load_skill`"),
            "it must instead name the only real recovery: load it again later"
        );
    }

    #[test]
    fn short_exempt_and_short_normal_results_are_untouched() {
        let mut results = vec!["a".to_string(), "b".to_string()];
        let before = results.clone();
        let n = apply_result_budget(&mut results, &[10, 10], 2_000, 256, &[true, false]);
        assert_eq!(n, 0);
        assert_eq!(results, before);
    }

    #[test]
    fn missing_exempt_flags_default_to_not_exempt() {
        // A caller that forgets to pass flags for every result must not
        // silently gain blanket immunity — fail toward the old behaviour.
        let mut results = vec![big_text(6_000)];
        let n = apply_result_budget(&mut results, &[1_000], 2_000, 256, &[]);
        assert_eq!(n, 1, "absent flag must not mean exempt");
        assert!(results[0].contains("RESULT TRUNCATED"));
    }

    /// Regression: what the model is actually asked to accept must never
    /// exceed `remaining`. Token estimates are passed in, so the assertion is
    /// exact rather than a proxy.
    #[test]
    fn exempt_result_cannot_exceed_what_is_left_after_the_others_share() {
        // Exemption used to cap the skill at `remaining`, so the result kept its
        // whole body *plus* the siblings' shares — the request went out over the
        // very budget this function exists to enforce.
        let skill = big_text(6_000);
        let sibling = big_text(6_000);
        let mut results = vec![skill, sibling];
        let tokens = [900u64, 5_000];
        let remaining = 1_000;
        let per_result = 500; // remaining / 2
        let n = apply_result_budget(&mut results, &tokens, remaining, per_result, &[true, false]);

        assert_eq!(n, 2, "both results exceed what they are entitled to");
        // The sibling takes its equal share; the skill may only use the 500 left.
        assert!(results[1].contains("RESULT TRUNCATED"));
        assert!(
            results[0].contains("SKILL BODY INCOMPLETE"),
            "a skill that no longer fits the unclaimed space must be cut, not silently passed"
        );
        assert!(
            !results[0].contains("RESULT TRUNCATED"),
            "the skill keeps its own marker — the pagination advice is meaningless here"
        );
        // Each result is cut to 500 tokens, so together they claim exactly
        // `remaining` — the sum can no longer exceed the free space.
        assert!(
            results[0].contains("~500 tokens"),
            "the skill's budget is the space the sibling did not claim"
        );
        assert!(
            results[1].contains("500 tokens"),
            "and the sibling keeps its equal share"
        );
    }

    #[test]
    fn exempt_skill_is_whole_when_the_siblings_leave_it_enough_room() {
        // The behaviour the exemption exists for: a 400-token skill is not
        // amputated just because a 200-token sibling happened to run alongside
        // it. Pool = 1000 - min(200, 500) = 800 >= 400 → verbatim.
        let skill = big_text(1_600);
        let sibling = "short output".to_string();
        let mut results = vec![skill.clone(), sibling.clone()];
        let n = apply_result_budget(&mut results, &[400, 200], 1_000, 500, &[true, false]);
        assert_eq!(n, 0);
        assert_eq!(
            results[0], skill,
            "the skill must survive verbatim once the siblings' shares are accounted for"
        );
        assert_eq!(results[1], sibling);
    }

    #[test]
    fn parallel_exempt_results_share_one_pool() {
        // Two skills loaded in the same turn (legal: `load_skill` is called per
        // name). Each is individually under `remaining`, but together they are
        // not — the second must be cut instead of both claiming the full space.
        let first = big_text(2_400);
        let second = big_text(2_400);
        let mut results = vec![first.clone(), second];
        let n = apply_result_budget(&mut results, &[600, 600], 1_000, 500, &[true, true]);
        assert_eq!(n, 1, "only the result the pool ran out on is cut");
        assert_eq!(results[0], first, "the pool is granted in order, newest first");
        assert!(results[1].contains("SKILL BODY INCOMPLETE"));
    }

    #[test]
    fn exemption_pool_does_not_depend_on_result_order() {
        // The pool is derived from the token counts up front, so an exempt
        // result's fate must not change when its siblings move around it — an
        // order-dependent policy would make the trim depend on which tool the
        // provider happened to report first.
        let skill = big_text(6_000);
        let noisy = big_text(6_000);
        let mut skill_first = vec![skill.clone(), noisy.clone()];
        let mut skill_last = vec![noisy, skill];
        let a = apply_result_budget(&mut skill_first, &[900, 5_000], 1_000, 500, &[true, false]);
        let b = apply_result_budget(&mut skill_last, &[5_000, 900], 1_000, 500, &[false, true]);

        assert_eq!(a, 2, "both results are over their entitlement either way");
        assert_eq!(b, 2);
        assert_eq!(
            skill_first[0].len(),
            skill_last[1].len(),
            "the skill keeps the same number of bytes in either slot"
        );
        assert_eq!(
            skill_first[1].len(),
            skill_last[0].len(),
            "and so does the sibling"
        );
    }
}
