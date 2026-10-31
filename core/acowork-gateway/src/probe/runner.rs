//! ADR-103 D10.2/D10.3 — the probe transport: run tiers L1→L3 over HTTP.
//!
//! The runner owns three responsibilities only:
//!
//! 1. Send the wire shapes from [`super::wire`] to `{base_url}/responses`.
//! 2. Classify a non-2xx answer with the shared D8 classifier
//!    ([`acowork_core::providers::responses_errors::classify_http_error`]) so a
//!    probe failure and a runtime failure carry the same error class.
//! 3. Report a [`ProbeOutcome`] — a pure data record, mapped to
//!    [`ProtocolProbe`] by [`super::store`].
//!
//! It deliberately knows nothing about Gateway state, vault, caching, or
//! persistence: the handler orchestrates those (D12.1), which keeps every
//! branch here testable against a fake server.
//!
//! **Usage accounting (D10.4).** Probe requests are real billed calls, and the
//! usage they report MUST NOT enter ADR-027/066 budget accounting. The runner
//! never touches a usage channel; it only records the counters in the outcome
//! (for the UI) and emits an audit log line under the `provider_probe_audit`
//! target.

use std::time::{Duration, Instant};

use acowork_core::protocol::ProbeState;
use acowork_core::providers::responses_errors::{TRIMMABLE_FIELDS, classify_http_error};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::probe::wire::{
    PROBE_TOOL_NAME, ProbeFunctionCall, ProbeUsage, assert_envelope, assert_usage,
    baseline_request, error_reason, find_function_call, round_trip_replay_request,
    round_trip_request, truncate,
};

/// Per-request timeout (D10.1). The batch-level budget lives in the
/// handler, because only the handler knows how many models are queued.
///
/// 30 s, not the original 5 s: the probe body is minimal, but a reasoning
/// model can spend well over 5 s on its first token (cold start + thinking
/// block), and a timeout is classified as a transport failure — the verdict
/// says "endpoint broken" when the truth is "endpoint slow". 5 s produced a
/// false `Failed` for exactly that reason (observed: latency_ms=5001,
/// tiers=[]). 30 s is generous for a ≤64-token completion while still
/// bounding a genuinely dead endpoint.
pub const PROBE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Which tiers a probe run should attempt.
///
/// `Standard` is an **internal optimisation, not a user-facing choice**: the
/// save path runs L3 once per `(provider, protocol)` and lets the other changed
/// rows take the cheap tier, inheriting the representative's L3 verdict
/// (D12.3). Because L3 is what makes a verdict positive (D10.2), an explicit
/// user probe is always upgraded to [`ProbeTier::Deep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProbeTier {
    /// L1 + L2 only — existence and response shape.
    Standard,
    /// L1 + L2 + L3 — plus the tool round-trip.
    Deep,
}

impl ProbeTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeTier::Standard => "standard",
            ProbeTier::Deep => "deep",
        }
    }
}

/// Why a probe tier failed. `class` is the D8 error-class name so the UI can
/// distinguish "endpoint does not exist" from "endpoint exists but drifted".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeFailure {
    pub class: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    /// The request field a 400 blamed (`UnsupportedField` only). The D8 trim
    /// path needs it: the error class alone cannot say whether the field is
    /// trimable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_field: Option<String>,
}

impl ProbeFailure {
    fn new(class: &str, message: impl Into<String>) -> Self {
        Self {
            class: class.to_string(),
            message: message.into(),
            http_status: None,
            unsupported_field: None,
        }
    }

    fn with_status(class: &str, message: impl Into<String>, status: u16) -> Self {
        Self {
            class: class.to_string(),
            message: message.into(),
            http_status: Some(status),
            unsupported_field: None,
        }
    }

    /// Is this failure the D8 trigger for `field`? Only then may the probe
    /// re-send the request with that field omitted.
    fn blames_trimmable_field(&self, field: &str) -> bool {
        self.class == "UnsupportedField"
            && self.unsupported_field.as_deref() == Some(field)
            && TRIMMABLE_FIELDS.contains(&field)
    }
}

