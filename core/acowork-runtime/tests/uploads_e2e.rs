//! ADR-122 end-to-end: the two chunk stores the Runtime owns, over real HTTP.
//!
//! The kernel is unit-tested (`acowork_core::upload`) and the handlers are
//! unit-tested for their error mapping, but neither proves the *chain* a
//! Desktop client actually walks:
//!
//! ```text
//! create → PUT chunk → PUT chunk → HEAD (resume point) → complete → landed bytes
//! ```
//!
//! and, just as importantly, that the single-request multipart route it
//! replaces still lands the same way (D9 keeps it for anything under one
//! chunk).
//!
//! Both stores are exercised because they land in different places and could
//! rot independently: `/sessions/{sid}/uploads` → `AttachmentService` (content
//! addressed blob, dedup), `/workspaces/uploads` → `WorkspaceMutationService`
//! (atomic replace of a workspace path).
//!
//! `RuntimeHttpServer` is started on an ephemeral port with real services over
//! a temp `work_dir`, so nothing here is a mock: the bytes come back off the
//! disk the assertions read.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use acowork_runtime::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use acowork_runtime::http::server::RuntimeHttpServer;
use acowork_runtime::usecases::workspace_mutation_impl::RuntimeWorkspaceMutationService;
use acowork_runtime::usecases::workspace_query_impl::RuntimeWorkspaceQueryService;
use acowork_runtime::usecases::workspace_query::WorkspaceQueryService;

const AGENT_ID: &str = "com.test.uploads";
const INSTANCE_ID: &str = "00000000-0000-4000-8000-0000000000aa";

/// One chunk, per the contract — a file of `CHUNK + 512 KiB` is a two-chunk
/// upload, which is the smallest case that proves the offset arithmetic.
const CHUNK: usize = acowork_core::upload::CHUNK_BYTES as usize;

struct Harness {
    port: u16,
    work_dir: std::path::PathBuf,
    _server: RuntimeHttpServer,
    _dir: tempfile::TempDir,
}

async fn boot() -> Harness {
    let dir = tempfile::tempdir().expect("temp dir");
    let work_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(&work_dir).unwrap();

    // `authorize_write` is what every session-scoped route calls first: a
    // session has to exist (and be writable by an unfiltered caller) before a
    // chunk is accepted, exactly as the multipart route demands.
    let conversations = work_dir.join("conversations");
    let meta: acowork_core::session_meta::SessionMeta = serde_json::from_value(serde_json::json!({
        "version": 1,
        "session_id": "s-1",
        "agent_id": AGENT_ID,
        "created_at": "2026-01-01T00:00:00Z",
        "last_active_at": "2026-01-01T00:00:00Z",
        "message_count": 0,
    }))
    .expect("session meta fixture parses");
    acowork_runtime::conversation::write_session_meta(&conversations, &meta)
        .expect("session meta is written");

    let snapshots: SharedSessionSnapshots = Arc::new(std::sync::RwLock::new(HashMap::new()));
    let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));

    let server = RuntimeHttpServer::start_with_bind_port(
        0,
        work_dir.clone(),
        work_dir.clone(),
        AGENT_ID.to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        Arc::new(tokio::sync::Mutex::new(None)),       // dispatch_tx
        Arc::new(std::sync::RwLock::new(Vec::new())),  // degraded_reasons
        Arc::new(tokio::sync::Mutex::new(None)),       // mqtt_client
        Arc::new(tokio::sync::Mutex::new(None)),       // session_metadata
        Arc::new(std::sync::RwLock::new(None)),        // conversation_index
        Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            RuntimeWorkspaceQueryService::new(work_dir.clone(), AGENT_ID.to_string()),
        ) as Arc<dyn WorkspaceQueryService>))),
        Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            RuntimeWorkspaceMutationService::new(work_dir.clone()),
        )))),
        Arc::new(tokio::sync::Mutex::new(None)),       // git_query
        Arc::new(tokio::sync::Mutex::new(None)),       // agent_tools
        Arc::new(tokio::sync::Mutex::new(None)),       // agent_config
        Arc::new(tokio::sync::Mutex::new(Some(Arc::new(
            acowork_runtime::usecases::RuntimeAttachmentService::new(work_dir.clone()),
        )))),
        Arc::new(tokio::sync::Mutex::new(None)),       // session_config
        Arc::new(std::sync::RwLock::new(None)),        // rag_provider
        Arc::new(tokio::sync::Mutex::new(None)),       // debug_service
        Arc::new(std::sync::RwLock::new(
            acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        )),
        Arc::new(tokio::sync::RwLock::new(None)),      // session_manager_slot
        Arc::new(std::sync::RwLock::new(None)),        // agent_core
    )
    .await
    .expect("runtime http server should start");

    Harness {
        port: server.port,
        work_dir,
        _server: server,
        _dir: dir,
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn url(port: u16, path: &str) -> String {
    format!("http://127.0.0.1:{port}{path}")
}

async fn create_upload(port: u16, base: &str, name: &str, bytes: &[u8]) -> (u16, serde_json::Value) {
    let resp = reqwest::Client::new()
        .post(url(port, base))
        .json(&serde_json::json!({
            "name": name,
            "size": bytes.len(),
            "sha256": sha256_hex(bytes),
        }))
        .send()
        .await
        .expect("create reaches the server");
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(serde_json::Value::Null))
}

