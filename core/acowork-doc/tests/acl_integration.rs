//! REST-level ACL integration tests (design §9, manage gate).
//!
//! Drives the full router over HTTP with the trusted headers the Gateway
//! reverse proxy injects (`x-actor`, `x-auth-role`) and asserts the
//! user-visible contract:
//!
//! - 403 + `not_authorized` + `required` tier for view/edit denials
//! - permission *management* gated on creator ∨ admin ∨ local human
//! - directory ACL inheritance down to documents
//! - `/tree` visibility filtering with `creator`/`acl` on the response
//! - zero-migration: pre-ACL (public) nodes and header-less callers pass

use std::net::SocketAddr;
use std::sync::Arc;

use acowork_doc::config::DocConfig;
use acowork_doc::server::DocService;
use reqwest::{Method, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;

struct TestServer {
    base: String,
    client: reqwest::Client,
    _data: TempDir,
}

impl TestServer {
    async fn send_as(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        actor: Option<&str>,
        role: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = self.client.request(method, format!("{}{}", self.base, path));
        if let Some(a) = actor {
            req = req.header("x-actor", a);
        }
        if let Some(r) = role {
            req = req.header("x-auth-role", r);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        (status, value)
    }

    /// Act as a plain `multi_user` account.
    async fn as_user(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        actor: &str,
    ) -> (StatusCode, Value) {
        self.send_as(method, path, body, Some(actor), Some("user"))
            .await
    }

    async fn mkdir_as(&self, actor: &str, parent: &str, name: &str) -> String {
        let (status, body) = self
            .as_user(
                Method::POST,
                "/dirs",
                Some(json!({ "parent_dir_id": parent, "name": name })),
                actor,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "mkdir {name}: {body}");
        body["dir_id"].as_str().unwrap().to_string()
    }

    async fn mkdoc_as(&self, actor: &str, dir_id: &str, title: &str) -> String {
        let (status, body) = self
            .as_user(
                Method::POST,
                "/docs",
                Some(json!({
                    "parent_dir_id": dir_id,
                    "title": title,
                    "content": "# v1",
                })),
                actor,
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "mkdoc {title}: {body}");
        body["doc_id"].as_str().unwrap().to_string()
    }

    fn error(body: &Value) -> (String, String) {
        (
            body["error"]["code"].as_str().unwrap_or("").to_string(),
            body["error"]["required"].as_str().unwrap_or("").to_string(),
        )
    }
}

async fn spawn() -> TestServer {
    let data = TempDir::new().unwrap();
    let config = DocConfig {
        data_dir: data.path().to_path_buf(),
        ..DocConfig::default()
    };
    let svc = Arc::new(DocService::new(config).await.unwrap());
    let addr: SocketAddr = svc
        .clone()
        .serve(SocketAddr::from(([127, 0, 0, 1], 0)))
        .await
        .unwrap();
    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _data: data,
    }
}

// ── view/edit enforcement + manage gate over HTTP ─────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn acl_tiers_and_manage_gate_end_to_end() {
    let srv = spawn().await;

    let dir = srv.mkdir_as("u-alice", "root", "proj").await;
    let doc = srv.mkdoc_as("u-alice", &dir, "PRD").await;

    // Zero migration: before any ACL is set, a stranger reads and writes.
    let (status, _) = srv
        .as_user(Method::GET, &format!("/docs/{doc}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::OK, "public doc must be readable");

    // Creator grants bob view on the dir (inherited by the doc).
    let (status, _) = srv
        .as_user(
            Method::PUT,
            &format!("/permissions/dir/{dir}"),
            Some(json!({ "acl": [{ "principal": "u-bob", "role": "view" }] })),
            "u-alice",
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "creator may set ACL");

    // Bob now sees the doc…
    let (status, _) = srv
        .as_user(Method::GET, &format!("/docs/{doc}"), None, "u-bob")
        .await;
    assert_eq!(status, StatusCode::OK, "view grant (inherited) must allow read");
    // …but cannot edit it — 403 + required "edit".
    let (status, body) = srv
        .as_user(
            Method::PUT,
            &format!("/docs/{doc}"),
            Some(json!({ "base_version": 1, "content": "# v2" })),
            "u-bob",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(TestServer::error(&body), ("not_authorized".into(), "edit".into()));

    // A stranger is locked out of reads — 403 + required "view".
    let (status, body) = srv
        .as_user(Method::GET, &format!("/docs/{doc}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(TestServer::error(&body), ("not_authorized".into(), "view".into()));

    // The dir creator never locks itself out.
    let (status, _) = srv
        .as_user(
            Method::PUT,
            &format!("/docs/{doc}"),
            Some(json!({ "base_version": 1, "content": "# v2" })),
            "u-alice",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "doc creator bypasses the dir ACL");

    // Manage gate: a view grant is not a sharing right — bob cannot touch
    // the ACL, and cannot even read it back on a node he can see? (read is
    // allowed, manage is not)
    let (status, _) = srv
        .as_user(
            Method::GET,
            &format!("/permissions/dir/{dir}"),
            None,
            "u-bob",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "view holder may inspect the ACL");
    let (status, body) = srv
        .as_user(
            Method::PUT,
            &format!("/permissions/dir/{dir}"),
            Some(json!({ "acl": [] })),
            "u-bob",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        TestServer::error(&body),
        ("not_authorized".into(), "manage".into())
    );

    // Admin may manage anything, even without a view grant.
    let (status, _) = srv.send_as(
        Method::PUT,
        &format!("/permissions/dir/{dir}"),
        Some(json!({ "acl": [] })),
        Some("u-adm"),
        Some("admin"),
    ).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "admin bypasses manage gate");

    // Private now: bob's inherited view is gone (empty list grants nobody).
    let (status, _) = srv
        .as_user(Method::GET, &format!("/docs/{doc}"), None, "u-bob")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "acl [] must hide the subtree");

    // Clearing (acl: null) restores inheritance → public again (admin).
    let (status, _) = srv
        .send_as(
            Method::PUT,
            &format!("/permissions/dir/{dir}"),
            Some(json!({ "acl": null })),
            Some("u-adm"),
            Some("admin"),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = srv
        .as_user(Method::GET, &format!("/docs/{doc}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::OK, "cleared ACL restores public access");
}

// ── permissions GET view shape ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn permissions_get_reports_view_and_manage_rights() {
    let srv = spawn().await;
    let dir = srv.mkdir_as("u-alice", "root", "team").await;
    srv.as_user(
        Method::PUT,
        &format!("/permissions/dir/{dir}"),
        Some(json!({ "acl": [{ "principal": "u-bob", "role": "edit" }] })),
        "u-alice",
    )
    .await;

    // Creator: can_manage true, explicit + creator echoed.
    let (status, body) = srv
        .as_user(Method::GET, &format!("/permissions/dir/{dir}"), None, "u-alice")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_manage"], json!(true));
    assert_eq!(body["creator"], json!("u-alice"));
    assert_eq!(body["explicit"].as_array().unwrap().len(), 1);

    // Edit holder: sees the ACL but cannot manage.
    let (status, body) = srv
        .as_user(Method::GET, &format!("/permissions/dir/{dir}"), None, "u-bob")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["can_manage"], json!(false));

    // No view grant → GET itself is 403 (ACLs must not be probeable).
    let (status, _) = srv
        .as_user(Method::GET, &format!("/permissions/dir/{dir}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── tree filtering + acl on the response ───────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn tree_filters_invisible_children_and_carries_acl() {
    let srv = spawn().await;
    let _pub_dir = srv.mkdir_as("u-alice", "root", "pub").await;
    let secret_dir = srv.mkdir_as("u-alice", "root", "secret").await;
    let secret_doc = srv.mkdoc_as("u-alice", &secret_dir, "hidden").await;
    srv.as_user(
        Method::PUT,
        &format!("/permissions/dir/{secret_dir}"),
        Some(json!({ "acl": [{ "principal": "u-insider", "role": "view" }] })),
        "u-alice",
    )
    .await;

    // Stranger's tree: secret dir is gone entirely.
    let (status, body) = srv
        .as_user(Method::GET, "/tree?dir_id=root", None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["dirs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["pub"], "invisible dirs must be filtered, got: {body}");

    // The visible entry still carries the ACL display fields.
    let (status, body) = srv
        .as_user(Method::GET, "/tree?dir_id=root", None, "u-insider")
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dirs"].as_array().unwrap().len(), 2);
    let secret = body["dirs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "secret")
        .unwrap();
    assert_eq!(secret["creator"], json!("u-alice"));
    assert_eq!(secret["acl"][0]["principal"], json!("u-insider"));

    // Insider reaches inside the secret dir; the stranger gets 403.
    let (status, _) = srv
        .as_user(
            Method::GET,
            &format!("/docs/{secret_doc}"),
            None,
            "u-insider",
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = srv
        .as_user(
            Method::GET,
            &format!("/tree?dir_id={secret_dir}"),
            None,
            "u-cara",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// ── legacy compatibility ────────────────────────────────────

/// No trusted headers at all (direct dev access) → legacy allow-all, and
/// nodes created without an identity have no creator to gate on.
#[tokio::test(flavor = "multi_thread")]
async fn header_less_callers_keep_legacy_access() {
    let srv = spawn().await;
    let (status, body) = srv
        .send_as(
            Method::POST,
            "/dirs",
            Some(json!({ "parent_dir_id": "root", "name": "dev" })),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let dir = body["dir_id"].as_str().unwrap().to_string();
    // Anyone (even a named user) still passes while nothing sets an ACL.
    let (status, _) = srv
        .as_user(Method::GET, &format!("/dirs/{dir}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::OK);
    // Creatorless node: only admins/local may manage it.
    let (status, _) = srv
        .as_user(
            Method::PUT,
            &format!("/permissions/dir/{dir}"),
            Some(json!({ "acl": [] })),
            "u-cara",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "creatorless node is admin-only");
    // Local human always passes the manage gate.
    let (status, _) = srv
        .send_as(
            Method::PUT,
            &format!("/permissions/dir/{dir}"),
            Some(json!({ "acl": [] })),
            Some("human"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

/// The `local` `human` constant bypasses every gate even inside a locked dir.
#[tokio::test(flavor = "multi_thread")]
async fn local_human_bypasses_private_dir() {
    let srv = spawn().await;
    let dir = srv.mkdir_as("u-alice", "root", "mine").await;
    let doc = srv.mkdoc_as("u-alice", &dir, "diary").await;
    srv.as_user(
        Method::PUT,
        &format!("/permissions/dir/{dir}"),
        Some(json!({ "acl": [] })),
        "u-alice",
    )
    .await;
    // Stranger blocked…
    let (status, _) = srv
        .as_user(Method::GET, &format!("/docs/{doc}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // …local human is not.
    let (status, _) = srv
        .send_as(Method::GET, &format!("/docs/{doc}"), None, Some("human"), None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

// ── trash is a global pool — gate view/edit + preserve ACL on restore ──

#[tokio::test(flavor = "multi_thread")]
async fn trash_gates_access_and_preserves_acl_on_restore() {
    let srv = spawn().await;
    // Public dir, but the doc itself is made private (creator + admin only).
    let dir = srv.mkdir_as("u-alice", "root", "proj").await;
    let doc = srv.mkdoc_as("u-alice", &dir, "diary").await;
    let (status, _) = srv
        .as_user(
            Method::PUT,
            &format!("/permissions/doc/{doc}"),
            Some(json!({ "acl": [] })),
            "u-alice",
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "doc creator may privatize");

    // Alice deletes it into the global .trash pool.
    let (status, _) = srv
        .as_user(Method::DELETE, &format!("/docs/{doc}"), None, "u-alice")
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // A stranger must not even see the private slot (the pre-fix B1 leak).
    let (status, body) = srv
        .as_user(Method::GET, "/trash", None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.as_array().map(|a| a.len()).unwrap_or(usize::MAX),
        0,
        "private trashed slot must be hidden from non-owners: {body}"
    );

    // The owner sees it and learns the slot id.
    let (status, body) = srv
        .as_user(Method::GET, "/trash", None, "u-alice")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let trash_id = body[0]["trash_id"].as_str().unwrap().to_string();

    // A stranger cannot restore or purge someone else's private slot.
    let (status, body) = srv
        .as_user(
            Method::POST,
            &format!("/trash/{trash_id}/restore"),
            None,
            "u-cara",
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        TestServer::error(&body),
        ("not_authorized".into(), "edit".into())
    );
    let (status, _) = srv
        .as_user(Method::DELETE, &format!("/trash/{trash_id}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "purge must be gated too");

    // The owner restores — the restored doc keeps creator + private ACL.
    let (status, body) = srv
        .as_user(
            Method::POST,
            &format!("/trash/{trash_id}/restore"),
            None,
            "u-alice",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "restore as owner: {body}");
    let new_doc = body["doc_id"].as_str().unwrap().to_string();

    // Still private after the round-trip (pre-fix B2 made it public).
    let (status, _) = srv
        .as_user(Method::GET, &format!("/docs/{new_doc}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "restored doc must stay private");
    // Creator manage gate survived the restore.
    let (status, _) = srv
        .as_user(
            Method::PUT,
            &format!("/permissions/doc/{new_doc}"),
            Some(json!({ "acl": [{ "principal": "u-bob", "role": "view" }] })),
            "u-alice",
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "creator retained after restore");
}

// ── update-request queue gates read + review on the target doc ──

#[tokio::test(flavor = "multi_thread")]
async fn request_queue_gates_read_and_review_on_doc_acl() {
    let srv = spawn().await;
    let dir = srv.mkdir_as("u-alice", "root", "proj").await;
    let doc = srv.mkdoc_as("u-alice", &dir, "spec").await;
    // Private doc: only the creator (and admins) can view/edit it.
    srv.as_user(
        Method::PUT,
        &format!("/permissions/doc/{doc}"),
        Some(json!({ "acl": [] })),
        "u-alice",
    )
    .await;

    // An agent proposal is inert intake — submit is allowed without a
    // trusted human actor (the real gate is on approve/reject/read).
    let (status, body) = srv
        .send_as(
            Method::POST,
            "/requests",
            Some(json!({
                "doc_id": doc,
                "base_version": 1,
                "content": "# v2 proposed",
                "submitted_by": "agent:com.test",
            })),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "submit proposal: {body}");
    let req_id = body["request_id"].as_str().unwrap().to_string();

    // A stranger must not see the proposal in the global queue…
    let (status, body) = srv
        .as_user(Method::GET, "/requests", None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body.as_array().map(|a| a.len()).unwrap_or(usize::MAX),
        0,
        "proposals for invisible docs must be filtered: {body}"
    );
    // …nor read its detail (which quotes the proposed content).
    let (status, body) = srv
        .as_user(Method::GET, &format!("/requests/{req_id}"), None, "u-cara")
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        TestServer::error(&body),
        ("not_authorized".into(), "view".into())
    );

    // Rejecting is a review decision — a stranger without edit is refused…
    let (status, body) = srv
        .send_as(
            Method::POST,
            &format!("/requests/{req_id}/reject"),
            Some(json!({ "reviewed_by": "u-cara", "note": null })),
            Some("u-cara"),
            Some("user"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(
        TestServer::error(&body),
        ("not_authorized".into(), "edit".into())
    );

    // …while the doc creator (has edit) may reject it.
    let (status, _) = srv
        .send_as(
            Method::POST,
            &format!("/requests/{req_id}/reject"),
            Some(json!({ "reviewed_by": "u-alice", "note": null })),
            Some("u-alice"),
            Some("user"),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "doc editor/creator may reject");
}
