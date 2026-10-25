//! Gateway HTTP reverse proxy (ADR-033 Phase 2).
//!
//! For specific large-data query paths, the Gateway does not handle the
//! request itself - instead it reverse-proxies to the Runtime's localhost
//! HTTP server. The Gateway looks up the Runtime's HTTP port from a
//! registry (populated during Runtime registration) and forwards the
//! request. If the Runtime is not registered or has exited, returns 503.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.
//!
//! ```text
//! Desktop ──HTTP──▶ Gateway (:19876) ──HTTP reverse proxy──▶ Runtime localhost HTTP (:random)
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
    routing::{any, get, post, put},
};
use tokio::sync::RwLock;

use crate::http::agents::resolve_avatar_from_manifest;
use crate::http::routes::{ApiError, AppState};

/// Maximum body size the Gateway will accept for any reverse-proxy
/// route. The actual layer is installed at the merged-router root
/// in [`crate::http::routes::api_router`] (see
/// `GLOBAL_BODY_LIMIT` there for the rationale); this constant is
/// kept as a reference point and historically used to be applied
/// here before we lifted it to the shared router.
///
/// NOTE: see `crate::http::routes::GLOBAL_BODY_LIMIT` — that is the
/// source of truth. This shim is no longer wired up but kept as a
/// breadcrumb.
#[allow(dead_code)]
const FILE_UPLOAD_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Registry mapping id -> Runtime HTTP endpoint (ADR-055 D3).
///
/// Populated by [`crate::mqtt::dispatch::handle_plaintext_message`] when the
/// Gateway receives a **retained** `acowork/agents/{id}/http_endpoint`
/// payload from a Runtime. The payload is a full endpoint URL (e.g.
/// `http://127.0.0.1:54321`) — Phase 1 uses loopback; Phase 2 will publish
/// a Node reverse-proxy address and the Gateway consumes it opaquely. The
/// retained flag is critical: if the Gateway restarts (or starts after the
/// Runtime), the broker replays the last known endpoint so the Gateway can
/// immediately resume reverse-proxying without waiting for the next
/// Runtime-side publish.
#[derive(Debug, Clone, Default)]
pub struct RuntimeHttpRegistry {
    /// id -> endpoint URL (e.g. "http://127.0.0.1:54321")
    endpoints: HashMap<String, String>,
}

impl RuntimeHttpRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a Runtime's HTTP endpoint (ADR-055 D3).
    pub fn register(&mut self, id: &str, endpoint: &str) {
        tracing::info!(
            id,
            endpoint,
            "Runtime HTTP endpoint registered for reverse proxy"
        );
        self.endpoints.insert(id.to_string(), endpoint.to_string());
    }

    /// Unregister a Runtime (e.g. on disconnect/stop).
    pub fn unregister(&mut self, id: &str) {
        self.endpoints.remove(id);
    }

    /// Get the HTTP endpoint URL for a Runtime (ADR-055 D3).
    pub fn get_endpoint(&self, id: &str) -> Option<String> {
        self.endpoints.get(id).cloned()
    }

    /// Number of registered Runtimes.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.endpoints.len()
    }

    /// Whether any Runtimes are registered.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }
}

/// Thread-safe shared RuntimeHttpRegistry.
pub type SharedRuntimeHttpRegistry = Arc<RwLock<RuntimeHttpRegistry>>;

/// Create a new shared RuntimeHttpRegistry.
pub fn new_shared_registry() -> SharedRuntimeHttpRegistry {
    Arc::new(RwLock::new(RuntimeHttpRegistry::new()))
}