/// Drive a complete chunked upload, returning the `complete` answer.
async fn upload_in_chunks(
    port: u16,
    base: &str,
    name: &str,
    bytes: &[u8],
    complete_query: &str,
) -> serde_json::Value {
    let (status, created) = create_upload(port, base, name, bytes).await;
    assert_eq!(status, 201, "create: {created}");
    let id = created["uploadId"].as_str().expect("create answers uploadId").to_string();

    let client = reqwest::Client::new();
    let mut offset = 0usize;
    while offset < bytes.len() {
        let end = (offset + CHUNK).min(bytes.len());
        let resp = client
            .put(url(port, &format!("{base}/{id}/{offset}")))
            .body(bytes[offset..end].to_vec())
            .send()
            .await
            .expect("chunk reaches the server");
        assert_eq!(resp.status().as_u16(), 200, "chunk at {offset}");
        // D4: the answer IS the resume cursor — no extra HEAD per chunk.
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["received"].as_u64(),
            Some(end as u64),
            "the upload offset the client keeps"
        );
        offset = end;
    }
    let resp = client
        .post(url(port, &format!("{base}/{id}/complete{complete_query}")))
        .send()
        .await
        .expect("complete reaches the server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    assert_eq!(status, 200, "complete: {body}");
    body
}

/// The central claim of the ADR: a file larger than one chunk lands, byte for
/// byte, through the session store — and the same bytes land on the *same*
/// content-addressed blob twice (the dedup the multipart route already had).
#[tokio::test]
async fn session_chunked_upload_lands_a_blob_and_dedups() {
    let harness = boot().await;
    let base = "/sessions/s-1/uploads";
    let mut bytes = vec![0u8; CHUNK + 512 * 1024];
    for (i, b) in bytes.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }

    let landed = upload_in_chunks(harness.port, base, "report.pdf", &bytes, "?format=pdf").await;
    let document_id = landed["documentId"]
        .as_str()
        .expect("complete answers documentId")
        .to_string();
    assert_eq!(landed["sizeBytes"].as_u64(), Some(bytes.len() as u64));

    // The bytes are readable back through the route the Desktop uses.
    let fetched = reqwest::get(url(
        harness.port,
        &format!("/files/{document_id}"),
    ))
    .await
    .expect("blob fetch reaches the server");
    assert_eq!(fetched.status().as_u16(), 200);
    assert_eq!(
        fetched.bytes().await.unwrap().as_ref(),
        bytes.as_slice(),
        "the landed blob is the uploaded file"
    );

    // Same content, second upload → same document id (dedup by digest), so a
    // resumed/retried send never doubles the store.
    let again = upload_in_chunks(harness.port, base, "report.pdf", &bytes, "?format=pdf").await;
    assert_eq!(
        again["documentId"].as_str(),
        Some(document_id.as_str()),
        "identical bytes must resolve to one blob"
    );
}

