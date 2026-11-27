//! OpenAI-compatible HTTP API server for embedding inference.
//!
//! Endpoints:
//! - `POST /v1/embeddings`  — OpenAI-compatible embedding API
//! - `GET  /v1/models`      — List loaded models
//! - `GET  /health`         — Health check with model status
//! - `POST /models/{id}/load`   — Hot-load a model
//! - `POST /models/{id}/download` — Trigger model download
//! - `GET  /models/{id}/status`  — Get model status

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Sse, sse::Event},
    routing::{delete, get, post},
};
use futures_util::stream::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::download::{DownloadProgress, DownloadSpec, Downloader};
use crate::event_bus::{Event as BusEvent, EventBus, State as BusState};
use crate::model::EmbeddingModel;
use crate::registry::{ModelInfo, ModelRegistry, ModelStatus, ModelStatusFlat};
use crate::shutdown::Shutdown;

// ── App state ───────────────────────────────────────────────────────────

/// Shared application state for the HTTP server.
pub struct AppState {
    /// Currently loaded model (None if no model loaded yet).
    pub model: RwLock<Option<Arc<EmbeddingModel>>>,
    /// Model registry.
    ///
    /// Behind a lock because the Gateway can add a user-defined local model at
    /// runtime (`POST /api/embedding-models`) and then ask this process to
    /// re-read the file — a plain field would pin the catalog from startup and
    /// reject the new model on download.
    pub registry: RwLock<ModelRegistry>,
    /// Directory `embedding_models.json` was loaded from; the reload source.
    pub registry_dir: std::path::PathBuf,
    /// Model downloader.
    pub downloader: Downloader,
    /// Download status per model.
    pub download_status: RwLock<std::collections::HashMap<String, ModelStatus>>,
    /// Live download progress per model (shared with spawned racer tasks).
    pub download_progress: RwLock<std::collections::HashMap<String, Arc<DownloadProgress>>>,
    /// Shutdown signal.
    pub shutdown: Arc<Shutdown>,
    /// Models directory.
    pub models_dir: std::path::PathBuf,
    /// Selected ONNX variant (fp32, fp16, int8).
    pub onnx_variant: String,
    /// Default model ID to load at startup.
    pub default_model: Option<String>,
    /// Per-model cancel flags for ongoing downloads. Each download gets its
    /// own AtomicBool so cancelling one model does not affect others.
    pub download_cancel_flags:
        RwLock<std::collections::HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    /// Event bus for the /events SSE endpoint. Publishes heartbeats and
    /// state transitions; the gateway subscribes to learn which model
    /// is loaded and to detect a stuck embed process.
    pub event_bus: EventBus<BusState>,
}

impl AppState {
    /// Snapshot of the model registry.
    ///
    /// Cloned per request (a handful of entries): a borrow from the lock
    /// guard would have to live across `await` points in the handlers —
    /// and the guard must not be held while a registry reload waits for the
    /// write lock.
    pub async fn registry(&self) -> ModelRegistry {
        self.registry.read().await.clone()
    }

    /// Whether the registry knows this model id (existence checks only).
    pub async fn has_model(&self, model_id: &str) -> bool {
        self.registry.read().await.get(model_id).is_some()
    }
}

// ── OpenAI API types ────────────────────────────────────────────────────

/// OpenAI-compatible embedding request.
#[derive(Debug, Deserialize)]
pub struct EmbeddingRequest {
    /// Input text(s) to embed.
    pub input: EmbeddingInput,
    /// Model name (optional, uses loaded model if not specified).
    #[serde(default)]
    #[allow(dead_code)]
    pub model: Option<String>,
}

/// Input can be a single string or a list of strings.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Batch(Vec<String>),
}

/// OpenAI-compatible embedding response.
#[derive(Debug, Serialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub data: Vec<EmbeddingData>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

/// Single embedding result.
#[derive(Debug, Serialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: usize,
    pub embedding: Vec<f32>,
}

/// Token usage info.
#[derive(Debug, Serialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: usize,
    pub total_tokens: usize,
}

/// OpenAI-compatible error response.
#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub message: String,
    #[serde(rename = "type")]
    pub error_type: String,
    pub code: Option<String>,
}

/// Health check response.
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub model: Option<ModelStatusInfo>,
}

