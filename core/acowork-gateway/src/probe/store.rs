//! ADR-103 D12 — pure planning and application of save-time probes.
//!
//! The handler in [`crate::http::provider_api`] does three things around a
//! save: plan which rows need probing, run them, then write the verdicts back.
//! The planning and writing halves live here and are pure functions over
//! [`ProviderListItem`], so the rules that decide *what* gets probed can be
//! tested without a vault, a socket, or an HTTP request:
//!
//! - **D12.2 changed rows only** — an unmodified model row is never re-probed,
//!   so a provider holding 40 models and switching one of them to `Responses`
//!   bills exactly one probe.
//! - **D12.3 L3 deduplicated per (provider, protocol)** — the tool round-trip
//!   costs two extra calls; the first planned row pays for it, the rest run the
//!   cheap standard tier.
//! - **D12.4 staleness** — any config or key change invalidates every existing
//!   verdict of that provider, because a verdict is a statement about a
//!   `(base_url, key)` pairing that no longer holds.

use acowork_core::protocol::{ProtocolProbe, ProtocolType, ProviderListItem, ProbeState};

use crate::probe::runner::{ProbeOutcome, ProbeTier};

/// One model queued for probing by [`plan_probes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbePlanItem {
    pub model: String,
    pub tier: ProbeTier,
    /// The base URL to hit — model override when present, otherwise the
    /// provider default. `None` means "provider has no base URL configured"
    /// (the built-in default resolution happens at request time, D7).
    pub base_url: Option<String>,
}

/// Which models of `provider` must be probed on this save (D12.2/D12.3).
///
/// `previous` is the provider entry as it was stored *before* the save;
/// `None` means "newly added provider", in which case every `Responses` row is
/// changed by definition.
///
/// `probe_inputs_changed` says the *probe target itself* moved (provider
/// `base_url` or API key). Every existing verdict is then a statement about a
/// host/credential pair that no longer holds, so every `Responses` row is
/// re-probed even though its own row is byte-identical.
///
/// A row is planned when *both*:
/// - its **selected** protocol is `Responses` — the model's explicit choice,
///   or the provider-level default when the model has none. This is the
///   *intent*, not the gate-resolved effective protocol: probing reads the
///   intent, and the gate is what the resulting verdict feeds (D1).
/// - it is **changed**: new, its `protocol_type` / `base_url` differs from the
///   stored row, or `probe_inputs_changed`.
pub fn plan_probes(
    provider: &ProviderListItem,
    previous: Option<&ProviderListItem>,
    probe_inputs_changed: bool,
) -> Vec<ProbePlanItem> {
    let mut plan: Vec<ProbePlanItem> = Vec::new();

    for model in &provider.models {
        let selected = model
            .protocol_type
            .clone()
            .unwrap_or_else(|| provider.protocol_type.clone());
        if selected != ProtocolType::Responses {
            continue;
        }

        let changed = probe_inputs_changed
            || match previous.and_then(|prev| prev.models.iter().find(|m| m.id == model.id)) {
                None => true,
                Some(prev_model) => {
                    prev_model.protocol_type != model.protocol_type
                        || prev_model.base_url != model.base_url
                }
            };
        if !changed {
            continue;
        }

        plan.push(ProbePlanItem {
            model: model.id.clone(),
            tier: ProbeTier::Standard,
            base_url: probe_base_url(model.base_url.as_deref(), &provider.base_url),
        });
    }

    // D12.3: one deep probe per (provider, protocol) — every row in this plan
    // shares provider + Responses, so exactly the first row goes deep.
    if let Some(first) = plan.first_mut() {
        first.tier = ProbeTier::Deep;
    }

    plan
}

/// Resolve the URL the probe should hit: model override wins, then the
/// provider default. Empty strings count as "not set" (they mean "inherit").
fn probe_base_url(model_base_url: Option<&str>, provider_base_url: &str) -> Option<String> {
    model_base_url
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let trimmed = provider_base_url.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
}

/// Mark every existing verdict of `provider` as `Stale` (D12.4).
///
/// Returns how many rows were demoted. The verdict's tiers/reason are kept so
/// the UI can still show what the last probe found; only `state` changes, and
/// `state` is what gates `Responses` (D1), so a stale verdict stops being
/// trusted without losing its explanation.
pub fn mark_probes_stale(provider: &mut ProviderListItem) -> usize {
    let mut marked = 0;
    for model in &mut provider.models {
        if let Some(probe) = model.protocol_probe.as_mut()
            && probe.state != ProbeState::Stale
        {
            probe.state = ProbeState::Stale;
            marked += 1;
        }
    }
    marked
}

