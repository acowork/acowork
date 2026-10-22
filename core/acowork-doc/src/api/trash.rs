//! Recycle-bin REST handlers (design §4: `GET /api/trash`,
//! `POST /api/trash/:id/restore`).
//!
//! ACL (design §9): the trash is a single global `.trash/` pool, so every
//! handler resolves the caller from the trusted headers and gates on the
//! slot's snapshotted ACL — `view` to list, `edit` to restore/purge. Admins
//! and the local `human` actor bypass (the whole account system is off in
//! local mode).

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;

use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::error::DocError;
use crate::service::trash::TrashService;
use crate::types::{AclRole, DocMeta, TrashEntry};

/// Locate one slot by id (the service exposes only a full `list`), so the
/// restore/purge handlers can gate before mutating.
async fn find_slot(
    state: &crate::state::DocState,
    trash_id: &str,
) -> Result<TrashEntry, ApiError> {
    state
        .trash
        .list()
        .await
        .map_err(ApiError::from)?
        .into_iter()
        .find(|e| e.trash_id == trash_id)
        .ok_or_else(|| ApiError::from(DocError::TrashMissing(trash_id.to_string())))
}

/// `GET /trash` — list trash slots visible to the caller (newest first;
/// lazy 30-day purge runs).
pub async fn list_trash(
    state: ApiState,
    headers: HeaderMap,
) -> ApiResult<Json<Vec<TrashEntry>>> {
    let actor = actor_from(&headers);
    let entries = state.trash.list().await.map_err(ApiError::from)?;
    let mut visible = Vec::with_capacity(entries.len());
    for e in entries {
        // Skip slots the caller cannot view; never error the whole list on
        // one invisible entry.
        if state.acl.check_trashed(&actor, &e, AclRole::View).await.is_ok() {
            visible.push(e);
        }
    }
    Ok(Json(visible))
}

/// `POST /trash/{trash_id}/restore` — restore into its original directory.
pub async fn restore_trash(
    state: ApiState,
    headers: HeaderMap,
    Path(trash_id): Path<String>,
) -> ApiResult<Json<DocMeta>> {
    let actor = actor_from(&headers);
    let slot = find_slot(&state, &trash_id).await?;
    // Restoring writes the document back into its directory — an edit.
    state
        .acl
        .check_trashed(&actor, &slot, AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state
        .trash
        .restore(&trash_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(meta))
}

/// `DELETE /trash/{trash_id}` — permanently delete a slot.
pub async fn purge_trash(
    state: ApiState,
    headers: HeaderMap,
    Path(trash_id): Path<String>,
) -> ApiResult<StatusCode> {
    let actor = actor_from(&headers);
    let slot = find_slot(&state, &trash_id).await?;
    // Purging destroys content the caller could edit — gate on edit.
    state
        .acl
        .check_trashed(&actor, &slot, AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    state
        .trash
        .purge(&trash_id)
        .await
        .map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}