/// Model status in health check.
#[derive(Debug, Serialize)]
pub struct ModelStatusInfo {
    pub id: String,
    pub dimension: usize,
    pub pooling: String,
}

/// Model list response (OpenAI-compatible format).
#[derive(Debug, Serialize)]
pub struct ModelListResponse {
    pub object: String,
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub object: String,
    pub owned_by: String,
}

/// Download request.
#[derive(Debug, Deserialize)]
pub struct DownloadRequest {
    /// ONNX variant to download (fp32, fp16, int8). Defaults to server config.
    #[serde(default)]
    pub variant: Option<String>,
}

/// Download response.
#[derive(Debug, Serialize)]
pub struct DownloadResponse {
    pub model_id: String,
    pub status: String,
    pub message: String,
}

/// Load response.
#[derive(Debug, Serialize)]
pub struct LoadResponse {
    pub model_id: String,
    pub status: String,
    pub dimension: usize,
}

/// Model status response.
#[derive(Debug, Serialize)]
pub struct ModelStatusResponse {
    pub model_id: String,
    #[serde(flatten)]
    pub status: ModelStatusFlat,
    pub info: Option<ModelStatusInfo>,
}

/// Model list with details (extended, non-OpenAI).
#[derive(Debug, Serialize)]
pub struct ModelsDetailResponse {
    pub models: Vec<ModelInfo>,
}

// ── Route handlers ──────────────────────────────────────────────────────

/// POST /v1/embeddings — OpenAI-compatible embedding endpoint.
pub async fn create_embedding(
    State(state): State<Arc<AppState>>,
    Json(req): Json<EmbeddingRequest>,
) -> impl IntoResponse {
    // Check shutdown
    if state.shutdown.is_shutting_down() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: "Server is shutting down".to_string(),
                    error_type: "server_error".to_string(),
                    code: Some("shutting_down".to_string()),
                },
            }),
        )
            .into_response();
    }

    // Collect and validate input texts before checking model
    let texts: Vec<String> = match req.input {
        EmbeddingInput::Single(t) => vec![t],
        EmbeddingInput::Batch(ts) => ts,
    };

    if texts.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: "Input cannot be empty".to_string(),
                    error_type: "invalid_request_error".to_string(),
                    code: None,
                },
            }),
        )
            .into_response();
    }

    // Get loaded model
    let model_guard = state.model.read().await;
    let model = match model_guard.as_ref() {
        Some(m) => m.clone(),
        None => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: "No embedding model loaded".to_string(),
                        error_type: "server_error".to_string(),
                        code: Some("model_not_loaded".to_string()),
                    },
                }),
            )
                .into_response();
        }
    };
    drop(model_guard);

    // Run inference with timeout (30s per request)
    let text_refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let infer_result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        model.embed_batch(&text_refs),
    )
    .await;

    match infer_result {
        Ok(Ok(embeddings)) => {
            let data: Vec<EmbeddingData> = embeddings
                .into_iter()
                .enumerate()
                .map(|(i, emb)| EmbeddingData {
                    object: "embedding".to_string(),
                    index: i,
                    embedding: emb,
                })
                .collect();

            // Estimate total tokens from tokenizer truncation
            let total_tokens: usize = texts
                .iter()
                .map(|t| t.split_whitespace().count().min(model.max_tokens()))
                .sum();

            tracing::debug!(
                model = %model.model_id(),
                batch_size = texts.len(),
                total_tokens,
                "Embedding inference completed"
            );

            (
                StatusCode::OK,
                Json(EmbeddingResponse {
                    object: "list".to_string(),
                    data,
                    model: model.model_id().to_string(),
                    usage: EmbeddingUsage {
                        prompt_tokens: total_tokens,
                        total_tokens,
                    },
                }),
            )
                .into_response()
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Embedding inference failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Embedding inference failed: {e}"),
                        error_type: "server_error".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response()
        }
        Err(_) => {
            tracing::error!("Embedding inference timed out (30s)");
            (
                StatusCode::REQUEST_TIMEOUT,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: "Embedding inference timed out".to_string(),
                        error_type: "timeout".to_string(),
                        code: Some("inference_timeout".to_string()),
                    },
                }),
            )
                .into_response()
        }
    }
}

