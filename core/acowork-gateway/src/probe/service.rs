//! ADR-103 D10/D12 — probe orchestration: the Gateway side that owns the
//! pieces the pure modules deliberately do not.
//!
//! Responsibilities, in the order a save runs them:
//!
//! 1. **Snapshot** the provider row and its vault key while holding the
//!    Gateway lock, then release it — probing is network I/O and MUST NOT
//!    run under the state write lock (it would stall every other handler for
//!    up to 20 s).
//! 2. **Plan** the rows to probe ([`super::store::plan_probes`]) and run them
//!    under a total budget, honouring the 24 h cache unless forced.
//! 3. **Write back** the verdicts (plus D12.4 staleness) and re-publish, so the
//!    persisted `provider_list.json` and the MQTT retained document agree.
//!
//! `D12.5` ordering is structural, not defensive: the save has already been
//! persisted before this module runs, and every failure here is recorded as a
//! failed verdict instead of an error, so a probe can never block a save.
//!
//! ## No platform gate
//!
//! Probing is not gated by any global switch. The rows that get probed are
//! exactly the rows the user explicitly selected as `Responses` (D12.2), and
//! each probe is a single real request the user was told about before the
//! save. A global kill switch would only make a selection silently fail to
//! take effect — the exact failure mode ADR-103 §4.4 (revised) removes.

use std::time::{Duration, Instant};

use acowork_core::protocol::{ProtocolType, ProviderListItem};
use chrono::Utc;

use crate::http::routes::{ApiError, AppState};
use crate::probe::cache::ProbeCache;
use crate::probe::runner::{ProbeFailure, ProbeOutcome, ProbeRunner, ProbeTier, log_probe_audit};
use crate::probe::store::{apply_probe_outcome, mark_probes_stale, plan_probes};

/// Total wall-clock budget for one save's probe batch (D10.1).
///
/// A provider may have many changed rows; the save response must not hang on
/// them. Rows that do not get their turn are reported as `ProbeBudgetExceeded`
/// and stay unprobed (→ the D1 gate keeps them on the default protocol).
pub const PROBE_BATCH_BUDGET: Duration = Duration::from_secs(20);

/// Upper bound on probes one save may trigger (D10.4 throttling).
///
/// A provider with 40 changed `responses` rows would otherwise issue 40 real
/// billed calls while the user waits. Rows past the cap stay unprobed, which
/// keeps the D1 gate off for them — the safe direction — and the UI's
/// "re-validate" action (S6) is the intended way to probe them on demand.
pub const MAX_PROBES_PER_SAVE: usize = 10;

/// Agent id used for probe tokens, so probe traffic is accounted separately
/// from user-agent traffic in the provider's fairness ledger.
const PROBE_RATE_AGENT: &str = "provider_probe";

/// Per-model probe result returned with the save response (D12.1).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeReport {
    pub model: String,
    /// Protocol label the verdict is about (`"responses"` in v1).
    pub protocol: String,
    pub tier: ProbeTier,
    pub outcome: ProbeOutcome,
    /// `true` when the answer came out of the 24 h cache rather than a live
    /// call — surfaced so the UI can say "cached verdict".
    pub cached: bool,
}

/// Everything probing needs from Gateway state, read under one lock scope.
struct ProbeContext {
    provider: ProviderListItem,
    api_key: Option<String>,
}

/// Read the provider row and its key.
async fn probe_context(state: &AppState, provider_id: &str) -> Result<ProbeContext, ApiError> {
    let gw = state.gateway_state.read().await;

    let provider = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider_id)
        .cloned()
        .ok_or_else(|| ApiError::not_found(&format!("Provider '{}' not found", provider_id)))?;

    // A key-less provider (local model server, key-less custom endpoint) is
    // legitimately probeable — the header is simply omitted.
    let api_key = gw.vault.get_key(provider_id).ok();

    Ok(ProbeContext { provider, api_key })
}

/// Charge one token from the provider's rate-limit bucket (D10.4).
///
/// A provider without a configured bucket always grants, matching
/// [`crate::rate::bucket::RateLimiter::try_acquire_for`] — probing must not
/// invent a limit the operator never set.
async fn checkout_rate_token(state: &AppState, provider_id: &str) -> crate::rate::RateResult {
    let mut gw = state.gateway_state.write().await;
    match gw.rate_limiter_mut() {
        Some(limiter) => limiter.try_acquire_for(provider_id, PROBE_RATE_AGENT),
        None => crate::rate::RateResult {
            granted: true,
            retry_after_ms: None,
            retryable: true,
        },
    }
}

