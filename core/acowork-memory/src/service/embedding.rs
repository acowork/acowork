//! OpenAI-compatible embedding client for the memory service (ADR-101).
//!
//! The service needs outbound embedding calls at two moments, and they do not
//! share a configuration:
//!
//! 1. **Normal operation** — `memory_store` / `memory_recall` embed through the
//!    provider installed from the `[embedding]` config section
//!    ([`crate::service::state::MemoryState::set_embedding`]).
//! 2. **Index rebuild** — `POST /memory/rebuild-embeddings` carries the model
//!    the operator has *just* switched to (`endpoint` / `model_id` /
//!    `dimension` in the request body), so the migration must embed against
//!    that target, not against whatever the service loaded at startup. Hence a
//!    throwaway client built from the request ([`HttpEmbeddingClient::new`]).
//!
//! Keeping both on one client type is what stops the rebuild path and the
//! steady-state path from drifting apart on response parsing or error wording.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use acowork_core::{EmbeddingError, EmbeddingProvider};

/// Provider name reported through the trait.
const PROVIDER_NAME: &str = "memory-service-http";

/// Plain HTTP embedding provider speaking the OpenAI `/embeddings` contract.
pub struct HttpEmbeddingClient {
    /// Base URL **including** the API prefix, without the `/embeddings` suffix
    /// and without a trailing slash, e.g. `http://127.0.0.1:18080/v1`.
    base_url: String,
    api_key: Option<String>,
    model: String,
    /// Vector width this provider reports. Seeds from the declaration but is
    /// corrected by [`Self::record_observed_dim`] on every successful embed —
    /// see the trait impl note on why measurement beats declaration.
    dimension: AtomicUsize,
    http: Client,
    /// Request timeout this client was built with, kept because reqwest offers
    /// no getter and a zero/typo timeout is otherwise invisible in a log.
    timeout: Duration,
}

