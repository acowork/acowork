//! Gateway global-resources client (ADR-101 decision 7).
//!
//! The distiller and the embedding path both need credentials the memory
//! process does not own: the LLM provider list (base URL + decrypted API key)
//! and the active embedding endpoint. Both live behind the Gateway Vault, and
//! the Gateway publishes them as the `acowork/global/*` snapshot — the same
//! five retained topics the Runtime consumes over MQTT, projected over HTTP by
//! `GET /api/global-resources` (protocol §4.13).
//!
//! Why HTTP and not MQTT: the Runtime's MQTT subscription exists to react to
//! changes within milliseconds for an interactive session. The memory pipeline
//! polls once per hour; a deterministic pull on the same documented contract
//! keeps this service free of a broker client while staying bit-compatible
//! with the MQTT payload (each topic value is the base64 of the same
//! `DataEnvelope` protobuf bytes). The MQTT flags on the CLI stay as the
//! upgrade path if the refresh cadence ever has to tighten.
//!
//! Retry semantics mirror the Runtime's active pull
//! (`acowork-runtime/src/startup/global_resources_pull.rs`) because the
//! Gateway answers the same way while it is `Booting` / `Failed`:
//!
//! - `503` → honour `Retry-After` (header or body `retry_after_seconds`, the
//!   longer of the two; `-1` means "stop, the Gateway is shutting down");
//! - connect / `5xx` other than `503` → linear backoff, capped;
//! - `4xx` other than `503`, undecodable body → give up (a credential problem
//!   never fixes itself);
//! - a total deadline, so a stuck Gateway delays the pipeline instead of
//!   blocking service startup.

use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use prost::Message as _;
use serde::Deserialize;

use acowork_core::defaults::GATEWAY_HTTP_PORT;
use acowork_core::mqtt_proto::{DataEnvelope, data_envelope::Payload};
use acowork_core::protocol::{CompactModelRef, ProtocolType, llm_protocol_to_protocol_type};

/// Topic key carrying the LLM provider list (with decrypted keys).
pub const TOPIC_PROVIDERS: &str = "acowork/global/providers";
/// Topic key carrying the embedding endpoint / active model snapshot.
pub const TOPIC_EMBEDDING_MODELS: &str = "acowork/global/embedding_models";

/// Deadline for the startup pull: long enough to outlive a cold Vault unlock +
/// provider onboarding, short enough that a wedged Gateway cannot keep the
/// HTTP server from serving (the store paths work without any provider).
pub const PULL_MAX_DURATION: Duration = Duration::from_secs(30);
/// Base backoff between transient failures (linear, the Gateway is not
/// adversarial).
pub const PULL_BACKOFF_BASE: Duration = Duration::from_millis(500);
/// Cap for that backoff.
pub const PULL_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// `Retry-After` sentinel meaning "abort, do not retry".
pub const RETRY_AFTER_DONT_RETRY: i64 = -1;
/// Default gap between background refreshes once the first pull succeeded.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// One LLM provider as the Gateway publishes it, reduced to what a chat call
/// needs. `api_key` is the Vault-decrypted value; it is never written to disk
/// by this service and never echoed into a response body or log line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderSnapshot {
    pub id: String,
    pub base_url: String,
    pub protocol: ProtocolType,
    pub api_key: Option<String>,
    /// Model ids this provider serves, in the order the Gateway published.
    pub models: Vec<String>,
    /// Per-provider compact model, when one is configured.
    pub compact_model: Option<String>,
}

/// The embedding backend the Gateway currently considers live. Cloud selection
/// wins over the local ONNX sidecar, matching what the Runtime builds from the
/// same payload.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmbeddingSnapshot {
    /// OpenAI-compatible base URL without the `/embeddings` suffix.
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    /// Vector width the active model emits; 0 = unknown.
    pub dimension: usize,
    /// True when the endpoint is a cloud provider (carries an API key).
    pub cloud: bool,
}

/// A decoded `GET /api/global-resources` snapshot, limited to the two topics
/// this service consumes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GlobalResources {
    pub providers: Vec<ProviderSnapshot>,
    /// Global compact-model candidate chain (ADR-056 follow-up), the distiller
    /// fallback when `[distiller].models` is empty.
    pub default_compact_models: Vec<CompactModelRef>,
    pub embedding: Option<EmbeddingSnapshot>,
    /// Gateway generation id the snapshot came from — a change means the
    /// Gateway restarted and every cached credential belongs to the previous
    /// generation.
    pub instance_id: String,
}