/// Does this save invalidate the provider's existing probe verdicts (D12.6)?
///
/// ADR-103 D12.6 lists three triggers: `base_url`, the API key, and the model
/// list. All three make a stored verdict a statement about a configuration that
/// no longer holds — including the model list, because the operator just told us
/// this provider serves a different set of models.
///
/// Model *order* is not a trigger: the frontend reorders rows freely and a
/// reorder changes nothing observable.
pub fn probe_inputs_changed(
    previous: &ProviderListItem,
    current: &ProviderListItem,
    key_changed: bool,
) -> bool {
    if key_changed || previous.base_url != current.base_url {
        return true;
    }
    let mut prev_ids: Vec<&str> = previous.models.iter().map(|m| m.id.as_str()).collect();
    let mut cur_ids: Vec<&str> = current.models.iter().map(|m| m.id.as_str()).collect();
    prev_ids.sort_unstable();
    cur_ids.sort_unstable();
    prev_ids != cur_ids
}

/// D12.1/D12.2 — probe the models a save changed, then persist the verdicts.
///
/// Returns one report per probed model. Never returns an error: a probe
/// failure is data, not a failed save (D12.5).
pub async fn run_save_time_probes(
    state: &AppState,
    provider_id: &str,
    previous: Option<ProviderListItem>,
    probe_inputs_changed: bool,
) -> Vec<ProbeReport> {
    let ctx = match probe_context(state, provider_id).await {
        Ok(ctx) => ctx,
        Err(e) => {
            tracing::warn!(
                provider_id = %provider_id,
                error = ?e,
                "save-time probe skipped: provider context unavailable"
            );
            return Vec::new();
        }
    };

    // A verdict cached against the *old* base URL / key / model list must not be
    // replayed for the new one, so drop the provider's entries up front.
    if probe_inputs_changed {
        let dropped = state.probe_cache.invalidate_provider(provider_id);
        tracing::info!(
            provider_id = %provider_id,
            cache_entries_dropped = dropped,
            "probe inputs changed: cached verdicts invalidated (D12.6)"
        );
    }

    let mut plan = plan_probes(&ctx.provider, previous.as_ref(), probe_inputs_changed);
    if plan.len() > MAX_PROBES_PER_SAVE {
        tracing::warn!(
            provider_id = %provider_id,
            planned = plan.len(),
            cap = MAX_PROBES_PER_SAVE,
            "save-time probe capped; remaining rows stay unprobed (gate keeps them on the default protocol)"
        );
        plan.truncate(MAX_PROBES_PER_SAVE);
    }
    let mut reports: Vec<ProbeReport> = Vec::with_capacity(plan.len());
    let mut verdicts: Vec<(String, ProbeOutcome)> = Vec::with_capacity(plan.len());
    let deadline = Instant::now() + PROBE_BATCH_BUDGET;

    let runner = ProbeRunner::new();
    for item in &plan {
        let (outcome, cached) = probe_planned_item(
            state,
            provider_id,
            ctx.api_key.as_deref(),
            &runner,
            item,
            deadline,
        )
        .await;

        log_probe_audit(provider_id, &item.model, &outcome);
        verdicts.push((item.model.clone(), outcome.clone()));
        reports.push(ProbeReport {
            model: item.model.clone(),
            protocol: "responses".to_string(),
            tier: item.tier,
            outcome,
            cached,
        });
    }

    // D12.3: L3 is a `(provider, protocol)` property, so the representative's
    // round-trip verdict applies to the whole batch. Without this, every other
    // changed row would be persisted as `degraded` merely because we did not
    // re-run a check that cannot differ per model.
    inherit_round_trip_verdict(&plan, &mut verdicts, &mut reports);

    write_back(state, provider_id, &verdicts, probe_inputs_changed).await;
    reports
}