impl HttpEmbeddingClient {
    /// Build a client for one endpoint.
    ///
    /// `api_key` is optional — the local acowork-embed sidecar runs
    /// unauthenticated and rejects nothing, while a cloud provider needs the
    /// bearer token. `dimension` is the client's opening declaration (what the
    /// store seeds its vector index from, what the panel compares against
    /// `stored_dim`), not a hard claim: once vectors arrive, the observed
    /// width wins, because a declaration read from a stale config default is
    /// exactly what kept the rebuild banner up after every node had already
    /// been re-embedded at the real width.
    pub fn new(
        base_url: &str,
        api_key: Option<&str>,
        model: &str,
        dimension: usize,
        timeout: Duration,
    ) -> Result<Self, EmbeddingError> {
        let http = Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| EmbeddingError::Remote(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            base_url: normalize_base_url(base_url),
            api_key: api_key.map(str::to_string).filter(|k| !k.is_empty()),
            model: model.to_string(),
            dimension: AtomicUsize::new(dimension),
            http,
            timeout,
        })
    }

    /// Follow the provider's real output width. Called after every successful
    /// batch with the first vector's length; logs only when the width actually
    /// changes, so a converged client stays quiet.
    fn record_observed_dim(&self, len: usize) {
        if len == 0 {
            return;
        }
        let prev = self.dimension.swap(len, Ordering::Relaxed);
        if prev != len {
            tracing::warn!(
                declared = prev,
                observed = len,
                base_url = %self.base_url,
                model = %self.model,
                "embedding provider reports a different dimension than declared — following the provider"
            );
        }
    }

    /// The per-request timeout this client enforces — reqwest keeps no getter,
    /// so diagnostics and tests read it back from here.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Build from the service's `[embedding]` config section plus the width the
    /// store indexes at (`MemoryServiceConfig::embedding_dim`) — the section
    /// itself has no dimension field, because the dimension is a property of
    /// the storage layout, not of the endpoint.
    pub fn from_config(
        section: &crate::service::config::EmbeddingSection,
        dimension: usize,
    ) -> Result<Self, EmbeddingError> {
        Self::new(
            &section.base_url,
            section.api_key.as_deref(),
            section.model.as_deref().unwrap_or(""),
            dimension,
            Duration::from_millis(section.timeout_ms.max(1)),
        )
    }

    /// POST `{base_url}/embeddings` with one batch and return the vectors in
    /// the order the provider reported them.
    async fn post(&self, input: EmbeddingInput) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let request = EmbeddingRequest {
            model: self.model.clone(),
            input,
        };
        let mut builder = self.http.post(format!("{}/embeddings", self.base_url));
        if let Some(ref key) = self.api_key {
            builder = builder.bearer_auth(key);
        }
        let response = builder
            .json(&request)
            .send()
            .await
            .map_err(|e| EmbeddingError::Remote(format!("request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            // Prefer the provider's own message: an HTTP 400 caused by a wrong
            // model name is actionable, "HTTP 400 Bad Request" is not.
            let message = serde_json::from_str::<EmbeddingErrorResp>(&body)
                .map(|e| e.error.message)
                .unwrap_or_else(|_| format!("HTTP {status}: {body}"));
            return Err(EmbeddingError::Remote(message));
        }

        let resp: EmbeddingResponse = response
            .json()
            .await
            .map_err(|e| EmbeddingError::Remote(format!("failed to parse response: {e}")))?;
        let vectors: Vec<Vec<f32>> = resp.data.into_iter().map(|d| d.embedding).collect();
        if let Some(first) = vectors.first() {
            self.record_observed_dim(first.len());
        }
        Ok(vectors)
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbeddingClient {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if text.is_empty() {
            return Err(EmbeddingError::InvalidInput("text is empty".to_string()));
        }
        self.post(EmbeddingInput::Single(text.to_string()))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::Remote("no embedding in response".to_string()))
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let input = if texts.len() == 1 {
            EmbeddingInput::Single(texts[0].to_string())
        } else {
            EmbeddingInput::Batch(texts.iter().map(|t| t.to_string()).collect())
        };
        self.post(input).await
    }

    fn dimension(&self) -> usize {
        self.dimension.load(Ordering::Relaxed)
    }

    /// Reachability probe: `GET {base_url}/models`, which both the embed
    /// sidecar and the OpenAI-compatible cloud providers serve.
    ///
    /// Deliberately not an embedding call — the pipeline asks this before
    /// deciding whether to start, and a probe must not write a vector anywhere.
    async fn is_available(&self) -> bool {
        let mut builder = self.http.get(format!("{}/models", self.base_url));
        if let Some(ref key) = self.api_key {
            builder = builder.bearer_auth(key);
        }
        builder.send().await.is_ok_and(|r| r.status().is_success())
    }
}

/// Trim whitespace and the trailing slash, so callers can write either
/// `http://host:18080/v1` or `http://host:18080/v1/` and get one URL.
fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

// ── Wire types (OpenAI-compatible) ────────────────────────────────────────

#[derive(Debug, Serialize)]
struct EmbeddingRequest {
    model: String,
    input: EmbeddingInput,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
enum EmbeddingInput {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingErrorResp {
    error: EmbeddingErrorDetail,
}

#[derive(Debug, Deserialize)]
struct EmbeddingErrorDetail {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;

    fn client(base_url: &str) -> HttpEmbeddingClient {
        HttpEmbeddingClient::new(
            base_url,
            None,
            "bge-small-zh-v1.5",
            4,
            Duration::from_secs(5),
        )
        .unwrap()
    }

    /// A stub OpenAI-compatible server: records the last request body and
    /// answers with vectors of the requested count.
    struct Stub {
        base_url: String,
        seen: std::sync::Arc<tokio::sync::Mutex<Option<serde_json::Value>>>,
    }

    async fn stub_server(reply: serde_json::Value) -> Stub {
        let seen: std::sync::Arc<tokio::sync::Mutex<Option<serde_json::Value>>> =
            std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let shared = seen.clone();
        let app = axum::Router::new().route(
            "/v1/embeddings",
            axum::routing::post(move |Json(body): Json<serde_json::Value>| {
                let reply = reply.clone();
                let shared = shared.clone();
                async move {
                    *shared.lock().await = Some(body);
                    Json(reply)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Stub {
            base_url: format!("http://{addr}/v1"),
            seen,
        }
    }

    fn two_vectors() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {"embedding": [0.1, 0.2, 0.3, 0.4]},
                {"embedding": [0.5, 0.6, 0.7, 0.8]}
            ]
        })
    }