/// Resume semantics: `HEAD` reports the offset to continue from, and a client
/// that starts a *new* upload after a retry is unaffected. Abort is the third
/// verb, and it must leave nothing addressable behind.
#[tokio::test]
async fn session_upload_reports_its_resume_point_and_aborts_cleanly() {
    let harness = boot().await;
    let base = "/sessions/s-1/uploads";
    // Three chunks' worth, so a *gap* can be attempted without also running
    // past the declared size (the two refusals are different statuses).
    let bytes = vec![7u8; 2 * CHUNK + 100];
    let (status, created) = create_upload(harness.port, base, "big.bin", &bytes).await;
    assert_eq!(status, 201, "{created}");
    let id = created["uploadId"].as_str().unwrap().to_string();

    let client = reqwest::Client::new();
    let resp = client
        .put(url(harness.port, &format!("{base}/{id}/0")))
        .body(bytes[..CHUNK].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    let head = client
        .head(url(harness.port, &format!("{base}/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(head.status().as_u16(), 200);
    // A HEAD answer carries no body — the cursor rides the tus-shaped header,
    // which is the form a resuming client reads after a dropped connection.
    assert_eq!(
        head.headers()
            .get("upload-offset")
            .and_then(|v| v.to_str().ok()),
        Some(CHUNK.to_string().as_str()),
        "HEAD names the byte to resume from"
    );

    // Skipping ahead is refused rather than silently gap-filled — otherwise
    // `complete` would concatenate a file with a hole in it.
    let skip = client
        .put(url(harness.port, &format!("{base}/{id}/{}", 2 * CHUNK)))
        .body(vec![0u8; 100])
        .send()
        .await
        .unwrap();
    assert_eq!(skip.status().as_u16(), 409, "a skipped offset is a conflict");

    // Abort discards the reservation.
    let aborted = client
        .delete(url(harness.port, &format!("{base}/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(aborted.status().as_u16(), 204);
    let gone = client
        .head(url(harness.port, &format!("{base}/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status().as_u16(), 404, "an aborted upload is gone");
}

/// A declared digest that the bytes do not match fails `complete` with 422 and
/// keeps the chunks, so the client can re-PUT instead of starting over.
#[tokio::test]
async fn session_upload_rejects_a_digest_mismatch_and_keeps_the_chunks() {
    let harness = boot().await;
    let base = "/sessions/s-1/uploads";
    let bytes = vec![3u8; 1024];
    let client = reqwest::Client::new();
    let resp = client
        .post(url(harness.port, base))
        .json(&serde_json::json!({
            "name": "x.bin",
            "size": bytes.len(),
            "sha256": "0".repeat(64),
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let id = resp.json::<serde_json::Value>().await.unwrap()["uploadId"]
        .as_str()
        .unwrap()
        .to_string();
    client
        .put(url(harness.port, &format!("{base}/{id}/0")))
        .body(bytes.clone())
        .send()
        .await
        .unwrap();

    let complete = client
        .post(url(harness.port, &format!("{base}/{id}/complete")))
        .send()
        .await
        .unwrap();
    assert_eq!(complete.status().as_u16(), 422);
    let body: serde_json::Value = complete.json().await.unwrap();
    assert_eq!(body["retry"], serde_json::json!("re-put"));

    let head = client
        .head(url(harness.port, &format!("{base}/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(
        head.status().as_u16(),
        200,
        "chunks survive a failed verification"
    );
}

/// The workspace store: the merged bytes replace the target file atomically,
/// and the answer is disk metadata rather than a document id.
#[tokio::test]
async fn workspace_chunked_upload_replaces_the_target_file() {
    let harness = boot().await;
    std::fs::write(harness.work_dir.join("notes.md"), "old\n").unwrap();
    let mut bytes = vec![b'x'; CHUNK + 4096];
    bytes[..4].copy_from_slice(b"new\n");

    let landed = upload_in_chunks(
        harness.port,
        "/workspaces/uploads",
        "notes.md",
        &bytes,
        "?workspace_id=&path=notes.md",
    )
    .await;
    assert_eq!(
        landed["written"],
        serde_json::json!(true),
        "workspace complete answers the mutation envelope: {landed}"
    );
    assert_eq!(landed["path"].as_str(), Some("notes.md"));
    assert_eq!(landed["size"].as_u64(), Some(bytes.len() as u64));
    assert_eq!(
        std::fs::read(harness.work_dir.join("notes.md")).unwrap(),
        bytes,
        "the target file holds exactly the uploaded bytes"
    );
    // The staging file must not survive next to the target.
    let strays: Vec<_> = std::fs::read_dir(&harness.work_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("staging"))
        .collect();
    assert!(strays.is_empty(), "staging leftovers: {strays:?}");
}

/// A path that escapes the workspace is refused before anything is written —
/// the chunk route reuses the same traversal guard as `PUT /workspaces/file`.
#[tokio::test]
async fn workspace_chunked_upload_refuses_a_path_outside_the_workspace() {
    let harness = boot().await;
    let bytes = vec![1u8; 1024];
    let (status, created) =
        create_upload(harness.port, "/workspaces/uploads", "escape.txt", &bytes).await;
    assert_eq!(status, 201, "{created}");
    let id = created["uploadId"].as_str().unwrap().to_string();
    let client = reqwest::Client::new();
    client
        .put(url(harness.port, &format!("/workspaces/uploads/{id}/0")))
        .body(bytes.clone())
        .send()
        .await
        .unwrap();

    let complete = client
        .post(url(
            harness.port,
            &format!("/workspaces/uploads/{id}/complete?path=../escaped.txt"),
        ))
        .send()
        .await
        .unwrap();
    assert!(
        complete.status().is_client_error(),
        "traversal answered {}",
        complete.status()
    );
    assert!(
        !Path::new(&harness.work_dir).parent().unwrap().join("escaped.txt").exists(),
        "nothing was written outside the workspace"
    );
}

/// D9: the single-request multipart route is untouched, and still the right
/// path for anything under one chunk. This is the "old functionality did not
/// regress" half of the suite.
#[tokio::test]
async fn single_request_multipart_upload_still_lands() {
    let harness = boot().await;
    let bytes = b"a small text file\n".to_vec();
    let form = reqwest::multipart::Form::new()
        .part(
            "file",
            reqwest::multipart::Part::bytes(bytes.clone())
                .file_name("small.txt")
                .mime_str("text/plain")
                .unwrap(),
        )
        .text("format", "txt");

    let resp = reqwest::Client::new()
        .post(url(harness.port, "/sessions/s-1/files"))
        .multipart(form)
        .send()
        .await
        .expect("multipart reaches the server");
    let status = resp.status().as_u16();
    let body: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    assert_eq!(status, 200, "multipart: {body}");
    let document_id = body["documentId"].as_str().expect("documentId").to_string();

    let fetched = reqwest::get(url(harness.port, &format!("/files/{document_id}")))
        .await
        .unwrap();
    assert_eq!(fetched.status().as_u16(), 200);
    assert_eq!(fetched.bytes().await.unwrap().as_ref(), bytes.as_slice());
}

/// The chunk tree lives under `.uploads/` and is swept by TTL — it must never
/// surface as workspace content. (The runtime also excludes it from the file
/// tree; this asserts the location the sweep relies on.)
#[tokio::test]
async fn chunk_storage_stays_out_of_the_workspace_tree() {
    let harness = boot().await;
    let bytes = vec![9u8; 2048];
    let (status, _) = create_upload(harness.port, "/sessions/s-1/uploads", "t.bin", &bytes).await;
    assert_eq!(status, 201);
    let root = harness.work_dir.join(".uploads").join("sessions");
    assert!(
        root.is_dir(),
        "chunks live under <work_dir>/.uploads/sessions"
    );
}