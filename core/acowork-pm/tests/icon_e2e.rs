//! Project-icon endpoint e2e (`/projects/{pid}/icon*`).
//!
//! Covers the custom-upload path added for the PM settings panel:
//! - upload writes `projects/{pid}/icon/icon-NN.{ext}` and returns the
//!   relative path (name auto-increments across uploads)
//! - `icon-assets` lists what was uploaded for that project
//! - `icon-file?path=…` serves the bytes with an image content-type
//! - the namespace guard rejects a path outside this project and any
//!   traversal in the filename; an unknown project 404s
//!
//! Harness mirrors `handlers_e2e.rs` (tempdir + `pm_router`, driven
//! in-process through `tower::ServiceExt::oneshot`).

use std::sync::Arc;

use acowork_pm::{PmConfig, TreePmStore};
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use tower::ServiceExt;

struct TestApp {
    router: axum::Router,
    _tmp: tempfile::TempDir,
}

impl TestApp {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = PmConfig {
            data_dir: tmp.path().to_path_buf(),
            index_rebuild_on_start: false,
            ..Default::default()
        };
        let store = Arc::new(TreePmStore::new(cfg.clone()).await.unwrap());
        let router = acowork_pm::pm_router(store, cfg, Arc::new(acowork_pm::NoopAgentDirectory), None);
        Self { router, _tmp: tmp }
    }

    /// Create a project, return its id (from `POST /projects`).
    async fn create_project(&self, title: &str) -> String {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/projects")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-actor", "human-alice")
            .body(Body::from(
                serde_json::json!({ "title": title, "description": "" }).to_string(),
            ))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        json["id"].as_str().unwrap().to_string()
    }

    /// POST a multipart icon upload; returns (status, body-json).
    async fn upload_icon(&self, pid: &str, filename: &str, bytes: &[u8]) -> (StatusCode, serde_json::Value) {
        let boundary = "----acowork-icon-test-boundary";
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n").as_bytes(),
        );
        body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
        body.extend_from_slice(bytes);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

        let req = Request::builder()
            .method(Method::POST)
            .uri(format!("/projects/{pid}/icon"))
            .header(header::CONTENT_TYPE, format!("multipart/form-data; boundary={boundary}"))
            .body(Body::from(body))
            .unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    async fn get(&self, uri: &str) -> axum::response::Response {
        let req = Request::builder().method(Method::GET).uri(uri).body(Body::empty()).unwrap();
        self.router.clone().oneshot(req).await.unwrap()
    }
}

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\x00\x00\x00\x01";

#[tokio::test]
async fn upload_list_and_serve_roundtrip() {
    let app = TestApp::new().await;
    let pid = app.create_project("Icons").await;

    // First upload → icon-01.png, relative path under the project namespace.
    let (status, body) = app.upload_icon(&pid, "pic.png", PNG_BYTES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rel1 = body["relative_path"].as_str().unwrap().to_string();
    assert_eq!(rel1, format!("projects/{pid}/icon/icon-01.png"));

    // Second upload auto-increments.
    let (status, body) = app.upload_icon(&pid, "another.png", PNG_BYTES).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let rel2 = body["relative_path"].as_str().unwrap().to_string();
    assert_eq!(rel2, format!("projects/{pid}/icon/icon-02.png"));

    // Assets list returns both, sorted.
    let resp = app.get(&format!("/projects/{pid}/icon-assets")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let list: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let assets = list["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 2, "{list}");
    assert_eq!(assets[0]["relative_path"].as_str().unwrap(), rel1);
    assert_eq!(assets[1]["relative_path"].as_str().unwrap(), rel2);

    // Serve returns the image bytes with a png content-type.
    let resp = app
        .get(&format!("/projects/{pid}/icon-file?path={}", urlencoding(&rel1)))
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png",
        "served icon must carry its image content-type"
    );
    let served = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(&served[..], PNG_BYTES, "byte-for-byte roundtrip");
}

#[tokio::test]
async fn rejects_non_image_and_outside_namespace_and_unknown_project() {
    let app = TestApp::new().await;
    let pid = app.create_project("Guards").await;

    // Non-image extension → 400 (mime rejected).
    let (status, _) = app.upload_icon(&pid, "evil.exe", b"MZ\x90\x00").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "exe upload must be rejected");

    // Upload one real icon so a serve target exists.
    app.upload_icon(&pid, "ok.png", PNG_BYTES).await;

    // Path outside this project's namespace → 400.
    let other = format!("projects/{pid}/tasks/tx/icon-01.png");
    let resp = app
        .get(&format!("/projects/{pid}/icon-file?path={}", urlencoding(&other)))
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "cross-namespace path must be rejected");

    // Traversal smuggled into the filename segment → rejected (400), never a 500.
    let trick = format!("projects/{pid}/icon/../../secret.txt");
    let resp = app
        .get(&format!("/projects/{pid}/icon-file?path={}", urlencoding(&trick)))
        .await;
    assert!(
        resp.status() == StatusCode::BAD_REQUEST,
        "traversal must be rejected, got {}",
        resp.status()
    );

    // A request inside an UNKNOWN project's own namespace → its icon dir
    // never exists, so the byte lookup misses → 404 (never a 500 / leak).
    let ghost = "projects/p-doesnotexist/icon/icon-01.png".to_string();
    let resp = app
        .get(&format!("/projects/p-doesnotexist/icon-file?path={}", urlencoding(&ghost)))
        .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unknown project icon must 404");
}

/// Minimal percent-encoding for the query value (only `/` needs escaping here).
fn urlencoding(s: &str) -> String {
    s.replace('/', "%2F")
}
