//! Chunked resumable upload routes (ADR-122 §3).
//!
//! Two stores on this side of the boundary, one contract:
//!
//! | `{store}` | landing | owner of the bytes |
//! |---|---|---|
//! | `/sessions/{sid}/uploads` | `<work_dir>/files/<stem>_<doc_id>.<ext>` | [`crate::usecases::AttachmentService`] |
//! | `/workspaces/uploads` | atomically replaces an existing workspace file | [`crate::usecases::WorkspaceMutationService`] |
//!
//! The other two stores of the ADR (chat session files, the agent→agent
//! transfer area) belong to the user service and live there; this module owns
//! only what the Runtime lands. Both wrap [`acowork_core::upload::UploadStore`]
//! and add exactly what a store cannot know: who may write, and what the merged
//! bytes become.
//!
//! Why this exists at all (ADR-122 §2): the single-request upload caps a file
//! at the smallest per-hop body limit and makes every hop buffer the whole
//! thing. A 2 MiB chunk keeps every hop small, and the offset is the resume
//! point.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use acowork_core::upload::{CHUNK_BYTES, Completion, UploadError, UploadStore};
use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::http::server::HttpState;
use crate::http::session_control::{authorize_write, scope_from_headers};

/// Chunk-level cap on the `Bytes` extractor for the PUT route.
///
/// Strictly above one chunk so the store — not axum — produces the readable
/// "chunk exceeds N bytes" error, and far below the 64 MiB global cap.
pub const CHUNK_BODY_LIMIT: usize = CHUNK_BYTES as usize + 64 * 1024;

/// Where a chunk stream lands. The store root is picked by the route.
enum Store {
    Session,
    Workspace,
}

/// The two chunk stores, held for the process lifetime.
///
/// Constructed from the boot-time `work_dir`, so it needs no async resource —
/// same pattern as every other usecase slot in this crate.
pub struct UploadStores {
    sessions: UploadStore,
    workspaces: UploadStore,
}

impl UploadStores {
    /// `<work_dir>/.uploads/{sessions,workspaces}` (ADR-122 §3 D5: chunks live
    /// in a temp tree, never next to a landed file).
    pub fn new(work_dir: &std::path::Path) -> Self {
        let root = work_dir.join(".uploads");
        Self {
            sessions: UploadStore::new(root.join("sessions")),
            workspaces: UploadStore::new(root.join("workspaces")),
        }
    }

    /// Sweep abandoned uploads on both stores (ADR-122 D10).
    pub async fn sweep(&self) {
        let ttl = acowork_core::upload::UPLOAD_TTL_SECS;
        let sessions = self.sessions.sweep(ttl).await;
        let workspaces = self.workspaces.sweep(ttl).await;
        if sessions + workspaces > 0 {
            tracing::info!(sessions, workspaces, "swept expired uploads (ADR-122)");
        }
    }

    fn of(&self, store: &Store) -> &UploadStore {
        match store {
            Store::Session => &self.sessions,
            Store::Workspace => &self.workspaces,
        }
    }
}

/// Shared handle, published on [`HttpState`] at startup.
pub type SharedUploadStores = Arc<UploadStores>;

/// `POST …/uploads` body.
#[derive(Debug, Deserialize)]
pub struct CreateUploadBody {
    /// Declared filename — a hint and the on-disk stem, never a path.
    #[serde(default)]
    pub name: String,
    /// Declared total length. Not a fact until `complete` verifies it.
    pub size: u64,
    /// Declared `sha256` (64 lowercase hex). Verified by `complete`.
    pub sha256: String,
    /// Image dimensions for the attachment store (same rules as the multipart
    /// route: present for images, absent otherwise). Ignored by the workspace
    /// store, whose response is disk metadata.
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
}