/// Result of one HTTP round-trip.
#[derive(Debug)]
pub enum PullOutcome {
    /// `200` and a decoded snapshot.
    Applied(GlobalResources),
    /// `503`: how long to wait before the next attempt (`None` = abort).
    NotReady(Option<Duration>),
    /// Not retryable, or retryable until the deadline runs out.
    Failed(String),
}

/// Minimal projection of the Gateway's `NotReadyView` (§4.13). Re-declared
/// here rather than imported so this crate does not depend on the Gateway.
#[derive(Debug, Deserialize)]
struct NotReadyView {
    #[serde(default)]
    retry_after_seconds: i64,
}

/// Client for the Gateway global-resources endpoint.
#[derive(Debug, Clone)]
pub struct ResourceClient {
    url: String,
    /// Presented as `Authorization: Bearer` — the **internal service token**
    /// the Gateway hands to the processes it spawns (the same credential
    /// `acowork-pm` uses). The auth middleware admits it on every `/api/*`
    /// route under `multi_user`; in `local` mode nothing is checked and the
    /// token is simply absent. Without it, a `multi_user` Gateway answers 401
    /// and the pipeline silently runs with no model.
    gateway_token: Option<String>,
    http: reqwest::Client,
}

impl ResourceClient {
    /// Build a client for `http://{gateway_host}:{GATEWAY_HTTP_PORT}`.
    pub fn new(gateway_host: &str, gateway_token: Option<String>, timeout: Duration) -> Self {
        let host = if gateway_host.trim().is_empty() {
            "127.0.0.1"
        } else {
            gateway_host.trim()
        };
        Self::for_base(
            &format!("http://{host}:{GATEWAY_HTTP_PORT}"),
            gateway_token,
            timeout,
        )
    }

    /// Build a client against an explicit Gateway base URL (tests point this
    /// at a stub; production uses [`ResourceClient::new`]).
    pub fn for_base(base_url: &str, gateway_token: Option<String>, timeout: Duration) -> Self {
        Self {
            url: format!("{}/api/global-resources", base_url.trim_end_matches('/')),
            gateway_token: gateway_token.filter(|t| !t.is_empty()),
            http: reqwest::Client::builder()
                .timeout(timeout)
                .build()
                .unwrap_or_default(),
        }
    }

    /// The URL this client pulls (diagnostics and tests).
    pub fn url(&self) -> &str {
        &self.url
    }

    /// One round-trip: send, map the status, decode the two topics we use.
    pub async fn pull_once(&self) -> PullOutcome {
        let mut req = self.http.get(&self.url);
        if let Some(token) = &self.gateway_token {
            req = req.bearer_auth(token);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return PullOutcome::Failed(format!("send: {e}")),
        };
        let status = resp.status();

        if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
            let header = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let body = match resp.json::<NotReadyView>().await {
                Ok(view) => view.retry_after_seconds,
                Err(_) => 0,
            };
            let retry_after = match (header, body) {
                (a, b) if a == RETRY_AFTER_DONT_RETRY || b == RETRY_AFTER_DONT_RETRY => {
                    return PullOutcome::NotReady(None);
                }
                (0, 0) => 2,
                (h, b) => h.max(b),
            };
            return PullOutcome::NotReady(Some(Duration::from_secs(
                retry_after.clamp(1, 60) as u64
            )));
        }

