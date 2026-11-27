//! Chunked resumable upload routes for a chat session's attachment area
//! (ADR-122 §3, `{store}` = `/api/chat-sessions/{session_id}`).
//!
//! The bytes of a chat attachment are the user service's to keep (§4.2.1's
//! "one trusted writer": the same reason D12 puts the agent→agent transfer
//! area here), so this store is not the Runtime's — nothing crosses a process
//! boundary to land a blob, and `complete` renames the merged file straight
//! into `files/<attachment_id>`.
//!
//! The participation rule lands at `complete`, not at `create`: the chunk
//! store is keyed by a server-generated id under a shared `.uploads/` root, so
//! an id collected by the wrong sender addresses nothing a session can see —
//! [`chat_sessions::store_attachment_from_path`] is the one authority check,
//! exactly as it is for the single-request route.

use acowork_core::upload::{CHUNK_BYTES, Completion, UploadError};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{head, post, put},
    Json, Router,
};
use serde::Deserialize;

use crate::auth::context::AuthContext;
use crate::error::ApiError;
use crate::http::chat_sessions_api::offload;
use crate::state::AppState;

/// Cap on the `Bytes` extractor for one chunk: one chunk plus slack so the
/// store, not axum, produces the readable "chunk exceeds N bytes" refusal.
const CHUNK_BODY_LIMIT: usize = CHUNK_BYTES as usize + 64 * 1024;

/// `POST {store}/uploads` — reserve an id for a declared total + digest.
#[derive(Debug, Deserialize)]
pub struct CreateUploadBody {
    /// Filename as the client knows it: a hint, and the name that is stored.
    #[serde(default)]
    pub name: String,
    /// Declared total length — a claim until `complete` verifies it.
    pub size: u64,
    /// Declared `sha256`, 64 hex characters.
    pub sha256: String,
    /// Declared MIME type (the browser's `File.type`). Checked here so an
    /// over-limit file is refused **before** its bytes are sent; the store
    /// itself knows nothing about attachment classes.
    #[serde(default)]
    pub mime: String,
}

/// Query of `POST {store}/uploads/{id}/complete`.
///
/// The chunk stream carries the bytes and the size; the mime type is
/// presentation metadata the store never persisted, so the client states it
/// again here. Missing or malformed degrades to `application/octet-stream` —
/// the same normalization the single-request route applies to the multipart
/// field's `Content-Type`.
#[derive(Debug, Default, Deserialize)]
pub struct CompleteQuery {
    #[serde(default)]
    pub mime: String,
}

/// The four verbs, merged into
/// [`chat_sessions_api::chat_session_routes`](crate::http::chat_sessions_api::chat_session_routes)
/// so every chat-session path stays declared in one place.
pub fn upload_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/chat-sessions/{session_id}/uploads",
            post(create_upload),
        )
        .route(
            "/api/chat-sessions/{session_id}/uploads/{upload_id}",
            head(upload_head).delete(upload_abort),
        )
        .route(
            "/api/chat-sessions/{session_id}/uploads/{upload_id}/complete",
            post(complete_upload),
        )
        .route(
            "/api/chat-sessions/{session_id}/uploads/{upload_id}/{offset}",
            put(put_chunk).layer(DefaultBodyLimit::max(CHUNK_BODY_LIMIT)),
        )
}

fn store_error(e: UploadError) -> ApiError {
    match e {
        UploadError::NotFound(m) => ApiError::not_found(&m),
        UploadError::BadRequest(m) => ApiError::bad_request(&m),
        UploadError::Conflict(m) => ApiError::conflict(&m),
        UploadError::TooLarge { .. } => ApiError::payload_too_large(&e.to_string()),
        UploadError::Checksum { .. } => ApiError::unprocessable_entity(&e.to_string()),
        UploadError::Io(m) => ApiError::internal(&m),
    }
}

