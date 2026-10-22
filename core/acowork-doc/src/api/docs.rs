//! Document REST handlers (design §4).
//!
//! Each handler is a thin shell: parse + validate input, delegate to
//! `DocState::docs`, render the result as a DTO. **Never** touch
//! `crate::store::*` / `crate::types` directly — go through the
//! `DocumentService` trait so business rules (version concurrency,
//! rename ↔ filename invariants) stay in one auditable place.

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use crate::api::dto::{
    CreateDocBody, DocMetaDto, DocReadDto, MoveDocBody, RenameDocBody, UpdateDocBody,
};
use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::path::validate_doc_id;
use crate::service::acl::AclTarget;
use crate::service::document::{CreateDocumentInput, DocumentService, UpdateDocumentInput};
use crate::types::{AclRole, DocMeta};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub dir_id: String,
}

/// `POST /api/docs` — create a document.
pub async fn create_doc(
    state: ApiState,
    headers: HeaderMap,
    Json(body): Json<CreateDocBody>,
) -> ApiResult<(StatusCode, Json<DocMetaDto>)> {
    let actor = actor_from(&headers);
    // Adding a doc is an edit on the hosting directory (design §9).
    state
        .acl
        .check(&actor, AclTarget::Dir(&body.parent_dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta: DocMeta = state
        .docs
        .create(CreateDocumentInput {
            parent_dir_id: body.parent_dir_id,
            title: body.title,
            content: body.content,
            import: body.import,
            creator: actor.id.clone(),
        })
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(DocMetaDto::from(meta))))
}

/// `GET /api/docs/:doc_id` — read a document (meta + content).
pub async fn read_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<DocReadDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let read = state.docs.read(&doc_id).await.map_err(ApiError::from)?;
    Ok(Json(DocReadDto::from(read)))
}

/// `PUT /api/docs/:doc_id` — update content + (optionally) title.
pub async fn update_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    Json(body): Json<UpdateDocBody>,
) -> ApiResult<Json<DocMetaDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state
        .docs
        .update(
            &doc_id,
            UpdateDocumentInput {
                base_version: body.base_version,
                title: body.title,
                content: body.content,
            },
        )
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DocMetaDto::from(meta)))
}

/// `PATCH /api/docs/:doc_id/title` — rename a document.
pub async fn rename_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    Json(body): Json<RenameDocBody>,
) -> ApiResult<Json<DocMetaDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state
        .docs
        .rename(&doc_id, &body.new_title, body.base_version)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DocMetaDto::from(meta)))
}

/// `POST /api/docs/:doc_id/move` — move to another directory.
pub async fn move_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
    Json(body): Json<MoveDocBody>,
) -> ApiResult<Json<DocMetaDto>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    // Moving removes the doc from its current dir (edit there) and adds
    // it to the target (edit there) — both gates apply (design §9).
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    state
        .acl
        .check(&actor, AclTarget::Dir(&body.target_dir_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let meta = state
        .docs
        .move_doc(&doc_id, &body.target_dir_id, body.overwrite)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(DocMetaDto::from(meta)))
}

/// `DELETE /api/docs/:doc_id` — soft-delete (moves to `.trash/`).
pub async fn delete_doc(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<StatusCode> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    state.docs.delete(&doc_id).await.map_err(ApiError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/docs/:doc_id/path` — return the on-disk relative path.
pub async fn doc_path(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    validate_doc_id(&doc_id)?;
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let path = state.docs.path_of(&doc_id).await.map_err(ApiError::from)?;
    Ok(Json(serde_json::json!({ "path": path })))
}

/// `GET /api/docs?dir_id=...` — list docs under a directory.
pub async fn list_docs(
    state: ApiState,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<DocMetaDto>>> {
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Dir(&q.dir_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let mut list = state.docs.list(&q.dir_id).await.map_err(ApiError::from)?;
    state.acl.filter_docs(&actor, &q.dir_id, &mut list).await;
    Ok(Json(list.into_iter().map(DocMetaDto::from).collect()))
}
