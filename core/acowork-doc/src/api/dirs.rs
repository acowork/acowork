//! Directory REST handlers (design §4).
//!
//! Same thin-shell pattern as `docs.rs` — parse body, delegate to
//! `DocState::dirs`, render the DTO. Never touch `store::*` directly.

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use crate::api::dto::{
    CreateDirBody, DirMetaDto, RenameDirBody, TreeNodeDto,
};
use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::path::validate_dir_id;
use crate::service::acl::AclTarget;
use crate::service::directory::{CreateDirectoryInput, DirectoryService};
use crate::types::{AclRole, DirMeta};

#[derive(Debug, Deserialize)]
pub struct TreeQuery {
    /// Empty / absent → root tree.
    pub dir_id: Option<String>,
}

/// `POST /api/dirs` — create a subdirectory.
pub async fn create_dir(
    state: ApiState,
    headers: HeaderMap,
    Json(body): Json<CreateDirBody>,
) -> ApiResult<(StatusCode, Json<DirMetaDto>)> {
    let actor = actor_from(&headers);
    // Creating inside a directory is an edit on it (design §9).
    state
        .acl
        .check(&actor, AclTarget::Dir(&body.parent_dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta: DirMeta = state
        .dirs
        .create(CreateDirectoryInput {
            parent_dir_id: body.parent_dir_id,
            name: body.name,
            creator: actor.id.clone(),
        })
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(DirMetaDto::from(meta))))
}

/// `GET /api/dirs/:dir_id` — read directory metadata.
pub async fn read_dir(
    state: ApiState,
    headers: HeaderMap,
    Path(dir_id): Path<String>,
) -> ApiResult<Json<DirMetaDto>> {
    validate_dir_id(&dir_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&dir_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let meta = state.dirs.read(&dir_id).await.map_err(ApiError::from)?;
    Ok(Json(DirMetaDto::from(meta)))
}

/// `GET /api/tree?dir_id=...` — list the immediate children.
pub async fn list_tree(
    state: ApiState,
    headers: HeaderMap,
    Query(q): Query<TreeQuery>,
) -> ApiResult<Json<TreeNodeDto>> {
    let dir_id = q.dir_id.unwrap_or_else(|| crate::types::ROOT_DIR_ID.to_string());
    validate_dir_id(&dir_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&dir_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let mut tree = state.dirs.list_tree(&dir_id).await.map_err(ApiError::from)?;
    // Children the actor cannot `view` never reach the UI (design §9).
    state
        .acl
        .filter_tree(&actor, &mut tree)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(TreeNodeDto::from(tree)))
}

/// `PATCH /api/dirs/:dir_id/name` — rename.
pub async fn rename_dir(
    state: ApiState,
    headers: HeaderMap,
    Path(dir_id): Path<String>,
    Json(body): Json<RenameDirBody>,
) -> ApiResult<Json<DirMetaDto>> {
    validate_dir_id(&dir_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state.dirs.rename(&dir_id, &body.new_name).await.map_err(ApiError::from)?;
    Ok(Json(DirMetaDto::from(meta)))
}

/// `DELETE /api/dirs/:dir_id` — cascade-delete into `.trash/`.
pub async fn delete_dir(
    state: ApiState,
    headers: HeaderMap,
    Path(dir_id): Path<String>,
) -> ApiResult<StatusCode> {
    validate_dir_id(&dir_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    state.dirs.delete(&dir_id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}