        if !status.is_success() {
            return PullOutcome::Failed(format!("unexpected status {status}"));
        }

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => return PullOutcome::Failed(format!("parse JSON body: {e}")),
        };
        match decode_snapshot(&body) {
            Ok(snapshot) => PullOutcome::Applied(snapshot),
            Err(e) => PullOutcome::Failed(format!("decode topics: {e}")),
        }
    }

    /// Pull with the documented retry loop: `Retry-After` on `503`, linear
    /// backoff on transient failures, abort on a fatal error or on reaching
    /// `deadline`. Returns the snapshot, or `None` when nothing was obtained.
    ///
    /// The deadline is a **hard** bound: a `Retry-After` longer than what is
    /// left is truncated to the remaining budget instead of being slept out.
    /// (The Runtime's loop slept the full hint and only checked the deadline
    /// afterwards, so one `Failed` Gateway with a 60 s hint overshot its 30 s
    /// budget by 30 s — a memory pipeline that wakes up hourly cannot afford
    /// to be wrong about that.)
    pub async fn pull_with_retry(&self, deadline: Duration) -> Option<GlobalResources> {
        let start = Instant::now();
        let mut attempt = 0u32;
        loop {
            if start.elapsed() >= deadline {
                tracing::warn!(
                    url = %self.url,
                    attempt,
                    "global-resources pull exceeded its deadline — the distiller \
                     stays without a model until the next refresh"
                );
                return None;
            }
            attempt += 1;
            match self.pull_once().await {
                PullOutcome::Applied(snapshot) => {
                    tracing::info!(
                        url = %self.url,
                        attempt,
                        providers = snapshot.providers.len(),
                        embedding = snapshot.embedding.is_some(),
                        "pulled Gateway global resources"
                    );
                    return Some(snapshot);
                }
                PullOutcome::NotReady(None) => {
                    tracing::error!(
                        url = %self.url,
                        "Gateway reports ShuttingDown — aborting the global-resources pull"
                    );
                    return None;
                }
                PullOutcome::NotReady(Some(wait)) => {
                    tracing::debug!(
                        url = %self.url,
                        attempt,
                        retry_after_ms = wait.as_millis() as u64,
                        "Gateway not ready for global resources"
                    );
                    self.sleep_within(deadline, start, wait).await;
                }
                PullOutcome::Failed(msg) => {
                    // A 4xx / decode failure never resolves by retrying; a
                    // connection error might, so keep going until the deadline.
                    if fatal_message(&msg) {
                        tracing::error!(
                            url = %self.url,
                            error = %msg,
                            "fatal error pulling global-resources"
                        );
                        return None;
                    }
                    let backoff = (PULL_BACKOFF_BASE.saturating_mul(attempt)).min(PULL_BACKOFF_MAX);
                    tracing::warn!(
                        url = %self.url,
                        attempt,
                        error = %msg,
                        backoff_ms = backoff.as_millis() as u64,
                        "transient error pulling global-resources"
                    );
                    self.sleep_within(deadline, start, backoff).await;
                }
            }
        }
    }

    /// Sleep at most until `deadline` elapses, so no hint can overshoot it.
    async fn sleep_within(&self, deadline: Duration, start: Instant, wanted: Duration) {
        let remaining = deadline.saturating_sub(start.elapsed());
        tokio::time::sleep(wanted.min(remaining)).await;
    }
}

/// Errors that will not fix themselves: an auth rejection or a body we cannot
/// decode. Everything else (connect refused, timeout, 5xx) is transient.
fn fatal_message(msg: &str) -> bool {
    msg.starts_with("unexpected status 4")
        || msg.starts_with("decode topics")
        || msg.starts_with("parse JSON body")
}

/// Decode the topics map of a `200` body into [`GlobalResources`].
///
/// Unknown / undecodable topics are skipped with a warning: the Gateway ships
/// five of them and this service only consumes two.
pub fn decode_snapshot(body: &serde_json::Value) -> std::result::Result<GlobalResources, String> {
    let instance_id = body["instance_id"].as_str().unwrap_or("").to_string();
    let topics = body["topics"]
        .as_object()
        .ok_or_else(|| "`topics` missing or not an object".to_string())?;

    let mut out = GlobalResources {
        instance_id,
        ..Default::default()
    };

    if let Some(envelope) = topic_envelope(topics, TOPIC_PROVIDERS)?
        && let Some(Payload::AvailableProviders(providers)) = envelope.payload
    {
        out.providers = providers
            .providers
            .into_iter()
            .map(|p| ProviderSnapshot {
                id: p.id,
                base_url: p.base_url,
                protocol: llm_protocol_to_protocol_type(p.protocol_type),
                api_key: (!p.api_key.is_empty()).then_some(p.api_key),
                models: p.models.into_iter().map(|m| m.id).collect(),
                compact_model: (!p.compact_model.is_empty()).then_some(p.compact_model),
            })
            .collect();
        out.default_compact_models = providers
            .default_compact_models
            .into_iter()
            .map(|m| CompactModelRef {
                provider_id: m.provider_id,
                model_id: m.model_id,
            })
            .collect();
    }

    if let Some(envelope) = topic_envelope(topics, TOPIC_EMBEDDING_MODELS)?
        && let Some(Payload::AvailableEmbeddingModels(models)) = envelope.payload
    {
        // A selected cloud provider beats the local ONNX sidecar; the
        // sidecar endpoint stays as the fallback the payload always
        // carries.
        let cloud = !models.active_provider_id.is_empty() && !models.active_base_url.is_empty();
        let base_url = if cloud {
            Some(models.active_base_url)
        } else if !models.endpoint.is_empty() {
            Some(models.endpoint)
        } else {
            None
        };
        if base_url.is_some() {
            out.embedding = Some(EmbeddingSnapshot {
                base_url,
                model: (!models.active_model_id.is_empty()).then_some(models.active_model_id),
                api_key: (!models.active_api_key.is_empty()).then_some(models.active_api_key),
                dimension: models.active_dimension as usize,
                cloud,
            });
        }
    }

    Ok(out)
}

