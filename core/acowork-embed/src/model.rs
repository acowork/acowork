//! ONNX model loading and inference for embedding generation.
//!
//! Uses `ort` v2 for ONNX Runtime inference and `tokenizers` for
//! HuggingFace-compatible tokenization. The ONNX Session is wrapped
//! in `Arc<std::sync::Mutex<Session>>` for concurrent access safety.
//!
//! # Blocking safety
//!
//! ONNX inference (`session.run()`) is CPU-intensive and blocks the
//! calling thread. We use `tokio::task::spawn_blocking` to move the
//! entire inference pipeline off the tokio async runtime, and
//! `std::sync::Mutex` (not `tokio::sync::Mutex`) to guard the session.
//! This ensures the tokio worker threads remain free to serve HTTP
//! requests while inference runs on dedicated blocking threads.

use std::path::Path;
use std::sync::Arc;

use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::pool::{apply_pooling, l2_normalize};
use crate::registry::{OnnxOutputKind, PoolingStrategy};

// ── Error type ──────────────────────────────────────────────────────────

/// Error type for model operations.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("ONNX session error: {0}")]
    Session(String),

    #[error("Tokenizer error: {0}")]
    Tokenizer(String),

    #[error("Inference error: {0}")]
    Inference(String),

    #[error("Model not loaded")]
    NotLoaded,

    #[error("Invalid output shape: {0}")]
    InvalidShape(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ── EmbeddingModel ──────────────────────────────────────────────────────

/// A loaded ONNX embedding model with its tokenizer.
pub struct EmbeddingModel {
    /// ONNX Session wrapped in Arc<std::sync::Mutex> for concurrent access safety.
    /// Uses std::sync::Mutex (not tokio::sync::Mutex) because inference runs
    /// inside spawn_blocking where holding an async lock is impossible.
    session: Arc<std::sync::Mutex<Session>>,
    /// HuggingFace tokenizer.
    tokenizer: Tokenizer,
    /// Pooling strategy for this model. Only consulted when
    /// `output_kind == HiddenStates`; ignored for `AlreadyPooled`
    /// models whose ONNX graph already pools internally.
    pooling: PoolingStrategy,
    /// ONNX output shape category — drives the inference post-processing
    /// branch (see [`OnnxOutputKind`]).
    output_kind: OnnxOutputKind,
    /// Expected embedding dimension.
    dimension: usize,
    /// Maximum token length.
    max_tokens: usize,
    /// Model ID (e.g., "bge-small-zh-v1.5").
    model_id: String,
}

impl EmbeddingModel {
    /// Load an ONNX model from disk.
    ///
    /// # Arguments
    /// * `model_id` - Model identifier (must match registry entry).
    /// * `onnx_path` - Path to the ONNX model file.
    /// * `tokenizer_path` - Path to the `tokenizer.json` file.
    /// * `pooling` - Pooling strategy (CLS / Mean / LastToken). Ignored
    ///   when `output_kind == AlreadyPooled`.
    /// * `output_kind` - ONNX output shape category — see [`OnnxOutputKind`].
    /// * `dimension` - Expected embedding vector dimension.
    /// * `max_tokens` - Maximum token length for truncation.
    pub fn load(
        model_id: &str,
        onnx_path: &Path,
        tokenizer_path: &Path,
        pooling: PoolingStrategy,
        output_kind: OnnxOutputKind,
        dimension: usize,
        max_tokens: usize,
    ) -> Result<Self, ModelError> {
        // Load tokenizer
        let mut tokenizer = Tokenizer::from_file(tokenizer_path)
            .map_err(|e| ModelError::Tokenizer(format!("Failed to load tokenizer: {e}")))?;

        // Configure truncation
        let trunc_params = tokenizers::TruncationParams {
            max_length: max_tokens,
            ..Default::default()
        };
        tokenizer
            .with_truncation(Some(trunc_params))
            .map_err(|e| ModelError::Tokenizer(format!("Failed to set truncation: {e}")))?;

        // Disable padding — we handle padding manually in the batch logic
        tokenizer.with_padding(None);

        // Load ONNX session with retry — ONNX Runtime Initialize() can
        // fail transiently, especially after a previous process was SIGKILL'd
        // and OS resources haven't been fully released yet.
        const ONNX_LOAD_MAX_RETRIES: u32 = 3;
        const ONNX_LOAD_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

        let mut last_err = None;
        for attempt in 1..=ONNX_LOAD_MAX_RETRIES {
            tracing::info!(
                attempt,
                path = %onnx_path.display(),
                "Creating ONNX environment..."
            );
            let env = ort::init().build().map_err(|e| {
                ModelError::Session(format!("Failed to build ort Environment: {e}"))
            })?;

            tracing::info!(
                attempt,
                path = %onnx_path.display(),
                "Creating ONNX session builder..."
            );
            let mut builder = Session::builder(&env).map_err(|e| {
                ModelError::Session(format!("Failed to create session builder: {e}"))
            })?;

            tracing::info!(
                attempt,
                path = %onnx_path.display(),
                "Loading ONNX model from file..."
            );
            match builder.commit_from_file(onnx_path) {
                Ok(session) => {
                    if attempt > 1 {
                        tracing::info!(
                            attempt,
                            "ONNX model loaded successfully after retry"
                        );
                    }

                    tracing::info!(
                        model_id,
                        dimension,
                        max_tokens,
                        pooling = ?pooling,
                        output_kind = ?output_kind,
                        "Loaded ONNX embedding model"
                    );

                    return Ok(Self {
                        session: Arc::new(std::sync::Mutex::new(session)),
                        tokenizer,
                        pooling,
                        output_kind,
                        dimension,
                        max_tokens,
                        model_id: model_id.to_string(),
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        attempt,
                        error = %e,
                        "ONNX session load failed, will retry"
                    );
                    last_err = Some(e);
                    if attempt < ONNX_LOAD_MAX_RETRIES {
                        std::thread::sleep(ONNX_LOAD_RETRY_DELAY);
                    }
                }
            }
        }

        Err(ModelError::Session(format!(
            "Failed to load ONNX model after {} attempts: {:?}",
            ONNX_LOAD_MAX_RETRIES,
            last_err
        )))
    }

    /// Generate embedding for a single text.
    pub async fn embed(&self, text: &str) -> Result<Vec<f32>, ModelError> {
        if text.is_empty() {
            return Err(ModelError::Inference("Text cannot be empty".to_string()));
        }

        let embeddings = self.embed_batch(&[text]).await?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| ModelError::Inference("No embedding produced".to_string()))
    }

    /// Generate embeddings for multiple texts (batch).
    ///
    /// Tokenization runs on the async thread (lightweight), then ONNX inference
    /// is dispatched to a blocking thread via `spawn_blocking` to avoid blocking
    /// the tokio runtime.
    pub async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, ModelError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Tokenize all texts (lightweight — stays on async thread)
        let encodings = self
            .tokenizer
            .encode_batch(texts.iter().map(|s| s.to_string()).collect(), true)
            .map_err(|e| ModelError::Tokenizer(format!("Tokenization failed: {e}")))?;

        // Get max sequence length for manual padding
        let max_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);

        let batch_size = encodings.len();

        // Build flat input arrays with manual padding
        let mut input_ids_flat = Vec::with_capacity(batch_size * max_len);
        let mut attention_mask_flat = Vec::with_capacity(batch_size * max_len);
        let mut token_type_ids_flat = Vec::with_capacity(batch_size * max_len);

        for encoding in &encodings {
            let ids = encoding.get_ids();
            let mask = encoding.get_attention_mask();
            let type_ids = encoding.get_type_ids();

            for i in 0..max_len {
                input_ids_flat.push(if i < ids.len() { ids[i] as i64 } else { 0 });
                attention_mask_flat.push(if i < mask.len() { mask[i] as i64 } else { 0 });
                token_type_ids_flat.push(if i < type_ids.len() {
                    type_ids[i] as i64
                } else {
                    0
                });
            }
        }

        // Clone the attention_mask_flat for post-inference pooling
        let attention_mask_for_pooling = attention_mask_flat.clone();

        // Move ONNX inference to a blocking thread to avoid blocking tokio workers.
        // We use std::sync::Mutex so the guard can be held across the blocking call.
        let session = self.session.clone();
        let pooling = self.pooling.clone();
        let output_kind = self.output_kind;
        let dimension = self.dimension;
        let model_id = self.model_id.clone();

        let result = tokio::task::spawn_blocking(move || {
            let mut session = session
                .lock()
                .map_err(|e| ModelError::Inference(format!("Session lock poisoned: {e}")))?;

            // Check if model expects token_type_ids
            let has_token_type_ids = session
                .inputs()
                .iter()
                .any(|info| info.name() == "token_type_ids");

            // Create input tensors
            let input_ids_tensor = Tensor::from_array(([batch_size, max_len], input_ids_flat))
                .map_err(|e| {
                    ModelError::Inference(format!("Failed to create input_ids tensor: {e}"))
                })?;

            let attention_mask_tensor = Tensor::from_array((
                [batch_size, max_len],
                attention_mask_flat.clone(),
            ))
            .map_err(|e| {
                ModelError::Inference(format!("Failed to create attention_mask tensor: {e}"))
            })?;

            let outputs = if has_token_type_ids {
                let token_type_ids_tensor = Tensor::from_array((
                    [batch_size, max_len],
                    token_type_ids_flat,
                ))
                .map_err(|e| {
                    ModelError::Inference(format!("Failed to create token_type_ids tensor: {e}"))
                })?;

                session
                    .run(ort::inputs![
                        "input_ids" => input_ids_tensor,
                        "attention_mask" => attention_mask_tensor,
                        "token_type_ids" => token_type_ids_tensor,
                    ])
                    .map_err(|e| ModelError::Inference(format!("ONNX inference failed: {e}")))?
            } else {
                session
                    .run(ort::inputs![
                        "input_ids" => input_ids_tensor,
                        "attention_mask" => attention_mask_tensor,
                    ])
                    .map_err(|e| ModelError::Inference(format!("ONNX inference failed: {e}")))?
            };

            // Extract output data — must copy before releasing session
            let output = &outputs[0];
            let (shape, data) = output
                .try_extract_tensor::<f32>()
                .map_err(|e| ModelError::Inference(format!("Failed to extract output: {e}")))?;

            let shape_vec: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            let data_vec: Vec<f32> = data.to_vec();

            // Release session lock + outputs — all data is copied out
            drop(outputs);
            drop(session);

            // Branch on the model's ONNX output shape. Different exporters
            // bake different amounts of post-processing into the graph:
            //   * HiddenStates   → raw last_hidden_state [batch, seq_len, dim];
            //                     we apply pooling_strategy + L2 norm here.
            //   * AlreadyPooled  → pooled (and often normalized) [batch, dim];
            //                     we only re-apply L2 norm (idempotent on
            //                     already-normed vectors) and skip pooling.
            // ponytail ceiling: shape inspection is the only auto-detect
            // signal; we still require the registry to opt in via
            // `onnx_output_kind` so a misconfigured entry fails loud,
            // not silent.
            let mut results = Vec::with_capacity(batch_size);
            match output_kind {
                OnnxOutputKind::AlreadyPooled => {
                    // Expect [batch, dim]
                    if shape_vec.len() != 2 || shape_vec[0] != batch_size {
                        return Err(ModelError::InvalidShape(format!(
                            "Expected [batch, dim] for already-pooled model, got {:?}",
                            shape_vec
                        )));
                    }
                    let dim = shape_vec[1];
                    for b in 0..batch_size {
                        let mut pooled: Vec<f32> = (0..dim)
                            .map(|d| data_vec[b * dim + d])
                            .collect();
                        l2_normalize(&mut pooled);
                        if pooled.len() != dimension {
                            return Err(ModelError::InvalidShape(format!(
                                "Expected dimension {}, got {}",
                                dimension,
                                pooled.len()
                            )));
                        }
                        results.push(pooled);
                    }
                }
                OnnxOutputKind::HiddenStates => {
                    // Expect [batch, seq_len, hidden_dim]
                    if shape_vec.len() != 3 || shape_vec[0] != batch_size {
                        return Err(ModelError::InvalidShape(format!(
                            "Expected [batch, seq_len, dim], got {:?}",
                            shape_vec
                        )));
                    }

                    let seq_len = shape_vec[1];
                    let hidden_dim = shape_vec[2];

                    for b in 0..batch_size {
                        // Extract [seq_len, hidden_dim] for this batch item from flat data
                        let mut hidden_state: Vec<Vec<f32>> = Vec::with_capacity(seq_len);
                        for s in 0..seq_len {
                            let mut row = Vec::with_capacity(hidden_dim);
                            for d in 0..hidden_dim {
                                row.push(data_vec[b * seq_len * hidden_dim + s * hidden_dim + d]);
                            }
                            hidden_state.push(row);
                        }

                        // Extract attention_mask for this batch item
                        let mask: Vec<i64> = (0..max_len)
                            .map(|s| attention_mask_for_pooling[b * max_len + s])
                            .collect();

                        // Apply pooling
                        let mut pooled = apply_pooling(&hidden_state, &mask, &pooling);

                        // L2 normalize
                        l2_normalize(&mut pooled);

                        // Validate dimension
                        if pooled.len() != dimension {
                            return Err(ModelError::InvalidShape(format!(
                                "Expected dimension {}, got {}",
                                dimension,
                                pooled.len()
                            )));
                        }

                        results.push(pooled);
                    }
                }
            }

            Ok(results)
        })
        .await
        .map_err(|e| ModelError::Inference(format!("spawn_blocking task failed: {e}")))??;

        tracing::trace!(model_id, batch_size, "Embedding batch completed");

        Ok(result)
    }

    /// Get the model ID.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Get the embedding dimension.
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Get the max tokens.
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }
}
