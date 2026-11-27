//! `GET /storage/report` + `POST /storage/cleanup` — the embed service's half of
//! ADR-123 (unified cache management).
//!
//! This is the biggest cache on a Gateway box: `{data_dir}/models` holds one
//! ONNX model directory per downloadable embedding model (tens to hundreds of
//! MB each), plus `{model_id}.downloading/` staging trees left by an aborted or
//! cancelled download. Everything here is re-downloadable, so the whole service
//! is cache-class — but "cache" does not mean "delete freely":
//!
//! | id | what | cleanable |
//! |---|---|---|
//! | `models-active` | the model currently loaded into ONNX Runtime | no |
//! | `models` | every other downloaded model directory | **yes** |
//! | `models-partial` | `*.downloading/` staging trees, unless that download is live | **yes** |
//!
//! The loaded model is excluded by the service, not by the caller: deleting it
//! would pull the model out from under the running inference loop, and the
//! retention policy is exactly what ADR-123 §1.3 red line 3 keeps inside the
//! owner. `POST /models/{id}/download` restores anything the user cleans.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use std::path::Path;
use std::sync::Arc;

use acowork_core::storage::{CleanupRequest, CleanupResult, StorageItem, StorageKind, StorageReport, dir_bytes};

use crate::server::AppState;

/// Staging suffix for an in-progress download (`{models_dir}/{id}.downloading`).
const DOWNLOAD_SUFFIX: &str = ".downloading";

const ITEM_ACTIVE: &str = "models-active";
const ITEM_MODELS: &str = "models";
const ITEM_PARTIAL: &str = "models-partial";

const KNOWN_ITEMS: &[&str] = &[ITEM_ACTIVE, ITEM_MODELS, ITEM_PARTIAL];

/// `GET /storage/report`.
pub async fn storage_report(State(state): State<Arc<AppState>>) -> Json<StorageReport> {
    let active_id = loaded_model_id(&state).await;
    let mut report = StorageReport::default();

    let mut active_bytes = 0u64;
    let mut models_bytes = 0u64;
    for (name, bytes) in model_dirs(&state.models_dir).await {
        if Some(&name) == active_id.as_ref() {
            active_bytes += bytes;
        } else {
            models_bytes += bytes;
        }
    }
    let partial_bytes = partial_dirs(&state.models_dir)
        .await
        .into_iter()
        .map(|(_, bytes)| bytes)
        .sum();

    report.items.push(StorageItem {
        id: ITEM_ACTIVE.to_string(),
        kind: StorageKind::Cache,
        bytes: active_bytes,
        // Cache-class but not cleanable right now: the bytes go away the moment
        // the user switches models, and deleting them here would break inference.
        cleanable: false,
    });
    report.items.push(StorageItem {
        id: ITEM_MODELS.to_string(),
        kind: StorageKind::Cache,
        bytes: models_bytes,
        cleanable: true,
    });
    report.items.push(StorageItem {
        id: ITEM_PARTIAL.to_string(),
        kind: StorageKind::Cache,
        bytes: partial_bytes,
        cleanable: true,
    });
    Json(report)
}

/// `POST /storage/cleanup` — remove what this service decided is collectable.
pub async fn storage_cleanup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CleanupRequest>,
) -> Result<Json<CleanupResult>, StatusCode> {
    if req.ids.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let active_id = loaded_model_id(&state).await;
    let mut freed_bytes = 0u64;

    for id in &req.ids {
        match id.as_str() {
            ITEM_MODELS => {
                for (name, bytes) in model_dirs(&state.models_dir).await {
                    if Some(&name) == active_id.as_ref() {
                        continue; // the retention rule lives here, not in the caller
                    }
                    if tokio::fs::remove_dir_all(state.models_dir.join(&name))
                        .await
                        .is_ok()
                    {
                        freed_bytes += bytes;
                    }
                }
            }
            ITEM_PARTIAL => {
                for (name, bytes) in partial_dirs(&state.models_dir).await {
                    // A live download owns its staging tree; only leftovers go.
                    if is_downloading(&state, name.trim_end_matches(DOWNLOAD_SUFFIX)).await {
                        continue;
                    }
                    if tokio::fs::remove_dir_all(state.models_dir.join(&name))
                        .await
                        .is_ok()
                    {
                        freed_bytes += bytes;
                    }
                }
            }
            ITEM_ACTIVE => return Err(StatusCode::CONFLICT),
            other => {
                tracing::warn!(category = other, known = ?KNOWN_ITEMS, "unknown storage category");
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    tracing::info!(freed_bytes, ids = ?req.ids, "embed storage cleanup ran");
    Ok(Json(CleanupResult { freed_bytes }))
}

async fn loaded_model_id(state: &AppState) -> Option<String> {
    state
        .model
        .read()
        .await
        .as_ref()
        .map(|m| m.model_id().to_string())
}

/// Whether a download is live right now.
///
/// Keyed on `download_progress`, not `download_status`: the progress map is the
/// one the racer tasks populate for the duration of a download, so a staging
/// tree it names is in use. (`download_status` keeps a terminal `Downloading`
/// value behind after a failure, which would make a leftover tree immortal.)
async fn is_downloading(state: &AppState, model_id: &str) -> bool {
    state.download_progress.read().await.contains_key(model_id)
}

/// `(dir_name, bytes)` for every real model directory under `root`.
///
/// A model directory is a plain directory; the staging trees (`*.downloading`)
/// are reported separately so the two rows never double-count.
async fn model_dirs(root: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(DOWNLOAD_SUFFIX) {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        out.push((name, dir_bytes(&path).await));
    }
    out
}

/// `(dir_name, bytes)` for every `*.downloading` staging tree under `root`.
async fn partial_dirs(root: &Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return out;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(DOWNLOAD_SUFFIX) || !entry.path().is_dir() {
            continue;
        }
        out.push((name.clone(), dir_bytes(&entry.path()).await));
    }
    out
}