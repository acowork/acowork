//! Active pull of global resources from the Gateway HTTP API.
//!
//! After Phase A (mqtt client + available_cache ready), the Runtime
//! **actively** calls `GET /api/global-resources` and feeds the response
//! into [`AvailableResourceCache::update_from_mqtt`] — the exact same
//! handler the MQTT retained path uses. This is the fix for **Bug B**
//! (cleared `.acowork` + onboarding → first chat fails with "unexpected
//! error").
//!
//! ## Why this is needed (Bug B)
//!
//! Bug B: after wiping `.acowork` and re-running onboarding, the first
//! chat with the system agent failed with "unexpected error". Root cause:
//! the Gateway published an empty `AvailableProviders` snapshot
//! (`provider_count=0, api_key_lengths=[]`) immediately after the vault
//! was unlocked but before any provider had been added; the system agent
//! booted ~150 ms later and cached that empty retained. When the Desktop
//! finally added the provider ~19 s later, the new retained snapshot was
//! NOT delivered to the already-subscribed Runtime (rumqttd only delivers
//! a retained message to subscribers that have not yet seen the prior
//! value for that topic), so the session's view of `providers` stayed
//! empty until a manual `model_switch` re-broke the staleness check.
//!
//! ## Protocol split (ADR-033 + this module)
//!
//! - **MQTT retained** is the **primary** delivery channel: it pushes
//!   resource updates to *every* connected Runtime in real time without
//!   any explicit pull.
//! - **`GET /api/global-resources`** is the **active pull** channel:
//!   once-per-startup, deterministic, not subject to retained-delivery
//!   races. It is the same Gateway state the retained publisher would
//!   emit, exposed via HTTP so a fresh Runtime can request the *latest*
//!   snapshot regardless of what it has already cached.
//!
//! Both channels share the same `update_from_mqtt` handler, so version
//! checks / stale-retained rejection / ADR-059 generation switch logic
//! are reused without a parallel update pipeline.
//!
//! ## Retry semantics (Bug B fix v3)
//!
//! The Gateway may be in `Booting`/`Failed` when the Runtime starts (see
//! `acowork-gateway/src/http/global_resources_api.rs` for the full phase
//! semantics). The previous "best-effort, single shot" implementation
//! silently failed when the Gateway returned `503` and the session
//! booted with a half-coherent `AvailableResourceCache`.
//!
//! This module now drives a retry loop with the following guarantees:
//!
//! 1. **Parses Gateway phase semantics.** A `503` response is decoded as
//!    [`PullOutcome::NotReady`] carrying the Gateway's `Retry-After`
//!    hint (from the standard header or the JSON body's
//!    `retry_after_seconds`). The Runtime sleeps for exactly that long
//!    before the next attempt — no busy-loop.
//! 2. **Respects the abort sentinel.** `Retry-After: -1` (or
//!    `retry_after_seconds: -1`, used for `BootstrapPhase::ShuttingDown`)
//!    means "do not retry, abort the pull" — the Runtime logs an error
//!    and gives up; the MQTT retained path remains the fallback.
//! 3. **Never poisons the cache.** A `503` returns NOTHING — the pull
//!    function exits the per-attempt path BEFORE acquiring the cache
//!    write lock, so an existing coherent snapshot (delivered via MQTT
//!    retained after the active pull started) is never overwritten with
//!    "I'm not ready" data. This is the critical correctness property:
//!    the previous behaviour silently cleared half the cache when the
//!    Gateway returned an empty `topics` map while still booting.
//! 4. **Honours a total deadline.** The loop terminates after
//!    [`PULL_MAX_DURATION`] wall-clock seconds. Subsequent Phase A work
//!    can proceed even on pull failure — the Runtime still serves from
//!    whatever the MQTT retained path has delivered so far.
//! 5. **Backs off on transient errors.** Connection refused, request
//!    timeouts, and `5xx` non-`503 responses use exponential backoff
//!    capped at [`PULL_BACKOFF_MAX`]. The first attempt is immediate.
//! 6. **Presents the node credential (ADR-076).** `/api/global-resources`
//!    sits behind the same `auth_middleware` gate as everything else, so
//!    (accounts are always on, ADR-114) an unauthenticated pull gets a `401` —
//!    which is [`PullOutcome::Fatal`], i.e. the pull **aborts on the very
//!    first attempt** and the Runtime silently degrades to MQTT-retained
//!    only. The node token the Node injected at spawn is attached as
//!    [`acowork_core::auth::NODE_TOKEN_HEADER`], for the same reason the
//!    MCP path carries it: without it this call is dead weight in every
//!    `multi_user` deployment.
//!
//! Order of operations on a successful `200`:
//! 1. Detect ADR-059 §5.3 **generation switch** (compare `instance_id`):
//!    if the remote Gateway generation differs from what we have cached
//!    locally, clear every old-generation resource snapshot **before**
//!    applying the new ones. `update_from_mqtt`'s own switch logic for
//!    `bootstrap_state` would also clear non-bootstrap fields via the
//!    bootstrap's generation switch, but doing it explicitly here keeps
//!    the critical section tight and the log trace clear.
//! 2. Decode each `topics[k]` from base64 and call
//!    `cache.update_from_mqtt(topic, &bytes)` — identical to the MQTT
//!    retained path. Version checks and stale-retained rejection apply
//!    uniformly.