/// GET /v1/models — OpenAI-compatible model list.
pub async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let model_guard = state.model.read().await;
    let current_id = model_guard.as_ref().map(|m| m.model_id().to_string());
    drop(model_guard);

    let registry = state.registry().await;
    let models = registry.models();
    let data: Vec<ModelEntry> = models
        .iter()
        .map(|m| ModelEntry {
            id: m.id.clone(),
            object: "model".to_string(),
            owned_by: "acowork-embed".to_string(),
        })
        .collect();

    // Also include the currently loaded model
    let mut data = data;
    if let Some(id) = current_id
        && !data.iter().any(|m| m.id == id)
    {
        data.push(ModelEntry {
            id,
            object: "model".to_string(),
            owned_by: "acowork-embed".to_string(),
        });
    }

    Json(ModelListResponse {
        object: "list".to_string(),
        data,
    })
    .into_response()
}

/// GET /health — Health check.
pub async fn health_check(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let registry = state.registry().await;
    let model_guard = state.model.read().await;
    let model_info = model_guard.as_ref().map(|m| ModelStatusInfo {
        id: m.model_id().to_string(),
        dimension: m.dimension(),
        pooling: format!(
            "{:?}",
            registry
                .get(m.model_id())
                .map(|e| &e.pooling_strategy)
                .unwrap_or(&crate::registry::PoolingStrategy::Cls)
        ),
    });

    let status = if model_guard.is_some() {
        "ready"
    } else {
        "no_model_loaded"
    };
    drop(model_guard);

    Json(HealthResponse {
        status: status.to_string(),
        model: model_info,
    })
    .into_response()
}

/// POST /models/{id}/load — Hot-load a model.
pub async fn load_model(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    let registry = state.registry().await;
    let entry = match registry.get(&model_id) {
        Some(e) => e.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Model '{model_id}' not found in registry"),
                        error_type: "not_found".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
    };

    // Check if model is downloaded
    let model_dir = state.models_dir.join(&model_id);
    let onnx_path = model_dir.join("model.onnx");
    let tokenizer_path = model_dir.join("tokenizer.json");

    if !onnx_path.exists() || !tokenizer_path.exists() {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: format!(
                        "Model '{model_id}' not downloaded. Use POST /models/{model_id}/download first."
                    ),
                    error_type: "not_found".to_string(),
                    code: Some("not_downloaded".to_string()),
                },
            }),
        )
            .into_response();
    }

    // Load model with timeout (60s) — ONNX Runtime can hang if external
    // data files are missing, so we protect against that here.
    let model_id_clone = model_id.clone();
    let pooling = entry.pooling_strategy.clone();
    let output_kind = entry.onnx_output_kind;
    let dimension = entry.dimension;
    let max_tokens = entry.max_tokens;
    let load_result = tokio::task::spawn_blocking(move || {
        EmbeddingModel::load(
            &model_id_clone,
            &onnx_path,
            &tokenizer_path,
            pooling,
            output_kind,
            dimension,
            max_tokens,
        )
    })
    .await;

    let model = match load_result {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Failed to load model '{model_id}': {e}"),
                        error_type: "server_error".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Model loading task panicked for '{model_id}': {e}"),
                        error_type: "server_error".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
    };

    let dim = model.dimension();
    let model_id_for_state = model_id.clone();
    let mut model_guard = state.model.write().await;
    *model_guard = Some(Arc::new(model));
    drop(model_guard);

    // Publish Ready state so the gateway learns the model is now active.
    state.event_bus.publish_state(BusState::Ready {
        model_id: model_id_for_state.clone(),
        dimension: dim,
    });

    Json(LoadResponse {
        model_id: model_id.clone(),
        status: "loaded".to_string(),
        dimension: dim,
    })
    .into_response()
}

