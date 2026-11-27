//! Agent-to-agent transfer area (ADR-122 §4.3, D12): the blob store an
//! `intent` attachment passes through.
//!
//! Why this lives in the user service and not in a Runtime: the sender's
//! Runtime cannot write the receiver's workspace (ADR-009 private boundary),
//! and at send time the receiver may not even be running (intent delivery is
//! asynchronous), so the bytes need one neutral, durable landing zone that
//! both sides can name. The Gateway is deliberately **not** that zone — it
//! keeps no bytes (D12), it only reverse-proxies and pins authorization.
//!
//! Trust model is the existing one for `/internal/*`: the Gateway is the only
//! caller (this process binds loopback) and it is the only writer of the two
//! identity headers here — [`CREATOR_HEADER`] after it verified the sending
//! agent instance, [`READER_HEADER`] after it verified the receiving one. The
//! creator is recorded at `create` and is what the Gateway checks an intent
//! attachment against; a record's `granted_to` list is what makes a pull
//! legal, and only the creator may extend it.
//!
//! Ceiling (ADR-122 §6, deliberate): bytes travel two hops (sender → store →
//! receiver) because store-and-forward is what an asynchronous intent needs.
//! `ponytail:` no sha256 fast-path dedup — a re-sent identical file moves its
//! bytes again. Upgrade path is the ADR's: a digest index before concurrency.

use std::path::{Path, PathBuf};

use acowork_core::upload::{CHUNK_BYTES, Completion, UploadError, UploadStore};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::http::chat_api::DOWNLOAD_CHUNK_BYTES;
use crate::state::AppState;

/// The agent instance the Gateway verified as the uploader (ADR-122 D11).
pub const CREATOR_HEADER: &str = "x-acowork-creator-instance";
/// The agent instance the Gateway verified as the reader (ADR-122 D13).
pub const READER_HEADER: &str = "x-acowork-reader-instance";

/// A transfer outlives its intent by a day, then the TTL sweep takes it
/// (ADR-122 D10). Same number as the chunk store, one policy.
const TRANSFER_TTL_SECS: i64 = 24 * 3600;

/// Slack over one chunk so the store, not axum, produces the refusal.
const CHUNK_BODY_LIMIT: usize = CHUNK_BYTES as usize + 64 * 1024;

/// What `complete` landed, and who may read it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRecord {
    /// The upload id it was created with — one id for both phases, so a
    /// client never has to translate one coordinate into another.
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mime: String,
    pub size: u64,
    pub sha256: String,
    /// Who the Gateway verified as the sender. An intent referencing this id
    /// is only accepted from this instance (ADR-122 §4.3).
    pub creator_instance_id: String,
    pub created_at: i64,
    /// Instance ids allowed to pull the bytes. The Gateway appends the intent
    /// target before delivering, which is the receive-side authorization.
    #[serde(default)]
    pub granted_to: Vec<String>,
}

/// The transfer area: a chunk store plus the landed blobs and their records.
pub struct Transfers {
    chunks: UploadStore,
    /// Landed payloads. Named `blobs`, not `files`, on purpose: this store owns
    /// its own layout, and the Chat pair tree's `files/` is a different domain
    /// (ADR-076 §决策 8 guards that one against a second derivation).
    blobs: PathBuf,
    /// Unix seconds of the last opportunistic sweep. In memory because a
    /// restart only means one extra walk, and a persisted value would need
    /// its own file for no benefit.
    last_sweep: std::sync::Mutex<i64>,
}

impl Transfers {
    pub fn new(data_dir: &Path) -> Self {
        let root = data_dir.join("transfers");
        Self {
            chunks: UploadStore::new(root.join(".uploads")),
            blobs: root.join("blobs"),
            last_sweep: std::sync::Mutex::new(0),
        }
    }

    /// `blobs/<id>` — the landed payload.
    fn blob(&self, id: &str) -> PathBuf {
        self.blobs.join(id)
    }

    /// `blobs/<id>.json` — the record, beside the blob so a sweep and a read
    /// both see them as one unit.
    fn record_path(&self, id: &str) -> PathBuf {
        self.blobs.join(format!("{id}.json"))
    }

    /// The record for `id`, or `None` when nothing was landed under it.
    pub async fn record(&self, id: &str) -> Option<TransferRecord> {
        // Only a UUID reaches a path, exactly as the blob routes do.
        if uuid::Uuid::parse_str(id).is_err() {
            return None;
        }
        let raw = tokio::fs::read_to_string(self.record_path(id)).await.ok()?;
        serde_json::from_str(&raw).ok()
    }