use std::time::{Duration, Instant};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use reqwest::StatusCode;
use serde::Deserialize;
use tracing::{debug, error, info, warn};

use acowork_core::defaults::gateway_http_port;
use acowork_core::mqtt_proto::DataEnvelope;

use crate::config::RuntimeConfig;
use crate::mqtt::SharedAvailableCache;

/// Total wall-clock budget for the retry loop. After this elapses we
/// give up regardless of how many attempts remain. Tuned to comfortably
/// outlive the typical cold-start vault-unlock + provider-onboarding
/// window (well under 30 s even on a slow disk) while keeping a stuck
/// Gateway from blocking Phase A forever.
pub(crate) const PULL_MAX_DURATION: Duration = Duration::from_secs(30);

/// Base backoff for transient errors (connection refused, 5xx other than
/// 503). Grows linearly — we don't need exponential, Gateway starts are
/// not adversarial.
pub(crate) const PULL_BACKOFF_BASE: Duration = Duration::from_millis(500);

/// Cap for the transient-error backoff so a long-running incident does
/// not push the loop into multi-second sleeps.
pub(crate) const PULL_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Sentinel value the Gateway emits for "do not retry, abort the pull"
/// (used for `BootstrapPhase::ShuttingDown`). The pull loop aborts on
/// seeing this value in either the `Retry-After` header or the JSON
/// body's `retry_after_seconds` field.
pub(crate) const RETRY_AFTER_DONT_RETRY: i64 = -1;

/// Minimal projection of the Gateway's `NotReadyView` (defined in
/// `acowork-gateway/src/http/global_resources_api.rs`). Defined here
/// rather than imported so this crate does not gain a reverse
/// dependency on `acowork-gateway`. The Runtime only needs
/// `retry_after_seconds`; the remaining fields are surfaced via
/// `debug!` logs when present.
#[derive(Debug, Deserialize)]
struct NotReadyView {
    #[serde(default)]
    #[allow(dead_code)]
    instance_id: String,
    #[serde(default)]
    #[allow(dead_code)]
    phase: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    phase_detail: Option<String>,
    retry_after_seconds: i64,
    #[serde(default)]
    #[allow(dead_code)]
    error: Option<String>,
}

/// Per-attempt outcome of `try_pull_once`. The retry loop maps each
/// variant to the appropriate backoff / abort decision.
#[derive(Debug)]
enum PullOutcome {
    /// `200 OK`, payload applied to the cache.
    Applied,
    /// `503 Service Unavailable` with the Gateway's `Retry-After`
    /// hint. `-1` means abort the pull entirely. Positive values mean
    /// sleep at least that many seconds before retrying.
    NotReady(i64),
    /// Network / 5xx-other / unknown status — backoff and retry until
    /// the deadline.
    Transient(String),
    /// 4xx other than 503, JSON parse failure, or `update_from_mqtt`
    /// rejection — these are NOT expected to resolve on retry, so
    /// abort immediately.
    Fatal(String),
}

