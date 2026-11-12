//! LSP registry HTTP API on the Node (ADR-091 §5.5, D13).
//!
//! Eight routes under `/lsp/...`, node-token authenticated like every other
//! node-local route (`crate::proxy::authorize`). Permission checks — who may
//! install a shell script — live in the Gateway, which is the only process
//! that has a caller identity (§5.7); the Node sees one credential and one
//! policy.
//!
//! Two shapes coexist here on purpose (the comment in the module doc of
//! §5.5 of the ADR): the envelope is `camelCase` like the rest of the Node
//! HTTP surface, while a registry entry keeps the snake_case field names of
//! `lsp_servers.json` — it is the same object read from disk and served by the
//! relay's `/api/lsp/servers`, and renaming it would fork the schema.
//!
//! What each mutation does with the relay (D12 + §5.10):
//!
//! | route | relay restart? | why |
//! |-------|----------------|-----|
//! | `POST/PUT/DELETE /lsp/servers...` | yes, debounced | the relay reads its server list once per process |
//! | `POST /lsp/servers/{name}/install` | **no** | a forced probe refreshes the relay's profile PATH, so a new binary is visible without dropping the editors' Websockets |

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use acowork_core::lsp::{
    LSP_REGISTRY_VERSION, LspInstallState, LspRuntimeEntry, LspServerEntry,
};

use crate::lsp_install::{find_install_script, load_install_script, run_install_script, script_filename};
use crate::lsp_registry::RegistryError;
use crate::state::{LspRelayRestartRequest, NodeHttpState};

/// `GET /lsp/servers` and the mutation confirmations all answer with this
/// envelope: the merged registry plus the two things a persisted file cannot
/// say (in-flight installs, collisions).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegistryView {
    version: u32,
    servers: IndexMap<String, LspRuntimeEntry>,
    /// Preset/custom key collisions from the last merge (§5.3): the user's
    /// entry won, and the UI must say so instead of hiding it.
    conflicts: Vec<String>,
    /// Keys whose install script is running now (D10 — memory only, never in
    /// `servers[].install_state`).
    installing: Vec<String>,
}

/// `POST /lsp/servers` body: the key plus the metadata block, flat.
#[derive(Debug, Deserialize)]
struct AddCustomBody {
    name: String,
    #[serde(flatten)]
    metadata: LspServerEntry,
}

/// Response of a mutation that may require a relay restart (§5.6 item 2: the
/// flag is what the UI raises its confirmation dialog from).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MutationResponse {
    ok: bool,
    will_restart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<LspRuntimeEntry>,
}

/// `POST /lsp/servers/{name}/install` response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallResponse {
    name: String,
    success: bool,
    /// What the registry now persists for this key.
    install_state: LspInstallState,
    /// Always `false` today: install converges through a forced probe instead
    /// of a restart (§5.10). Kept in the contract so the UI does not have to
    /// special-case this route if that ever changes.
    will_restart: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    failure: Option<String>,
}

/// `GET /lsp/servers/{name}/install/status` response — the in-flight view.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallStatusResponse {
    name: String,
    installing: bool,
    install_state: LspInstallState,
    #[serde(skip_serializing_if = "Option::is_none")]
    installed_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_install_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_check_ms: Option<u64>,
}

pub fn router(state: NodeHttpState) -> Router {
    Router::new()
        .route("/lsp/servers", get(list_servers).post(add_server))
        .route("/lsp/presets", get(list_presets))
        .route("/lsp/servers/{name}/script", get(get_script))
        .route("/lsp/servers/{name}", put(update_server).delete(delete_server))
        .route("/lsp/servers/{name}/install", post(run_install))
        .route(
            "/lsp/servers/{name}/install/status",
            get(install_status),
        )
        .with_state(state)
}

