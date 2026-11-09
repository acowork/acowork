//! Model degradation — the run-level half of plan §D7.
//!
//! [`crate::agent::model_fallback`] owns the *policy* (which triple is bad, in
//! what order to look for a replacement); this module owns the *effects*: mark
//! the verdict, rebuild the provider, persist the switch and leave a notice the
//! human can read. Extracted from `loop_.rs` so the one place that changes the
//! model mid-run is reviewable on its own.
//!
//! # Why this is not a retry
//!
//! A retry re-issues the same request on the same triple. A `402` /
//! `insufficient_quota` / rejected key cannot be fixed by waiting, so the retry
//! policy correctly answers `GiveUp` ([`crate::agent::retry`]). Before that
//! `GiveUp` ends the run, this module gets one question: *is there another
//! triple that can serve this turn?* If yes the run continues on it; if no the
//! caller's give-up path stays exactly as it was — the turn fails, the receipt
//! is written, and the transcript carries a notice
//! ([`crate::agent::session::session_task`]). Never a silent hang.

use crate::agent::context::ContextBuilder;
use crate::agent::model_fallback::{self, ModelRef};

impl crate::agent::loop_::AgentLoop {
    /// Try to move the current run onto a usable model after a capacity failure.
    ///
    /// Returns `true` when the run may continue — the provider instance, the
    /// session state, the persisted config and the context builder's model
    /// override have all been switched, and a `system` notice row records what
    /// happened. `false` means the ladder is exhausted and the caller must fall
    /// through to its normal give-up path.
    ///
    /// Only capacity failures qualify ([`model_fallback::classify`]): a network
    /// blip or a tool error says nothing about the account, and rotating a
    /// healthy session off a working model on those would be worse than the
    /// failure itself.
    pub(crate) fn degrade_after_capacity_failure(
        &mut self,
        context_builder: &mut ContextBuilder,
        error: &crate::error::RuntimeError,
    ) -> bool {
        let Some(failure) = model_fallback::classify(error) else {
            return false;
        };

        let failed = ModelRef {
            provider: self.session.provider.clone(),
            model: self.resolve_current_model(Some(context_builder)),
            account: self.session.account_id.clone(),
        };
        // Record the verdict *before* choosing: the triples considered below
        // are filtered by the registry, and the one that just failed must be
        // excluded even if a rung would otherwise offer it again.
        self.core
            .model_health
            .mark_out_of_service(&failed, failure);

        let candidates = self.core.fallback_candidates(&failed);
        let Some(chosen) =
            model_fallback::next_usable(&failed, &candidates, &self.core.model_health)
        else {
            tracing::warn!(
                failed = %failed.label(),
                considered = candidates.len(),
                reason = failure.as_str(),
                "no usable model on the fallback ladder — the run gives up"
            );
            return false;
        };

        let target = chosen.target.clone();
        let Some(provider_id) = target.provider.clone() else {
            // A triple with no provider cannot be built; treat as exhaustion.
            return false;
        };

        let Some(new_provider) = self.session_core.build_provider_for(
            &provider_id,
            Some(target.model.as_str()),
            target.account.as_deref(),
            &crate::agent::session_core::ProviderBuildContext {
                config: &self.core.config,
                global_provider_list: &self.core.global_provider_list,
                provider_key_vault: &self.core.provider_key_vault,
                compat_cache: self.core.compat_cache.as_ref(),
                responses_compat_cache: self.core.responses_compat_cache.as_ref(),
                anthropic_compat_cache: self.core.anthropic_compat_cache.as_ref(),
            },
        ) else {
            // The provider metadata is gone (or has no usable key). Nothing was
            // changed, so the caller's give-up path reports the original error.
            tracing::warn!(
                provider = %provider_id,
                "fallback provider could not be built — the run gives up"
            );
            return false;
        };

        // ── Live state ───────────────────────────────────────────────────
        self.update_provider(new_provider, target.model.clone(), Some(provider_id.clone()));
        self.session.set_model(target.model.clone());
        self.session.set_provider(provider_id.clone());
        if let Some(account) = target.account.clone() {
            self.session.set_account_id(account);
        }
        context_builder.set_override_model(target.model.clone());

        // ── Persisted state ──────────────────────────────────────────────
        //
        // The switch is written to the session, not to the agent: a degradation
        // is evidence about *this* task's account and model, and rewriting the
        // agent's default from a failure path would silently change every
        // future session. Persisting it also keeps the panel honest — the model
        // menu must show the model that is actually running.
        if let Some(conv) = self.session.conversation() {
            conv.update_model_provider(&target.model, Some(provider_id.as_str()));
            // Account only when the candidate names one. A triple with
            // `account = None` means "the provider's default key", and there is
            // no way to express "no account" on the row (`update_account_id`
            // takes a `&str`) — the same limitation `llm_effects` already has
            // when a config change drops the pick. Left as-is the row keeps the
            // old id; `build_provider_for` treats an id it cannot resolve as
            // "account gone" and falls back to the provider's first key, so the
            // run lands where this candidate intended either way. Written down
            // rather than silently inherited.
            if let Some(account) = target.account.as_deref() {
                conv.update_account_id(account);
            }
            let notice = format!(
                "Model switched for this session: {} {} — running on {} instead ({}).",
                failed.label(),
                failure.describe(),
                target.label(),
                chosen.step.as_str()
            );
            conv.append_message(
                "system",
                &notice,
                Some(serde_json::json!({
                    "notice": "model_degraded",
                    "from": failed.label(),
                    "to": target.label(),
                    "reason": failure.as_str(),
                    "step": chosen.step.as_str(),
                })),
            );
        }

        tracing::warn!(
            from = %failed.label(),
            to = %target.label(),
            step = chosen.step.as_str(),
            reason = failure.as_str(),
            "degraded to a fallback model; continuing the run"
        );
        true
    }
}