/// Pull global resources from `GET /api/global-resources`, retrying on
/// `503` (with the Gateway's `Retry-After`) and transient errors, until
/// success or [`PULL_MAX_DURATION`] elapses.
///
/// Returns `true` on a successful apply and `false` on
/// abort / deadline / fatal. Never panics. Never poisons the cache on
/// failure — see module docs §"Never poisons the cache" for the full
/// invariant.
pub(crate) async fn pull_global_resources_from_gateway(
    config: &RuntimeConfig,
    cache: &SharedAvailableCache,
) -> bool {
    let host = config.gateway_host.as_deref().unwrap_or("127.0.0.1");
    let url = format!("http://{}:{}/api/global-resources", host, gateway_http_port());

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            // Building a client only fails on broken global state (no
            // allocator, etc.); treat as fatal rather than retrying.
            error!(
                url = %url,
                error = %e,
                "Cannot build HTTP client for /api/global-resources pull — aborting"
            );
            return false;
        }
    };

    let start = Instant::now();
    let mut attempt: u32 = 0;

    loop {
        if start.elapsed() >= PULL_MAX_DURATION {
            warn!(
                url = %url,
                elapsed_ms = start.elapsed().as_millis() as u64,
                attempt,
                "Global resources pull exceeded PULL_MAX_DURATION — aborting. \
                 Session will boot with whatever the MQTT retained path has delivered."
            );
            return false;
        }

        attempt += 1;
        match try_pull_once(&client, &url, cache, config.mqtt_password.as_deref()).await {
            PullOutcome::Applied => {
                info!(
                    url = %url,
                    attempt,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "Active pull of /api/global-resources succeeded"
                );
                return true;
            }
            PullOutcome::NotReady(retry_after) => {
                if retry_after == RETRY_AFTER_DONT_RETRY {
                    error!(
                        url = %url,
                        "Gateway returned Retry-After: -1 (SHUTTING_DOWN) — aborting active pull. \
                         Runtime will rely on whatever MQTT retained messages have arrived."
                    );
                    return false;
                }
                // Clamp to at least 1s so we never busy-loop, but honour
                // a long Gateway-suggested backoff (e.g. Failed → 10s).
                let sleep_for = Duration::from_secs(retry_after.clamp(1, 60) as u64);
                warn!(
                    url = %url,
                    attempt,
                    retry_after_seconds = retry_after,
                    sleep_ms = sleep_for.as_millis() as u64,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "Gateway reports not-ready; honouring Retry-After before next attempt"
                );
                tokio::time::sleep(sleep_for).await;
            }
            PullOutcome::Transient(msg) => {
                let backoff = backoff_for_attempt(attempt);
                warn!(
                    url = %url,
                    attempt,
                    error = %msg,
                    backoff_ms = backoff.as_millis() as u64,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "Transient error during active pull; backing off"
                );
                tokio::time::sleep(backoff).await;
            }
            PullOutcome::Fatal(msg) => {
                error!(
                    url = %url,
                    attempt,
                    error = %msg,
                    "Fatal error during active pull; aborting. \
                     Runtime will rely on whatever MQTT retained messages have arrived."
                );
                return false;
            }
        }
    }
}

/// Compute the backoff for `attempt` (1-based). Linear growth clamped
/// to [`PULL_BACKOFF_MAX`] so a long outage does not produce
/// multi-minute sleeps.
fn backoff_for_attempt(attempt: u32) -> Duration {
    let raw = PULL_BACKOFF_BASE.saturating_mul(attempt);
    if raw > PULL_BACKOFF_MAX {
        PULL_BACKOFF_MAX
    } else {
        raw
    }
}