/// Map a registry error onto an HTTP status. The messages are already
/// user-facing (they name the rule that failed), so they go through verbatim.
fn registry_error(e: RegistryError) -> Response {
    let (status, code) = match &e {
        RegistryError::InvalidName(_) => (StatusCode::BAD_REQUEST, "invalid_name"),
        RegistryError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "registry_io"),
        RegistryError::AlreadyExists(_) => (StatusCode::CONFLICT, "already_exists"),
        RegistryError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
        RegistryError::PresetNotRemovable(_) => (StatusCode::CONFLICT, "preset_not_removable"),
        RegistryError::InstallInFlight(_) => (StatusCode::CONFLICT, "install_in_flight"),
    };
    tracing::warn!(error = %e, code, "LSP registry request failed");
    (
        status,
        Json(serde_json::json!({ "error": e.to_string(), "code": code })),
    )
        .into_response()
}

/// Every route here is node-scoped: the token is the whole auth story on this
/// side of the boundary (§5.7).
async fn check_token(state: &NodeHttpState, headers: &HeaderMap) -> Option<Response> {
    crate::proxy::authorize(state, headers, "lsp").await
}

/// `GET /lsp/servers` — merged registry (preset ∪ custom ∪ persisted state).
///
/// Also trigger (c) of §5.8: when the persisted probe state is past
/// [`crate::lsp_registry::CONVERGE_TTL`], a background convergence is started
/// (non-forced, so the relay may serve it from its own TTL cache). The
/// response never waits for it — a full probe is seconds.
async fn list_servers(State(state): State<NodeHttpState>, headers: HeaderMap) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    let registry = state.lsp.clone();
    let snapshot = registry.snapshot().await;
    let installing = registry.installing_names().await;
    if registry.converge_stale().await {
        let relay = &state.lsp_relay;
        crate::lsp_converge::spawn_converge(
            registry.clone(),
            relay.bind_host.clone(),
            relay.port,
            false,
            "stale-read",
        );
    }
    Json(RegistryView {
        version: LSP_REGISTRY_VERSION,
        servers: snapshot.servers,
        conflicts: snapshot.conflicts,
        installing,
    })
    .into_response()
}

/// `GET /lsp/presets` — the read-only shipped registry, the candidate source
/// for the "add server" form (D1: preset metadata is never editable).
async fn list_presets(State(state): State<NodeHttpState>, headers: HeaderMap) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    Json(state.lsp.presets().await).into_response()
}

/// `GET /lsp/servers/{name}/script` — the install script text (D14: the UI
/// shows what it is about to run before running it).
async fn get_script(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    let entry = state.lsp.get(&name).await;
    let script = match entry.as_ref().and_then(|e| e.metadata.install_script.as_deref()) {
        Some(script) => script,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({
                    "error": format!("LSP server {name} has no install script"),
                    "code": "no_install_script",
                })),
            )
                .into_response();
        }
    };
    let filename = match script_filename(script) {
        Ok(filename) => filename,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e, "code": "invalid_script_name" })),
            )
                .into_response();
        }
    };
    let asset_dir = state.lsp.asset_dir();
    match load_install_script(asset_dir.as_deref(), &filename) {
        Some((path, content)) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_string();
            Json(serde_json::json!({
                "name": name,
                "filename": filename,
                "path": path.display().to_string(),
                "platform": ext,
                "script": content,
            }))
            .into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("Install script '{filename}' not found on this node"),
                "code": "script_not_found",
            })),
        )
            .into_response(),
    }
}

/// `POST /lsp/servers` — add a custom server (D4). Name rules and the
/// preset-collision refusal are the registry's (`RegistryError` → status).
async fn add_server(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Json(body): Json<AddCustomBody>,
) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    let name = body.name.clone();
    match state.lsp.add_custom(&name, body.metadata).await {
        Ok(()) => mutation_response(&state, name, true, "custom-server-added").await,
        Err(e) => registry_error(e),
    }
}

/// `PUT /lsp/servers/{name}` — update the instance block.
///
/// For a preset server only `candidate_args` is accepted (D6: the rest is
/// refreshed from the shipped preset on the next merge, so saving it would be
/// a promise the Node cannot keep).
async fn update_server(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(metadata): Json<LspServerEntry>,
) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    match state.lsp.update(&name, metadata).await {
        Ok(server) => {
            let will_restart = restart_relay(&state, "server-instance-changed");
            Json(MutationResponse {
                ok: true,
                will_restart,
                server: Some(server),
            })
            .into_response()
        }
        Err(e) => registry_error(e),
    }
}

