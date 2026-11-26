//! ACowork Embedding Runtime — ONNX-based embedding service
//! with OpenAI-compatible API.
//!
//! Entry point: parse CLI arguments, initialize logging, load model,
//! start HTTP server, and handle graceful shutdown.

use std::sync::Arc;

use clap::Parser;

use acowork_embed::config::Cli;
use acowork_embed::download::{DownloadProgress, DownloadSpec, Downloader};
use acowork_embed::event_bus::{EventBus, State as BusState};
use acowork_embed::model::EmbeddingModel;
use acowork_embed::registry::{ModelRegistry, ModelStatus};
use acowork_embed::server::AppState;
use acowork_embed::shutdown::Shutdown;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Initialize logging to stderr (Gateway redirects stderr → embed.log)
    acowork_core::logging::init_subprocess_logging(&cli.log_level);

    // Install global panic hook AFTER tracing is initialized so panic
    // messages are captured in the log output.
    acowork_core::logging::install_panic_hook();

    tracing::info!("ACowork Embedding Runtime starting");
    tracing::info!(addr = %cli.listen_addr(), "Listen address");

    // Create shutdown signal
    let shutdown = Shutdown::new();
    acowork_embed::shutdown::install_signal_handlers(shutdown.clone());

    // Ensure models directory exists
    let models_dir = std::path::PathBuf::from(&cli.models_dir);
    if !models_dir.exists() {
        std::fs::create_dir_all(&models_dir).expect("Failed to create models directory");
        tracing::info!(dir = %models_dir.display(), "Created models directory");
    }

    // Load model registry
    let data_dir = std::path::PathBuf::from(cli.data_dir());
    let registry = ModelRegistry::load(&data_dir);
    tracing::info!(count = registry.models().len(), "Loaded model registry");

    // Create event bus for SSE — heartbeats run on a 2s cadence so the
    // gateway can detect a stuck embed process within ~10s.
    let event_bus = EventBus::new(64);
    event_bus.spawn_heartbeat(2000);
    event_bus.publish_state(BusState::Starting);

    // Create downloader
    let downloader = Downloader::new(&models_dir, cli.hf_mirrors.clone());

    // Determine which model to load (resolve to owned String before moving registry)
    let default_model_id = cli
        .model
        .clone()
        .or_else(|| registry.recommended().map(|m| m.id.clone()))
        .unwrap_or_else(|| {
            registry
                .models()
                .first()
                .map(|m| m.id.clone())
                .unwrap_or_else(|| "bge-small-zh-v1.5".to_string())
        });

    tracing::info!(model_id = %default_model_id, "Target model");

    let state = Arc::new(AppState {
        model: tokio::sync::RwLock::new(None),
        registry: tokio::sync::RwLock::new(registry),
        registry_dir: data_dir,
        downloader,
        download_status: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        download_progress: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        shutdown: shutdown.clone(),
        models_dir: models_dir.clone(),
        onnx_variant: cli.onnx_variant.clone(),
        default_model: Some(default_model_id.clone()),
        download_cancel_flags: tokio::sync::RwLock::new(std::collections::HashMap::new()),
        event_bus: event_bus.clone(),
    });

    tokio::spawn(bootstrap_default_model(state.clone(), default_model_id));

    // Build router
    let app = acowork_embed::server::build_router(state.clone());

    // Start HTTP server
    let listener = tokio::net::TcpListener::bind(cli.listen_addr())
        .await
        .expect("Failed to bind listen address");

    tracing::info!(addr = %cli.listen_addr(), "HTTP server listening");

    // Run server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await
        .expect("HTTP server error");

    tracing::info!("ACowork Embedding Runtime stopped");
}