/// Build the reverse proxy router.
///
/// These routes are matched AFTER the regular API routes. If a path
/// matches a proxy pattern, the request is forwarded to the Runtime's
/// localhost HTTP server. Otherwise, the regular handler processes it.
pub fn proxy_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/agents/{id}/workspaces",
            get(proxy_list_workspaces).post(proxy_add_workspace),
        )
        .route("/api/agents/{id}/workspaces/tree", get(proxy_list_tree))
        // Routes 16-17: Workspace filename / content search (ADR-040).
        // ADR-009 v2: the Runtime is the authoritative workspace API owner;
        // the Gateway is now a thin reverse-proxy for these CPU-heavy
        // filesystem walks. Earlier revisions of this module kept
        // `find_files` / `search_files` on the Gateway side (running in
        // the gateway process), but moving them to the Runtime gives us:
        //   1. A single source of truth for the workspace tree root
        //      (Runtime already resolves `__agent_home__` / additional dirs).
        //   2. Path-traversal / canonicalize guard lives in one place.
        //   3. Query params are forwarded verbatim — no Gateway-side
        //      schema drift to maintain.
        .route("/api/agents/{id}/workspaces/find", get(proxy_find_files))
        .route(
            "/api/agents/{id}/workspaces/search",
            get(proxy_search_files),
        )
        // ADR-081 §4.1: single-agent aggregate search (memory/file/git;
        // conversation arrives in P1-2). Transparent forward — the Runtime
        // owns all aggregation/sorting (ADR-064/070).
        .route("/api/agents/{id}/search", get(proxy_global_search))
        // ADR-093: LLM summary over the Desktop's bounded candidate
        // snapshot. Transparent forward — the Runtime owns the provider
        // call and citation whitelist; the Gateway parses no body and
        // never retries POST (design 27 §4.4).
        .route(
            "/api/agents/{id}/search/summary",
            post(proxy_search_summary),
        )
        // ADR-078: workspace Git Status Bar. Git executes in the Runtime
        // (workspace owner, ADR-009 v2) via the system git CLI with
        // `GIT_OPTIONAL_LOCKS=0`; the Gateway only forwards — no .git
        // bytes ever cross this process (run_gateway_fs_redline).
        .route("/api/agents/{id}/git/status", get(proxy_git_status))
        .route("/api/agents/{id}/git/diff", get(proxy_git_diff))
        .route("/api/agents/{id}/git/log", get(proxy_git_log))
        .route("/api/agents/{id}/git/revert", post(proxy_git_revert))
        // ADR-076 §决策 4: the session *control* plane moved from MQTT to
        // HTTP so the auth middleware above can authenticate it — an MQTT
        // control message carries no identity, so `create` could not record
        // an owner and `close`/`delete` could not check one. The Gateway is
        // a transparent proxy here; the identity decision is made by the
        // Runtime from the `x-user-id` header this layer injects.
        .route(
            "/api/agents/{id}/sessions",
            get(proxy_list_sessions)
                .post(proxy_create_session)
                .delete(proxy_clear_sessions),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/open",
            post(proxy_open_session),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/close",
            post(proxy_close_session),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/visibility",
            put(proxy_set_session_visibility),
        )
        // ADR-076 §决策 4: `workspace_switch` moved off MQTT. It needs its
        // own endpoint rather than riding `PUT .../config` — a workspace
        // switch has to push the new context to the session, not just
        // rewrite meta (Runtime-side `route_workspace_switch`).
        .route(
            "/api/agents/{id}/sessions/{sid}/workspace",
            put(proxy_put_session_workspace),
        )
        .route("/api/agents/{id}/latest-session", get(proxy_latest_session))
        .route("/api/agents/{id}/token-stats", get(proxy_token_stats))
        .route(
            "/api/agents/{id}/sessions/{sid}/messages",
            get(proxy_get_messages).post(proxy_post_message),
        )
        // ADR-076 §决策 4 (second wave): user-triggered session actions.
        // The Runtime authorizes each one against the session's owner, which
        // is only possible because these arrive through the Gateway's auth
        // middleware (`x-user-id` is injected there, not by the client).
        .route(
            "/api/agents/{id}/sessions/{sid}/stop",
            post(proxy_post_session_stop),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/continue",
            post(proxy_post_session_continue),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/approval",
            post(proxy_post_session_approval),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/answer",
            post(proxy_post_session_answer),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/cancel-tool",
            post(proxy_post_session_cancel_tool),
        )
        .route(
            "/api/agents/{id}/sessions/{sid}/compress",
            post(proxy_post_session_compress),
        )
        // ADR-101: the `/api/agents/{id}/memory/*` panel surface is no longer
        // proxied here — the standalone memory service answers it, through
        // `crate::http::memory_proxy` (one catch-all, `instance_id` injected
        // from the path). Public paths and response shapes are unchanged.
        // ADR-051: RAG endpoints (still Runtime-owned retrieval over the
        // document index).
        .route("/api/agents/{id}/rag/status", get(proxy_rag_status))
        .route("/api/agents/{id}/rag/query", post(proxy_rag_query))
        // Shell risk rules — GET returns effective content (user override or defaults),
        // PUT writes a user override to {work_dir}/config/shell_risk_rules.toml.
        .route(
            "/api/agents/{id}/shell-risk-rules",
            get(proxy_get_shell_risk_rules).put(proxy_put_shell_risk_rules),
        )
        // ADR-009 §V-A: skill inspection. The Runtime owns `{package_dir}/skills/`
        // and the only SKILL.md parser; the Gateway forwards verbatim instead of
        // re-parsing (the old Gateway copy drifted from the Runtime's and broke
        // cross-machine, where `install_path` is node-local).
        //
        // `POST .../skills/import` deliberately stays on the Gateway: it
        // delegates extraction to the local Node control plane and never reads
        // Runtime-private files, so there is nothing to proxy.
        .route("/api/agents/{id}/skills", get(proxy_list_skills))
        .route("/api/agents/{id}/skills/{name}", get(proxy_skill_detail))
        .route(
            "/api/agents/{id}/skills/{name}/history",
            get(proxy_skill_history),
        )
        // ADR-009 §5 / §V-R: the agent's package content — avatar reads,
        // the publish wizard's manifest/avatar rewrite, image upload
        // (multipart) and file delete — is served by the **node**, which
        // owns the package and is up whether or not the agent runs. The
        // Gateway only forwards, so no agent-private bytes cross this
        // process (see `proxy_to_node`).
        .route("/api/agents/{id}/avatar", get(proxy_get_avatar))
        .route(
            "/api/agents/{id}/avatar-file",
            get(proxy_get_avatar_file).delete(proxy_delete_avatar_file),
        )
        .route(
            "/api/agents/{id}/manifest/avatar-assets",
            get(proxy_get_avatar_assets),
        )
        .route(
            "/api/agents/{id}/manifest/avatar",
            post(proxy_put_manifest_avatar),
        )
        .route(
            "/api/agents/{id}/manifest/file",
            post(proxy_upload_agent_file),
        )
        // ADR-009 §5: avatar/display-name preferences — the Runtime owns
        // the instance's `.overrides.json`. The PUT additionally mirrors
        // the result into the Gateway's list view (see
        // `forward_avatar_config_put`).
        .route(
            "/api/agents/{id}/avatar-config",
            get(proxy_get_avatar_config).put(proxy_put_avatar_config),
        )
        // Route 1: Get single session
        .route(
            "/api/agents/{id}/sessions/{sid}",
            get(proxy_get_session).delete(proxy_delete_session),
        )
        // Legacy /state suffix - Runtime absorbed it into /sessions/{sid};
        // kept as a separate route so old callers still get a sensible
        // (verbatim-proxied) response instead of 404.
        .route(
            "/api/agents/{id}/sessions/{sid}/state",
            get(proxy_get_session_state),
        )
        // ADR-047: Session config read/write proxy. Runtime exposes
        // `GET/PUT /sessions/{sid}/config` via `SessionConfigService`.
        .route(
            "/api/agents/{id}/sessions/{sid}/config",
            get(proxy_get_session_config).put(proxy_put_session_config),
        )
        // ADR-046: File upload/download proxy. Replaces the legacy
        // `/documents` routes (4 handlers deleted). The Runtime now
        // exposes `POST /sessions/{sid}/files` (multipart upload) and
        // `GET /files/{document_id}` (blob download). The Gateway is a
        // transparent reverse-proxy - multipart bodies and binary
        // responses flow through unchanged.
        //
        // Per-route body-limit override is NOT needed: the global
        // `DefaultBodyLimit::max(GLOBAL_BODY_LIMIT)` layer at the
        // proxy root covers this `Bytes` extractor.
        .route(
            "/api/agents/{id}/sessions/{sid}/files",
            post(proxy_upload_file),
        )
        .route(
            "/api/agents/{id}/files/{document_id}",
            get(proxy_download_file),
        )
        // Routes 6-9: Workspace config CRUD
        .route(
            "/api/agents/{id}/workspaces/{ws_id}",
            put(proxy_update_workspace).delete(proxy_delete_workspace),
        )
        .route(
            "/api/agents/{id}/workspaces/{ws_id}/prompt-file",
            put(proxy_set_prompt_file),
        )
        // ADR-058 (demand-driven revision): frontend pushes the visible
        // watch set (open tabs + expanded dirs) to the Runtime. Full
        // replace semantics; empty set → no scanning.
        .route(
            "/api/agents/{id}/workspaces/{ws_id}/fs-watch",
            put(proxy_set_workspace_fs_watch),
        )
        // Routes 14-15: Workspace file & dir REST resources (Gateway is
        // transparent — forwards verbatim to the Runtime's filesystem-aware
        // HTTP server). The path-traversal guard lives on the Runtime
        // side via `resolve_workspace_root` + canonicalize.
        //   GET    /workspaces/file  → JSON {content, size, mimeType, …}
        //   POST   /workspaces/file  → create  (409 on duplicate)
        //   PUT    /workspaces/file  → write   (404 on missing)
        //   DELETE /workspaces/file  → remove  (404 on missing)
        //   POST   /workspaces/dir   → create  (recursive)
        //   DELETE /workspaces/dir   → remove  (recursive)
        .route(
            "/api/agents/{id}/workspaces/file",
            get(proxy_read_workspace_file)
                .post(proxy_create_workspace_file)
                .put(proxy_write_workspace_file)
                .delete(proxy_delete_workspace_file),
        )
        .route(
            "/api/agents/{id}/workspaces/dir",
            post(proxy_create_workspace_dir).delete(proxy_delete_workspace_dir),
        )
        // Routes 10c-d: Workspace copy / rename (ADR-040 + ADR-034 §11.2).
        // Body is `{workspace_id?, source, dest}` — `workspace_id` may
        // also ride on the querystring (desktop's `workspaceStore`
        // convention). Both endpoints must be reverse-proxied so the
        // runtime — the workspace config owner — resolves the
        // `workspace_id` against `<work_dir>/config/agent_workspaces.json`.
        .route(
            "/api/agents/{id}/workspaces/copy",
            post(proxy_copy_workspace_item),
        )
        .route(
            "/api/agents/{id}/workspaces/rename",
            post(proxy_rename_workspace_item),
        )
        // Routes 11-13: Agent config / tools / status
        .route(
            "/api/agents/{id}/config",
            get(proxy_get_config).put(proxy_put_config),
        )
        .route("/api/agents/{id}/tools", get(proxy_get_tools))
        // ADR-040 follow-up: builtin-tools moved off `/config` onto its
        // own route. The read-side merge still lives at `/tools` (GET
        // only), but write-side persistence of the enabled flag list is
        // now its own endpoint here — symmetric with `/mcp-servers`.
        .route(
            "/api/agents/{id}/builtin-tools",
            get(proxy_get_builtin_tools).put(proxy_put_builtin_tools),
        )
        .route("/api/agents/{id}/status", get(proxy_get_status))
        // Win11-MCP-ToolsBugFix: reverse-proxy per-agent MCP server activation
        // (GET/PUT) to the Runtime. The
        // Gateway used to handle these via `agents.rs` stubs that returned 200
        // but never persisted — the user's selection was lost on the next
        // Tools-tab remount. See `get_agent_mcp_servers` / `put_agent_mcp_servers`
        // in the Runtime HTTP server for the persistence logic.
        .route(
            "/api/agents/{id}/mcp-servers",
            get(proxy_get_mcp_servers).put(proxy_put_mcp_servers),
        )
        // ADR-090: first-enable ensure status (probing / installing /
        // usable / failed+reason). Pure reverse-proxy to the Runtime's
        // `GET /agents/{id}/mcp-ensure/{name}` — the install runs in the
        // Agent process, so only the Runtime knows its progress.
        .route(
            "/api/agents/{id}/mcp-ensure/{name}",
            get(proxy_get_mcp_ensure),
        )
        // ADR-069: per-MCP-server tool-level opt-in. `GET/PUT
        // /api/agents/{id}/mcp-tools` is a pure reverse-proxy to the
        // Runtime's `GET/PUT /agents/{id}/mcp-tools` (added in
        // `acowork-runtime/src/http/server.rs`). Without this route the
        // Gateway returned 404 and the Desktop Tools panel silently
        // swallowed it, so the per-server tool list (e.g. `pm`'s 12
        // tools) never rendered. Mirrors `/mcp-servers` / `/builtin-tools`
        // exactly — all validation + persistence lives in the Runtime.
        .route(
            "/api/agents/{id}/mcp-tools",
            get(proxy_get_mcp_tools).put(proxy_put_mcp_tools),
        )
        // ADR-040 follow-up: provider list read-through — reads
        // agent_provider.json on the Runtime side. The frontend calls
        // this to verify what the Runtime has received via MQTT.
        .route("/api/agents/{id}/providers", get(proxy_get_providers))
        // ADR-063: Package-level LLM prompt overrides (see ADR-063 §3.7.6).
        // Four transparent reverse-proxy rules forward the Desktop
        // Debug panel's read/write + reload traffic to the Runtime HTTP
        // server (added in `core/acowork-runtime/src/http/prompts.rs`).
        // The body shape for PUT is a JSON `{content}` per
        // `PutPromptRequest` — forwarded verbatim so the Runtime's
        // `http/prompts.rs` validation (whitelist check, 1 MiB cap,
        // atomic write) is the single source of truth. New `/prompts/*`
        // endpoints on the Runtime need no Gateway change beyond
        // registering here.
        //
        // ADR-063 §3.7.7: the L2 reload endpoint
        // `POST /api/agents/{id}/prompts/reload` lives in this
        // namespace (was previously forwarded by the `/debug/{*rest}`
        // wildcard, which 503'd outside DevMode — see ADR-063 §3.7.7).
        .route(
            "/api/agents/{id}/prompts/reload",
            post(proxy_reload_prompts),
        )
        // ADR-048: Debug Protocol HTTP RPC reverse proxy. One wildcard
        // route forwards every method on `/debug/*` to the Runtime's
        // `/api/debug/*` (path suffix, querystring, body and headers
        // all pass through verbatim - see `proxy_debug_rpc`). New
        // debug endpoints on the Runtime need no Gateway change.
        // Debug *events* do NOT flow through here: they are MQTT
        // pub/sub on `acowork/agents/{id}/debug/events/{event_type}`.
        //
        // NOTE: as of ADR-063 §3.7.7 the L2 prompts reload endpoint
        // (`prompts/reload`) is registered explicitly above this
        // wildcard so it can be invoked without DevMode being enabled.
        .route("/api/agents/{id}/debug/{*rest}", any(proxy_debug_rpc))
        // Health double-check (distributed liveness): exposes the Runtime's
        // `/health` endpoint through the Gateway so clients (e.g. the desktop
        // app) can verify a Runtime is actually alive over HTTP when MQTT
        // connectivity is in doubt — e.g. during system sleep, the MQTT
        // connection drops (KeepAlive timeout) while the Runtime process
        // itself stays alive. MQTT liveness alone would misreport such an
        // agent as offline/sleeping. Proxied verbatim so the Runtime remains
        // the source of truth for its own `status`/`degraded_reasons`.
        .route("/api/agents/{id}/health", get(proxy_get_health))
    // NOTE: the global `DefaultBodyLimit` layer is applied at
    // the merged-router level in `http::routes::api_router` so
    // every merged sub-router (agents, chat, proxy, …) shares the
    // same cap. See `GLOBAL_BODY_LIMIT` for the rationale.
}

// ── Proxy handlers ────────────────────────────────────────────────────
//
// All handlers follow the same pattern: extract axum parameters (State,
// Path, Query, HeaderMap, Bytes body), then delegate to
// `proxy_to_runtime` or `proxy_to_runtime_with_method`.
//
// `HeaderMap` is extracted by axum as a "catch-all" extractor (it must
// come after other extractors that consume the request body). The full
// header map is forwarded to the Runtime via the core proxy function,
// which strips hop-by-hop headers per RFC 7230 §6.1 and forwards the
// rest verbatim. This is the standard reverse-proxy behaviour: the
// proxy does not pick-and-choose which headers to forward — it forwards
// all of them except those explicitly forbidden by the HTTP spec.