/// D10.1 — probe one model on explicit request (the `POST …/probe` endpoint).
///
/// Works with the platform gate off (see the module docs). `force` bypasses the
/// 24 h cache.
pub async fn probe_one(
    state: &AppState,
    provider_id: &str,
    model: &str,
    protocol: &str,
    tier: ProbeTier,
    force: bool,
) -> Result<ProbeOutcome, ApiError> {
    if protocol != "responses" {
        return Err(ApiError::bad_request(&format!(
            "protocol '{}' has no probe implementation (supported: responses)",
            protocol
        )));
    }

    let ctx = probe_context(state, provider_id).await?;
    let entry = ctx
        .provider
        .models
        .iter()
        .find(|m| m.id == model)
        .ok_or_else(|| {
            ApiError::not_found(&format!(
                "Model '{}' not configured on provider '{}'",
                model, provider_id
            ))
        })?;

    // D7 order: model override > provider default. No built-in per-provider
    // default is applied here — the probe must hit the URL the Runtime would
    // hit, and an empty base URL is a configuration error worth reporting.
    let base_url = entry
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let provider_base = ctx.provider.base_url.trim();
            (!provider_base.is_empty()).then(|| provider_base.to_string())
        })
        .ok_or_else(|| {
            ApiError::bad_request(
                "provider has no base_url to probe; set one before probing Responses",
            )
        })?;

    let key = ProbeCache::key(provider_id, model, "responses");
    let outcome = match state.probe_cache.get(&key, force) {
        Some(cached) => cached,
        None => {
            // D10.4: an explicit probe is a real billed call — it is charged
            // against the provider's bucket like any other traffic.
            let rate = checkout_rate_token(state, provider_id).await;
            if !rate.granted {
                return Err(ApiError::too_many_requests(&format!(
                    "provider rate limit reached; retry after {} ms",
                    rate.retry_after_ms.unwrap_or(1_000)
                )));
            }
            let runner = ProbeRunner::new();
            let outcome = runner
                .run(model, &base_url, ctx.api_key.as_deref(), tier)
                .await;
            state.probe_cache.insert(&key, outcome.clone());
            outcome
        }
    };

    log_probe_audit(provider_id, model, &outcome);
    write_back(state, provider_id, &[(model.to_string(), outcome.clone())], false).await;
    Ok(outcome)
}

/// Run one planned probe, in the order that avoids spending anything it can
/// skip: cache → rate token → URL sanity → batch budget → HTTP.
///
/// Returns `(outcome, cache_hit)`.
async fn probe_planned_item(
    state: &AppState,
    provider_id: &str,
    api_key: Option<&str>,
    runner: &ProbeRunner,
    item: &crate::probe::store::ProbePlanItem,
    deadline: Instant,
) -> (ProbeOutcome, bool) {
    let key = ProbeCache::key(provider_id, &item.model, "responses");
    if let Some(cached) = state.probe_cache.get(&key, false) {
        return (cached, true);
    }

    // D10.4: probe calls are real billed requests and are charged against the
    // provider's bucket. Throttling is reported as a failed verdict, never as a
    // save failure (D12.5).
    let rate = checkout_rate_token(state, provider_id).await;
    if !rate.granted {
        return (
            failure_outcome(
                "RateLimited",
                &format!(
                    "provider rate limit reached; retry after {} ms",
                    rate.retry_after_ms.unwrap_or(1_000)
                ),
            ),
            false,
        );
    }

    // No target URL: probing `/responses` on an empty host would report a
    // transport error that says nothing about the provider.
    let Some(base_url) = item.base_url.as_deref() else {
        return (
            failure_outcome("NoBaseUrl", "provider has no base_url to probe"),
            false,
        );
    };

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return (
            failure_outcome(
                "ProbeBudgetExceeded",
                "batch budget exhausted before this model",
            ),
            false,
        );
    }

    match tokio::time::timeout(remaining, runner.run(&item.model, base_url, api_key, item.tier)).await
    {
        Ok(outcome) => {
            state.probe_cache.insert(&key, outcome.clone());
            (outcome, false)
        }
        Err(_) => (
            failure_outcome(
                "ProbeBudgetExceeded",
                "model probe exceeded the batch budget",
            ),
            false,
        ),
    }
}

/// Spread the deep row's L3 verdict over the rest of the batch (D12.3).
///
/// Applied to both `verdicts` (what gets persisted) and `reports` (what the save
/// response shows), so the UI and the on-disk verdict never disagree.
fn inherit_round_trip_verdict(
    plan: &[crate::probe::store::ProbePlanItem],
    verdicts: &mut [(String, ProbeOutcome)],
    reports: &mut [ProbeReport],
) {
    let Some(deep_idx) = plan.iter().position(|i| i.tier == ProbeTier::Deep) else {
        return;
    };
    let Some(deep) = verdicts.get(deep_idx).map(|(_, o)| o.clone()) else {
        return;
    };
    // A representative that never reached L1/L2 tells us nothing about L3 for
    // its siblings — leave them as they are.
    let passed_l1_l2 = |o: &ProbeOutcome| {
        o.tier_passed.iter().any(|t| t == "L1") && o.tier_passed.iter().any(|t| t == "L2")
    };
    if !passed_l1_l2(&deep) {
        return;
    }
    let l3_ok = deep.tier_passed.iter().any(|t| t == "L3");

    for (idx, (_, outcome)) in verdicts.iter_mut().enumerate() {
        if idx == deep_idx || outcome.tier_passed.iter().any(|t| t == "L3") {
            continue;
        }
        if !passed_l1_l2(outcome) {
            continue;
        }
        if l3_ok {
            outcome.tier_passed.push("L3".to_string());
            outcome.success = true;
            outcome.error = None;
        } else {
            // Same failure class as the representative: the endpoint speaks the
            // shape but cannot complete a tool round-trip.
            outcome.success = false;
            outcome.error = deep.error.clone();
        }
        if let Some(report) = reports.get_mut(idx) {
            report.outcome = outcome.clone();
        }
    }
}

