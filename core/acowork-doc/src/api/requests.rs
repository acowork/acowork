//! Update-request REST handlers — the human review queue (design §4/§5).
//!
//! Thin shells only: parse + validate input, delegate to
//! `RequestService`, render results as DTOs. No handler touches
//! `crate::store::*` / `crate::types` directly.
//!
//! Routes (mounted without `/api` prefix — Gateway strips it):
//! - `POST   /requests`                  submit (agent path)
//! - `GET    /requests?status=`          review queue
//! - `GET    /requests/{id}`             request detail / status check
//! - `POST   /requests/{id}/approve`     review-approve (merge + version+1)
//! - `POST   /requests/{id}/reject`      review-reject (with note)
//! - `GET    /docs/{doc_id}/requests`    per-document history (router-side)

use axum::extract::{Path, Query};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;

use crate::api::dto::{
    ApproveDto, ReviewBody, SubmitRequestBody, UpdateRequestDto,
};
use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::path::{validate_doc_id, validate_request_id};
use crate::service::acl::AclTarget;
use crate::service::request::{RequestService, SubmitRequestInput};
use crate::types::{AclRole, RequestStatus};

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    /// Optional filter: pending | approved | rejected | expired.
    #[serde(default)]
    pub status: Option<RequestStatus>,
}

/// `POST /requests` — agent submits a PR-style update proposal.
///
/// Agent-intake path (mirrors the MCP surface): the *proposal* is inert
/// until a human with `edit` approves it, so it stays ungated in v1 — the
/// real enforcement lands on `approve` / `reject` / read (design §9).
pub async fn submit_request(
    state: ApiState,
    Json(body): Json<SubmitRequestBody>,
) -> ApiResult<(StatusCode, Json<UpdateRequestDto>)> {
    validate_doc_id(&body.doc_id)?;
    let req = state
        .requests
        .submit(SubmitRequestInput {
            doc_id: body.doc_id,
            base_version: body.base_version,
            content: body.content,
            submitted_by: body.submitted_by,
        })
        .await
        .map_err(ApiError::from)?;
    Ok((StatusCode::CREATED, Json(req.into())))
}

/// `GET /requests?status=pending` — human review queue.
pub async fn list_requests(
    state: ApiState,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<UpdateRequestDto>>> {
    let actor = actor_from(&headers);
    let reqs = state
        .requests
        .list(q.status)
        .await
        .map_err(ApiError::from)?;
    // The queue is global; drop proposals whose target doc the caller
    // cannot view (titles + proposed content would otherwise leak).
    let mut visible = Vec::with_capacity(reqs.len());
    for req in reqs {
        if state.acl.can_view_doc(&actor, &req.doc_id).await {
            visible.push(req);
        }
    }
    Ok(Json(visible.into_iter().map(Into::into).collect()))
}

/// `GET /requests/{id}` — request detail / status check (doc_check_request).
pub async fn get_request(
    state: ApiState,
    headers: HeaderMap,
    Path(request_id): Path<String>,
) -> ApiResult<Json<UpdateRequestDto>> {
    validate_request_id(&request_id)?;
    let actor = actor_from(&headers);
    let req = state
        .requests
        .get(&request_id)
        .await
        .map_err(ApiError::from)?;
    // The detail quotes the proposed content — gate on the target doc's
    // `view` (mirrors `list_doc_requests`).
    state
        .acl
        .check(&actor, AclTarget::Doc(&req.doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(req.into()))
}

/// `POST /requests/{id}/approve` — human approves; merges into the doc.
pub async fn approve_request(
    state: ApiState,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    Json(body): Json<ReviewBody>,
) -> ApiResult<Json<ApproveDto>> {
    validate_request_id(&request_id)?;
    // Approving rewrites the target document — an edit on it (design §9).
    let actor = actor_from(&headers);
    let pending = state
        .requests
        .get(&request_id)
        .await
        .map_err(ApiError::from)?;
    state
        .acl
        .check(&actor, AclTarget::Doc(&pending.doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let outcome = state
        .requests
        .approve(&request_id, &body.reviewed_by, body.note.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(ApproveDto {
        request: outcome.request,
        doc_version: outcome.doc_version,
    }))
}

/// `POST /requests/{id}/reject` — human rejects (keeps content + note).
pub async fn reject_request(
    state: ApiState,
    headers: HeaderMap,
    Path(request_id): Path<String>,
    Json(body): Json<ReviewBody>,
) -> ApiResult<Json<UpdateRequestDto>> {
    validate_request_id(&request_id)?;
    // Rejecting is a review decision on the target document — an edit
    // there (mirrors `approve_request`); otherwise any caller could veto
    // another owner's proposals.
    let actor = actor_from(&headers);
    let pending = state
        .requests
        .get(&request_id)
        .await
        .map_err(ApiError::from)?;
    state
        .acl
        .check(&actor, AclTarget::Doc(&pending.doc_id), AclRole::Edit)
        .await
        .map_err(ApiError::from)?;
    let req = state
        .requests
        .reject(&request_id, &body.reviewed_by, body.note.as_deref())
        .await
        .map_err(ApiError::from)?;
    Ok(Json(req.into()))
}

/// `GET /docs/{doc_id}/requests` — review history of one document.
pub async fn list_doc_requests(
    state: ApiState,
    headers: HeaderMap,
    Path(doc_id): Path<String>,
) -> ApiResult<Json<Vec<UpdateRequestDto>>> {
    validate_doc_id(&doc_id)?;
    // Request history quotes proposed content — gate on doc `view`.
    let actor = actor_from(&headers);
    state
        .acl
        .check(&actor, AclTarget::Doc(&doc_id), AclRole::View)
        .await
        .map_err(ApiError::from)?;
    let reqs = state
        .requests
        .list_for_doc(&doc_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(reqs.into_iter().map(Into::into).collect()))
}

