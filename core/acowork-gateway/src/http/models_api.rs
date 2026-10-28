//! Models API — provider model lists and model discovery
//!
//! Endpoints:
//!   GET  /api/models              — list all providers with models
//!   POST /api/models/discover     — discover models from a custom base URL
//!   GET  /api/models/{provider}   — get models for a specific provider
//!
//! Data source: offline_providers.json loaded at startup into a static cache.

use axum::{Json, Router, extract::{Path, State}, http::StatusCode, response::{IntoResponse, Response}, routing::{get, post}};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
use parking_lot::RwLock;

use crate::http::routes::AppState;

// ── Local provider registry ───────────────────────────────────────────

/// Static registry of local (self-hosted) LLM providers.
///
/// Local providers do NOT require an API key and are NOT listed on
/// models.dev. Their models must be discovered at runtime by querying
/// the local server (e.g. ollama `/api/tags`, LMStudio `/v1/models`).
///
/// To add a new local provider, add a tuple here — no other changes needed.
///   (id, display_name, default_base_url)
const LOCAL_PROVIDERS: &[(&str, &str, &str)] = &[
    ("ollama", "Ollama (Local)", "http://localhost:11434"),
    ("lmstudio", "LM Studio (Local)", "http://localhost:1234/v1"),
];

/// Check whether a provider ID refers to a local (self-hosted) provider.
///
/// Used by provider_api.rs to skip API key validation for local providers,
/// and by the frontend to determine UI treatment (no key input, local badge).
pub fn is_local_provider(id: &str) -> bool {
    LOCAL_PROVIDERS.iter().any(|(pid, _, _)| *pid == id)
}

/// Get the default base URL for a local provider, if it exists in the registry.
pub fn local_provider_default_url(id: &str) -> Option<&'static str> {
    LOCAL_PROVIDERS
        .iter()
        .find_map(|(pid, _, url)| if *pid == id { Some(*url) } else { None })
}

/// Derive the protocol type for a local provider.
///
/// ollama → Ollama (native API), lmstudio → OpenAI (OpenAI-compatible).
/// Other local providers default to OpenAI-compatible.
fn local_protocol_type(id: &str) -> acowork_core::protocol::ProtocolType {
    use acowork_core::protocol::ProtocolType;
    match id {
        "ollama" => ProtocolType::Ollama,
        _ => ProtocolType::OpenAI,
    }
}

// ── CN variant helpers ────────────────────────────────────────────────

/// Providers that have both international and CN variants on models.dev
const CN_VARIANT_PROVIDERS: &[&str] = &["minimax", "zhipuai", "moonshotai", "alibaba"];

// ── Response types ────────────────────────────────────────────────────

/// A single model from models.dev
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub family: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub tool_call: Option<bool>,
    #[serde(default)]
    pub attachment: Option<bool>,
    #[serde(default)]
    pub temperature: Option<bool>,
    #[serde(default)]
    pub release_date: Option<String>,
    /// Context window size (total tokens: input + output)
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Maximum output tokens
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Maximum input tokens (from models.dev limit.input)
    #[serde(default)]
    pub max_input_tokens: Option<u64>,
    /// Knowledge cutoff date (e.g. "2025-04")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<String>,
    /// Input cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_cost: Option<f64>,
    /// Output cost per million tokens (USD)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_cost: Option<f64>,
    /// Input modalities (e.g. ["text", "image"])
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modalities: Option<Vec<String>>,
    /// Output modalities (e.g. ["text"])
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modalities: Option<Vec<String>>,
}

/// Provider info with its models
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderModels {
    pub id: String,
    pub name: String,
    pub models: Vec<ModelInfo>,
}

// ── Route builder ─────────────────────────────────────────────────────

pub fn models_routes() -> Router<AppState> {
    Router::new()
        .route("/api/models", get(list_all_providers))
        .route("/api/models/discover", post(discover_models))
        .route("/api/models/refresh-catalog", post(refresh_catalog))
        .route("/api/models/{provider}", get(get_provider_models))
}

// ── Model discovery ─────────────────────────────────────────────────

/// Request body for `POST /api/models/discover`.
///
/// Queries an OpenAI-compatible `{base_url}/models` endpoint to discover
/// available models. Used by the frontend when adding a custom provider.
#[derive(Deserialize)]
struct DiscoverModelsRequest {
    /// Base URL of the OpenAI-compatible API (e.g. "https://my-proxy.com/v1")
    base_url: String,
    /// Optional API key for authentication
    #[serde(default)]
    api_key: Option<String>,
}

/// Discover models from an OpenAI-compatible API.
///
/// Core logic shared by `POST /api/models/discover` and
/// `GET /api/models/{provider}` (for custom providers).
///
/// Queries `{base_url}/models` with a 5-second timeout.
/// If `api_key` is provided, sends `Authorization: Bearer {api_key}` header.
/// Returns the discovered model list (enriched from offline data when available).
async fn discover_models_from_url(
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelInfo>, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(key) = api_key
        && !key.is_empty()
    {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("Failed to reach {}: {}", url, e))?;

    if !resp.status().is_success() {
        return Err(format!("Upstream returned HTTP {}", resp.status()));
    }

    let body_val: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {}", e))?;

    let data = body_val
        .get("data")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "Response missing 'data' array".to_string())?;

    let mut models: Vec<ModelInfo> = data
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?;
            Some(ModelInfo {
                id: id.to_string(),
                name: id.to_string(),
                family: None,
                reasoning: None,
                tool_call: None,
                attachment: None,
                temperature: None,
                release_date: None,
                context_window: None,
                max_tokens: None,
                max_input_tokens: None,
                knowledge: None,
                input_cost: None,
                output_cost: None,
                input_modalities: None,
                output_modalities: None,
            })
        })
        .collect();

    // Enrich discovered models with offline capabilities when available.
    for model in &mut models {
        enrich_model_from_offline(model);
    }

    // Sort: reasoning models first, then alphabetically by id
    models.sort_by(|a, b| {
        let a_reasoning = a.reasoning.unwrap_or(false);
        let b_reasoning = b.reasoning.unwrap_or(false);
        b_reasoning.cmp(&a_reasoning).then(a.id.cmp(&b.id))
    });

    Ok(models)
}

/// `POST /api/models/discover` — discover models from an OpenAI-compatible API.
///
/// Queries `{base_url}/models` with a 5-second timeout.
/// If `api_key` is provided, sends `Authorization: Bearer {api_key}` header.
/// Returns the discovered model list (enriched from offline data when available).
async fn discover_models(
    Json(body): Json<DiscoverModelsRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    if body.base_url.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "base_url must not be empty"})),
        ));
    }

    let models = discover_models_from_url(&body.base_url, body.api_key.as_deref())
        .await
        .map_err(|e| {
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({"error": e})),
            )
        })?;

    Ok(Json(serde_json::json!({"models": models})))
}

// ── Catalog refresh ────────────────────────────────────────────────────

/// Default source URL for the offline provider catalog. Override with the
/// `ACOWORK_CATALOG_URL` env var (e.g. for internal mirrors).
fn catalog_source_url() -> String {
    std::env::var("ACOWORK_CATALOG_URL")
        .unwrap_or_else(|_| "https://models.dev/catalog.json".to_string())
}

/// Request body for `POST /api/models/refresh-catalog` (all fields optional).
#[derive(Deserialize, Default)]
#[serde(default)]
struct RefreshCatalogRequest {
    /// Override the catalog source URL for this call (env var wins if unset).
    url: Option<String>,
    /// Hard cap on overall request duration in seconds (server-side, 1..=60).
    timeout_secs: Option<u64>,
}

/// Summary returned to the caller after a successful refresh.
#[derive(Serialize)]
struct RefreshCatalogResponse {
    url: String,
    providers: usize,
    models: usize,
    bytes: usize,
    path: String,
}

/// `POST /api/models/refresh-catalog` — download the latest provider
/// catalog, persist it to `data_dir/offline_providers.json`, and swap the
/// in-memory cache so subsequent requests see the new data immediately.
///
/// Returns 502 on network/parse failure (the on-disk file and in-memory
/// cache are left untouched on failure).
async fn refresh_catalog(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Option<Json<RefreshCatalogRequest>>,
) -> Response {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let url = req
        .url
        .filter(|s| !s.is_empty())
        .unwrap_or_else(catalog_source_url);
    let timeout_secs = req.timeout_secs.unwrap_or(30).clamp(1, 120);
    let sse = wants_sse(&headers);

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("build http client: {e}"),
            );
        }
    };

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            return error_response(
                StatusCode::BAD_GATEWAY,
                format!("fetch catalog failed: {e}"),
            );
        }
    };

    if !resp.status().is_success() {
        return error_response(
            StatusCode::BAD_GATEWAY,
            format!("upstream returned HTTP {}", resp.status()),
        );
    }

    let total = resp.content_length();

    if sse {
        return refresh_catalog_sse(state, url, resp, total).await;
    }

    let bytes = match resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(e) => {
            return error_response(StatusCode::BAD_GATEWAY, format!("read body: {e}"));
        }
    };
    match finalize_refresh(state, url, bytes).await {
        Ok(summary) => Json(summary).into_response(),
        Err((code, msg)) => error_response(code, msg),
    }
}