/// Persist verdicts (and optional staleness), then re-publish over MQTT.
///
/// The lock is taken only for the in-memory mutation + disk write. `probe_stale`
/// applies D12.4 *before* the fresh verdicts so a stale provider row cannot
/// overwrite a verdict written in this same pass.
async fn write_back(
    state: &AppState,
    provider_id: &str,
    verdicts: &[(String, ProbeOutcome)],
    probe_stale: bool,
) {
    if verdicts.is_empty() && !probe_stale {
        return;
    }

    let mut gw = state.gateway_state.write().await;

    let Some(entry) = gw
        .resource_cache
        .provider_list
        .providers
        .iter_mut()
        .find(|p| p.id == provider_id)
    else {
        tracing::warn!(
            provider_id = %provider_id,
            "probe verdicts dropped: provider removed while probing"
        );
        return;
    };

    if probe_stale {
        let marked = mark_probes_stale(entry);
        tracing::info!(
            provider_id = %provider_id,
            stale_rows = marked,
            "provider config changed: existing probe verdicts marked stale (D12.4)"
        );
    }

    let probed_at = Utc::now().to_rfc3339();
    for (model, outcome) in verdicts {
        apply_probe_outcome(entry, model, outcome, probed_at.clone());
    }

    let data_dir = crate::http::provider_api::get_data_dir_from_gw(&gw);
    crate::resource_cache::persist_provider_cache(&mut gw, &data_dir);
    drop(gw);

    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }
}

/// Synthetic verdict for a row that could not be probed at all (no URL, no
/// budget left). Recorded as a `Failed` verdict so the D1 gate keeps the model
/// on its default protocol instead of silently allowing `Responses`.
fn failure_outcome(class: &str, detail: &str) -> ProbeOutcome {
    ProbeOutcome {
        success: false,
        tier_passed: Vec::new(),
        latency_ms: 0,
        observed_base_url: None,
        usage: None,
        error: Some(ProbeFailure {
            class: class.to_string(),
            message: detail.to_string(),
            http_status: None,
            unsupported_field: None,
        }),
        stripped_fields: Vec::new(),
    }
}