/// Result of one probe run. Pure data — no timestamps, no provider identity
/// (the caller attaches both when it stores the verdict).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeOutcome {
    /// Every tier this run attempted passed.
    pub success: bool,
    /// Tiers that passed, in order: e.g. `["L1","L2"]`.
    #[serde(default)]
    pub tier_passed: Vec<String>,
    pub latency_ms: u64,
    /// The base URL that actually answered, when it differs from the
    /// configured one (the `/v1` recovery path, D10.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProbeUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ProbeFailure>,
    /// Request fields the probe had to drop to get a 2xx (D8 trim).
    ///
    /// Empty means the endpoint accepted the full probe body. Non-empty is the
    /// evidence for [`ProbeState::Degraded`]: the endpoint works, but only
    /// without a field the Runtime would otherwise send.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_fields: Vec<String>,
}

impl ProbeOutcome {
    /// D10.2/D10.3 verdict mapping — the only place the tier list turns into a
    /// gate input.
    ///
    /// - **L1+L2+L3** → `Verified`.
    /// - **L1+L2+L3 after a D8 trim** → `Degraded` (D10.3: works, but only
    ///   without a field), so the R badge can say so instead of claiming a
    ///   clean verify.
    /// - anything else → `Failed`. L3 (tool round-trip) is the minimum for a
    ///   positive verdict on purpose: "ACowork 的 provider 存在的唯一目的是跑
    ///   agent 工具循环", so a provider that answers the shape but cannot
    ///   complete a tool call would be a green light that fails in production
    ///   (D10.2). L1/L2 results stay on the outcome for diagnosis.
    pub fn state(&self) -> ProbeState {
        let passed = |tier: &str| self.tier_passed.iter().any(|t| t == tier);
        if !passed("L3") {
            ProbeState::Failed
        } else if self.stripped_fields.is_empty() {
            ProbeState::Verified
        } else {
            ProbeState::Degraded
        }
    }

    /// Short human-readable summary for logs and the verdict reason.
    pub fn summary(&self) -> String {
        match &self.error {
            Some(err) => format!("{}: {}", err.class, err.message),
            None => format!(
                "{} passed ({:?})",
                self.tier_passed.join("+"),
                self.state()
            ),
        }
    }
}

/// HTTP executor for protocol probes. Holds one `reqwest::Client` so repeated
/// probes reuse the connection pool (and the TLS setup) instead of paying it
/// per call.
#[derive(Clone)]
pub struct ProbeRunner {
    client: reqwest::Client,
}

impl Default for ProbeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeRunner {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(PROBE_REQUEST_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self { client }
    }

    /// Run the requested tiers against `base_url`.
    ///
    /// `api_key` is `None` for local/key-less providers; the `Authorization`
    /// header is then omitted rather than sent empty.
    pub async fn run(
        &self,
        model: &str,
        base_url: &str,
        api_key: Option<&str>,
        tier: ProbeTier,
    ) -> ProbeOutcome {
        let base = base_url.trim().trim_end_matches('/');
        let outcome = self.attempt(base, model, api_key, tier).await;

        // D10.3: a 404 that names the Responses endpoint usually means the
        // configured base URL is missing its API-version suffix. Retry once
        // with `/v1` appended and report the URL that worked, so the UI can
        // offer it as the fix instead of just failing.
        let endpoint_missing = outcome
            .error
            .as_ref()
            .is_some_and(|e| e.class == "EndpointUnavailable" && e.http_status == Some(404));
        if endpoint_missing && !base.ends_with("/v1") {
            let retry_base = format!("{base}/v1");
            let retry = self.attempt(&retry_base, model, api_key, tier).await;
            if retry.success {
                return ProbeOutcome {
                    observed_base_url: Some(retry_base),
                    ..retry
                };
            }
            // Still failed: report the original attempt, because the user's
            // configured URL is what they must fix.
        }

        outcome
    }