/// True if the client wants SSE (`Accept: text/event-stream`).
fn wants_sse(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get(axum::http::header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false)
}

fn error_response(code: StatusCode, message: impl Into<String>) -> Response {
    (
        code,
        Json(serde_json::json!({ "error": message.into() })),
    )
        .into_response()
}

/// Streaming variant of the refresh — drives `resp.bytes_stream()`,
/// accumulates into a `Vec<u8>`, emits `progress` events (≥256 KiB or
/// ≥200 ms cadence), then persists + swaps cache and emits `done`.
///
/// Returns an SSE response. The producer task drains the chunk stream
/// in the background; the consumer stream forwards events to the client.
/// On disconnect the producer is cleaned up via the trailing `.chain`
/// join.
async fn refresh_catalog_sse(
    state: AppState,
    url: String,
    resp: reqwest::Response,
    total: Option<u64>,
) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures_util::stream::StreamExt;
    use std::time::{Duration, Instant};
    use tokio::sync::mpsc;

    let (tx, rx) = mpsc::channel::<Result<Event, std::convert::Infallible>>(32);

    let producer = tokio::spawn(async move {
        let mut byte_stream = resp.bytes_stream();
        let mut buf: Vec<u8> =
            Vec::with_capacity(total.unwrap_or(5 * 1024 * 1024) as usize);
        let mut last_emit = Instant::now();
        let mut last_bytes: u64 = 0;

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buf.extend_from_slice(&chunk);
                    let now = buf.len() as u64;
                    let delta = now - last_bytes;
                    let elapsed = last_emit.elapsed();
                    if delta >= 256 * 1024 || elapsed >= Duration::from_millis(200) {
                        last_bytes = now;
                        last_emit = Instant::now();
                        let payload = serde_json::json!({
                            "bytes": now,
                            "total": total,
                        });
                        let event = Event::default()
                            .event("progress")
                            .json_data(payload)
                            .unwrap_or_else(|_| {
                                Event::default().event("progress").data("{}")
                            });
                        if tx.send(Ok(event)).await.is_err() {
                            return; // client disconnected
                        }
                    }
                }
                Err(e) => {
                    let payload = serde_json::json!({ "error": format!("read body: {e}") });
                    let event = Event::default()
                        .event("error")
                        .json_data(payload)
                        .unwrap_or_else(|_| Event::default().event("error").data("{}"));
                    let _ = tx.send(Ok(event)).await;
                    let _ = tx
                        .send(Ok(Event::default()
                            .event("done")
                            .json_data(serde_json::json!({ "ok": false }))
                            .unwrap_or_else(|_| Event::default().event("done").data("{}"))))
                        .await;
                    return;
                }
            }
        }

        match finalize_refresh(state, url.clone(), buf).await {
            Ok(summary) => {
                let event = Event::default()
                    .event("done")
                    .json_data(summary)
                    .unwrap_or_else(|_| Event::default().event("done").data("{}"));
                let _ = tx.send(Ok(event)).await;
            }
            Err((code, msg)) => {
                let payload = serde_json::json!({ "error": msg, "status": code.as_u16() });
                let event = Event::default()
                    .event("error")
                    .json_data(payload)
                    .unwrap_or_else(|_| Event::default().event("error").data("{}"));
                let _ = tx.send(Ok(event)).await;
                let _ = tx
                    .send(Ok(Event::default()
                        .event("done")
                        .json_data(serde_json::json!({ "ok": false }))
                        .unwrap_or_else(|_| Event::default().event("done").data("{}"))))
                    .await;
            }
        }
    });

    // Bridge tokio mpsc Receiver into the SSE response stream. After the
    // channel closes (producer finished or client dropped), wait for the
    // spawned task to clean up so we don't leak a JoinHandle.
    let mut rx = rx;
    let consumer = futures_util::stream::poll_fn(move |cx| {
        let r = std::pin::Pin::new(&mut rx).poll_recv(cx);
        match r {
            std::task::Poll::Ready(Some(event)) => std::task::Poll::Ready(Some(Some(event))),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    })
    .chain(futures_util::stream::once(async move {
        // Drain the producer after the channel closed so the task is
        // awaited (no leaked JoinHandle). Yields no event.
        let _ = producer.await;
        None
    }))
    .filter_map(|x| async move { x });

    Sse::new(consumer)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Persist `bytes` to `data_dir/offline_providers.json` and swap the
/// in-memory cache. Shared by the JSON and SSE code paths.
async fn finalize_refresh(
    state: AppState,
    url: String,
    bytes: Vec<u8>,
) -> Result<RefreshCatalogResponse, (StatusCode, String)> {
    let raw: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| (StatusCode::BAD_GATEWAY, format!("invalid JSON: {e}")))?;
    let providers_val = raw
        .get("providers")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            (
                StatusCode::BAD_GATEWAY,
                "catalog missing 'providers' object".to_string(),
            )
        })?;
    let models_val = raw.get("models").and_then(|v| v.as_object());
    let providers_count = providers_val.len();
    let models_count = models_val.map(|m| m.len()).unwrap_or(0);

    let target_dir = refresh_target_dir();
    std::fs::create_dir_all(&target_dir)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("create data dir: {e}")))?;
    let target = target_dir.join("offline_providers.json");
    let tmp = target.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("write tmp: {e}")))?;
    std::fs::rename(&tmp, &target)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("rename: {e}")))?;

    let cached = match raw.get("providers") {
        Some(serde_json::Value::Object(p)) => serde_json::Value::Object(p.clone()),
        _ => raw,
    };
    replace_offline_providers(cached);

    // The catalog just changed, so capabilities captured when the user added a
    // provider may be stale. Refresh them for the providers that already exist.
    let caps_updated = resync_configured_capabilities(&state).await;

    tracing::info!(
        url = %url,
        providers = providers_count,
        models = models_count,
        bytes = bytes.len(),
        path = %target.display(),
        caps_updated,
        "Refreshed offline provider catalog"
    );

    if let Some(trigger) = state.mqtt_publisher_trigger.as_ref() {
        trigger.trigger();
    }

    Ok(RefreshCatalogResponse {
        url,
        providers: providers_count,
        models: models_count,
        bytes: bytes.len(),
        path: target.display().to_string(),
    })
}

/// Refresh the stored capabilities of already-configured providers from the
/// catalog that was just swapped in.
///
/// Without this, `provider_list.json` keeps the context windows / costs /
/// capability flags captured when the user added the provider, so a catalog
/// refresh would never reach the configured entries (nor the runtimes fed from
/// them).
///
/// Returns the number of model entries updated.
async fn resync_configured_capabilities(state: &AppState) -> usize {
    let mut gw = state.gateway_state.write().await;
    let updated = resync_capabilities_in_place(
        &mut gw.resource_cache.provider_list.providers,
        lookup_model_capabilities,
    );
    if updated > 0 {
        let data_dir = crate::http::provider_api::get_data_dir_from_gw(&gw);
        crate::resource_cache::persist_provider_cache(&mut gw, &data_dir);
    }
    updated
}

/// Pure core of [`resync_configured_capabilities`]; kept separate so it can be
/// tested without a `GatewayState`.
///
/// Skips custom providers (absent from the catalog — their capabilities are
/// hand-written) and local providers (limits depend on the user's hardware and
/// are editable in the UI, so the catalog must not overwrite them). Model
/// *selection*, `base_url`, `compact_model` and `max_output_tokens_limit` are
/// never touched — only `capabilities`.
fn resync_capabilities_in_place(
    providers: &mut [acowork_core::protocol::ProviderListItem],
    lookup: impl Fn(&str, &str) -> Option<acowork_core::protocol::ModelCapabilitiesInfo>,
) -> usize {
    let mut updated = 0;
    for cfg in providers.iter_mut() {
        if cfg.custom || is_local_provider(&cfg.id) {
            continue;
        }
        for entry in cfg.models.iter_mut() {
            if let Some(caps) = lookup(&cfg.id, &entry.id)
                && caps != entry.capabilities
            {
                entry.capabilities = caps;
                updated += 1;
            }
        }
    }
    updated
}

// ── Offline data ──────────────────────────────────────────────────────

/// Load offline provider data from a file on disk.
///
/// Search order:
///   1. $CARGO_MANIFEST_DIR/../../assets/offline_providers.json  (dev / test via cargo)
///   2. {exe_dir}/offline_providers.json                          (installer-provided)
///   3. {cwd}/offline_providers.json                              (dev convenience)
///
/// Static cache holding the offline providers map. Wrapped in `Arc<Value>`
/// so a refresh can swap the inner `Value` atomically without invalidating
/// outstanding references, and so callers can hold an `Arc<Value>` cheaply
/// instead of cloning the multi-MB tree.
static OFFLINE_PROVIDERS: OnceLock<RwLock<Arc<serde_json::Value>>> = OnceLock::new();

/// `data_dir` captured at startup so the read candidates and refresh writes
/// agree on a single, writable location. `None` = test/dev only (no refresh).
static OFFLINE_DATA_DIR: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();

