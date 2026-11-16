//! `GET /api/global-resources` — Runtime active pull endpoint.
//!
//! Returns the current snapshot of all 5 global resource topics. Each
//! value is **base64-encoded `DataEnvelope` protobuf bytes**, structurally
//! identical to the payload that the Gateway would publish on the
//! matching MQTT retained topic:
//!
//! | JSON `topics` key                 | MQTT topic                       | Protobuf message              |
//! |-----------------------------------|----------------------------------|-------------------------------|
//! | `acowork/global/providers`        | `acowork/global/providers`       | `AvailableProviders`          |
//! | `acowork/global/mcps`             | `acowork/global/mcps`            | `AvailableMcps`               |
//! | `acowork/global/embedding_models` | `acowork/global/embedding_models`| `AvailableEmbeddingModels`    |
//! | `acowork/global/user_profile`     | `acowork/global/user_profile`    | `AvailableUsers` (ADR-042)    |
//! | `acowork/global/bootstrap`        | `acowork/global/bootstrap`       | `BootstrapState` (ADR-059)    |
//!
//! ## Protocol contract: "not ready" vs "empty" (Bug B fix, v2)
//!
//! The endpoint distinguishes two semantically different conditions
//! that the previous implementation conflated under "200 + empty payload":
//!
//! | Gateway `BootstrapPhase`     | HTTP status | Body                                                                  | `Retry-After` |
//! |------------------------------|-------------|-----------------------------------------------------------------------|---------------|
//! | `Booting`                    | `503`       | `{instance_id, phase, phase_detail, retry_after_seconds, error}`      | `1`–`2`s      |
//! | `Unspecified` (wiring defect)| `503`       | same                                                                  | `2`s          |
//! | `Failed`                     | `503`       | same                                                                  | `10`s         |
//! | `ShuttingDown`               | `503`       | same with `retry_after_seconds: -1` (do not retry)                     | `-1`          |
//! | `Ready`                      | `200`       | full `GlobalResourcesView` (topics may be empty arrays — legitimate)  | N/A           |
//! | `Degraded`                   | `200`       | same as `Ready` (treat as authoritative)                              | N/A           |
//!
//! **Why this distinction matters**: when the Gateway has just unlocked
//! its Vault but no provider has been onboarded yet, the
//! `AvailableProviders` snapshot legitimately contains zero entries.
//! Under the previous "always 200" design the Runtime treated the empty
//! list as "no provider configured" and skipped waiting, causing the
//! first chat to fail with "unexpected error" until a manual
//! `model_switch` re-broke the staleness check (Bug B). With this
//! revision the Gateway returns `503 + Retry-After` while it is still
//! `Booting`, and the Runtime keeps polling (see
//! `acowork-runtime/src/startup/global_resources_pull.rs`) until it
//! sees `200`. The Runtime never writes a half-coherent snapshot into
//! its `AvailableResourceCache` from a `503`.
//!
//! `503` responses are also returned when the Gateway is `Failed` or
//! `ShuttingDown`; `retry_after_seconds: -1` is a sentinel meaning
//! "do not retry, abort the pull".
//!
//! ## Does NOT depend on Runtime being online
//!
//! The Gateway is the authoritative source for these resources (Vault +
//! resource_cache + embed_process). Whether any Runtime is alive does
//! not affect this endpoint's availability, so it lives under
//! §4 (Gateway-native, no Runtime dependency).
//!
//! ## Design trade-off: base64 protobuf vs. inline JSON
//!
//! We deliberately do NOT flatten the 6 protobuf structures into inline
//! JSON because:
//! 1. prost-generated types lack `serde::Serialize` / `Deserialize`
//!    derives; flattening would require adding
//!    `#[derive(serde::Serialize, Deserialize)]` to every message in
//!    `mqtt_payload.proto`, and resolving `oneof`-field `serde` quirks
//!    (tag/flatten) — a wide-blast-radius change.
//! 2. Returning base64-encoded protobuf bytes keeps the HTTP wire format
//!    **bit-identical** to the MQTT wire format: new fields in `.proto`
//!    surface in both channels for free, and the Runtime's handling is
//!    zero-special-case.
//!
//! Total snapshot size is small (typically < 5 KB across all 5 topics);
//! the base64 expansion is negligible.

use std::collections::BTreeMap;