    async fn attempt(
        &self,
        base: &str,
        model: &str,
        api_key: Option<&str>,
        tier: ProbeTier,
    ) -> ProbeOutcome {
        let started = Instant::now();
        let mut outcome = ProbeOutcome {
            success: false,
            tier_passed: Vec::new(),
            latency_ms: 0,
            observed_base_url: None,
            usage: None,
            error: None,
            stripped_fields: Vec::new(),
        };

        // ── L1 + L2: one request, two verdicts ──────────────────────────
        //
        // The baseline body carries a `reasoning` block on purpose: that is
        // the only field D8 may trim, and probing with it is what lets a probe
        // run report "works, but the field had to go" (D10.3) instead of only
        // ever claiming a clean verify.
        //
        // The **value** inside must be one every vendor accepts
        // (`reasoning.effort: low` — see [`crate::probe::wire`]): a 400 that
        // names the field is indistinguishable from "the endpoint does not
        // implement `reasoning`", so an illegal value would be mis-read as an
        // unsupported field and would report a false `degraded`.
        let url = format!("{base}/responses");
        let baseline = match self.post(&url, api_key, baseline_request(model, false)).await {
            Ok(body) => body,
            Err(failure) if failure.blames_trimmable_field("reasoning") => {
                // D8: re-send **once** with the field omitted. No chain and no
                // "retry until 200" — the ADR forbids both.
                tracing::warn!(
                    model,
                    base_url = base,
                    class = %failure.class,
                    "probe hit the D8 trim: provider rejects `reasoning` - retrying the baseline once without it"
                );
                match self.post(&url, api_key, baseline_request(model, true)).await {
                    Ok(body) => {
                        outcome.stripped_fields = vec!["reasoning".to_string()];
                        body
                    }
                    Err(retry_failure) => {
                        outcome.error = Some(retry_failure);
                        outcome.latency_ms = started.elapsed().as_millis() as u64;
                        return outcome;
                    }
                }
            }
            Err(failure) => {
                outcome.error = Some(failure);
                outcome.latency_ms = started.elapsed().as_millis() as u64;
                return outcome;
            }
        };
        // Once a field is known to be unsupported, every later tier uses the
        // body the Runtime would send (i.e. without it).
        let trimmed = !outcome.stripped_fields.is_empty();

        if let Err(msg) = assert_envelope(&baseline) {
            outcome.error = Some(ProbeFailure::new("ShapeMismatch", msg));
            outcome.latency_ms = started.elapsed().as_millis() as u64;
            return outcome;
        }
        outcome.tier_passed.push("L1".to_string());

        match assert_usage(&baseline) {
            Ok(usage) => {
                outcome.tier_passed.push("L2".to_string());
                outcome.usage = Some(usage);
            }
            Err(msg) => {
                outcome.error = Some(ProbeFailure::new("UsageShapeMismatch", msg));
                outcome.latency_ms = started.elapsed().as_millis() as u64;
                return outcome;
            }
        }

        if tier == ProbeTier::Standard {
            outcome.success = true;
            outcome.latency_ms = started.elapsed().as_millis() as u64;
            return outcome;
        }

        // ── L3: tool round-trip ─────────────────────────────────────────
        if let Err(failure) = self.round_trip(base, model, api_key, trimmed).await {
            outcome.error = Some(failure);
            outcome.latency_ms = started.elapsed().as_millis() as u64;
            return outcome;
        }
        // The round-trip verdict is binary; the produced call is not persisted.
        outcome.tier_passed.push("L3".to_string());
        outcome.success = true;
        outcome.latency_ms = started.elapsed().as_millis() as u64;
        outcome
    }

    /// L3: ask for a tool call, then replay it as `function_call_output`.
    ///
    /// Both halves must work. A provider that emits `function_call` but rejects
    /// the matching output cannot run the Runtime's tool loop, which is the
    /// only reason L3 exists.
    async fn round_trip(
        &self,
        base: &str,
        model: &str,
        api_key: Option<&str>,
        trimmed: bool,
    ) -> Result<ProbeFunctionCall, ProbeFailure> {
        let url = format!("{base}/responses");
        let body = self
            .post(&url, api_key, round_trip_request(model, trimmed))
            .await?;
        assert_envelope(&body)
            .map_err(|msg| ProbeFailure::new("ShapeMismatch", msg))?;
        let call = find_function_call(&body).map_err(|msg| {
            ProbeFailure::new(
                "RoundTripFailed",
                format!("{msg} (probe tool: {PROBE_TOOL_NAME})"),
            )
        })?;

        let replay = self
            .post(&url, api_key, round_trip_replay_request(model, &call, trimmed))
            .await?;
        assert_envelope(&replay).map_err(|msg| {
            ProbeFailure::new("RoundTripFailed", format!("replay response: {msg}"))
        })?;
        Ok(call)
    }