/// Initialise the offline providers cache. Must be called once during
/// Gateway startup, before the HTTP server begins serving requests, with
/// the resolved `data_dir`. Safe to call multiple times — only the first
/// call wins.
pub fn init_offline_providers(data_dir: &std::path::Path) {
    let _ = OFFLINE_DATA_DIR.set(Some(data_dir.to_path_buf()));
    let _ = OFFLINE_PROVIDERS.get_or_init(|| {
        let v = load_offline_providers_from_file(Some(data_dir))
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        RwLock::new(Arc::new(v))
    });
}

fn offline_providers_cache() -> &'static RwLock<Arc<serde_json::Value>> {
    OFFLINE_PROVIDERS.get_or_init(|| {
        let v = load_offline_providers_from_file(OFFLINE_DATA_DIR.get().and_then(|o| o.as_deref()))
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
        RwLock::new(Arc::new(v))
    })
}

/// Returns the current offline providers map (clones the Arc, cheap).
fn offline_providers() -> Arc<serde_json::Value> {
    offline_providers_cache().read().clone()
}

/// Atomically swap the cached offline providers map (used by
/// `POST /api/models/refresh-catalog`).
fn replace_offline_providers(new_value: serde_json::Value) {
    *offline_providers_cache().write() = Arc::new(new_value);
}

/// `data_dir` to use as the writable refresh target. Falls back to the
/// first existing candidate (or cwd) so dev/test without `init` still work.
fn refresh_target_dir() -> std::path::PathBuf {
    if let Some(Some(d)) = OFFLINE_DATA_DIR.get() {
        return d.clone();
    }
    if let Ok(cwd) = std::env::current_dir() {
        return cwd;
    }
    std::path::PathBuf::from(".")
}

/// Catalog file shape (models.dev catalog.json):
///
/// ```json
/// { "models": { ... }, "providers": { "<provider_id>": { id, env, npm, api, name, doc, models: {...} } } }
/// ```
///
/// We keep the filename as `offline_providers.json` for backward compat with
/// installer/tauri bundling, but its content follows the catalog schema.
/// Downstream code consumes a flat `{ provider_id: provider_obj }` map, so the
/// loader unwraps `.providers` here — one boundary, no caller changes.
fn load_offline_providers_from_file(data_dir: Option<&std::path::Path>) -> Option<serde_json::Value> {
    let candidates = build_offline_file_candidates(data_dir);

    for path in &candidates {
        if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => match serde_json::from_str::<serde_json::Value>(&content) {
                    Ok(raw) => {
                        // Adapt catalog.json wrapper `{ models, providers }` → flat provider map.
                        // Fall back to treating the root itself as the provider map for
                        // backward compat with the pre-catalog flat layout.
                        let data = match raw.get("providers") {
                            Some(serde_json::Value::Object(p)) => {
                                serde_json::Value::Object(p.clone())
                            }
                            Some(other) => {
                                tracing::warn!(
                                    "offline_providers.json: 'providers' is not an object ({:?}), using empty",
                                    other
                                );
                                serde_json::Value::Object(serde_json::Map::new())
                            }
                            None => raw,
                        };
                        tracing::info!(
                            "Loaded offline providers from: {} (providers={})",
                            path.display(),
                            data.as_object().map(|m| m.len()).unwrap_or(0)
                        );
                        return Some(data);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse offline_providers.json at {}: {}",
                            path.display(),
                            e
                        );
                    }
                },
                Err(e) => {
                    tracing::warn!(
                        "Failed to read offline_providers.json at {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
    }

    tracing::warn!("offline_providers.json not found in any candidate path, using empty data");
    None
}

fn build_offline_file_candidates(data_dir: Option<&std::path::Path>) -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();

    // 0. data_dir/offline_providers.json (highest priority — written by
    //    POST /api/models/refresh-catalog, survives Gateway restarts).
    if let Some(d) = data_dir {
        candidates.push(d.join("offline_providers.json"));
    }

    // 1. CARGO_MANIFEST_DIR ../../assets/  (dev and test via cargo)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let assets = std::path::PathBuf::from(&manifest_dir)
            .join("..")
            .join("..")
            .join("assets")
            .join("offline_providers.json");
        if assets.exists() {
            candidates.push(assets);
        }
    }

    // 2. Same directory as the executable (installer-provided, read-only)
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(exe_dir) = exe_path.parent()
    {
        candidates.push(exe_dir.join("offline_providers.json"));
    }

    // 3. Current working directory (dev convenience)
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("offline_providers.json"));
    }

    candidates
}

// ── CN variant helpers ────────────────────────────────────────────────

/// Build the list of provider IDs to query for a given provider_id.
///
/// For providers with CN variants, both the base and `-cn` suffixed IDs
/// are returned. For zhipuai, the `zhipuai-coding-plan` variant is also
/// included. No legacy alias mapping (qwen→alibaba, etc.) is performed —
/// callers should use the canonical models.dev provider ID directly.
fn provider_ids_to_query(provider_id: &str) -> Vec<String> {
    let mut ids = vec![provider_id.to_string()];
    if CN_VARIANT_PROVIDERS.contains(&provider_id) {
        ids.push(format!("{}-cn", provider_id));
    }
    // zhipuai also has zhipuai-coding-plan variant
    if provider_id == "zhipuai" {
        ids.push("zhipuai-coding-plan".to_string());
    }
    ids
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Extract models from a provider JSON object.
/// Works with both models.dev API response and offline_providers.json format.
fn extract_models(provider_data: &serde_json::Value) -> Vec<ModelInfo> {
    let models_obj = match provider_data.get("models") {
        Some(serde_json::Value::Object(m)) => m,
        _ => return Vec::new(),
    };

    let mut models = Vec::new();
    for (id, model_data) in models_obj {
        let context_window = model_data
            .get("limit")
            .and_then(|v| v.get("context"))
            .and_then(|v| v.as_u64());

        let max_tokens = model_data
            .get("limit")
            .and_then(|v| v.get("output"))
            .and_then(|v| v.as_u64());

        let max_input_tokens = model_data
            .get("limit")
            .and_then(|v| v.get("input"))
            .and_then(|v| v.as_u64());

        let model = ModelInfo {
            id: id.clone(),
            name: model_data
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(id)
                .to_string(),
            family: model_data
                .get("family")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            reasoning: model_data.get("reasoning").and_then(|v| v.as_bool()),
            tool_call: model_data.get("tool_call").and_then(|v| v.as_bool()),
            attachment: model_data.get("attachment").and_then(|v| v.as_bool()),
            temperature: model_data.get("temperature").and_then(|v| v.as_bool()),
            release_date: model_data
                .get("release_date")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            context_window,
            max_tokens,
            max_input_tokens,
            knowledge: model_data
                .get("knowledge")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            input_cost: model_data
                .get("cost")
                .and_then(|v| v.get("input"))
                .and_then(|v| v.as_f64()),
            output_cost: model_data
                .get("cost")
                .and_then(|v| v.get("output"))
                .and_then(|v| v.as_f64()),
            input_modalities: model_data
                .get("modalities")
                .and_then(|v| v.get("input"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                }),
            output_modalities: model_data
                .get("modalities")
                .and_then(|v| v.get("output"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                }),
        };
        models.push(model);
    }

    // Sort: reasoning models first, then alphabetically by id
    models.sort_by(|a, b| {
        let a_reasoning = a.reasoning.unwrap_or(false);
        let b_reasoning = b.reasoning.unwrap_or(false);
        b_reasoning.cmp(&a_reasoning).then(a.id.cmp(&b.id))
    });

    models
}

/// Resolve provider data from the given JSON value, querying all variant IDs.
/// Returns (provider_name, models) or None if no data found.
fn resolve_provider(
    data: &serde_json::Value,
    provider_id: &str,
) -> Option<(String, Vec<ModelInfo>)> {
    let providers = data.as_object()?;
    let query_ids = provider_ids_to_query(provider_id);
    let mut all_models = Vec::new();
    let mut provider_name: Option<String> = None;

    for qid in &query_ids {
        if let Some(provider_data) = providers.get(qid) {
            if provider_name.is_none() {
                provider_name = Some(
                    provider_data
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(provider_id)
                        .to_string(),
                );
            }
            let models = extract_models(provider_data);
            all_models.extend(models);
        }
    }

    if all_models.is_empty() && provider_name.is_none() {
        return None;
    }

    // Deduplicate by model id
    let mut seen = std::collections::HashSet::new();
    all_models.retain(|m| seen.insert(m.id.clone()));

    Some((
        provider_name.unwrap_or_else(|| provider_id.to_string()),
        all_models,
    ))
}

// ── Handlers ──────────────────────────────────────────────────────────

/// GET /api/models — list all providers with model counts
///
/// Returns offline provider data from the static in-memory cache, plus
/// custom providers from resource_cache (user-defined OpenAI-compatible
/// endpoints) so the frontend can display them in the providers list.
async fn list_all_providers(
    State(state): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let data = offline_providers().clone();

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Invalid provider data"})),
            ));
        }
    };

    let mut result = Vec::new();
    for (id, provider_data) in providers {
        let name = provider_data
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        let model_count = provider_data
            .get("models")
            .and_then(|v| v.as_object())
            .map(|m| m.len())
            .unwrap_or(0);

        let mut entry = serde_json::json!({
            "id": id,
            "name": name,
            "model_count": model_count,
            "local": false,
        });
        // Include the provider's base API URL when available (from models.dev or offline data)
        if let Some(api_url) = provider_data.get("api").and_then(|v| v.as_str()) {
            entry["api"] = serde_json::json!(api_url);
        }
        result.push(entry);
    }

    // Append / overlay local providers from the static registry.
    // Most local providers don't appear in models.dev, but some (e.g. lmstudio) do.
    for (id, name, default_url) in LOCAL_PROVIDERS {
        if let Some(existing) = result
            .iter_mut()
            .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(id))
        {
            // Already present in models.dev data — override to mark as local
            existing["local"] = serde_json::json!(true);
            existing["name"] = serde_json::json!(name);
            if existing.get("api").is_none() {
                existing["api"] = serde_json::json!(default_url);
            }
        } else {
            result.push(serde_json::json!({
                "id": id,
                "name": name,
                "model_count": 0,
                "local": true,
                "api": default_url,
            }));
        }
    }

    // Append custom providers from resource_cache (user-defined OpenAI-compatible
    // endpoints). These are NOT in offline_providers.json, so without this they
    // would be invisible in the provider list on the frontend.
    {
        let gw = state.gateway_state.read().await;
        for cfg in &gw.resource_cache.provider_list.providers {
            if cfg.custom {
                // Skip if already present (shouldn't happen, but defensive)
                if result
                    .iter()
                    .any(|e| e.get("id").and_then(|v| v.as_str()) == Some(&cfg.id))
                {
                    continue;
                }
                result.push(serde_json::json!({
                    "id": cfg.id,
                    "name": cfg.id,
                    "model_count": cfg.models.len(),
                    "local": false,
                    "custom": true,
                    "api": cfg.base_url,
                }));
            }
        }
    }

    // Sort by id
    result.sort_by(|a, b| {
        a.get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .cmp(b.get("id").and_then(|v| v.as_str()).unwrap_or(""))
    });

    Ok(Json(serde_json::json!({"providers": result})))
}