async fn create_upload(
    State(state): State<AppState>,
    Json(body): Json<CreateUploadBody>,
) -> Result<Response, ApiError> {
    let limit = crate::chat::limit_for(&body.mime);
    if body.size > limit {
        return Err(ApiError::payload_too_large(&format!(
            "attachment exceeds {limit} bytes"
        )));
    }
    let id = state
        .uploads
        .create(&body.name, body.size, &body.sha256)
        .await
        .map_err(store_error)?;
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "uploadId": id, "chunkBytes": CHUNK_BYTES })),
    )
        .into_response())
}

async fn put_chunk(
    State(state): State<AppState>,
    Path((_session_id, upload_id, offset)): Path<(String, String, u64)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let received = state
        .uploads
        .put(&upload_id, offset, &body)
        .await
        .map_err(store_error)?;
    Ok(Json(serde_json::json!({ "received": received })).into_response())
}

/// `HEAD {store}/uploads/{id}` — the resume point, also echoed as
/// `Upload-Offset` so a client can read it without parsing the body.
async fn upload_head(
    State(state): State<AppState>,
    Path((_session_id, upload_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let progress = state.uploads.head(&upload_id).await.map_err(store_error)?;
    let offset = axum::http::HeaderValue::from_str(&progress.received.to_string())
        .expect("decimal digits are a valid header value");
    let mut resp = Json(&progress).into_response();
    resp.headers_mut()
        .insert(axum::http::header::HeaderName::from_static("upload-offset"), offset);
    Ok(resp)
}

async fn upload_abort(
    State(state): State<AppState>,
    Path((_session_id, upload_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    state
        .uploads
        .discard(&upload_id)
        .await
        .map_err(store_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST {store}/uploads/{id}/complete` — land the merged file.
///
/// Idempotent by replay (ADR-122 §3): a second `complete` returns the recorded
/// attachment instead of storing it twice.
async fn complete_upload(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((session_id, upload_id)): Path<(String, String)>,
    Query(q): Query<CompleteQuery>,
) -> Result<Response, ApiError> {
    let completer = match state.uploads.complete(&upload_id).await.map_err(store_error)? {
        Completion::Replay(v) => return Ok(Json(v).into_response()),
        Completion::Staged(c) => c,
    };
    let from = ctx.effective_user_id().to_string();
    let dir = state.auth_service.data_dir().to_path_buf();
    let filename = completer.name().to_string();
    let size = completer.size();
    let staged = completer.path().to_path_buf();
    let mime = q.mime;

    let stored = offload(move || {
        crate::chat_sessions::store_attachment_from_path(
            &dir, &session_id, &from, &filename, &mime, &staged, size,
        )
    })
    .await;

    let attachment = match stored {
        Ok(a) => a,
        Err(e) => {
            // Landing refused (limits, participation, I/O): keep the chunks so
            // the client can re-`PUT` and retry without re-sending the file
            // (ADR-122 Q3). `store_attachment_from_path` has already dropped
            // the staging file it refused.
            completer.abandon().await;
            return Err(e);
        }
    };
    let json = serde_json::to_value(&attachment)
        .map_err(|e| ApiError::internal(&format!("serialize attachment: {e}")))?;
    completer.commit(json).await.map_err(store_error)?;
    Ok((StatusCode::CREATED, Json(attachment)).into_response())
}

#[cfg(test)]
mod tests {
    use crate::test_support;
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    fn router(dir: &std::path::Path) -> Router {
        test_support::build_router(test_support::admin_state(dir))
    }

    fn req(method: &str, uri: &str, body: Option<&str>, token: Option<&str>) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri);
        if body.is_some() {
            b = b.header("content-type", "application/json");
        }
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(match body {
            Some(s) => Body::from(s.to_string()),
            None => Body::empty(),
        })
        .unwrap()
    }

    async fn json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    async fn login(router: &Router, username: &str, password: &str) -> String {
        let resp = router
            .clone()
            .oneshot(req(
                "POST",
                "/api/auth/login",
                Some(&format!(
                    r#"{{"username":"{username}","password":"{password}"}}"#
                )),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "login as {username}");
        json(resp).await["access_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// A session owned by a fresh account whose token the test can use.
    async fn session_for(app: &Router, admin: &str, username: &str) -> (String, String) {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/users",
                Some(&format!(
                    r#"{{"username":"{username}","password":"s3cret123","role":"user"}}"#
                )),
                Some(admin),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let user = json(resp).await["account"]["user_id"]
            .as_str()
            .unwrap()
            .to_string();
        let token = login(app, username, "s3cret123").await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/chat-sessions",
                Some(&format!(
                    r#"{{"kind":"user_agent","participants":["{user}","agent-1"],"title":"chunk"}}"#
                )),
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let sid = json(resp).await["session_id"].as_str().unwrap().to_string();
        (sid, token)
    }

    /// ADR-122 D5/D10: two chunks merge into one attachment, the digest is
    /// verified, the bytes readable through the normal route are exactly what
    /// was sent, and a second `complete` replays the recorded result instead of
    /// storing a copy.
    #[tokio::test]
    async fn a_two_chunk_upload_lands_one_attachment_and_replays() {
        let dir = test_support::temp_dir("chunkupload");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (sid, token) = session_for(&app, &admin, "carol").await;

        // One byte past a full chunk: the interest is the boundary, not size.
        let chunk = acowork_core::upload::CHUNK_BYTES as usize;
        let mut bytes = vec![7u8; chunk];
        bytes.extend_from_slice(b"tail!");
        let sha256: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/uploads"),
                Some(&format!(
                    r#"{{"name":"report.pdf","size":{},"sha256":"{sha256}","mime":"application/pdf"}}"#,
                    bytes.len()
                )),
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let upload_id = json(resp).await["uploadId"]
            .as_str()
            .unwrap()
            .to_string();

        for offset in (0..bytes.len()).step_by(chunk) {
            let end = std::cmp::min(offset + chunk, bytes.len());
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("PUT")
                        .uri(format!(
                            "/api/chat-sessions/{sid}/uploads/{upload_id}/{offset}"
                        ))
                        .header("authorization", format!("Bearer {token}"))
                        .body(Body::from(bytes[offset..end].to_vec()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "chunk at {offset}");
        }

        // HEAD is the resume point, before the merge.
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("HEAD")
                    .uri(format!("/api/chat-sessions/{sid}/uploads/{upload_id}"))
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("upload-offset")
                .and_then(|v| v.to_str().ok()),
            Some(bytes.len().to_string().as_str())
        );

        let complete = format!(
            "/api/chat-sessions/{sid}/uploads/{upload_id}/complete?mime=application/pdf"
        );
        let resp = app
            .clone()
            .oneshot(req("POST", &complete, None, Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let landed = json(resp).await;
        assert_eq!(landed["filename"].as_str(), Some("report.pdf"));
        assert_eq!(landed["mime"].as_str(), Some("application/pdf"));
        assert_eq!(landed["size"].as_u64(), Some(bytes.len() as u64));

        // The merged bytes are what the session's normal read route serves.
        let attachment_id = landed["id"].as_str().unwrap().to_string();
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chat-sessions/{sid}/files/{attachment_id}"),
                None,
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 24)
            .await
            .unwrap();
        assert_eq!(body.len(), bytes.len());
        assert_eq!(&body[body.len() - 5..], b"tail!");

        // Idempotent replay: the same attachment, nothing stored twice.
        let resp = app
            .clone()
            .oneshot(req("POST", &complete, None, Some(&token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            json(resp).await["id"].as_str(),
            Some(attachment_id.as_str())
        );
    }

    /// The attachment-class ceiling is enforced at `create`, before a byte is
    /// sent — the chunk store itself only knows the deployment-wide limit.
    #[tokio::test]
    async fn an_over_class_limit_declaration_is_refused_before_any_chunk() {
        let dir = test_support::temp_dir("chunklimit");
        let app = router(&dir);
        let admin = login(&app, "root", test_support::PWD).await;
        let (sid, token) = session_for(&app, &admin, "dave").await;

        let too_big = crate::chat::MAX_IMAGE_BYTES + 1;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chat-sessions/{sid}/uploads"),
                Some(&format!(
                    r#"{{"name":"huge.png","size":{too_big},"sha256":"{}","mime":"image/png"}}"#,
                    "0".repeat(64)
                )),
                Some(&token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}