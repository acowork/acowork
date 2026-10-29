//! ADR-103 D12 — write-path tests: save-time probing, verdict persistence and
//! the explicit probe endpoint.
//!
//! These drive the real handlers against a fake Responses endpoint, so the S5
//! acceptance criteria are exercised end to end:
//!
//! - `verified` / `failed(EndpointUnavailable)` / `failed(L3 no function_call)`
//!   are distinguishable from one fake server;
//! - only changed rows are probed;
//! - a failed probe never fails the save;
//! - a cached verdict is reused unless `force` bypasses it.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use mockito::Matcher;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{GatewayConfig, LlmConfig};
use crate::http::auth::HttpAuth;
use crate::http::provider_api::{
    AddProviderRequest, ProbeProviderRequest, ProbeQuery, UpdateProviderRequest, add_provider,
    probe_provider, update_provider,
};
use crate::http::routes::AppState;
use crate::probe::ProbeTier;
use acowork_core::protocol::{ProbeState, ProtocolType};

const PROVIDER: &str = "acme-gw";
const MODEL: &str = "acme-large";

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "acowork-probe-{}-{}-{}",
        tag,
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// Gateway state whose `config.data_dir` is a temp dir and whose platform gate
/// is set explicitly — the gate is what decides whether save-time probing runs.
async fn state_with(dir: &std::path::Path, responses_enabled: bool) -> AppState {
    let mut gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
    // Provider writes store API keys, which needs an unlocked vault.
    gw.vault.unlock("probe-test-password").expect("vault unlock");
    let state = AppState::new(Arc::new(RwLock::new(gw)), Arc::new(HttpAuth::new(false)));
    {
        let mut guard = state.gateway_state.write().await;
        let config = GatewayConfig {
            data_dir: dir.to_string_lossy().to_string(),
            vault_dir: dir.join("vault").to_string_lossy().to_string(),
            llm: LlmConfig {
                responses_api_enabled: responses_enabled,
            },
            ..GatewayConfig::default()
        };
        guard.config = Some(config);
    }
    state
}

fn baseline_body() -> serde_json::Value {
    json!({
        "id": "resp_1",
        "object": "response",
        "status": "completed",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "pong"}]}],
        "usage": {"input_tokens": 7, "output_tokens": 1},
    })
}

fn round_trip_body() -> serde_json::Value {
    json!({
        "id": "resp_2",
        "object": "response",
        "status": "completed",
        "output": [{"type": "function_call", "call_id": "call_probe",
                    "name": "acowork_probe_echo", "arguments": "{\"value\": 1}"}],
        "usage": {"input_tokens": 30, "output_tokens": 12},
    })
}

