//! `DocJson` — the JSON extractor for request bodies that carry **document
//! content**.
//!
//! ## Why not `axum::Json`
//!
//! Axum's built-in `Json` extractor consults `DefaultBodyLimit` and, when the
//! body is too large, rejects with a plain-text 413 *before* the handler runs.
//! Two consequences made `DocConfig::max_doc_size_bytes` a lie:
//!
//! 1. The configured limit was never applied (nothing read the field), so the
//!    effective ceiling was axum's built-in 2 MiB default; raising the config
//!    changed nothing.
//! 2. The rejection body was `Failed to buffer the request body: length limit
//!    exceeded` — not the documented `payload_too_large` JSON envelope. A
//!    caller could not distinguish "too large" from "malformed JSON", and the
//!    `DocError::PayloadTooLarge` variant was never constructed anywhere.
//!
//! This extractor reads the body itself with an explicit ceiling taken from
//! `DocState`, so oversize failures come back as the same JSON envelope as
//! every other error, with a machine-readable `payload_too_large` code.
//!
//! The ceiling enforced here is the **transport** ceiling
//! ([`crate::limit::transport_ceiling`]), not the document limit: JSON
//! escaping inflates the wire body, so the document limit is measured after
//! decoding, by [`crate::limit::ensure_document_size`] in the handlers.

use axum::body::to_bytes;
use axum::extract::{FromRequest, Request};
use serde::de::DeserializeOwned;

use crate::api::ApiError;
use crate::error::DocError;
use crate::state::DocState;

/// JSON body extractor bounded by `DocConfig::max_doc_size_bytes`.
pub struct DocJson<T>(pub T);

impl<T> FromRequest<DocState> for DocJson<T>
where
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &DocState) -> Result<Self, Self::Rejection> {
        let max = state.config.max_doc_size_bytes;
        let ceiling = crate::limit::transport_ceiling(max);
        // A declared length lets us reject before reading a single byte.
        let declared = req
            .headers()
            .get(axum::http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok());
        if let Some(size) = declared.filter(|size| *size > ceiling) {
            return Err(DocError::PayloadTooLarge {
                size: Some(size),
                max,
            }
            .into());
        }
        let bytes = to_bytes(req.into_body(), ceiling).await.map_err(|_| {
            // Undeclared (chunked) body that crossed the ceiling mid-stream.
            ApiError::from(DocError::PayloadTooLarge { size: None, max })
        })?;
        serde_json::from_slice(&bytes)
            .map(DocJson)
            .map_err(|e| DocError::BadRequest(format!("invalid JSON body: {e}")).into())
    }
}