/// Perform one HTTP round-trip and, on `200 OK`, apply the snapshot to
/// the cache. On `503` it returns immediately — the cache is NOT
/// mutated (this is the critical "never poison" guarantee). All other
/// failure modes are mapped to [`PullOutcome::Transient`] or
/// [`PullOutcome::Fatal`] depending on whether retry is plausible.
async fn try_pull_once(
    client: &reqwest::Client,
    url: &str,
    cache: &SharedAvailableCache,
    node_token: Option<&str>,
) -> PullOutcome {
    let mut req = client.get(url);
    // ADR-076: `multi_user` gates this route like any other, and a 401 is
    // Fatal (abort) rather than retryable — so the credential is attached
    // here or the whole pull is skipped. Omitted in standalone mode, where
    // no Gateway credential exists.
    if let Some(token) = node_token.filter(|t| !t.is_empty()) {
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            // Connection refused / DNS / timeout — Gateway is either
            // not up yet or having a transient network blip. Retry.
            return PullOutcome::Transient(format!("send: {e}"));
        }
    };

    let status = resp.status();

    if status == StatusCode::SERVICE_UNAVAILABLE {
        // Parse the Gateway's Retry-After. Prefer the standard HTTP
        // header (RFC 7231 §7.1.3 delta-seconds form), fall back to
        // the JSON body field which is what every body we emit carries.
        let header_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);

        let body_secs = match resp.json::<NotReadyView>().await {
            Ok(view) => view.retry_after_seconds,
            Err(_) => 0,
        };

        // Use the LONGER of the two values — a misbehaving Gateway
        // that set a tight header but a longer body field would
        // otherwise cause us to retry before the Gateway was actually
        // ready. Both 0 means "no hint" — fall back to the Booting
        // default (2s) so we always make forward progress.
        let retry_after = match (header_secs, body_secs) {
            (0, 0) => 2,
            (h, 0) => h,
            (0, b) => b,
            (h, b) => h.max(b),
        };

        return PullOutcome::NotReady(retry_after);
    }

    // 2xx other than 200 are treated as success (the Gateway does not
    // emit them today, but be defensive).
    if status.is_success() {
        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return PullOutcome::Fatal(format!("parse JSON body: {e}")),
        };
        if let Err(e) = apply_pull_body(cache, body).await {
            return PullOutcome::Fatal(format!("apply: {e}"));
        }
        return PullOutcome::Applied;
    }

    if status.is_server_error() {
        // 5xx other than 503 — transient infrastructure problem on
        // the Gateway side; back off and retry.
        return PullOutcome::Transient(format!("server error {status}"));
    }

    // 4xx other than 503 — these are not expected and will not fix
    // themselves by retrying (e.g. 401 if the Gateway ever adopts
    // auth). Surface and abort.
    PullOutcome::Fatal(format!("unexpected status {status}"))
}