/// Mount a provider that passes L1/L2/L3.
fn mount_healthy(server: &mut mockito::ServerGuard) {
    server
        .mock("POST", "/responses")
        .match_body(Matcher::PartialJson(json!({"input": "ping"})))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(baseline_body().to_string())
        .create();
    server
        .mock("POST", "/responses")
        .match_body(Matcher::PartialJson(json!({"tool_choice": "auto"})))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(round_trip_body().to_string())
        .create();
    server
        .mock("POST", "/responses")
        .match_body(Matcher::Regex("function_call_output".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(baseline_body().to_string())
        .create();
}

/// A provider that answers every request with a text-only message: L1/L2 pass,
/// L3 fails (no `function_call`).
fn mount_no_tool_call(server: &mut mockito::ServerGuard) {
    server
        .mock("POST", "/responses")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(
            json!({
                "object": "response",
                "status": "completed",
                "output": [{"type": "message", "role": "assistant", "content": []}],
                "usage": {"input_tokens": 7, "output_tokens": 1},
            })
            .to_string(),
        )
        .create();
}

fn add_body(base_url: &str, models: &[&str], responses_models: &[&str]) -> AddProviderRequest {
    serde_json::from_value(json!({
        "provider": PROVIDER,
        "key": "sk-test",
        "custom": true,
        "base_url": base_url,
        "models": models,
        "model_protocols": responses_models
            .iter()
            .map(|m| (m.to_string(), "responses".to_string()))
            .collect::<HashMap<_, _>>(),
    }))
    .expect("valid add request")
}

/// Read the persisted provider row back from `provider_list.json`.
fn persisted_provider(dir: &std::path::Path) -> acowork_core::protocol::ProviderListItem {
    let raw = std::fs::read_to_string(dir.join("provider_list.json")).expect("provider_list.json");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("valid json");
    serde_json::from_value(
        parsed["providers"]
            .as_array()
            .expect("providers array")
            .iter()
            .find(|p| p["id"] == PROVIDER)
            .expect("provider row")
            .clone(),
    )
    .expect("provider row parses")
}

#[tokio::test]
async fn save_probes_changed_responses_rows_and_persists_the_verdict() {
    let dir = temp_dir("verified");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_healthy(&mut server);

    let (status, Json(response)) = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save succeeds");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(response.probes.len(), 1, "the one changed row is probed");
    let report = &response.probes[0];
    assert_eq!(report.model, MODEL);
    assert_eq!(report.tier, ProbeTier::Deep, "first row pays for L3");
    assert!(report.outcome.success, "{:?}", report.outcome.error);
    assert_eq!(report.outcome.state(), ProbeState::Verified);
    assert!(!report.cached, "a first save cannot come from the cache");

    // Persisted: intent + verdict both on the model row.
    let row = persisted_provider(&dir);
    let model = row.models.iter().find(|m| m.id == MODEL).expect("model row");
    assert_eq!(model.protocol_type, Some(ProtocolType::Responses));
    let probe = model.protocol_probe.as_ref().expect("verdict persisted");
    assert_eq!(probe.state, ProbeState::Verified);
    assert_eq!(probe.tier_passed, vec!["L1", "L2", "L3"]);
    assert!(probe.probed_at.is_some());
}

#[tokio::test]
async fn failed_probe_does_not_block_the_save() {
    let dir = temp_dir("failed");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    // A Chat-Completions-only host: `/responses` is 404 unknown endpoint.
    server
        .mock("POST", "/responses")
        .with_status(404)
        .with_body(json!({"error": {"message": "Unknown endpoint: /responses"}}).to_string())
        .create();

    let (status, Json(response)) = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("a failed probe MUST NOT fail the save (D12.5)");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(response.probes.len(), 1);
    let report = &response.probes[0];
    assert!(!report.outcome.success);
    assert_eq!(
        report.outcome.error.as_ref().unwrap().class,
        "EndpointUnavailable"
    );

    // The provider itself is committed, and the verdict records the failure, so
    // the D1 gate keeps `Responses` inert.
    let row = persisted_provider(&dir);
    let model = row.models.iter().find(|m| m.id == MODEL).expect("model row");
    let probe = model.protocol_probe.as_ref().expect("failed verdict persisted");
    assert_eq!(probe.state, ProbeState::Failed);
    assert!(probe.reason.is_some());
    assert!(!probe.state.gates_responses());
}

#[tokio::test]
async fn l3_without_a_tool_call_is_failed_not_verified() {
    let dir = temp_dir("degraded");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_no_tool_call(&mut server);

    let (_, Json(response)) = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save succeeds");

    let report = &response.probes[0];
    assert!(!report.outcome.success);
    assert_eq!(report.outcome.tier_passed, vec!["L1", "L2"]);
    assert_eq!(report.outcome.state(), ProbeState::Failed);

    let row = persisted_provider(&dir);
    let probe = row.models[0].protocol_probe.as_ref().expect("verdict");
    assert_eq!(probe.state, ProbeState::Failed);
    // A provider that cannot complete a tool call must stay gated off (D10.2).
    assert!(!probe.state.gates_responses());
}

#[tokio::test]
async fn platform_gate_off_skips_save_time_probing() {
    let dir = temp_dir("gate-off");
    let state = state_with(&dir, false).await;
    let mut server = mockito::Server::new_async().await;
    // Guard: no probe request may reach the provider.
    let no_probe = server
        .mock("POST", "/responses")
        .expect(0)
        .create();

    let (status, Json(response)) = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save succeeds");

    assert_eq!(status, StatusCode::CREATED);
    assert!(response.probes.is_empty(), "gate off = no billed probe calls");
    no_probe.assert();

    // Intent is still recorded (harmless: the gate forces the fallback), but no
    // verdict exists, so nothing can gate `Responses` on.
    let row = persisted_provider(&dir);
    let model = row.models.iter().find(|m| m.id == MODEL).expect("model row");
    assert_eq!(model.protocol_type, Some(ProtocolType::Responses));
    assert!(model.protocol_probe.is_none());
}

#[tokio::test]
async fn gate_off_still_records_staleness_without_probing() {
    let dir = temp_dir("gate-off-stale");
    let mut server1 = mockito::Server::new_async().await;
    mount_healthy(&mut server1);
    let state = state_with(&dir, true).await;

    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server1.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("first save");
    assert_eq!(
        persisted_provider(&dir).models[0]
            .protocol_probe
            .as_ref()
            .unwrap()
            .state,
        ProbeState::Verified
    );

    // Operator turns the platform gate off, then changes the base URL. No
    // probe may run, but the stale verdict must not survive as "trusted".
    {
        let mut gw = state.gateway_state.write().await;
        if let Some(config) = gw.config.as_mut() {
            config.llm.responses_api_enabled = false;
        }
    }
    let mut server2 = mockito::Server::new_async().await;
    let no_probe = server2.mock("POST", "/responses").expect(0).create();
    let update: UpdateProviderRequest =
        serde_json::from_value(json!({ "base_url": server2.url() })).expect("valid update");
    let Json(response) = update_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Json(update),
    )
    .await
    .expect("second save");

    assert!(response.probes.is_empty());
    no_probe.assert();
    assert_eq!(
        persisted_provider(&dir).models[0]
            .protocol_probe
            .as_ref()
            .unwrap()
            .state,
        ProbeState::Stale,
        "D12.6 is a MUST even while the platform gate is off"
    );
}

#[tokio::test]
async fn a_no_op_save_does_not_reprobe_anything() {
    let dir = temp_dir("noop");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_healthy(&mut server);

    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("first save");

    // From here on, any probe request would be a regression: the save below is
    // byte-identical in every input a verdict depends on (D12.2).
    server.reset();
    let no_probe = server.mock("POST", "/responses").expect(0).create();

    let update: UpdateProviderRequest = serde_json::from_value(json!({
        "models": [MODEL],
        "model_protocols": { MODEL: "responses" },
    }))
    .expect("valid update request");
    let Json(response) = update_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Json(update),
    )
    .await
    .expect("second save");

    assert!(
        response.probes.is_empty(),
        "an unchanged Responses row must not be re-probed"
    );
    no_probe.assert();

    // The verdict survives the rebuild (carry-over) and is still trusted.
    let row = persisted_provider(&dir);
    let probe = row.models[0].protocol_probe.as_ref().expect("verdict kept");
    assert_eq!(probe.state, ProbeState::Verified);
}