/// GET /api/models/{provider} — get models for a specific provider
///
/// Resolution order:
///   0. (local providers only) Query the running local server directly
///   1. (custom providers only) Discover models live from the stored base_url
///      using the decrypted API key from Vault
///   2. Built-in offline data (instant, always available)
///   3. Empty result
///
/// For local providers (ollama, lmstudio), their models depend on what the
/// user has actually loaded in their local server. We query the server first
/// (with a short timeout), then return empty if unreachable.
///
/// For custom providers (user-defined OpenAI-compatible endpoints), models are
/// discovered live by querying `{base_url}/models` with the stored API key.
/// If discovery fails (network error, auth error), we fall back to the models
/// stored in provider_list.json so the edit dialog can still show configured models.
///
/// For remote providers, returns offline data only.
async fn get_provider_models(
    State(state): State<AppState>,
    Path(provider_id): Path<String>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // 0. For local providers, try querying the running server first
    if is_local_provider(&provider_id) {
        if let Some(models) = fetch_local_server_models(&provider_id).await {
            let name = LOCAL_PROVIDERS
                .iter()
                .find(|(pid, _, _)| *pid == provider_id)
                .map(|(_, n, _)| n.to_string())
                .unwrap_or_else(|| provider_id.clone());
            return Ok(Json(ProviderModels {
                id: provider_id,
                name,
                models,
            }));
        }
        // Server unreachable — offline data is meaningless for local
        // providers (user didn't download those models), return empty.
        tracing::debug!(
            "Local provider {} server not reachable, returning empty model list",
            provider_id
        );
        let name = LOCAL_PROVIDERS
            .iter()
            .find(|(pid, _, _)| *pid == provider_id)
            .map(|(_, n, _)| n.to_string())
            .unwrap_or_else(|| provider_id.clone());
        return Ok(Json(ProviderModels {
            id: provider_id,
            name,
            models: vec![],
        }));
    }

    // 1. For custom providers, discover models live from the stored base_url.
    //
    // Custom providers are NOT in offline_providers.json, so the offline
    // lookup below would return empty. Instead, we use the base_url and
    // API key stored in the Gateway (provider_list.json + Vault) to query
    // the provider's OpenAI-compatible /v1/models endpoint.
    {
        let gw = state.gateway_state.read().await;
        let custom_cfg = gw
            .resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider_id && p.custom);

        if let Some(cfg) = custom_cfg {
            let base_url = cfg.base_url.clone();
            let provider_name = cfg.id.clone();

            // Capture stored capabilities from provider_list.json before
            // dropping the lock. These are the user-saved capabilities that
            // must be merged into the live-discovered models (the /v1/models
            // endpoint does not return capability info).
            //
            // We clone the capabilities (not hold references) so the read
            // lock can be safely dropped before the outbound HTTP call.
            let stored_caps: std::collections::HashMap<String, acowork_core::protocol::ModelCapabilitiesInfo> =
                cfg.models.iter().map(|m| (m.id.clone(), m.capabilities.clone())).collect();
            let stored_model_ids: Vec<String> =
                cfg.models.iter().map(|m| m.id.clone()).collect();

            // Retrieve the decrypted API key from Vault.
            let api_key = gw.vault.get_key(&provider_id).ok();

            // Drop the read lock before making the outbound HTTP request
            // to avoid holding it during the 5-second discovery timeout.
            drop(gw);

            tracing::info!(
                provider = %provider_id,
                base_url = %base_url,
                has_key = api_key.is_some(),
                "Custom provider: discovering models live"
            );

            match discover_models_from_url(&base_url, api_key.as_deref()).await {
                Ok(mut models) => {
                    // Merge stored capabilities from provider_list.json into
                    // the live-discovered models. The /v1/models endpoint
                    // returns only model IDs — context_window, max_tokens,
                    // modalities, etc. come from the user's saved config.
                    for model in &mut models {
                        if let Some(caps) = stored_caps.get(&model.id) {
                            if model.context_window.is_none() {
                                model.context_window = Some(caps.context_window);
                            }
                            if model.max_tokens.is_none() {
                                model.max_tokens = Some(caps.max_output_tokens);
                            }
                            if model.tool_call.is_none() {
                                model.tool_call = Some(caps.supports_tool_calling);
                            }
                            if model.reasoning.is_none() {
                                model.reasoning = caps.supports_reasoning;
                            }
                            if model.attachment.is_none() {
                                model.attachment = caps.supports_attachment;
                            }
                            if model.input_modalities.is_none() {
                                model.input_modalities = caps.modalities.as_ref().map(|m| m.input.clone());
                            }
                            if model.output_modalities.is_none() {
                                model.output_modalities = caps.modalities.as_ref().map(|m| m.output.clone());
                            }
                        }
                    }
                    return Ok(Json(ProviderModels {
                        id: provider_id,
                        name: provider_name,
                        models,
                    }));
                }
                Err(e) => {
                    // Discovery failed (server down, auth error, etc.).
                    // Fall back to the stored model list so the edit dialog
                    // can still show previously configured models with their
                    // saved capabilities.
                    tracing::warn!(
                        provider = %provider_id,
                        error = %e,
                        "Custom provider model discovery failed, falling back to stored models"
                    );
                    let fallback_models: Vec<ModelInfo> = stored_model_ids
                        .iter()
                        .map(|id| {
                            let caps = stored_caps.get(id);
                            ModelInfo {
                                id: id.clone(),
                                name: id.clone(),
                                family: None,
                                reasoning: caps.and_then(|c| c.supports_reasoning),
                                tool_call: caps.map(|c| c.supports_tool_calling),
                                attachment: caps.and_then(|c| c.supports_attachment),
                                temperature: None,
                                release_date: None,
                                context_window: caps.map(|c| c.context_window),
                                max_tokens: caps.map(|c| c.max_output_tokens),
                                max_input_tokens: caps.and_then(|c| c.max_input_tokens),
                                knowledge: None,
                                input_cost: None,
                                output_cost: None,
                                input_modalities: caps.and_then(|c| c.modalities.as_ref()).map(|m| m.input.clone()),
                                output_modalities: caps.and_then(|c| c.modalities.as_ref()).map(|m| m.output.clone()),
                            }
                        })
                        .collect();
                    return Ok(Json(ProviderModels {
                        id: provider_id,
                        name: provider_name,
                        models: fallback_models,
                    }));
                }
            }
        }
    }

    // 2. Try offline data (instant, no network)
    if let Some((name, models)) = resolve_provider(&offline_providers(), &provider_id) {
        return Ok(Json(ProviderModels {
            id: provider_id,
            name,
            models,
        }));
    }

    // 3. Provider not found — return empty model list
    Ok(Json(ProviderModels {
        id: provider_id.clone(),
        name: provider_id,
        models: vec![],
    }))
}

