//! acowork-doc unified error type.
//!
//! All API handlers / MCP tools return `Result<T, DocError>`.
//! The HTTP layer maps `DocError` to a REST status code plus a JSON body of
//! shape `{"error": {"code", "message", "details"}}` (design §4).
//!
//! Design ref: `docs/design/zh/20-doc-online-document.md` §4 / §9.

use thiserror::Error;

/// All errors produced by the acowork-doc service.
///
/// Each variant carries structured context (doc_id / path) so logs and the
/// frontend can render precise diagnostics. Display output contains no
/// secrets — paths and ids are non-sensitive.
#[derive(Debug, Error)]
pub enum DocError {
    // ── Not found ─────────────────────────────────────────────────────
    #[error("document not found: {0}")]
    DocNotFound(String),

    #[error("directory not found: {0}")]
    DirNotFound(String),

    #[error("update request not found: {0}")]
    RequestNotFound(String),

    // ── Validation ────────────────────────────────────────────────────
    #[error("invalid id: {0}")]
    InvalidId(String),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("path traversal detected: {0}")]
    PathTraversal(String),

    #[error("reserved name cannot be used: {0}")]
    ReservedName(String),

    #[error("payload too large: {size} bytes (max {max})")]
    PayloadTooLarge { size: usize, max: usize },

    // ── Conflict / concurrency ────────────────────────────────────────
    #[error("name already exists in this directory: {0}")]
    NameConflict(String),

    /// Optimistic-version concurrency clash (design §4 / §5.4 DOC-09).
    ///
    /// `base_version` is what the caller supplied (e.g. PUT body); `current_version`
    /// is the version now persisted on disk. Both are `u64` because version
    /// numbers are monotonically increasing natural counters.
    #[error("version conflict: base_version {base_version} does not match current {current_version}")]
    VersionConflict { base_version: u64, current_version: u64 },

    #[error("request already reviewed (status={0})")]
    AlreadyReviewed(String),

    #[error("request expired: {0}")]
    RequestExpired(String),

    #[error("trash slot not found: {0}")]
    TrashMissing(String),

    // ── Authorization ─────────────────────────────────────────────────
    #[error("forbidden: anonymous callers may only read (actor={0:?})")]
    Forbidden(Option<String>),

    /// Node-level ACL denial (design §9). `required` names the capability
    /// tier that refused (`view` / `edit` / `manage`) so the client can
    /// render a precise message without parsing the display text.
    #[error("forbidden: actor {actor:?} lacks `{required}` permission")]
    AclDenied { actor: String, required: &'static str },

    // ── IO / storage ──────────────────────────────────────────────────
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("corrupt library index: {0}")]
    CorruptIndex(String),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Convenience alias for `Result<T, DocError>`.
pub type Result<T> = std::result::Result<T, DocError>;

impl DocError {
    /// HTTP status code mapping (design §4 error table).
    ///
    /// 400 invalid input, 403 forbidden, 404 not found, 409 conflict /
    /// version clash / name clash, 413 payload too large, 422 unprocessable
    /// (expired / already-reviewed requests), 500 internal.
    pub fn http_status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        use DocError::*;
        match self {
            DocNotFound(_) | DirNotFound(_) | RequestNotFound(_) => StatusCode::NOT_FOUND,
            InvalidId(_) | BadRequest(_) | PathTraversal(_) | ReservedName(_) => {
                StatusCode::BAD_REQUEST
            }
            PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            NameConflict(_) | VersionConflict { .. } => StatusCode::CONFLICT,
            AlreadyReviewed(_) | RequestExpired(_) | TrashMissing(_) => StatusCode::NOT_FOUND,
            Forbidden(_) | AclDenied { .. } => StatusCode::FORBIDDEN,
            Io(_) | CorruptIndex(_) | Serde(_) | Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Machine-readable error code (frontend keys i18n / retry logic on this).
    pub fn code(&self) -> &'static str {
        use DocError::*;
        match self {
            DocNotFound(_) => "doc_not_found",
            DirNotFound(_) => "dir_not_found",
            RequestNotFound(_) => "request_not_found",
            InvalidId(_) => "invalid_id",
            BadRequest(_) => "bad_request",
            PathTraversal(_) => "path_traversal",
            ReservedName(_) => "reserved_name",
            PayloadTooLarge { .. } => "payload_too_large",
            NameConflict(_) => "name_conflict",
            VersionConflict { .. } => "version_conflict",
            AlreadyReviewed(_) => "already_reviewed",
            RequestExpired(_) => "request_expired",
            TrashMissing(_) => "trash_missing",
            Forbidden(_) | AclDenied { .. } => "not_authorized",
            Io(_) | CorruptIndex(_) | Serde(_) | Internal(_) => "internal_error",
        }
    }

    /// Render the standard error JSON body `{"error": {...}}` (design §4).
    pub fn to_json_response(&self) -> axum::Json<serde_json::Value> {
        // The Desktop client pattern-matches `required` on 403s to pick the
        // localized permission message (ADR-087 D5.3 contract).
        let required = match self {
            DocError::AclDenied { required, .. } => Some(*required),
            _ => None,
        };
        axum::Json(serde_json::json!({
            "error": {
                "code": self.code(),
                "message": self.to_string(),
                "details": null,
                "required": required,
            }
        }))
    }
}