    /// POST one request and return the parsed JSON body.
    ///
    /// Non-2xx answers are classified through the shared D8 classifier so the
    /// probe reports `EndpointUnavailable` / `UnsupportedField` /
    /// `ContentIntegrity` exactly like the Runtime would.
    async fn post(
        &self,
        url: &str,
        api_key: Option<&str>,
        body: Value,
    ) -> Result<Value, ProbeFailure> {
        let mut request = self.client.post(url).json(&body);
        if let Some(key) = api_key.filter(|k| !k.is_empty()) {
            request = request.bearer_auth(key);
        }

        let response = request.send().await.map_err(|e| {
            if e.is_timeout() {
                // reqwest's raw timeout message ("error sending request for
                // url …") reads like "endpoint unreachable"; say the truth.
                let secs = PROBE_REQUEST_TIMEOUT.as_secs();
                ProbeFailure::new(
                    "TransportTimeout",
                    truncate(
                        &format!("probe request timed out after {secs}s: {e}"),
                        300,
                    ),
                )
            } else {
                ProbeFailure::new("TransportError", truncate(&e.to_string(), 300))
            }
        })?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            // `classify_http_error` is the single D8 http-status arbiter.
            let classified = classify_http_error(status.as_u16(), &text);
            let mut failure = ProbeFailure::with_status(
                &format!("{:?}", classified.error_type),
                error_reason(&text),
                status.as_u16(),
            );
            failure.unsupported_field = classified.unsupported_field().map(str::to_string);
            return Err(failure);
        }

        serde_json::from_str::<Value>(&text).map_err(|e| {
            ProbeFailure::with_status(
                "InvalidJson",
                format!("response is not JSON ({}): {}", e, error_reason(&text)),
                status.as_u16(),
            )
        })
    }
}