/// Try to discover models from a running local provider server.
///
/// Returns `Some(models)` on success (server responded), `None` if the
/// server is not reachable or the response cannot be parsed.
///
/// Supported providers & endpoints:
///   - lmstudio: `GET {base_url}/models` (OpenAI-compatible)
///   - ollama:   `GET {base_url}/api/tags`
async fn fetch_local_server_models(provider_id: &str) -> Option<Vec<ModelInfo>> {
    let base_url = local_provider_default_url(provider_id)?;

    // Shared HTTP client with a short timeout — if the local server isn't
    // running we don't want the caller hanging for more than 2 seconds.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .ok()?;

    let mut models = match provider_id {
        "lmstudio" => fetch_lmstudio_models(&client, base_url).await,
        "ollama" => fetch_ollama_models(&client, base_url).await,
        _ => None,
    }?;

    // Enrich each model with known capabilities from offline_providers.json.
    // LM Studio / Ollama only returns model IDs — context_window, max_tokens,
    // tool_call support etc. come from the offline data when available.
    for model in &mut models {
        enrich_model_from_offline(model);
    }

    Some(models)
}

/// Parse an OpenAI-compatible `/v1/models` response into `ModelInfo`.
async fn fetch_lmstudio_models(client: &reqwest::Client, base_url: &str) -> Option<Vec<ModelInfo>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let data = body.get("data")?.as_array()?;

    Some(
        data.iter()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?;
                Some(ModelInfo {
                    id: id.to_string(),
                    name: id.to_string(),
                    family: None,
                    reasoning: None,
                    tool_call: None,
                    attachment: None,
                    temperature: None,
                    release_date: None,
                    context_window: None,
                    max_tokens: None,
                    max_input_tokens: None,
                    knowledge: None,
                    input_cost: None,
                    output_cost: None,
                    input_modalities: None,
                    output_modalities: None,
                })
            })
            .collect(),
    )
}

/// Parse an Ollama `/api/tags` response into `ModelInfo`.
async fn fetch_ollama_models(client: &reqwest::Client, base_url: &str) -> Option<Vec<ModelInfo>> {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    let models = body.get("models")?.as_array()?;

    Some(
        models
            .iter()
            .filter_map(|m| {
                let name = m.get("name")?.as_str()?;
                // Strip `:latest` suffix for cleaner display
                let id = name.strip_suffix(":latest").unwrap_or(name).to_string();
                Some(ModelInfo {
                    id: id.clone(),
                    name: id,
                    family: None,
                    reasoning: None,
                    tool_call: None,
                    attachment: None,
                    temperature: None,
                    release_date: None,
                    context_window: None,
                    max_tokens: None,
                    max_input_tokens: None,
                    knowledge: None,
                    input_cost: None,
                    output_cost: None,
                    input_modalities: None,
                    output_modalities: None,
                })
            })
            .collect(),
    )
}

