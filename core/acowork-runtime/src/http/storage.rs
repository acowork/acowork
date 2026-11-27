//! `GET /internal/storage-report` + `POST /internal/storage-cleanup` — one
//! Runtime's rows in ADR-123's unified cache-management report.
//!
//! The Gateway never reads this directory: a Runtime's `work_dir` is
//! agent-private (ADR-009), so the numbers come out of the owner. They travel
//! Gateway ← Node ← Runtime (ADR-055): the Node fans the call out over its
//! process table and the Gateway aggregates per node (ADR-123 §6 Q4, first
//! version = per-node summary).
//!
//! | id | paths | class | cleanable |
//! |---|---|---|---|
//! | `uploads` | `{work_dir}/.uploads` (both chunk stores) | cache | **yes** |
//! | `logs` | `{work_dir}/logs` | cache | **yes** |
//! | `data` | everything else in `work_dir` — conversations, config, workspace | data | no |
//!
//! `data` is a remainder, not a fresh walk: subtracting the two cache trees
//! keeps the three rows non-overlapping, so the card's per-service total is the
//! directory's real size instead of 1.5× it. The workspace is the same
//! remainder — a Runtime's default workspace root IS its `work_dir`, and a
//! second walk of it would count every file twice.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use acowork_core::storage::{
    CleanupRequest, CleanupResult, LOG_RETENTION_SECS, StorageItem, StorageKind, StorageReport,
    dir_bytes, purge_files_older_than,
};

use crate::http::server::HttpState;

const ITEM_UPLOADS: &str = "uploads";
const ITEM_LOGS: &str = "logs";
const ITEM_DATA: &str = "data";

const KNOWN_ITEMS: &[&str] = &[ITEM_UPLOADS, ITEM_LOGS, ITEM_DATA];

/// `GET /internal/storage-report`.
pub(crate) async fn storage_report(State(state): State<HttpState>) -> Json<StorageReport> {
    let uploads = dir_bytes(&state.work_dir.join(".uploads")).await;
    let logs = dir_bytes(&state.work_dir.join("logs")).await;
    let total = dir_bytes(&state.work_dir).await;

    Json(StorageReport {
        items: vec![
            StorageItem {
                id: ITEM_UPLOADS.to_string(),
                kind: StorageKind::Cache,
                bytes: uploads,
                cleanable: true,
            },
            StorageItem {
                id: ITEM_LOGS.to_string(),
                kind: StorageKind::Cache,
                bytes: logs,
                cleanable: true,
            },
            StorageItem {
                id: ITEM_DATA.to_string(),
                kind: StorageKind::Data,
                bytes: total.saturating_sub(uploads).saturating_sub(logs),
                cleanable: false,
            },
        ],
    })
}

/// `POST /internal/storage-cleanup`.
pub(crate) async fn storage_cleanup(
    State(state): State<HttpState>,
    Json(req): Json<CleanupRequest>,
) -> Result<Json<CleanupResult>, StatusCode> {
    if req.ids.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut sweep_uploads = false;
    let mut sweep_logs = false;
    for id in &req.ids {
        match id.as_str() {
            ITEM_UPLOADS => sweep_uploads = true,
            ITEM_LOGS => sweep_logs = true,
            ITEM_DATA => return Err(StatusCode::CONFLICT),
            other => {
                tracing::warn!(category = other, known = ?KNOWN_ITEMS, "unknown storage category");
                return Err(StatusCode::BAD_REQUEST);
            }
        }
    }

    let mut freed_bytes = 0u64;
    if sweep_uploads {
        // Measure around the sweep: the store reports entries, not bytes.
        let before = dir_bytes(&state.work_dir.join(".uploads")).await;
        // The 24 h TTL the boot-time sweeper already applies; a user click just
        // makes it run now.
        match state.uploads.as_ref() {
            Some(uploads) => uploads.sweep().await,
            None => tracing::warn!("storage cleanup asked for uploads without an upload store"),
        }
        freed_bytes += before.saturating_sub(dir_bytes(&state.work_dir.join(".uploads")).await);
    }
    if sweep_logs {
        freed_bytes += purge_files_older_than(&state.work_dir.join("logs"), LOG_RETENTION_SECS).await;
    }

    tracing::info!(freed_bytes, ids = ?req.ids, "runtime storage cleanup ran");
    Ok(Json(CleanupResult { freed_bytes }))
}