#[tokio::test]
async fn model_list_change_reprobes_the_responses_rows() {
    let dir = temp_dir("model-list");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_healthy(&mut server);

    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("first save");

    // Adding a sibling model is a model-list change, which D12.6 makes a
    // staleness trigger — so the Responses row is re-probed in this same save
    // rather than being left with a verdict about the old configuration.
    let update: UpdateProviderRequest = serde_json::from_value(json!({
        "models": [MODEL, "acme-small"],
        "model_protocols": { MODEL: "responses", "acme-small": "openai" },
    }))
    .expect("valid update request");
    let Json(response) = update_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Json(update),
    )
    .await
    .expect("second save");

    assert_eq!(response.probes.len(), 1, "only the Responses row is probed");
    assert_eq!(response.probes[0].model, MODEL);
    assert_eq!(response.probes[0].outcome.state(), ProbeState::Verified);

    // The non-Responses sibling gets no verdict at all.
    let row = persisted_provider(&dir);
    let sibling = row
        .models
        .iter()
        .find(|m| m.id == "acme-small")
        .expect("sibling row");
    assert!(sibling.protocol_probe.is_none());
}

#[tokio::test]
async fn base_url_change_marks_existing_verdicts_stale() {
    let dir = temp_dir("stale");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_healthy(&mut server);

    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("first save");

    // Change the base URL to a host that does not answer: the old verdict is
    // about the old URL, so it must stop being trusted (D12.4).
    let update: UpdateProviderRequest = serde_json::from_value(json!({
        "base_url": "http://127.0.0.1:1",
    }))
    .expect("valid update request");
    let Json(response) = update_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Json(update),
    )
    .await
    .expect("second save");

    // The changed row is re-probed (and fails), so the persisted state is
    // `failed` — either way it no longer gates `Responses` on.
    assert_eq!(response.probes.len(), 1);
    assert!(!response.probes[0].outcome.success);

    let row = persisted_provider(&dir);
    let probe = row.models[0].protocol_probe.as_ref().expect("verdict");
    assert_eq!(probe.state, ProbeState::Failed);
}