/// Write a probe result back onto the model's persisted verdict (D10.6).
///
/// Returns `false` when the provider no longer carries that model — possible
/// when a concurrent save removed it while the probe was in flight; the
/// verdict is then simply dropped rather than resurrecting the row.
pub fn apply_probe_outcome(
    provider: &mut ProviderListItem,
    model_id: &str,
    outcome: &ProbeOutcome,
    probed_at: String,
) -> bool {
    let Some(model) = provider.models.iter_mut().find(|m| m.id == model_id) else {
        tracing::warn!(
            provider_id = %provider.id,
            model_id = %model_id,
            "probe verdict dropped: model no longer present in provider list"
        );
        return false;
    };

    model.protocol_probe = Some(ProtocolProbe {
        state: outcome.state(),
        tier_passed: outcome.tier_passed.clone(),
        reason: outcome.error.as_ref().map(|e| e.message.clone()),
        observed_base_url: outcome.observed_base_url.clone(),
        probed_at: Some(probed_at),
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::runner::ProbeFailure;
    use crate::probe::test_support;
    use acowork_core::protocol::ProviderModelEntry;

    const ACME_BASE: &str = "https://api.acme.test/v1";

    fn model(id: &str, protocol: Option<ProtocolType>, base_url: Option<&str>) -> ProviderModelEntry {
        ProviderModelEntry {
            base_url: base_url.map(str::to_string),
            ..test_support::model(id, protocol)
        }
    }

    fn provider(models: Vec<ProviderModelEntry>) -> ProviderListItem {
        test_support::provider(ACME_BASE, models)
    }

    #[test]
    fn new_provider_plans_every_responses_row() {
        let p = provider(vec![
            model("m-openai", None, None),
            model("m-resp", Some(ProtocolType::Responses), None),
            model("m-resp-2", Some(ProtocolType::Responses), None),
        ]);
        let plan = plan_probes(&p, None, false);
        assert_eq!(plan.len(), 2);
        assert_eq!(plan[0].model, "m-resp");
        assert_eq!(plan[0].base_url.as_deref(), Some("https://api.acme.test/v1"));
    }

    #[test]
    fn provider_level_intent_makes_rows_probe_candidates() {
        let mut p = provider(vec![model("m1", None, None), model("m2", None, None)]);
        p.protocol_type = ProtocolType::Responses;
        let plan = plan_probes(&p, None, false);
        assert_eq!(plan.len(), 2, "provider-level Responses is inherited intent");
    }

    #[test]
    fn unchanged_rows_are_not_reprobed() {
        let previous = provider(vec![model("m1", Some(ProtocolType::Responses), None)]);
        let current = provider(vec![
            model("m1", Some(ProtocolType::Responses), None),
            model("m2", Some(ProtocolType::Responses), None),
        ]);
        let plan = plan_probes(&current, Some(&previous), false);
        assert_eq!(plan.len(), 1, "only the newly-added row is planned");
        assert_eq!(plan[0].model, "m2");
    }

    #[test]
    fn provider_level_probe_input_change_replans_every_row() {
        // Identical rows, but the provider URL/key moved: every verdict is
        // about a host that no longer holds.
        let previous = provider(vec![
            model("a", Some(ProtocolType::Responses), None),
            model("b", Some(ProtocolType::Responses), None),
        ]);
        let current = provider(vec![
            model("a", Some(ProtocolType::Responses), None),
            model("b", Some(ProtocolType::Responses), None),
        ]);
        assert!(
            plan_probes(&current, Some(&previous), false).is_empty(),
            "unchanged rows are skipped when the target is unchanged"
        );
        assert_eq!(plan_probes(&current, Some(&previous), true).len(), 2);
    }

    #[test]
    fn base_url_change_replans_the_row() {
        let previous = provider(vec![model("m1", Some(ProtocolType::Responses), None)]);
        let current = provider(vec![model(
            "m1",
            Some(ProtocolType::Responses),
            Some("https://alt.acme.test/v1"),
        )]);
        let plan = plan_probes(&current, Some(&previous), false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].base_url.as_deref(), Some("https://alt.acme.test/v1"));
    }

    #[test]
    fn only_the_first_row_runs_the_deep_tier() {
        let p = provider(vec![
            model("a", Some(ProtocolType::Responses), None),
            model("b", Some(ProtocolType::Responses), None),
            model("c", Some(ProtocolType::Responses), None),
        ]);
        let plan = plan_probes(&p, None, false);
        assert_eq!(plan.iter().filter(|i| i.tier == ProbeTier::Deep).count(), 1);
        assert_eq!(plan[0].tier, ProbeTier::Deep);
    }

    #[test]
    fn non_responses_rows_are_never_planned() {
        let p = provider(vec![
            model("a", Some(ProtocolType::Anthropic), None),
            model("b", Some(ProtocolType::OpenAI), None),
            model("c", None, None),
        ]);
        assert!(plan_probes(&p, None, false).is_empty());
    }

    #[test]
    fn empty_base_urls_do_not_become_probe_targets() {
        let mut p = provider(vec![model("a", Some(ProtocolType::Responses), Some("  "))]);
        p.base_url = String::new();
        let plan = plan_probes(&p, None, false);
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].base_url, None, "no base URL is a request-time default");
    }

    #[test]
    fn config_change_marks_every_verdict_stale() {
        let mut p = provider(vec![
            model("a", Some(ProtocolType::Responses), None),
            model("b", Some(ProtocolType::Responses), None),
            model("c", None, None),
        ]);
        for m in &mut p.models {
            m.protocol_probe = Some(ProtocolProbe {
                state: ProbeState::Verified,
                tier_passed: vec!["L1".into()],
                reason: None,
                observed_base_url: None,
                probed_at: Some("2025-01-01T00:00:00Z".into()),
            });
        }
        assert_eq!(mark_probes_stale(&mut p), 3);
        assert!(
            p.models
                .iter()
                .all(|m| m.protocol_probe.as_ref().unwrap().state == ProbeState::Stale)
        );
        // Tiers survive so the UI can still explain the last probe.
        assert_eq!(
            p.models[0].protocol_probe.as_ref().unwrap().tier_passed,
            vec!["L1"]
        );
        assert_eq!(mark_probes_stale(&mut p), 0, "already-stale rows are not recounted");
    }

    #[test]
    fn apply_outcome_writes_verified_verdict() {
        let mut p = provider(vec![model("a", Some(ProtocolType::Responses), None)]);
        let outcome = ProbeOutcome {
            success: true,
            tier_passed: vec!["L1".into(), "L2".into(), "L3".into()],
            latency_ms: 90,
            observed_base_url: None,
            usage: None,
            error: None,
        };
        assert!(apply_probe_outcome(
            &mut p,
            "a",
            &outcome,
            "2025-06-01T12:00:00Z".into()
        ));
        let probe = p.models[0].protocol_probe.as_ref().unwrap();
        assert_eq!(probe.state, ProbeState::Verified);
        assert_eq!(probe.tier_passed, vec!["L1", "L2", "L3"]);
        assert_eq!(probe.probed_at.as_deref(), Some("2025-06-01T12:00:00Z"));
        assert!(probe.reason.is_none());
    }

    #[test]
    fn failed_probe_records_reason_and_gates_off() {
        let mut p = provider(vec![model("a", Some(ProtocolType::Responses), None)]);
        let outcome = ProbeOutcome {
            success: false,
            tier_passed: vec![],
            latency_ms: 30,
            observed_base_url: None,
            usage: None,
            error: Some(ProbeFailure {
                class: "EndpointUnavailable".into(),
                message: "provider does not serve /responses".into(),
                http_status: Some(404),
            }),
        };
        assert!(apply_probe_outcome(&mut p, "a", &outcome, "t".into()));
        let probe = p.models[0].protocol_probe.as_ref().unwrap();
        assert_eq!(probe.state, ProbeState::Failed);
        assert_eq!(
            probe.reason.as_deref(),
            Some("provider does not serve /responses")
        );
        assert!(!probe.state.gates_responses(), "D1: failed must gate off");
    }

    #[test]
    fn missing_round_trip_persists_as_failed() {
        let mut p = provider(vec![model("a", Some(ProtocolType::Responses), None)]);
        let outcome = ProbeOutcome {
            success: false,
            tier_passed: vec!["L1".into(), "L2".into()],
            latency_ms: 30,
            observed_base_url: None,
            usage: None,
            error: Some(ProbeFailure {
                class: "RoundTripFailed".into(),
                message: "no function_call item".into(),
                http_status: None,
            }),
        };
        apply_probe_outcome(&mut p, "a", &outcome, "t".into());
        let probe = p.models[0].protocol_probe.as_ref().unwrap();
        assert_eq!(
            probe.state,
            ProbeState::Failed,
            "L3 is required for a positive verdict (D10.2)"
        );
        assert!(!probe.state.gates_responses());
    }

    #[test]
    fn degraded_verdict_from_a_field_trim_still_gates_on() {
        // S7 (field trim) produces this state; the store must keep persisting it
        // as gate-passing even though S5's runner cannot emit it.
        let mut p = provider(vec![model("a", Some(ProtocolType::Responses), None)]);
        let outcome = ProbeOutcome {
            success: true,
            tier_passed: vec!["L1".into(), "L2".into(), "L3".into()],
            latency_ms: 30,
            observed_base_url: None,
            usage: None,
            error: None,
        };
        apply_probe_outcome(&mut p, "a", &outcome, "t".into());
        p.models[0].protocol_probe.as_mut().unwrap().state = ProbeState::Degraded;
        let probe = p.models[0].protocol_probe.as_ref().unwrap();
        assert_eq!(probe.state, ProbeState::Degraded);
        assert!(probe.state.gates_responses(), "D10.3: degraded is still usable");
    }

    #[test]
    fn verdict_for_missing_model_is_dropped() {
        let mut p = provider(vec![model("a", Some(ProtocolType::Responses), None)]);
        let outcome = ProbeOutcome {
            success: true,
            tier_passed: vec!["L1".into()],
            latency_ms: 1,
            observed_base_url: None,
            usage: None,
            error: None,
        };
        assert!(!apply_probe_outcome(&mut p, "gone", &outcome, "t".into()));
        assert!(p.models[0].protocol_probe.is_none());
    }
}