    #[test]
    fn base_url_is_normalized() {
        assert_eq!(normalize_base_url("http://h:1/v1/"), "http://h:1/v1");
        assert_eq!(normalize_base_url("  http://h/v1  "), "http://h/v1");
    }

    #[tokio::test]
    async fn embed_sends_the_openai_shape_and_reads_the_first_vector() {
        let stub = stub_server(two_vectors()).await;
        let got = client(&stub.base_url).embed("hello").await.unwrap();
        assert_eq!(got, vec![0.1f32, 0.2, 0.3, 0.4]);

        let sent = stub.seen.lock().await.clone().unwrap();
        assert_eq!(sent["model"], "bge-small-zh-v1.5");
        // A single input travels as a bare string, not a one-element array —
        // some providers reject the array form.
        assert_eq!(sent["input"], "hello");
    }

    #[tokio::test]
    async fn embed_batch_sends_an_array_and_keeps_order() {
        let stub = stub_server(two_vectors()).await;
        let got = client(&stub.base_url)
            .embed_batch(&["a", "b"])
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1], vec![0.5f32, 0.6, 0.7, 0.8]);
        assert_eq!(
            stub.seen.lock().await.clone().unwrap()["input"],
            serde_json::json!(["a", "b"])
        );
    }

    #[tokio::test]
    async fn empty_text_and_empty_batch_short_circuit() {
        let c = client("http://127.0.0.1:1/v1");
        assert!(matches!(
            c.embed("").await,
            Err(EmbeddingError::InvalidInput(_))
        ));
        assert!(c.embed_batch(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn error_body_message_is_surfaced_verbatim() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/v1/embeddings",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": {"message": "model not found"}})),
                    )
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let err = client(&format!("http://{addr}/v1"))
            .embed("x")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("model not found"),
            "provider message lost: {err}"
        );
    }

    #[tokio::test]
    async fn availability_probes_models_and_reports_unreachable() {
        let stub = stub_server(two_vectors()).await;
        assert!(
            !client(&stub.base_url).is_available().await,
            "no /models route"
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = axum::Router::new().route(
                "/v1/models",
                axum::routing::get(|| async { Json(serde_json::json!({"data": []})) }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        assert!(client(&format!("http://{addr}/v1")).is_available().await);
    }

    #[test]
    fn dimension_starts_at_the_declaration() {
        let c = client("http://h/v1");
        assert_eq!(c.dimension(), 4);
        assert_eq!(c.name(), PROVIDER_NAME);
    }

    /// The declaration is only the opening value: a provider whose real output
    /// is wider (config default 384 vs a loaded bge-m3, the rebuild-banner
    /// loop) must be followed, or `model_dim` never converges with `stored_dim`
    /// and the panel asks for a rebuild forever.
    #[tokio::test]
    async fn observed_dimension_follows_the_provider() {
        let stub = stub_server(serde_json::json!({
            "data": [{"embedding": [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]}]
        }))
        .await;
        let c = client(&stub.base_url);
        assert_eq!(c.dimension(), 4, "before any traffic the declaration stands");
        c.embed("hello").await.unwrap();
        assert_eq!(c.dimension(), 6, "the observed width wins after an embed");
        // Converged: a repeat call at the same width changes nothing.
        c.embed("again").await.unwrap();
        assert_eq!(c.dimension(), 6);
    }

    #[test]
    fn config_section_maps_to_a_client() {
        let section = crate::service::config::EmbeddingSection {
            base_url: "http://127.0.0.1:18080/v1/".into(),
            model: Some("bge-small-zh-v1.5".into()),
            api_key: None,
            timeout_ms: 0,
            ..Default::default()
        };
        let c = HttpEmbeddingClient::from_config(&section, 384).unwrap();
        assert_eq!(c.base_url, "http://127.0.0.1:18080/v1");
        assert_eq!(c.dimension(), 384);
        // A zero timeout would make every call fail instantly; `max(1)` keeps
        // the config unusable-by-typo out of the client.
        assert_eq!(c.timeout(), Duration::from_millis(1));
    }
}

