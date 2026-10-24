//! End-to-end test: spin up a real RuntimeHttpServer on a random port,
//! then exercise the GET / PUT /agents/{id}/shell-risk-rules handlers
//! via reqwest. This is what the desktop "Save" button hits in
//! production, so it's the only way to catch route-level regressions
//! before the user does.

use std::sync::Arc;

/// Test-only instance identity (ADR-073: must be a UUIDv4).
const INSTANCE_ID: &str = "0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d";

#[tokio::test]
async fn test_shell_risk_rules_get_put_roundtrip() {
    let temp_dir = std::env::temp_dir().join("acowork-test-shell-risk-e2e");
    let _ = std::fs::remove_dir_all(&temp_dir);
    std::fs::create_dir_all(&temp_dir).unwrap();

    let snapshots = Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
    let latest = Arc::new(std::sync::RwLock::new(None));
    let dispatch_tx = Arc::new(tokio::sync::Mutex::new(None));
    let degraded_reasons = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mqtt_client = Arc::new(tokio::sync::Mutex::new(None));
    let session_metadata = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_query = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation = Arc::new(tokio::sync::Mutex::new(None));
    let agent_tools = Arc::new(tokio::sync::Mutex::new(None));
    let agent_config = Arc::new(tokio::sync::Mutex::new(None));
    let attachment = Arc::new(tokio::sync::Mutex::new(None));
    let session_config = Arc::new(tokio::sync::Mutex::new(None));
    let rag_provider: Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>> =
        Arc::new(std::sync::RwLock::new(None));
    let debug_service = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_resolver: Arc<
        std::sync::RwLock<acowork_runtime::tools::workspace_resolver::WorkspaceResolver>,
    > = Arc::new(std::sync::RwLock::new(
        acowork_runtime::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
    ));
    let session_manager_slot: Arc<
        tokio::sync::RwLock<
            Option<Arc<tokio::sync::Mutex<acowork_runtime::agent::session::SessionManager>>>,
        >,
    > = Arc::new(tokio::sync::RwLock::new(None));

    let server = acowork_runtime::http::RuntimeHttpServer::start(
        temp_dir.clone(),
        temp_dir.clone(), // package_dir (ADR-063): tests reuse work_dir as package dir
        "com.test.agent".to_string(),
        INSTANCE_ID.to_string(),
        snapshots,
        latest,
        dispatch_tx,
        degraded_reasons,
        mqtt_client,
        session_metadata,
        std::sync::Arc::new(std::sync::RwLock::new(None)),
        workspace_query,
        workspace_mutation,
        agent_tools,
        Arc::new(tokio::sync::Mutex::new(None)),
        agent_config,
        attachment,
        session_config,
        rag_provider,
        debug_service,
        workspace_resolver,
        session_manager_slot,
        std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
    )
    .await
    .expect("server should start");

    let base = format!("http://127.0.0.1:{}", server.port);
    let client = reqwest::Client::new();

    // Step 1: GET — first access MATERIALIZES the user template on disk
    // (UX contract, see get_shell_risk_rules: "clicking the Edit button
    // creates the user file on disk if it does not already exist" so the
    // file appears in the agent file tree and the frontend can show the
    // "local copy" hint). has_user_override is therefore `true` even on
    // first GET; the content is the generated template, not a user edit.
    let resp = client
        .get(format!("{}/agents/{}/shell-risk-rules", base, INSTANCE_ID))
        .send()
        .await
        .expect("GET should not error");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["has_user_override"], true,
        "first GET must materialize the user template"
    );
    let content = body["content"]
        .as_str()
        .expect("content should be a string");
    assert!(!content.is_empty(), "default content should not be empty");
    // The materialized template must parse as valid TOML and contain no
    // active user rules (the embedded defaults are comments).
    let parsed: serde_json::Value =
        toml::from_str(content).expect("materialized template must parse as TOML");
    assert_eq!(
        parsed["rules"].as_array().map(|a| a.len()).unwrap_or(0),
        0,
        "materialized template must have zero active user rules"
    );
    println!("[e2e] default content len = {}", content.len());

    // Step 2: PUT a valid override
    let new_rules =
        "[[rules]]\ncommand = \"echo\"\nrisk = \"Low\"\nreason = \"safe test override\"\n";
    let resp = client
        .put(format!("{}/agents/{}/shell-risk-rules", base, INSTANCE_ID))
        .json(&serde_json::json!({ "content": new_rules }))
        .send()
        .await
        .expect("PUT should not error");
    assert_eq!(resp.status(), 200, "PUT should succeed");

    // Step 3: GET again — should now reflect the override on disk
    let resp = client
        .get(format!("{}/agents/{}/shell-risk-rules", base, INSTANCE_ID))
        .send()
        .await
        .expect("GET should not error");
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["has_user_override"], true);
    let content = body["content"]
        .as_str()
        .expect("content should be a string");
    assert_eq!(
        content, new_rules,
        "GET should return the override verbatim"
    );

    // Step 4: PUT invalid TOML — should be rejected with 400
    let resp = client
        .put(format!("{}/agents/{}/shell-risk-rules", base, INSTANCE_ID))
        .json(&serde_json::json!({ "content": "this is = not toml = at all" }))
        .send()
        .await
        .expect("PUT should not error at the HTTP level");
    assert_eq!(resp.status(), 400, "invalid TOML must return 400");

    // Step 5: PUT for wrong agent_id — should be rejected with 404
    let resp = client
        .put(format!("{}/agents/wrong.agent/shell-risk-rules", base))
        .json(&serde_json::json!({ "content": new_rules }))
        .send()
        .await
        .expect("PUT should not error at the HTTP level");
    assert_eq!(resp.status(), 404, "wrong agent_id must return 404");

    // Cleanup
    std::fs::remove_dir_all(&temp_dir).ok();
}