/// Base64-decode one topic and parse it as a `DataEnvelope`. `Ok(None)` when
/// the topic is absent from the snapshot.
fn topic_envelope(
    topics: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> std::result::Result<Option<DataEnvelope>, String> {
    let Some(value) = topics.get(key) else {
        return Ok(None);
    };
    let Some(b64) = value.as_str() else {
        tracing::warn!(
            topic = key,
            "global-resources topic value is not a string; skipping"
        );
        return Ok(None);
    };
    let bytes = BASE64
        .decode(b64)
        .map_err(|e| format!("{key}: base64: {e}"))?;
    match DataEnvelope::decode(&bytes[..]) {
        Ok(envelope) => Ok(Some(envelope)),
        Err(e) => {
            tracing::warn!(topic = key, error = %e, "topic does not decode as DataEnvelope; skipping");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::mqtt_proto::{
        AvailableEmbeddingModels, AvailableProviders, CompactModelRef as ProtoCompactModelRef,
        ProviderModelRef, ProviderRef,
    };
    use axum::Json;
    use serde_json::json;

    fn envelope(payload: Payload) -> String {
        let msg = DataEnvelope {
            version: 1,
            payload: Some(payload),
        };
        BASE64.encode(msg.encode_to_vec())
    }

    fn providers_topic() -> Payload {
        Payload::AvailableProviders(AvailableProviders {
            version: 7,
            #[allow(deprecated)]
            default_compact_model: None,
            default_compact_models: vec![ProtoCompactModelRef {
                provider_id: "volc".into(),
                model_id: "deepseek-v3".into(),
            }],
            providers: vec![ProviderRef {
                id: "volc".into(),
                base_url: "https://ark.example.com/api/v3".into(),
                protocol_type: acowork_core::mqtt_proto::LlmProtocol::Openai as i32,
                models: vec![
                    ProviderModelRef {
                        id: "deepseek-v3".into(),
                        capabilities: None,
                        max_output_tokens_limit: 0,
                        // ADR-103: model-level fields — absent = inherit provider level.
                        protocol_type: None,
                        base_url: None,
                    },
                    ProviderModelRef {
                        id: "kimi".into(),
                        capabilities: None,
                        max_output_tokens_limit: 0,
                        protocol_type: None,
                        base_url: None,
                    },
                ],
                compact_model: "deepseek-v3".into(),
                custom: false,
                api_key: "sk-live".into(),
                account_id: String::new(),
            }],
        })
    }

    fn embedding_topic(endpoint: &str, cloud_base: &str, cloud_key: &str) -> Payload {
        Payload::AvailableEmbeddingModels(AvailableEmbeddingModels {
            version: 1,
            models: vec![],
            active_model_id: "bge-small-zh-v1.5".into(),
            active_dimension: 512,
            endpoint: endpoint.into(),
            active_provider_id: if cloud_base.is_empty() {
                String::new()
            } else {
                "volc".into()
            },
            active_api_key: cloud_key.into(),
            active_base_url: cloud_base.into(),
        })
    }

    #[test]
    fn decodes_providers_and_local_embedding_endpoint() {
        let body = json!({
            "instance_id": "gen-A",
            "topics": {
                TOPIC_PROVIDERS: envelope(providers_topic()),
                TOPIC_EMBEDDING_MODELS: envelope(embedding_topic("http://127.0.0.1:18080/v1", "", "")),
            }
        });
        let got = decode_snapshot(&body).unwrap();
        assert_eq!(got.instance_id, "gen-A");
        assert_eq!(got.providers.len(), 1);
        let p = &got.providers[0];
        assert_eq!(p.id, "volc");
        assert_eq!(p.protocol, ProtocolType::OpenAI);
        assert_eq!(p.api_key.as_deref(), Some("sk-live"));
        assert_eq!(
            p.models,
            vec!["deepseek-v3".to_string(), "kimi".to_string()]
        );
        assert_eq!(p.compact_model.as_deref(), Some("deepseek-v3"));
        assert_eq!(got.default_compact_models[0].model_id, "deepseek-v3");
        let emb = got.embedding.unwrap();
        assert_eq!(emb.base_url.as_deref(), Some("http://127.0.0.1:18080/v1"));
        assert_eq!(emb.dimension, 512);
        assert!(!emb.cloud, "no active cloud provider");
        assert_eq!(emb.api_key, None);
    }

    #[test]
    fn cloud_embedding_beats_the_local_sidecar() {
        let body = json!({
            "instance_id": "gen-A",
            "topics": {
                TOPIC_EMBEDDING_MODELS: envelope(embedding_topic(
                    "http://127.0.0.1:18080/v1",
                    "https://ark.example.com/api/v3",
                    "sk-embed",
                )),
            }
        });
        let emb = decode_snapshot(&body).unwrap().embedding.unwrap();
        assert!(emb.cloud);
        assert_eq!(
            emb.base_url.as_deref(),
            Some("https://ark.example.com/api/v3")
        );
        assert_eq!(emb.api_key.as_deref(), Some("sk-embed"));
    }

    #[test]
    fn an_empty_snapshot_is_legitimate_not_an_error() {
        // Gateway `Ready` with zero providers (fresh install, no onboarding):
        // decoding must succeed with nothing in it, never fall back to defaults.
        let body = json!({"instance_id": "gen-A", "topics": {}});
        let got = decode_snapshot(&body).unwrap();
        assert!(got.providers.is_empty());
        assert!(got.embedding.is_none());

        let empty_topic = json!({
            "instance_id": "gen-A",
            "topics": {TOPIC_PROVIDERS: envelope(Payload::AvailableProviders(AvailableProviders {
                version: 1,
                #[allow(deprecated)]
                default_compact_model: None,
                default_compact_models: vec![],
                providers: vec![],
            }))}
        });
        assert!(decode_snapshot(&empty_topic).unwrap().providers.is_empty());
    }

    #[test]
    fn a_garbage_topic_is_skipped_not_fatal() {
        let body = json!({
            "instance_id": "gen-A",
            "topics": {
                TOPIC_PROVIDERS: "not base64 at all!!",
                TOPIC_EMBEDDING_MODELS: BASE64.encode(b"protobuf-shaped nonsense"),
            }
        });
        // A base64 decode failure is an error we report; a value that decodes
        // but is not a DataEnvelope is skipped so the other topics still land.
        assert!(decode_snapshot(&body).is_err());
        let body = json!({
            "instance_id": "gen-A",
            "topics": {
                TOPIC_PROVIDERS: BASE64.encode(b"x"),
                TOPIC_EMBEDDING_MODELS: envelope(embedding_topic("http://h/v1", "", "")),
            }
        });
        let got = decode_snapshot(&body).unwrap();
        assert!(got.providers.is_empty());
        assert!(got.embedding.is_some(), "the good topic still applied");
    }

    #[test]
    fn missing_topics_object_is_an_error() {
        assert!(decode_snapshot(&json!({"instance_id": "gen-A"})).is_err());
    }

    #[test]
    fn only_a_real_key_counts_as_a_credential() {
        let mut topic = providers_topic();
        if let Payload::AvailableProviders(ref mut p) = topic {
            p.providers[0].api_key = String::new();
            p.providers[0].compact_model = String::new();
        }
        let body = json!({"instance_id": "g", "topics": {TOPIC_PROVIDERS: envelope(topic)}});
        let got = decode_snapshot(&body).unwrap();
        assert_eq!(got.providers[0].api_key, None, "empty key is no key");
        assert_eq!(got.providers[0].compact_model, None);
    }

    #[test]
    fn backoff_grows_then_caps_and_fatals_are_classified() {
        assert_eq!(
            PULL_BACKOFF_BASE.saturating_mul(1),
            Duration::from_millis(500)
        );
        assert_eq!(
            (PULL_BACKOFF_BASE.saturating_mul(20)).min(PULL_BACKOFF_MAX),
            PULL_BACKOFF_MAX
        );
        assert!(fatal_message("unexpected status 401 Unauthorized"));
        assert!(fatal_message("decode topics: base64: bad"));
        assert!(!fatal_message("send: connection refused"));
        assert!(!fatal_message("unexpected status 503 Service Unavailable"));
    }

    #[test]
    fn client_builds_the_documented_url_and_drops_an_empty_token() {
        let c = ResourceClient::new("127.0.0.1", Some("".into()), Duration::from_secs(1));
        assert_eq!(c.url(), "http://127.0.0.1:19876/api/global-resources");
        assert_eq!(c.gateway_token, None);
        let c = ResourceClient::new("", Some("tok".into()), Duration::from_secs(1));
        assert_eq!(c.gateway_token.as_deref(), Some("tok"));
    }

    /// End-to-end against a stub Gateway: the 503 → 200 transition the retry
    /// loop exists for, plus the service credential actually going out.
    #[tokio::test]
    async fn pull_with_retry_waits_for_ready_then_applies() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Short by design: the loop's own deadline logic is what is under
        // test, and a production 30 s budget would make the suite crawl.
        const DEADLINE: Duration = Duration::from_secs(5);

        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let handler_hits = hits.clone();
        let body = json!({
            "instance_id": "gen-A",
            "topics": {TOPIC_PROVIDERS: envelope(providers_topic())}
        });
        let app = axum::Router::new().route(
            "/api/global-resources",
            axum::routing::get(move |headers: axum::http::HeaderMap| {
                let counter = handler_hits.clone();
                let body = body.clone();
                async move {
                    // Gate on the credential exactly like the auth middleware:
                    // without it the answer is a 401, which the loop must treat
                    // as fatal instead of retrying forever.
                    if headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        != Some("Bearer tok-abc")
                    {
                        return (
                            axum::http::StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "missing service token"})),
                        );
                    }
                    if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (
                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({"instance_id": "gen-A", "retry_after_seconds": 1})),
                        );
                    }
                    (axum::http::StatusCode::OK, Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{addr}");

        let client =
            ResourceClient::for_base(&base, Some("tok-abc".into()), Duration::from_secs(5));
        let pulled =
            tokio::time::timeout(Duration::from_secs(10), client.pull_with_retry(DEADLINE))
                .await
                .expect("the retry loop must finish inside its own deadline");
        let pulled = pulled.expect("503 then 200 should apply");
        assert_eq!(pulled.providers[0].id, "volc");
        assert!(hits.load(Ordering::SeqCst) >= 2, "503 then 200");

        // No credential: the 401 is fatal, so the loop returns None at once —
        // the whole point of classifying 4xx apart from 5xx.
        let anonymous = ResourceClient::for_base(&base, None, Duration::from_secs(5));
        let none =
            tokio::time::timeout(Duration::from_secs(5), anonymous.pull_with_retry(DEADLINE))
                .await
                .expect("a fatal answer must not burn the deadline");
        assert!(none.is_none(), "a 401 must abort the pull");
    }

    /// The retry loop must not run away when the Gateway never becomes ready.
    #[tokio::test]
    async fn pull_with_retry_gives_up_at_the_deadline() {
        let app = axum::Router::new().route(
            "/api/global-resources",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"instance_id": "gen-A", "retry_after_seconds": 60})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = ResourceClient::for_base(
            &format!("http://{addr}"),
            Some("tok".into()),
            Duration::from_secs(2),
        );
        let start = Instant::now();
        let got = client.pull_with_retry(Duration::from_secs(1)).await;
        assert!(got.is_none());
        // The Gateway asked for 60s; the deadline still bounds the loop, with
        // no slack for the hint to overshoot.
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "deadline overshoot: {:?}",
            start.elapsed()
        );
    }
}