/// Apply a successfully-pulled `200` body to the cache. Holds the
/// cache write lock for the entire decode loop so generation-switch
/// logic and per-topic updates commit as one atomic write.
///
/// `async` (not `blocking_write`) because this function is invoked
/// from an async task — `tokio::sync::RwLock::blocking_write` panics
/// with "Cannot block the current thread from within a runtime" when
/// called inside any runtime thread, multi-threaded or not.
async fn apply_pull_body(
    cache: &SharedAvailableCache,
    body: serde_json::Value,
) -> Result<(), String> {
    let remote_instance_id = body["instance_id"].as_str().unwrap_or("").to_string();
    let topics = body["topics"]
        .as_object()
        .ok_or_else(|| "`topics` field missing or not an object".to_string())?;

    // Hold the cache write lock for the entire apply so generation
    // switch + topic decode + update_from_mqtt commit atomically.
    // `update_from_mqtt` may take an internal short-lived read lock,
    // but the surrounding RwLock here is what guarantees no other
    // writer (the MQTT retained path) interleaves between our switch
    // decision and the new payload application.
    let mut cache_guard = cache.write().await;

    // ADR-059 §5.3: pre-emptive generation switch.
    let local_instance = cache_guard
        .bootstrap_instance_id()
        .unwrap_or("")
        .to_string();
    if !remote_instance_id.is_empty()
        && !local_instance.is_empty()
        && remote_instance_id != local_instance
    {
        info!(
            old = %local_instance,
            new = %remote_instance_id,
            "Generation switch detected during active pull; clearing old resource snapshots"
        );
        cache_guard.providers = None;
        cache_guard.mcps = None;
        cache_guard.embedding_models = None;
        cache_guard.lsps = None;
        cache_guard.user_profile = None;
        // `bootstrap` will be overwritten by the bootstrap_state
        // payload via `update_from_mqtt` below, which also clears
        // every resource snapshot under a bootstrap-driven switch.
        // We therefore also reset `bootstrap` here so the bootstrap's
        // own switch logic can fire on the first `update_from_mqtt`
        // call below without being confused by a stale snapshot.
        cache_guard.bootstrap = None;
    }

    let mut applied = 0usize;
    let mut skipped = 0usize;
    for (topic, value) in topics {
        let b64 = match value.as_str() {
            Some(s) => s,
            None => {
                warn!(topic = %topic, "topic value is not a string; skipping");
                skipped += 1;
                continue;
            }
        };
        let bytes = match BASE64.decode(b64) {
            Ok(b) => b,
            Err(e) => {
                warn!(topic = %topic, error = %e, "base64 decode failed; skipping");
                skipped += 1;
                continue;
            }
        };
        // Sanity-check that the decoded bytes form a valid DataEnvelope
        // before feeding them into `update_from_mqtt`. The handler will
        // log a decode warning on malformed input, but checking here
        // keeps the pull's own log trace crisp (one log line per
        // failure mode).
        let decode_result: Result<DataEnvelope, prost::DecodeError> =
            prost::Message::decode(&bytes[..]);
        if decode_result.is_err() {
            warn!(
                topic = %topic,
                "decoded bytes do not parse as DataEnvelope; skipping"
            );
            skipped += 1;
            continue;
        }
        cache_guard.update_from_mqtt(topic, &bytes);
        applied += 1;
    }

    info!(
        instance_id = %remote_instance_id,
        applied,
        skipped,
        "Applied /api/global-resources snapshot into AvailableResourceCache"
    );
    // Drop the write lock explicitly so any caller (the retry loop)
    // can observe the updated cache before the next attempt.
    drop(cache_guard);
    let _ = remote_instance_id; // suppress unused warning if applied=0
    debug!("apply_pull_body complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::new_shared_cache;
    use acowork_core::mqtt_proto::{BootstrapState, LlmProtocol, ProviderRef};
    use mockito::Server;

    #[test]
    fn backoff_grows_then_caps() {
        assert_eq!(backoff_for_attempt(1), Duration::from_millis(500));
        assert_eq!(backoff_for_attempt(2), Duration::from_secs(1));
        assert_eq!(backoff_for_attempt(8), Duration::from_secs(4));
        // 10 attempts * 500ms = 5s — still under cap.
        assert_eq!(backoff_for_attempt(10), Duration::from_secs(5));
        // Anything beyond the cap stays at the cap.
        assert_eq!(backoff_for_attempt(20), PULL_BACKOFF_MAX);
        assert_eq!(backoff_for_attempt(1000), PULL_BACKOFF_MAX);
    }

    /// Sentinel value is exposed as a constant so the rest of the
    /// Runtime can match against it (e.g. when surfacing the
    /// "Gateway shutting down" error path through the HTTP layer in
    /// future).
    #[test]
    fn retry_after_dont_retry_is_negative_one() {
        assert_eq!(RETRY_AFTER_DONT_RETRY, -1);
    }

    // ── Bug B fix v3: HTTP pull semantics (mockito) ─────────────────
    //
    // These lock in the §4.13 contract: a 503 must never touch the
    // cache ("never poison"), the Retry-After hint (header vs body, max
    // of both) drives the sleep, and only a 200 applies the snapshot.

    /// Encode an AvailableProviders envelope and base64 it the way the
    /// Gateway's `GlobalResourcesView.topics` does.
    fn providers_topic_payload(version: u64) -> String {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::AvailableProviders(
                    acowork_core::mqtt_proto::AvailableProviders {
                        version,
                        #[allow(deprecated)]
                        default_compact_model: None,
                        default_compact_models: vec![],
                        providers: vec![ProviderRef {
                            id: "openai".to_string(),
                            base_url: "https://api.openai.com/v1".to_string(),
                            protocol_type: LlmProtocol::Openai as i32,
                            models: vec![],
                            compact_model: String::new(),
                            custom: false,
                            api_key: "sk-test".to_string(),
                            account_id: String::new(),
                        }],
                    },
                ),
            ),
        };
        BASE64.encode(prost::Message::encode_to_vec(&envelope))
    }

    /// Encode a BootstrapState envelope (ADR-059 §5.3 generation
    /// snapshot) and base64 it.
    fn bootstrap_topic_payload(instance_id: &str, version: u64, phase: i32) -> String {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(
                acowork_core::mqtt_proto::data_envelope::Payload::BootstrapState(BootstrapState {
                    protocol_version: 1,
                    instance_id: instance_id.to_string(),
                    version,
                    phase,
                    phase_detail: "test".to_string(),
                    issued_at_ms: 0,
                }),
            ),
        };
        BASE64.encode(prost::Message::encode_to_vec(&envelope))
    }

    #[tokio::test]
    async fn try_pull_once_503_does_not_poison_cache() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(503)
            .with_header("retry-after", "2")
            .with_body(
                r#"{"instance_id":"gen-A","phase":"Booting",
                   "phase_detail":"vault unlocking",
                   "retry_after_seconds":2,"error":"booting"}"#,
            )
            .create_async()
            .await;

        let cache = new_shared_cache();
        // Seed a coherent snapshot, as the MQTT retained path would
        // have delivered while the active pull was still pending.
        {
            let mut guard = cache.write().await;
            guard.providers = Some(acowork_core::mqtt_proto::AvailableProviders {
                version: 9,
                #[allow(deprecated)]
                default_compact_model: None,
                default_compact_models: vec![],
                providers: vec![],
            });
        }

        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(outcome, PullOutcome::NotReady(2)));
        // The critical invariant: a 503 must NOT touch the cache.
        let guard = cache.read().await;
        assert_eq!(guard.providers.as_ref().unwrap().version, 9);
    }

    #[tokio::test]
    async fn try_pull_once_uses_longer_of_header_and_body_retry_after() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(503)
            .with_header("retry-after", "1")
            .with_body(
                r#"{"instance_id":"gen-A","phase":"Failed",
                   "phase_detail":"provider onboarding failed",
                   "retry_after_seconds":10,"error":"failed"}"#,
            )
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        // Header says 1s, body says 10s — the LONGER hint wins so we
        // never retry before the Gateway is actually ready.
        assert!(matches!(outcome, PullOutcome::NotReady(10)));
    }

    #[tokio::test]
    async fn try_pull_once_aborts_on_shutting_down_sentinel() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(503)
            .with_header("retry-after", "-1")
            .with_body(
                r#"{"instance_id":"gen-A","phase":"ShuttingDown",
                   "phase_detail":"gateway exiting",
                   "retry_after_seconds":-1,"error":"shutting_down"}"#,
            )
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(
            outcome,
            PullOutcome::NotReady(RETRY_AFTER_DONT_RETRY)
        ));
    }

    #[tokio::test]
    async fn try_pull_once_defaults_to_two_seconds_without_hint() {
        // A 503 with no Retry-After at all (header absent, body field
        // absent) must still make forward progress: the Booting default
        // of 2s kicks in instead of busy-looping.
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(503)
            .with_body(r#"{"instance_id":"gen-A"}"#)
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(outcome, PullOutcome::NotReady(2)));
    }

    #[tokio::test]
    async fn try_pull_once_applies_200_snapshot() {
        let mut server = Server::new_async().await;
        let body = serde_json::json!({
            "instance_id": "gen-A",
            "topics": {
                "acowork/global/providers": providers_topic_payload(3),
            }
        })
        .to_string();
        server
            .mock("GET", "/api/global-resources")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(outcome, PullOutcome::Applied));
        let guard = cache.read().await;
        let providers = guard.providers.as_ref().expect("providers applied");
        assert_eq!(providers.version, 3);
        assert_eq!(providers.providers[0].id, "openai");
    }

    #[tokio::test]
    async fn try_pull_once_generation_switch_clears_old_snapshots() {
        // ADR-059 §5.3: a different remote instance_id must drop the
        // old generation's resource snapshots BEFORE the new payload is
        // applied — otherwise the new (lower-version) snapshot would be
        // rejected as "stale" by update_from_mqtt and the old
        // generation's providers would linger.
        let mut server = Server::new_async().await;
        let body = serde_json::json!({
            "instance_id": "gen-B",
            "topics": {
                "acowork/global/providers": providers_topic_payload(3),
                "acowork/global/bootstrap": bootstrap_topic_payload(
                    "gen-B", 1, acowork_core::mqtt_proto::BootstrapPhase::Ready as i32,
                ),
            }
        })
        .to_string();
        server
            .mock("GET", "/api/global-resources")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;

        let cache = new_shared_cache();
        {
            let mut guard = cache.write().await;
            guard.bootstrap = Some(BootstrapState {
                protocol_version: 1,
                instance_id: "9d0e1f2a-3b4c-4025-8465-157f8a9b0c1d".to_string(),
                version: 7,
                phase: acowork_core::mqtt_proto::BootstrapPhase::Ready as i32,
                phase_detail: "old generation".to_string(),
                issued_at_ms: 0,
            });
            guard.providers = Some(acowork_core::mqtt_proto::AvailableProviders {
                version: 9,
                #[allow(deprecated)]
                default_compact_model: None,
                default_compact_models: vec![],
                providers: vec![],
            });
        }

        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(outcome, PullOutcome::Applied));
        let guard = cache.read().await;
        assert_eq!(guard.bootstrap.as_ref().unwrap().instance_id, "gen-B");
        // v3 came from gen-B; had the switch not cleared gen-A's v9
        // first, update_from_mqtt would have rejected it as stale.
        assert_eq!(guard.providers.as_ref().unwrap().version, 3);
    }

    #[tokio::test]
    async fn try_pull_once_fatal_on_unexpected_4xx() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(401)
            .with_body(r#"{"error":"unauthorized"}"#)
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(outcome, PullOutcome::Fatal(_)));
    }

    /// ADR-076 regression: the pull must present the node credential.
    ///
    /// A 401 is `Fatal`, so an unauthenticated pull gives up on the *first*
    /// attempt and the Runtime silently falls back to MQTT-retained only —
    /// no error anywhere, just a permanently thinner resource view. Assert
    /// the header actually goes out, since the mock below matches on it
    /// and returns 401 when it is absent.
    #[tokio::test]
    async fn try_pull_once_presents_the_node_credential() {
        let mut server = Server::new_async().await;
        let body = serde_json::json!({
            "instance_id": "gen-A",
            "topics": {
                "acowork/global/providers": providers_topic_payload(3),
            }
        })
        .to_string();
        server
            .mock("GET", "/api/global-resources")
            .match_header(acowork_core::auth::NODE_TOKEN_HEADER, "tok-abc")
            .with_status(200)
            .with_body(body)
            .create_async()
            .await;
        // Same route, but a request arriving without the credential is
        // rejected the way `auth_middleware` would — so this mock only
        // matches if the header was actually attached.
        server
            .mock("GET", "/api/global-resources")
            .with_status(401)
            .with_body(r#"{"error":"missing bearer token"}"#)
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, Some("tok-abc")).await;

        assert!(
            matches!(outcome, PullOutcome::Applied),
            "the node credential must be attached or the pull 401s and \
             aborts as Fatal: {outcome:?}"
        );
    }

    /// No credential (standalone Runtime) must simply omit the header
    /// rather than send an empty one — an empty value would reach the
    /// Gateway's middleware and be compared against every node token.
    #[tokio::test]
    async fn try_pull_once_omits_empty_node_credential() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(401)
            .with_body(r#"{"error":"unauthorized"}"#)
            .expect_at_least(1)
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        // Standalone: no credential → the request still goes out (and the
        // 401 is the pre-ADR-076 behaviour this test pins), just without a
        // bogus header.
        let outcome = try_pull_once(&client, &url, &cache, Some("")).await;
        assert!(matches!(outcome, PullOutcome::Fatal(_)));
    }

    #[tokio::test]
    async fn try_pull_once_transient_on_5xx() {
        let mut server = Server::new_async().await;
        server
            .mock("GET", "/api/global-resources")
            .with_status(500)
            .with_body(r#"{"error":"internal"}"#)
            .create_async()
            .await;

        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let url = format!("{}/api/global-resources", server.url());
        let outcome = try_pull_once(&client, &url, &cache, None).await;

        assert!(matches!(outcome, PullOutcome::Transient(_)));
    }

    #[tokio::test]
    async fn try_pull_once_transient_on_connection_error() {
        // No server listens on this port — reqwest fails at connect
        // time, which must map to Transient (retryable), never Fatal.
        let cache = new_shared_cache();
        let client = reqwest::Client::new();
        let outcome = try_pull_once(
            &client,
            "http://127.0.0.1:1/api/global-resources",
            &cache,
            None,
        )
        .await;

        assert!(matches!(outcome, PullOutcome::Transient(_)));
    }
}