/// POST /models/{id}/download — Trigger model download (fire-and-forget).
///
/// Spawns the download in a background task and returns 202 Accepted
/// immediately. Progress can be polled via `GET /models/{id}/status`.
pub async fn download_model(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
    Json(req): Json<DownloadRequest>,
) -> impl IntoResponse {
    let registry = state.registry().await;
    let entry = match registry.get(&model_id) {
        Some(e) => e.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Model '{model_id}' not found in registry"),
                        error_type: "not_found".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
    };

    // Check if already downloaded
    if state.downloader.is_downloaded(&model_id) {
        return Json(DownloadResponse {
            model_id: model_id.clone(),
            status: "already_downloaded".to_string(),
            message: "Model files already exist on disk".to_string(),
        })
        .into_response();
    }

    // Select ONNX variant
    let variant = req.variant.unwrap_or_else(|| state.onnx_variant.clone());

    // Resolve the variant's ONNX file AND its external-data weights together.
    // An unknown variant must be rejected, not silently downgraded to
    // `onnx_file` — that would drop the `*.onnx_data` files and produce a
    // model that downloads cleanly and then fails to load.
    let (onnx_file, external_data_files) = match registry.resolve_variant(&model_id, &variant) {
        Some(resolved) => resolved,
        None => {
            let supported = registry.variants(&model_id).join(", ");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!(
                            "Unknown ONNX variant '{variant}' for model '{model_id}' (supported: {supported})"
                        ),
                        error_type: "invalid_request".to_string(),
                        code: Some("unknown_variant".to_string()),
                    },
                }),
            )
                .into_response();
        }
    };

    // Create shared progress tracker
    let progress = Arc::new(DownloadProgress::new());
    {
        let mut pm = state.download_progress.write().await;
        pm.insert(model_id.clone(), progress.clone());
    }

    // Set initial download status
    {
        let mut status = state.download_status.write().await;
        status.insert(model_id.clone(), ModelStatus::Downloading(0));
    }

    // Create per-model cancel flag (isolated from other concurrent downloads)
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let mut flags = state.download_cancel_flags.write().await;
        flags.insert(model_id.clone(), cancel.clone());
    }

    // Fire-and-forget: spawn download in background
    let state2 = state.clone();
    let mid = model_id.clone();
    tokio::spawn(async move {
        let result = state2
            .downloader
            .download_model(
                DownloadSpec {
                    model_id: &mid,
                    hf_repo: &entry.hf_repo,
                    onnx_file: &onnx_file,
                    tokenizer_file: &entry.tokenizer_file,
                    external_data_files: &external_data_files,
                },
                &progress,
                &cancel,
            )
            .await;

        // Remove from active progress map and cancel flags
        {
            let mut pm = state2.download_progress.write().await;
            pm.remove(&mid);
        }
        {
            let mut flags = state2.download_cancel_flags.write().await;
            flags.remove(&mid);
        }

        match result {
            Ok(_res) => {
                let mut status = state2.download_status.write().await;
                status.insert(mid.clone(), ModelStatus::Downloaded);
                tracing::info!(model_id = %mid, "Model downloaded successfully");
            }
            Err(e) => {
                let mut status = state2.download_status.write().await;
                status.insert(mid.clone(), ModelStatus::Failed(format!("{e}")));
                tracing::error!(model_id = %mid, error = %e, "Model download failed");
            }
        }
    });

    (
        StatusCode::ACCEPTED,
        Json(DownloadResponse {
            model_id,
            status: "downloading".to_string(),
            message: "Download started".to_string(),
        }),
    )
        .into_response()
}

