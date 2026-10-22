//! REST API layer.
//!
//! Each handler delegates to a service trait and **only** to a service
//! trait (mirrors `runtime::usecases::services`). No handler touches
//! `crate::store::*` or `crate::types` directly.
//!
//! Error handling: services return `crate::error::DocError`; `ApiError`
//! below converts it to a structured JSON envelope (matching design
//! §4 / §5.1). The same envelope is reused by the MCP layer later.

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::error::DocError;
use crate::service::acl::ActorCtx;
use crate::state::DocState;

pub mod dirs;
pub mod docs;
pub mod dto;
pub mod permissions;
pub mod requests;
pub mod router;
pub mod search;
pub mod trash;

/// Application-level error wrapping `DocError` with HTTP status + a
/// stable JSON error code so the desktop UI / MCP clients can pattern-
/// match on `code` rather than human-readable `message`.
#[derive(Debug)]
pub struct ApiError(pub DocError);

impl From<DocError> for ApiError {
    fn from(e: DocError) -> Self {
        Self(e)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = match &self.0 {
            DocError::DocNotFound(_)
            | DocError::DirNotFound(_)
            | DocError::RequestNotFound(_)
            | DocError::TrashMissing(_)
            | DocError::InvalidId(_) => (StatusCode::NOT_FOUND, "not_found"),
            DocError::BadRequest(_)
            | DocError::PathTraversal(_)
            | DocError::ReservedName(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            DocError::PayloadTooLarge { .. } => {
                (StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large")
            }
            DocError::VersionConflict { .. } => {
                (StatusCode::CONFLICT, "version_conflict")
            }
            DocError::NameConflict(_) => (StatusCode::CONFLICT, "name_conflict"),
            DocError::AlreadyReviewed(_) => (StatusCode::CONFLICT, "already_reviewed"),
            DocError::RequestExpired(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "request_expired")
            }
            DocError::Forbidden(_) | DocError::AclDenied { .. } => {
                (StatusCode::FORBIDDEN, "not_authorized")
            }
            DocError::Io(_) | DocError::Serde(_) | DocError::Internal(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
            DocError::CorruptIndex(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "corrupt_index")
            }
        };
        // `required` rides on 403s so the Desktop can pick the precise
        // localized permission message (ADR-087 D5.3 contract).
        let required = match &self.0 {
            DocError::AclDenied { required, .. } => Some(*required),
            _ => None,
        };
        let body = Json(json!({
            "error": {
                "code": code,
                "message": self.0.to_string(),
                "required": required,
            }
        }));
        (status, body).into_response()
    }
}

/// Result alias used by all API handlers.
pub type ApiResult<T> = Result<T, ApiError>;

/// Convenience: `State<DocState>` is used by every handler.
pub type ApiState = axum::extract::State<DocState>;

/// Resolve the caller identity from the Gateway-injected trusted headers.
///
/// The Gateway reverse proxy strips client-claimed `X-Actor` and injects
/// the authenticated identity (ADR-076 §决策 10) plus `x-auth-role` when
/// an account token backs the request; anything reaching here unstripped
/// was set by the Gateway and is therefore trustworthy.
pub fn actor_from(headers: &HeaderMap) -> ActorCtx {
    let id = headers
        .get("x-actor")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let is_admin = headers
        .get(acowork_core::auth::AUTH_ROLE_HEADER)
        .and_then(|v| v.to_str().ok())
        == Some("admin");
    ActorCtx { id, is_admin }
}