async fn bootstrap_default_model(state: Arc<AppState>, model_id: String) {
    if state.downloader.is_downloaded(&model_id) {
        state.event_bus.publish_state(BusState::Loading {
            model_id: model_id.clone(),
        });
        load_model_into_state(state, model_id).await;
        return;
    }

    tracing::warn!(
        model_id = %model_id,
        "Model not available at startup. Auto-downloading recommended model..."
    );

    let registry = state.registry().await;
    let Some(entry) = registry.get(&model_id).cloned() else {
        state.event_bus.publish_state(BusState::Error {
            message: format!("Model '{model_id}' not found in registry"),
        });
        return;
    };

    // Resolve the ONNX file and its external-data weights together; an
    // unresolvable variant is a registry bug, so fail loudly instead of
    // downloading a graph whose weights are missing.
    let (onnx_file, external_data_files) =
        match registry.resolve_variant(&model_id, &state.onnx_variant) {
            Some(resolved) => resolved,
            None => {
                let supported = registry.variants(&model_id).join(", ");
                state.event_bus.publish_state(BusState::Error {
                    message: format!(
                        "Unknown ONNX variant '{}' for model '{}' (supported: {supported})",
                        state.onnx_variant, model_id
                    ),
                });
                return;
            }
        };
    let progress = Arc::new(DownloadProgress::new());
    let cancel_flag = std::sync::atomic::AtomicBool::new(false);

    {
        let mut pm = state.download_progress.write().await;
        pm.insert(model_id.clone(), progress.clone());
    }
    {
        let mut status = state.download_status.write().await;
        status.insert(model_id.clone(), ModelStatus::Downloading(0));
    }
    state
        .event_bus
        .publish_state(BusState::DownloadingRecommended {
            model_id: model_id.clone(),
            progress: 0,
        });

    let progress_bus = progress.clone();
    let status_state = state.clone();
    let status_model_id = model_id.clone();
    let status_publisher = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(500));
        let mut last_progress = 0;
        loop {
            ticker.tick().await;
            let (pct, _, _) = progress_bus.snapshot();
            if pct != last_progress {
                last_progress = pct;
                {
                    let mut status = status_state.download_status.write().await;
                    status.insert(status_model_id.clone(), ModelStatus::Downloading(pct));
                }
                status_state
                    .event_bus
                    .publish_state(BusState::DownloadingRecommended {
                        model_id: status_model_id.clone(),
                        progress: pct,
                    });
            }
        }
    });

    let result = state
        .downloader
        .download_model(
            DownloadSpec {
                model_id: &model_id,
                hf_repo: &entry.hf_repo,
                onnx_file: &onnx_file,
                tokenizer_file: &entry.tokenizer_file,
                external_data_files: &external_data_files,
            },
            &progress,
            &cancel_flag,
        )
        .await;

    status_publisher.abort();
    {
        let mut pm = state.download_progress.write().await;
        pm.remove(&model_id);
    }

    match result {
        Ok(_) => {
            {
                let mut status = state.download_status.write().await;
                status.insert(model_id.clone(), ModelStatus::Downloaded);
            }
            tracing::info!(model_id = %model_id, "Auto-download complete, loading model...");
            state.event_bus.publish_state(BusState::Loading {
                model_id: model_id.clone(),
            });
            load_model_into_state(state, model_id).await;
        }
        Err(e) => {
            {
                let mut status = state.download_status.write().await;
                status.insert(model_id.clone(), ModelStatus::Failed(format!("{e}")));
            }
            tracing::error!(
                model_id = %model_id,
                error = %e,
                "Auto-download failed. Server will continue without a loaded model."
            );
            state.event_bus.publish_state(BusState::Error {
                message: format!("Auto-download of '{model_id}' failed: {e}"),
            });
        }
    }
}

async fn load_model_into_state(state: Arc<AppState>, model_id: String) {
    let registry = state.registry().await;
    let models_dir = state.models_dir.clone();
    let model_id_for_load = model_id.clone();
    let load_result = tokio::task::spawn_blocking(move || {
        try_load_model(&model_id_for_load, &registry, &models_dir)
    })
    .await;

    match load_result {
        Ok(Some(model)) => {
            let dim = model.dimension();
            {
                let mut model_guard = state.model.write().await;
                *model_guard = Some(Arc::new(model));
            }
            {
                let mut status = state.download_status.write().await;
                status.insert(model_id.clone(), ModelStatus::Loaded);
            }
            tracing::info!(model_id = %model_id, "Model loaded");
            state.event_bus.publish_state(BusState::Ready {
                model_id,
                dimension: dim,
            });
        }
        Ok(None) => {
            let message = format!("Model '{model_id}' files are missing or failed to load");
            let mut status = state.download_status.write().await;
            status.insert(model_id.clone(), ModelStatus::Failed(message.clone()));
            state.event_bus.publish_state(BusState::Error { message });
        }
        Err(e) => {
            let message = format!("Model loading task panicked for '{model_id}': {e}");
            let mut status = state.download_status.write().await;
            status.insert(model_id.clone(), ModelStatus::Failed(message.clone()));
            state.event_bus.publish_state(BusState::Error { message });
        }
    }
}

/// Synchronously try to load a model from disk.
fn try_load_model(
    model_id: &str,
    registry: &ModelRegistry,
    models_dir: &std::path::Path,
) -> Option<EmbeddingModel> {
    let entry = registry.get(model_id)?;

    let model_dir = models_dir.join(model_id);
    let onnx_path = model_dir.join("model.onnx");
    let tokenizer_path = model_dir.join("tokenizer.json");

    if !onnx_path.exists() {
        tracing::debug!(path = %onnx_path.display(), "ONNX file not found");
        return None;
    }
    if !tokenizer_path.exists() {
        tracing::debug!(path = %tokenizer_path.display(), "Tokenizer file not found");
        return None;
    }

    match EmbeddingModel::load(
        model_id,
        &onnx_path,
        &tokenizer_path,
        entry.pooling_strategy.clone(),
        entry.onnx_output_kind,
        entry.dimension,
        entry.max_tokens,
    ) {
        Ok(model) => Some(model),
        Err(e) => {
            tracing::error!(model_id, error = %e, "Failed to load model");
            None
        }
    }
}

/// Wait for shutdown signal.
async fn shutdown_signal(shutdown: Arc<Shutdown>) {
    // Wait until shutdown flag is set
    while !shutdown.is_shutting_down() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tracing::info!("Graceful shutdown initiated, waiting for in-flight requests...");

    // Grace period: wait up to 5 seconds for in-flight requests
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    tracing::info!("Grace period elapsed, shutting down");
}

/// Initialize the tracing subscriber.
/// Replaced by acowork_core::logging::init_subprocess_logging — kept as
/// a thin wrapper for now to avoid breaking pub API if referenced elsewhere.
#[allow(dead_code)]
fn init_logging(level: &str) {
    acowork_core::logging::init_subprocess_logging(level);
}