    async fn write_record(&self, record: &TransferRecord) -> Result<(), String> {
        tokio::fs::create_dir_all(&self.blobs)
            .await
            .map_err(|e| format!("{}: {e}", self.blobs.display()))?;
        let json = serde_json::to_string(record).map_err(|e| e.to_string())?;
        tokio::fs::write(self.record_path(&record.id), json)
            .await
            .map_err(|e| format!("{}: {e}", self.record_path(&record.id).display()))
    }

    /// Allow `instance` to pull this transfer. Idempotent: the Gateway may
    /// re-pin a redelivered intent.
    pub async fn grant(&self, id: &str, instance: &str) -> Result<TransferRecord, ApiError> {
        let mut record = self
            .record(id)
            .await
            .ok_or_else(|| ApiError::not_found("transfer not found"))?;
        if !record.granted_to.iter().any(|g| g == instance) {
            record.granted_to.push(instance.to_string());
            self.write_record(&record)
                .await
                .map_err(|e| ApiError::internal(&e))?;
        }
        Ok(record)
    }

    /// Delete chunk leftovers and landed transfers past the TTL, at most once
    /// an hour. Called from `create` — the one moment a transfer arrives, so
    /// the sweep needs no timer of its own.
    async fn sweep_if_due(&self) {
        let now = chrono::Utc::now().timestamp();
        {
            let Ok(mut last) = self.last_sweep.lock() else {
                return;
            };
            if now - *last < 3600 {
                return;
            }
            *last = now;
        }
        let purged = self.chunks.sweep(TRANSFER_TTL_SECS).await;
        let mut removed = 0usize;
        if let Ok(mut entries) = tokio::fs::read_dir(&self.blobs).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let stale = entry
                    .metadata()
                    .await
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age.as_secs() as i64 > TRANSFER_TTL_SECS)
                    .unwrap_or(false);
                if stale && tokio::fs::remove_file(entry.path()).await.is_ok() {
                    removed += 1;
                }
            }
        }
        if purged > 0 || removed > 0 {
            tracing::info!(purged, removed, "transfer store swept");
        }
    }
}

/// The Gateway-only transfer surface (mounted under `/internal/transfers`).
pub fn transfer_routes() -> Router<AppState> {
    Router::new()
        .route("/internal/transfers/uploads", post(create_upload))
        .route(
            "/internal/transfers/uploads/{upload_id}",
            axum::routing::head(head_upload).delete(abort_upload),
        )
        .route(
            "/internal/transfers/uploads/{upload_id}/complete",
            post(complete_upload),
        )
        .route(
            "/internal/transfers/uploads/{upload_id}/{offset}",
            put(put_chunk).layer(DefaultBodyLimit::max(CHUNK_BODY_LIMIT)),
        )
        .route("/internal/transfers/{transfer_id}/meta", get(get_meta))
        .route("/internal/transfers/{transfer_id}/content", get(get_content))
        .route("/internal/transfers/{transfer_id}/grant", post(grant_access))
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

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn creator_of(headers: &HeaderMap) -> Result<String, ApiError> {
    header_str(headers, CREATOR_HEADER)
        .map(str::to_string)
        .ok_or_else(|| ApiError::bad_request("missing creator identity"))
}

#[derive(Debug, Deserialize)]
struct CreateTransferBody {
    #[serde(default)]
    name: String,
    size: u64,
    sha256: String,
}

async fn create_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateTransferBody>,
) -> Result<Response, ApiError> {
    let creator = creator_of(&headers)?;
    state.transfers.sweep_if_due().await;
    let id = state
        .transfers
        .chunks
        .create_owned(&body.name, body.size, &body.sha256, Some(&creator))
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
    AxumPath((upload_id, offset)): AxumPath<(String, u64)>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let received = state
        .transfers
        .chunks
        .put(&upload_id, offset, &body)
        .await
        .map_err(store_error)?;
    Ok(Json(serde_json::json!({ "received": received })).into_response())
}

