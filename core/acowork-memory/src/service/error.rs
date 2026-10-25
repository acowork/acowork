//! acowork-memory service error type (ADR-101).
//!
//! Covers both service surfaces: the panel-facing REST API and the MCP
//! endpoint. MCP handlers do not return these directly — they map them onto
//! JSON-RPC error codes (see `service::mcp`).
//!
//! The REST representation is a **bare status code with an empty body**, on
//! purpose: the panel endpoints (`/memory/nodes`, `/memory/stats`, …) are
//! byte-compatible ports of the Runtime handlers they replace, and those
//! returned `Result<Json<_>, StatusCode>`. Detailed reasons go to the log.

use axum::response::{IntoResponse, Response};
use thiserror::Error;

use crate::sqlite::Error as SqliteError;

/// JSON-RPC error code: malformed JSON body.
pub const PARSE_ERROR: i32 = -32700;
/// JSON-RPC error code: not a valid request envelope.
pub const INVALID_REQUEST: i32 = -32600;
/// JSON-RPC error code: unknown method.
pub const METHOD_NOT_FOUND: i32 = -32601;
/// JSON-RPC error code: the call's own parameters are unusable.
pub const INVALID_PARAMS: i32 = -32602;
/// JSON-RPC error code: server-side failure.
pub const INTERNAL_ERROR: i32 = -32603;
/// Custom code in the server-error range: the caller lacks the identity the
/// operation needs. The MCP client classifies it as `[permission]` and never
/// retries it (same code acowork-doc / acowork-pm emit).
pub const CODE_FORBIDDEN: i32 = -32002;

/// All errors produced by the memory service.
#[derive(Debug, Error)]
pub enum ServiceError {
    // ── Instance namespace ────────────────────────────────────────────
    /// No store exists / can be opened for the requested instance.
    #[error("memory instance not found: {0}")]
    InstanceNotFound(String),

    /// The supplied `instance_id` is not usable as a directory name
    /// (path-traversal guard, ADR-101 decision 3).
    #[error("invalid instance id: {0}")]
    InvalidInstanceId(String),