/// Look up a model ID in `offline_providers.json` and copy known capabilities
/// (context_window, max_tokens, tool_call, reasoning, modalities, etc.) into the model.
///
/// Search strategy:
///   1. Exact match — search all providers for the full model ID
///   2. Bare ID — if the model ID contains a path (e.g. "google/gemma-4-26b-a4b"),
///      try the last segment ("gemma-4-26b-a4b") as well
///
/// NOTE: Suffix-based matching (e.g. adding "-it" or stripping "-instruct") is
/// deliberately NOT done — it's unreliable guessing. The caller in the frontend
/// must always fall back to safe defaults when enrichment finds no match.
///
/// This allows locally-discovered models (which only have an ID) to inherit
/// capabilities from the offline data when available.
fn enrich_model_from_offline(model: &mut ModelInfo) {
    let data = offline_providers();
    let providers = match data.as_object() {
        Some(obj) => obj,
        None => return,
    };

    // Build list of candidate IDs to try (ordered by specificity)
    let mut candidates: Vec<String> = Vec::new();

    // 1. Exact match
    candidates.push(model.id.clone());

    // 2. Bare ID
    if let Some(bare) = model.id.split('/').next_back()
        && bare != model.id.as_str() {
            candidates.push(bare.to_string());
        }

    for candidate in &candidates {
        for provider_data in providers.values() {
            let models_map = match provider_data.get("models").and_then(|m| m.as_object()) {
                Some(m) => m,
                None => continue,
            };
            let model_data = match models_map.get(candidate) {
                Some(m) => m,
                None => continue,
            };

            // Found a match — copy capabilities
            if let Some(cw) = model_data
                .get("limit")
                .and_then(|l| l.get("context"))
                .and_then(|v| v.as_u64())
            {
                model.context_window = Some(cw);
            }
            if let Some(mt) = model_data
                .get("limit")
                .and_then(|l| l.get("output"))
                .and_then(|v| v.as_u64())
            {
                model.max_tokens = Some(mt);
            }
            if let Some(tc) = model_data.get("tool_call").and_then(|v| v.as_bool()) {
                model.tool_call = Some(tc);
            }
            if let Some(r) = model_data.get("reasoning").and_then(|v| v.as_bool()) {
                model.reasoning = Some(r);
            }
            if let Some(a) = model_data.get("attachment").and_then(|v| v.as_bool()) {
                model.attachment = Some(a);
            }
            if let Some(name) = model_data.get("name").and_then(|v| v.as_str()) {
                model.name = name.to_string();
            }
            if let Some(family) = model_data.get("family").and_then(|v| v.as_str()) {
                model.family = Some(family.to_string());
            }
            // Copy modalities (input/output) when not already set
            if model.input_modalities.is_none() {
                model.input_modalities = model_data
                    .get("modalities")
                    .and_then(|v| v.get("input"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    });
            }
            if model.output_modalities.is_none() {
                model.output_modalities = model_data
                    .get("modalities")
                    .and_then(|v| v.get("output"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(|s| s.to_string()))
                            .collect()
                    });
            }
            return; // Found the best match, stop searching
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

// ── Model capabilities lookup (used by gRPC and vault hot-push) ──────────

/// Look up model capabilities for a specific provider + model_id.
/// Uses built-in offline provider data (always available, no network required).
/// Returns None if the model is not found in the offline data.
pub fn lookup_model_capabilities(
    provider: &str,
    model_id: &str,
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    let data = offline_providers();
    lookup_model_capabilities_from_data(&data, provider, model_id)
}

/// Internal helper: look up model capabilities from a JSON data source.
fn lookup_model_capabilities_from_data(
    data: &serde_json::Value,
    provider: &str,
    model_id: &str,
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    // 1. Try exact provider match first
    if let Some((_, models)) = resolve_provider(data, provider)
        && let Some(model) = models.iter().find(|m| m.id == model_id)
    {
        return model_to_capabilities(model);
    }
    // 2. Fallback: cross-provider search by model ID
    //    This handles cases like alibaba-cn proxying moonshotai/kimi-k2.6
    cross_provider_lookup(data, model_id)
}

/// Search all providers for a model matching the given model_id.
/// Strips provider prefix if present (e.g. "moonshotai/kimi-k2.6" -> "kimi-k2.6").
fn cross_provider_lookup(
    data: &serde_json::Value,
    model_id: &str,
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    let bare_id = if model_id.contains('/') {
        model_id.split('/').next_back().unwrap_or(model_id)
    } else {
        model_id
    };

    let providers = data.as_object()?;
    for (pid, provider_data) in providers {
        if let Some(models) = extract_models(provider_data)
            .into_iter()
            .find(|m| m.id == bare_id || m.id == model_id)
        {
            tracing::debug!(
                model_id = model_id,
                found_in_provider = %pid,
                "Cross-provider model capabilities lookup succeeded"
            );
            return model_to_capabilities(&models);
        }
    }
    None
}

// ── Protocol type lookup (used by gRPC to derive protocol from npm field) ──────

/// Derive protocol type from models.dev npm field.
///
/// Mapping rules:
/// - npm contains "anthropic" → ProtocolType::Anthropic
/// - npm contains "google"    → ProtocolType::Google
/// - npm contains "ollama"    → ProtocolType::Ollama
/// - everything else          → ProtocolType::OpenAI (default)
fn derive_protocol_type(npm: Option<&str>) -> acowork_core::protocol::ProtocolType {
    use acowork_core::protocol::ProtocolType;
    match npm {
        Some(s) if s.contains("anthropic") => ProtocolType::Anthropic,
        Some(s) if s.contains("google") => ProtocolType::Google,
        Some(s) if s.contains("ollama") => ProtocolType::Ollama,
        _ => ProtocolType::OpenAI,
    }
}

// ── ADR-103 D1: effective-protocol resolution (the single decision point) ─────

/// ADR-103 D1 — the gate: does the user's model-level choice actually take
/// effect, or does the model fall back to the npm-derived default?
///
/// This is the ONLY arbitration function for the effective wire protocol.
/// Runtime and frontend consume its result and MUST NOT re-derive it
/// (§3 red line 1). The gate lives at *configuration time*: a session never
/// re-probes, and network flapping cannot flip a model's protocol
/// mid-conversation.
///
/// Decision table (D1):
///
/// | `choice` | `probe` | result |
/// |---|---|---|
/// | `Responses` | `verified` / `degraded` | `Responses` (if the global switch is on) |
/// | `Responses` | `failed` / `stale` / `unverified` / absent | `npm_default` |
/// | `None` | any | `npm_default` |
/// | other known value | any | the chosen value (no gate needed) |
/// | `Unknown` (unrecognized wire value) | any | `npm_default` + warn (never guess) |
///
/// Global switch (`llm.responses_api_enabled`, default `false`): when off,
/// a `Responses` selection is forced to `npm_default` — the fallback MUST be
/// visible (warn), otherwise the switch itself would be a silent protocol
/// change (§4.4).
pub fn resolve_effective_protocol(
    choice: Option<&acowork_core::protocol::ProtocolType>,
    probe: Option<&acowork_core::protocol::ProtocolProbe>,
    npm_default: acowork_core::protocol::ProtocolType,
    responses_api_enabled: bool,
) -> acowork_core::protocol::ProtocolType {
    use acowork_core::protocol::ProtocolType;

    let Some(chosen) = choice else {
        return npm_default; // no explicit selection → npm derivation (status quo)
    };

    match chosen {
        ProtocolType::Responses => {
            if !responses_api_enabled {
                tracing::warn!(
                    "resolve_protocol: model selected 'responses' but llm.responses_api_enabled=false; \
                     forcing fallback to the npm-derived default '{}'. Restart the Gateway with the \
                     switch enabled to honour the selection.",
                    npm_default
                );
                return npm_default;
            }
            match probe {
                // Probe verdict written by the save/probe path (D12) and
                // still fresh: enable Responses.
                Some(p) if p.gates_responses() => ProtocolType::Responses,
                // Failed / stale / unverified / absent / unrecognized →
                // fall back to the default. The fallback itself is visible
                // via the UI yellow badge (D11); it is NOT a silent fallback.
                other => {
                    tracing::info!(
                        probe_state = ?other.map(|p| &p.state),
                        "resolve_protocol: 'responses' selected but probe does not pass the gate; \
                         falling back to the npm-derived default '{}'",
                        npm_default
                    );
                    npm_default
                }
            }
        }
        // An unrecognized wire value: never guess a concrete protocol from
        // it — fall back to the npm-derived default (D1 MUST).
        ProtocolType::Unknown => {
            tracing::warn!(
                "resolve_protocol: model-level protocol_type is an unrecognized wire value \
                 (deserialized to Unknown); falling back to the npm-derived default '{}'",
                npm_default
            );
            npm_default
        }
        // Anthropic / Google / Ollama / OpenAI: explicit selections take
        // effect directly (probe does not apply, no gate).
        other => other.clone(),
    }
}

/// ADR-103 D2 — resolve the effective protocol (and per-model base URL
/// override, when the Responses shape is in effect) for one model of a
/// cached provider entry.
///
/// `npm_default` is the provider-level `protocol_type` already derived from
/// offline models.dev data when the provider entry was built. The
/// per-model `protocol_type` / `protocol_probe` fields carry the user's
/// selection and the Gateway probe verdict; `responses_api_enabled` is the
/// global kill switch (§4.3/§4.4).
///
/// Returns `(effective_protocol, model_base_url_override)`; the override is
/// `Some` only when the effective protocol is `Responses` AND the model
/// entry carries a non-empty `base_url`.
pub fn resolve_protocol(
    provider: &acowork_core::protocol::ProviderListItem,
    model_id: &str,
    responses_api_enabled: bool,
) -> (
    acowork_core::protocol::ProtocolType,
    Option<String>,
) {
    use acowork_core::protocol::ProtocolType;

    let npm_default = provider.protocol_type.clone();
    let Some(model) = provider.models.iter().find(|m| m.id == model_id) else {
        // Model entry not found: fall back to provider level, never guess (D2).
        tracing::warn!(
            provider_id = %provider.id,
            model_id = %model_id,
            "resolve_protocol: model entry not found in provider list; inheriting provider-level protocol"
        );
        return (npm_default, None);
    };

    let effective = resolve_effective_protocol(
        model.protocol_type.as_ref(),
        model.protocol_probe.as_ref(),
        npm_default,
        responses_api_enabled,
    );
    let base_url = if effective == ProtocolType::Responses {
        model
            .base_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string())
    } else {
        None
    };
    (effective, base_url)
}

/// Look up protocol info for a provider+model combination using offline data.
///
/// Returns (protocol_type, api_override):
/// - protocol_type: derived from npm field (model-level > provider-level > default OpenAI)
/// - api_override: model-level api URL override if present
pub fn lookup_protocol_info(
    provider_id: &str,
    model_id: Option<&str>,
) -> (acowork_core::protocol::ProtocolType, Option<String>) {
    let data = offline_providers();
    lookup_protocol_info_from_data(&data, provider_id, model_id)
}

/// Internal helper: look up protocol info from a JSON data source.
///
/// Priority: local provider registry > model-level npm > provider-level npm > default OpenAI.
/// Model-level api override is returned when present in the model's provider block.
fn lookup_protocol_info_from_data(
    data: &serde_json::Value,
    provider_id: &str,
    model_id: Option<&str>,
) -> (acowork_core::protocol::ProtocolType, Option<String>) {
    use acowork_core::protocol::ProtocolType;

    let providers = match data.as_object() {
        Some(obj) => obj,
        None => {
            // Check local provider registry when no offline data available
            if is_local_provider(provider_id) {
                let proto = local_protocol_type(provider_id);
                let url = local_provider_default_url(provider_id).map(|s| s.to_string());
                return (proto, url);
            }
            return (ProtocolType::OpenAI, None);
        }
    };

    // Find the provider object
    let provider_obj = match providers.get(provider_id) {
        Some(p) => p,
        None => {
            // Check local provider registry when not found in offline data
            if is_local_provider(provider_id) {
                let proto = local_protocol_type(provider_id);
                let url = local_provider_default_url(provider_id).map(|s| s.to_string());
                return (proto, url);
            }
            return (ProtocolType::OpenAI, None);
        }
    };

    // Provider-level npm
    let provider_npm = provider_obj.get("npm").and_then(|v| v.as_str());

    // If model_id provided, check model-level override
    if let Some(mid) = model_id
        && let Some(models) = provider_obj.get("models").and_then(|m| m.as_object())
        && let Some(model_obj) = models.get(mid)
        && let Some(model_provider) = model_obj.get("provider").and_then(|p| p.as_object())
    {
        let model_npm = model_provider.get("npm").and_then(|v| v.as_str());
        let model_api = model_provider
            .get("api")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        // Model-level npm takes precedence
        if model_npm.is_some() {
            return (derive_protocol_type(model_npm), model_api);
        }

        // Model has provider block but no npm → use provider-level npm + model api
        if model_api.is_some() {
            return (derive_protocol_type(provider_npm), model_api);
        }
    }

    // Fall back to provider-level npm + provider-level api
    let provider_api = provider_obj
        .get("api")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    (derive_protocol_type(provider_npm), provider_api)
}

/// Convert a ModelInfo to ModelCapabilitiesInfo
fn model_to_capabilities(
    model: &ModelInfo,
) -> Option<acowork_core::protocol::ModelCapabilitiesInfo> {
    Some(acowork_core::protocol::ModelCapabilitiesInfo {
        context_window: model.context_window.unwrap_or(0),
        max_output_tokens: model.max_tokens.unwrap_or(0),
        max_input_tokens: model.max_input_tokens,
        supports_tool_calling: model.tool_call.unwrap_or(true),
        supports_reasoning: model.reasoning,
        supports_attachment: model.attachment,
        supports_temperature: model.temperature,
        cost: match (model.input_cost, model.output_cost) {
            (Some(inp), Some(out)) => Some(acowork_core::protocol::ModelCostInfo {
                input_per_million: Some(inp),
                output_per_million: Some(out),
            }),
            (Some(inp), None) => Some(acowork_core::protocol::ModelCostInfo {
                input_per_million: Some(inp),
                output_per_million: None,
            }),
            (None, Some(out)) => Some(acowork_core::protocol::ModelCostInfo {
                input_per_million: None,
                output_per_million: Some(out),
            }),
            (None, None) => None,
        },
        modalities: match (&model.input_modalities, &model.output_modalities) {
            (Some(inp), Some(out)) => Some(acowork_core::protocol::ModelModalities {
                input: inp.clone(),
                output: out.clone(),
            }),
            _ => None,
        },
        name: Some(model.name.clone()),
        family: model.family.clone(),
        knowledge_cutoff: model.knowledge.clone(),
        default_reasoning_effort: None,
        thinking_mode: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::{
        ModelCapabilitiesInfo, ProtocolType, ProviderListItem, ProviderModelEntry,
    };

    /// Capabilities built from JSON so the test does not have to spell out
    /// every optional field of `ModelCapabilitiesInfo`.
    fn caps(context_window: u64) -> ModelCapabilitiesInfo {
        serde_json::from_value(serde_json::json!({
            "context_window": context_window,
            "max_output_tokens": 4096,
        }))
        .unwrap()
    }

    fn provider(id: &str, custom: bool, models: &[&str]) -> ProviderListItem {
        ProviderListItem {
            id: id.to_string(),
            base_url: "http://localhost:1234/v1".to_string(),
            protocol_type: ProtocolType::OpenAI,
            models: models
                .iter()
                .map(|m| ProviderModelEntry {
                    id: m.to_string(),
                    capabilities: caps(1000),
                    max_output_tokens_limit: 4096,
                    protocol_type: None,
                    base_url: None,
                    protocol_probe: None,
                })
                .collect(),
            compact_model: None,
            custom,
        }
    }

    /// A catalog refresh must reach remote providers, and only those.
    #[test]
    fn resync_capabilities_refreshes_remote_providers_only() {
        let mut providers = vec![
            // Stored entry for gpt-4o was captured when the catalog had 1000.
            provider("openai", false, &["gpt-4o", "not-in-catalog"]),
            // Local models run on the user's hardware; caps are calibrated in the UI.
            provider("lmstudio", false, &["llama3"]),
            // Custom endpoints are not in the catalog at all.
            provider("my-endpoint", true, &["gpt-4o"]),
        ];
        // Pretend the catalog knows every id — including the local model and
        // the custom endpoint. The skip rules must still win for those.
        let lookup = |pid: &str, mid: &str| match (pid, mid) {
            (_, "not-in-catalog") => None,
            _ => Some(caps(2000)),
        };

        assert_eq!(resync_capabilities_in_place(&mut providers, lookup), 1);
        assert_eq!(providers[0].models[0].capabilities.context_window, 2000);
        // Model unknown to the catalog keeps the value it was configured with.
        assert_eq!(providers[0].models[1].capabilities.context_window, 1000);
        assert_eq!(providers[1].models[0].capabilities.context_window, 1000);
        assert_eq!(providers[2].models[0].capabilities.context_window, 1000);

        // Already in sync → no persistence, no version bump, no MQTT push.
        assert_eq!(resync_capabilities_in_place(&mut providers, lookup), 0);
    }

    #[test]
    fn test_offline_providers_loads() {
        let data = offline_providers();
        assert!(data.is_object(), "offline_providers must be a JSON object");

        let obj = data.as_object().unwrap();
        // Verify all expected providers exist
        let expected = [
            "openai",
            "anthropic",
            "google",
            "deepseek",
            "minimax",
            "minimax-cn",
            "zhipuai",
            "moonshotai",
            "moonshotai-cn",
            "alibaba",
            "alibaba-cn",
            "groq",
            "mistral",
            "xai",
            "openrouter",
            "azure",
            "lmstudio",
        ];
        for pid in &expected {
            assert!(obj.contains_key(*pid), "Missing provider: {}", pid);
        }
    }

    #[test]
    fn test_offline_provider_has_required_fields() {
        let data = offline_providers();
        let openai = &data["openai"];
        assert!(openai.get("id").is_some(), "provider must have 'id'");
        assert!(openai.get("name").is_some(), "provider must have 'name'");
        assert!(
            openai.get("models").is_some(),
            "provider must have 'models'"
        );

        // Check a model has required fields
        let models = openai["models"].as_object().unwrap();
        let (_, first_model) = models.iter().next().unwrap();
        assert!(first_model.get("id").is_some(), "model must have 'id'");
        assert!(first_model.get("name").is_some(), "model must have 'name'");
        assert!(
            first_model.get("family").is_some(),
            "model must have 'family'"
        );
        assert!(
            first_model.get("reasoning").is_some(),
            "model must have 'reasoning'"
        );
        assert!(
            first_model.get("attachment").is_some(),
            "model must have 'attachment'"
        );
        assert!(
            first_model.get("tool_call").is_some(),
            "model must have 'tool_call'"
        );
        assert!(
            first_model.get("limit").is_some(),
            "model must have 'limit'"
        );
    }

    #[test]
    fn test_offline_provider_has_api_field_when_expected() {
        let data = offline_providers();
        // Providers that should have an api field in the source data
        let providers_with_api = [
            "deepseek",
            "minimax",
            "minimax-cn",
            "zhipuai",
            "moonshotai",
            "moonshotai-cn",
            "alibaba",
            "alibaba-cn",
            "openrouter",
            "lmstudio",
        ];
        for pid in &providers_with_api {
            let provider = &data[pid];
            assert!(
                provider.get("api").is_some(),
                "provider '{}' should have 'api' field",
                pid
            );
        }
    }

    #[test]
    fn test_provider_ids_to_query_simple() {
        assert_eq!(provider_ids_to_query("openai"), vec!["openai".to_string()]);
        assert_eq!(
            provider_ids_to_query("anthropic"),
            vec!["anthropic".to_string()]
        );
    }

    #[test]
    fn test_provider_ids_to_query_cn_variants() {
        assert_eq!(
            provider_ids_to_query("minimax"),
            vec!["minimax".to_string(), "minimax-cn".to_string()]
        );
        assert_eq!(
            provider_ids_to_query("alibaba"),
            vec!["alibaba".to_string(), "alibaba-cn".to_string()]
        );
    }

    #[test]
    fn test_provider_ids_to_query_zhipuai() {
        // zhipuai has both -cn and -coding-plan variants
        assert_eq!(
            provider_ids_to_query("zhipuai"),
            vec![
                "zhipuai".to_string(),
                "zhipuai-cn".to_string(),
                "zhipuai-coding-plan".to_string(),
            ]
        );
    }

    #[test]
    fn test_provider_ids_to_query_unknown() {
        // Unknown providers get no extra variants
        assert_eq!(
            provider_ids_to_query("some-new-provider"),
            vec!["some-new-provider".to_string()]
        );
    }

    #[test]
    fn test_extract_models() {
        let provider_data = serde_json::json!({
            "name": "Test Provider",
            "models": {
                "model-a": {
                    "name": "Model A",
                    "reasoning": true,
                    "tool_call": true,
                    "attachment": false,
                    "release_date": "2025-01-01"
                },
                "model-b": {
                    "name": "Model B",
                    "tool_call": true
                }
            }
        });

        let models = extract_models(&provider_data);
        assert_eq!(models.len(), 2);
        // reasoning model should come first
        assert_eq!(models[0].id, "model-a");
        assert!(models[0].reasoning.unwrap_or(false));
    }

    #[test]
    fn test_extract_models_offline_format() {
        // Backward-compat: extract_models handles sparse model data gracefully
        let provider_data = serde_json::json!({
            "name": "Test Offline",
            "models": {
                "model-x": {
                    "id": "model-x",
                    "name": "Model X",
                    "tool_call": true,
                    "limit": { "context": 128000, "output": 4096 }
                }
            }
        });

        let models = extract_models(&provider_data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "model-x");
        assert_eq!(models[0].context_window, Some(128000));
        assert_eq!(models[0].max_tokens, Some(4096));
        assert!(models[0].tool_call.unwrap_or(false));
        // Fields not present should be None
        assert!(models[0].family.is_none());
        assert!(models[0].reasoning.is_none());
        assert!(models[0].attachment.is_none());
        assert!(models[0].release_date.is_none());
    }

    #[test]
    fn test_extract_models_with_new_fields() {
        // Verify extract_models reads family, reasoning, attachment from offline data
        let provider_data = serde_json::json!({
            "name": "Test Provider",
            "models": {
                "model-y": {
                    "id": "model-y",
                    "name": "Model Y",
                    "family": "test-family",
                    "reasoning": true,
                    "tool_call": true,
                    "attachment": false,
                    "limit": { "context": 64000, "output": 8192 }
                }
            }
        });

        let models = extract_models(&provider_data);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "model-y");
        assert_eq!(models[0].family, Some("test-family".to_string()));
        assert_eq!(models[0].reasoning, Some(true));
        assert_eq!(models[0].attachment, Some(false));
        assert_eq!(models[0].context_window, Some(64000));
        assert_eq!(models[0].max_tokens, Some(8192));
    }

    #[test]
    fn test_resolve_provider_from_offline() {
        let data = offline_providers();
        let result = resolve_provider(&data, "openai");
        assert!(result.is_some());
        let (name, models) = result.unwrap();
        assert!(!name.is_empty());
        assert!(!models.is_empty());
    }

    #[test]
    fn test_resolve_provider_cn_variant() {
        let data = offline_providers();
        // minimax should resolve both minimax and minimax-cn
        let result = resolve_provider(&data, "minimax");
        assert!(result.is_some());
        let (_, models) = result.unwrap();
        assert!(
            !models.is_empty(),
            "minimax should have models from both variants"
        );
    }

    #[test]
    fn test_resolve_provider_not_found() {
        let data = offline_providers();
        let result = resolve_provider(&data, "nonexistent-provider");
        assert!(result.is_none());
    }

    #[test]
    fn test_build_offline_file_candidates_data_dir_first() {
        let data_dir = std::path::Path::new("/tmp/fake-data-dir");
        let cands = build_offline_file_candidates(Some(data_dir));
        // The very first candidate must be the data_dir-joined file, so a
        // refreshed catalog always wins over the bundled baseline.
        assert!(
            cands.first().unwrap().starts_with(data_dir),
            "expected first candidate to live under data_dir, got {:?}",
            cands.first()
        );
        assert!(cands[0].ends_with("offline_providers.json"));
    }

    #[test]
    fn test_catalog_source_url_default_and_env_override() {
        // Default: hard-coded models.dev URL when env var unset.
        // SAFETY: tests in this module run on the same thread; we set a
        // process-scoped env var briefly and restore it to avoid leaking
        // state into other tests.
        let prev = std::env::var("ACOWORK_CATALOG_URL").ok();
        // SAFETY: see note above. Tests are single-threaded w.r.t. env access.
        unsafe {
            std::env::remove_var("ACOWORK_CATALOG_URL");
        }
        assert_eq!(catalog_source_url(), "https://models.dev/catalog.json");
        // Override: env var wins.
        // SAFETY: see note above.
        unsafe {
            std::env::set_var("ACOWORK_CATALOG_URL", "https://mirror.example/catalog.json");
        }
        assert_eq!(catalog_source_url(), "https://mirror.example/catalog.json");
        // Restore.
        match prev {
            Some(v) => unsafe {
                std::env::set_var("ACOWORK_CATALOG_URL", v);
            },
            None => unsafe {
                std::env::remove_var("ACOWORK_CATALOG_URL");
            },
        }
    }

    #[test]
    fn test_catalog_adapt_keeps_providers_object() {
        // Mirrors the adapt step `load_offline_providers_from_file` and
        // `refresh_catalog` both perform: drop the wrapper, keep `.providers`
        // as the new root object.
        let raw = serde_json::json!({
            "models": {"foo": {"id": "foo"}},
            "providers": {"acme": {"id": "acme", "models": {"x": {"id": "x"}}}}
        });
        let adapted = match raw.get("providers") {
            Some(serde_json::Value::Object(p)) => serde_json::Value::Object(p.clone()),
            _ => raw.clone(),
        };
        let obj = adapted.as_object().expect("object");
        assert!(obj.contains_key("acme"));
        assert!(!obj.contains_key("models"));
        // Now go through the consumer helper to prove end-to-end shape.
        // `extract_models` expects a single-provider object: drill into `acme`.
        let models = extract_models(&adapted["acme"]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "x");
    }

    #[test]
    fn test_wants_sse_dispatch() {
        // No Accept header → JSON (default).
        let h = axum::http::HeaderMap::new();
        assert!(!wants_sse(&h));
        // Explicit JSON Accept → JSON.
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::ACCEPT, "application/json".parse().unwrap());
        assert!(!wants_sse(&h));
        // SSE Accept → SSE.
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::ACCEPT,
            "text/event-stream".parse().unwrap(),
        );
        assert!(wants_sse(&h));
        // Mixed Accept where SSE appears → SSE (browser-style "q=" ok too).
        let mut h = axum::http::HeaderMap::new();
        h.insert(
            axum::http::header::ACCEPT,
            "application/json, text/event-stream".parse().unwrap(),
        );
        assert!(wants_sse(&h));
    }
}

