//! Consolidation / distill REST integration tests (ADR-101, P1e).
//!
//! Boots the real service and drives the two panel routes the Runtime used to
//! answer itself (`GET /memory/consolidation/status`, `POST /memory/distill`)
//! plus the `PUT` that replaces the Runtime's session-config write path.
//!
//! Locked down here:
//! - the **wire contract** the Desktop memory panel parses — the field names and
//!   nesting of the status body, the `started` + counters + `funnel` of the
//!   distill body, and the `409` / `500` / `503` classification with its
//!   `{"error": …}` payloads;
//! - that a `PUT` patches **one card's** fields without resetting the other's,
//!   is validated, is persisted next to the store, and takes effect on the live
//!   scheduler;
//! - that all of it is per instance namespace, and that an unusable
//!   `instance_id` never reaches a scheduler.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use acowork_memory::service::pipeline::SETTINGS_FILE_NAME;
use acowork_memory::service::state::validate_instance_id;
use acowork_memory::{DEFAULT_EMBEDDING_DIM, MemoryService, MemoryServiceConfig};
use axum::Json;
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;

const INSTANCE_A: &str = "11111111-1111-4111-8111-111111111111";
const INSTANCE_B: &str = "22222222-2222-4222-8222-222222222222";

struct TestServer {
    base: String,
    client: reqwest::Client,
    svc: Arc<MemoryService>,
    data: TempDir,
}

fn config_in(dir: &Path) -> MemoryServiceConfig {
    MemoryServiceConfig {
        data_dir: dir.to_path_buf(),
        ..Default::default()
    }
}

impl TestServer {
    async fn spawn() -> TestServer {
        let data = TempDir::new().unwrap();
        // P1e installs an embedding backend at startup, so point the local
        // `[embedding]` endpoint at a stub: the default sidecar port may be
        // occupied by a developer's running acowork-embed, and a test must not
        // observe the machine instead of the service.
        let mut config = config_in(data.path());
        config.embedding.base_url = stub_embed_server(DEFAULT_EMBEDDING_DIM).await;
        Self::serve(config, data).await
    }

