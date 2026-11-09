//! Model fallback — the second line of defence when a run's model cannot be
//! used (plan §D7).
//!
//! # The fact this module exists to handle
//!
//! Nothing in the provider inventory says how much balance an account has:
//! [`acowork_core::protocol::ProviderListItem`] carries models, capabilities and
//! limits, and the only quota signal the platform ever gets is a provider
//! *rejecting* a call (`402` / `insufficient_quota` / MiniMax `1113`). So a
//! "pre-flight budget check" cannot be honest — there is nothing to check
//! against. What we can do is turn the one fact we do get into a durable one:
//! the moment a `(provider, model, account)` triple answers "no balance", the
//! Runtime remembers it, and the next run (and the next turn of this run,
//! after degrading) never starts on it again.
//!
//! Two pieces, both pure and separately testable:
//!
//! - [`ModelHealthRegistry`] — the negative cache. Bounded TTL, because a
//!   top-up or a rotated key is out of band and we must not blacklist a
//!   provider for the life of the process.
//! - [`next_usable`] — the ladder filter. The caller builds the ordered
//!   candidates (it is the only one that knows the inventory), this module
//!   decides which of them may actually be used right now.
//!
//! # The ladder (user-approved order, plan §7 O5)
//!
//! 1. [`DegradationStep::SameProviderOtherAccount`] — same model, different key.
//!    The cheapest, least surprising recovery: the task premise (model
//!    behaviour, context window, capabilities) is untouched.
//! 2. [`DegradationStep::AgentDefaultModel`] — the model this agent is
//!    configured to use, on the same provider.
//! 3. [`DegradationStep::AgentDefaultProvider`] — the same default model on
//!    whichever provider serves it.
//!
//! Crossing providers is allowed (§7 O5), and the switch is scoped to the
//! session that hit the wall *plus* a visible notice row: a silent global
//! model change would rewrite future work nobody asked to change.
//!
//! When the ladder is exhausted the caller must **not** loop and must not fall
//! silent — it gives up with the original error, which the turn-failure path
//! turns into a persistent notice ([`crate::agent::session::session_task`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use acowork_core::providers::error_patterns::is_non_retryable_rate_limit;
use acowork_core::providers::is_minimax_balance_code;
use acowork_core::providers::ProviderErrorType;
use acowork_core::AcoworkError;

use crate::error::RuntimeError;

/// How long a "this triple cannot be used" verdict is trusted.
///
/// 15 minutes: long enough that a session stuck on an empty wallet does not
/// re-hit the same 402 on every turn, short enough that a user who tops up (or
/// adds a key) is not made to wait for a restart — the mutation happens outside
/// this process, so nothing can invalidate the entry for us.
pub const OUT_OF_SERVICE_TTL: Duration = Duration::from_secs(15 * 60);

/// Why a `(provider, model, account)` triple was taken out of service.
///
/// The distinction is diagnostic only (it reaches the notice row and the logs);
/// both reasons mean "do not start a call on this triple".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFailure {
    /// The account answers `402` / `insufficient_quota` / MiniMax `1113-1311`:
    /// the model is fine, the money is gone.
    BalanceExhausted,
    /// The key was rejected (`401` / `403`). Another account of the same
    /// provider may still work, which is exactly what step 1 of the ladder
    /// looks for.
    KeyRejected,
}

impl ModelFailure {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BalanceExhausted => "balance_exhausted",
            Self::KeyRejected => "key_rejected",
        }
    }

    /// Human-facing sentence for the degradation notice row.
    pub fn describe(self) -> &'static str {
        match self {
            Self::BalanceExhausted => "has no remaining balance",
            Self::KeyRejected => "rejected its API key",
        }
    }
}

/// One `(provider, model, account)` triple — the unit of both the health cache
/// and a fallback candidate.
///
/// `account` is `None` for "the provider's first account", which is how a
/// session that never picked one is built (`build_provider_for`). Two `None`s
/// therefore mean the same key, and two `Some`s only match when they are equal:
/// an explicitly-chosen account must not be conflated with the default one.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelRef {
    pub provider: Option<String>,
    pub model: String,
    pub account: Option<String>,
}

impl ModelRef {
    pub fn new(provider: Option<&str>, model: &str, account: Option<&str>) -> Self {
        Self {
            provider: provider.map(str::to_string),
            model: model.to_string(),
            account: account.map(str::to_string),
        }
    }

    /// Log/notice spelling: `provider/model` plus the account when one was
    /// chosen. Deliberately never prints a key.
    pub fn label(&self) -> String {
        match (&self.provider, &self.account) {
            (Some(p), Some(a)) => format!("{p}/{}/account {}", self.model, short(a)),
            (Some(p), None) => format!("{p}/{}", self.model),
            (None, _) => self.model.clone(),
        }
    }
}