/// `DELETE /lsp/servers/{name}` — drop the record.
///
/// The language-server binary itself stays on disk (ADR-091 §5.5): removing a
/// registry entry is not an uninstall of a third-party tool other tooling may
/// share, and a preset key cannot be deleted at all (it comes back from the
/// suite).
async fn delete_server(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    match state.lsp.delete(&name).await {
        Ok(()) => mutation_response(&state, name, true, "custom-server-removed").await,
        Err(e) => registry_error(e),
    }
}

/// `POST /lsp/servers/{name}/install` — the single install entry point (D15).
async fn run_install(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    let registry = state.lsp.clone();
    let entry = match registry.get(&name).await {
        Some(entry) => entry,
        None => return registry_error(RegistryError::NotFound(name.clone())),
    };
    let script = match entry.metadata.install_script.as_deref() {
        Some(script) => script.to_string(),
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("LSP server {name} has no install script — install it manually and it will be detected"),
                    "code": "no_install_script",
                })),
            )
                .into_response();
        }
    };
    let filename = match script_filename(&script) {
        Ok(filename) => filename,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e, "code": "invalid_script_name" })),
            )
                .into_response();
        }
    };
    let asset_dir = registry.asset_dir();
    let Some(path) = find_install_script(asset_dir.as_deref(), &filename) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("Install script '{filename}' not found on this node"),
                "code": "script_not_found",
            })),
        )
            .into_response();
    };

    // D10: the in-flight mark is memory-only, and the long part (the script
    // run) happens strictly outside the registry lock (§5.4).
    if let Err(e) = registry.begin_install(&name).await {
        return registry_error(e);
    }
    tracing::info!(name = %name, script = %path.display(), "Running LSP install script");
    let outcome = run_install_script(&path).await;
    let state_after = if outcome.success {
        LspInstallState::Installed
    } else {
        LspInstallState::Failed
    };
    if let Err(e) = registry
        .finish_install(
            &name,
            state_after,
            &outcome.stdout,
            outcome.failure.as_deref().unwrap_or(&outcome.stderr),
        )
        .await
    {
        // Release the mark: a record that could not be written must not lock
        // the key out of every later install attempt.
        registry.abort_install(&name).await;
        return registry_error(e);
    }

    // §5.8 trigger (b): ask the relay for a forced probe. That is also how a
    // newly installed binary becomes visible — the relay refreshes its profile
    // PATH on a forced probe, so no restart is needed here (§5.10).
    let relay = &state.lsp_relay;
    crate::lsp_converge::spawn_converge(
        registry.clone(),
        relay.bind_host.clone(),
        relay.port,
        true,
        "post-install",
    );

    // Report what was just written, not what a background convergence might
    // meanwhile decide — the UI re-reads the registry when it refreshes.
    Json(InstallResponse {
        name: name.clone(),
        success: outcome.success,
        install_state: state_after,
        will_restart: false,
        exit_code: outcome.exit_code,
        stdout: LspRuntimeEntry::truncate_output(&outcome.stdout),
        stderr: LspRuntimeEntry::truncate_output(
            outcome.failure.as_deref().unwrap_or(&outcome.stderr),
        ),
        failure: outcome.failure,
    })
    .into_response()
}

/// `GET /lsp/servers/{name}/install/status` — the in-flight phase (D10).
async fn install_status(
    State(state): State<NodeHttpState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    if let Some(denied) = check_token(&state, &headers).await {
        return denied;
    }
    let Some(entry) = state.lsp.get(&name).await else {
        return registry_error(RegistryError::NotFound(name.clone()));
    };
    let installing = state.lsp.is_installing(&name).await;
    Json(InstallStatusResponse {
        name,
        installing,
        install_state: entry.install.install_state,
        installed_command: entry.install.installed_command,
        last_install_ms: entry.install.last_install_ms,
        last_check_ms: entry.install.last_check_ms,
    })
    .into_response()
}