/// Is this protocol one the probe can verify at all?
pub fn is_probeable(protocol: &ProtocolType) -> bool {
    matches!(protocol, ProtocolType::Responses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::{ProbeState, ProviderModelEntry};
    use crate::probe::test_support;

    fn provider(models: Vec<ProviderModelEntry>) -> ProviderListItem {
        test_support::provider("https://api.acme.test/v1", models)
    }

    #[test]
    fn only_responses_is_probeable() {
        assert!(is_probeable(&ProtocolType::Responses));
        assert!(!is_probeable(&ProtocolType::OpenAI));
        assert!(!is_probeable(&ProtocolType::Unknown));
    }

    #[test]
    fn synthetic_failures_stay_failed_not_verified() {
        for class in ["ProbeBudgetExceeded", "NoBaseUrl"] {
            let outcome = failure_outcome(class, "detail");
            assert_eq!(outcome.state(), ProbeState::Failed, "{class}");
            assert!(!outcome.success, "{class}");
            assert_eq!(outcome.error.unwrap().class, class);
        }
    }

    fn plan_item(model: &str, tier: ProbeTier) -> crate::probe::store::ProbePlanItem {
        crate::probe::store::ProbePlanItem {
            model: model.to_string(),
            tier,
            base_url: Some("https://api.acme.test/v1".to_string()),
        }
    }

    fn outcome(tiers: &[&str], error: Option<ProbeFailure>) -> ProbeOutcome {
        ProbeOutcome {
            success: error.is_none() && tiers.contains(&"L3"),
            tier_passed: tiers.iter().map(|t| t.to_string()).collect(),
            latency_ms: 5,
            observed_base_url: None,
            usage: None,
            error,
            stripped_fields: Vec::new(),
        }
    }

    fn report(model: &str, outcome: ProbeOutcome) -> ProbeReport {
        ProbeReport {
            model: model.to_string(),
            protocol: "responses".to_string(),
            tier: ProbeTier::Standard,
            outcome,
            cached: false,
        }
    }

    #[test]
    fn round_trip_verdict_is_inherited_by_siblings() {
        // D12.3: one representative runs L3; the siblings' L1/L2-only verdicts
        // must not be persisted as "degraded" just because L3 was deduplicated.
        let plan = vec![
            plan_item("a", ProbeTier::Deep),
            plan_item("b", ProbeTier::Standard),
            plan_item("c", ProbeTier::Standard),
        ];
        let mut verdicts = vec![
            ("a".to_string(), outcome(&["L1", "L2", "L3"], None)),
            ("b".to_string(), outcome(&["L1", "L2"], None)),
            ("c".to_string(), outcome(&["L1", "L2"], None)),
        ];
        let mut reports = vec![
            report("a", verdicts[0].1.clone()),
            report("b", verdicts[1].1.clone()),
            report("c", verdicts[2].1.clone()),
        ];

        inherit_round_trip_verdict(&plan, &mut verdicts, &mut reports);

        for idx in 1..3 {
            assert_eq!(verdicts[idx].1.tier_passed, vec!["L1", "L2", "L3"]);
            assert!(verdicts[idx].1.success);
            assert_eq!(verdicts[idx].1.state(), ProbeState::Verified);
            assert_eq!(reports[idx].outcome, verdicts[idx].1, "report must agree");
        }
    }

    #[test]
    fn failed_round_trip_fails_siblings_with_the_same_reason() {
        let plan = vec![
            plan_item("a", ProbeTier::Deep),
            plan_item("b", ProbeTier::Standard),
        ];
        let failure = ProbeFailure {
            class: "RoundTripFailed".to_string(),
            message: "no function_call item".to_string(),
            http_status: None,
            unsupported_field: None,
        };
        let mut verdicts = vec![
            ("a".to_string(), outcome(&["L1", "L2"], Some(failure.clone()))),
            ("b".to_string(), outcome(&["L1", "L2"], None)),
        ];
        let mut reports = vec![
            report("a", verdicts[0].1.clone()),
            report("b", verdicts[1].1.clone()),
        ];

        inherit_round_trip_verdict(&plan, &mut verdicts, &mut reports);

        assert!(!verdicts[1].1.success);
        assert_eq!(verdicts[1].1.state(), ProbeState::Failed);
        assert_eq!(verdicts[1].1.error, Some(failure));
    }

    #[test]
    fn inheritance_is_skipped_when_the_representative_never_reached_l2() {
        // The representative failed at L1: it says nothing about L3, and the
        // siblings keep their own verdicts untouched.
        let plan = vec![
            plan_item("a", ProbeTier::Deep),
            plan_item("b", ProbeTier::Standard),
        ];
        let mut verdicts = vec![
            (
                "a".to_string(),
                outcome(
                    &[],
                    Some(ProbeFailure {
                        class: "EndpointUnavailable".to_string(),
                        message: "404".to_string(),
                        http_status: Some(404),
                        unsupported_field: None,
                    }),
                ),
            ),
            ("b".to_string(), outcome(&["L1", "L2"], None)),
        ];
        let mut reports = vec![
            report("a", verdicts[0].1.clone()),
            report("b", verdicts[1].1.clone()),
        ];

        inherit_round_trip_verdict(&plan, &mut verdicts, &mut reports);

        assert_eq!(verdicts[1].1.tier_passed, vec!["L1", "L2"]);
        // No L3, so no positive verdict — and no inherited failure either.
        assert_eq!(verdicts[1].1.state(), ProbeState::Failed);
        assert!(verdicts[1].1.error.is_none());
    }

    #[test]
    fn plan_runs_through_context_shape() {
        // Guards the contract the service relies on: a provider intent of
        // Responses without an explicit model choice still plans rows.
        let mut p = provider(vec![
            test_support::model("m1", None),
            test_support::model("m2", Some(ProtocolType::OpenAI)),
        ]);
        p.protocol_type = ProtocolType::Responses;
        let plan = plan_probes(&p, None, false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].model, "m1");
        assert_eq!(plan[0].base_url.as_deref(), Some("https://api.acme.test/v1"));
    }
}