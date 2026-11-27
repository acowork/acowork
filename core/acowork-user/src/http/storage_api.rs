//! `GET /internal/storage-report` + `POST /internal/storage-cleanup` — the user
//! service's half of ADR-123 (unified cache management).
//!
//! The Gateway aggregates these two verbs across every data-owning service and
//! forwards a user's cleanup click; it never walks `{user-data-dir}` itself
//! (ADR-123 §1.3 red line 1). This module is therefore the only place that
//! knows what the user domain holds on disk:
//!
//! | id | paths | class | cleanable |
//! |---|---|---|---|
//! | `chat-files` | `users/` + `chat-sessions/` + `assets/` | data | no |
//! | `transfers-active` | `transfers/blobs/*` inside the TTL | data | no |
//! | `transfers-expired` | `transfers/blobs/*` past the TTL | cache | **yes** |
//! | `uploads` | `.uploads/chat-sessions` + `transfers/.uploads` | cache | **yes** |
//!
//! The split in the middle two rows is the whole point of ADR-123 D4: a landed
//! transfer is somebody's in-flight file until [`TRANSFER_TTL_SECS`] passes, and
//! an attachment directory is user content forever. Both are *displayed*; only
//! the past-TTL half and the aborted-chunk half get a delete button.
//!
//! Reachable only over loopback — the Gateway is the sole caller, same trust
//! model as `/internal/user-profiles` (this process binds 127.0.0.1, ADR-084
//! §决策 7), so there is no per-request authorization here.

use axum::{
    Json, Router,
    extract::State,
    routing::{get, post},
};

use acowork_core::storage::{CleanupRequest, CleanupResult, StorageItem, StorageKind, StorageReport};
use acowork_core::storage::dir_bytes;
use acowork_core::upload::UPLOAD_TTL_SECS;

use crate::error::ApiError;
use crate::state::AppState;

/// Chat attachments (DM pair trees + session trees) and profile assets.
const ITEM_CHAT_FILES: &str = "chat-files";
/// Landed transfers still inside their TTL — shown, never deleted here.
const ITEM_TRANSFERS_ACTIVE: &str = "transfers-active";
/// Landed transfers past their TTL — the cache half of the same store.
const ITEM_TRANSFERS_EXPIRED: &str = "transfers-expired";
/// Aborted chunk uploads from both stores (ADR-122 §3).
const ITEM_UPLOADS: &str = "uploads";

/// Every id this service can answer, for the error message on a bad request.
const KNOWN_ITEMS: &[&str] = &[
    ITEM_CHAT_FILES,
    ITEM_TRANSFERS_ACTIVE,
    ITEM_TRANSFERS_EXPIRED,
    ITEM_UPLOADS,
];

pub fn storage_routes() -> Router<AppState> {
    Router::new()
        .route("/internal/storage-report", get(storage_report))
        .route("/internal/storage-cleanup", post(storage_cleanup))
}

/// `GET /internal/storage-report`.
async fn storage_report(State(state): State<AppState>) -> Json<StorageReport> {
    let data_dir = state.shared.read().await.data_dir.clone();

    let chat_files = dir_bytes(&crate::chat::pair_store_root(&data_dir)).await
        + dir_bytes(&data_dir.join("chat-sessions")).await
        + dir_bytes(&data_dir.join("assets")).await;
    let (active, expired) = state.transfers.landed_bytes_by_age().await;
    let uploads =
        dir_bytes(state.uploads.root()).await + dir_bytes(state.transfers.chunks_dir()).await;

    Json(StorageReport {
        items: vec![
            StorageItem {
                id: ITEM_CHAT_FILES.to_string(),
                kind: StorageKind::Data,
                bytes: chat_files,
                cleanable: false,
            },
            StorageItem {
                id: ITEM_TRANSFERS_ACTIVE.to_string(),
                kind: StorageKind::Data,
                bytes: active,
                cleanable: false,
            },
            StorageItem {
                id: ITEM_TRANSFERS_EXPIRED.to_string(),
                kind: StorageKind::Cache,
                bytes: expired,
                cleanable: true,
            },
            StorageItem {
                id: ITEM_UPLOADS.to_string(),
                kind: StorageKind::Cache,
                bytes: uploads,
                cleanable: true,
            },
        ],
    })
}

/// `POST /internal/storage-cleanup` — run only the categories asked for.
///
/// An unknown or data-class id is a **400, not a silent no-op**: the Desktop
/// shows "已清理 X MB" from this response, and a no-op success would be a lie
/// the user then acts on.
async fn storage_cleanup(
    State(state): State<AppState>,
    Json(req): Json<CleanupRequest>,
) -> Result<Json<CleanupResult>, ApiError> {
    if req.ids.is_empty() {
        return Err(ApiError::bad_request("cleanup needs at least one category id"));
    }

    // Measure before/after rather than trusting the sweep's return value: only
    // the byte delta answers "how much did the disk actually get back".
    let uploads_before = dir_bytes(state.uploads.root()).await
        + dir_bytes(state.transfers.chunks_dir()).await;

    let mut freed_bytes = 0u64;
    let mut sweep_uploads = false;
    for id in &req.ids {
        match id.as_str() {
            ITEM_UPLOADS => sweep_uploads = true,
            ITEM_TRANSFERS_EXPIRED => freed_bytes += state.transfers.purge_expired().await,
            ITEM_CHAT_FILES | ITEM_TRANSFERS_ACTIVE => {
                return Err(ApiError::bad_request(&format!(
                    "'{id}' is data-class and has no cleanup verb (ADR-123 D4)"
                )));
            }
            other => {
                return Err(ApiError::bad_request(&format!(
                    "unknown storage category '{other}' (known: {})",
                    KNOWN_ITEMS.join(", ")
                )));
            }
        }
    }

    if sweep_uploads {
        // Both stores carry the same 24 h TTL as the Runtime's chunk store, one
        // policy (ADR-122 D10) — the retained bytes are aborted uploads.
        state.uploads.sweep(UPLOAD_TTL_SECS).await;
        state.transfers.sweep_chunks().await;
        freed_bytes += uploads_before.saturating_sub(
            dir_bytes(state.uploads.root()).await
                + dir_bytes(state.transfers.chunks_dir()).await,
        );
    }

    tracing::info!(freed_bytes, ids = ?req.ids, "user storage cleanup ran");
    Ok(Json(CleanupResult { freed_bytes }))
}