use axum::{
    extract::State,
    http::{header::RETRY_AFTER, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::Serialize;

use acowork_core::mqtt_proto::{data_envelope::Payload, DataEnvelope};

use crate::bootstrap::orchestrator::BootstrapPhase;
use crate::http::routes::AppState;
use crate::mqtt::global_resources_builders::{
    build_available_embedding_models, build_available_mcps, build_available_providers,
    build_available_users,
};

/// MQTT topic constants — must stay in lockstep with
/// `mqtt::global_resources_publisher::topics` and
/// `mqtt::bootstrap_publisher::TOPIC_BOOTSTRAP`.
mod topics {
    pub const PROVIDERS: &str = "acowork/global/providers";
    pub const MCPS: &str = "acowork/global/mcps";
    pub const EMBEDDING_MODELS: &str = "acowork/global/embedding_models";
    pub const USER_PROFILE: &str = "acowork/global/user_profile";
    pub const BOOTSTRAP: &str = "acowork/global/bootstrap";
}

/// Sentinel `Retry-After` value meaning "do not retry" (used for
/// `BootstrapPhase::ShuttingDown` and `Failed` after a long timeout).
/// Kept as a small negative number so the wire format is still valid
/// (the header value is a string).
pub const RETRY_AFTER_DONT_RETRY: i64 = -1;

/// HTTP projection of the global resources snapshot (200 OK body).
///
/// Field-for-field contract:
/// - `instance_id` — ADR-059 §5.3: current Gateway generation id.
///   Runtime compares with its locally cached instance_id; a mismatch
///   triggers `AvailableResourceCache::update_from_mqtt`'s built-in
///   generation-switch logic (drops every old-generation resource
///   snapshot before applying the new ones).
/// - `topics` — sorted map (`BTreeMap`) so the response is
///   deterministic across calls; easier to diff in tests. Each value is
///   the base64 encoding of the `DataEnvelope` protobuf bytes that the
///   Gateway would publish on the corresponding MQTT retained topic.
#[derive(Debug, Serialize)]
pub struct GlobalResourcesView {
    pub instance_id: String,
    pub topics: BTreeMap<&'static str, String>,
}

/// HTTP projection of a "not ready" response (503 body).
///
/// Returned while the Gateway is `Booting` / `Failed` /
/// `ShuttingDown` / `Unspecified` so consumers can distinguish
/// "temporarily unable to answer" from "the answer is empty".
///
/// Wire-format fields:
/// - `instance_id` — same generation id semantics as
///   `GlobalResourcesView`. Empty if the orchestrator is not yet
///   attached (the wiring-defect case).
/// - `phase` — the proto enum as `SCREAMING_SNAKE_CASE`
///   (`BOOTING`/`READY`/…). Kept stringly-typed on the wire so future
///   phases do not break older clients.
/// - `phase_detail` — orchestrator-derived human-readable detail
///   (e.g. `"vault unlocking"`).
/// - `retry_after_seconds` — recommended delay before retrying:
///   positive = sleep that many seconds; `-1 = ` do not retry,
///   abort the pull (used for `ShuttingDown` / long-term `Failed`).
/// - `error` — short stable error code for programmatic handling
///   (`NOT_READY` / `FAILED` / `SHUTTING_DOWN`).
#[derive(Debug, Serialize)]
pub struct NotReadyView {
    pub instance_id: String,
    pub phase: BootstrapPhase,
    pub phase_detail: String,
    pub retry_after_seconds: i64,
    pub error: &'static str,
}

/// Build the router for `GET /api/global-resources`.
pub fn global_resources_routes() -> Router<AppState> {
    Router::new().route("/api/global-resources", get(get_global_resources))
}

/// Compute the recommended `Retry-After` seconds for a non-ready
/// `BootstrapPhase`.
///
/// Tuning rationale:
/// - `Booting`: subsystems are usually milliseconds away from
///   `Ready`; keep the retry short so the user-visible startup
///   latency is small but the consumer does not busy-loop.
/// - `Unspecified`: same as `Booting` (orchestrator not attached,
///   usually a wiring defect — short retries surface the bug fast).
/// - `Failed`: a required subsystem has hard-failed; retries are
///   unlikely to help within the typical session lifetime, but the
///   Gateway can recover on its own (e.g. operator restarts the
///   stuck subsystem) — wait longer to avoid log noise.
/// - `ShuttingDown`: gateway is exiting, no point retrying.
fn retry_after_for_phase(phase: BootstrapPhase) -> i64 {
    match phase {
        BootstrapPhase::Booting => 2,
        BootstrapPhase::Unspecified => 2,
        BootstrapPhase::Failed => 10,
        BootstrapPhase::ShuttingDown => RETRY_AFTER_DONT_RETRY,
        // Ready / Degraded should not enter this function — the
        // caller is expected to short-circuit before reaching here.
        BootstrapPhase::Ready | BootstrapPhase::Degraded => 0,
    }
}

/// Stable error code for `NotReadyView.error`. Kept as
/// `&'static str` so it is part of the API surface and can be matched
/// by clients without parsing free-form text.
fn error_code_for_phase(phase: BootstrapPhase) -> &'static str {
    match phase {
        BootstrapPhase::ShuttingDown => "SHUTTING_DOWN",
        BootstrapPhase::Failed => "FAILED",
        BootstrapPhase::Unspecified | BootstrapPhase::Booting => "NOT_READY",
        // Should not be reached; defensive default so the handler
        // still returns a coherent body if invariants are violated.
        BootstrapPhase::Ready | BootstrapPhase::Degraded => "UNEXPECTED_READY",
    }
}

/// Render a `NotReadyView` as a `503` response with a `Retry-After`
/// header (RFC 7231 §7.1.3 — delta-seconds form, which is what every
/// major HTTP client understands).
///
/// We pass the seconds as an integer string (RFC allows either an
/// HTTP-date or delta-seconds; delta-seconds is simpler to parse and
/// is the form used by axios / reqwest / curl retry plugins). For
/// `retry_after_seconds = -1` we still emit a numeric value
/// (`"-1"`) so the header is always present — clients that do not
/// understand the sentinel will simply retry after 1 second, which is
/// harmless; clients that DO understand the sentinel (see
/// `acowork-runtime/src/startup/global_resources_pull.rs`) abort the
/// pull instead.
fn not_ready_response(view: NotReadyView) -> Response {
    // Snapshot the seconds before the view is moved into the body so
    // the Retry-After header can still reference it.
    let retry_after_seconds = view.retry_after_seconds;
    let body = Json(view);
    let mut response = (StatusCode::SERVICE_UNAVAILABLE, body).into_response();
    if let Ok(value) = HeaderValue::from_str(&retry_after_seconds.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

/// `GET /api/global-resources` — full snapshot of all global resource
/// topics, encoded as base64 protobuf bytes.
///
/// Status semantics:
/// - `200 OK` when `BootstrapPhase ∈ {Ready, Degraded}`. Body is the
///   full `GlobalResourcesView`. Empty `topics` arrays are a legitimate
///   answer (the user has not onboarded any providers yet), not an
///   error.
/// - `503 Service Unavailable` otherwise. Body is `NotReadyView`. The
///   `Retry-After` header carries the recommended delay (or `-1` to
///   abort the pull).
///
/// Runtime contract (consumer side) — see
/// `acowork-runtime/src/startup/global_resources_pull.rs`:
/// 1. On `503`: parse `Retry-After` (header or
///    `retry_after_seconds` body field) and sleep. Do NOT update the
///    local `AvailableResourceCache` — the previous snapshot may be
///    more coherent than the empty data we would otherwise write.
/// 2. On `200`: for each entry, decode `topics[k]` from base64 and call
///    `AvailableResourceCache::update_from_mqtt(topic, &bytes)`. The
///    handler is identical to the MQTT-retained path, so version
///    checks and stale-retained rejection work uniformly.
/// 3. Compare `instance_id` against the locally cached one — a
///    mismatch triggers generation switch (clears all old snapshots
///    before applying the new ones).
pub async fn get_global_resources(State(state): State<AppState>) -> Response {
    // ── Phase gate (Bug B fix v2) ──────────────────────────────────────
    // The orchestrator is attached during Gateway bootstrap. If it is
    // `None`, we treat the endpoint as not-ready (wiring defect) rather
    // than returning empty data, for the same reason as `Booting`: the
    // consumer should wait, not assume emptiness means "no data".
    // `Orchestrator::snapshot()` returns `BootstrapSnapshot` by value
    // (see `bootstrap/orchestrator.rs`), so the `Option<BootstrapSnapshot>`
    // produced here is already owned — no `.cloned()` is needed (and
    // adding it would try to call `Iterator::cloned`, which doesn't
    // exist on the owned `Option<BootstrapSnapshot>` we have here).
    let snapshot = {
        let gw = state.gateway_state.read().await;
        gw.bootstrap.orchestrator
            .as_ref()
            .map(|o| o.snapshot())
    };

    let snapshot = match snapshot {
        Some(s) => s,
        None => {
            let retry_after = retry_after_for_phase(BootstrapPhase::Unspecified);
            let view = NotReadyView {
                instance_id: String::new(),
                phase: BootstrapPhase::Unspecified,
                phase_detail: "bootstrap_orchestrator not attached".to_string(),
                retry_after_seconds: retry_after,
                error: error_code_for_phase(BootstrapPhase::Unspecified),
            };
            return not_ready_response(view);
        }
    };

    match snapshot.phase {
        BootstrapPhase::Ready | BootstrapPhase::Degraded => {
            // Authoritative path — drop through to the body builder.
        }
        phase @ (BootstrapPhase::Booting
        | BootstrapPhase::Failed
        | BootstrapPhase::ShuttingDown
        | BootstrapPhase::Unspecified) => {
            let view = NotReadyView {
                instance_id: snapshot.instance_id.clone(),
                phase,
                phase_detail: snapshot.phase_detail.clone(),
                retry_after_seconds: retry_after_for_phase(phase),
                error: error_code_for_phase(phase),
            };
            return not_ready_response(view);
        }
    }

    // ── Build all 5 payloads from the GatewayState snapshot ────────────
    // The builders live in `mqtt::global_resources_builders` so HTTP and
    // MQTT paths stay in lockstep — any new field added to a protobuf
    // message appears in both channels without separate mapping code.
    let gw = state.gateway_state.read().await;
    let providers = build_available_providers(&gw);
    let mcps = build_available_mcps(&gw);
    let embedding_models = build_available_embedding_models(&gw);
    let user_profile = build_available_users(&gw);

    // We already short-circuited on the non-ready phase above, so the
    // bootstrap payload here is for the authoritatively-ready case.
    let bootstrap = snapshot.to_proto();

    // Drop the read lock before any encoding work — encoding is
    // pure CPU and can be expensive for large payloads.
    let instance_id = snapshot.instance_id.clone();
    drop(gw);

    let mut topics = BTreeMap::new();
    topics.insert(
        topics::PROVIDERS,
        encode_envelope_b64(&providers, Payload::AvailableProviders),
    );
    topics.insert(
        topics::MCPS,
        encode_envelope_b64(&mcps, Payload::AvailableMcps),
    );
    topics.insert(
        topics::EMBEDDING_MODELS,
        encode_envelope_b64(&embedding_models, Payload::AvailableEmbeddingModels),
    );
    topics.insert(
        topics::USER_PROFILE,
        encode_envelope_b64(&user_profile, Payload::AvailableUsers),
    );
    topics.insert(
        topics::BOOTSTRAP,
        encode_envelope_b64(&bootstrap, Payload::BootstrapState),
    );

    Json(GlobalResourcesView {
        instance_id,
        topics,
    })
    .into_response()
}

/// Wrap a protobuf message into a `DataEnvelope` and return the
/// base64-encoded wire bytes.
fn encode_envelope_b64<P, F>(payload: &P, into: F) -> String
where
    P: prost::Message + Clone,
    F: FnOnce(P) -> Payload,
{
    let envelope = DataEnvelope {
        version: 1,
        payload: Some(into(payload.clone())),
    };
    let bytes = prost::Message::encode_to_vec(&envelope);
    BASE64.encode(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    use acowork_core::mqtt_proto::DataEnvelope;

    use crate::bootstrap::orchestrator::{BootstrapOrchestrator, BootstrapSnapshot};
    use crate::bootstrap::registry::SubsystemReadinessRegistry;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;

    /// Consume the response and parse its body as JSON `Value`. The wire
    /// views are `Serialize`-only by design, and `NotReadyView` carries
    /// a `&'static str` error code that cannot be safely deserialised
    /// from a borrowed buffer, so tests assert on the JSON shape instead
    /// of round-tripping through the Rust structs.
    async fn response_body_json(resp: Response, limit: usize) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), limit)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("body must be valid JSON")
    }

    /// Build an `AppState` whose `GatewayState` carries an optional
    /// pre-built `BootstrapOrchestrator`. The orchestrator is created
    /// via [`BootstrapOrchestrator::from_snapshot_for_test`] so we can
    /// exercise every `BootstrapPhase` branch (Booting / Ready /
    /// Degraded / Failed / ShuttingDown) without standing up real
    /// subsystems. `None` exercises the orchestrator-not-attached path.
    //
    // Unique-per-call directory suffix: tests in this module run
    // concurrently inside one process, and a shared fixed path made
    // `remove_dir_all` + `create_dir_all` race each other (sporadic
    // OS error 183).
    static DIR_SEQ: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// ADR-059 §5.3: the Gateway generation id is a UUIDv4. Tests must
    /// use one coherent value — the snapshot the handler projects and
    /// the orchestrator that produced it carry the same instance id.
    const GATEWAY_GENERATION: &str = "6a7b8c9d-0e1f-4022-8132-e24c5d6e7f8a";

    async fn test_state_with_snapshot(snapshot: Option<BootstrapSnapshot>) -> AppState {
        let dir = crate::test_support::unique_temp_dir(format!(
            "acowork-test-global-resources-api-{}-{}",
            std::process::id(),
            DIR_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = Arc::new(RwLock::new(GatewayState::new(&dir.to_string_lossy())));
        // We deliberately do NOT attach a BootstrapOrchestrator here —
        // the helper exposes the `with_snapshot` variant for tests that
        // need a controlled phase. The default branch exercises the
        // "orchestrator not attached" 503 path.
        if let Some(s) = snapshot {
            let registry = SubsystemReadinessRegistry::new_shared();
            let orchestrator = BootstrapOrchestrator::from_snapshot_for_test(
                GATEWAY_GENERATION.to_string(),
                registry,
                s,
            );
            // Async write so we do not need a multi-threaded runtime.
            let mut guard = gw_state.write().await;
            guard.bootstrap.orchestrator = Some(orchestrator);
        }
        AppState::new(gw_state, Arc::new(HttpAuth::new(false)))
    }

    async fn test_state() -> AppState {
        test_state_with_snapshot(None).await
    }

    fn snapshot_with_phase(phase: BootstrapPhase, detail: &str) -> BootstrapSnapshot {
        BootstrapSnapshot {
            protocol_version: 1,
            instance_id: GATEWAY_GENERATION.to_string(),
            version: 7,
            phase,
            phase_detail: detail.to_string(),
            issued_at_ms: 1_700_000_000_000,
        }
    }

    /// No orchestrator attached → 503 with the `UNSPECIFIED`-equivalent
    /// error code. The body must NOT include any `topics` data so a
    /// misbehaving consumer that ignores the status code still sees
    /// "I have nothing for you".
    #[tokio::test]
    async fn not_ready_when_no_orchestrator() {
        let state = test_state().await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(RETRY_AFTER).unwrap(),
            "2",
            "no-orchestrator defaults to a 2s retry like Unspecified"
        );
        let body = response_body_json(resp, 4096).await;
        assert_eq!(body["phase"], "UNSPECIFIED");
        assert_eq!(body["error"], "NOT_READY");
        assert_eq!(body["retry_after_seconds"], 2);
    }

    #[tokio::test]
    async fn not_ready_when_phase_is_booting() {
        let state = test_state_with_snapshot(Some(snapshot_with_phase(
            BootstrapPhase::Booting,
            "vault unlocking",
        )))
        .await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get(RETRY_AFTER).unwrap(), "2");
        let body = response_body_json(resp, 4096).await;
        assert_eq!(body["phase"], "BOOTING");
        assert_eq!(body["phase_detail"], "vault unlocking");
        assert_eq!(body["error"], "NOT_READY");
        assert_eq!(body["retry_after_seconds"], 2);
    }

    #[tokio::test]
    async fn not_ready_when_phase_is_failed_uses_longer_retry() {
        let state = test_state_with_snapshot(Some(snapshot_with_phase(
            BootstrapPhase::Failed,
            "vault init failed",
        )))
        .await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(RETRY_AFTER).unwrap(),
            "10",
            "Failed phase uses a longer retry to avoid log spam"
        );
        let body = response_body_json(resp, 4096).await;
        assert_eq!(body["error"], "FAILED");
        assert_eq!(body["retry_after_seconds"], 10);
    }

    #[tokio::test]
    async fn not_ready_when_shutting_down_uses_sentinel_retry() {
        let state = test_state_with_snapshot(Some(snapshot_with_phase(
            BootstrapPhase::ShuttingDown,
            "exiting",
        )))
        .await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers().get(RETRY_AFTER).unwrap(),
            "-1",
            "ShuttingDown uses the -1 sentinel meaning 'abort pull'"
        );
        let body = response_body_json(resp, 4096).await;
        assert_eq!(body["error"], "SHUTTING_DOWN");
        assert_eq!(body["retry_after_seconds"], -1);
        assert_eq!(body["phase"], "SHUTTING_DOWN");
    }

    #[tokio::test]
    async fn ready_phase_returns_full_snapshot() {
        let state = test_state_with_snapshot(Some(snapshot_with_phase(
            BootstrapPhase::Ready,
            "all subsystems ready",
        )))
        .await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers().get(RETRY_AFTER).is_none(),
            "Retry-After is only set on 503 responses"
        );
        let body = response_body_json(resp, 16384).await;
        assert_eq!(body["instance_id"], GATEWAY_GENERATION);
        assert_eq!(body["topics"].as_object().unwrap().len(), 5);
        for (_topic, b64) in body["topics"].as_object().unwrap() {
            let b64 = b64.as_str().expect("topic value is base64 string");
            let bytes = BASE64.decode(b64).expect("valid base64");
            let envelope = DataEnvelope::decode(&bytes[..]).expect("DataEnvelope");
            assert_eq!(envelope.version, 1);
            assert!(envelope.payload.is_some());
        }
    }

    #[tokio::test]
    async fn degraded_phase_is_treated_as_ready() {
        // Degraded means a non-required subsystem failed; the
        // authoritative resources (vault / providers / etc.) are still
        // coherent, so we surface them with the same 200 path.
        let state = test_state_with_snapshot(Some(snapshot_with_phase(
            BootstrapPhase::Degraded,
            "optional fs-watcher failed",
        )))
        .await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = response_body_json(resp, 16384).await;
        assert_eq!(body["instance_id"], GATEWAY_GENERATION);
        assert_eq!(body["topics"].as_object().unwrap().len(), 5);
    }

    /// Empty GatewayState (no BootstrapOrchestrator attached): every
    /// snapshot serialises to a valid `DataEnvelope`, `instance_id` is
    /// the empty string (the orchestrator-not-attached fallback), and
    /// all 5 topics appear in the map.
    #[tokio::test]
    async fn get_global_resources_empty_gateway() {
        let state = test_state().await;
        let resp = get_global_resources(State(state)).await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Each encoded envelope round-trips back to the same oneof
    /// variant it was built from — proves the dispatch table in
    /// `encode_envelope_b64` is wired correctly per topic.
    #[tokio::test]
    async fn envelopes_round_trip_per_topic() {
        let gw = GatewayState::new("/tmp/empty");
        let providers = build_available_providers(&gw);
        let mcps = build_available_mcps(&gw);
        let embedding_models = build_available_embedding_models(&gw);
        let user_profile = build_available_users(&gw);

        let cases = [
            (
                topics::PROVIDERS,
                encode_envelope_b64(&providers, Payload::AvailableProviders),
            ),
            (
                topics::MCPS,
                encode_envelope_b64(&mcps, Payload::AvailableMcps),
            ),
            (
                topics::EMBEDDING_MODELS,
                encode_envelope_b64(&embedding_models, Payload::AvailableEmbeddingModels),
            ),
            (
                topics::USER_PROFILE,
                encode_envelope_b64(&user_profile, Payload::AvailableUsers),
            ),
        ];

        for (topic, b64) in cases {
            let bytes = BASE64.decode(&b64).expect("valid base64");
            let envelope = DataEnvelope::decode(&bytes[..])
                .unwrap_or_else(|e| panic!("decode {topic}: {e}"));
            assert!(matches!(
                envelope.payload,
                Some(Payload::AvailableProviders(_))
                | Some(Payload::AvailableMcps(_))
                | Some(Payload::AvailableEmbeddingModels(_))
                | Some(Payload::AvailableUsers(_))
            ));
        }
    }
}