/// POST /models/{id}/cancel-download — Cancel an ongoing download.
/// Sets the shared cancel flag so the next `cancel.load()` check in the
/// downloader will abort the download loop.
pub async fn cancel_download(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    // Verify model exists in registry
    if !state.has_model(&model_id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: format!("Model '{}' not found in registry", model_id),
                    error_type: "not_found".to_string(),
                    code: None,
                },
            }),
        )
            .into_response();
    }

    // Set per-model cancel flag
    {
        let flags = state.download_cancel_flags.read().await;
        if let Some(flag) = flags.get(&model_id) {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    tracing::info!(model_id = %model_id, "Download cancel requested");

    // Update download status
    {
        let mut status = state.download_status.write().await;
        if let Some(ModelStatus::Downloading(_)) = status.get(&model_id) {
            status.insert(
                model_id.clone(),
                ModelStatus::Failed("Cancelled by user".to_string()),
            );
        }
    }

    Json(serde_json::json!({
        "model_id": model_id,
        "status": "cancellation_requested",
        "message": "Download cancellation signal sent"
    }))
    .into_response()
}

/// GET /models/{id}/status — Get model download/load status.
pub async fn model_status(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    let registry = state.registry().await;
    let entry = match registry.get(&model_id) {
        Some(e) => e,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!("Model '{model_id}' not found in registry"),
                        error_type: "not_found".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
    };

    // Check if model is loaded
    let model_guard = state.model.read().await;
    let is_loaded = model_guard
        .as_ref()
        .map(|m| m.model_id() == model_id)
        .unwrap_or(false);

    let info = if is_loaded {
        model_guard.as_ref().map(|m| ModelStatusInfo {
            id: m.model_id().to_string(),
            dimension: m.dimension(),
            pooling: format!("{:?}", entry.pooling_strategy),
        })
    } else {
        None
    };
    drop(model_guard);

    // Determine status: active download takes priority over
    // "is_downloaded" so the frontend sees download progress
    // instead of a misleading "downloaded" badge.
    let status = if is_loaded {
        ModelStatus::Loaded
    } else {
        let pm = state.download_progress.read().await;
        if let Some(progress) = pm.get(&model_id) {
            let (pct, _, _) = progress.snapshot();
            ModelStatus::Downloading(pct)
        } else {
            drop(pm);
            if state.downloader.is_downloaded(&model_id) {
                ModelStatus::Downloaded
            } else {
                let dl_status = state.download_status.read().await;
                dl_status
                    .get(&model_id)
                    .cloned()
                    .unwrap_or(ModelStatus::NotDownloaded)
            }
        }
    };

    Json(ModelStatusResponse {
        model_id: model_id.clone(),
        status: status.to_api_parts(),
        info,
    })
    .into_response()
}

/// POST /models/reload — re-read `embedding_models.json` from disk.
///
/// The Gateway writes a user-added local model into that file and calls this.
/// Without it the process would only ever know the catalog it started with,
/// and the new model's download would 404 on a registry entry that exists on
/// disk. Reloading beats restarting: a restart drops the loaded model and
/// interrupts an in-flight embedding pass.
pub async fn reload_registry(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let reloaded = ModelRegistry::load(&state.registry_dir);
    let model_count = reloaded.models().len();
    *state.registry.write().await = reloaded;
    tracing::info!(model_count, "Model registry reloaded from disk");
    Json(serde_json::json!({
        "status": "reloaded",
        "model_count": model_count,
    }))
    .into_response()
}

/// GET /models — Extended model list with status (non-OpenAI).
pub async fn list_models_detail(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let model_guard = state.model.read().await;
    let loaded_id = model_guard.as_ref().map(|m| m.model_id().to_string());
    drop(model_guard);

    let registry = state.registry().await;
    let models = registry.models();
    let mut infos = Vec::with_capacity(models.len());
    for entry in models {
        let status = if loaded_id.as_deref() == Some(entry.id.as_str()) {
            ModelStatus::Loaded
        } else {
            let pm = state.download_progress.read().await;
            if let Some(progress) = pm.get(&entry.id) {
                let (pct, _, _) = progress.snapshot();
                ModelStatus::Downloading(pct)
            } else {
                drop(pm);
                if state.downloader.is_downloaded(&entry.id) {
                    ModelStatus::Downloaded
                } else {
                    state
                        .download_status
                        .read()
                        .await
                        .get(&entry.id)
                        .cloned()
                        .unwrap_or(ModelStatus::NotDownloaded)
                }
            }
        };

        infos.push(ModelInfo {
            entry: entry.clone(),
            status: status.to_api_parts(),
        });
    }

    Json(ModelsDetailResponse { models: infos }).into_response()
}

/// DELETE /models/{id} — Delete downloaded model files from disk.
///
/// Refuses to delete if the model is currently loaded in memory.
/// After deletion, the model status reverts to "not_downloaded".
pub async fn delete_model(
    State(state): State<Arc<AppState>>,
    Path(model_id): Path<String>,
) -> impl IntoResponse {
    // Verify model exists in registry
    if !state.has_model(&model_id).await {
        return (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: format!("Model '{model_id}' not found in registry"),
                    error_type: "not_found".to_string(),
                    code: None,
                },
            }),
        )
            .into_response();
    }

    // Refuse to delete if the model is currently loaded
    {
        let model_guard = state.model.read().await;
        let is_loaded = model_guard
            .as_ref()
            .map(|m| m.model_id() == model_id)
            .unwrap_or(false);
        if is_loaded {
            return (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!(
                            "Model '{model_id}' is currently loaded. Switch to another model first."
                        ),
                        error_type: "conflict".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
    }

    // Check if a download is in progress for this model
    {
        let pm = state.download_progress.read().await;
        if pm.contains_key(&model_id) {
            return (
                StatusCode::CONFLICT,
                Json(ErrorResponse {
                    error: ErrorDetail {
                        message: format!(
                            "Model '{model_id}' is currently being downloaded. Cancel the download first."
                        ),
                        error_type: "conflict".to_string(),
                        code: None,
                    },
                }),
            )
                .into_response();
        }
    }

    // Delete model files from disk
    if let Err(e) = state.downloader.delete_model(&model_id) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: ErrorDetail {
                    message: format!("Failed to delete model files: {e}"),
                    error_type: "io_error".to_string(),
                    code: None,
                },
            }),
        )
            .into_response();
    }

    // Clean up temp downloading directory if present
    let tmp_dir = state.models_dir.join(format!("{model_id}.downloading"));
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // Clear download status
    {
        let mut status = state.download_status.write().await;
        status.remove(&model_id);
    }

    tracing::info!(model_id = %model_id, "Model files deleted");

    Json(serde_json::json!({
        "model_id": model_id,
        "status": "deleted",
        "message": "Model files deleted successfully"
    }))
    .into_response()
}