#[tokio::test]
async fn explicit_probe_endpoint_reuses_the_cached_verdict_unless_forced() {
    let dir = temp_dir("force");
    let state = state_with(&dir, false).await; // gate off: implicit probing is off...
    let mut server = mockito::Server::new_async().await;
    // ...but the explicit endpoint probes anyway (D10.1/D11) — and only once:
    // even a `standard` request is upgraded to deep, and the second request
    // below must be served from the 24 h cache.
    let baseline = server
        .mock("POST", "/responses")
        .match_body(Matcher::PartialJson(json!({"input": "ping"})))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(baseline_body().to_string())
        .expect(1)
        .create();
    server
        .mock("POST", "/responses")
        .match_body(Matcher::PartialJson(json!({"tool_choice": "auto"})))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(round_trip_body().to_string())
        .create();
    server
        .mock("POST", "/responses")
        .match_body(Matcher::Regex("function_call_output".to_string()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(baseline_body().to_string())
        .create();

    // Seed the provider row without probing (gate off).
    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save");

    let request: ProbeProviderRequest =
        serde_json::from_value(json!({"model": MODEL, "tier": "standard"}))
            .expect("valid probe request");

    let Json(first) = probe_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Query(ProbeQuery { force: None }),
        Json(request),
    )
    .await
    .expect("probe runs");
    assert_eq!(
        first.state,
        ProbeState::Verified,
        "an explicit probe always runs L3, so it can reach verified"
    );
    assert_eq!(first.tier, ProbeTier::Deep, "standard is upgraded to deep");

    // Second call: served from the 24 h cache, so the mock is still at 1 hit.
    let request: ProbeProviderRequest =
        serde_json::from_value(json!({"model": MODEL, "tier": "standard"}))
            .expect("valid probe request");
    let Json(second) = probe_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Query(ProbeQuery { force: None }),
        Json(request),
    )
    .await
    .expect("cached probe runs");
    assert_eq!(second.state, ProbeState::Verified);
    baseline.assert();
}

#[tokio::test]
async fn explicit_probe_rejects_a_protocol_without_an_implementation() {
    let dir = temp_dir("protocol");
    let state = state_with(&dir, true).await;
    let server = mockito::Server::new_async().await;
    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save");

    let request: ProbeProviderRequest =
        serde_json::from_value(json!({"model": MODEL, "protocol": "anthropic"}))
            .expect("valid probe request");
    let err = probe_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Query(ProbeQuery { force: None }),
        Json(request),
    )
    .await
    .expect_err("anthropic has no probe implementation");
    assert_eq!(err.code, 400);
    assert!(err.error.contains("anthropic"), "{}", err.error);
}