async fn head_upload(
    State(state): State<AppState>,
    AxumPath(upload_id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let progress = state
        .transfers
        .chunks
        .head(&upload_id)
        .await
        .map_err(store_error)?;
    Ok(Json(serde_json::json!({
        "received": progress.received,
        "size": progress.size,
        "chunkBytes": progress.chunk_bytes,
        "completed": progress.completed,
    }))
    .into_response())
}

async fn abort_upload(
    State(state): State<AppState>,
    AxumPath(upload_id): AxumPath<String>,
) -> Result<Response, ApiError> {
    state
        .transfers
        .chunks
        .discard(&upload_id)
        .await
        .map_err(store_error)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[derive(Debug, Default, Deserialize)]
struct CompleteQuery {
    #[serde(default)]
    mime: String,
}

/// Land the merged bytes and write the record the intent path reads.
///
/// The creator comes from the store's own `create` record rather than from a
/// header on this request: the identity that reserved the id is the one the
/// Gateway will check an intent against, and a later request must not be able
/// to restate it.
async fn complete_upload(
    State(state): State<AppState>,
    AxumPath(upload_id): AxumPath<String>,
    Query(q): Query<CompleteQuery>,
) -> Result<Response, ApiError> {
    match state
        .transfers
        .chunks
        .complete(&upload_id)
        .await
        .map_err(store_error)?
    {
        // Already landed: hand back the same record, do not overwrite.
        Completion::Replay(v) => Ok(Json(v).into_response()),
        Completion::Staged(c) => {
            let Some(creator) = c.creator().map(str::to_string) else {
                c.abandon().await;
                return Err(ApiError::internal("transfer upload has no creator"));
            };
            let record = TransferRecord {
                id: upload_id.clone(),
                name: c.name().to_string(),
                mime: if q.mime.trim().is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    q.mime.clone()
                },
                size: c.size(),
                sha256: c.sha256().to_string(),
                creator_instance_id: creator,
                created_at: chrono::Utc::now().timestamp(),
                granted_to: Vec::new(),
            };
            if let Err(e) = state.transfers.write_record(&record).await {
                c.abandon().await;
                return Err(ApiError::internal(&e));
            }
            let blob = state.transfers.blob(&record.id);
            if let Err(e) = tokio::fs::rename(c.path(), &blob).await {
                c.abandon().await;
                return Err(ApiError::internal(&format!(
                    "{}: {e}",
                    blob.display()
                )));
            }
            let landed = serde_json::to_value(&record).unwrap_or_default();
            c.commit(landed.clone()).await.map_err(store_error)?;
            Ok(Json(landed).into_response())
        }
    }
}

/// Read `id` and require `instance` to be on its grant list.
async fn authorized(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<TransferRecord, ApiError> {
    let reader = header_str(headers, READER_HEADER)
        .ok_or_else(|| ApiError::bad_request("missing reader identity"))?;
    let record = state
        .transfers
        .record(id)
        .await
        .ok_or_else(|| ApiError::not_found("transfer not found"))?;
    if record.creator_instance_id != reader && !record.granted_to.iter().any(|g| g == reader) {
        // 404, not 403: an unauthorized reader learns nothing about whether
        // the id exists, which is also what the blob routes answer.
        return Err(ApiError::not_found("transfer not found"));
    }
    Ok(record)
}

async fn get_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(transfer_id): AxumPath<String>,
) -> Result<Json<TransferRecord>, ApiError> {
    Ok(Json(authorized(&state, &headers, &transfer_id).await?))
}

#[derive(Debug, Deserialize)]
struct GrantBody {
    instance: String,
}

/// Allow one instance to pull this transfer. Only the creator may extend the
/// list — the Gateway calls this after it has matched an intent's sender to
/// this record, so a receiver cannot grant itself anything.
async fn grant_access(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(transfer_id): AxumPath<String>,
    Json(body): Json<GrantBody>,
) -> Result<Json<TransferRecord>, ApiError> {
    let creator = creator_of(&headers)?;
    let instance = body.instance.trim();
    if instance.is_empty() {
        return Err(ApiError::bad_request("instance must not be empty"));
    }
    let record = state
        .transfers
        .record(&transfer_id)
        .await
        .ok_or_else(|| ApiError::not_found("transfer not found"))?;
    if record.creator_instance_id != creator {
        return Err(ApiError::forbidden("only the creator may grant access"));
    }
    state
        .transfers
        .grant(&transfer_id, instance)
        .await
        .map(Json)
}

/// Stream the bytes to an authorized reader (ADR-122 D13).
async fn get_content(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(transfer_id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let record = authorized(&state, &headers, &transfer_id).await?;
    let path = state.transfers.blob(&record.id);
    let file = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ApiError::not_found(&format!("{}: {e}", path.display())))?;
    let stream = tokio_util::io::ReaderStream::with_capacity(file, DOWNLOAD_CHUNK_BYTES);
    let mut response = Response::new(Body::from_stream(stream));
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&record.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    out.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&crate::chat::content_disposition(&record.name))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    out.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    // No `Content-Length`: `Body::from_stream` is length-less by design, and
    // hyper falls back to chunked — which is what lets the receiver start
    // writing to disk before the store has finished reading.
    Ok(response)
}

