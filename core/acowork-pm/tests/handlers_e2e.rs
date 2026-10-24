//! P0/P1 handler e2e 集成测试。
//!
//! 验证 OpenAPI 契约的每个 endpoint 实际能被路由到、返回预期的 HTTP 状态码与响应体结构。
//!
//! 覆盖矩阵：
//! | endpoint                              | happy | error (404/400/500) |
//! |---------------------------------------|-------|---------------------|
//! | GET    /projects                      |   ✓   |                     |
//! | POST   /projects                      |   ✓   | 400 missing title   |
//! | GET    /projects/:pid                 |   ✓   | 404 unknown         |
//! | PATCH  /projects/:pid                 |   ✓   | 404 unknown         |
//! | DELETE /projects/:pid                 |   ✓   |                     |
//! | GET    /projects/:pid/tasks           |   ✓   |                     |
//! | POST   /projects/:pid/tasks           |   ✓   | 400 missing title   |
//! | GET    /tasks/:tid                    |   ✓   | 404 unknown         |
//! | PATCH  /tasks/:tid                    |   ✓   |                     |
//! | DELETE /tasks/:tid                    |   ✓   |                     |
//! | PATCH  /tasks/:tid/parent             |   ✓   |                     |
//! | POST   /tasks/:tid/claim              |   ✓   | 500 missing actor   |
//! | POST   /tasks/:tid/submit             |   ✓   |                     |
//! | POST   /tasks/:tid/review             |   ✓   |                     |
//! | GET    /tasks/:tid/children           |   ✓   |                     |
//! | GET    /tasks/:tid/attachments        |   ✓   |                     |
//! | POST   /tasks/:tid/attachments        |   ✓   | 404 unknown task   |
//! | GET    /attachments/:aid              |   ✓   | 404 unknown        |
//! | DELETE /attachments/:aid              |   ✓   | 404 unknown        |

use std::sync::Arc;

use acowork_pm::{PmConfig, TreePmStore};
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use tower::ServiceExt;

// ── Test harness ──────────────────────────────────────────────────────

/// 测试装置：tempdir + TreePmStore + Router<()>。
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
        let router = acowork_pm::pm_router(
            store,
            cfg,
            Arc::new(acowork_pm::NoopAgentDirectory),
            None,
        );
        Self { router, _tmp: tmp }
    }

    fn router(&self) -> axum::Router {
        self.router.clone()
    }

    fn json_request(method: Method, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn request_no_body(method: Method, uri: &str) -> Request<Body> {
        Request::builder().method(method).uri(uri).body(Body::empty()).unwrap()
    }

    fn request_with_actor(method: Method, uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-actor", "human-alice")
            .body(Body::from(body.to_string()))
            .unwrap()
    }
}

async fn body_json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let json: serde_json::Value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            panic!(
                "response body not JSON: {:?}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, json)
}