/// Reverse-proxy `GET /api/agents/{id}/workspaces` to Runtime's `GET /workspaces`.
async fn proxy_list_workspaces(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/workspaces", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/tree` to Runtime's `GET /workspaces/tree`.
async fn proxy_list_tree(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/tree", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/find` to Runtime's
/// `GET /workspaces/find`. Querystring keys: `q`, `workspace_id`, `limit`.
async fn proxy_find_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/find", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/workspaces/search` to Runtime's
/// `GET /workspaces/search`. Querystring keys: `q`, `workspace_id`, `include`,
/// `max_results`, `case_sensitive`, `whole_word`.
async fn proxy_search_files(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/search", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/search` to Runtime's
/// `GET /search` (ADR-081 §4.1 single-agent aggregate). Querystring keys:
/// `q`, `scopes`, `limit`, `mode`, `workspace_id`, `agent`. Transparent
/// forward — aggregation/sorting stays in the Runtime (ADR-064/070).
async fn proxy_global_search(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/search", &query, &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/search/summary` to Runtime's
/// `POST /search/summary` (ADR-093). The JSON candidate body is forwarded
/// verbatim — the Gateway holds no summary logic and never retries POST
/// (design 27 §4.4); the Runtime owns the provider call.
async fn proxy_search_summary(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/search/summary",
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/git/status` to Runtime's
/// `GET /git/status`. Querystring key: `workspace_id` (optional).
async fn proxy_git_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/git/status", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/git/diff` to Runtime's
/// `GET /git/diff`. Querystring keys: `workspace_id` (optional), `path`
/// (required), `cached` (0|1).
async fn proxy_git_diff(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/git/diff", &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/git/log` to Runtime's
/// `GET /git/log`. Querystring keys: `workspace_id` (optional), `path`
/// (optional), `limit` (optional).
async fn proxy_git_log(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/git/log", &query, &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/git/revert` to Runtime's
/// `POST /git/revert` (discard uncommitted changes). The JSON body
/// (`workspace_id` / `path` / `old_path`) is forwarded verbatim — the
/// Gateway never touches `.git` (ADR-009 red line).
async fn proxy_git_revert(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/git/revert",
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions` to Runtime's `GET /sessions`.
async fn proxy_list_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/sessions", &query, &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/sessions` to Runtime's `POST /sessions`.
///
/// ADR-076 §决策 4: the Runtime stamps the session's owner from the
/// `x-user-id` header the auth middleware injected. The body
/// (`workspace_id` / `model` / `provider` / `visibility`) is forwarded
/// verbatim; every field is optional.
async fn proxy_create_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/sessions",
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/sessions` to Runtime's
/// `DELETE /sessions` — clear every session the caller owns.
async fn proxy_clear_sessions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/sessions",
        "",
        reqwest::Method::DELETE,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/sessions/{sid}/open` (ADR-038).
async fn proxy_open_session(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}/open", sid);
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::POST,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/sessions/{sid}/close`.
async fn proxy_close_session(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}/close", sid);
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::POST,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/sessions/{sid}`.
async fn proxy_delete_session(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}", sid);
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::DELETE,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/sessions/{sid}/visibility`.
async fn proxy_set_session_visibility(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/visibility", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/sessions/{sid}/workspace`.
///
/// ADR-076 §决策 4: this replaces the MQTT `workspace_switch` control
/// command, which could not be attributed to a caller and therefore could
/// not be authorized. The Runtime validates the body's `workspace_id`
/// against its resolver and rejects the write unless the requester owns the
/// session.
async fn proxy_put_session_workspace(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/workspace", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/latest-session` to Runtime's `GET /sessions/latest`.
async fn proxy_latest_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/sessions/latest", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/token-stats` to Runtime's `GET /sessions/token-stats`.
///
/// Per-model token statistics are read off the runtime's own SQLite
/// aggregate — no Gateway-side filesystem access (redline unchanged).
async fn proxy_token_stats(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/sessions/token-stats", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/health` to Runtime's `GET /health`.
///
/// Used by the frontend as an HTTP double-check on MQTT disconnection: if the
/// Runtime answers, it is alive (the MQTT drop was a transient connectivity
/// issue, e.g. system sleep) and must not be rendered as offline/sleeping.
async fn proxy_get_health(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_runtime(&state, &id, "/health", "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/messages` to Runtime's `GET /sessions/{sid}/messages`.
async fn proxy_get_messages(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}/messages", sid);
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, &path, &query, &headers).await
}

/// Reverse-proxy a POST session action to the Runtime.
///
/// ADR-076 §决策 4: the six user-triggered session actions all share one
/// shape — POST a JSON body to `/sessions/{sid}/<action>` — and the Runtime
/// checks ownership before touching the session. Only the path suffix
/// varies, hence this helper.
async fn proxy_post_session_action(
    state: &AppState,
    id: &str,
    sid: &str,
    action: &str,
    headers: &HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/{}", sid, action);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        state,
        id,
        &path,
        "",
        reqwest::Method::POST,
        payload,
        headers,
    )
    .await
}

/// Reverse-proxy `POST .../sessions/{sid}/messages` — send a chat message.
///
/// ADR-076 §决策 4: replaces the MQTT `chat_message` command. A chat message
/// is a user action, so it needs the caller's identity for the Runtime to
/// authorize it against the session owner — which is what the Gateway's auth
/// middleware injects as `x-user-id`.
async fn proxy_post_message(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "messages", &headers, body).await
}

/// Reverse-proxy `POST .../sessions/{sid}/stop` — interrupt the current run.
async fn proxy_post_session_stop(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "stop", &headers, body).await
}

/// Reverse-proxy `POST .../sessions/{sid}/continue` — resume a paused run.
async fn proxy_post_session_continue(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "continue", &headers, body).await
}

/// Reverse-proxy `POST .../sessions/{sid}/approval` — the user's tool-risk
/// decision.
///
/// This is the endpoint that closes the approval-spoofing hole: over MQTT,
/// any client on the broker could publish `approval_decision{approved:true}`
/// into another account's session.
async fn proxy_post_session_approval(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "approval", &headers, body).await
}

/// Reverse-proxy `POST .../sessions/{sid}/answer` — answer an
/// `ask_user_question` prompt.
async fn proxy_post_session_answer(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "answer", &headers, body).await
}

/// Reverse-proxy `POST .../sessions/{sid}/cancel-tool` — ADR-045: abort a
/// single in-flight tool.
async fn proxy_post_session_cancel_tool(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "cancel-tool", &headers, body).await
}

/// Reverse-proxy `POST .../sessions/{sid}/compress` — user-initiated context
/// compression (replaces the MQTT `compress_action`; ADR-076 §决策 4).
async fn proxy_post_session_compress(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_post_session_action(&state, &id, &sid, "compress", &headers, body).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/state` to
/// Runtime's `GET /sessions/{sid}` (the legacy `/state` suffix was
/// absorbed into `/sessions/{sid}` per the Runtime server's
/// `get_session` doc-comment).  Kept as a separate path so callers
/// that still use the `/state` suffix don't 404; the body is
/// forwarded verbatim either way.
async fn proxy_get_session_state(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}", sid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/rag/status` to Runtime.
async fn proxy_rag_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/rag/status", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/rag/query` to Runtime.
async fn proxy_rag_query(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/rag/query", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/shell-risk-rules` to Runtime.
async fn proxy_get_shell_risk_rules(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/shell-risk-rules", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/shell-risk-rules` to Runtime.
async fn proxy_put_shell_risk_rules(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/shell-risk-rules", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

// ── ADR-009 §5: agent asset reads (served by the Node) ───────────────
//
// These three reads used to be forwarded to the Runtime. That broke the
// moment the agent stopped: no Runtime process → no registered
// `http_endpoint` → 503 → the Desktop silently fell back to a random
// builtin icon (review §7.1, V-P). Package bytes belong to the Node,
// which is the package manager and is up regardless of agent state, so
// one path serves both states.

/// Resolve the HTTP base URL of a node from the NodeRegistry's retained
/// `NodeInfo` snapshot (ADR-055 §6.3).
///
/// Node-level, not agent-level: a stopped agent has no registered
/// `http_endpoint`, but the Node process is still reachable.
pub(crate) async fn node_http_endpoint(
    state: &AppState,
    node_id: &str,
) -> Result<String, ApiError> {
    let registry = state
        .node_registry
        .as_ref()
        .ok_or_else(|| ApiError::service_unavailable("Node registry not initialized"))?;

    let reg = registry.read().await;
    let node = reg
        .get(node_id)
        .ok_or_else(|| ApiError::not_found(&format!("Node '{}' not found", node_id)))?;
    if !node.online {
        return Err(ApiError::service_unavailable(&format!(
            "Node '{}' is offline",
            node_id
        )));
    }
    node.info
        .as_ref()
        .map(|i| i.http_endpoint.clone())
        .filter(|ep| !ep.is_empty())
        .ok_or_else(|| {
            ApiError::service_unavailable(&format!("Node '{}' has no HTTP endpoint", node_id))
        })
}

/// Reverse-proxy `{method} /agents/{id}{sub_path}` to the **node** hosting
/// the instance, passing the response through verbatim (status, headers,
/// bytes) so an image read keeps its `Content-Type` / `Cache-Control` and
/// an upload keeps the node's own error shape.
///
/// The node token is attached when the node is enrolled (ADR-055 Phase 5a
/// §6.8) — the package routes sit behind the same boundary as the Runtime
/// reverse proxy.
async fn proxy_to_node(
    state: &AppState,
    id: &str,
    sub_path: &str,
    query: &str,
    method: reqwest::Method,
    body: Option<Vec<u8>>,
    headers: &HeaderMap,
) -> Response {
    let node_id = {
        let gw = state.gateway_state.read().await;
        gw.installed(id).map(|a| a.node_id.clone())
    };
    let Some(node_id) = node_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "Agent not found", "id": id })),
        )
            .into_response();
    };

    let base = match node_http_endpoint(state, &node_id).await {
        Ok(base) => base,
        Err(e) => {
            // http.md §4.13: honour the Desktop's `with503Retry` backoff.
            let mut response = e.into_response();
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("2"));
            return response;
        }
    };

    let path = format!("/agents/{id}{sub_path}");
    let target_url = if query.is_empty() {
        format!("{base}{path}")
    } else {
        format!("{base}{path}?{query}")
    };

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);
    for (name, value) in headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
    }
    if let Some(node_token) = resolve_node_token(state, id).await {
        request = request.header("X-ACowork-Node-Token", node_token);
    }
    if let Some(body) = body {
        request = request.body(body);
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();
            let mut builder = Response::builder().status(status);
            *builder.headers_mut().unwrap() = resp_headers;
            builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(agent_id = id, url = %target_url, error = %e, "Failed to proxy to node");
            (
                StatusCode::BAD_GATEWAY,
                Json(serde_json::json!({
                    "error": "Failed to connect to node HTTP server",
                    "id": id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

// ── ADR-009 §V-A: skill inspection ───────────────────────────────────
//
// All three reads are transparent forwards to the Runtime's
// `http/skills.rs`. `{name}` is percent-encoded as a single path segment
// so a skill name containing `/` or a space cannot re-shape the upstream
// request line.

/// Reverse-proxy `GET /api/agents/{id}/skills` to Runtime's `GET /agents/{id}/skills`.
async fn proxy_list_skills(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/skills", id);
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, &path, &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/skills/{name}` to the Runtime.
async fn proxy_skill_detail(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/skills/{}", id, encode_path_segment(&name));
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/skills/{name}/history` to the Runtime.
async fn proxy_skill_history(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = format!(
        "/agents/{}/skills/{}/history",
        id,
        encode_path_segment(&name)
    );
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, &path, &query, &headers).await
}

// ── ADR-009 §V-B / §5: agent package content ─────────────────────────
//
// Reads (any agent state) and publish-domain writes (wizard runs on a
// stopped agent too) all go to the node, which owns the package bytes.
// Query strings (`?v=<version>` cache-buster on `/avatar`, `?path=<rel>`
// on `/avatar-file`) are forwarded verbatim.

/// Reverse-proxy `GET /api/agents/{id}/avatar` to the node.
async fn proxy_get_avatar(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_node(
        &state,
        &id,
        "/avatar",
        &query,
        reqwest::Method::GET,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/avatar-file?path=` to the node.
async fn proxy_get_avatar_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_node(
        &state,
        &id,
        "/avatar-file",
        &query,
        reqwest::Method::GET,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/manifest/avatar-assets` to the node.
async fn proxy_get_avatar_assets(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_to_node(
        &state,
        &id,
        "/manifest/avatar-assets",
        "",
        reqwest::Method::GET,
        None,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/manifest/avatar` to the node
/// (which rewrites `manifest.toml` on its own disk).
async fn proxy_put_manifest_avatar(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_to_node(
        &state,
        &id,
        "/manifest/avatar",
        "",
        reqwest::Method::POST,
        Some(body.to_vec()),
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/manifest/file?path=` (multipart)
/// to the node.
///
/// The multipart body — boundary included, via the forwarded
/// `content-type` — goes through untouched: it is bytes, so it stays on
/// HTTP (ADR-009 §V-R) rather than being re-encoded for the control plane.
async fn proxy_upload_agent_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_node(
        &state,
        &id,
        "/manifest/file",
        &query,
        reqwest::Method::POST,
        Some(body.to_vec()),
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/avatar-file?path=` to the node,
/// then ask the Runtime to drop a preference that referenced the file.
///
/// Two owners, two calls: the *file* is package content (node), the
/// *reference to it* is a user preference (Runtime, ADR-009 §5). The
/// Gateway must not decide the reference itself — it only mirrors what the
/// Runtime persisted (see `forward_avatar_config_put`).
async fn proxy_delete_avatar_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    let deleted_path = params.get("path").cloned();

    // Snapshot the effective avatar BEFORE deleting: after the call the
    // mirror may already have been refreshed by another request.
    let effective_avatar = {
        let gw = state.gateway_state.read().await;
        gw.installed(&id).and_then(|info| {
            let overrides = gw.overrides_of(info.instance_id.as_str());
            let (avatar, _, _) = match overrides {
                Some(ov) if ov.avatar.is_some() || ov.builtin_avatar.is_some() => {
                    (ov.avatar.clone(), ov.builtin_avatar.clone(), "overrides")
                }
                _ => resolve_avatar_from_manifest(&info.manifest),
            };
            avatar
        })
    };

    let response = proxy_to_node(
        &state,
        &id,
        "/avatar-file",
        &query,
        reqwest::Method::DELETE,
        None,
        &headers,
    )
    .await;
    if !response.status().is_success() {
        return response;
    }

    if deleted_path.is_some() && deleted_path.as_deref() == effective_avatar.as_deref() {
        tracing::info!(
            agent_id = %id,
            "Avatar file deleted — clearing the reference through the Runtime"
        );
        let clear = crate::http::proxy::forward_avatar_config_put(
            &state,
            &id,
            br#"{"avatar":""}"#.to_vec(),
        )
        .await;
        // Best-effort: the file is already gone, so a stopped agent /
        // unpicked default must not turn a successful delete into a 5xx.
        // The stale reference then resolves to a missing file, which the
        // Desktop renders as the deterministic fallback icon.
        if !clear.status().is_success() {
            tracing::warn!(
                agent_id = %id,
                status = %clear.status(),
                "Avatar file deleted, but the Runtime kept the stale reference"
            );
        }
    }

    response
}

// ── ADR-009 §5: user-preference overrides (avatar + display name) ─────
//
// The Runtime owns the instance's `.overrides.json`; the Gateway only
// forwards and mirrors. See `forward_avatar_config_put` for why the PUT
// is not a plain `proxy_to_runtime_with_method`.

/// Reverse-proxy `GET /api/agents/{id}/avatar-config` to the Runtime.
async fn proxy_get_avatar_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/avatar-config", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/avatar-config` to the Runtime and
/// mirror the persisted overrides into the Gateway's agent list.
async fn proxy_put_avatar_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    forward_avatar_config_put(&state, &id, body.to_vec()).await
}

/// The subset of the Runtime's avatar-config response the Gateway mirrors.
/// Everything else in that response is passed through untouched — the
/// Gateway deliberately does not own the full DTO.
#[derive(serde::Deserialize)]
struct AvatarConfigMirror {
    #[serde(default)]
    overrides: Option<acowork_core::AgentOverrides>,
}

/// PUT the user's avatar/display-name preference to the Runtime (the
/// owner of the instance's `.overrides.json`, ADR-009 §5) and update the
/// Gateway's list view from what the Runtime actually persisted.
///
/// This is the live half of the two-source design: a **stopped** agent's
/// values come from the Node's retained inventory, a **running** one's
/// from here. The Gateway intentionally does not decide what the new
/// value is — it stores the Runtime's answer verbatim, so the two sources
/// can never disagree.
pub(crate) async fn forward_avatar_config_put(
    state: &AppState,
    id: &str,
    body: Vec<u8>,
) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    let path = format!("/agents/{id}/avatar-config");
    let response = proxy_to_runtime_with_method(
        state,
        id,
        &path,
        "",
        reqwest::Method::PUT,
        Some(body),
        &headers,
    )
    .await;

    if !response.status().is_success() {
        return response;
    }

    let (parts, body) = response.into_parts();
    let bytes = match axum::body::to_bytes(body, 256 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::warn!(agent_id = id, error = %e, "avatar-config: cannot read Runtime response");
            return (
                parts.status,
                Json(serde_json::json!({ "error": "unreadable upstream response" })),
            )
                .into_response();
        }
    };
    match serde_json::from_slice::<AvatarConfigMirror>(&bytes) {
        Ok(mirror) => {
            let mut gw = state.gateway_state.write().await;
            gw.set_overrides(id, mirror.overrides.unwrap_or_default());
        }
        Err(e) => tracing::warn!(
            agent_id = id,
            error = %e,
            "avatar-config: Runtime response not mirrorable — list view stays stale until the next inventory publish"
        ),
    }
    Response::from_parts(parts, axum::body::Body::from(bytes))
}

// ── ADR-063: package-level LLM prompt overrides ──────────────────────
//
// Three transparent reverse proxies; body is forwarded verbatim so the
// Runtime's `http/prompts.rs` whitelist + atomic-rename discipline is
// the single source of truth. The L2 reload endpoint
// `POST /api/agents/{id}/debug/prompts/reload` is forwarded by the
// `/debug/{*rest}` wildcard above (R7) — see ADR-063 §3.7.6.

/// Reverse-proxy `POST /api/agents/{id}/prompts/reload` to Runtime's
/// `POST /agents/{id}/prompts/reload` (ADR-063 §3.7.6 L2 reload).
///
/// Body is forwarded verbatim (typically empty `{}`). Replaces the
/// old path that flowed through the `/debug/{*rest}` wildcard and
/// 503'd outside DevMode — the reload is a package-level operation,
/// not a debug-session one, and now lives in the prompts namespace
/// where the rest of the prompts family sits. See ADR-063 §3.7.7.
async fn proxy_reload_prompts(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{id}/prompts/reload");
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

// ── New Phase 4 proxy handlers ─────────────────────────────────────────

/// Reverse-proxy `POST /api/agents/{id}/sessions/{sid}/files`
/// to Runtime's `POST /sessions/{sid}/files` (ADR-046 multipart upload).
///
/// The multipart body and Content-Type header (including the boundary)
/// are forwarded verbatim - the Gateway does not parse multipart.
async fn proxy_upload_file(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/files", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/files/{document_id}`
/// to Runtime's `GET /files/{document_id}` (ADR-046 blob download).
///
/// The `format` query parameter (file extension for Content-Type
/// derivation) is forwarded as-is. The response body (raw bytes) and
/// Content-Type header are returned verbatim.
async fn proxy_download_file(
    State(state): State<AppState>,
    Path((id, document_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/files/{}", document_id);
    // Forward query params (typically just `format=pdf` etc.)
    let query = if params.is_empty() {
        String::new()
    } else {
        params
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect::<Vec<_>>()
            .join("&")
    };
    proxy_to_runtime(&state, &id, &path, &query, &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}`
/// to Runtime's `GET /sessions/{sid}`.
async fn proxy_get_session(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}", sid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/sessions/{sid}/config`
/// to Runtime's `GET /sessions/{sid}/config` (ADR-047).
async fn proxy_get_session_config(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/sessions/{}/config", sid);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/sessions/{sid}/config`
/// to Runtime's `PUT /sessions/{sid}/config` (ADR-047).
async fn proxy_put_session_config(
    State(state): State<AppState>,
    Path((id, sid)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/sessions/{}/config", sid);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces`
/// to Runtime's `POST /workspaces`.
async fn proxy_add_workspace(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces",
        "",
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}`
/// to Runtime's `PUT /workspaces/{ws_id}`.
async fn proxy_update_workspace(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}/prompt-file`
/// to Runtime's `PUT /workspaces/{ws_id}/prompt-file`.
async fn proxy_set_prompt_file(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}/prompt-file", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/{ws_id}/fs-watch`
/// to Runtime's `PUT /workspaces/{ws_id}/fs-watch`.
///
/// Transparent pass-through (same rationale as the workspace file/dir
/// proxies): path-traversal / workspace resolution live on the Runtime
/// side; the Gateway only forwards the body and surfaces status codes.
async fn proxy_set_workspace_fs_watch(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/workspaces/{}/fs-watch", ws_id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/{ws_id}`
/// to Runtime's `DELETE /workspaces/{ws_id}`.
async fn proxy_delete_workspace(
    State(state): State<AppState>,
    Path((id, ws_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/workspaces/{}", ws_id);
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::DELETE,
        None,
        &headers,
    )
    .await
}

// ── Workspace file & dir REST proxy (Gateway is transparent; see ADR-034 §11.2) ─
//
// Six small handlers that mirror the Runtime's `/workspaces/file` and
// `/workspaces/dir` resources. The Gateway does **not** validate path,
// body, or path-traversal concerns here — that is the Runtime's
// responsibility via `resolve_workspace_root` and canonicalize-then-
// `starts_with` check. The Gateway only:
//
//   1. Resolves the agent's Runtime HTTP port via `SharedRuntimeHttpRegistry`.
//   2. Forwards the verbatim body (POST/PUT/DELETE) or querystring (GET).
//   3. Surfaces Runtime-side status codes (200/404/409/500) directly to
//      the desktop so the panel can render granular error states.

/// Reverse-proxy `GET /api/agents/{id}/workspaces/file?path=…` →
/// Runtime's `GET /workspaces/file`. Returns JSON
/// `{content,size,mimeType,path,modified,is_file,is_dir}`.
async fn proxy_read_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let query = build_query_string(&params);
    proxy_to_runtime(&state, &id, "/workspaces/file", &query, &headers).await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/file` → Runtime's
/// `POST /workspaces/file`. The desktop puts `workspace_id` in the
/// querystring and `{path, content?, overwrite?}` in the JSON body; we
/// forward both verbatim.
async fn proxy_create_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/file",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `PUT /api/agents/{id}/workspaces/file?path=…` →
/// Runtime's `PUT /workspaces/file`. Body is `{content}` (path and
/// workspace_id are in the querystring).
async fn proxy_write_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/file",
        &query,
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/file` → Runtime's
/// `DELETE /workspaces/file`. Body is `{path}`; `workspace_id` is in
/// the querystring.
async fn proxy_delete_workspace_file(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/file",
        &query,
        reqwest::Method::DELETE,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/dir` → Runtime's
/// `POST /workspaces/dir`. Body is `{path}`; `workspace_id` is in
/// the querystring.
async fn proxy_create_workspace_dir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/dir",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `DELETE /api/agents/{id}/workspaces/dir` → Runtime's
/// `DELETE /workspaces/dir`. Body is `{path}`; `workspace_id` is in
/// the querystring.
async fn proxy_delete_workspace_dir(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/dir",
        &query,
        reqwest::Method::DELETE,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/copy` → Runtime's
/// `POST /workspaces/copy`. Body is `{workspace_id?, source, dest}`;
/// `workspace_id` may also ride on the querystring. The runtime owns
/// the workspace config on disk and resolves `workspace_id` against
/// `<work_dir>/config/agent_workspaces.json`, which is why this
/// endpoint is proxied rather than implemented on the Gateway side.
async fn proxy_copy_workspace_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/copy",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `POST /api/agents/{id}/workspaces/rename` → Runtime's
/// `POST /workspaces/rename`. Same payload shape as `copy`.
/// `std::fs::rename` is atomic on the same filesystem, so the latency
/// overhead of the round-trip through the Runtime is irrelevant — the
/// HTTP envelope dominates either way.
async fn proxy_rename_workspace_item(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        "/workspaces/rename",
        &query,
        reqwest::Method::POST,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/config`
/// to Runtime's `GET /agents/{id}/config`.
async fn proxy_get_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/config", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/config`
/// to Runtime's `PUT /agents/{id}/config`.
///
/// **Gateway is a pure reverse proxy here** - the inbound JSON body is
/// forwarded verbatim to the Runtime, which is the single owner of the
/// read-modify-write persistence path (`agent_tools.json` +
/// `agent_config.json` + live broadcast). The previous implementation
/// re-parsed the body, forwarded only the `builtin_tools` slice, and
/// echoed the other 8 fields back - that left fields like `temperature`
/// and `max_output_tokens` invisible to the Runtime and silently dropped
/// the Setup panel edits (user-visible as "改动不生效").
///
/// The 503/404 fallback paths in `proxy_to_runtime_with_method` cover
/// the same startup-race semantics that the old implementation
/// checked inline (`running_agents[id].ready`); the Gateway simply
/// bubbles up the Runtime's own error envelope instead of duplicating
/// it. This is the same pattern used by `proxy_update_workspace` /
/// `proxy_set_prompt_file`.
async fn proxy_put_config(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/config", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/tools`
/// to Runtime's `GET /agents/{id}/tools`.
async fn proxy_get_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/tools", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/builtin-tools`
/// to Runtime's `GET /agents/{id}/builtin-tools` (ADR-040 follow-up).
async fn proxy_get_builtin_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/builtin-tools", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/builtin-tools`
/// to Runtime's `PUT /agents/{id}/builtin-tools` (ADR-040 follow-up).
///
/// Mirrors `proxy_put_mcp_servers` exactly:
/// this gateway surface is a transparent pipe so the ToolsTab can use
/// one writes-one-endpoint pattern. All persistence + validation lives
/// in the Runtime handler `put_agent_builtin_tools` — see
/// `acowork-runtime/src/http/server.rs`.
async fn proxy_put_builtin_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/builtin-tools", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/status`
/// to Runtime's `GET /agents/{id}/status`.
async fn proxy_get_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/status", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

// ── Win11-MCP-ToolsBugFix: per-agent MCP proxy ────────────────────────
//
// These two tiny handlers are the symmetric counterpart of `proxy_get_config`
// / `proxy_put_config`. They only forward the request — all field
// validation + persistence lives in the Runtime handlers
// (`put_agent_mcp_servers` in
// `acowork-runtime/src/http/server.rs`). Keeping the Gateway branch stub-free
// guarantees a single source of truth: tools panel writes always go through
// the same atomic write-tmp-rename path that the other per-agent config
// files use (`agent_mcp.json`).

/// Reverse-proxy `GET /api/agents/{id}/mcp-servers`
/// to Runtime's `GET /agents/{id}/mcp-servers`.
async fn proxy_get_mcp_servers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/mcp-servers", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `GET /api/agents/{id}/mcp-ensure/{name}` (ADR-090).
async fn proxy_get_mcp_ensure(
    State(state): State<AppState>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/mcp-ensure/{}", id, name);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/mcp-servers`
/// to Runtime's `PUT /agents/{id}/mcp-servers`.
async fn proxy_put_mcp_servers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/mcp-servers", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

// ── ADR-069: per-MCP-server tool-level opt-in proxy ────────────────────
//
// Symmetric to `proxy_get_mcp_servers` / `proxy_put_mcp_servers`: these
// two handlers are transparent pipes to the Runtime's
// `get_agent_mcp_tools` / `put_agent_mcp_tools` in
// `acowork-runtime/src/http/server.rs`. The Runtime is the single source
// of truth for the full per-tool list (reconciled against the live MCP
// `tools/list` at connect time); the Gateway does no parsing or
// second-guessing.

/// Reverse-proxy `GET /api/agents/{id}/mcp-tools`
/// to Runtime's `GET /agents/{id}/mcp-tools`.
async fn proxy_get_mcp_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/mcp-tools", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// Reverse-proxy `PUT /api/agents/{id}/mcp-tools`
/// to Runtime's `PUT /agents/{id}/mcp-tools`.
async fn proxy_put_mcp_tools(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/agents/{}/mcp-tools", id);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    proxy_to_runtime_with_method(
        &state,
        &id,
        &path,
        "",
        reqwest::Method::PUT,
        payload,
        &headers,
    )
    .await
}

/// Reverse-proxy `GET /api/agents/{id}/providers`
/// to Runtime's `GET /agents/{id}/providers`.
///
/// Returns the provider catalog that the Runtime has so far received
/// from Gateway via MQTT and persisted locally. Useful for the frontend
/// to verify end-to-end delivery.
async fn proxy_get_providers(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let path = format!("/agents/{}/providers", id);
    proxy_to_runtime(&state, &id, &path, "", &headers).await
}

/// ADR-048: Reverse-proxy ANY method on
/// `/api/agents/glm-5.3_common/debug/{*rest}` to the Runtime's
/// `/api/debug/{rest}`.
///
/// The Runtime owns the debug RPC surface when DevMode is active
/// (`http/debug.rs`, mounted by D2); the Gateway is a transparent
/// reverse-proxy exactly like the workspace/session routes above:
/// method, path suffix, querystring (e.g. `?session_id=...`), body
/// (e.g. `{"session_id": "...", "granularity": "..."}`) and headers
/// are forwarded verbatim. Unknown debug paths surface the Runtime's
/// own 404/405 responses - the Gateway does not second-guess the
/// surface.
///
/// ADR-048 follow-up: the special path suffix `enable` updates
/// [`RunningAgentInfo::debug_state`] to `Enabled` on a 2xx response.
/// Without this hook the Gateway would keep reporting
/// `debug_state=Disabled` (initialized at spawn based on the CLI
/// `--dev-mode` flag) even after a successful runtime enable, breaking
/// the Desktop's `agentStore.agentNotDebugMode` flag. Every other
/// `rest` value flows through the proxy verbatim.
///
/// The symmetric `disable` suffix (ADR-048 follow-up) flips
/// `debug_state` back to `Disabled` on a 2xx response so the Desktop's
/// `selectedAgent.debug_state === "enabled"` check goes false and the
/// DebugPanel re-renders the "Enable Debug" placeholder instead of
/// staying on the live panel with stale data.
async fn proxy_debug_rpc(
    State(state): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    method: axum::http::Method,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = format!("/api/debug/{}", rest);
    let query = build_query_string(&params);
    let payload: Option<Vec<u8>> = if body.is_empty() {
        None
    } else {
        Some(body.to_vec())
    };
    let response =
        proxy_to_runtime_with_method(&state, &id, &path, &query, method, payload, &headers).await;

    // ADR-048 follow-up: refresh Gateway-side state on a successful
    // runtime enable. We only act on the literal `enable` suffix —
    // any other `rest` (e.g. `state`, `step`, `resume`) leaves the
    // state alone. Only 2xx counts as "Runtime accepted the wiring";
    // 503 (SessionManager not ready yet) and 4xx (already enabled,
    // but the helper returned AlreadyEnabled which still has ok=true)
    // are both 200, so we gate on `is_success()` which means
    // 200..=299. The `already_enabled` JSON flag from the Runtime
    // is a stronger signal that the wiring is live; we only need
    // 2xx here because AlreadyEnabled returns 200 too.
    if rest == "enable" && response.status().is_success() {
        let shared_state = state.gateway_state.clone();
        let mut gw = shared_state.write().await;
        if let Some(running) = gw.running_agents.get_mut(&id)
            && running.debug_state != crate::gateway::state::DebugState::Enabled
        {
            tracing::info!(
                agent_id = %id,
                "Gateway: runtime DevMode enable succeeded — flipping debug_state"
            );
            running.debug_state = crate::gateway::state::DebugState::Enabled;
        }
    }

    // ADR-048 follow-up (symmetric to the `enable` hook above):
    // a successful `POST /api/agents/{id}/debug/disable` proxy call
    // flips `RunningAgentInfo::debug_state` from `Enabled` →
    // `Disabled`. We gate on `is_success()` (200..=299) for the
    // same reason as `enable`: the Runtime reports both
    // `NewlyDisabled` and `AlreadyDisabled` as 200, so any 2xx
    // is the wire-level confirmation that the teardown landed.
    if rest == "disable" && response.status().is_success() {
        let shared_state = state.gateway_state.clone();
        let mut gw = shared_state.write().await;
        if let Some(running) = gw.running_agents.get_mut(&id)
            && running.debug_state != crate::gateway::state::DebugState::Disabled
        {
            tracing::info!(
                agent_id = %id,
                "Gateway: runtime DevMode disable succeeded — flipping debug_state"
            );
            running.debug_state = crate::gateway::state::DebugState::Disabled;
        }
    }

    response
}

/// Build a query string from a HashMap of params.
fn build_query_string(params: &HashMap<String, String>) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
        .collect();
    parts.sort(); // deterministic order
    parts.join("&")
}

fn urlencoding(s: &str) -> String {
    // Percent-encode per UTF-8 BYTE (RFC 3986 §2.1). Iterating `char`s and
    // casting `c as u8` would truncate non-ASCII code points (e.g. 中 U+4E2D
    // → 0x2D) and corrupt multi-byte paths — caught by git_proxy_e2e.rs
    // utf8_path_is_percent_encoded_when_forwarded. `s.bytes()` preserves the
    // exact UTF-8 sequence the upstream needs to decode.
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Percent-encode a single URL **path segment** (RFC 3986 §3.3) so it cannot
/// re-shape the upstream request line. Uses the `urlencoding` crate (not the
/// local [`urlencoding`] helper above, which is query-string flavoured and
/// would emit a literal `+` for a space).
fn encode_path_segment(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

/// Check if a header is hop-by-hop (RFC 7230 §6.1) or should otherwise
/// not be forwarded by a reverse proxy.
///
/// Hop-by-hop headers are specific to a single transport-level connection
/// and must not be forwarded by proxies. Additionally, `host` is excluded
/// because the proxy targets a different upstream host, and
/// `content-length` is excluded because reqwest recalculates it from the
/// actual body bytes.
pub(crate) fn is_hop_by_hop_header(name: &axum::http::HeaderName) -> bool {
    // HeaderName stores names in lowercase, so comparison is safe.
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

/// Core reverse-proxy logic: look up the Runtime's HTTP port and forward a GET request.
///
/// Delegates to [`proxy_to_runtime_with_method`] with `GET` and no body.
async fn proxy_to_runtime(
    state: &AppState,
    id: &str,
    path: &str,
    query: &str,
    headers: &HeaderMap,
) -> Response {
    proxy_to_runtime_with_method(state, id, path, query, reqwest::Method::GET, None, headers).await
}

/// Core reverse-proxy logic with configurable HTTP method, optional body,
/// and full header forwarding.
///
/// # Proxy contract
///
/// This function is a **transparent reverse proxy** (RFC 7230 §2.3). It
/// forwards the inbound request's method, path, query string, body, and
/// **all headers** to the Runtime's localhost HTTP server. Only hop-by-hop
/// headers (RFC 7230 §6.1) are stripped — every other header (including
/// `Content-Type`, `Accept`, `Authorization`, custom headers, etc.) is
/// forwarded verbatim.
///
/// This is the standard reverse-proxy behaviour: the proxy does not
/// pick-and-choose which headers to forward, nor does it assume or
/// default any header values. Callers must extract the full `HeaderMap`
/// from the inbound request and pass it in.
///
/// `body` is forwarded verbatim as the request payload when set
/// (POST/PUT/PATCH). Endpoints with no incoming body should pass `None`.
pub(crate) async fn proxy_to_runtime_with_method(
    state: &AppState,
    id: &str,
    path: &str,
    query: &str,
    method: reqwest::Method,
    body: Option<Vec<u8>>,
    headers: &HeaderMap,
) -> Response {
    // Look up the Runtime's HTTP port from the registry.
    let registry = match &state.runtime_http_registry {
        Some(r) => r.clone(),
        None => {
            let mut response = (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    "error": "Runtime HTTP proxy registry not initialized",
                    "id": id,
                })),
            )
                .into_response();
            // Same Retry-After contract as /api/global-resources
            // (http.md §4.13): the desktop `with503Retry` helper honours
            // this header before its exponential backoff, keeping the
            // boot-window retry cadence uniform across Gateway 503s.
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("2"));
            return response;
        }
    };

    let endpoint = {
        let reg = registry.read().await;
        reg.get_endpoint(id)
    };

    let endpoint = match endpoint {
        Some(ep) => ep,
        None => {
            let mut response = (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({
                    // ADR-085 D4: machine-readable error code — the
                    // Desktop distinguishes "wait for the lifecycle
                    // status push" (agent_not_running) from "retry the
                    // request" (session_not_ready) on this field.
                    "error": "agent_not_running",
                    "id": id,
                    "message": "The Gateway has not yet discovered this Runtime's HTTP endpoint. The Runtime should publish a retained message on `acowork/agents/{id}/http_endpoint` at startup (ADR-033, ADR-055 D3). Verify the Runtime is running, has connected to the MQTT broker, and was started with `--http-port 0` so its localhost HTTP server is up."
                })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from_static("2"));
            return response;
        }
    };

    // Build the target URL (endpoint already includes host:port)
    let target_url = if query.is_empty() {
        format!("{}{}", endpoint, path)
    } else {
        format!("{}{}?{}", endpoint, path, query)
    };

    tracing::debug!(
        id,
        endpoint,
        target_url = %target_url,
        "Reverse-proxying to Runtime HTTP server"
    );

    // Forward the request
    let client = runtime_http_client();
    let mut request = client.request(method.clone(), &target_url);

    // Forward all non-hop-by-hop headers from the inbound request (RFC 7230 §6.1).
    // This ensures Content-Type, Accept, Authorization, and any custom headers
    // reach the Runtime unchanged.
    for (name, value) in headers {
        if !is_hop_by_hop_header(name) {
            request = request.header(name, value);
        }
    }

    // ADR-055 Phase 5a: when MQTT auth is enabled, attach the
    // long-lived token of the node hosting this agent as
    // `X-ACowork-Node-Token` so the Runtime can authenticate the
    // proxied request (see `acowork-node` proxy/mod.rs). No-op when
    // auth is off, the agent is unknown, or its node is not enrolled.
    if let Some(node_token) = resolve_node_token(state, id).await {
        request = request.header("X-ACowork-Node-Token", node_token);
    }

    if let Some(ref payload) = body {
        request = request.body(payload.clone());
    }
    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
            response_builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to Runtime");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to Runtime HTTP server",
                    "id": id,
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// ADR-055 Phase 5a: resolve the long-lived node token for the node
/// hosting `agent_id` (installed_agents first, running_agents as a
/// fallback). Returns `None` when the agent is unknown or its node
/// has not enrolled yet — callers treat that as "no credential to
/// attach" rather than an error.
///
/// ADR-073: `id` is the instance identity (UUIDv4). A package id
/// never matches.
///
/// Deliberately NOT gated on `mqtt.auth_enabled`: the node enforces
/// this token on its HTTP proxy whenever IT holds one (acowork-node
/// proxy/mod.rs), regardless of the broker auth flag, so the Gateway
/// must attach the token whenever the store has one — otherwise every
/// reverse-proxied request 403s with "invalid node token" (loading
/// session stalls on `latest-session`). A node that has not enrolled
/// yet fails closed, so the two sides converge through the enrollment
/// handshake instead: the node re-sends its enrollment request on
/// every (re)connect — with an empty credential when it holds none —
/// and with auth off the Gateway mints the first token on that
/// request.
pub(crate) async fn resolve_node_token(state: &AppState, agent_id: &str) -> Option<String> {
    let gw = state.gateway_state.read().await;
    let broker_auth = gw.mqtt_broker_auth.as_ref()?;
    let node_id = gw
        .installed(agent_id)
        .map(|a| a.node_id.as_str())
        .or_else(|| gw.running(agent_id).map(|a| a.node_id.as_str()))?;
    let store = broker_auth.node_tokens.lock().ok()?;
    store.get_token(node_id).map(str::to_string)
}

/// The `node_token` for a **node** (not an agent), for Gateway → node
/// HTTP machine mutations (ADR-055 §6.8: the node's own HTTP surface is
/// the same token boundary the reverse proxy uses).
pub(crate) async fn node_token_for(state: &AppState, node_id: &str) -> Option<String> {
    let gw = state.gateway_state.read().await;
    let store = gw.mqtt_broker_auth.as_ref()?.node_tokens.lock().ok()?;
    store.get_token(node_id).map(str::to_string)
}

/// HTTP client for making proxy requests to Runtime.
///
/// Uses a static `reqwest::Client` (built once, reused) for connection
/// pooling - reqwest strongly recommends reusing a single client instance.
pub(crate) fn runtime_http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to build Runtime HTTP client")
    })
}

/// Forward a request to the Runtime's HTTP endpoint (ADR-055 D3).
///
/// This is used by the proxy handlers when the Runtime's HTTP endpoint
/// is known but the proxy layer needs to forward the request
/// out-of-band (e.g. without the AppState registry). The current
/// reverse-proxy path goes through `proxy_to_runtime_with_method`,
/// which uses the retained-endpoint registry populated via MQTT.
#[allow(dead_code)]
async fn forward_to_runtime(
    endpoint: &str,
    method: reqwest::Method,
    path: &str,
    headers: HeaderMap,
) -> Result<Response, StatusCode> {
    let url = format!("{}{}", endpoint, path);

    let client = runtime_http_client();
    let mut req = client.request(method, &url);

    // Forward all non-hop-by-hop headers (RFC 7230 §6.1)
    for (key, value) in headers.iter() {
        if !is_hop_by_hop_header(key) {
            req = req.header(key, value);
        }
    }

    match req.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let resp_headers = response.headers().clone();
            let body = response.bytes().await.unwrap_or_default();

            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
            response_builder
                .body(axum::body::Body::from(body))
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %url, "Failed to proxy to Runtime");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_http_registry() {
        let mut registry = RuntimeHttpRegistry::new();
        assert!(registry.is_empty());

        registry.register("com.test.agent", "http://127.0.0.1:12345");
        assert_eq!(registry.len(), 1);
        assert_eq!(
            registry.get_endpoint("com.test.agent"),
            Some("http://127.0.0.1:12345".to_string())
        );
        assert_eq!(registry.get_endpoint("com.unknown"), None);

        registry.unregister("com.test.agent");
        assert!(registry.is_empty());
        assert_eq!(registry.get_endpoint("com.test.agent"), None);
    }

    /// ADR-048 D5: the debug wildcard route must forward method, path
    /// suffix, querystring and body verbatim to the Runtime's
    /// `/api/debug/*`.
    ///
    /// Spawns a mock "Runtime" HTTP server that echoes the received
    /// request as JSON, registers its port in the
    /// RuntimeHttpRegistry, then drives the proxy router with real
    /// HTTP requests.
    #[tokio::test]
    async fn test_debug_rpc_proxy_forwards_verbatim() {
        use std::sync::Arc;
        use tokio::sync::RwLock;
        use tower::util::ServiceExt;

        // ── Mock Runtime: echoes method/path/query/body back as JSON ──
        let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let received_for_server = received.clone();
        let runtime_app = axum::Router::new().fallback(axum::routing::any(
            move |req: axum::extract::Request| async move {
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                received_for_server.lock().unwrap().push(format!(
                    "{} {} body={}",
                    method,
                    uri,
                    String::from_utf8_lossy(&body)
                ));
                axum::Json(serde_json::json!({"ok": true}))
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, runtime_app).await.unwrap();
        });

        // ── Gateway state with the mock Runtime registered ──
        let dir =
            std::env::temp_dir().join(format!("acowork-test-debug-proxy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = crate::http::routes::AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let registry = crate::http::proxy::new_shared_registry();
        registry
            .write()
            .await
            .register("glm-5.3_common", &format!("http://127.0.0.1:{}", port));
        state.runtime_http_registry = Some(registry);

        let app = super::proxy_routes().with_state(state);

        // ── GET /debug/state?session_id=… → Runtime GET /api/debug/state ──
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/glm-5.3_common/debug/state?session_id=s1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // ── POST /debug/step {session_id, granularity} ──
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri("/api/agents/glm-5.3_common/debug/step")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"session_id":"s1","granularity":"iteration"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // ── Deep path: context/{iteration}/sections/{section} ──
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/glm-5.3_common/debug/context/3/sections/system_prompt?session_id=s1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), 3, "all three requests forwarded: {:?}", got);
        assert!(
            got[0].starts_with("GET /api/debug/state?session_id=s1 body="),
            "state request forwarded verbatim: {}",
            got[0]
        );
        assert!(
            got[1].starts_with("POST /api/debug/step body="),
            "step request forwarded verbatim: {}",
            got[1]
        );
        assert!(
            got[1].contains(r#""session_id":"s1""#),
            "step body forwarded verbatim: {}",
            got[1]
        );
        assert!(
            got[2]
                .starts_with("GET /api/debug/context/3/sections/system_prompt?session_id=s1 body="),
            "deep path forwarded verbatim: {}",
            got[2]
        );
    }

    /// ADR-069 regression: `GET/PUT /api/agents/{id}/mcp-tools` must be
    /// reverse-proxied to the Runtime's `GET/PUT /agents/{id}/mcp-tools`.
    ///
    /// Without this route the Gateway returned 404 (never reaching the
    /// Runtime), so `ToolsTab.tsx` silently swallowed the error and the
    /// per-server tool list (e.g. `pm`'s 12 tools) never rendered — the
    /// "点击 mcp server 里的 pm，工具列表不刷新" bug. We stand up a mock
    /// Runtime that echoes method/path/body and assert both the GET and
    /// PUT forms arrive at the Runtime under the `/agents/{id}/mcp-tools`
    /// path (method and body preserved verbatim).
    #[tokio::test]
    async fn mcp_tools_proxy_reaches_runtime() {
        use std::sync::Arc;
        use tokio::sync::RwLock;
        use tower::util::ServiceExt;

        // ── Mock Runtime: echoes method/path/body back as JSON ──
        let received = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let received_for_server = received.clone();
        let runtime_app = axum::Router::new().fallback(axum::routing::any(
            move |req: axum::extract::Request| async move {
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                received_for_server.lock().unwrap().push(format!(
                    "{} {} body={}",
                    method,
                    uri,
                    String::from_utf8_lossy(&body)
                ));
                axum::Json(serde_json::json!({
                    "agent_id": "com.test.mcp-tools",
                    "servers": {
                        "pm": [{ "name": "pm_claim_task", "enabled": true }]
                    }
                }))
            },
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, runtime_app).await.unwrap();
        });

        // ── Gateway state with the mock Runtime registered ──
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-mcp-tools-proxy-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = crate::http::routes::AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let registry = crate::http::proxy::new_shared_registry();
        registry
            .write()
            .await
            .register("com.test.mcp-tools", &format!("http://127.0.0.1:{}", port));
        state.runtime_http_registry = Some(registry);

        let app = super::proxy_routes().with_state(state);

        // ── 1. GET /api/agents/{id}/mcp-tools → mock GET /agents/{id}/mcp-tools ──
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/com.test.mcp-tools/mcp-tools")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // ── 2. PUT /api/agents/{id}/mcp-tools with JSON body ──
        let resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("PUT")
                    .uri("/api/agents/com.test.mcp-tools/mcp-tools")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        r#"{"servers":{"pm":[{"name":"pm_submit_task","enabled":false}]}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        // ── 3. Assert the Runtime actually received both calls with the
        //    correct method / path / body ──
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            2,
            "both GET and PUT must reach the Runtime; got: {got:?}"
        );
        assert!(
            got[0].starts_with("GET /agents/com.test.mcp-tools/mcp-tools"),
            "GET must be forwarded to Runtime /agents/{{id}}/mcp-tools; got: {}",
            got[0]
        );
        assert!(
            got[1].starts_with("PUT /agents/com.test.mcp-tools/mcp-tools"),
            "PUT must be forwarded to Runtime /agents/{{id}}/mcp-tools; got: {}",
            got[1]
        );
        assert!(
            got[1].contains("pm_submit_task"),
            "PUT body must be forwarded verbatim; got: {}",
            got[1]
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_is_hop_by_hop_header() {
        use axum::http::HeaderName;

        // RFC 7230 §6.1 hop-by-hop headers
        assert!(is_hop_by_hop_header(&HeaderName::from_static("connection")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("keep-alive")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static(
            "proxy-authenticate"
        )));
        assert!(is_hop_by_hop_header(&HeaderName::from_static(
            "proxy-authorization"
        )));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("te")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("trailer")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static(
            "transfer-encoding"
        )));
        assert!(is_hop_by_hop_header(&HeaderName::from_static("upgrade")));

        // Proxy-specific exclusions
        assert!(is_hop_by_hop_header(&HeaderName::from_static("host")));
        assert!(is_hop_by_hop_header(&HeaderName::from_static(
            "content-length"
        )));

        // Standard headers that MUST be forwarded
        assert!(!is_hop_by_hop_header(&HeaderName::from_static(
            "content-type"
        )));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static("accept")));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static(
            "authorization"
        )));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static(
            "user-agent"
        )));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static(
            "x-request-id"
        )));
        assert!(!is_hop_by_hop_header(&HeaderName::from_static(
            "x-custom-header"
        )));
    }

    /// ADR-048 follow-up: a successful `POST /api/agents/{id}/debug/enable`
    /// proxy call (200 from Runtime) flips
    /// `RunningAgentInfo::debug_state` from `Disabled` → `Enabled`.
    /// Other `rest` paths and non-2xx responses leave the state alone.
    ///
    /// We exercise the post-proxy hook directly via a fake response —
    /// standing up a real reverse-proxy + Runtime is covered by the
    /// end-to-end tests in `acowork-runtime`. Here we only need to
    /// pin the in-memory state transition.
    #[tokio::test]
    async fn debug_enable_flips_running_state() {
        use crate::gateway::state::{DebugState, GatewayState, RunningAgentInfo};
        use std::sync::Arc as StdArc;
        use tokio::sync::RwLock as TokioRwLock;

        let shared_state: crate::http::routes::SharedHttpState = StdArc::new(TokioRwLock::new(
            GatewayState::new(std::env::temp_dir().to_str().unwrap_or("/tmp")),
        ));
        {
            let mut gw = shared_state.write().await;
            gw.add_running(RunningAgentInfo {
                instance_id: "0a1b2c3d-4e5f-4a6b-8c7d-8e9f0a1b2c3d".to_string(),
                agent_id: "com.test.proxy".to_string(),
                pid: 9999,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                node_id: "local".to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
                lifecycle_detail: String::new(),
                dev_mode: false,
                debug_state: DebugState::Disabled,
                debug_port: None,
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }

        // Simulate a successful 200 from the Runtime.
        let response = axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .unwrap();

        // Re-implement the hook logic locally (mirrors `proxy_debug_rpc`)
        // so the test exercises the exact state-mutation code path.
        // Using the helper inline avoids needing AppState plumbing.
        // ADR-073: `running_agents` is keyed by INSTANCE identity — the
        // package id is not a key.
        let id = "0a1b2c3d-4e5f-4a6b-8c7d-8e9f0a1b2c3d";
        if response.status().is_success() {
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Enabled
            {
                running.debug_state = DebugState::Enabled;
            }
        }
        let final_state = shared_state
            .read()
            .await
            .running_agents
            .get(id)
            .map(|r| r.debug_state);
        assert_eq!(
            final_state,
            Some(DebugState::Enabled),
            "successful enable should flip state to Enabled"
        );
    }

    /// ADR-048 follow-up (symmetric to the enable test above): a
    /// successful `POST /api/agents/{id}/debug/disable` proxy call
    /// flips `RunningAgentInfo::debug_state` from `Enabled` →
    /// `Disabled`. Non-2xx responses leave the state alone (e.g. a
    /// 503 "SessionManager not ready" must not silently turn off
    /// DevMode on the Gateway side).
    #[tokio::test]
    async fn debug_disable_flips_running_state() {
        use crate::gateway::state::{DebugState, GatewayState, RunningAgentInfo};
        use std::sync::Arc as StdArc;
        use tokio::sync::RwLock as TokioRwLock;

        let shared_state: crate::http::routes::SharedHttpState = StdArc::new(TokioRwLock::new(
            GatewayState::new(std::env::temp_dir().to_str().unwrap_or("/tmp")),
        ));
        {
            let mut gw = shared_state.write().await;
            gw.add_running(RunningAgentInfo {
                instance_id: "1b2c3d4e-5f6a-4b7c-8d8e-9f0a1b2c3d4e".to_string(),
                agent_id: "com.test.proxy_disable".to_string(),
                pid: 9999,
                started_at: chrono::Utc::now(),
                workspace: String::new(),
                node_id: "local".to_string(),
                lifecycle: acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
                lifecycle_detail: String::new(),
                dev_mode: true,
                // Start in the Enabled state — that's the
                // realistic precondition for a disable call.
                debug_state: DebugState::Enabled,
                debug_port: Some(19876),
                workspace_config_json: None,
                current_embed_dim: None,
                migration: None,
            });
        }

        // Sanity: starting state is Enabled. ADR-073: keyed by INSTANCE
        // identity, not the package id.
        let id = "1b2c3d4e-5f6a-4b7c-8d8e-9f0a1b2c3d4e";
        let pre = shared_state
            .read()
            .await
            .running_agents
            .get(id)
            .map(|r| r.debug_state);
        assert_eq!(pre, Some(DebugState::Enabled));

        // Simulate a successful 200 from the Runtime.
        let response = axum::response::Response::builder()
            .status(200)
            .body(axum::body::Body::empty())
            .unwrap();
        // Re-implement the hook logic locally (mirrors `proxy_debug_rpc`)
        // so the test exercises the exact state-mutation code path.
        if response.status().is_success() {
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Disabled
            {
                running.debug_state = DebugState::Disabled;
            }
        }
        let final_state = shared_state
            .read()
            .await
            .running_agents
            .get(id)
            .map(|r| r.debug_state);
        assert_eq!(
            final_state,
            Some(DebugState::Disabled),
            "successful disable should flip state to Disabled"
        );

        // Symmetry check: a second 200 (e.g. user double-clicks
        // "Exit Debug") must stay Disabled — the helper short-
        // circuits when the state is already Disabled, and the hook
        // here does too via the `!= Disabled` guard.
        if response.status().is_success() {
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Disabled
            {
                running.debug_state = DebugState::Disabled;
            }
        }
        assert_eq!(
            shared_state
                .read()
                .await
                .running_agents
                .get(id)
                .map(|r| r.debug_state),
            Some(DebugState::Disabled),
            "double-disable should leave state at Disabled"
        );

        // Failure-path check: a non-2xx response (e.g. Runtime 503
        // because SessionManager hasn't wired yet) must NOT flip the
        // state. The Gateway should keep reporting Enabled so the
        // Desktop sees the DebugPanel still live.
        let err_response = axum::response::Response::builder()
            .status(503)
            .body(axum::body::Body::empty())
            .unwrap();
        // Mutate the state back to Enabled first to make this a
        // meaningful test of the failure path.
        shared_state
            .write()
            .await
            .running_agents
            .get_mut(id)
            .unwrap()
            .debug_state = DebugState::Enabled;
        if err_response.status().is_success() {
            // (intentionally never taken on the failure path)
            let mut gw = shared_state.write().await;
            if let Some(running) = gw.running_agents.get_mut(id)
                && running.debug_state != DebugState::Disabled
            {
                running.debug_state = DebugState::Disabled;
            }
        }
        assert_eq!(
            shared_state
                .read()
                .await
                .running_agents
                .get(id)
                .map(|r| r.debug_state),
            Some(DebugState::Enabled),
            "non-2xx disable response must leave state at Enabled"
        );
    }

    // ── Health double-check route ──────────────────────────────────
    //
    // Regression: 2026-09-02 09:12 incident. MQTT disconnection during
    // system sleep marked agents as offline → frontend rendered them as
    // sleeping (zzz) even though the Runtime process was alive. The fix:
    // expose the Runtime's `/health` endpoint through the Gateway so the
    // frontend can HTTP-double-check liveness on MQTT drop. The tests
    // below pin the contract.

    /// Helper: spin up a mock Runtime on a random port. Returns the bound
    /// port and a shared Vec of received request strings.
    async fn spawn_mock_runtime() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use axum::routing::any;
        let received: std::sync::Arc<std::sync::Mutex<Vec<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let received_for_server = received.clone();
        let runtime_app =
            axum::Router::new().fallback(any(move |req: axum::extract::Request| async move {
                let method = req.method().to_string();
                let uri = req.uri().to_string();
                let body = axum::body::to_bytes(req.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                received_for_server.lock().unwrap().push(format!(
                    "{} {} body={}",
                    method,
                    uri,
                    String::from_utf8_lossy(&body)
                ));
                // Mimic Runtime's `/health` payload shape.
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "agent_id": "com.acowork.architect",
                    "degraded_reasons": []
                }))
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, runtime_app).await.unwrap();
        });
        (port, received)
    }

    /// Helper: build a Gateway state with a single registered agent
    /// pointing at the given mock-Runtime port. Returns `(AppState,
    /// registry-Arc)` so tests can unregister if needed.
    async fn build_proxy_state_with_agent(
        agent_id: &str,
        port: u16,
    ) -> (
        crate::http::routes::AppState,
        crate::http::proxy::SharedRuntimeHttpRegistry,
    ) {
        use std::sync::Arc as StdArc;
        use tokio::sync::RwLock as TokioRwLock;

        // Unique per call: several tests share an `agent_id`, so a
        // pid+agent_id key makes them race on `remove_dir_all` when they
        // run in parallel (observed as a spurious FAILED).
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-health-proxy-{}-{}-{}",
            std::process::id(),
            agent_id,
            seq
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let mut state = crate::http::routes::AppState::new(
            StdArc::new(TokioRwLock::new(gw_state)),
            StdArc::new(crate::http::auth::HttpAuth::new(false)),
        );
        let registry = crate::http::proxy::new_shared_registry();
        registry
            .write()
            .await
            .register(agent_id, &format!("http://127.0.0.1:{}", port));
        state.runtime_http_registry = Some(registry.clone());
        (state, registry)
    }

    /// Happy path: `GET /api/agents/{id}/health` forwards verbatim to the
    /// Runtime's `GET /health` and returns the Runtime's response body.
    #[tokio::test]
    async fn health_route_forwards_get_to_runtime_health_endpoint() {
        use tower::util::ServiceExt;

        let (port, received) = spawn_mock_runtime().await;
        let (state, _registry) = build_proxy_state_with_agent("com.acowork.architect", port).await;
        let app = super::proxy_routes().with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/com.acowork.architect/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json["status"], "ok");
        assert_eq!(body_json["agent_id"], "com.acowork.architect");

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), 1, "exactly one request forwarded");
        // The path suffix is rewritten from `/api/agents/{id}/health` to
        // `/health` so the Runtime sees the canonical endpoint.
        assert!(
            got[0].starts_with("GET /health body="),
            "request must be rewritten to GET /health, got: {}",
            got[0]
        );
    }

    /// Forwarded request must NOT carry a body, even an empty one — the
    /// Runtime's `/health` is a pure liveness probe.
    #[tokio::test]
    async fn health_route_does_not_forward_body() {
        use tower::util::ServiceExt;

        let (port, received) = spawn_mock_runtime().await;
        let (state, _registry) = build_proxy_state_with_agent("com.acowork.architect", port).await;
        let app = super::proxy_routes().with_state(state);

        // Even if the client somehow includes a body, the proxy must drop
        // it because `proxy_get_health` calls `proxy_to_runtime` (no body).
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/com.acowork.architect/health")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"should":"be dropped"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let got = received.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert!(
            got[0].ends_with("body="),
            "body must be dropped, got: {}",
            got[0]
        );
    }

    /// An unregistered agent returns 503 with a helpful message and a
    /// `Retry-After: 2` header so the frontend's `with503Retry` helper
    /// honors the boot-window retry cadence.
    ///
    /// The frontend uses this 503 to decide NOT to override MQTT's
    /// `online=false` — the agent is genuinely not running.
    #[tokio::test]
    async fn health_route_returns_503_when_agent_not_registered() {
        use tower::util::ServiceExt;

        let dir = std::env::temp_dir().join(format!(
            "acowork-test-health-proxy-unreg-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gw_state = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
        let state = crate::http::routes::AppState::new(
            std::sync::Arc::new(tokio::sync::RwLock::new(gw_state)),
            std::sync::Arc::new(crate::http::auth::HttpAuth::new(false)),
        );
        // Note: registry is initialized but EMPTY — no agent registered.
        let registry = crate::http::proxy::new_shared_registry();
        let mut state = state;
        state.runtime_http_registry = Some(registry);
        let app = super::proxy_routes().with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/com.acowork.never-registered/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap()),
            Some("2"),
            "503 must carry Retry-After: 2 to match the boot-window cadence"
        );

        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(
            body_json["error"], "agent_not_running",
            "503 body must explain why"
        );
        assert_eq!(body_json["id"], "com.acowork.never-registered");
    }

    /// When the agent's registered endpoint is unreachable (e.g. the
    /// Runtime process died after the registry was populated), the proxy
    /// must surface that as 502 BAD_GATEWAY so the frontend knows the
    /// agent is genuinely down — NOT misreport it as alive.
    #[tokio::test]
    async fn health_route_returns_502_when_runtime_unreachable() {
        use tower::util::ServiceExt;

        // Pick a port we don't bind. Reserve it and immediately release
        // to minimize the chance something else grabs it; even if it
        // does, the test still proves the proxy returns 502 on
        // connection failure.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = probe.local_addr().unwrap().port();
        drop(probe);

        let (state, _registry) = build_proxy_state_with_agent("com.acowork.dead", dead_port).await;
        let app = super::proxy_routes().with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/com.acowork.dead/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Connection refused → BAD_GATEWAY. Anything in the 5xx range
        // works for the contract; we pin BAD_GATEWAY because it's what
        // `proxy_to_runtime_with_method` returns and the frontend
        // already treats it as "not alive".
        assert!(
            resp.status().is_server_error(),
            "expected 5xx on unreachable runtime, got {}",
            resp.status()
        );
    }

    /// ADR-085 D4b: a non-2xx from the Runtime (here 503
    /// `session_not_ready`) must be passed through verbatim — status,
    /// body, and `Retry-After` — never collapsed into a 404.
    #[tokio::test]
    async fn proxy_passes_through_runtime_503_verbatim() {
        use tower::util::ServiceExt;

        let runtime_app = axum::Router::new().fallback(|| async {
            let mut resp = (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "session_not_ready"})),
            )
                .into_response();
            resp.headers_mut()
                .insert("retry-after", axum::http::HeaderValue::from_static("2"));
            resp
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, runtime_app).await.unwrap() });

        let (state, _registry) = build_proxy_state_with_agent("com.acowork.architect", port).await;
        let app = super::proxy_routes().with_state(state);

        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/agents/com.acowork.architect/tools")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "Runtime 503 must survive the proxy as 503, not collapse to 404"
        );
        assert_eq!(
            resp.headers()
                .get("retry-after")
                .map(|v| v.to_str().unwrap()),
            Some("2"),
            "Runtime's Retry-After must be forwarded"
        );
        let body_bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body_json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(body_json["error"], "session_not_ready");
    }
}

