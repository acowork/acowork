//! Integration tests for `GET/PUT /api/settings/default-compact-model`
//! (ADR-056).
//!
//! Each test:
//!   1. Spins up a real `axum::Router` via `routes::build_router(AppState)`
//!   2. Issues the request via `tower::ServiceExt::oneshot`
//!   3. Asserts on status code + JSON body
//!
//! The Gateway's MQTT publisher trigger is left unset (None) — that's
//! the realistic in-process test shape and avoids needing an embedded
//! MQTT broker.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use tower::ServiceExt;

use acowork_core::protocol::{CompactModelRef, ProtocolType, ProviderListItem, ProviderModelEntry};
use acowork_gateway::gateway::state::GatewayState;
use acowork_gateway::http::auth::HttpAuth;
use acowork_gateway::http::routes::{build_router, AppState};

/// Build an `AppState` wired to a fresh temp directory.
fn test_app_state() -> (AppState, std::path::PathBuf) {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-settings-api-{}-{}",
        std::process::id(),
        unique
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut gw_state = GatewayState::new(&dir.to_string_lossy());
    gw_state.config = Some(acowork_gateway::config::GatewayConfig {
        data_dir: dir.to_string_lossy().to_string(),
        ..Default::default()
    });

    (
        AppState::new(Arc::new(RwLock::new(gw_state)), Arc::new(HttpAuth::new(false))),
        dir,
    )
}

/// Seed `provider_list` with two providers, one of which carries a model
/// that satisfies the (provider_id, model_id) pair we test with.
async fn seed_provider_list(state: &AppState, dir: &std::path::Path) {
    let mut gw = state.gateway_state.write().await;
    gw.resource_cache.provider_list.providers = vec![
        ProviderListItem {
            id: "deepseek".to_string(),
            base_url: "https://api.deepseek.com/v1".to_string(),
            protocol_type: ProtocolType::OpenAI,
            compact_model: Some("deepseek-v4-flash".to_string()),
            custom: false,
            models: vec![ProviderModelEntry {
                id: "deepseek-v4-flash".to_string(),
                capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                    context_window: 128_000,
                    max_output_tokens: 16_384,
                    max_input_tokens: None,
                    supports_tool_calling: true,
                    supports_reasoning: None,
                    supports_attachment: None,
                    supports_temperature: None,
                    cost: None,
                    modalities: None,
                    name: None,
                    family: None,
                    knowledge_cutoff: None,
                    default_reasoning_effort: None,
                    thinking_mode: None,
                    reasoning_efforts: None,
                },
                max_output_tokens_limit: 32_768,
                protocol_type: None,
                base_url: None,
                protocol_probe: None,
            }],
        },
        ProviderListItem {
            id: "ollama-local".to_string(),
            base_url: "http://localhost:11434/v1".to_string(),
            protocol_type: ProtocolType::OpenAI,
            compact_model: None,
            custom: false,
            models: vec![ProviderModelEntry {
                id: "qwen2.5:0.5b".to_string(),
                capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                    context_window: 32_000,
                    max_output_tokens: 8_192,
                    max_input_tokens: None,
                    supports_tool_calling: true,
                    supports_reasoning: None,
                    supports_attachment: None,
                    supports_temperature: None,
                    cost: None,
                    modalities: None,
                    name: None,
                    family: None,
                    knowledge_cutoff: None,
                    default_reasoning_effort: None,
                    thinking_mode: None,
                    reasoning_efforts: None,
                },
                max_output_tokens_limit: 16_384,
                protocol_type: None,
                base_url: None,
                protocol_probe: None,
            }],
        },
    ];
    // Persist so that PUT /settings saves through the same write path.
    drop(gw);
    let gw = state.gateway_state.read().await;
    acowork_gateway::resource_cache::save_provider_list(dir, &gw.resource_cache.provider_list)
        .expect("save_provider_list");
}