/// Ask the supervisor to restart the relay, reporting whether the request was
/// accepted (a closed channel means no supervisor is listening — the UI then
/// must not promise a restart it will not get).
fn restart_relay(state: &NodeHttpState, reason: &'static str) -> bool {
    let sent = state
        .lsp_relay
        .restart
        .send(LspRelayRestartRequest { reason })
        .is_ok();
    if !sent {
        tracing::warn!(
            reason,
            "LSP relay restart requested but no supervisor is listening"
        );
    }
    sent
}

/// A mutation response for the routes that do not return an entry.
async fn mutation_response(
    state: &NodeHttpState,
    name: String,
    restart: bool,
    reason: &'static str,
) -> Response {
    let will_restart = if restart {
        restart_relay(state, reason)
    } else {
        false
    };
    Json(MutationResponse {
        ok: true,
        will_restart,
        server: state.lsp.get(&name).await,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NodeConfig;
    use crate::identity::{EnrollmentState, NodeIdentity};
    use crate::lsp_registry::{for_tests, test_entry};
    use crate::state::{LspRelayControl, NodeState, SharedNodeState};
    use axum::body::Body;
    use axum::http::Request;
    use chrono::Utc;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test-token";

    fn state(home: &std::path::Path) -> NodeHttpState {
        NodeHttpState {
            node: Arc::new(RwLock::new(NodeState::new(8))) as SharedNodeState,
            config: NodeConfig {
                home: home.to_path_buf(),
                ..NodeConfig::default()
            },
            identity: Arc::new(RwLock::new(NodeIdentity {
                node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
                node_name: "node-1".to_string(),
                node_token: Some(TEST_TOKEN.to_string()),
                gateway_addr: None,
                enrollment: EnrollmentState::Enrolled,
                created_at: Utc::now(),
                enrolled_at: None,
            })),
            // Empty asset dir (the temp home): the install route must resolve
            // its script inside the fixture, never the repo's real scripts.
            lsp: for_tests(home, Some(home.to_path_buf())),
            // Port 1: convergence probes started by `GET /lsp/servers` must
            // fail fast here instead of reaching a relay a developer happens
            // to have running.
            lsp_relay: LspRelayControl::detached("127.0.0.1", 1),
        }
    }

    fn request(
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        token: Option<&str>,
    ) -> Request<Body> {
        let mut b = Request::builder().uri(uri).method(method);
        if let Some(t) = token {
            b = b.header("X-ACowork-Node-Token", t);
        }
        match body {
            Some(v) => b
                .header("Content-Type", "application/json")
                .body(Body::from(v.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        }
    }

    async fn send(state: NodeHttpState, req: Request<Body>) -> Response {
        router(state).oneshot(req).await.unwrap()
    }

    async fn json_of(resp: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Every route here writes machine state or reveals it, so all of them
    /// sit behind the same fail-closed token check as the rest of the node
    /// HTTP surface (§5.7).
    #[tokio::test]
    async fn every_route_refuses_a_missing_token() {
        let tmp = tempfile::tempdir().unwrap();
        // Write routes carry a body — axum's JSON extractor would otherwise
        // answer 415 before the handler's token check runs.
        let json_body = serde_json::json!({ "name": "x", "candidates": ["x"] });
        for (method, uri, body) in [
            ("GET", "/lsp/servers", None),
            ("GET", "/lsp/presets", None),
            ("POST", "/lsp/servers", Some(json_body.clone())),
            ("PUT", "/lsp/servers/rust", Some(json_body.clone())),
            ("DELETE", "/lsp/servers/rust", None),
            ("GET", "/lsp/servers/rust/script", None),
            ("POST", "/lsp/servers/rust/install", None),
            ("GET", "/lsp/servers/rust/install/status", None),
        ] {
            let resp = send(state(tmp.path()), request(method, uri, body, None)).await;
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn add_list_delete_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());

        let resp = send(
            state.clone(),
            request(
                "POST",
                "/lsp/servers",
                Some(serde_json::json!({
                    "name": "my-lsp",
                    "candidates": ["my-lsp-bin"],
                })),
                Some(TEST_TOKEN),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_of(resp).await;
        // The UI raises its "this restarts the relay" confirmation from here.
        assert_eq!(body["willRestart"], serde_json::json!(true));
        assert_eq!(body["server"]["source"], serde_json::json!("custom"));

        let resp = send(
            state.clone(),
            request("GET", "/lsp/servers", None, Some(TEST_TOKEN)),
        )
        .await;
        let body = json_of(resp).await;
        assert_eq!(
            body["servers"]["my-lsp"]["candidates"],
            serde_json::json!(["my-lsp-bin"])
        );
        assert!(body["conflicts"].as_array().unwrap().is_empty());

        let resp = send(
            state.clone(),
            request("DELETE", "/lsp/servers/my-lsp", None, Some(TEST_TOKEN)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.lsp.get("my-lsp").await.is_none());
    }

    /// The name is a URL segment, a JSON key and a script stem, so the API
    /// must refuse the shapes that would break all three.
    #[tokio::test]
    async fn add_rejects_an_invalid_name_and_a_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        let body = serde_json::json!({ "name": "Bad.Name", "candidates": ["x"] });
        let resp = send(
            state.clone(),
            request("POST", "/lsp/servers", Some(body), Some(TEST_TOKEN)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let ok = serde_json::json!({ "name": "my-lsp", "candidates": ["x"] });
        send(
            state.clone(),
            request("POST", "/lsp/servers", Some(ok), Some(TEST_TOKEN)),
        )
        .await;
        let dup = serde_json::json!({ "name": "my-lsp", "candidates": ["y"] });
        let resp = send(
            state.clone(),
            request("POST", "/lsp/servers", Some(dup), Some(TEST_TOKEN)),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn update_returns_the_entry_and_reports_the_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        state
            .lsp
            .add_custom("my-lsp", test_entry(&["my-lsp-bin"]))
            .await
            .unwrap();

        let resp = send(
            state.clone(),
            request(
                "PUT",
                "/lsp/servers/my-lsp",
                Some(serde_json::json!({ "candidates": ["my-lsp-new"] })),
                Some(TEST_TOKEN),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_of(resp).await;
        assert_eq!(
            body["server"]["candidates"],
            serde_json::json!(["my-lsp-new"])
        );
        assert_eq!(body["willRestart"], serde_json::json!(true));
    }

    #[tokio::test]
    async fn install_needs_a_script_and_a_place_to_find_it() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());

        // No install script at all → the user installs by hand, and the API
        // says so instead of running an empty command.
        let mut entry = test_entry(&["my-lsp-bin"]);
        entry.install_script = None;
        state.lsp.add_custom("no-script", entry).await.unwrap();
        let resp = send(
            state.clone(),
            request(
                "POST",
                "/lsp/servers/no-script/install",
                None,
                Some(TEST_TOKEN),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // A script name that resolves to nothing on this node → 404, and the
        // registry must not be left claiming an install is running. The stem
        // has to be one that exists nowhere: the lookup falls back to the
        // repository's own `assets/lsp_install/` (Q4), so a real preset name
        // here would execute a real installer.
        let mut ghost = test_entry(&["ghost-bin"]);
        ghost.install_script = Some("no-such-lsp-script".to_string());
        state.lsp.add_custom("ghost", ghost).await.unwrap();
        let resp = send(
            state.clone(),
            request(
                "POST",
                "/lsp/servers/ghost/install",
                None,
                Some(TEST_TOKEN),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(!state.lsp.is_installing("ghost").await);
    }

    #[tokio::test]
    async fn install_status_reports_the_persisted_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state = state(tmp.path());
        state
            .lsp
            .add_custom("my-lsp", test_entry(&["my-lsp-bin"]))
            .await
            .unwrap();
        let resp = send(
            state.clone(),
            request(
                "GET",
                "/lsp/servers/my-lsp/install/status",
                None,
                Some(TEST_TOKEN),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_of(resp).await;
        assert_eq!(body["installing"], serde_json::json!(false));
        assert_eq!(body["installState"], serde_json::json!("unknown"));
    }
}