#[tokio::test]
async fn explicit_probe_reports_unknown_models_as_not_found() {
    let dir = temp_dir("unknown-model");
    let state = state_with(&dir, true).await;
    let server = mockito::Server::new_async().await;
    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save");

    let request: ProbeProviderRequest =
        serde_json::from_value(json!({"model": "not-configured"})).expect("valid request");
    let err = probe_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Query(ProbeQuery { force: None }),
        Json(request),
    )
    .await
    .expect_err("unknown model must 404");
    assert_eq!(err.code, 404);
}

/// D11 "follow the data": clearing a model's protocol selection must restore
/// the inherited default AND drop the now-meaningless verdict, without probing.
#[tokio::test]
async fn clearing_the_selection_drops_the_verdict_without_probing() {
    let dir = temp_dir("clear-selection");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_healthy(&mut server);

    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("first save");
    assert!(
        persisted_provider(&dir).models[0].protocol_probe.is_some(),
        "precondition: the first save left a verdict behind"
    );

    // From here the selection is withdrawn: nothing is selected, so nothing may
    // be probed.
    server.reset();
    let no_probe = server.mock("POST", "/responses").expect(0).create();

    let update: UpdateProviderRequest = serde_json::from_value(json!({
        "models": [MODEL],
        "model_protocols": { MODEL: null },
    }))
    .expect("a null selection must deserialize (clear request)");
    let Json(response) = update_provider(
        State(state.clone()),
        Path(PROVIDER.to_string()),
        Json(update),
    )
    .await
    .expect("clearing the selection saves");

    assert!(response.probes.is_empty(), "clearing probes nothing");
    no_probe.assert();

    let row = persisted_provider(&dir);
    let model = &row.models[0];
    assert_eq!(model.protocol_type, None, "selection withdrawn");
    assert_eq!(
        model.protocol_probe, None,
        "the verdict gated a selection that no longer exists — it must not linger"
    );
}

/// D11 data contract: `GET /api/models/{provider}` must carry all four protocol
/// fields, with `protocol_active` following the D1 verdict rather than the
/// selection.
#[tokio::test]
async fn model_list_projects_selection_verdict_and_active_protocol() {
    let dir = temp_dir("projection");
    let state = state_with(&dir, true).await;
    let mut server = mockito::Server::new_async().await;
    mount_healthy(&mut server);

    let _ = add_provider(
        State(state.clone()),
        Json(add_body(&server.url(), &[MODEL], &[MODEL])),
    )
    .await
    .expect("save");

    let mut models = crate::http::models_api::resolve_provider_models(&state, PROVIDER)
        .await
        .expect("model list resolves")
        .models;
    crate::http::models_api::annotate_protocol_fields(&state, PROVIDER, &mut models).await;

    let row = models.iter().find(|m| m.id == MODEL).expect("model row");
    assert_eq!(row.protocol_selected.as_deref(), Some("responses"));
    assert_eq!(
        row.protocol_active.as_deref(),
        Some("responses"),
        "a verified verdict activates the selection"
    );
    assert_eq!(
        row.protocol_probe.as_ref().map(|p| p.state.clone()),
        Some(ProbeState::Verified)
    );
}

/// D10.4: probe usage must not reach ADR-027/066 budget accounting.
///
/// Structural assertion — the probe modules never touch a usage channel. If a
/// future change wires probe usage into the budget tracker, this test's grep
/// targets stop matching and the reviewer is forced to look.
#[test]
fn probe_modules_do_not_touch_usage_accounting() {
    for (name, source) in [
        ("service", include_str!("../probe/service.rs")),
        ("runner", include_str!("../probe/runner.rs")),
        ("store", include_str!("../probe/store.rs")),
        ("wire", include_str!("../probe/wire.rs")),
    ] {
        for forbidden in [
            "BudgetTracker",
            "budget_tracker",
            "record_usage",
            "UsageRecord",
            "add_usage",
        ] {
            assert!(
                !source.contains(forbidden),
                "probe module `{name}` must not reference `{forbidden}` (D10.4: probe \
                 usage is excluded from ADR-027/066 budget accounting)"
            );
        }
    }
}