fn error_response(e: UploadError) -> Response {
    let status = match &e {
        UploadError::NotFound(_) => StatusCode::NOT_FOUND,
        UploadError::BadRequest(_) => StatusCode::BAD_REQUEST,
        UploadError::TooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
        UploadError::Conflict(_) => StatusCode::CONFLICT,
        UploadError::Checksum { .. } => StatusCode::UNPROCESSABLE_ENTITY,
        UploadError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    // 422/409 both mean "chunks are kept, fix and retry" — say so, since the
    // client that reads this is a resumable uploader with a decision to make.
    let mut body = serde_json::json!({ "error": e.to_string() });
    if matches!(e, UploadError::Checksum { .. }) {
        body["retry"] = serde_json::Value::String("re-put".into());
    }
    (status, Json(body)).into_response()
}

fn extension_of(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.rfind('.') {
        Some(i) if i > 0 => base[i + 1..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

// ── Handlers ───────────────────────────────────────────────────────────────
//
// Each route has a session-scoped and a workspace-scoped twin; both are thin
// wrappers over the shared bodies below, because the only differences are the
// authority check and the landing sink.

/// `POST /sessions/{sid}/uploads`
pub(crate) async fn session_create(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    Json(body): Json<CreateUploadBody>,
) -> Response {
    if let Err(e) = authorize_write(&state, &sid, &scope_from_headers(&headers)) {
        return e.into_response();
    }
    create(state.uploads.as_ref().map(|s| s.of(&Store::Session)), body).await
}

/// `POST /workspaces/uploads`
pub(crate) async fn workspace_create(
    State(state): State<HttpState>,
    Json(body): Json<CreateUploadBody>,
) -> Response {
    create(state.uploads.as_ref().map(|s| s.of(&Store::Workspace)), body).await
}

/// `PUT /sessions/{sid}/uploads/{upload_id}/{offset}`
pub(crate) async fn session_put(
    State(state): State<HttpState>,
    Path((sid, upload_id, offset)): Path<(String, String, u64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(e) = authorize_write(&state, &sid, &scope_from_headers(&headers)) {
        return e.into_response();
    }
    put(state.uploads.as_ref().map(|s| s.of(&Store::Session)), &upload_id, offset, &body).await
}

/// `PUT /workspaces/uploads/{upload_id}/{offset}`
pub(crate) async fn workspace_put(
    State(state): State<HttpState>,
    Path((upload_id, offset)): Path<(String, u64)>,
    body: Bytes,
) -> Response {
    put(state.uploads.as_ref().map(|s| s.of(&Store::Workspace)), &upload_id, offset, &body).await
}

/// `HEAD /sessions/{sid}/uploads/{upload_id}`
pub(crate) async fn session_head(
    State(state): State<HttpState>,
    Path((sid, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authorize_write(&state, &sid, &scope_from_headers(&headers)) {
        return e.into_response();
    }
    head(state.uploads.as_ref().map(|s| s.of(&Store::Session)), &upload_id).await
}

/// `HEAD /workspaces/uploads/{upload_id}`
pub(crate) async fn workspace_head(
    State(state): State<HttpState>,
    Path(upload_id): Path<String>,
) -> Response {
    head(state.uploads.as_ref().map(|s| s.of(&Store::Workspace)), &upload_id).await
}

/// `DELETE /sessions/{sid}/uploads/{upload_id}`
pub(crate) async fn session_abort(
    State(state): State<HttpState>,
    Path((sid, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authorize_write(&state, &sid, &scope_from_headers(&headers)) {
        return e.into_response();
    }
    abort(state.uploads.as_ref().map(|s| s.of(&Store::Session)), &upload_id).await
}

/// `DELETE /workspaces/uploads/{upload_id}`
pub(crate) async fn workspace_abort(
    State(state): State<HttpState>,
    Path(upload_id): Path<String>,
) -> Response {
    abort(state.uploads.as_ref().map(|s| s.of(&Store::Workspace)), &upload_id).await
}

/// `POST /sessions/{sid}/uploads/{upload_id}/complete`
///
/// Lands the verified bytes through [`crate::usecases::AttachmentService`], so
/// the response is the same `documentId`-bearing envelope the multipart route
/// returns and the desktop needs no second code path.
pub(crate) async fn session_complete(
    State(state): State<HttpState>,
    Path((sid, upload_id)): Path<(String, String)>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(e) = authorize_write(&state, &sid, &scope_from_headers(&headers)) {
        return e.into_response();
    }
    let Some(store) = state.uploads.as_ref().map(|s| s.of(&Store::Session)) else {
        return unavailable("upload store");
    };
    let svc = match state.attachment.lock().await.clone() {
        Some(svc) => svc,
        None => return unavailable("attachment service"),
    };
    let completion = match store.complete(&upload_id).await {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let completer = match completion {
        Completion::Replay(v) => return Json(v).into_response(),
        Completion::Staged(c) => c,
    };
    let width = q.get("width").and_then(|v| v.parse().ok());
    let height = q.get("height").and_then(|v| v.parse().ok());
    let params = crate::usecases::StagedUploadParams {
        filename: completer.name().to_string(),
        format: extension_of(completer.name()),
        staged: completer.path().to_path_buf(),
        sha256: completer.sha256().to_string(),
        size: completer.size(),
        width,
        height,
    };
    match svc.adopt_staged_file(params).await {
        Ok(landed) => match serde_json::to_value(&landed) {
            Ok(json) => match completer.commit(json).await {
                Ok(result) => Json(result).into_response(),
                Err(e) => error_response(e),
            },
            Err(e) => {
                completer.abandon().await;
                error_response(UploadError::Io(e.to_string()))
            }
        },
        Err(e) => {
            completer.abandon().await;
            super::server::attachment_error_to_response(e).into_response()
        }
    }
}

/// `POST /workspaces/uploads/{upload_id}/complete?workspace_id=…&path=…`
///
/// The target coordinates travel on the query, exactly like
/// `PUT /workspaces/file`, and go through the same traversal guard.
pub(crate) async fn workspace_complete(
    State(state): State<HttpState>,
    Path(upload_id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(store) = state.uploads.as_ref().map(|s| s.of(&Store::Workspace)) else {
        return unavailable("upload store");
    };
    let svc = match state.workspace_mutation.lock().await.clone() {
        Some(svc) => svc,
        None => return unavailable("workspace service"),
    };
    let completion = match store.complete(&upload_id).await {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let completer = match completion {
        Completion::Replay(v) => return Json(v).into_response(),
        Completion::Staged(c) => c,
    };
    let query = crate::usecases::workspace_mutation::FilePathQuery {
        workspace_id: q.get("workspace_id").cloned(),
        path: q.get("path").cloned().unwrap_or_default(),
    };
    let staged: PathBuf = completer.path().to_path_buf();
    match svc.write_file_from_staged(query, staged).await {
        Ok(resp) => {
            let json = resp.entry.unwrap_or(serde_json::json!({"written": resp.ok}));
            match completer.commit(json).await {
                Ok(result) => Json(result).into_response(),
                Err(e) => error_response(e),
            }
        }
        Err(e) => {
            completer.abandon().await;
            super::server::workspace_error_to_response(e).into_response()
        }
    }
}

// ── Shared bodies ──────────────────────────────────────────────────────────

async fn create(store: Option<&UploadStore>, body: CreateUploadBody) -> Response {
    let Some(store) = store else {
        return unavailable("upload store");
    };
    match store.create(&body.name, body.size, &body.sha256).await {
        Ok(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "uploadId": id, "chunkBytes": CHUNK_BYTES })),
        )
            .into_response(),
        Err(e) => error_response(e),
    }
}

async fn put(store: Option<&UploadStore>, upload_id: &str, offset: u64, body: &[u8]) -> Response {
    let Some(store) = store else {
        return unavailable("upload store");
    };
    match store.put(upload_id, offset, body).await {
        // `Received` is the resume cursor the client keeps (ADR-122 D4): it
        // needs no extra HEAD round trip after each chunk.
        Ok(received) => Json(serde_json::json!({ "received": received })).into_response(),
        Err(e) => error_response(e),
    }
}

async fn head(store: Option<&UploadStore>, upload_id: &str) -> Response {
    let Some(store) = store else {
        return unavailable("upload store");
    };
    match store.head(upload_id).await {
        Ok(progress) => {
            let mut resp = Json(&progress).into_response();
            // The tus-shaped header, for clients that read it instead of the
            // body; both carry the same number.
            if let Ok(v) = progress.received.to_string().parse() {
                resp.headers_mut()
                    .insert(axum::http::header::HeaderName::from_static("upload-offset"), v);
            }
            resp
        }
        Err(e) => error_response(e),
    }
}

async fn abort(store: Option<&UploadStore>, upload_id: &str) -> Response {
    let Some(store) = store else {
        return unavailable("upload store");
    };
    match store.discard(upload_id).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => error_response(e),
    }
}

fn unavailable(what: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": format!("{what} not ready") })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_of_reads_the_last_segment_only() {
        assert_eq!(extension_of("report.PDF"), "pdf");
        assert_eq!(extension_of("C:\\tmp\\a.docx"), "docx");
        assert_eq!(extension_of("/tmp/a.tar.gz"), "gz");
        assert_eq!(extension_of("no-ext"), "");
        assert_eq!(extension_of(".gitignore"), "");
    }

    /// The status mapping is a contract with the resumable client: 409 means
    /// "you are out of order / someone else is writing", 422 means "chunks are
    /// kept, re-PUT", 413 means "this file is over the deployment ceiling".
    #[test]
    fn upload_errors_map_to_the_documented_statuses() {
        for (err, expected) in [
            (UploadError::NotFound("x".into()), StatusCode::NOT_FOUND),
            (UploadError::BadRequest("x".into()), StatusCode::BAD_REQUEST),
            (
                UploadError::TooLarge {
                    declared: 2,
                    limit: 1,
                },
                StatusCode::PAYLOAD_TOO_LARGE,
            ),
            (UploadError::Conflict("x".into()), StatusCode::CONFLICT),
            (
                UploadError::Checksum {
                    expected: "a".into(),
                    actual: "b".into(),
                    size: 0,
                },
                StatusCode::UNPROCESSABLE_ENTITY,
            ),
            (UploadError::Io("x".into()), StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            assert_eq!(error_response(err).status(), expected);
        }
    }
}