    // ── Validation ────────────────────────────────────────────────────
    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("memory node not found: {0}")]
    NodeNotFound(u64),

    /// This instance has no embedding rebuild on record (`GET
    /// /memory/rebuild-progress` before any run started, or after a service
    /// restart cleared the in-memory slots). REST-only — the Runtime answered
    /// the same 404 from its empty process-wide slot.
    #[error("no rebuild record for instance: {0}")]
    RebuildNotFound(String),

    /// The operation collides with the same instance's in-flight work
    /// (a second rebuild while one is already running). REST-only: the MCP
    /// surface has no long-running operation to collide with.
    #[error("conflict: {0}")]
    Conflict(String),

    // ── Authorization ─────────────────────────────────────────────────
    /// Write attempted without an actor identity (`X-MCP-Actor` missing).
    #[error("forbidden: {0}")]
    Forbidden(String),

    // ── Dependencies ──────────────────────────────────────────────────
    /// The embedding backend (acowork-embed) could not produce a vector.
    /// Retrieval degrades to the lexical path rather than failing.
    #[error("embedding backend unavailable: {0}")]
    EmbeddingUnavailable(String),

    /// The instance store was opened read-only or the pipeline is disabled,
    /// so the operation cannot be served (mirrors the Runtime's 503).
    #[error("service unavailable: {0}")]
    Unavailable(String),

    // ── Storage / plumbing ────────────────────────────────────────────
    #[error("storage error: {0}")]
    Storage(#[from] SqliteError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias for `Result<T, ServiceError>`.
pub type Result<T> = std::result::Result<T, ServiceError>;

impl ServiceError {
    /// HTTP status mapping.
    ///
    /// 400 invalid input, 403 missing actor, 404 unknown instance / node /
    /// rebuild record, 409 a same-instance operation is already running, 503 a
    /// dependency is not serving (embedding, store not initialized), 500
    /// everything else.
    pub fn http_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            ServiceError::InvalidInstanceId(_) | ServiceError::BadRequest(_) => {
                StatusCode::BAD_REQUEST
            }
            ServiceError::Conflict(_) => StatusCode::CONFLICT,
            ServiceError::Forbidden(_) => StatusCode::FORBIDDEN,
            ServiceError::InstanceNotFound(_)
            | ServiceError::NodeNotFound(_)
            | ServiceError::RebuildNotFound(_) => StatusCode::NOT_FOUND,
            ServiceError::EmbeddingUnavailable(_) | ServiceError::Unavailable(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            ServiceError::Storage(_) | ServiceError::Io(_) | ServiceError::Serde(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
            ServiceError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Machine-readable code, for logs and the JSON-RPC error `message`.
    pub fn code(&self) -> &'static str {
        match self {
            ServiceError::InstanceNotFound(_) => "instance_not_found",
            ServiceError::InvalidInstanceId(_) => "invalid_instance_id",
            ServiceError::BadRequest(_) => "bad_request",
            ServiceError::Conflict(_) => "conflict",
            ServiceError::NodeNotFound(_) => "node_not_found",
            ServiceError::RebuildNotFound(_) => "rebuild_not_found",
            ServiceError::Forbidden(_) => "not_authorized",
            ServiceError::EmbeddingUnavailable(_) => "embedding_unavailable",
            ServiceError::Unavailable(_) => "service_unavailable",
            ServiceError::Storage(_) => "storage_error",
            ServiceError::Io(_) | ServiceError::Serde(_) | ServiceError::Internal(_) => {
                "internal_error"
            }
        }
    }

    /// JSON-RPC error code for the MCP endpoint.
    ///
    /// - `Forbidden` → `-32002` (client-side `[permission]`, not retried)
    /// - `InvalidInstanceId` / `BadRequest` → `-32602` (the call itself was
    ///   unusable, so it belongs to the params class)
    /// - everything else → `-32603` (server-side failure)
    pub fn mcp_error_code(&self) -> i32 {
        match self {
            ServiceError::Forbidden(_) => CODE_FORBIDDEN,
            ServiceError::InvalidInstanceId(_) | ServiceError::BadRequest(_) => INVALID_PARAMS,
            _ => INTERNAL_ERROR,
        }
    }

    /// JSON-RPC error `message`: the machine code plus the operation-specific
    /// detail, without the human `Display` prefix repeated twice. The prefix
    /// is what lets the client bucket `[permission]` / `[permanent]`.
    pub fn mcp_message(&self) -> String {
        let detail = match self {
            ServiceError::InstanceNotFound(s)
            | ServiceError::InvalidInstanceId(s)
            | ServiceError::BadRequest(s)
            | ServiceError::Forbidden(s)
            | ServiceError::EmbeddingUnavailable(s)
            | ServiceError::Unavailable(s)
            | ServiceError::Internal(s) => s.clone(),
            ServiceError::NodeNotFound(id) => id.to_string(),
            ServiceError::RebuildNotFound(s) => s.clone(),
            ServiceError::Conflict(s) => s.clone(),
            ServiceError::Storage(e) => e.to_string(),
            ServiceError::Io(e) => e.to_string(),
            ServiceError::Serde(e) => e.to_string(),
        };
        format!("{}: {detail}", self.code())
    }
}

impl IntoResponse for ServiceError {
    fn into_response(self) -> Response {
        if self.http_status().is_server_error() {
            tracing::error!(code = self.code(), error = %self, "memory service request failed");
        } else {
            tracing::debug!(code = self.code(), error = %self, "memory service request rejected");
        }
        self.http_status().into_response()
    }
}

// `#[from] SqliteError` above already provides `From<crate::sqlite::Error>`.

/// The admin trait ([`crate::MemoryAdminService`]) speaks `AcoworkError`, and
/// CRUD calls from the REST handlers land there. Such a failure is always a
/// server-side problem on this surface: the routes that must distinguish a
/// missing node match the message themselves, the way the Runtime's
/// `MemoryAdminAdapter` did, so no richer variant is invented here.
impl From<acowork_core::error::AcoworkError> for ServiceError {
    fn from(e: acowork_core::error::AcoworkError) -> Self {
        ServiceError::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping() {
        assert_eq!(
            ServiceError::InvalidInstanceId("x".into()).http_status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ServiceError::BadRequest("x".into()).http_status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ServiceError::Forbidden("x".into()).http_status(),
            axum::http::StatusCode::FORBIDDEN
        );
        assert_eq!(
            ServiceError::InstanceNotFound("u".into()).http_status(),
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            ServiceError::NodeNotFound(1).http_status(),
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            ServiceError::RebuildNotFound("u".into()).http_status(),
            axum::http::StatusCode::NOT_FOUND
        );
        assert_eq!(
            ServiceError::EmbeddingUnavailable("down".into()).http_status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ServiceError::Unavailable("x".into()).http_status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ServiceError::Conflict("x".into()).http_status(),
            axum::http::StatusCode::CONFLICT
        );
    }

    #[test]
    fn mcp_codes_bucket_by_class() {
        assert_eq!(ServiceError::Forbidden("x".into()).mcp_error_code(), -32002);
        assert_eq!(
            ServiceError::InvalidInstanceId("x".into()).mcp_error_code(),
            -32602
        );
        assert_eq!(
            ServiceError::BadRequest("x".into()).mcp_error_code(),
            -32602
        );
        assert_eq!(
            ServiceError::Internal("x".into()).mcp_error_code(),
            INTERNAL_ERROR
        );
    }

    #[test]
    fn codes_are_stable() {
        assert_eq!(ServiceError::NodeNotFound(7).code(), "node_not_found");
        assert_eq!(
            ServiceError::RebuildNotFound("x".into()).code(),
            "rebuild_not_found"
        );
        assert_eq!(ServiceError::Conflict("x".into()).code(), "conflict");
        assert_eq!(ServiceError::Forbidden("x".into()).code(), "not_authorized");
        assert_eq!(ServiceError::Internal("x".into()).code(), "internal_error");
    }

    #[test]
    fn mcp_message_is_code_plus_detail_without_double_prefix() {
        assert_eq!(
            ServiceError::BadRequest("Missing required parameter 'content'".into()).mcp_message(),
            "bad_request: Missing required parameter 'content'"
        );
        assert_eq!(
            ServiceError::NodeNotFound(7).mcp_message(),
            "node_not_found: 7"
        );
    }

    /// An admin-trait failure keeps its message for the log and lands in the
    /// 500 bucket — the REST routes that need 404 read the message themselves.
    #[test]
    fn admin_errors_are_internal_500s() {
        let e: ServiceError =
            acowork_core::error::AcoworkError::Memory("node 3 not found".into()).into();
        assert_eq!(
            e.http_status(),
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(e.code(), "internal_error");
        // The store's own wording survives (`AcoworkError::Memory` prepends its
        // variant name), which is what `PUT /memory/nodes/{nid}` matches on.
        assert_eq!(
            e.to_string(),
            "internal error: Memory error: node 3 not found"
        );
        assert!(e.to_string().contains("not found"));
    }
}