// ── ADR-103 S1: D1 gate decision-table tests ──────────────────────────

#[cfg(test)]
mod adr103_resolve_tests {
    use super::resolve_effective_protocol;
    use super::resolve_protocol;
    use acowork_core::protocol::{
        ModelCapabilitiesInfo, ProbeState, ProtocolProbe, ProtocolType, ProviderListItem,
        ProviderModelEntry,
    };

    fn probe(state: ProbeState) -> ProtocolProbe {
        ProtocolProbe {
            state,
            tier_passed: vec!["L1".into(), "L2".into(), "L3".into()],
            reason: None,
            observed_base_url: None,
            probed_at: None,
        }
    }

    fn entry(
        choice: Option<ProtocolType>,
        pr: Option<ProtocolProbe>,
        base_url: Option<&str>,
    ) -> ProviderModelEntry {
        serde_json::from_value::<ProviderModelEntry>(serde_json::json!({
            "id": "gpt-5",
            "capabilities": { "context_window": 1000u64, "max_output_tokens": 100u64 },
            "max_output_tokens_limit": 4096u64,
            "protocol_type": choice,
            "protocol_probe": pr,
            "base_url": base_url,
        }))
        .unwrap()
    }

    fn cached(m: ProviderModelEntry, provider_proto: ProtocolType) -> ProviderListItem {
        ProviderListItem {
            id: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol_type: provider_proto,
            models: vec![m],
            compact_model: None,
            custom: false,
        }
    }