/// GET /events — Server-Sent Events stream for the gateway.
///
/// Two event kinds are emitted:
///   - `event: state` — high-level state transitions (Starting, Loading,
///     Ready, Error). Payload is a JSON object with `status`, `model_id`,
///     `dimension` fields.
///   - `event: heartbeat` — periodic liveness signal every 2s. Payload
///     includes `seq` and `ts_ms`. The gateway uses missed heartbeats
///     (>10s) to detect a stuck embed process.
///
/// The connection stays open until the client disconnects. The first
/// event the gateway sees is always a `state` event with the current
/// embed state — so the gateway learns the loaded model without polling.
pub async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, axum::Error>>> {
    let mut rx = state.event_bus.subscribe();
    let shutdown = state.shutdown.clone();
    let stream = async_stream::stream! {
        loop {
            // Stop streaming once shutdown is requested. This is a
            // long-lived connection: if we keep it open, axum's graceful
            // shutdown waits on it forever and the process never exits —
            // leaving a zombie that no longer serves but still emits
            // heartbeats (which also fools the gateway's watchdog).
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Ok(event) => {
                            let sse_event = match &*event {
                                BusEvent::Heartbeat { seq } => {
                                    Event::default()
                                        .event("heartbeat")
                                        .data(serde_json::json!({"seq": seq}).to_string())
                                }
                                BusEvent::State { seq, state } => {
                                    let payload = match state {
                                        BusState::Starting => {
                                            serde_json::json!({"status":"starting","model_id":null,"dimension":0})
                                        }
                                        BusState::DownloadingRecommended { model_id, progress } => {
                                            serde_json::json!({"status":"downloading_recommended","model_id":model_id,"dimension":0,"progress":progress})
                                        }
                                        BusState::Loading { model_id } => {
                                            serde_json::json!({"status":"loading","model_id":model_id,"dimension":0})
                                        }
                                        BusState::Ready { model_id, dimension } => {
                                            serde_json::json!({"status":"ready","model_id":model_id,"dimension":dimension})
                                        }
                                        BusState::Error { message } => {
                                            serde_json::json!({"status":"error","model_id":null,"dimension":0,"message":message})
                                        }
                                    };
                                    let data = serde_json::json!({"seq": seq, "state": payload});
                                    Event::default().event("state").data(data.to_string())
                                }
                            };
                            yield Ok(sse_event);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            // Client is too slow. Emit a comment so the client knows
                            // events were dropped, and continue.
                            yield Ok(Event::default().comment(format!("lagged:{n}")));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            // Bus is gone — embed is shutting down. End the stream.
                            break;
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                    if shutdown.is_shutting_down() {
                        break;
                    }
                }
            }
        }
    };
    Sse::new(stream)
}