/// Emit the D10.4 audit trail for one probe run.
///
/// Kept separate from the return value so the log can carry the provider and
/// model identity the runner does not know.
pub fn log_probe_audit(provider: &str, model: &str, outcome: &ProbeOutcome) {
    tracing::info!(
        target: "provider_probe_audit",
        provider = %provider,
        model = %model,
        state = ?outcome.state(),
        tiers = ?outcome.tier_passed,
        latency_ms = outcome.latency_ms,
        // D8: an empty list is the normal case; a non-empty one is the only
        // evidence that a verdict was degraded rather than clean.
        stripped_fields = ?outcome.stripped_fields,
        input_tokens = outcome.usage.map(|u| u.input_tokens),
        output_tokens = outcome.usage.map(|u| u.output_tokens),
        "protocol probe finished (usage is NOT counted against ADR-027/066 budgets)"
    );
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::wire::PROBE_TOOL_NAME;
    use mockito::Matcher;
    use serde_json::json;

    /// A minimal body the L1/L2 tiers accept.
    fn baseline_body() -> serde_json::Value {
        json!({
            "id": "resp_1",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "pong"}],
            }],
            "usage": {"input_tokens": 7, "output_tokens": 1},
        })
    }

    fn round_trip_body() -> serde_json::Value {
        json!({
            "id": "resp_2",
            "object": "response",
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_probe",
                "name": PROBE_TOOL_NAME,
                "arguments": "{\"value\": 1}",
            }],
            "usage": {"input_tokens": 30, "output_tokens": 12},
        })
    }

    /// A fully working Responses provider: baseline, round-trip and replay.
    ///
    /// The three requests are told apart by content, not by order: the
    /// baseline sends `input: "ping"`, the round-trip asks for a tool call, the
    /// replay echoes `function_call_output`.
    fn mount_healthy_provider(server: &mut mockito::ServerGuard) {
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

    #[tokio::test]
    async fn deep_probe_on_healthy_provider_is_verified() {
        let mut server = mockito::Server::new_async().await;
        mount_healthy_provider(&mut server);

        let outcome = ProbeRunner::new()
            .run("gpt-5", &server.url(), Some("sk-test"), ProbeTier::Deep)
            .await;

        assert!(outcome.success, "{:?}", outcome.error);
        assert_eq!(outcome.tier_passed, vec!["L1", "L2", "L3"]);
        assert_eq!(outcome.state(), ProbeState::Verified);
        assert_eq!(
            outcome.usage.map(|u| (u.input_tokens, u.output_tokens)),
            Some((7, 1)),
            "L2 usage comes from the baseline call"
        );
        assert!(outcome.observed_base_url.is_none(), "no URL change needed");
    }

    #[tokio::test]
    async fn standard_probe_skips_the_round_trip() {
        let mut server = mockito::Server::new_async().await;
        mount_healthy_provider(&mut server);
        // Guard: L3 must not run on the standard tier.
        let replay = server
            .mock("POST", "/responses")
            .match_body(Matcher::Regex("function_call_output".to_string()))
            .expect(0)
            .create();

        let outcome = ProbeRunner::new()
            .run("gpt-5", &server.url(), None, ProbeTier::Standard)
            .await;

        // L1/L2 pass, but without L3 there is no positive verdict (D10.2) —
        // which is exactly why explicit probes always run deep.
        assert!(outcome.success);
        assert_eq!(outcome.tier_passed, vec!["L1", "L2"]);
        assert_eq!(outcome.state(), ProbeState::Failed);
        replay.assert();
    }

    #[tokio::test]
    async fn missing_function_call_fails_the_verdict() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/responses")
            .match_body(Matcher::PartialJson(json!({"input": "ping"})))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(baseline_body().to_string())
            .create();
        // The model answers in text instead of calling the no-op tool.
        server
            .mock("POST", "/responses")
            .match_body(Matcher::PartialJson(json!({"tool_choice": "auto"})))
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "object": "response",
                    "status": "completed",
                    "output": [{"type": "message", "role": "assistant", "content": []}],
                    "usage": {"input_tokens": 30, "output_tokens": 4},
                })
                .to_string(),
            )
            .create();

        let outcome = ProbeRunner::new()
            .run("gpt-5", &server.url(), None, ProbeTier::Deep)
            .await;

        assert!(!outcome.success);
        assert_eq!(outcome.tier_passed, vec!["L1", "L2"]);
        assert_eq!(
            outcome.state(),
            ProbeState::Failed,
            "a provider that cannot complete a tool call must not be enabled (D10.2)"
        );
        let err = outcome.error.expect("L3 failure must be recorded");
        assert_eq!(err.class, "RoundTripFailed");
        assert!(err.message.contains("no function_call item"), "{err:?}");
    }

    #[tokio::test]
    async fn usage_shape_mismatch_fails_the_shape_tier() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/responses")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                json!({
                    "object": "response",
                    "status": "completed",
                    // Chat-Completions usage names — exactly the drift L2 exists
                    // to catch.
                    "usage": {"prompt_tokens": 7, "completion_tokens": 1},
                })
                .to_string(),
            )
            .create();

        let outcome = ProbeRunner::new()
            .run("gpt-5", &server.url(), None, ProbeTier::Standard)
            .await;

        assert!(!outcome.success);
        assert_eq!(outcome.tier_passed, vec!["L1"]);
        assert_eq!(outcome.state(), ProbeState::Failed);
        assert_eq!(outcome.error.unwrap().class, "UsageShapeMismatch");
    }

    #[tokio::test]
    async fn chat_completions_endpoint_is_classified_not_probed_as_responses() {
        let mut server = mockito::Server::new_async().await;
        // A Chat-Completions-only host: `/v1/responses` is 404 "unknown endpoint".
        server
            .mock("POST", "/v1/responses")
            .with_status(404)
            .with_body(
                json!({"error": {"message": "Unknown endpoint: /v1/responses", "type": "invalid_request_error"}})
                .to_string(),
            )
            .create();

        let outcome = ProbeRunner::new()
            .run(
                "gpt-5",
                &format!("{}/v1", server.url()),
                None,
                ProbeTier::Standard,
            )
            .await;

        assert!(!outcome.success);
        assert!(outcome.tier_passed.is_empty());
        assert_eq!(outcome.state(), ProbeState::Failed);
        let err = outcome.error.unwrap();
        assert_eq!(err.class, "EndpointUnavailable");
        assert_eq!(err.http_status, Some(404));
    }

    #[tokio::test]
    async fn missing_v1_suffix_is_recovered_and_reported() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/responses")
            .with_status(404)
            .with_body(json!({"error": {"message": "unknown endpoint"}}).to_string())
            .create();
        server
            .mock("POST", "/v1/responses")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(baseline_body().to_string())
            .create();

        let base = server.url();
        let expected = format!("{base}/v1");
        let outcome = ProbeRunner::new()
            .run("gpt-5", &base, None, ProbeTier::Standard)
            .await;

        assert!(outcome.success, "{:?}", outcome.error);
        assert_eq!(
            outcome.observed_base_url.as_deref(),
            Some(expected.as_str()),
            "D10.3: the working URL is reported, never written into the config"
        );
    }

    #[tokio::test]
    async fn non_json_success_body_is_reported_as_invalid_json() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("POST", "/responses")
            .with_status(200)
            .with_body("<html>proxy error</html>")
            .create();

        let outcome = ProbeRunner::new()
            .run("gpt-5", &server.url(), None, ProbeTier::Standard)
            .await;

        assert_eq!(outcome.state(), ProbeState::Failed);
        assert_eq!(outcome.error.unwrap().class, "InvalidJson");
    }

    #[tokio::test]
    async fn closed_port_is_a_transport_error() {
        // Bind then drop: the port is closed — no server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let outcome = ProbeRunner::new()
            .run("gpt-5", &format!("http://{addr}"), None, ProbeTier::Standard)
            .await;

        assert!(!outcome.success);
        assert_eq!(outcome.state(), ProbeState::Failed);
        let err = outcome.error.unwrap();
        assert!(
            err.class == "TransportError" || err.class == "TransportTimeout",
            "unexpected class: {}",
            err.class
        );
    }