/// First 8 chars of an id — enough to tell two accounts apart in a log line
/// without pasting a whole UUID into a transcript.
///
/// `get` rather than slicing: an account id arrives from configuration, and a
/// non-ASCII id would make `&id[..8]` panic on a non-char boundary. Nothing
/// stops such an id from existing, and this function runs on the *failure*
/// path — panicking there would turn one provider's bad key into a dead
/// session.
pub fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

struct HealthEntry {
    failure: ModelFailure,
    until: Instant,
}

/// The negative cache: which triples must not be used right now.
///
/// One per agent instance (owned by [`crate::agent::agent_core::AgentCore`]),
/// shared by every session of that agent — the fact "this account has no
/// balance" is a property of the key, not of the session that discovered it.
#[derive(Default)]
pub struct ModelHealthRegistry {
    entries: Mutex<HashMap<ModelRef, HealthEntry>>,
}

impl std::fmt::Debug for ModelHealthRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelHealthRegistry")
            .field("tracked", &self.len())
            .finish()
    }
}

impl ModelHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `target` may not be used until [`OUT_OF_SERVICE_TTL`] passes.
    ///
    /// Overwrites an existing entry: the newest reason is the true one, and a
    /// fresh failure should restart the clock rather than inherit a nearly
    /// expired verdict.
    pub fn mark_out_of_service(&self, target: &ModelRef, failure: ModelFailure) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        entries.insert(
            target.clone(),
            HealthEntry {
                failure,
                until: Instant::now() + OUT_OF_SERVICE_TTL,
            },
        );
        tracing::warn!(
            model = %target.model,
            provider = ?target.provider,
            account = ?target.account.as_deref().map(short),
            reason = failure.as_str(),
            "model taken out of service for {}s",
            OUT_OF_SERVICE_TTL.as_secs()
        );
    }

    /// Is `target` currently known-bad? Expired verdicts are dropped on read, so
    /// the cache needs no sweeper.
    pub fn is_out_of_service(&self, target: &ModelRef) -> bool {
        self.reason(target).is_some()
    }

    /// Why `target` is out of service, if it still is.
    pub fn reason(&self, target: &ModelRef) -> Option<ModelFailure> {
        let Ok(mut entries) = self.entries.lock() else {
            return None;
        };
        match entries.get(target) {
            Some(entry) if entry.until > Instant::now() => Some(entry.failure),
            Some(_) => {
                entries.remove(target);
                None
            }
            None => None,
        }
    }

    /// Forget everything. For tests and for an explicit "retry anyway" (the
    /// human pressed Retry after topping up).
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Which rung of the ladder produced a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DegradationStep {
    /// Same provider and model, a different account of it.
    SameProviderOtherAccount,
    /// The agent's configured default model, on the current provider.
    AgentDefaultModel,
    /// The agent's configured default model on another provider.
    AgentDefaultProvider,
}

impl DegradationStep {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SameProviderOtherAccount => "same_provider_other_account",
            Self::AgentDefaultModel => "agent_default_model",
            Self::AgentDefaultProvider => "agent_default_provider",
        }
    }
}

/// A fallback the caller could switch to, before the health filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackCandidate {
    pub target: ModelRef,
    pub step: DegradationStep,
}

/// First candidate that is neither the failed triple nor currently out of
/// service.
///
/// Pure: no clock, no I/O, no inventory knowledge. Candidates arrive already
/// ordered by the caller ([`DegradationStep`] order), and the failed triple is
/// skipped explicitly — a ladder that can hand back the model that just failed
/// would loop forever, which is the one outcome this whole module exists to
/// prevent.
pub fn next_usable(
    failed: &ModelRef,
    candidates: &[FallbackCandidate],
    health: &ModelHealthRegistry,
) -> Option<FallbackCandidate> {
    candidates
        .iter()
        .find(|c| &c.target != failed && !health.is_out_of_service(&c.target))
        .cloned()
}