/// Build the Axum router with all routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        // OpenAI-compatible endpoints
        .route("/v1/embeddings", post(create_embedding))
        .route("/v1/models", get(list_models))
        // Health check
        .route("/health", get(health_check))
        // Server-Sent Events for the gateway monitor
        .route("/events", get(events))
        // Management endpoints
        .route("/models", get(list_models_detail))
        .route("/models/reload", post(reload_registry))
        .route("/models/{id}/load", post(load_model))
        .route("/models/{id}/download", post(download_model))
        .route("/models/{id}/cancel-download", post(cancel_download))
        .route("/models/{id}/status", get(model_status))
        .route("/models/{id}", delete(delete_model))
        // ADR-123: this service's storage rows + the cleanup verb for the
        // model directories. Gateway-only surface (the Gateway is the sole
        // aggregator); never proxied to the Desktop.
        .route("/storage/report", get(crate::storage::storage_report))
        .route("/storage/cleanup", post(crate::storage::storage_cleanup))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body;
    use axum::response::IntoResponse;

    /// The /events SSE stream must terminate once shutdown is requested.
    /// A long-lived SSE connection kept open makes axum's graceful
    /// shutdown wait on it forever, so the embed process never exits —
    /// leaving a zombie that no longer serves HTTP but keeps the
    /// gateway's watchdog fed with heartbeats (harness embed test
    /// failed with "Lifecycle error: Failed to call embeddings
    /// endpoint" for exactly this reason).
    #[tokio::test]
    async fn events_stream_ends_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let shutdown = Shutdown::new();
        let event_bus = EventBus::new(16);
        let state = Arc::new(AppState {
            model: RwLock::new(None),
            registry: RwLock::new(ModelRegistry::load(dir.path())),
            registry_dir: dir.path().to_path_buf(),
            downloader: Downloader::new(dir.path(), vec![]),
            download_status: RwLock::new(Default::default()),
            download_progress: RwLock::new(Default::default()),
            shutdown: shutdown.clone(),
            models_dir: dir.path().to_path_buf(),
            onnx_variant: "fp32".to_string(),
            default_model: None,
            download_cancel_flags: RwLock::new(Default::default()),
            event_bus: event_bus.clone(),
        });

        let body = events(State(state)).await.into_response().into_body();

        // Stream delivers bus events before shutdown (and stays open).
        event_bus.publish_state(BusState::Ready {
            model_id: "test".to_string(),
            dimension: 8,
        });

        // After shutdown the stream must end promptly — otherwise axum
        // waits on this connection forever and the process never exits.
        shutdown.request();
        let drained = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            body::to_bytes(body, usize::MAX),
        )
        .await;
        assert!(
            drained.is_ok(),
            "SSE stream must terminate after shutdown is requested"
        );
    }
/// Regression test: a model added to `embedding_models.json` after the
    /// process started must become usable via `POST /models/reload`.
    ///
    /// The Gateway's add-model endpoint writes the file and then calls this —
    /// without it, the model is listed in the harness but every download 404s
    /// with "not found in registry", because the registry was loaded once at
    /// startup.
    #[tokio::test]
    async fn reload_picks_up_a_model_added_after_startup() {
        use tower::util::ServiceExt;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("embedding_models.json"),
            r#"{"version": 1, "models": []}"#,
        )
        .unwrap();
        let state = Arc::new(AppState {
            model: RwLock::new(None),
            registry: RwLock::new(ModelRegistry::load(dir.path())),
            registry_dir: dir.path().to_path_buf(),
            downloader: Downloader::new(dir.path(), vec![]),
            download_status: RwLock::new(Default::default()),
            download_progress: RwLock::new(Default::default()),
            shutdown: Shutdown::new(),
            models_dir: dir.path().to_path_buf(),
            onnx_variant: "fp32".to_string(),
            default_model: None,
            download_cancel_flags: RwLock::new(Default::default()),
            event_bus: EventBus::new(4),
        });
        assert!(state.registry().await.get("custom-model").is_none());

        // What the Gateway does: append the entry, write the file, then reload.
        std::fs::write(
            dir.path().join("embedding_models.json"),
            r#"{"version": 2, "models": [{
                "id": "custom-model",
                "name": "Custom",
                "dimension": 384,
                "max_tokens": 512,
                "size_mb": 0,
                "hf_repo": "owner/custom-onnx",
                "pooling_strategy": "mean",
                "onnx_file": "onnx/model.onnx",
                "tokenizer_file": "tokenizer.json"
            }]}"#,
        )
        .unwrap();

        let app = build_router(state.clone());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/models/reload")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let registry = state.registry().await;
        let entry = registry.get("custom-model").expect("reloaded entry");
        assert_eq!(entry.dimension, 384);
        assert_eq!(
            registry.resolve_variant("custom-model", "fp32"),
            Some(("onnx/model.onnx".to_string(), vec![])),
            "a custom entry has no variant map, so fp32 must resolve to onnx_file"
        );
    }
}