// ── D8/S7: the trim path and the `Degraded` verdict ────────────────

    /// MiniMax M3.1-Flash wording: 400 that blames the `reasoning` parameter.
    const REASONING_400: &str = r#"{"error":{"code":"invalid_parameter","message":"invalid parameter: `reasoning`","param":"reasoning","type":"invalid_request_error"}}"#;

    /// A provider that rejects `reasoning` but works without it: the untrimmed
    /// baseline is matched by including the block in the matcher, so the
    /// trimmed retry can never hit the 400 mock by accident.
    fn mount_reasoning_intolerant_provider(server: &mut mockito::ServerGuard) {
        server
            .mock("POST", "/responses")
            .match_body(Matcher::PartialJson(json!({
                "input": "ping",
                "reasoning": {"effort": "low"},
            })))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(REASONING_400)
            .expect(1)
            .create();
        server
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
    }

    #[tokio::test]
    async fn a_rejected_reasoning_field_makes_the_verdict_degraded() {
        let mut server = mockito::Server::new_async().await;
        mount_reasoning_intolerant_provider(&mut server);

        let outcome = ProbeRunner::new()
            .run("m3.1-flash", &server.url(), Some("sk-test"), ProbeTier::Deep)
            .await;

        // The probe proves the endpoint works — after dropping one field.
        assert!(outcome.success, "{:?}", outcome.error);
        assert_eq!(outcome.tier_passed, vec!["L1", "L2", "L3"]);
        assert_eq!(outcome.stripped_fields, vec!["reasoning".to_string()]);
        assert_eq!(
            outcome.state(),
            ProbeState::Degraded,
            "works, but only without `reasoning` — D10.3 calls that degraded"
        );
    }

    /// A provider that rejects the *value* `effort: none` but supports the
    /// `reasoning` field — MiniMax M3.1-Flash, per MiniMax's own docs.
    ///
    /// This is the regression test for a false `degraded`: the probe used to
    /// send `effort: none` as its baseline, MiniMax answered 400 naming the
    /// `reasoning` field, and the classifier read "this value is illegal" as
    /// "this field is not implemented" → trim → `degraded` +
    /// `stripped_fields: ["reasoning"]`. The user was told a fully
    /// reasoning-capable model had its reasoning dropped.
    ///
    /// The correct outcome is `verified`, with the field intact.
    #[tokio::test]
    async fn a_provider_rejecting_only_the_none_value_still_verifies() {
        let mut server = mockito::Server::new_async().await;

        // `effort: none` is the value MiniMax rejects — and the probe must
        // never send it, so this mock must stay untouched.
        server
            .mock("POST", "/responses")
            .match_body(Matcher::PartialJson(json!({
                "reasoning": {"effort": "none"},
            })))
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(REASONING_400)
            .expect(0)
            .create();

        // The probe's actual baseline (`effort: low`) is accepted outright.
        server
            .mock("POST", "/responses")
            .match_body(Matcher::PartialJson(json!({
                "input": "ping",
                "reasoning": {"effort": "low"},
            })))
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

        let outcome = ProbeRunner::new()
            .run("m3.1-flash", &server.url(), Some("sk-test"), ProbeTier::Deep)
            .await;

        assert!(outcome.success, "{:?}", outcome.error);
        assert_eq!(outcome.tier_passed, vec!["L1", "L2", "L3"]);
        assert!(
            outcome.stripped_fields.is_empty(),
            "a rejected value is not a rejected field — nothing may be stripped"
        );
        assert_eq!(outcome.state(), ProbeState::Verified);
    }

    #[tokio::test]
    async fn a_trim_that_does_not_help_stays_failed() {
        let mut server = mockito::Server::new_async().await;
        // Every baseline 400s, trimmed or not: one retry, no chain.
        server
            .mock("POST", "/responses")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(REASONING_400)
            .expect(2)
            .create();

        let outcome = ProbeRunner::new()
            .run("m3.1-flash", &server.url(), None, ProbeTier::Standard)
            .await;

        assert!(!outcome.success);
        assert!(outcome.stripped_fields.is_empty());
        assert_eq!(outcome.state(), ProbeState::Failed);
        assert_eq!(outcome.error.unwrap().class, "UnsupportedField");
    }

    #[tokio::test]
    async fn a_non_whitelisted_field_is_reported_not_trimmed() {
        let mut server = mockito::Server::new_async().await;
        // `temperature` is not in the probe body, so the probe's trim path
        // (which only knows `reasoning`) cannot apply: exactly one request.
        // A 400 naming it is surfaced verbatim for the user to act on.
        server
            .mock("POST", "/responses")
            .with_status(400)
            .with_header("content-type", "application/json")
            .with_body(r#"{"error":{"message":"unsupported parameter: temperature"}}"#)
            .expect(1)
            .create();

        let outcome = ProbeRunner::new()
            .run("gpt-5", &server.url(), None, ProbeTier::Standard)
            .await;

        assert_eq!(outcome.state(), ProbeState::Failed);
        assert!(outcome.stripped_fields.is_empty());
        let err = outcome.error.unwrap();
        assert_eq!(err.class, "UnsupportedField");
        assert_eq!(
            err.unsupported_field.as_deref(),
            Some("temperature"),
            "the blamed field must survive into the verdict"
        );
    }

    #[tokio::test]
    async fn verified_and_degraded_both_gate_the_selection_but_stay_distinct() {
        // D1: both pass the gate. D10/D11: only `Degraded` carries the reason.
        let mut server = mockito::Server::new_async().await;
        mount_healthy_provider(&mut server);
        let clean = ProbeRunner::new()
            .run("gpt-5", &server.url(), None, ProbeTier::Deep)
            .await;
        assert!(clean.stripped_fields.is_empty());
        assert_eq!(clean.state(), ProbeState::Verified);
        assert!(clean.state().gates_responses());

        let mut tolerant = mockito::Server::new_async().await;
        mount_reasoning_intolerant_provider(&mut tolerant);
        let degraded = ProbeRunner::new()
            .run("m3.1-flash", &tolerant.url(), None, ProbeTier::Deep)
            .await;
        assert_eq!(degraded.state(), ProbeState::Degraded);
        assert!(degraded.state().gates_responses());
        assert_ne!(
            degraded.state(),
            clean.state(),
            "the R badge needs the two states to be distinguishable"
        );
        assert!(
            degraded.summary().contains("Degraded"),
            "{}",
            degraded.summary()
        );
    }
}