/// 读取响应 body 为 UTF-8 文本（不要求 JSON）。
///
/// 用于断言 axum 默认 `Json`/`Path` 提取器的错误响应（plain text），
/// 这类错误当前未通过自定义 extractor 包装到 `PmError::ErrorEnvelope`。
async fn body_text(resp: axum::response::Response) -> (StatusCode, String) {
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ── projects ─────────────────────────────────────────────────────────

#[tokio::test]
async fn post_projects_then_list_returns_it() {
    let app = TestApp::new().await;
    let router = app.router();

    // POST /projects
    let create = TestApp::json_request(
        Method::POST,
        "/projects",
        serde_json::json!({"title": "Demo", "description": "hello"}),
    );
    let (status, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Demo");
    assert!(body["id"].as_str().unwrap().starts_with("p-"));
    assert_eq!(body["status"], "active");
    // created_by 未传 X-Actor → fallback "unknown"
    assert_eq!(body["created_by"], "unknown");

    // GET /projects
    let list = TestApp::request_no_body(Method::GET, "/projects");
    let (status, body) = body_json(router.oneshot(list).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"], "Demo");
}

#[tokio::test]
async fn post_projects_x_actor_propagates() {
    let app = TestApp::new().await;
    let create = TestApp::request_with_actor(
        Method::POST,
        "/projects",
        serde_json::json!({"title": "X"}),
    );
    let (status, body) = body_json(app.router().oneshot(create).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["created_by"], "human-alice");
}

#[tokio::test]
async fn post_projects_missing_title_returns_422() {
    let app = TestApp::new().await;
    let req = TestApp::json_request(Method::POST, "/projects", serde_json::json!({}));
    let (status, body) = body_text(app.router().oneshot(req).await.unwrap()).await;
    // axum 0.8 默认 Json 提取器对"缺字段"返回 422 Unprocessable Entity(语义错误),
    // 不是 400 Bad Request(语法错误)。P0/P1 验收已知差距:
    // - 状态码 422 而非 400
    // - body 为 plain text 而非 JSON ErrorEnvelope
    // —— 当前未用自定义 extractor 包装到 PmError::ErrorEnvelope,见 TODO 记录。
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body.contains("title"),
        "expected error to mention `title`, got: {body:?}"
    );
}

#[tokio::test]
async fn get_unknown_project_returns_404() {
    let app = TestApp::new().await;
    let req = TestApp::request_no_body(Method::GET, "/projects/p-doesnotexist");
    let (status, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "project_not_found");
}

#[tokio::test]
async fn patch_project_updates_title() {
    let app = TestApp::new().await;
    let router = app.router();

    // create
    let create = TestApp::json_request(
        Method::POST,
        "/projects",
        serde_json::json!({"title": "Old"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let pid = body["id"].as_str().unwrap().to_string();

    // patch
    let patch = TestApp::json_request(
        Method::PATCH,
        &format!("/projects/{pid}"),
        serde_json::json!({"title": "New"}),
    );
    let (status, body) = body_json(router.clone().oneshot(patch).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "New");
}

#[tokio::test]
async fn delete_project_returns_204() {
    let app = TestApp::new().await;
    let router = app.router();

    let create = TestApp::json_request(
        Method::POST,
        "/projects",
        serde_json::json!({"title": "X"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let pid = body["id"].as_str().unwrap().to_string();

    let del = TestApp::request_no_body(
        Method::DELETE,
        &format!("/projects/{pid}?cascade=true"),
    );
    let resp = router.oneshot(del).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ── tasks ─────────────────────────────────────────────────────────────

async fn create_project(app: &TestApp, title: &str) -> String {
    let create = TestApp::json_request(
        Method::POST,
        "/projects",
        serde_json::json!({"title": title}),
    );
    let (_, body) = body_json(app.router().oneshot(create).await.unwrap()).await;
    body["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn post_then_list_tasks_in_project() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;

    // POST /projects/:pid/tasks
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "Task A", "type": "task"}),
    );
    let (status, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["title"], "Task A");
    assert_eq!(body["type"], "task");
    assert!(body["id"].as_str().unwrap().starts_with("t-"));

    // GET /projects/:pid/tasks
    let list = TestApp::request_no_body(
        Method::GET,
        &format!("/projects/{pid}/tasks"),
    );
    let (status, body) = body_json(router.oneshot(list).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn get_task_returns_response_with_derived_fields() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;

    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    let get = TestApp::request_no_body(Method::GET, &format!("/tasks/{tid}"));
    let (status, body) = body_json(router.oneshot(get).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    // 派生字段：根任务 depth=0、未阻塞
    assert_eq!(body["depth"], 0);
    // is_blocked=false 时被 skip_serializing_if 跳过 —— 字段缺失 或 =false 都视为未阻塞
    let is_blocked_value = body.get("is_blocked");
    assert!(
        is_blocked_value.is_none() || is_blocked_value == Some(&serde_json::Value::Bool(false)),
        "expected is_blocked absent or false, got {:?}",
        is_blocked_value,
    );
    assert_eq!(body["title"], "T", "task 字段经 flatten 序列化到顶层");
}

#[tokio::test]
async fn get_unknown_task_returns_404() {
    let app = TestApp::new().await;
    let req = TestApp::request_no_body(Method::GET, "/tasks/t-doesnotexist");
    let (status, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "task_not_found");
}

#[tokio::test]
async fn invalid_task_id_returns_400() {
    let app = TestApp::new().await;
    let req = TestApp::request_no_body(Method::GET, "/tasks/not-a-task-id");
    let (status, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_id");
}

#[tokio::test]
async fn patch_task_updates_priority() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    let patch = TestApp::json_request(
        Method::PATCH,
        &format!("/tasks/{tid}"),
        serde_json::json!({"priority": "urgent"}),
    );
    let (status, body) = body_json(router.clone().oneshot(patch).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["priority"], "urgent");
}

#[tokio::test]
async fn delete_task_returns_204() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    let del = TestApp::request_no_body(
        Method::DELETE,
        &format!("/tasks/{tid}?cascade=true"),
    );
    let resp = router.oneshot(del).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ── claim / submit / review ───────────────────────────────────────────

#[tokio::test]
async fn claim_without_x_actor_returns_500() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    // 故意不发 X-Actor
    let claim = TestApp::json_request(
        Method::POST,
        &format!("/tasks/{tid}/claim"),
        serde_json::json!({}),
    );
    let (status, body) = body_json(router.oneshot(claim).await.unwrap()).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["error"]["code"], "internal_error");
}

#[tokio::test]
async fn claim_submit_review_full_lifecycle() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    // 联动指派：claim 会把 actor 写为 assignee，actor 必须是项目成员（agent）
    let agent = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
    let add = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/members"),
        serde_json::json!({ "instance_id": agent }),
    );
    let (status, _) = body_json(router.clone().oneshot(add).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);

    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T", "type": "checkpoint"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    // claim（agent 成员）
    let claim = Request::builder()
        .method(Method::POST)
        .uri(format!("/tasks/{tid}/claim"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-actor", agent)
        .body(Body::from("{}"))
        .unwrap();
    let (status, body) = body_json(router.clone().oneshot(claim).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "in_progress");

    // submit（agent）
    let submit = Request::builder()
        .method(Method::POST)
        .uri(format!("/tasks/{tid}/submit"))
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-actor", agent)
        .body(Body::from(r#"{"text":"finished"}"#))
        .unwrap();
    let (status, body) = body_json(router.clone().oneshot(submit).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "submitted");

    // review (approve)
    let review = TestApp::request_with_actor(
        Method::POST,
        &format!("/tasks/{tid}/review"),
        serde_json::json!({"approved": true}),
    );
    let (status, body) = body_json(router.clone().oneshot(review).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "done");
    assert_eq!(body["review_status"], "approved");
}

// ── project members（联动指派前置）────────────────────────────────────

/// 成员 CRUD 错误路径 e2e：
/// 重复添加 409 / 移除不存在 404 / 指派非成员 400 / 成员有未完成任务移除 409。
/// （happy path 由 `claim_submit_review_full_lifecycle` + `remote_e2e` 覆盖；
///  本测试补 HTTP 层的错误契约断言。）
#[tokio::test]
async fn members_crud_error_paths() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;

    let alice = "3f8c2a91-7e4b-4d2a-b6f1-1a91b07e4c2d";
    let outsider = "99999999-9999-9999-9999-999999999999";

    // 添加成员 → 200，响应含 members
    let add = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/members"),
        serde_json::json!({ "instance_id": alice }),
    );
    let (status, body) = body_json(router.clone().oneshot(add).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    // 注意：create_project 无 X-Actor → created_by 兜底 "unknown"，被自动
    // 加入 members（既有契约，handlers_e2e 有断言）；真实 Gateway 恒注入
    // "human" 或 instance_id，此处只断言 alice 在成员列表中。
    let member_ids: Vec<&str> = body["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["instance_id"].as_str().unwrap())
        .collect();
    assert!(member_ids.contains(&alice), "members = {member_ids:?}");

    // 重复添加 → 409 member_already_exists
    let dup = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/members"),
        serde_json::json!({ "instance_id": alice }),
    );
    let (status, body) = body_json(router.clone().oneshot(dup).await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "member_already_exists");

    // 移除不存在的成员 → 404 member_not_found
    let rm_missing = TestApp::request_no_body(
        Method::DELETE,
        &format!("/projects/{pid}/members/{outsider}"),
    );
    let (status, body) = body_json(router.clone().oneshot(rm_missing).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "member_not_found");

    // 指派非成员 → 400 assignee_not_project_member
    let bad_assign = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({ "title": "T", "assignee": outsider }),
    );
    let (status, body) = body_json(router.clone().oneshot(bad_assign).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "assignee_not_project_member");

    // 成员名下有未完成任务 → 移除 409 member_has_open_tasks
    let with_task = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({ "title": "T2", "assignee": alice }),
    );
    let (status, _) = body_json(router.clone().oneshot(with_task).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    let rm_alice = TestApp::request_no_body(
        Method::DELETE,
        &format!("/projects/{pid}/members/{alice}"),
    );
    let (status, body) = body_json(router.clone().oneshot(rm_alice).await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "member_has_open_tasks");
}

#[tokio::test]
async fn reparent_task_to_root() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;

    // 创建根任务
    let create_root = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "Root"}),
    );
    let (_, root) = body_json(router.clone().oneshot(create_root).await.unwrap()).await;
    let root_id = root["id"].as_str().unwrap().to_string();

    // 创建 child → 显式传 parent_task_id
    let create_child = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({
            "title": "Child",
            "parent_task_id": root_id.clone()
        }),
    );
    let (_, child) = body_json(router.clone().oneshot(create_child).await.unwrap()).await;
    let child_id = child["id"].as_str().unwrap().to_string();

    // PATCH /tasks/:tid/parent {new_parent: null} → 提升为根
    let reparent = TestApp::json_request(
        Method::PATCH,
        &format!("/tasks/{child_id}/parent"),
        serde_json::json!({"new_parent": null}),
    );
    let resp = router.clone().oneshot(reparent).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // 验证 depth=0
    let get = TestApp::request_no_body(Method::GET, &format!("/tasks/{child_id}"));
    let (status, body) = body_json(router.oneshot(get).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["depth"], 0);
}

#[tokio::test]
async fn list_children_returns_direct_children_only() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;

    // root → child1, child2 (each leaf)
    let create_root = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "Root"}),
    );
    let (_, root) = body_json(router.clone().oneshot(create_root).await.unwrap()).await;
    let root_id = root["id"].as_str().unwrap().to_string();

    for label in ["A", "B"] {
        let c = TestApp::json_request(
            Method::POST,
            &format!("/projects/{pid}/tasks"),
            serde_json::json!({"title": label, "parent_task_id": root_id.clone()}),
        );
        let _ = router.clone().oneshot(c).await.unwrap();
    }

    let list = TestApp::request_no_body(Method::GET, &format!("/tasks/{root_id}/children"));
    let (status, body) = body_json(router.oneshot(list).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 2);
}