async fn json_body(resp: axum::response::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let value: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_default_compact_models_returns_empty_initially() {
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;

    let router = build_router(state);
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/settings/default-compact-model")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "default_compact_models": [] }));

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn put_default_compact_model_happy_path_persists() {
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;

    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "default_compact_model": {
                            "provider_id": "ollama-local",
                            "model_id": "qwen2.5:0.5b"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "default_compact_models": [{
                "provider_id": "ollama-local",
                "model_id": "qwen2.5:0.5b"
            }]
        })
    );

    // GET reflects the new value.
    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .uri("/api/settings/default-compact-model")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["default_compact_models"],
        json!([ { "provider_id": "ollama-local", "model_id": "qwen2.5:0.5b" } ])
    );

    // Disk file has the same value (re-read with resource_cache helper).
    let reloaded_cache = acowork_gateway::resource_cache::load_resource_cache(&dir);
    assert_eq!(
        reloaded_cache.provider_list.default_compact_models,
        vec![CompactModelRef {
            provider_id: "ollama-local".to_string(),
            model_id: "qwen2.5:0.5b".to_string()
        }]
    );
    // version monotonically increased (fixture started at version=0).
    assert!(reloaded_cache.provider_list.version > 0);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn put_default_compact_model_with_unknown_provider_returns_422() {
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;

    let router = build_router(state);
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "default_compact_model": {
                            "provider_id": "anthropic",
                            "model_id": "claude-3-5-sonnet"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("anthropic"),
        "error should mention the unknown provider_id: {body}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn put_default_compact_model_with_wrong_model_returns_422() {
    // Cross-provider ref: `qwen2.5:0.5b` belongs to `ollama-local`, not
    // `deepseek`. Gateway must reject.
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;

    let router = build_router(state);
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "default_compact_model": {
                            "provider_id": "deepseek",
                            "model_id": "qwen2.5:0.5b"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let msg = body["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("qwen2.5:0.5b") && msg.contains("deepseek"),
        "error should mention both model and provider: {msg}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn put_default_compact_model_with_null_clears() {
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;

    // First set, then clear.
    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "default_compact_model": {
                            "provider_id": "deepseek",
                            "model_id": "deepseek-v4-flash"
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Now clear.
    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "default_compact_model": null }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "default_compact_models": [] }));

    // Disk also cleared.
    let reloaded_cache = acowork_gateway::resource_cache::load_resource_cache(&dir);
    assert!(reloaded_cache.provider_list.default_compact_models.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn put_default_compact_model_round_trip_with_two_puts_bumps_version() {
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;
    let v0 = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.provider_list.version
    };

    for body in [
        json!({ "default_compact_model": { "provider_id": "deepseek", "model_id": "deepseek-v4-flash" } }),
        json!({ "default_compact_model": { "provider_id": "ollama-local", "model_id": "qwen2.5:0.5b" } }),
    ] {
        let router = build_router(state.clone());
        let resp = router
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/settings/default-compact-model")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let gw = state.gateway_state.read().await;
    assert!(
        gw.resource_cache.provider_list.version > v0 + 1,
        "two PUTs must bump version at least twice"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn put_default_compact_models_list_form_round_trips_ordered() {
    // New list wire shape: ordered candidates survive PUT → GET → disk.
    let (state, dir) = test_app_state();
    seed_provider_list(&state, &dir).await;

    let list = json!([
        { "provider_id": "deepseek", "model_id": "deepseek-v4-flash" },
        { "provider_id": "ollama-local", "model_id": "qwen2.5:0.5b" }
    ]);
    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "default_compact_models": list }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, body) = json_body(resp).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["default_compact_models"], list);

    // Rejected atomically when ANY entry is invalid (nothing persisted).
    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/settings/default-compact-model")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "default_compact_models": [
                            { "provider_id": "ollama-local", "model_id": "qwen2.5:0.5b" },
                            { "provider_id": "anthropic", "model_id": "nope" }
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _body) = json_body(resp).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Old list survives the failed PUT.
    let reloaded_cache = acowork_gateway::resource_cache::load_resource_cache(&dir);
    assert_eq!(reloaded_cache.provider_list.default_compact_models.len(), 2);
    assert_eq!(reloaded_cache.provider_list.default_compact_models[0].provider_id, "deepseek");

    let _ = std::fs::remove_dir_all(&dir);
}