/// Classify a run-level error as "this triple cannot serve us".
///
/// Only capacity facts qualify. A network blip, a context overflow or a tool
/// error says nothing about the account, and degrading on those would rotate a
/// session off a perfectly good model — so they return `None` and keep the
/// existing retry policy untouched.
pub fn classify(err: &RuntimeError) -> Option<ModelFailure> {
    let RuntimeError::Core(AcoworkError::Provider(p)) = err else {
        return None;
    };
    if p.error_type == ProviderErrorType::PaymentRequired
        || is_non_retryable_rate_limit(&p.message)
        || is_minimax_balance_code(&p.message)
    {
        return Some(ModelFailure::BalanceExhausted);
    }
    if p.error_type == ProviderErrorType::Unauthorized {
        return Some(ModelFailure::KeyRejected);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::providers::ProviderError;

    fn triple(provider: &str, model: &str, account: Option<&str>) -> ModelRef {
        ModelRef::new(Some(provider), model, account)
    }

    fn balance_err() -> RuntimeError {
        RuntimeError::Core(AcoworkError::Provider(ProviderError::payment_required(
            "402 {\"error\":{\"code\":\"insufficient_quota\"}}".into(),
        )))
    }

    #[test]
    fn payment_required_is_a_capacity_failure() {
        assert_eq!(classify(&balance_err()), Some(ModelFailure::BalanceExhausted));
    }

    #[test]
    fn minimax_business_codes_count_as_balance_exhaustion() {
        let err = RuntimeError::Core(AcoworkError::Provider(ProviderError::unknown(
            "{\"base_resp\":{\"status_code\":1113}}".into(),
        )));
        assert_eq!(classify(&err), Some(ModelFailure::BalanceExhausted));
    }

    #[test]
    fn rejected_key_is_a_capacity_failure() {
        let err = RuntimeError::Core(AcoworkError::Provider(ProviderError::from_status_code(
            401,
            "unauthorized".into(),
        )));
        assert_eq!(classify(&err), Some(ModelFailure::KeyRejected));
    }

    #[test]
    fn network_errors_are_not_capacity_failures() {
        // The retry tiers own these; degrading on them would move a healthy
        // session off a working model because the Wi-Fi blinked.
        let err = RuntimeError::Core(AcoworkError::Provider(ProviderError::network(
            "connection reset".into(),
        )));
        assert_eq!(classify(&err), None);
    }

    #[test]
    fn ladder_skips_the_triple_that_just_failed() {
        let failed = triple("ark", "doubao-pro", Some("acct-1"));
        let health = ModelHealthRegistry::new();
        let candidates = vec![
            FallbackCandidate {
                target: failed.clone(),
                step: DegradationStep::SameProviderOtherAccount,
            },
            FallbackCandidate {
                target: triple("ark", "doubao-pro", Some("acct-2")),
                step: DegradationStep::SameProviderOtherAccount,
            },
        ];
        let chosen = next_usable(&failed, &candidates, &health).expect("second account");
        assert_eq!(chosen.target.account.as_deref(), Some("acct-2"));
    }

    #[test]
    fn ladder_skips_a_triple_known_to_be_out_of_service() {
        let failed = triple("ark", "doubao-pro", Some("acct-1"));
        let other = triple("ark", "doubao-pro", Some("acct-2"));
        let health = ModelHealthRegistry::new();
        health.mark_out_of_service(&other, ModelFailure::BalanceExhausted);

        let candidates = vec![FallbackCandidate {
            target: other.clone(),
            step: DegradationStep::SameProviderOtherAccount,
        }];
        assert!(next_usable(&failed, &candidates, &health).is_none());
    }

    #[test]
    fn ladder_falls_through_to_the_agent_default_on_another_provider() {
        let failed = triple("ark", "doubao-pro", None);
        let health = ModelHealthRegistry::new();
        let candidates = vec![FallbackCandidate {
            target: triple("openai", "gpt-4o", None),
            step: DegradationStep::AgentDefaultProvider,
        }];
        let chosen = next_usable(&failed, &candidates, &health).expect("cross-provider rung");
        assert_eq!(chosen.step, DegradationStep::AgentDefaultProvider);
    }

    #[test]
    fn an_expired_verdict_stops_blocking_the_triple() {
        let t = triple("ark", "doubao-pro", None);
        let health = ModelHealthRegistry::new();
        health.mark_out_of_service(&t, ModelFailure::BalanceExhausted);
        assert!(health.is_out_of_service(&t));

        // Simulate the TTL passing without sleeping: rewrite the deadline.
        {
            let mut entries = health.entries.lock().unwrap();
            let entry = entries.get_mut(&t).unwrap();
            entry.until = Instant::now() - Duration::from_secs(1);
        }
        assert!(!health.is_out_of_service(&t));
        assert_eq!(health.len(), 0, "expired verdict is dropped on read");
    }

    #[test]
    fn explicit_account_and_default_account_are_different_triples() {
        // `None` means "the provider's first account"; conflating it with a
        // chosen account id would let a degraded run keep hitting the empty one.
        assert_ne!(
            triple("ark", "doubao-pro", None),
            triple("ark", "doubao-pro", Some("acct-1"))
        );
    }
}