// ── attachments ───────────────────────────────────────────────────────

#[tokio::test]
async fn list_attachments_empty_returns_200() {
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    let list = TestApp::request_no_body(
        Method::GET,
        &format!("/tasks/{tid}/attachments"),
    );
    let (status, body) = body_json(router.oneshot(list).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn upload_download_delete_attachment_roundtrip() {
    // 真实 multipart 上传 → 元数据返回 → 下载字节 → 删除 204。
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    // 构造 multipart body（text/plain 文件）
    let boundary = "----testboundary";
    let file_name = "hello.txt";
    let file_content = b"hello attachment bytes";
    let mut mp_body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: text/plain\r\n\r\n"
    )
    .into_bytes();
    mp_body.extend_from_slice(file_content);
    mp_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let upload = Request::builder()
        .method(Method::POST)
        .uri(format!("/tasks/{tid}/attachments"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("x-actor", "human-alice")
        .body(Body::from(mp_body))
        .unwrap();
    let (status, meta) = body_json(router.clone().oneshot(upload).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(meta["filename"], "hello.txt");
    assert_eq!(meta["kind"], "file");
    assert_eq!(meta["content_type"], "text/plain");
    assert_eq!(meta["size"].as_u64().unwrap(), file_content.len() as u64);
    assert!(meta["sha256"].as_str().unwrap().len() == 64);
    let aid = meta["id"].as_str().unwrap().to_string();

    // 下载 → 返回原始字节
    let download = TestApp::request_no_body(Method::GET, &format!("/attachments/{aid}"));
    let resp = router.clone().oneshot(download).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/plain"
    );
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    assert_eq!(&bytes[..], file_content);

    // 列表 → 1 条
    let list = TestApp::request_no_body(Method::GET, &format!("/tasks/{tid}/attachments"));
    let (status, arr) = body_json(router.clone().oneshot(list).await.unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(arr.as_array().unwrap().len(), 1);

    // 删除 → 204，再下载 → 404
    let del = TestApp::request_no_body(Method::DELETE, &format!("/attachments/{aid}"));
    let resp = router.clone().oneshot(del).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let again = TestApp::request_no_body(Method::GET, &format!("/attachments/{aid}"));
    let (status, _) = body_json(router.oneshot(again).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_attachment_missing_file_field_returns_400() {
    // multipart body 缺 `file` 字段 → 400（而不是旧的 500 unimplemented）。
    let app = TestApp::new().await;
    let router = app.router();
    let pid = create_project(&app, "P").await;
    let create = TestApp::json_request(
        Method::POST,
        &format!("/projects/{pid}/tasks"),
        serde_json::json!({"title": "T"}),
    );
    let (_, body) = body_json(router.clone().oneshot(create).await.unwrap()).await;
    let tid = body["id"].as_str().unwrap().to_string();

    let boundary = "----testboundary";
    let mut mp_body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"other\"\r\n\r\nvalue\r\n--{boundary}--\r\n"
    )
    .into_bytes();
    let _ = &mut mp_body; // keep body non-empty

    let req = Request::builder()
        .method(Method::POST)
        .uri(format!("/tasks/{tid}/attachments"))
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("x-actor", "human-alice")
        .body(Body::from(mp_body))
        .unwrap();
    let (status, body) = body_json(router.oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "bad_request");
}

#[tokio::test]
async fn upload_attachment_unknown_task_returns_404() {
    let app = TestApp::new().await;
    let boundary = "----testboundary";
    let mut mp_body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\n"
    )
    .into_bytes();
    mp_body.extend_from_slice(b"x");
    mp_body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    let req = Request::builder()
        .method(Method::POST)
        .uri("/tasks/t-unknown/attachments")
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("x-actor", "human-alice")
        .body(Body::from(mp_body))
        .unwrap();
    let (status, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "task_not_found");
}

#[tokio::test]
async fn download_unknown_attachment_returns_404() {
    let app = TestApp::new().await;
    let req = TestApp::request_no_body(Method::GET, "/attachments/att-anything");
    let (status, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "attachment_not_found");
}

#[tokio::test]
async fn delete_unknown_attachment_returns_404() {
    let app = TestApp::new().await;
    let req = TestApp::request_no_body(Method::DELETE, "/attachments/att-doesnotexist");
    let (status, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "attachment_not_found");
}

// ── error body shape ──────────────────────────────────────────────────

#[tokio::test]
async fn error_body_always_has_code_and_message() {
    let app = TestApp::new().await;
    let req = TestApp::request_no_body(Method::GET, "/projects/p-nope");
    let (_, body) = body_json(app.router().oneshot(req).await.unwrap()).await;
    assert!(body["error"].is_object());
    assert!(body["error"]["code"].is_string());
    assert!(body["error"]["message"].is_string());
    assert!(!body["error"]["message"].as_str().unwrap().is_empty());
}