/// Used by the sweep test and by `create`'s throttle path.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support;

    fn state(dir: &Path) -> AppState {
        test_support::multi_user_state(dir)
    }

    /// The whole transfer lifecycle over HTTP: reserve, stream two chunks,
    /// land, then read the record and the bytes back — and prove an
    /// unauthorized instance is answered 404 rather than the file.
    #[tokio::test]
    async fn transfer_round_trip_lands_a_record_and_gates_reads() {
        let dir = test_support::temp_dir("transfers-roundtrip");
        let state = state(&dir);
        let app = test_support::build_router(state);

        let bytes: Vec<u8> = (0..CHUNK_BYTES as usize + 7).map(|i| i as u8).collect();
        let digest = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(&bytes);
            h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>()
        };

        // create
        let created = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/transfers/uploads")
                    .header(CREATOR_HEADER, "inst-sender")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "name": "brief.pdf",
                            "size": bytes.len(),
                            "sha256": digest,
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let body = axum::body::to_bytes(created.into_body(), 64 * 1024).await.unwrap();
        let id = serde_json::from_slice::<serde_json::Value>(&body).unwrap()["uploadId"]
            .as_str()
            .unwrap()
            .to_string();

        // two chunks
        for (n, chunk) in bytes.chunks(CHUNK_BYTES as usize).enumerate() {
            let resp = app
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .method("PUT")
                        .uri(format!("/internal/transfers/uploads/{id}/{}", n * CHUNK_BYTES as usize))
                        .body(Body::from(chunk.to_vec()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        // complete
        let landed = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/internal/transfers/uploads/{id}/complete?mime=application/pdf"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(landed.status(), StatusCode::OK);
        let record: TransferRecord = serde_json::from_slice(
            &axum::body::to_bytes(landed.into_body(), 64 * 1024).await.unwrap(),
        )
        .unwrap();
        assert_eq!(record.creator_instance_id, "inst-sender");
        assert_eq!(record.size, bytes.len() as u64);
        assert!(record.granted_to.is_empty());

        // a stranger may not read
        let denied = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/internal/transfers/{id}/content"))
                    .header(READER_HEADER, "inst-stranger")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);

        // grant, then read
        let granted = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/internal/transfers/{id}/grant"))
                    .header(CREATOR_HEADER, "inst-sender")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"instance":"inst-receiver"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(granted.status(), StatusCode::OK);

        let content = app
            .oneshot(
                axum::http::Request::builder()
                    .uri(format!("/internal/transfers/{id}/content"))
                    .header(READER_HEADER, "inst-receiver")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(content.status(), StatusCode::OK);
        let got = axum::body::to_bytes(content.into_body(), usize::MAX).await.unwrap();
        assert_eq!(got.as_ref(), bytes.as_slice(), "bytes arrive intact");
    }

    use tower::ServiceExt as _;

    /// `create` without a creator identity would land a transfer nobody is
    /// accountable for — the Gateway injects that header, so its absence means
    /// the request bypassed the Gateway.
    #[tokio::test]
    async fn create_without_a_creator_is_refused() {
        let dir = test_support::temp_dir("transfers-nocreator");
        let app = test_support::build_router(state(&dir));
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/internal/transfers/uploads")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({"name":"x","size":4,"sha256":"0".repeat(64)}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// The record's `created_at`/grant list survive a round trip through disk,
    /// which is what the intent path later reads.
    #[tokio::test]
    async fn record_survives_a_reload() {
        let dir = test_support::temp_dir("transfers-reload");
        let first = state(&dir);
        let record = TransferRecord {
            id: uuid::Uuid::new_v4().to_string(),
            name: "a.bin".into(),
            mime: "application/octet-stream".into(),
            size: 4,
            sha256: "0".repeat(64),
            creator_instance_id: "inst-a".into(),
            created_at: 1,
            granted_to: vec![],
        };
        first.transfers.write_record(&record).await.unwrap();
        first.transfers.grant(&record.id, "inst-b").await.unwrap();

        let second = Transfers::new(&dir);
        let reloaded = second.record(&record.id).await.expect("record");
        assert_eq!(reloaded.creator_instance_id, "inst-a");
        assert_eq!(reloaded.granted_to, vec!["inst-b".to_string()]);
    }
}