    const ON: bool = true;
    const OFF: bool = false;

    #[test]
    fn no_choice_always_npm_default() {
        for probe in [None, Some(probe(ProbeState::Verified))] {
            assert_eq!(
                resolve_effective_protocol(None, probe.as_ref(), ProtocolType::Anthropic, ON),
                ProtocolType::Anthropic
            );
        }
    }

    #[test]
    fn responses_choice_gated_by_probe_state() {
        // verified / degraded pass the gate; everything else falls back.
        for (st, want) in [
            (ProbeState::Verified, ProtocolType::Responses),
            (ProbeState::Degraded, ProtocolType::Responses),
            (ProbeState::Failed, ProtocolType::OpenAI),
            (ProbeState::Stale, ProtocolType::OpenAI),
            (ProbeState::Unverified, ProtocolType::OpenAI),
            (ProbeState::Unknown, ProtocolType::OpenAI),
        ] {
            let got = resolve_effective_protocol(
                Some(&ProtocolType::Responses),
                Some(&probe(st.clone())),
                ProtocolType::OpenAI,
                ON,
            );
            assert_eq!(got, want, "probe state {st:?}");
        }
        // Absent probe = unverified = off.
        assert_eq!(
            resolve_effective_protocol(Some(&ProtocolType::Responses), None, ProtocolType::OpenAI, ON),
            ProtocolType::OpenAI
        );
    }

    #[test]
    fn kill_switch_forces_verified_responses_back_to_default() {
        // §4.4: switch off = pre-ADR-103 behavior even with a fresh verified probe.
        assert_eq!(
            resolve_effective_protocol(
                Some(&ProtocolType::Responses),
                Some(&probe(ProbeState::Verified)),
                ProtocolType::OpenAI,
                OFF,
            ),
            ProtocolType::OpenAI
        );
    }

    #[test]
    fn unknown_choice_never_guesses() {
        assert_eq!(
            resolve_effective_protocol(
                Some(&ProtocolType::Unknown),
                Some(&probe(ProbeState::Verified)),
                ProtocolType::Google,
                ON,
            ),
            ProtocolType::Google
        );
    }

    #[test]
    fn other_known_choices_take_effect_directly() {
        // Non-Responses selections are not gated (D1 table row 4).
        for p in [ProtocolType::Anthropic, ProtocolType::Google, ProtocolType::Ollama, ProtocolType::OpenAI] {
            assert_eq!(
                resolve_effective_protocol(Some(&p), None, ProtocolType::OpenAI, OFF),
                p.clone(),
                "choice {p}"
            );
        }
    }

    #[test]
    fn resolve_protocol_model_entry_missing_inherits_provider_level() {
        let p = cached(entry(None, None, None), ProtocolType::Ollama);
        let (proto, base) = resolve_protocol(&p, "not-in-list", ON);
        assert_eq!(proto, ProtocolType::Ollama);
        assert_eq!(base, None);
    }

    #[test]
    fn resolve_protocol_base_override_only_for_effective_responses() {
        let url = "https://api.minimax.io/v1";
        // Effective Responses + non-empty override → Some.
        let p = cached(
            entry(Some(ProtocolType::Responses), Some(probe(ProbeState::Verified)), Some(url)),
            ProtocolType::Anthropic,
        );
        let (proto, base) = resolve_protocol(&p, "gpt-5", ON);
        assert_eq!(proto, ProtocolType::Responses);
        assert_eq!(base.as_deref(), Some(url));
        // Gate closed (switch off) → override is NOT carried.
        let (proto, base) = resolve_protocol(&p, "gpt-5", OFF);
        assert_eq!(proto, ProtocolType::Anthropic);
        assert_eq!(base, None);
        // Empty-string override → None (inherit provider base).
        let p2 = cached(
            entry(Some(ProtocolType::Responses), Some(probe(ProbeState::Degraded)), Some("")),
            ProtocolType::OpenAI,
        );
        let (proto, base) = resolve_protocol(&p2, "gpt-5", ON);
        assert_eq!(proto, ProtocolType::Responses);
        assert_eq!(base, None);
    }

    #[test]
    fn resolve_protocol_status_quo_unchanged() {
        // S1 zero-traffic-change invariant: an entry with no model-level
        // fields resolves to exactly the provider-level protocol, with no
        // base_url override — regardless of the switch.
        let p = cached(entry(None, None, None), ProtocolType::Anthropic);
        for enabled in [ON, OFF] {
            let (proto, base) = resolve_protocol(&p, "gpt-5", enabled);
            assert_eq!(proto, ProtocolType::Anthropic);
            assert_eq!(base, None);
        }
    }

    #[test]
    fn model_capabilities_info_field_shape() {
        // Guard: `caps()`-style JSON construction keeps compiling if the
        // struct grows required fields.
        let _: ModelCapabilitiesInfo = serde_json::from_value(serde_json::json!({
            "context_window": 1u64, "max_output_tokens": 1u64
        }))
        .unwrap();
    }
}