    /// Serve `config`, keeping its data dir alive beside it.
    async fn serve(config: MemoryServiceConfig, data: TempDir) -> TestServer {
        let svc = Arc::new(MemoryService::new(config).await.unwrap());
        let addr: SocketAddr = svc
            .clone()
            .serve(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        TestServer {
            base: format!("http://{addr}"),
            client: reqwest::Client::new(),
            svc,
            data,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let resp = self.client.get(self.url(path)).send().await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn post(&self, path: &str) -> (StatusCode, Value) {
        let resp = self.client.post(self.url(path)).send().await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn put(&self, path: &str, payload: Value) -> (StatusCode, Value) {
        let resp = self
            .client
            .put(self.url(path))
            .json(&payload)
            .send()
            .await
            .unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    fn q(&self, id: &str) -> String {
        format!("instance_id={id}")
    }

    /// The policy file the panel write path owns.
    fn settings_file(&self, id: &str) -> PathBuf {
        self.data
            .path()
            .join("instances")
            .join(id)
            .join(SETTINGS_FILE_NAME)
    }
}

async fn body_json(resp: reqwest::Response) -> Value {
    let text = resp.text().await.unwrap_or_default();
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

/// Stub OpenAI-compatible embedding endpoint (full `{"data": [{"index",
/// "embedding"}]}` reply — a bare array of numbers does not parse as an
/// embedding on the client side).
async fn stub_embed_server(dimension: usize) -> String {
    let handler = axum::routing::post(move |Json(body): Json<Value>| async move {
        let n = body["input"].as_array().map(|a| a.len()).unwrap_or(1);
        let data: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "object": "embedding",
                    "index": i,
                    "embedding": (0..dimension)
                        .map(|d| d as f64 / dimension as f64)
                        .collect::<Vec<f64>>(),
                })
            })
            .collect();
        Json(json!({"data": data}))
    });
    let app = axum::Router::new().route("/v1/embeddings", handler);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/v1")
}

// ── GET /memory/consolidation/status ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn status_keeps_the_runtime_body_shape() {
    let server = TestServer::spawn().await;
    let (status, body) = server
        .get(&format!(
            "/memory/consolidation/status?{}",
            server.q(INSTANCE_A)
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Top level — the Runtime's four keys, and nothing replaced them.
    for key in ["idle_secs", "bg_task_running", "distiller", "forgetting"] {
        assert!(body.get(key).is_some(), "missing {key}: {body}");
    }
    assert_eq!(
        body["bg_task_running"],
        json!(true),
        "serve started the loops"
    );
    for key in [
        "enabled",
        "episode_backlog",
        "secs_since_distill",
        "interval_secs",
        "accumulation_threshold",
        "idle_secs",
        "last_run",
    ] {
        assert!(
            body["distiller"].get(key).is_some(),
            "distiller.{key}: {body}"
        );
    }
    for key in [
        "enabled",
        "half_life_days",
        "dormant_threshold",
        "archive_days",
        "interval_secs",
    ] {
        assert!(
            body["forgetting"].get(key).is_some(),
            "forgetting.{key}: {body}"
        );
    }
    // Opt-in defaults (config.toml), and a namespace that never distilled.
    assert_eq!(body["distiller"]["enabled"], json!(false));
    assert_eq!(body["forgetting"]["enabled"], json!(false));
    assert_eq!(body["distiller"]["interval_secs"], json!(3600));
    assert!(body["distiller"]["last_run"].is_null(), "{body}");
    // Additive: the model chain the panel's card reads back.
    assert_eq!(body["distiller"]["models"], json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn status_needs_a_usable_instance_id() {
    let server = TestServer::spawn().await;
    let (status, _) = server.get("/memory/consolidation/status").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .get("/memory/consolidation/status?instance_id=../escape")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // A rejected id never becomes a namespace directory or a scheduler.
    assert!(
        !server.data.path().join("instances").join("escape").exists(),
        "a traversal id opened a directory"
    );
    assert!(validate_instance_id("../escape").is_err());
}

// ── PUT /memory/consolidation/status ──────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn put_patches_one_card_and_leaves_the_other_alone() {
    let server = TestServer::spawn().await;
    let route = format!("/memory/consolidation/status?{}", server.q(INSTANCE_A));

    // The distiller card switches itself on and sets its own interval.
    let (status, after_distiller) = server
        .put(
            &route,
            json!({"distiller": {"enabled": true, "interval_secs": 900}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{after_distiller}");
    assert_eq!(after_distiller["distiller"]["enabled"], json!(true));
    assert_eq!(after_distiller["distiller"]["interval_secs"], json!(900));

    // The forgetting card then saves its own toggles: the distiller settings it
    // did not send must survive (a whole-section replace would reset them).
    let (status, after_forgetting) = server
        .put(
            &route,
            json!({"forgetting": {"enabled": true, "half_life_days": 30}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{after_forgetting}");
    assert_eq!(after_forgetting["forgetting"]["enabled"], json!(true));
    assert_eq!(after_forgetting["forgetting"]["half_life_days"], json!(30));
    assert_eq!(
        after_forgetting["distiller"]["enabled"],
        json!(true),
        "the distiller card's choice was reset by an unrelated write"
    );
    assert_eq!(after_forgetting["distiller"]["interval_secs"], json!(900));

    // Read back what the live scheduler now runs on.
    let (_, live) = server.get(&route).await;
    assert_eq!(live["distiller"]["interval_secs"], json!(900));

    // The model chain round-trips through the same route.
    let (status, with_models) = server
        .put(
            &route,
            json!({"distiller": {"models": [{"provider_id": "volc", "model_id": "kimi"}]}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{with_models}");
    assert_eq!(
        with_models["distiller"]["models"],
        json!([{"provider_id": "volc", "model_id": "kimi"}])
    );

    // A new process on the same data dir reads the file, not the config default
    // — a Gateway restart must not silently switch a user's opt-in back off.
    let reopened = MemoryService::new(config_in(server.data.path()))
        .await
        .unwrap();
    let settings = reopened.state.pipeline().settings(INSTANCE_A);
    assert!(settings.distiller.enabled, "{settings:?}");
    assert_eq!(settings.distiller.interval_secs, 900);
    assert!(settings.forgetting.enabled, "{settings:?}");
    assert_eq!(settings.forgetting.half_life_days, 30);
    assert_eq!(settings.distiller.models.len(), 1);
    assert!(server.settings_file(INSTANCE_A).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn put_validates_before_writing() {
    let server = TestServer::spawn().await;
    let route = format!("/memory/consolidation/status?{}", server.q(INSTANCE_A));

    // A typo in a field name must not be silently dropped.
    let (status, _) = server
        .put(
            &route,
            json!({"distiller": {"enabled": true, "interva": 5}}),
        )
        .await;
    assert!(status.is_client_error(), "unknown field accepted: {status}");

    // A zero interval would make the gate fire on every poll.
    let (status, _) = server
        .put(&route, json!({"distiller": {"interval_secs": 0}}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .put(&route, json!({"distiller": {"batch_size": 0}}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .put(&route, json!({"forgetting": {"dormant_threshold": 0.0}}))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // An entry without a model id can never resolve to a candidate.
    let (status, _) = server
        .put(
            &route,
            json!({"distiller": {"models": [{"provider_id": "volc", "model_id": "  "}]}}),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // None of that reached the disk or the scheduler.
    assert!(!server.settings_file(INSTANCE_A).exists());
    let (_, body) = server.get(&route).await;
    assert_eq!(body["distiller"]["enabled"], json!(false));
    assert_eq!(body["distiller"]["interval_secs"], json!(3600));
}

// ── POST /memory/distill ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn distill_conflicts_while_disabled_and_needs_a_usable_id() {
    let server = TestServer::spawn().await;

    let (status, body) = server
        .post(&format!("/memory/distill?{}", server.q(INSTANCE_A)))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("distiller is disabled"),
        "the panel cannot tell a switch from a crash: {body}"
    );

    let (status, _) = server.post("/memory/distill").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = server
        .post("/memory/distill?instance_id=with%20space")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn distill_conflicts_without_an_embedding_backend() {
    let server = TestServer::spawn().await;
    let route = format!("/memory/consolidation/status?{}", server.q(INSTANCE_A));
    server
        .put(&route, json!({"distiller": {"enabled": true}}))
        .await;

    // No callable embedding path ⇒ the distiller would project every episode as
    // a new node instead of merging it. The route refuses instead.
    server.svc.state.pipeline().set_embedding(None);
    let (status, body) = server
        .post(&format!("/memory/distill?{}", server.q(INSTANCE_A)))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("embedding provider not initialized"),
        "{body}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn distill_runs_and_leaves_a_run_record_the_panel_can_read() {
    let server = TestServer::spawn().await;
    let endpoint = stub_embed_server(DEFAULT_EMBEDDING_DIM).await;
    // Credentials arrive the way the background loop delivers them.
    server.svc.state.pipeline().apply_snapshot(
        acowork_memory::service::resources::GlobalResources {
            embedding: Some(acowork_memory::service::resources::EmbeddingSnapshot {
                base_url: Some(endpoint),
                model: Some("bge-test".into()),
                api_key: None,
                dimension: DEFAULT_EMBEDDING_DIM,
                cloud: false,
            }),
            ..Default::default()
        },
    );

    let status_route = format!("/memory/consolidation/status?{}", server.q(INSTANCE_A));
    server
        .put(&status_route, json!({"distiller": {"enabled": true}}))
        .await;

    let (status, body) = server
        .post(&format!("/memory/distill?{}", server.q(INSTANCE_A)))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["started"], json!(true));
    for key in [
        "episodes_scanned",
        "facts_promoted",
        "preferences_promoted",
        "relations_promoted",
        "procedures_promoted",
        "autobio_promoted",
        "episodes_marked_consolidated",
    ] {
        assert!(body.get(key).is_some(), "missing {key}: {body}");
    }
    // This service writes knowledge nodes through MCP; episodes come from the
    // conversation recorder, so a manual run here is a pass over an empty
    // episodic layer — which is still a completed run.
    assert_eq!(body["episodes_scanned"], json!(0));
    assert_eq!(body["funnel"]["scanned"], json!(0));

    // The run is remembered for the panel's "上次运行", with no error.
    let (_, after) = server.get(&status_route).await;
    let last_run = &after["distiller"]["last_run"];
    assert!(!last_run.is_null(), "{after}");
    assert_eq!(last_run["error"], Value::Null, "{last_run}");
    assert!(last_run["at"].as_str().is_some(), "{last_run}");
    assert_eq!(last_run["episodes_scanned"], json!(0));
    assert_eq!(last_run["total_promoted"], json!(0));
    assert!(last_run["funnel"].get("backlog_remaining").is_some());

    // And the next manual trigger is not blocked by the interval gate: force
    // semantics bypass it (ADR-071 D2).
    let (status, _) = server
        .post(&format!("/memory/distill?{}", server.q(INSTANCE_A)))
        .await;
    assert_eq!(status, StatusCode::OK, "a second forced run was gated");
}

#[tokio::test(flavor = "multi_thread")]
async fn scheduling_state_is_per_instance() {
    let server = TestServer::spawn().await;
    let a = format!("/memory/consolidation/status?{}", server.q(INSTANCE_A));
    let b = format!("/memory/consolidation/status?{}", server.q(INSTANCE_B));

    server
        .put(
            &a,
            json!({"distiller": {"enabled": true, "interval_secs": 600}}),
        )
        .await;

    let (_, body_b) = server.get(&b).await;
    assert_eq!(
        body_b["distiller"]["enabled"],
        json!(false),
        "A's policy leaked into B"
    );
    assert_eq!(body_b["distiller"]["interval_secs"], json!(3600));
    assert!(
        !server.settings_file(INSTANCE_B).exists(),
        "B was written to"
    );

    // B's scheduler never ran either, so its idle clock is its own.
    let (_, body_a) = server.get(&a).await;
    assert_eq!(body_a["distiller"]["interval_secs"], json!(600));
}

