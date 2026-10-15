//! Runtime localhost HTTP server (ADR-033 Phase 2 + ADR-034 Phase 3).
//!
//! Serves queries and write operations for the Gateway reverse proxy.
//!
//! ADR-034 §11.2 is the authoritative endpoint list (25 routes — 12
//! retained/fixed and 13 newly added in Phase 3). The control plane
//! (`POST /sessions/{sid}/{action}`) is deliberately **not** exposed:
//! user-initiated state changes flow through the
//! `acowork/agents/{id}/sessions/control/{cmd}` MQTT topic, not this
//! HTTP server.
//!
//! ```text
//! Data plane (25 routes — 12 retained + 13 added in Phase 3)
//! ───────────────────────────────────────────────────────────
//! GET    /health                                 // retained
//! GET    /sessions                               // retained
//! GET    /sessions/latest                        // retained
//! GET    /sessions/{sid}                         // NEW: panel-4 (merges meta + state)
//! GET    /sessions/{sid}/messages                // retained
//! POST   /sessions/{sid}/files                   // ADR-046: upload file/image
//! GET    /files/{document_id}                    // ADR-046: download blob
//! GET    /memory/graph                           // reads the memory store
//! GET    /memory/nodes                           // retained
//! GET    /memory/nodes/{nid}                     // NEW: memory_query::get_node
//! DELETE /memory/nodes/{nid}                     // retained
//! GET    /memory/stats                           // retained
//! GET    /files/{id}                             // retained
//! GET    /workspaces                             // retained
//! POST   /workspaces                             // NEW
//! PUT    /workspaces/{ws_id}                     // NEW
//! PUT    /workspaces/{ws_id}/prompt-file         // NEW
//! DELETE /workspaces/{ws_id}                     // NEW
//! GET    /workspaces/tree                        // retained (panel 6)
//! GET    /workspaces/file                        // NEW: read file  (panel 6)
//! POST   /workspaces/file                        // NEW: create file
//! PUT    /workspaces/file                        // NEW: overwrite file
//! DELETE /workspaces/file                        // NEW: delete file
//! GET    /workspaces/raw/{path}                  // ADR-055 L2-7: raw bytes (preview iframe)
//! POST   /workspaces/dir                         // NEW: create dir
//! DELETE /workspaces/dir                         // NEW: delete dir
//! POST   /workspaces/copy                        // NEW: copy file/dir tree
//! POST   /workspaces/rename                      // NEW: atomic rename (move)
//! POST   /memory/nodes                           // NEW: create memory node
//! PUT    /memory/nodes/{nid}                     // NEW: update memory node
//! GET    /agents/{id}/config                     // NEW: panel 1
//! GET    /agents/{id}/tools                      // NEW: panel 3
//! GET    /agents/{id}/status                     // NEW: panel 5
//! GET    /agents/{id}/skills                     // ADR-009 §V-A
//! GET    /agents/{id}/skills/{name}              // ADR-009 §V-A
//! GET    /agents/{id}/skills/{name}/history      // ADR-009 §V-A
//!
//! Removed in Phase 3 (ADR-034 §7.6.4):
//!   ~~GET  /sessions/{sid}/state~~        → absorbed by /sessions/{sid}
//!   ~~POST /sessions/{sid}/approval~~     → routed via MQTT
//!   ~~POST /sessions/{sid}/question~~     → routed via MQTT
//!   ~~POST /sessions/{sid}/continue~~     → routed via MQTT
//! ```
//!
//! The three Grafeo-backed `/memory/*` endpoints (`/memory/nodes`,
//! `/memory/nodes/{nid}`, `/memory/stats`) share their business logic
//! with the legacy gRPC path through [`crate::http::memory_query`], so
//! HTTP and gRPC responses stay
//! consistent.
//!
//! The server binds to `127.0.0.1:0` (random port) and is intended
//! for Gateway reverse proxy access only — not direct Desktop access.
//!
//! See `docs/zh/protocols/mqtt.md` §7.5.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use crate::http::global_search;
use crate::http::search_summary;

use crate::mqtt::client::SharedRuntimeMqttClient;

/// Error type for Runtime HTTP server operations.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeHttpServerError {
    #[error("HTTP server error: {0}")]
    Server(String),
    #[error("Failed to bind: {0}")]
    Bind(String),
}

/// Global body-size cap for every Runtime HTTP route.
///
/// Axum's default `DefaultBodyLimit` is 2 MiB — which silently caps
/// any extractor (`Json`, `Bytes`, `String`, `Multipart`, `Form`,
/// etc.) before our handlers can read a single byte. The user-facing
/// fallout is opaque ("Error parsing 'multipart/form-data' request"
/// for a 9 MB PDF, generic JSON parse errors for a 3 MB request
/// body, etc.) instead of a clean 413 + JSON error body from the
/// service layer.
///
/// We raise the global cap to 64 MiB at the root router so every
/// endpoint that legitimately needs a large body — multipart upload
/// (`/sessions/{sid}/files`), workspace file write
/// (`PUT /workspaces/file` with embedded content), memory node
/// create/update payloads, etc. — can reach its handler. Per-route
/// service-layer checks remain the source of truth for user-facing
/// limits:
///
///   - attachments: [`crate::usecases::MAX_UPLOAD_BYTES`] (50 MiB)
///   - workspace files: implicit 64 MiB ceiling via this limit;
///
/// anything larger is rejected at the handler with a clean error
/// instead of at the extractor with an opaque one.
const GLOBAL_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Shared dispatch sender for Runtime HTTP → agent loop.
///
/// Cloned from the main MQTT dispatch channel. Runtime HTTP handlers
/// use this to send (session_id, InboundMessage) tuples that are
/// forwarded to the right session's AgentLoop via `send_inbound()`.
pub type SharedDispatchSender =
    Arc<tokio::sync::Mutex<Option<mpsc::UnboundedSender<(String, InboundMessage)>>>>;

/// Shared handle to the Runtime's memory admin service.
///
/// `None` until Phase B (`init_memory_provider`) finishes; HTTP handlers
/// report a graceful "no store" response when it is still empty
/// (see [`memory_query`]).
///
/// ADR-051 P4: type changed from `Arc<GrafeoStore>` to
/// `Arc<dyn MemoryAdminService>` so the Runtime does not depend on
/// the concrete grafeo type for HTTP admin endpoints.
pub type SharedMemoryStore =
    Arc<std::sync::RwLock<Option<Arc<dyn acowork_memory::admin::MemoryAdminService>>>>;

/// Shared slot for the conversation vector index (ADR-081 §4.2, P1-2).
///
/// Late-bind from Phase B: `ConversationIndex` is built on the memory
/// backend's SQLite store once the memory store is up, and a
/// [`crate::conversation_index::ConversationIndexer`] task tails the
/// JSONL conversation logs. `None` until then — the `/search`
/// conversation scope reports `not_indexed` and search degrades to the
/// other scopes.
pub type SharedConversationIndex =
    Arc<std::sync::RwLock<Option<Arc<crate::conversation_index::ConversationIndex>>>>;

/// Shared slot for the consolidation timer (late-bind from AgentCore).
/// Used by `GET /memory/consolidation/status` to report idle time, pending count.
pub type SharedConsolidationTimer =
    Arc<std::sync::RwLock<Option<Arc<crate::memory::ConsolidationTimer>>>>;

/// Shared slot for the RAG provider (late-bind from AgentCore).
/// Used by `GET /agents/{id}/rag/status` and `POST /agents/{id}/rag/query`.
pub type SharedRagProvider =
    Arc<std::sync::RwLock<Option<Arc<dyn acowork_core::rag::RagProvider>>>>;

/// Shared handle to the Runtime's `AgentCore`.
///
/// `None` until Phase B creates the `AgentCore`; HTTP handlers that
/// need agent-level token totals (`list_sessions`) gracefully degrade
/// (return 0) when it is still empty.
///
/// ADR-028: the `list_sessions` handler still merges disk-scanned
/// totals into the live atomic counters (cold-start recovery) so the
/// first post-restart `context_usage` push reflects historical totals.
/// Since t-83afab47 the merged value is no longer echoed back in the
/// HTTP response — agent totals live on the live push path only.
pub type SharedAgentCore = Arc<std::sync::RwLock<Option<Arc<crate::agent::agent_core::AgentCore>>>>;

// `SharedEmbedDimension` — a u64 slot holding the model's width — is retired.
// It was written once, when the provider was built, and never again. After a model
// switch the memory panel therefore reported a width the process no longer used, and
// no amount of re-embedding could clear the banner. Its readers now ask the live
// provider (`AgentCore::live_embedding_provider`), the one value a session updates
// when it adopts a new model.
/// Shared degradation reasons — startup-phase failures that are not
/// fatal enough to abort the runtime but degrade functionality.
///
/// Populated during Phase B when session persistence is unavailable
/// (e.g. sandbox EPERM on the conversations directory) and read by
/// the `/health` endpoint so the frontend can surface a warning.
pub type SharedDegradation = Arc<std::sync::RwLock<Vec<String>>>;

/// Late-bind slot for the Runtime's MQTT client.
///
/// The HTTP server starts in Phase A before the MQTT client is
/// connected, so we hand it an `Option`-wrapped `SharedRuntimeMqttClient`
/// and populate it once Phase A finishes wiring up the broker
/// connection. Handlers treat `None` as "no MQTT available yet" and
/// silently skip the retained PUBLISH — the persisted on-disk file is
/// still authoritative for the next GET.
pub type SharedMqttClientSlot = Arc<tokio::sync::Mutex<Option<SharedRuntimeMqttClient>>>;

/// Late-bind slot for the Runtime's `SessionManager`.
///
/// The HTTP server starts in Phase A before `SessionManager` is
/// constructed in Phase B, so we hand the server an
/// `Option`-wrapped `Arc<Mutex<SessionManager>>` and populate it once
/// Phase B finishes. The only handler that reads this slot today is
/// `POST /api/debug/enable` (`http/debug.rs`), which uses it to call
/// `SessionManager::enable_debug_mode` for runtime DevMode activation.
/// `None` is reported back to the caller as 503 — same ADR-040 pattern
/// as every other late-bind slot.
///
/// Type is `Arc<...>` (not `Mutex`) because the server only ever reads
/// the slot to clone the inner `Arc<Mutex<SessionManager>>`; the
/// exclusive write happens once at Phase B and never changes again.
/// An `RwLock` keeps the read path lock-free.
pub type SharedSessionManagerSlot = Arc<
    tokio::sync::RwLock<Option<Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>>>,
>;

/// State shared with HTTP handlers.
#[derive(Clone)]
pub(crate) struct HttpState {
    pub(crate) work_dir: PathBuf,
    /// ADR-063: the agent's `.agent` package directory (where
    /// `prompts/<file>.md` lives). Distinct from `work_dir` — the work
    /// directory holds runtime state (sessions, attachments) while the
    /// package directory holds the declared directives (manifest.toml,
    /// prompts/, skills/). `prompts/*.md` PUT reads/writes go through
    /// this path; Phase A loads the 9 overrides from here via
    /// `load_optional_prompt`.
    pub(crate) package_dir: PathBuf,
    pub(crate) agent_id: String,
    /// ADR-073: instance identity (UUID v4). Accepts instance-scoped
    /// `/agents/{id}/*` paths from the Gateway reverse proxy.
    pub(crate) instance_id: String,
    /// Late-bind slot for the `AgentCore` Arc. Created empty in Phase A
    /// and cloned into the HTTP server alongside the other late-bind
    /// resources; populated by Phase B (`session_init.rs` line ~715) once
    /// `AgentCore::new` completes.
    ///
    /// Used by the L2 reload handler (`POST /agents/{id}/prompts/reload`
    /// in `http::prompts`) to push the on-disk `prompts/<file>.md`
    /// content back into the live `Arc<RwLock<Option<String>>>` fields
    /// without going through the per-session Debug controllers — the
    /// reload is a **package-level** operation, not a debug-session one,
    /// so it must work even when DevMode is disabled.
    ///
    /// Type mirrors `SharedRagProvider` / `SharedConsolidationTimer`:
    /// `Arc<std::sync::RwLock<Option<Arc<AgentCore>>>>`. Sync `RwLock`
    /// (not `tokio::Mutex`) because the reload only holds the lock for
    /// the duration of a `clone()` — the actual writes go through the
    /// per-field `Arc<RwLock<Option<String>>>` inside `AgentCore`.
    pub(crate) agent_core: SharedAgentCore,
    /// Shell risk rules — loaded at startup; updated in-place by PUT
    /// `/agents/{id}/shell-risk-rules` so live sessions pick up new rules
    /// without a restart. Write via `Arc<RwLock>` because the state is
    /// cloned into each axum handler (axum's `State<T>` requires `Clone`).
    shell_risk_rules: Arc<std::sync::RwLock<crate::security::shell_risk::ShellRiskRules>>,
    /// Retained on the state for parity with the startup wiring
    /// (`SessionManager.config.session_snapshots`); the HTTP layer no
    /// longer enumerates active sessions directly (the ADR-052 hot-reload
    /// path pushes a single system-level message and lets
    /// `dispatch_inbound` -> `SessionManager::{apply_runtime_config_override,
    /// apply_builtin_tools_enabled}` walk `SessionManager.sessions`
    /// itself). Suppresses `unused` because removing it would touch ~10
    /// `RuntimeHttpServer::start` test call sites for no functional
    /// gain.
    #[allow(dead_code)]
    session_snapshots: SharedSessionSnapshots,
    /// Shared latest session info, updated by SessionManager on every
    /// session creation and startup scan.  Read by `get_latest_session`.
    latest_session: SharedLatestSession,
    /// Dispatch sender for write operations (approval, question, continue, title).
    /// Set after the session manager starts. None = agent not ready yet.
    dispatch_tx: SharedDispatchSender,
    /// Active embedding provider dimension. Set once at Phase A
    /// and read by the usecase service (via the memory_query slot).
    /// Startup degradation reasons surfaced via `/health`.
    /// Populated by Phase B when non-fatal errors occur.
    degraded_reasons: SharedDegradation,
    /// Late-bind slot for the MQTT client. Populated by Phase A after
    /// the broker connection is established. The `PUT /agents/{id}/config`
    /// handler uses this to re-PUBLISH the retained config snapshot so
    /// other Desktop subscribers see the new values without a restart.
    /// Also consumed by `POST /api/debug/enable` to wire the
    /// DebugEventMqttPublisher when DevMode is flipped on at runtime.
    pub(crate) mqtt_client: SharedMqttClientSlot,
    /// ADR-040: late-bind usecase services. Populated by Phase B.
    /// All handlers depend solely on these traits; no direct access
    /// to memory_store or agent_core is required.
    session_metadata:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>>,
    pub(crate) memory_query:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
    /// ADR-081 §4.2: conversation vector index (late-bind slot, Phase B).
    pub(crate) conversation_index: SharedConversationIndex,
    /// Live state of a background `/memory/rebuild-embeddings` run.
    ///
    /// The rebuild is started as a detached task (the HTTP handler
    /// returns immediately with `{"status":"started"}`) and progress is
    /// read back via `GET /memory/rebuild-progress`. Keeping the slot
    /// here (not as a constructor argument) means the many test
    /// construction sites of `HttpState` are untouched. `std::sync::Mutex`
    /// (not tokio) because the progress callback fires from the
    /// `spawn_blocking` migration thread — a sync context.
    rebuild_progress: Arc<std::sync::Mutex<Option<RebuildProgress>>>,
    pub(crate) workspace_query:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>>,
    workspace_mutation:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>>,
    /// ADR-078: Late-bind slot for the git query service (read-only
    /// `/git/status` `/git/diff` `/git/log`). Git execution happens in
    /// the Runtime (the workspace owner, ADR-009 v2) — the Gateway only
    /// reverse-proxies these endpoints. Populated in Phase B.
    pub(crate) git_query:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::GitQueryService>>>>,
    /// ADR-040 follow-up: Tools-panel persistence (MCP active state).
    /// The `/agents/{id}/mcp-servers` HTTP handlers route through this
    /// trait instead of calling `agent_config::*` directly. Populated
    /// in Phase B (sync, no async resource dependency like memory).
    agent_tools: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
    /// ADR-040 follow-up: per-agent runtime config (`agent_config.json`)
    /// persistence. The `GET/PUT /agents/{id}/config` handlers route
    /// through this trait instead of calling `agent_config::*`
    /// directly. Also serves `/agents/{id}/builtin-tools` because that
    /// endpoint persists `agent_tools.json` (a separate file).
    agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
    /// ADR-046: Late-bind slot for the unified attachment blob store
    /// (`POST /sessions/{sid}/files` upload + `GET /files/{doc_id}`
    /// download). The HTTP handlers route through this trait instead
    /// of touching the filesystem directly — same ADR-040 pattern as
    /// every other UseCase service. Populated in Phase B (sync —
    /// requires only the boot-time `work_dir`).
    attachment: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
    /// ADR-047: Session config service for GET/PUT /sessions/{sid}/config.
    session_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
    /// Consolidation timer (late-bind from AgentCore Phase B).
    /// Used by `GET /memory/consolidation/status`.
    consolidation_timer: SharedConsolidationTimer,
    /// RAG provider (late-bind from AgentCore Phase B).
    /// Used by `GET /agents/{id}/rag/status` and `POST /agents/{id}/rag/query`.
    rag_provider: SharedRagProvider,
    /// ADR-048: late-bind slot for the Debug service. Populated by Phase
    /// B after SessionManager wires up per-session debug controllers.
    /// `None` until then; HTTP handlers return 503 with a descriptive
    /// message when this slot is still empty (same pattern as every
    /// other ADR-040 use-case slot).
    pub(crate) debug_service:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,
    /// Shared `WorkspaceResolver` — the **same** `Arc` injected into
    /// `SessionManager` at Phase B (see `startup/context.rs` +
    /// `session_init.rs`). The workspace-mutation handlers reload it
    /// after every successful create/update/delete so `route_workspace_switch`
    /// (which validates the requested `workspace_id` via
    /// `resolver.find_by_id`) sees newly-added workspaces without a
    /// Runtime restart. Without this, a fresh workspace could be
    /// persisted to disk and listed by the desktop but never selected —
    /// the switch would fall back to `__agent_home__`.
    workspace_resolver: crate::tools::workspace_resolver::SharedResolver,
    /// Late-bind slot for `SessionManager`. Populated by Phase B once
    /// the session manager is constructed. The only consumer is
    /// `POST /api/debug/enable` (`http/debug::post_enable`), which
    /// uses it to flip DevMode on at runtime without restarting the
    /// agent. `None` outside Phase B (e.g. early boot, tests that
    /// never build a SessionManager).
    pub(crate) session_manager_slot: crate::http::server::SharedSessionManagerSlot,
    /// ADR-069 follow-up: MCP config change notifier. Created inside
    /// `start_with_bind_port` (so the same Arc lives on this state AND
    /// the returned handle) and cloned out by `agent_init` so the
    /// builtin `mcp_install`/`mcp_uninstall` tools and the gateway loop
    /// subscribe to the **same** notifier.
    ///
    /// `PUT /agents/{id}/mcp-servers` and `PUT /agents/{id}/mcp-tools`
    /// call `notify()` after persisting so the gateway loop's
    /// `mcp_config_rx.changed()` branch reconnects MCP servers in the
    /// background and re-reconciles `agent_mcp_tools.json` — without
    /// this, enabling a server (e.g. `playwright`) never connects it,
    /// so its tool list never materialises and the Tools panel shows an
    /// empty expansion.
    pub(crate) mcp_notifier: crate::mcp_notify::McpNotifyRef,
    /// ADR-058: workspace FS watcher set. Created empty in Phase A,
    /// populated by Phase C (`start_workspace_watchers`) and reconciled
    /// after every workspace CRUD mutation (see
    /// `sync_workspace_watchers`). Publishing goes through
    /// [`Self::mqtt_client`] — the watcher set holds a clone of the
    /// same slot.
    pub(crate) workspace_watchers: crate::workspace::SharedWorkspaceWatcherSet,
}

/// Handle to the running HTTP server.
pub struct RuntimeHttpServer {
    /// The address the server is listening on.
    pub listen_addr: SocketAddr,
    /// The port the server is listening on (extracted from listen_addr).
    pub port: u16,
    /// ADR-058: the workspace FS watcher set shared with the HTTP
    /// state. Clone this into the boot context so Phase C and the CRUD
    /// mutation hooks reconcile the same set.
    pub workspace_watchers: crate::workspace::SharedWorkspaceWatcherSet,
    /// ADR-069 follow-up: MCP config change notifier — the **same** Arc
    /// stored on `HttpState`. `agent_init` clones this out (before the
    /// handle is dropped) so the builtin MCP tools and the gateway loop
    /// subscribe to the notifier that the HTTP `PUT /mcp-servers` /
    /// `PUT /mcp-tools` handlers fire.
    pub mcp_notifier: crate::mcp_notify::McpNotifyRef,
    /// The join handle for the server task. Dropping this aborts the server.
    _handle: tokio::task::JoinHandle<()>,
}

impl HttpState {
    /// ADR-073: a `/agents/{id}/*` path matches only when `id` equals the
    /// canonical **instance identity** (`instance_id`, UUID v4). The
    /// package `agent_id` is display metadata and is deliberately NOT
    /// accepted in the path — a package-addressed request is a caller bug
    /// (misconfigured Gateway / old client) and must fail loudly, not
    /// silently hit an arbitrary instance of the package.
    /// The dimension the agent's live embedding provider produces, 0 if none.
    ///
    /// Derived from the published `AgentCore`'s provider cell rather than kept
    /// as a number on this struct: the session that adopts a new embedding
    /// provider has no handle on HTTP state, so a copy here reported the model
    /// the process started with long after the model had changed - which is how
    /// the memory panel told the user to rebuild an index that already matched.
    pub(crate) fn live_embed_dim(&self) -> u64 {
        self.agent_core
            .read()
            .ok()
            .and_then(|guard| {
                guard
                    .as_ref()
                    .and_then(|core| core.live_embedding_provider())
            })
            .map(|p| p.dimension() as u64)
            .unwrap_or(0)
    }

    pub(crate) fn instance_matches(&self, id: &str) -> bool {
        id == self.instance_id
    }
}

impl RuntimeHttpServer {
    /// Start the HTTP server on `127.0.0.1:0` (random port).
    ///
    /// Returns the server handle with the assigned port. The Gateway
    /// uses this port to reverse-proxy large data queries.
    ///
    /// Note: `start` intentionally takes each shared resource individually
    /// (rather than bundling everything in a single config struct) to keep
    /// resource lifetimes explicit at the call site and to avoid coupling
    /// the HTTP module to startup-phase internals.
    ///
    /// ADR-063: `package_dir` is the agent's `.agent` package directory,
    /// distinct from `work_dir` (which holds runtime state). The two
    /// `/agents/{id}/prompts*` routes mount under this server via
    /// [`crate::http::prompts::prompts_routes`] and resolve every override
    /// through `<package_dir>/prompts/<file>.md`.
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        work_dir: PathBuf,
        package_dir: PathBuf,
        agent_id: String,
        // ADR-073: the instance identity (UUID). Production passes the real
        // UUID so instance-scoped reverse-proxy paths resolve; tests pass
        // a test-local literal (uuid in the MQTT-config test, a shared package
        // string elsewhere); the route guard matches this field only — the
        // package `agent_id` is never accepted in `/agents/{id}/*` paths.
        instance_id: String,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
        dispatch_tx: SharedDispatchSender,
            degraded_reasons: SharedDegradation,
        mqtt_client: SharedMqttClientSlot,
        session_metadata: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>,
        >,
        memory_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
        conversation_index: SharedConversationIndex,
        workspace_query: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>,
        >,
        workspace_mutation: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>,
        >,
        git_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::GitQueryService>>>>,
        agent_tools: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
        agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
        attachment: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
        session_config: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
        >,
        consolidation_timer: SharedConsolidationTimer,
        rag_provider: SharedRagProvider,
        debug_service: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,
        workspace_resolver: crate::tools::workspace_resolver::SharedResolver,
        session_manager_slot: SharedSessionManagerSlot,
        agent_core: SharedAgentCore,
    ) -> Result<Self, RuntimeHttpServerError> {
        // Historical behaviour: random loopback port.
        Self::start_with_bind_port(
            0,
            work_dir,
            package_dir,
            agent_id,
            instance_id,
            session_snapshots,
            latest_session,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            session_metadata,
            memory_query,
            conversation_index,
            workspace_query,
            workspace_mutation,
            git_query,
            agent_tools,
            agent_config,
            attachment,
            session_config,
            consolidation_timer,
            rag_provider,
            debug_service,
            workspace_resolver,
            session_manager_slot,
            agent_core,
        )
        .await
    }

    /// ADR-055 §6.4: start the HTTP server on an explicit loopback port.
    ///
    /// The Node allocates a concrete port (from `NODE_HTTP_PORT_BASE`) and
    /// passes it via `--http-port` so its reverse proxy has a stable
    /// `{instance_id} → port` mapping. `bind_port = 0` keeps the historical
    /// random-port behaviour (`127.0.0.1:0`).
    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_bind_port(
        bind_port: u16,
        work_dir: PathBuf,
        package_dir: PathBuf,
        agent_id: String,
        // ADR-073: the instance identity (UUID). Production passes the real
        // UUID so instance-scoped reverse-proxy paths resolve; tests pass
        // a test-local literal (uuid in the MQTT-config test, a shared package
        // string elsewhere); the route guard matches this field only — the
        // package `agent_id` is never accepted in `/agents/{id}/*` paths.
        instance_id: String,
        session_snapshots: SharedSessionSnapshots,
        latest_session: SharedLatestSession,
        dispatch_tx: SharedDispatchSender,
            degraded_reasons: SharedDegradation,
        mqtt_client: SharedMqttClientSlot,
        session_metadata: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>,
        >,
        memory_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
        conversation_index: SharedConversationIndex,
        workspace_query: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>,
        >,
        workspace_mutation: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>,
        >,
        git_query: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::GitQueryService>>>>,
        agent_tools: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
        agent_config: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
        attachment: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
        session_config: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
        >,
        consolidation_timer: SharedConsolidationTimer,
        rag_provider: SharedRagProvider,
        debug_service: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,
        workspace_resolver: crate::tools::workspace_resolver::SharedResolver,
        session_manager_slot: SharedSessionManagerSlot,
        agent_core: SharedAgentCore,
    ) -> Result<Self, RuntimeHttpServerError> {
        // ADR-058: the workspace FS watcher set is created here so the
        // HTTP state and the boot context share exactly one Arc. It is
        // exposed on the returned handle for `agent_init` to clone into
        // `AgentBootContext` (Phase C startup hook) — tests that never
        // touch workspace events simply ignore the field.
        let workspace_watchers: crate::workspace::SharedWorkspaceWatcherSet =
            Arc::new(tokio::sync::Mutex::new(
                crate::workspace::WorkspaceWatcherSet::new(agent_id.clone(), mqtt_client.clone()),
            ));
        // ADR-069 follow-up: one shared notifier for HTTP handlers +
        // builtin MCP tools + gateway loop. Created here (rather than
        // by the caller) so `start`/`start_with_bind_port` call sites —
        // including 27+ tests — need no new argument. The handle exposes
        // the same Arc so `agent_init` can clone it out for the tools
        // and boot context.
        let mcp_notifier: crate::mcp_notify::McpNotifyRef =
            Some(Arc::new(crate::mcp_notify::McpConfigNotifier::default()));
        let shell_risk_rules =
            crate::security::shell_risk::ShellRiskRules::load(&work_dir).unwrap_or_default();
        let state = HttpState {
            work_dir,
            package_dir,
            agent_id,
            instance_id,
            shell_risk_rules: Arc::new(std::sync::RwLock::new(shell_risk_rules)),
            session_snapshots,
            latest_session,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            session_metadata,
            memory_query,
            conversation_index,
            rebuild_progress: Arc::new(std::sync::Mutex::new(None)),
            workspace_query,
            workspace_mutation,
            git_query,
            agent_tools,
            agent_config,
            attachment,
            session_config,
            consolidation_timer,
            rag_provider,
            debug_service,
            workspace_resolver,
            session_manager_slot,
            agent_core,
            workspace_watchers: workspace_watchers.clone(),
            mcp_notifier: mcp_notifier.clone(),
        };

        // ADR-076 §决策 4: the session **control** plane lives here now,
        // not on MQTT. It moved because an MQTT control message carries no
        // identity — the broker cannot stamp one — so `create` could not
        // record an owner and `close`/`delete` could not check one. Over
        // HTTP the Gateway authenticates first and forwards the resolved
        // scope as `x-user-id`.
        //
        // This deliberately reverses ADR-034 §11.2 ("control plane
        // intentionally absent from HTTP"). The remaining MQTT control
        // commands (chat, model switch, …) stay on MQTT: they carry no
        // ownership decision, and ADR-076 defers MQTT-side identity
        // (ADR-077).
        let app = Router::new()
            .route("/health", get(health))
            .route(
                "/sessions",
                get(list_sessions)
                    .post(crate::http::session_control::post_create_session)
                    .delete(crate::http::session_control::clear_sessions),
            )
            .route(
                "/sessions/{sid}/open",
                post(crate::http::session_control::post_open_session),
            )
            .route(
                "/sessions/{sid}/close",
                post(crate::http::session_control::post_close_session),
            )
            .route(
                "/intent-context/close",
                post(crate::http::session_control::post_close_intent_context),
            )
            .route(
                "/sessions/{sid}/visibility",
                put(crate::http::session_control::put_session_visibility),
            )
            // ADR-076 §决策 4: workspace switch gets its own endpoint
            // rather than riding `PUT .../config` — only
            // `route_workspace_switch` updates `current_work_dir` and the
            // workspace context (see the handler's doc comment).
            .route(
                "/sessions/{sid}/workspace",
                put(crate::http::session_control::put_session_workspace),
            )
            .route("/sessions/latest", get(get_latest_session))
            // NEW: panel-4 endpoint, merges meta.json + live state snapshot.
            .route(
                "/sessions/{sid}",
                get(get_session).delete(crate::http::session_control::delete_session),
            )
            .route("/sessions/{sid}/messages", get(get_messages).post(crate::http::session_control::post_send_message))
            // ADR-076 §决策 4 (second wave): user-triggered session actions.
            // These carried no ownership decision, but they carried no
            // *identity* either, so over MQTT they could be replayed into
            // any session on the broker. Routing them through the Gateway's
            // authenticated API is what makes `authorize_write` possible.
            .route(
                "/sessions/{sid}/stop",
                post(crate::http::session_control::post_stop_session),
            )
            .route(
                "/sessions/{sid}/continue",
                post(crate::http::session_control::post_continue_execution),
            )
            .route(
                "/sessions/{sid}/approval",
                post(crate::http::session_control::post_approval_decision),
            )
            .route(
                "/sessions/{sid}/answer",
                post(crate::http::session_control::post_question_answer),
            )
            .route(
                "/sessions/{sid}/cancel-tool",
                post(crate::http::session_control::post_cancel_tool),
            )
            .route(
                "/sessions/{sid}/compress",
                post(crate::http::session_control::post_compress_action),
            )
            // ADR-047: session config endpoint - read/write config
            // without going through the serial inference queue.
            .route(
                "/sessions/{sid}/config",
                get(get_session_config).put(put_session_config),
            )
            // ADR-046: 2 attachment routes — POST is session-scoped (the desktop
            // knows which session is sending the upload), GET is
            // global-scoped (the desktop knows `document_id` from the
            // JSONL metadata it already loaded, no session_id needed).
            // Both route through `AttachmentService` (ADR-040).
            //
            // Per-route body-limit override is NOT needed here: the
            // global `DefaultBodyLimit::max(GLOBAL_BODY_LIMIT)` layer
            // (64 MiB) installed at the root of the router covers the
            // `Multipart` extractor. The Runtime's own size check
            // ([`crate::usecases::MAX_UPLOAD_BYTES`] = 50 MiB) then
            // produces a clean 413 + JSON error body if the payload is
            // over the user-facing limit.
            .route("/sessions/{sid}/files", post(upload_file))
            .route("/files/{document_id}", get(read_file))
            .route("/memory/graph", get(get_memory_graph))
            .route(
                "/memory/nodes",
                get(get_memory_nodes).post(create_memory_node),
            )
            // GET + DELETE retained from Phase 3; PUT (update) added now
            // so HTTP CRUD is complete and the gateway can proxy all 4 ops
            // through the same path shape.
            .route(
                "/memory/nodes/{nid}",
                get(get_memory_node)
                    .delete(delete_memory_node)
                    .put(update_memory_node),
            )
            .route("/memory/stats", get(get_memory_stats))
            .route("/memory/distill", post(post_memory_distill))
            .route("/memory/rebuild-embeddings", post(rebuild_embeddings))
            .route("/memory/rebuild-progress", get(rebuild_progress))
            .layer(DefaultBodyLimit::max(GLOBAL_BODY_LIMIT))
            // NOTE: the legacy `GET /files/{id}` handler was removed as part
            // of the ADR-040 / ADR-009 v2 workspace consolidation. Workspace
            // file reads now flow exclusively through
            // `GET /workspaces/file?path=…` (see `read_workspace_file`), which
            // dispatches via the `WorkspaceQueryService::read_file` trait so
            // the HTTP server does not directly touch the filesystem.
            // workspaces: retained GET + 4 new mutation routes.
            .route("/workspaces", get(list_workspaces).post(create_workspace))
            .route(
                "/workspaces/{ws_id}",
                put(update_workspace).delete(delete_workspace),
            )
            .route(
                "/workspaces/{ws_id}/prompt-file",
                put(set_workspace_prompt_file),
            )
            // ADR-058 (demand-driven revision): frontend pushes the watch
            // set (open editor tabs + expanded tree dirs) here. Full
            // replace semantics — the Runtime diffs against its current
            // set and watches each target NonRecursive. Empty set → no
            // scanning at all.
            .route("/workspaces/{ws_id}/fs-watch", put(set_workspace_fs_watch))
            .route("/workspaces/tree", get(list_tree))
            .route("/workspaces/find", get(find_files))
            .route("/workspaces/search", get(search_files))
            // ADR-078: git endpoints for the Desktop Git Status Bar. Git
            // executes in the Runtime (workspace owner, ADR-009 v2) via
            // the system git CLI — the Gateway reverse-proxies
            // `/api/agents/{id}/git/*` here and never touches the
            // filesystem. All handlers are thin protocol converters over
            // the GitQueryService trait. `/git/revert` is the single
            // deliberate WRITE (discard uncommitted changes).
            .route("/git/status", get(git_status))
            .route("/git/diff", get(git_diff))
            .route("/git/log", get(git_log))
            .route("/git/revert", post(git_revert))
            // ADR-081 §4.1: six-source global search aggregation endpoint.
            // Scopes: conversation / memory / file / git (doc/project stay
            // in their owner processes — Gateway stays zero-business).
            .route("/search", get(global_search::global_search))
            // ADR-093: LLM summary over the Desktop's bounded candidate
            // snapshot. POST is proxied transparently by the Gateway.
            .route("/search/summary", post(search_summary::search_summary))
            // ADR-081 §4.2 self-heal: purge + rebuild the conversation
            // vector index from the JSONL history (no Gateway route —
            // Runtime-internal admin endpoint, like /memory/rebuild-embeddings).
            .route("/conversation/index/rebuild", post(rebuild_conversation_index))
            // 2 NEW workspace file/dir resources, REST-style (ADR-034 §11.2 #6-9).
            // One path per resource; HTTP method dispatches the operation:
            //   GET    /workspaces/file — read  → JSON {content,size,mimeType}
            //   POST   /workspaces/file — create → body {path} (409 on dup)
            //   PUT    /workspaces/file — write  → body {content} (404 on miss)
            //   DELETE /workspaces/file — remove → body {path}
            //   POST   /workspaces/dir  — create → body {path}
            //   DELETE /workspaces/dir  — remove → body {path}
            // All share the `resolve_workspace_root` + canonicalize path-
            // traversal guard via `resolve_within_workspace` so the contract
            // is uniform across reads and writes.
            .route(
                "/workspaces/file",
                get(read_workspace_file)
                    .post(create_workspace_file)
                    .put(write_workspace_file)
                    .delete(delete_workspace_file),
            )
            // ADR-055 L2-7: raw-bytes read for the Gateway's HTML preview
            // iframe (the Gateway reverse-proxies `/workspace-files/…`
            // here). Serves verbatim bytes + Content-Type — unlike the
            // JSON envelope above.
            .route("/workspaces/raw/{*path}", get(read_workspace_raw))
            .route(
                "/workspaces/dir",
                post(create_workspace_dir).delete(delete_workspace_dir),
            )
            // Workspace copy/rename (ADR-040 + ADR-034 §11.2 — these
            // were previously gateway-direct and silently broken for
            // additional workspaces because the gateway-side
            // `resolve_workspace_root` read a never-populated in-memory
            // field). The runtime owns the workspace config on disk
            // (`<work_dir>/config/agent_workspaces.json`) so resolution
            // works for every workspace ID, including the agent home.
            .route("/workspaces/copy", post(copy_workspace_item))
            .route("/workspaces/rename", post(rename_workspace_item))
            // ADR-040 follow-up: agent panel endpoints now route
            // through UseCase traits (AgentConfigService +
            // AgentToolsService). Each handler is a thin protocol
            // converter — see `put_agent_config` /
            // `put_agent_builtin_tools` for the slot-first pattern.
            //
            //   /agents/{id}/config          — per-agent runtime
            //                                  config (`agent_config.json`)
            //   /agents/{id}/builtin-tools   — builtin tool enable flags
            //                                  (`agent_tools.json`)
            //   /agents/{id}/mcp-servers     — MCP active selection
            //                                  (`agent_mcp.json`)
            //
            // `/agents/{id}/tools` (GET only) remains a read-only merge
            // of the two Tools-panel files — see `get_agent_tools`.
            .route(
                "/agents/{id}/config",
                get(get_agent_config).put(put_agent_config),
            )
            .route(
                "/agents/{id}/builtin-tools",
                get(get_agent_builtin_tools).put(put_agent_builtin_tools),
            )
            .route("/agents/{id}/tools", get(get_agent_tools))
            .route("/agents/{id}/status", get(get_agent_status))
            // Win11-MCP-ToolsBugFix: see comment block above `get_agent_mcp_servers`.
            // These two routes power the per-agent activation toggles in the
            // Desktop Tools panel. They are pure persistence endpoints —
            // no in-memory side effects on active sessions (matches the
            // "next-session effective" contract documented in mqtt.md §3.5).
            .route(
                "/agents/{id}/mcp-servers",
                get(get_agent_mcp_servers).put(put_agent_mcp_servers),
            )
            // ADR-090: first-enable ensure status (probing / installing /
            // usable / failed + reason). The Desktop Tools panel polls this
            // after opening a preset server's toggle.
            .route("/agents/{id}/mcp-ensure/{name}", get(get_agent_mcp_ensure))
            // ADR-069: per-tool MCP opt-in. GET returns the **full**
            // per-server tool list (name + enabled + description) that
            // the Desktop Tools panel renders directly — the frontend
            // never maintains its own tool list or defaults. PUT
            // persists the complete list verbatim to
            // agent_mcp_tools.json; a session-level reconnect picks up
            // the new filter on next connect.
            .route(
                "/agents/{id}/mcp-tools",
                get(get_agent_mcp_tools).put(put_agent_mcp_tools),
            )
            // ADR-040 follow-up: provider list endpoint — reads from
            // agent_provider.json (persisted by MQTT handler on every
            // acowork/global/providers retained update). The Gateway
            // proxies GET /api/agents/{id}/providers here so the
            // frontend can verify what the Runtime actually has.
            .route("/agents/{id}/providers", get(get_agent_providers))
            // N1: Consolidation status - reports timer idle, pending count.
            // N2: RAG status - reports whether RAG is configured.
            // N3: RAG query - direct query bypassing LLM (for debugging).
            .route(
                "/memory/consolidation/status",
                get(get_consolidation_status),
            )
            .route("/agents/{id}/rag/status", get(get_rag_status))
            .route("/agents/{id}/rag/query", post(post_rag_query))
            // Shell risk rules — read effective content or write user override.
            .route(
                "/agents/{id}/shell-risk-rules",
                get(get_shell_risk_rules).put(put_shell_risk_rules),
            )
            // ADR-048: debug protocol HTTP routes — thin wrappers
            // around DebugService. Mounted under /api/debug/*. Phase A
            // starts the HTTP server before DebugService is wired up,
            // so handlers return 503 until Phase B populates the slot.
            .merge(crate::http::debug::debug_routes())
            // Global body-size cap. See `GLOBAL_BODY_LIMIT` for why we
            // override axum's 2 MiB default at the root of the router.
            // Each per-route service-layer check (e.g.
            // `AttachmentService::PayloadTooLarge`) remains the source
            // of truth for user-facing limits.
            .layer(DefaultBodyLimit::max(GLOBAL_BODY_LIMIT))
            // ADR-063: mount the 3 prompt override routes
            // (`/agents/{id}/prompts`, `/agents/{id}/prompts/{name}` GET+PUT)
            // via `prompts::prompts_routes`. Lives in its own sub-router so
            // the 3 handlers + their tests are self-contained and don't
            // bloat this 6800-line server.rs. Merging must happen BEFORE
            // `.with_state(state)` — axum applies state only to the
            // primary router; a `.merge()` after `.with_state()` is a
            // type error.
            .merge(crate::http::prompts::prompts_routes())
            // ADR-009 §V-A: the Runtime owns `{package_dir}/skills/`. The
            // Gateway reverse-proxies `/api/agents/{id}/skills*` here so a
            // single SKILL.md parser survives (no Gateway-side second copy).
            .merge(crate::http::skills::skills_routes())
            // ADR-009 §V-B: avatar reads resolve against the agent package
            // ({package_dir}/assets + manifest.avatar), which lives next to
            // the Runtime — not on the Gateway.
            .merge(crate::http::avatar::avatar_routes())
            .with_state(state);
        // Diagnostic: confirms the migration route was wired into the Router
        // during this build. Runs once at server boot. If this log never
        // appears, the .route() call above was lost in a refactor and
        // Gateway start-migration requests will silently 404.
        tracing::info!(
            target: "migration_diag",
            "runtime HTTP server: registered POST /memory/rebuild-embeddings"
        );

        // Bind to the Node-allocated loopback port (ADR-055 §6.4);
        // `127.0.0.1:0` = random port (pre-node-topology behaviour).
        let bind_addr = if bind_port == 0 {
            "127.0.0.1:0".to_string()
        } else {
            format!("127.0.0.1:{}", bind_port)
        };
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .map_err(|e| RuntimeHttpServerError::Bind(format!("Failed to bind: {}", e)))?;

        let listen_addr = listener.local_addr().map_err(|e| {
            RuntimeHttpServerError::Bind(format!("Failed to get local addr: {}", e))
        })?;

        let port = listen_addr.port();

        tracing::info!(
            addr = %listen_addr,
            port,
            "Runtime HTTP server started (for Gateway reverse proxy)"
        );

        let handle = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "Runtime HTTP server error");
            }
        });

        Ok(Self {
            listen_addr,
            port,
            workspace_watchers: workspace_watchers.clone(),
            mcp_notifier: mcp_notifier.clone(),
            _handle: handle,
        })
    }
}

// ── Handlers ───────────────────────────────────────────────────────────

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    agent_id: String,
    degraded_reasons: Vec<String>,
}

async fn health(State(state): State<HttpState>) -> Json<HealthResponse> {
    let degraded = state
        .degraded_reasons
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(HealthResponse {
        status: "ok",
        agent_id: state.agent_id,
        degraded_reasons: degraded,
    })
}

/// Query parameters for `GET /sessions`.
#[derive(Debug, Deserialize)]
struct ListSessionsQuery {
    /// 1-based page number (default 1).
    #[serde(default)]
    page: Option<u32>,
    /// Page size (default 20, capped at 200).
    #[serde(default)]
    size: Option<u32>,
}

/// `GET /sessions` — full session list.
///
/// Reads the session-meta store (ADR-082 §4 step 3: SQLite; the legacy
/// per-session meta files under `workspace/conversations/meta/` are gone).
/// Supports `page` / `size` pagination; results are returned sorted by
/// `last_active_at` descending.
///
/// ADR-028 / ADR-066 (regression t-83afab47): the response top level
/// NO LONGER carries `agent_total_*` fields — those live exclusively
/// on the live `context_usage` push path (every MQTT `session_state`
/// and `ChunkEvent::ContextUsage` carries them via `patch_agent_totals`).
/// `list_sessions` still performs the cold-start merge into the live
/// counters as a side effect (so the first post-restart push reflects
/// historical totals); the merged value is intentionally not echoed
/// back on the wire, and the desktop no longer maintains a stale
/// `agentTokenTotals` fallback that drift-flickers against the push.
///
/// This is the backend for `GET /api/agents/{id}/sessions` via the
/// Gateway reverse proxy.
///
/// ADR-085 D4: an unfilled `session_metadata` slot is answered with
/// `503 {"error":"session_not_ready"}` + `Retry-After: 2` — the
/// startup window must be protocol-level distinguishable from "this
/// agent has zero sessions" (a bare 404 here is what the Desktop used
/// to misread as "no session" and answer with a spurious
/// `POST /sessions`, leaving orphan sessions behind).
async fn list_sessions(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<ListSessionsQuery>,
) -> Result<Json<serde_json::Value>, SessionHttpError> {
    let page = query.page.unwrap_or(1).max(1);
    let size = query.size.unwrap_or(20).clamp(1, 200);
    let scope = crate::http::session_control::scope_from_headers(&headers);

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.session_metadata.lock().await;
    let svc = svc.as_ref().ok_or_else(session_not_ready)?;
    let resp = svc
        .list_sessions(page, size, &scope)
        .await
        .map_err(|_| session_http_error(StatusCode::INTERNAL_SERVER_ERROR))?;
    Ok(Json(
        serde_json::to_value(resp).map_err(|_| session_http_error(StatusCode::INTERNAL_SERVER_ERROR))?,
    ))
}

/// Error for the session read endpoints, rendered as a status + optional
/// `Retry-After` header + a JSON body (`{"error": "<code>"}`).
///
/// Modelled as a small discriminant enum rather than the 3-tuple it renders
/// to, so the `Result<_, SessionHttpError>` error variant stays a couple of
/// bytes and does not trip `clippy::large_error_types`. The heavy
/// `(StatusCode, HeaderMap, Json)` response is built only at the
/// `IntoResponse` boundary, not carried through every early-return `?`.
enum SessionHttpError {
    /// `503 session_not_ready` + `Retry-After: 2` (startup window, ADR-085 D4).
    NotReady,
    /// `404 no_session` — ready, and there is no session. The same body is
    /// used when the session exists but this caller may not read it, so the
    /// two stay indistinguishable on the wire (no existence leak).
    NoSession,
    /// Any other status; body is `{"error": <canonical_reason>}`.
    Status(StatusCode),
}

impl IntoResponse for SessionHttpError {
    fn into_response(self) -> axum::response::Response {
        match self {
            Self::NotReady => {
                let mut headers = HeaderMap::new();
                headers.insert(
                    header::RETRY_AFTER,
                    axum::http::HeaderValue::from_static("2"),
                );
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    headers,
                    Json(serde_json::json!({ "error": "session_not_ready" })),
                )
                    .into_response()
            }
            Self::NoSession => (
                StatusCode::NOT_FOUND,
                HeaderMap::new(),
                Json(serde_json::json!({ "error": "no_session" })),
            )
                .into_response(),
            Self::Status(status) => (
                status,
                HeaderMap::new(),
                Json(serde_json::json!({
                    "error": status.canonical_reason().unwrap_or("error")
                })),
            )
                .into_response(),
        }
    }
}

fn session_http_error(status: StatusCode) -> SessionHttpError {
    SessionHttpError::Status(status)
}

/// ADR-085 D4: the "not yet, ask again shortly" answer for session
/// interfaces — `503 {"error":"session_not_ready"}` + `Retry-After: 2`.
fn session_not_ready() -> SessionHttpError {
    SessionHttpError::NotReady
}

/// ADR-085 D4: the honest "ready, and there is no session" answer.
fn no_session() -> SessionHttpError {
    SessionHttpError::NoSession
}

/// ADR-085 D4 ("扫描未完成" row): whether an empty `latest_session`
/// cache is a definitive "zero sessions" verdict (→ 404) or merely
/// "not known yet" (→ 503 session_not_ready).
///
/// The definitive 404 requires the startup scan to have landed. The
/// process-level proxy for that is the SESSIONS_READY lifecycle stamp:
/// `publish_lifecycle(SessionsReady)` runs only after the scan seeded
/// `latest_session`, so before the stamp an empty cache can still mean
/// "sessions exist on disk but the scan has not found them yet" — the
/// orphan-session seed this ADR removes. FAILED is deliberately NOT
/// definitive: the session subsystem never became ready, so the honest
/// answer stays 503 (callers must not create sessions against it).
///
/// Standalone mode has no MQTT client and therefore no lifecycle
/// authority (ADR-085 §10 Q3: the state machine only constrains
/// Gateway mode); it keeps the immediate-404 behavior.
pub(crate) fn empty_cache_is_definitive(cur_lifecycle: u8, has_mqtt_client: bool) -> bool {
    !has_mqtt_client
        || cur_lifecycle == acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady as u8
}

/// Read the process lifecycle stamp through the late-bind MQTT slot.
/// Returns `(has_client, current_lifecycle)`; the slot is empty in
/// standalone mode and during the pre-connect window of Phase A.
async fn mqtt_lifecycle_stamp(state: &HttpState) -> (bool, u8) {
    let slot = state.mqtt_client.lock().await;
    match slot.as_ref() {
        Some(client) => (true, client.lock().await.current_lifecycle()),
        None => (false, 0),
    }
}

/// `GET /sessions/latest` — single latest session.
///
/// Reads from the shared `latest_session` Arc (updated by SessionManager
/// and seeded by the startup scan), so it always reflects the
/// authoritative latest session without any file-system scanning.
///
/// ADR-085 D4 status semantics:
/// - `session_metadata` slot unfilled → `503 session_not_ready`
///   (+ `Retry-After: 2`). The slot check is the protocol-level
///   self-defense for callers that ignore lifecycle state.
/// - slot filled but the startup scan has not landed yet (no
///   SESSIONS_READY stamp) and the cache is empty → also `503
///   session_not_ready`: an empty cache before the scan is "not known
///   yet", never "zero sessions" (see `empty_cache_is_definitive`).
/// - ready, no session (or not readable by this scope) →
///   `404 {"error":"no_session"}` — a legitimate zero-session agent.
///
/// This is the backend for `GET /api/agents/{id}/latest-session` via Gateway proxy.
async fn get_latest_session(
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, SessionHttpError> {
    let scope = crate::http::session_control::scope_from_headers(&headers);

    // ADR-085 D4: readiness gate — see `list_sessions` for the rationale.
    {
        let svc = state.session_metadata.lock().await;
        if svc.is_none() {
            return Err(session_not_ready());
        }
    }

    let latest = state
        .latest_session
        .read()
        .map_err(|_| session_http_error(StatusCode::INTERNAL_SERVER_ERROR))?
        .clone();

    let Some((session_id, title)) = latest else {
        // ADR-085 D4: an empty cache is only a definitive "no session"
        // once the startup scan has landed (SESSIONS_READY stamp);
        // before that, answer 503 so no caller can misread the boot
        // window as "zero sessions" and create an orphan.
        let (has_client, cur) = mqtt_lifecycle_stamp(&state).await;
        if !empty_cache_is_definitive(cur, has_client) {
            return Err(session_not_ready());
        }
        return Err(no_session());
    };

    // ADR-076 §决策 4: the cached value is agent-wide (written at startup),
    // so it can name a session this caller may not read. Answer 404 and let
    // the client fall back to the filtered list rather than leak the id —
    // ponytail: for a non-owner this costs one extra round trip; the
    // alternative is a full scan on every startup call.
    // ADR-085 D4: the body is the same `no_session` as the zero-session
    // case, so the two remain indistinguishable.
    if let Err((status, _)) =
        crate::http::session_control::authorize_read(&state, &session_id, &scope)
    {
        return Err(if status == StatusCode::NOT_FOUND {
            no_session()
        } else {
            session_http_error(status)
        });
    }

    Ok(Json(serde_json::json!({
        "session_id": session_id,
        "title": title,
    })))
}

/// Query parameters for `GET /sessions/{sid}/messages`.
#[derive(Debug, Deserialize)]
struct GetMessagesQuery {
    /// Offset from the **oldest** end, in **raw entries** (one JSONL line each).
    /// 0 = first (oldest) raw entry.  See
    /// [`crate::conversation::PaginatedMessages`] for the contract.
    #[serde(default)]
    offset: Option<u64>,
    /// Maximum number of **raw entries** to return (default 50, capped at 500).
    /// A raw entry is one JSONL line — a single user / assistant /
    /// thought / tool_call / tool_result row. Display-group collapsing
    /// is a frontend UI abstraction and is not visible here.
    #[serde(default)]
    limit: Option<u32>,
    /// ADR-050: when true, the window is anchored to the **tail** of the
    /// conversation (the latest `limit` entries), regardless of `offset`.
    /// Used by the frontend's initial-load code path which doesn't yet
    /// know `total`.  Ignored when total == 0.
    #[serde(default)]
    tail: Option<bool>,
}

// ── Session config (ADR-047) ───────────────────────────────────

/// `GET /sessions/{sid}/config` - read current session config.
///
/// ADR-047: returns a `SessionConfigSnapshot` containing model, provider,
/// workspace_id, reasoning_effort, temperature, and title.
async fn get_session_config(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-076 §决策 4: config exposes model / provider / workspace_id /
    // title — as much a leak as the messages themselves.
    crate::http::session_control::authorize_read(
        &state,
        &sid,
        &crate::http::session_control::scope_from_headers(&headers),
    )?;

    let svc = state.session_config.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session config service not ready"})),
    ))?;
    let snapshot = svc.get_config(&sid).await.map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "Failed to read session config");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    Ok(Json(serde_json::to_value(&snapshot).unwrap_or_default()))
}

/// `PUT /sessions/{sid}/config` - apply a config change.
///
/// ADR-047: accepts a `SessionConfigDelta` as JSON body. Each field is
/// optional (null or omitted means "unchanged"). Persistence is immediate;
/// LLM-side effects are deferred to the next inference turn.
///
/// ADR-074: `context_window` accepts `0` (or `null` / absent) as "clear
/// the override" and `FLOOR..=CEILING` as "set". Out-of-range values are
/// rejected with 400 — never silently clamped (§3.3, D6).
async fn put_session_config(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    Json(delta): Json<crate::agent::session_config::SessionConfigDelta>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-076 §决策 4: writing config is a write — a public session is
    // readable by all but switchable (model / workspace) only by its owner.
    crate::http::session_control::authorize_write(
        &state,
        &sid,
        &crate::http::session_control::scope_from_headers(&headers),
    )?;

    // ADR-074 D6: out-of-range `context_window` → 400 (no silent clamp).
    // `0` is legal (clear); validity is judged by the single
    // `is_valid_context_window` point shared with the resolution chain.
    if let Some(cw) = delta.context_window
        && cw != 0
        && !crate::agent::session_config::is_valid_context_window(cw)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!(
                    "context_window out of valid range: {}. Valid: {}..={} (0 = clear override)",
                    cw,
                    crate::agent::session_config::CONTEXT_WINDOW_FLOOR,
                    crate::agent::session_config::CONTEXT_WINDOW_CEILING,
                )
            })),
        ));
    }

    let svc = state.session_config.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session config service not ready"})),
    ))?;
    svc.apply_config(&sid, delta).await.map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "Failed to apply session config");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;
    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// `GET /sessions/{sid}/messages` - paginated message list for a session. — paginated message list for a session.
///
/// `GET /sessions/{sid}/messages` - paginated message list for a session.
///
/// ADR-040: delegates to [`SessionMetadataService::get_messages`] via
/// the late-bind slot. The handler is a thin protocol converter - all
/// file I/O, pagination, and ADR-035 D9.2 tool_result truncation live
/// in the UseCase impl.
async fn get_messages(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    Query(query): Query<GetMessagesQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-076 §决策 4: reading a private session you do not own is a 404,
    // not a 403 — a distinct status would confirm the session exists.
    crate::http::session_control::authorize_read(
        &state,
        &sid,
        &crate::http::session_control::scope_from_headers(&headers),
    )?;
    let svc = state.session_metadata.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session metadata service not ready"})),
    ))?;
    let resp = svc
        .get_messages(&sid, query.offset, query.limit, query.tail.unwrap_or(false))
        .await
        .map_err(|e| {
            tracing::warn!(
                session_id = %sid,
                error = %e,
                "Failed to read session messages"
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": e.to_string()})),
            )
        })?;
    Ok(Json(serde_json::to_value(resp).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "serialization failed"})),
        )
    })?))
}

/// `GET /memory/graph` — full memory graph (ADR-034 §11.2 #10).
///
/// Phase 3 (ADR-034): reads from the memory store via
/// [`memory_query::list_nodes`] (with no pagination), instead of the
/// legacy `.jsonl` file fallback. Returns all four user-visible memory
/// labels (`Episodic`/`Knowledge`/`Procedural`/`Autobiographical`) so
/// the desktop's Memory panel can render the full graph without having
/// to chain multiple paginated calls.
///
/// On a store that has not been initialised yet (HTTP server starts
/// before Phase B), the response is a well-formed empty graph. The
/// response shape (`agent_id` + `node_count` + `nodes` + `edges`) is
/// kept stable from the legacy handler so the desktop panel does not
/// have to branch on `node_count == 0`.
async fn get_memory_graph(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let query = crate::usecases::memory_query::MemoryNodeQuery {
        page: 1,
        size: 100_000,
        node_type: String::new(),
        sub_type: String::new(),
        keyword: String::new(),
        time_range: "all".to_string(),
    };
    let resp = svc
        .list_nodes(&query)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let nodes: Vec<serde_json::Value> = resp
        .nodes
        .iter()
        .map(|n| {
            serde_json::json!({
                "node_id": n.node_id,
                "node_type": n.node_type,
                "content": n.content,
                "confidence": n.confidence,
                "decay_score": n.decay_score,
                "created_at": n.created_at,
                "last_accessed_at": n.last_accessed_at,
                "access_count": n.access_count,
                "status": n.status,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "agent_id": state.agent_id,
        "node_count": nodes.len(),
        "nodes": nodes,
        "edges": [],
    })))
}

// ── Memory API (ADR-033 — Grafeo-backed) ─────────────────────

/// Query parameters for `GET /memory/nodes`.
///
/// Matches the `MemoryNodesQuery` contract documented in
/// `docs/zh/protocols/http.md` §7.7.
#[derive(Debug, Deserialize)]
struct ListNodesQuery {
    #[serde(default)]
    page: Option<u32>,
    #[serde(default)]
    size: Option<u32>,
    /// Filter by node type ("Episodic" / "Knowledge" / "Procedural" / "Autobiographical").
    #[serde(default, rename = "type")]
    node_type: Option<String>,
    /// Sub-classification filter (Knowledge / Autobiographical only):
    /// `Fact` | `Preference` | `Relation` | `Procedure` for Knowledge;
    /// `Identity` | `Capability` | `Limitation` | `Preference` | `History`
    /// | `Relationship` for Autobiographical. Ignored for other types.
    #[serde(default)]
    sub_type: Option<String>,
    /// Case-insensitive substring filter.
    #[serde(default)]
    keyword: Option<String>,
    /// Time-range bucket: "1h" / "1d" / "7d" / "30d" / "all".
    #[serde(default)]
    time_range: Option<String>,
    /// Semantic search query text (ADR-081 P1-1). Only used when `mode`
    /// is "vector" or "hybrid"; ignored otherwise.
    #[serde(default)]
    q: Option<String>,
    /// Semantic search mode: "vector" / "hybrid" / "keyword" (ADR-081
    /// P1-1). Empty string or "keyword" keeps the legacy substring path.
    #[serde(default)]
    mode: Option<String>,
}

/// `POST /conversation/index/rebuild` — reset the conversation vector
/// index and let the tailer rebuild from the JSONL history (ADR-081
/// §4.2 "索引目录独立，可删重建"). The index is append-only-derivable,
/// so this is the self-heal path after corruption or an embedding
/// dimension change — the next indexer sweep (≤2s) re-indexes from
/// scratch and `/search` reports `indexing: true` until caught up.
async fn rebuild_conversation_index(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let guard = state
        .conversation_index
        .read()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(index) = guard.as_ref() else {
        return Ok(Json(serde_json::json!({"status": "not_initialized"})));
    };
    index.rebuild();
    Ok(Json(serde_json::json!({"status": "rebuilding"})))
}

/// `GET /memory/nodes` — list memory nodes (paginated, filtered, searched).
async fn get_memory_nodes(
    State(state): State<HttpState>,
    Query(params): Query<ListNodesQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-081 P1-1: `?q=&mode=vector|hybrid` switches to semantic search.
    // `mode=keyword` (or no `q`) keeps the legacy substring path below.
    let semantic_mode = params.mode.as_deref().unwrap_or("").trim();
    if semantic_mode != "keyword" && !semantic_mode.is_empty() {
        let Some(q) = params.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Json(serde_json::json!({"nodes": [], "total": 0, "page": 1, "size": 0, "model_dim": 0})));
        };
        // Clone the provider out of AgentCore first: the read guard is not
        // Send, so it cannot be held across the async `embed` await.
        let embedding = global_search::embed_query(&state, q).await;
        let query = crate::usecases::memory_query::SemanticMemoryQuery {
            query_text: q.to_string(),
            mode: semantic_mode.to_string(),
            limit: params.size.unwrap_or(20) as usize,
            embedding,
        };
        let svc = state.memory_query.lock().await;
        let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
        let resp = svc
            .semantic_search(&query)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        return Ok(Json(
            serde_json::to_value(resp).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
        ));
    }

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let query = crate::usecases::memory_query::MemoryNodeQuery {
        page: params.page.unwrap_or(1),
        size: params.size.unwrap_or(20),
        node_type: params.node_type.unwrap_or_default(),
        sub_type: params.sub_type.unwrap_or_default(),
        keyword: params.keyword.unwrap_or_default(),
        time_range: params.time_range.unwrap_or_default(),
    };
    let resp = svc
        .list_nodes(&query)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::to_value(resp).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}

/// `GET /memory/nodes/{nid}` — single memory node detail (ADR-034 §11.2 #12).
///
/// Added in Phase 3 so the Memory panel can render node detail views
/// without having to download the entire graph. The business logic
/// lives in [`memory_query::get_node`] so the same shape can be
/// surfaced over gRPC in the future.
async fn get_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let node_id: u64 = nid.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let val = svc
        .get_node(node_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(val))
}

/// `GET /memory/stats` — memory statistics.
///
/// Returns the full `MemoryStats` contract (defined in
/// `crate::usecases::memory_query`). The same contract is produced by
/// both the ADR-040 service path and the pre-ADR-040 fallback path;
/// keeping the struct `Serialize` and using `serde_json::to_value`
/// directly avoids any drift between the two paths.
async fn get_memory_stats(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let stats = svc
        .get_stats()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::to_value(&stats).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
    ))
}

/// `DELETE /memory/nodes/{nid}` — delete a memory node.
async fn delete_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let node_id: u64 = nid.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    // ADR-040: usecase trait is the sole implementation path.
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    svc.delete_node(node_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(
        serde_json::json!({"deleted": true, "node_id": node_id}),
    ))
}

/// Request body for `POST /memory/nodes`.
///
/// `properties` is a flat `key → JSON value` map. The implementation is
/// responsible for serialising values into the underlying store (see
/// [`crate::http::memory_query::json_to_grafeo_value`]).
#[derive(Deserialize)]
struct CreateMemoryNodeBody {
    label: String,
    #[serde(default)]
    properties: std::collections::HashMap<String, serde_json::Value>,
}

/// `POST /memory/nodes` — create a new memory node.
///
/// Returns `{"node_id": <u64>, "label": "..."}` on success. The handler
/// relies on the ADR-040 usecase trait so memory-store initialisation
/// timing (the Runtime HTTP server starts before Phase B) reports a clean
/// 503 instead of a panic.
async fn create_memory_node(
    State(state): State<HttpState>,
    Json(body): Json<CreateMemoryNodeBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use crate::usecases::memory_query::CreateMemoryNodeInput;
    let input = CreateMemoryNodeInput {
        label: body.label,
        properties: body.properties,
    };
    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or(StatusCode::SERVICE_UNAVAILABLE)?;
    let node_id = svc
        .create_node(&input)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "node_id": node_id,
        "label": input.label,
    })))
}

/// Request body for `PUT /memory/nodes/{nid}`.
#[derive(Deserialize)]
struct UpdateMemoryNodeBody {
    #[serde(default)]
    properties: std::collections::HashMap<String, serde_json::Value>,
}

/// `PUT /memory/nodes/{nid}` — update (merge) properties on an existing node.
///
/// Returns 404 if the node is missing (mapped from
/// `RuntimeError::Memory("not found")`).
async fn update_memory_node(
    State(state): State<HttpState>,
    Path(nid): Path<String>,
    Json(body): Json<UpdateMemoryNodeBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let node_id: u64 = nid.parse().map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "node_id must be a non-negative integer"})),
        )
    })?;

    let svc = state.memory_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "memory store not ready"})),
    ))?;

    match svc.update_node(node_id, &body.properties).await {
        Ok(()) => Ok(Json(serde_json::json!({
            "updated": true,
            "node_id": node_id,
        }))),
        Err(crate::error::RuntimeError::Memory(msg)) if msg.contains("not found") => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": msg, "node_id": node_id})),
        )),
        Err(_) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "failed to update memory node"})),
        )),
    }
}

/// Request body for `POST /memory/rebuild-embeddings`.
///
/// Carries the new embedding model the Gateway has switched to. The
/// Runtime builds a temporary provider and re-embeds stored nodes,
/// rebuilding the HNSW indexes (Bug3 — restores the dimension-migration
/// feature lost in the gRPC→MQTT refactor).
#[derive(Debug, Deserialize)]
struct RebuildEmbeddingsBody {
    /// Embedding endpoint, e.g. `http://127.0.0.1:18080/v1`.
    #[serde(default)]
    endpoint: String,
    /// Embedding model ID.
    #[serde(default)]
    model_id: String,
    /// Target embedding dimension.
    dimension: usize,
    /// Which rows to re-embed. Absent on the wire means `All` — the only
    /// scope that existed before the memory panel grew targeted repair, and
    /// what a Gateway that predates the field effectively asks for.
    #[serde(default)]
    scope: acowork_memory::admin::RebuildScope,
}

/// Live progress of a background `/memory/rebuild-embeddings` run.
///
/// Serialized verbatim by `GET /memory/rebuild-progress` so the Gateway
/// can poll and forward `rebuilt/total` to the UI.
#[derive(Debug, Clone, Serialize)]
struct RebuildProgress {
    rebuilt: u64,
    total: u64,
    /// `true` once the background task has finished (success or error).
    done: bool,
    /// Set when the run fails; `None` while running or on success.
    error: Option<String>,
}

/// `POST /memory/rebuild-embeddings` — start a background re-embed and
/// return immediately (`202`).
///
/// The rebuild may take minutes on a large store; the client polls
/// `GET /memory/rebuild-progress` for `rebuilt/total` instead of holding
/// the connection open (a single synchronous call would trip the
/// Gateway's 30s proxy timeout and give the UI a bogus failure).
async fn rebuild_embeddings(
    State(state): State<HttpState>,
    Json(body): Json<RebuildEmbeddingsBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    tracing::info!(
        target: "migration_diag",
        endpoint = %body.endpoint,
        model_id = %body.model_id,
        dimension = body.dimension,
        ?body.scope,
        "rebuild_embeddings: received start request"
    );

    // ADR-040: usecase trait is the sole implementation path.
    let svc = {
        let guard = state.memory_query.lock().await;
        guard.clone()
    };
    let Some(svc) = svc else {
        tracing::warn!(
            target: "migration_diag",
            "rebuild_embeddings: memory_query service not ready, returning 503"
        );
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    // Reject a second concurrent rebuild of this store. The Gateway already
    // refuses to re-queue an agent whose migration is in flight, but this is
    // the layer that spawns the work, and a repeat would run two migrations
    // over the same rows at once - double the embedding cost, and both tasks
    // writing the single progress slot below, so the numbers the UI reads stop
    // meaning anything. A finished run (`done == true`) is not a collision:
    // re-running is how a partially-completed rebuild gets retried.
    {
        let slot = state
            .rebuild_progress
            .lock()
            .expect("rebuild progress mutex poisoned");
        if slot.as_ref().is_some_and(|p| !p.done) {
            tracing::warn!(
                target: "migration_diag",
                "rebuild_embeddings: rejected - a rebuild is already running in this runtime"
            );
            return Err(StatusCode::CONFLICT);
        }
    }

    // Mark the slot as running so GET /memory/rebuild-progress returns a
    // live (0/total) answer even before the first progress callback fires.
    *state
        .rebuild_progress
        .lock()
        .expect("rebuild progress mutex poisoned") = Some(RebuildProgress {
        rebuilt: 0,
        total: 0,
        done: false,
        error: None,
    });

    // Detached task: hold the service Arc and a clone of the shared state,
    // write progress from the migration's blocking thread, flip `done`
    // when finished. The HTTP handler returns before the work starts.
    let state_for_task = state.clone();
    // Clone the progress slot once; the `move` closure below consumes this
    // clone while the outer task still needs it to write the final state.
    let progress_slot = state_for_task.rebuild_progress.clone();
    let endpoint = body.endpoint.clone();
    let model_id = body.model_id.clone();
    let dimension = body.dimension;
    let scope = body.scope;
    tokio::spawn(async move {
        let progress: Arc<dyn Fn(u64, u64) + Send + Sync> = Arc::new(move |rebuilt, total| {
            *progress_slot
                .lock()
                .expect("rebuild progress mutex poisoned") = Some(RebuildProgress {
                rebuilt,
                total,
                done: false,
                error: None,
            });
        });
        let report = svc
            .rebuild_embeddings_with_progress(&endpoint, &model_id, dimension, scope, Some(progress))
            .await;
        let (rebuilt, total, error) = match report {
            Ok(r) => {
                tracing::info!(
                    target: "migration_diag",
                    total_scanned = r.total_scanned,
                    rebuilt = r.rebuilt,
                    skipped_no_embedding = r.skipped_no_embedding,
                    errors = r.errors,
                    "rebuild_embeddings: completed"
                );
                (r.rebuilt, r.total_scanned, None)
            }
            Err(e) => {
                let msg = e.to_string();
                tracing::warn!(
                    target: "migration_diag",
                    error = %msg,
                    "rebuild_embeddings: background task failed"
                );
                (0, 0, Some(msg))
            }
        };
        *state_for_task
            .rebuild_progress
            .lock()
            .expect("rebuild progress mutex poisoned") = Some(RebuildProgress {
            rebuilt,
            total,
            done: true,
            error,
        });
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"status": "started"})),
    ))
}

/// `GET /memory/rebuild-progress` — current state of the background
/// rebuild started by `POST /memory/rebuild-embeddings`.
async fn rebuild_progress(
    State(state): State<HttpState>,
) -> Result<Json<RebuildProgress>, StatusCode> {
    let slot = state
        .rebuild_progress
        .lock()
        .expect("rebuild progress mutex poisoned");
    match slot.as_ref() {
        Some(p) => Ok(Json(p.clone())),
        None => Err(StatusCode::NOT_FOUND),
    }
}

// ── Session detail (ADR-034 §11.2 #4 — panel 4) ──────────────

/// `GET /sessions/{sid}` - full session detail (ADR-034 \u00a711.2 #4, panel 4).
///
/// ADR-040: delegates to [`SessionMetadataService::get_session`] via
/// the late-bind slot. The impl reads `meta.json` and merges the live
/// `SharedSessionSnapshots` state into a single `SessionDetail` struct;
/// this handler is a thin protocol converter that returns 404 when the
/// session is completely unknown (no meta AND no live snapshot).
async fn get_session(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-076 §决策 4: same hidden-404 rule as `get_messages`.
    crate::http::session_control::authorize_read(
        &state,
        &sid,
        &crate::http::session_control::scope_from_headers(&headers),
    )?;
    let svc = state.session_metadata.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "session metadata service not ready"})),
    ))?;
    let detail = svc.get_session(&sid).await.map_err(|e| {
        tracing::warn!(session_id = %sid, error = %e, "Failed to read session detail");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e.to_string()})),
        )
    })?;

    // 404 only when both meta and live_state are absent - either
    // present is enough to return 200 (the meta card alone is useful
    // for sessions that have been written but never observed).
    if detail.created_at.is_empty() && detail.live_state.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "session not found"})),
        ));
    }

    // ADR-047: response no longer includes config fields (model,
    // provider, workspace_id, title, reasoning_effort, temperature).
    // Config is served by `GET /sessions/{sid}/config`. The `meta`
    // object here carries only state-level metadata.
    let meta = serde_json::json!({
        "session_id": detail.session_id,
        "created_at": detail.created_at,
        "last_active_at": detail.last_active_at,
        "message_count": detail.message_count,
    });
    Ok(Json(serde_json::json!({
        "session_id": sid,
        "meta": meta,
        "live_state": detail.live_state,
    })))
}

// ── Workspace Query Handlers (ADR-040) ─────────────────────────────────────
//
// These handlers are thin shells around [`WorkspaceQueryService`] — they
// parse the axum extractors, call the trait method, and serialize the
// result. All filesystem / path-resolution logic lives in the usecase
// implementation so HTTP handlers stay free of I/O concerns.

/// `GET /workspaces` — list workspace directories from
/// `agent_workspaces.json`. Returns `{ agent_id, workspaces: [...] }`.
async fn list_workspaces(
    State(state): State<HttpState>,
) -> Result<
    Json<crate::usecases::workspace_query::WorkspacesListResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.workspace_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.list_workspaces()
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/tree` — list directory contents for a workspace.
async fn list_tree(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::ListTreeParams>,
) -> Result<
    Json<crate::usecases::workspace_query::TreeResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.workspace_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.list_tree(&params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/file?path=…` — read a UTF-8 text file.
async fn read_workspace_file(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::FilePathQuery>,
) -> Result<
    Json<crate::usecases::workspace_query::WorkspaceFileDto>,
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.workspace_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    let read_params: crate::usecases::workspace_query::ReadFileParams = (&params).into();
    svc.read_file(&read_params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/raw/{path}?workspace_id=…` — serve a file's raw bytes
/// (ADR-055 L2-7). The Gateway reverse-proxies its `/workspace-files/…`
/// HTML-preview endpoints here instead of reading the workspace
/// filesystem itself. Returns `Content-Type` + verbatim bytes so the
/// preview iframe's `<img>` / `<link>` / `<script>` sub-resources resolve.
async fn read_workspace_raw(
    State(state): State<HttpState>,
    Path(file_rel_path): Path<String>,
    Query(q): Query<crate::usecases::workspace_query::RawFileQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    let params = crate::usecases::workspace_query::ReadFileParams {
        workspace_id: q.workspace_id,
        path: file_rel_path,
    };
    let dto = svc
        .read_file_raw(&params)
        .await
        .map_err(workspace_error_to_response)?;
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, dto.mime_type)],
        dto.bytes,
    ))
}

/// `GET /workspaces/find` — fuzzy filename search (Ctrl+P-style palette).
async fn find_files(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::FindFilesParams>,
) -> Result<
    Json<crate::usecases::workspace_query::FindResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.workspace_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.find_files(&params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

/// `GET /workspaces/search` — ripgrep-style content search (Ctrl+Shift+F).
async fn search_files(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::SearchFilesParams>,
) -> Result<
    Json<crate::usecases::workspace_query::SearchResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.workspace_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.search_files(&params)
        .await
        .map(Json)
        .map_err(workspace_error_to_response)
}

// ── Git Query Handlers (ADR-078) ───────────────────────────────────────────
//
// Git operations for the Desktop Git Status Bar. All handlers route
// through the `GitQueryService` UseCase trait (ADR-040) — the Runtime
// executes read commands with `GIT_OPTIONAL_LOCKS=0` (strictly
// read-only, ADR-078 §1.3 invariant 1); `/git/revert` is the single
// user-requested write (discard uncommitted changes). The Gateway
// reverse-proxies `/api/agents/{id}/git/*` to these routes and never
// touches the filesystem (ADR-009 / ADR-055 red line).
//
// Pre-Phase-B HTTP probes receive 503 from the slot-first pattern,
// matching every other `*Service` handler in this crate.

/// Convert a [`GitError`] into the `(status, json)` tuple used by every
/// git handler. Mirrors `workspace_error_to_response`.
fn git_error_to_response(e: crate::usecases::GitError) -> (StatusCode, Json<serde_json::Value>) {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(serde_json::json!({"error": e.to_string()})))
}

/// `GET /git/status?workspace_id=…` — branch + changed-file list.
/// Non-repo workspaces return 200 with `{is_repo:false, error:"not_a_repo"}`
/// (explicit state, never silent degradation — ADR-078 §1.3 invariant 5).
async fn git_status(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::git_query::GitStatusParams>,
) -> Result<
    Json<crate::usecases::git_query::GitStatusResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.git_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "git service not ready"})),
    ))?;
    svc.status(&params)
        .await
        .map(Json)
        .map_err(git_error_to_response)
}

/// `GET /git/diff?workspace_id=…&path=…&cached=0|1` — two full texts for
/// the Monaco DiffEditor (original = HEAD, modified = worktree/index).
async fn git_diff(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::git_query::GitDiffParams>,
) -> Result<Json<crate::usecases::git_query::GitDiffResponse>, (StatusCode, Json<serde_json::Value>)>
{
    let svc = state.git_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "git service not ready"})),
    ))?;
    svc.diff(&params)
        .await
        .map(Json)
        .map_err(git_error_to_response)
}

/// `GET /git/log?workspace_id=…&path=…&limit=…` — commit history
/// (path-scoped when `path` is given, repository-wide otherwise).
async fn git_log(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::git_query::GitLogParams>,
) -> Result<Json<crate::usecases::git_query::GitLogResponse>, (StatusCode, Json<serde_json::Value>)>
{
    let svc = state.git_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "git service not ready"})),
    ))?;
    svc.log(&params)
        .await
        .map(Json)
        .map_err(git_error_to_response)
}

/// `POST /git/revert` — discard uncommitted changes of one path
/// (restore to HEAD; untracked files are deleted). The only write
/// endpoint in the git API family — the Desktop surfaces it behind a
/// destructive confirm dialog.
async fn git_revert(
    State(state): State<HttpState>,
    Json(body): Json<crate::usecases::git_query::GitRevertParams>,
) -> Result<Json<crate::usecases::git_query::GitRevertResponse>, (StatusCode, Json<serde_json::Value>)>
{
    let svc = state.git_query.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "git service not ready"})),
    ))?;
    svc.revert(&body)
        .await
        .map(Json)
        .map_err(git_error_to_response)
}

// ── Workspace Mutation Handlers (ADR-040) ──────────────────────────────────

/// `POST /workspaces` — add a new workspace entry.
async fn create_workspace(
    State(state): State<HttpState>,
    Json(body): Json<crate::usecases::workspace_mutation::WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "workspace service not ready"})),
        ))?;
        svc.create_workspace(body).await
    };
    match result {
        Ok(r) => {
            reload_workspace_resolver(&state);
            // ADR-058: reconcile watcher set with the reloaded resolver.
            sync_workspace_watchers(&state).await;
            // Fix-3 sanity check: after reload, the newly-created
            // workspace id MUST be visible via `find_by_id`. A `None`
            // here indicates a write/read race (e.g. another thread
            // clobbered `agent_workspaces.json` between `save_config`
            // and the reload). The Desktop would experience this as
            // "workspace disappeared" — surface it as 500 so the
            // abnormal condition is loud rather than silently returning
            // an entry that downstream `route_workspace_switch` calls
            // cannot resolve. See `desktop-onboarding-bugfix_154b7ff7.md`
            // §Fix 3.
            if let Some(new_id) = r
                .entry
                .as_ref()
                .and_then(|v| v.get("id"))
                .and_then(|v| v.as_str())
            {
                match state.workspace_resolver.read() {
                    Ok(resolver) => {
                        if resolver.find_by_id(new_id).is_none() {
                            tracing::error!(
                                ws_id = %new_id,
                                "Workspace mutation: reload succeeded but new id not visible — possible write/read race"
                            );
                            return Err((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({
                                    "error": "Workspace persisted but not visible after reload — possible write/read race",
                                    "ws_id": new_id,
                                })),
                            ));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "create_workspace: resolver lock poisoned during sanity check"
                        );
                    }
                }
            }
            Ok(Json(
                r.entry.unwrap_or(serde_json::json!({"created": r.ok})),
            ))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// Reload the shared `WorkspaceResolver` from disk after a successful
/// workspace mutation (create/update/delete).
///
/// The resolver is built once at Phase A and injected into both
/// `SessionManager` (for `route_workspace_switch` validation) and this
/// `HttpState`. Persisting a new workspace to `agent_workspaces.json`
/// without reloading leaves the in-memory resolver stale, so the new id
/// fails `find_by_id` and the switch falls back to `__agent_home__`.
fn reload_workspace_resolver(state: &HttpState) {
    let mut guard = match state.workspace_resolver.write() {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(error = %e, "reload_workspace_resolver: resolver lock poisoned");
            return;
        }
    };
    *guard =
        crate::tools::workspace_resolver::WorkspaceResolver::new(&state.work_dir.to_string_lossy());
    tracing::info!(
        work_dir = %state.work_dir.display(),
        allowed = guard.allowed_dirs().len(),
        "reloaded WorkspaceResolver after workspace mutation"
    );
}

/// Reconcile the workspace watcher set (ADR-058) with the freshly
/// reloaded resolver — starts watchers for new workspaces, stops
/// watchers for removed ones, and restarts watchers whose root moved.
///
/// MUST be called after [`reload_workspace_resolver`] so the resolver
/// the set syncs against is the on-disk truth.
async fn sync_workspace_watchers(state: &HttpState) {
    // Clone the resolver out of the std RwLock guard so the guard is
    // not held across the tokio Mutex await (Send requirement).
    let resolver = state
        .workspace_resolver
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut watchers = state.workspace_watchers.lock().await;
    watchers.sync_from_resolver(&resolver);
}

/// `PUT /workspaces/{ws_id}` — update an existing workspace entry.
async fn update_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<crate::usecases::workspace_mutation::WorkspaceEntryInput>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "workspace service not ready"})),
        ))?;
        svc.update_workspace(&ws_id, body).await
    };
    match result {
        Ok(r) => {
            // Path/access changes must be visible to the resolver so
            // `find_by_id` returns the updated entry for live sessions.
            reload_workspace_resolver(&state);
            // ADR-058: a path change restarts the watcher for this id.
            sync_workspace_watchers(&state).await;
            Ok(Json(
                r.entry.unwrap_or(serde_json::json!({"updated": r.ok})),
            ))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// `PUT /workspaces/{ws_id}/prompt-file` — set the prompt_file field.
async fn set_workspace_prompt_file(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<crate::usecases::workspace_mutation::PromptFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "workspace service not ready"})),
        ))?;
        svc.set_prompt_file(&ws_id, body).await
    };
    match result {
        Ok(_) => {
            // The prompt_file field must be visible to the shared resolver
            // immediately — `read_prompt_file` (used by
            // `update_session_workspace_context` at session creation) reads
            // from the in-memory resolver, not disk. Without this reload a
            // freshly-set AGENTS.md/CLAUDE.md stays invisible until the
            // runtime restarts (regression from adae896a, which moved
            // set_prompt_file from Gateway's push_and_cache to Runtime
            // without preserving the resolver refresh).
            reload_workspace_resolver(&state);
            Ok(Json(serde_json::json!({"ok": true, "ws_id": ws_id})))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// Request body for `PUT /workspaces/{ws_id}/fs-watch`.
///
/// `paths` are workspace-relative watch targets: `""` is the workspace
/// root, every other entry is a file or directory to watch one level
/// deep (`NonRecursive`). Full replace semantics — the Runtime diffs
/// against the current set.
#[derive(Debug, Deserialize)]
struct SetWorkspaceFsWatchBody {
    paths: Vec<String>,
}

/// `PUT /workspaces/{ws_id}/fs-watch` — demand-driven watch set update.
///
/// The frontend pushes exactly what it is showing: open editor tabs
/// (file-level) plus the expanded directories of the visible file tree
/// (one-level dir watch). An empty `paths` clears all watches → no
/// scanning at all. This replaces the old "watch the whole workspace
/// recursively on startup" model whose cost was unbounded (a large
/// repo with a `target/` of 130k+ files pegged CPU at 47-100%).
async fn set_workspace_fs_watch(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
    Json(body): Json<SetWorkspaceFsWatchBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // Validate the workspace id against the resolver so unknown ids
    // fail loudly instead of silently updating nothing.
    {
        let resolver = state
            .workspace_resolver
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if resolver.find_by_id(&ws_id).is_none() {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "workspace not found", "ws_id": ws_id})),
            ));
        }
    }

    // Path-traversal guard: reject absolute paths and `..` escapes.
    // Targets must stay within the workspace; the watcher joins them
    // under the workspace root.
    let mut paths: Vec<PathBuf> = Vec::with_capacity(body.paths.len());
    for raw in body.paths {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            // Empty string = workspace root itself.
            paths.push(PathBuf::from(""));
            continue;
        }
        let p = PathBuf::from(trimmed);
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "watch path must be workspace-relative and not escape the workspace",
                    "path": trimmed,
                })),
            ));
        }
        paths.push(p);
    }

    // Deduplicate while preserving a deterministic order (frontend may
    // send overlapping sets from tab + tree state).
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| seen.insert(p.clone()));

    let watchers = state.workspace_watchers.lock().await;
    watchers.set_watch_targets(&ws_id, paths);
    Ok(Json(serde_json::json!({"ok": true, "ws_id": ws_id})))
}

/// `DELETE /workspaces/{ws_id}` — remove a workspace entry.
async fn delete_workspace(
    State(state): State<HttpState>,
    Path(ws_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let result = {
        let svc = state.workspace_mutation.lock().await;
        let svc = svc.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error": "workspace service not ready"})),
        ))?;
        svc.delete_workspace(&ws_id).await
    };
    match result {
        Ok(_) => {
            // A deleted workspace must leave the resolver immediately —
            // otherwise `route_workspace_switch` still accepts its id.
            reload_workspace_resolver(&state);
            // ADR-058: also stop the deleted workspace's watcher.
            sync_workspace_watchers(&state).await;
            Ok(Json(serde_json::json!({"deleted": true, "ws_id": ws_id})))
        }
        Err(e) => Err(workspace_error_to_response(e)),
    }
}

/// `POST /workspaces/file` — create a new text file.
async fn create_workspace_file(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::CreateFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.create_file(
        body,
        qparams.get("workspace_id").map(|s| s.as_str()),
        qparams.get("path").map(|s| s.as_str()),
    )
    .await
    .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"created": r.ok}))))
    .map_err(workspace_error_to_response)
}

/// `PUT /workspaces/file` — overwrite an existing text file.
async fn write_workspace_file(
    State(state): State<HttpState>,
    Query(params): Query<crate::usecases::workspace_query::FilePathQuery>,
    Json(body): Json<crate::usecases::workspace_mutation::WriteFileBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    let mutation_query = crate::usecases::workspace_mutation::FilePathQuery {
        workspace_id: params.workspace_id.clone(),
        path: params.path.clone(),
    };
    svc.write_file(mutation_query, body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"written": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `DELETE /workspaces/file` — remove a file.
async fn delete_workspace_file(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::PathOnlyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.delete_file(qparams.get("workspace_id").map(|s| s.as_str()), body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"deleted": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `POST /workspaces/dir` — create a directory (recursive).
async fn create_workspace_dir(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::PathOnlyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.create_dir(qparams.get("workspace_id").map(|s| s.as_str()), body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"created": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `DELETE /workspaces/dir` — remove a directory recursively.
async fn delete_workspace_dir(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::PathOnlyBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    svc.delete_dir(qparams.get("workspace_id").map(|s| s.as_str()), body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"deleted": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `POST /workspaces/copy` — copy a file or directory tree.
///
/// Body: `{workspace_id?, source, dest}` (`workspace_id` may also live in
/// the querystring). Both paths must resolve under the same workspace
/// root and the destination's parent directory must already exist.
/// Returns 409 if `dest` already exists.
async fn copy_workspace_item(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::CopyMoveBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    // Prefer the querystring's workspace_id (matches the desktop
    // `workspaceStore` convention) but fall back to the body if absent.
    let mut body = body;
    if let Some(qs_ws) = qparams.get("workspace_id").filter(|s| !s.is_empty())
        && body.workspace_id.as_deref().unwrap_or("").is_empty()
    {
        body.workspace_id = Some(qs_ws.clone());
    }
    svc.copy_item(body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"copied": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// `POST /workspaces/rename` — atomically rename (move) a file or
/// directory inside the same workspace.
///
/// Body: `{workspace_id?, source, dest}`. `std::fs::rename` is atomic
/// on the same filesystem and falls back to copy+delete across
/// filesystem boundaries at the OS layer. Returns 409 if `dest` already
/// exists — explicit user intent (paste-as-copy's recursive dedupe runs
/// client-side) is responsible for picking an unused name.
async fn rename_workspace_item(
    State(state): State<HttpState>,
    Query(qparams): Query<std::collections::HashMap<String, String>>,
    Json(body): Json<crate::usecases::workspace_mutation::CopyMoveBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let svc = state.workspace_mutation.lock().await;
    let svc = svc.as_ref().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "workspace service not ready"})),
    ))?;
    let mut body = body;
    if let Some(qs_ws) = qparams.get("workspace_id").filter(|s| !s.is_empty())
        && body.workspace_id.as_deref().unwrap_or("").is_empty()
    {
        body.workspace_id = Some(qs_ws.clone());
    }
    svc.rename_item(body)
        .await
        .map(|r| Json(r.entry.unwrap_or(serde_json::json!({"renamed": r.ok}))))
        .map_err(workspace_error_to_response)
}

/// Map [`WorkspaceError`] → `(StatusCode, Json<...>)` response tuple.
///
/// HTTP status is taken from [`WorkspaceError::http_status`]; the error
/// string becomes the JSON `error` field. This is the single place
/// where usecase errors become HTTP responses — adding a new variant
/// to `WorkspaceError` only requires touching this function.
fn workspace_error_to_response(
    e: crate::usecases::WorkspaceError,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let msg = e.to_string();
    (status, Json(serde_json::json!({"error": msg})))
}

// ── Attachment storage handlers (ADR-046) ─────────────────────────────────
//
// All uploads land in the unified blob store at
// `<work_dir>/files/<document_id>` (no session subdirectory — JSONL is
// the per-session index). Both handlers route through the
// `AttachmentService` UseCase trait (ADR-040) so the HTTP layer never
// touches the filesystem directly. Pre-Phase-B HTTP probes receive
// `503 Service Unavailable` from the slot-first pattern, matching
// every other `*Service` handler in this crate.

use axum::extract::Multipart;

/// Convert an [`AttachmentError`] into the `(status, json)` tuple
/// used by every attachment handler. Mirrors the shape of
/// `workspace_error_to_response` so a future API-surface change only
/// touches this function.
fn attachment_error_to_response(
    e: crate::usecases::AttachmentError,
) -> (StatusCode, Json<serde_json::Value>) {
    use crate::usecases::AttachmentError as Ae;
    let status = match &e {
        Ae::ServiceUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        Ae::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
        Ae::InvalidDocumentId(_) => StatusCode::BAD_REQUEST,
        Ae::NotFound(_) => StatusCode::NOT_FOUND,
        Ae::Persistence(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(serde_json::json!({"error": e.to_string()})))
}

/// `POST /sessions/{sid}/files` — upload a file or image.
///
/// Accepts a multipart form with one `file` field. Optional fields:
///   - `width`, `height`: pixel dimensions for image uploads (the
///     desktop frontend reads them via `new Image()` before sending).
///     Both are optional; the renderer falls back to natural sizing
///     when absent.
///
/// The `session_id` path parameter is informational only — the blob is
/// stored globally (one directory for the whole agent) and the
/// per-session association lives in the JSONL `file_upload` /
/// `image_upload` system entry that the desktop writes via the
/// `document_ids` MQTT param.
async fn upload_file(
    State(state): State<HttpState>,
    Path(sid): Path<String>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Json<crate::usecases::UploadedFileResponse>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-076 §决策 4. The blob itself is stored agent-wide (see the note
    // above), so `sid` only gates *who may attach to this session* — enough
    // to stop a stranger uploading into someone else's conversation. The
    // matching read path has no session id to check against; see the
    // ceiling in ADR-076 §5.5.
    crate::http::session_control::authorize_write(
        &state,
        &sid,
        &crate::http::session_control::scope_from_headers(&headers),
    )?;

    let svc = state.attachment.lock().await.clone().ok_or_else(|| {
        attachment_error_to_response(crate::usecases::AttachmentError::ServiceUnavailable)
    })?;

    let mut filename: Option<String> = None;
    let mut format: Option<String> = None;
    let mut bytes: Option<Vec<u8>> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        attachment_error_to_response(crate::usecases::AttachmentError::Persistence(format!(
            "multipart: {e}"
        )))
    })? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                bytes = Some(
                    field
                        .bytes()
                        .await
                        .map_err(|e| {
                            attachment_error_to_response(
                                crate::usecases::AttachmentError::Persistence(format!(
                                    "read bytes: {e}"
                                )),
                            )
                        })?
                        .to_vec(),
                );
            }
            "format" => {
                format = Some(field.text().await.map_err(|e| {
                    attachment_error_to_response(crate::usecases::AttachmentError::Persistence(
                        format!("read format: {e}"),
                    ))
                })?);
            }
            "width" => {
                if let Ok(s) = field.text().await {
                    width = s.parse().ok();
                }
            }
            "height" => {
                if let Ok(s) = field.text().await {
                    height = s.parse().ok();
                }
            }
            _ => {
                // Ignore unknown fields (forwards-compat for future
                // client additions).
            }
        }
    }

    let bytes = bytes.ok_or_else(|| {
        attachment_error_to_response(crate::usecases::AttachmentError::Persistence(
            "missing `file` field".to_string(),
        ))
    })?;
    let filename = filename.unwrap_or_else(|| "upload".to_string());
    let format = format.unwrap_or_else(|| {
        // Fall back to the filename extension when the client didn't
        // supply an explicit format. The runtime does NOT sniff
        // content — clients are the source of truth.
        std::path::Path::new(&filename)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_default()
    });

    let resp = svc
        .upload_file(crate::usecases::UploadFileParams {
            filename,
            format,
            bytes,
            width,
            height,
        })
        .await
        .map_err(attachment_error_to_response)?;
    Ok(Json(resp))
}

/// `GET /files/{document_id}` — read a previously-uploaded blob.
///
/// Returns the raw bytes with `Content-Type` derived from the
/// extension the desktop supplies via the `format` query param (the
/// on-disk file has no extension — format lives in JSONL metadata).
async fn read_file(
    State(state): State<HttpState>,
    Path(document_id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<
    (
        StatusCode,
        [(axum::http::HeaderName, &'static str); 1],
        Vec<u8>,
    ),
    (StatusCode, Json<serde_json::Value>),
> {
    let svc = state.attachment.lock().await.clone().ok_or_else(|| {
        attachment_error_to_response(crate::usecases::AttachmentError::ServiceUnavailable)
    })?;

    let bytes = svc
        .read_file(&document_id)
        .await
        .map_err(attachment_error_to_response)?;

    // Derive Content-Type from the `format` query param (extension
    // only — no MIME sniffing server-side). Default to
    // application/octet-stream when missing.
    let format = params.get("format").map(String::as_str).unwrap_or("");
    let content_type = match format {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    };

    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, content_type)],
        bytes,
    ))
}

// ── Agent panel handlers (ADR-034 §11.2 #23-25) ────────────────────────
//
// Three GET endpoints powering the desktop panels Setup (1), Tools (3)
// and Agent Status (5). All read-only — mutations flow through the
// dedicated MQTT control commands.

/// `GET /agents/{id}/config` — Agent Setup panel data.
///
/// ADR-040 follow-up: persistence + the `{matches, config,
/// manifest_path, work_dir}` envelope construction live in
/// [`AgentConfigService::get_config`]. This handler is a thin
/// protocol converter that:
///   1. validates path `id` against this runtime's `instance_id` (ADR-034
///      cross-process routing guard — return an empty envelope
///      rather than 404 so a misconfigured Gateway doesn't blank
///      the whole panel),
///   2. delegates the load to the use-case trait.
///
/// Returns 503 when the late-bind slot is still empty (Phase B
/// hasn't run yet) — mirrors every other ADR-040 use-case slot.
async fn get_agent_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let matches = state.instance_matches(&id);
    if !matches {
        // Tolerate a misconfigured Gateway rather than 404 — see ADR-034
        // and the comment block above.
        return Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "matches": false,
            "config": serde_json::Value::Null,
            "manifest_path": state.work_dir.join("manifest.toml"),
            "work_dir": state.work_dir,
        })));
    }
    let resp = state
        .agent_config
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent config service not ready"
            })),
        ))?
        .get_config(&id)
        .await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "matches": matches,
        "config": resp.config,
        "manifest_path": resp.manifest_path,
        "work_dir": resp.work_dir,
    })))
}

/// `PUT /agents/{id}/config` — live-edit per-agent runtime config
/// (mqtt.md §7).
///
/// ADR-040 follow-up: persistence + the load-merge-save cycle +
/// per-field dispatch + the `RuntimeConfigOverrides` projection all
/// live in [`AgentConfigService::put_config`]. This handler is a
/// thin protocol converter that:
///   1. decodes the wire shape into [`PutAgentConfigBody`],
///   2. hands it to the use-case trait,
///   3. broadcasts the returned `RuntimeConfigOverrides` to active
///      sessions via the existing dispatch channel,
///   4. re-PUBLISHes the retained `acowork/agents/{id}/config`
///      snapshot using the returned [`crate::agent_config::AgentConfig`].
///
/// **`builtin_tools` no longer lives here** — it was misleadingly
/// bundled into this endpoint in the original implementation, but it
/// persists to a different file (`agent_tools.json`, see ADR-029) and
/// has different read-modify-write semantics. It moved to its own
/// endpoint at `PUT /agents/{id}/builtin-tools` (handled by
/// [`AgentToolsService::put_builtin_tools`]) — the Desktop Tools
/// panel now calls that endpoint directly.
///
/// Active sessions are NOT force-reloaded here. New sessions created
/// after this call pick up the new values naturally because Phase A
/// re-reads `agent_config.json` at startup; existing in-flight
/// sessions pick up live-editable fields via the `UserOp::UpdateRuntimeConfig`
/// broadcast path (step 3 above) — same contract documented in
/// `mqtt.md` §3.5.
async fn put_agent_config(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentConfigRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    // ADR-034: path `id` must match this Runtime's instance identity. A mismatch
    // is a caller bug — 404 makes the misrouting loud instead of
    // silently writing to the wrong agent's directory.
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }

    // 1. Hand the per-field patches to the use-case trait. The impl
    //    runs the load-merge-save cycle against `agent_config.json`
    //    and returns the new on-disk state plus the
    //    `RuntimeConfigOverrides` projection for the live broadcast.
    let body = crate::usecases::PutAgentConfigBody::from_request_fields(
        req.max_output_tokens,
        req.max_iterations,
        req.max_sessions,
        req.temperature,
        req.context_window,
        req.shell_approval_threshold,
        req.approval_timeout_secs,
        req.compression_ratio_threshold,
        req.distiller_enabled,
        req.distiller_model,
        req.distiller_models,
        req.distiller_interval_minutes,
        req.distiller_accumulation_threshold,
        req.distiller_idle_minutes,
        req.memory_forgetting_enabled,
        req.memory_forgetting_half_life_days,
        req.memory_forgetting_dormant_threshold,
        req.memory_forgetting_archive_days,
        req.session_language,
    );
    let svc = state
        .agent_config
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent config service not ready"
            })),
        ))?
        .clone();
    let result = svc.put_config(&id, body).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("agent_config.json persistence failed: {}", e),
            })),
        )
    })?;

    // 2. Live-broadcast the live-editable subset (temperature,
    //    context_window, max_iterations, …)
    //    through `SessionManager::apply_runtime_config_override` via a
    //    SINGLE system-level message. That pipeline applies the change
    //    to (a) the shared AgentCore template so future sessions
    //    inherit it, (b) the `runtime_overrides` cache, (c) every
    //    active SessionTask's `ContextBuilder.tool_definitions` (so
    //    the LLM sees the new set on the next `build_chat_request`),
    //    and (d) mid-execution AgentLoops via the inbound fast
    //    channel. The pre-ADR-052 implementation fanned out one
    //    per-session message into the AgentLoop fast channel only,
    //    which left the LLM-visible `ContextBuilder.tool_definitions`
    //    stale - the "hot reload does nothing" symptom.
    broadcast_runtime_overrides(&state, &result.overrides).await;

    // 3. Re-PUBLISH retained config so any other Desktop subscriber
    //    (and the Desktop's own ConfigSnapshot listener) sees the new
    //    values immediately. Best-effort: if the broker isn't
    //    reachable yet, the on-disk file is still authoritative.
    //
    // The publisher is constructed locally from the late-bound MQTT
    // client slot — no `HttpState` field added, so the state struct
    // stays focused on persistence/dispatch concerns.
    if let Some(mqtt) = state.mqtt_client.lock().await.clone() {
        // ADR-040: use the config_json returned by the UseCase impl
        // instead of re-reading the file from disk.
        let runtime = mqtt.lock().await;
        crate::mqtt::MqttAgentConfigPublisher::from_runtime_client(&runtime)
            .publish(result.config_json.clone())
            .await;
    }

    Ok(Json(serde_json::json!({
        "agent_id": state.agent_id,
        "accepted": true,
    })))
}

/// Request body for `PUT /agents/{id}/config` (mqtt.md §7).
///
/// Each field is **optional** and matches the wire shape that the
/// Desktop `AgentSetupTab.handleApply` builds from `AgentProfileSettings`.
/// The handler applies a read-modify-write cycle so partial updates
/// don't clobber unrelated on-disk values (e.g. touching
/// `temperature` must not erase an existing `context_window`).
///
/// Semantics notes:
///   - Every field here uses the same `"Some(...)" -> overwrite,
///     "None" -> leave alone` rule, so the wire shape is partial.
///   - **`builtin_tools` no longer lives on this struct.** It used to
///     persist to `agent_tools.json` while everything else persisted
///     to `agent_config.json` — a confusing mismatch that the original
///     design papered over by sharing one endpoint. It moved to its
///     own endpoint `PUT /agents/{id}/builtin-tools` (handled by
///     [`AgentToolsService::put_builtin_tools`]) and the Desktop Tools
///     panel now calls that endpoint directly.
///   - Fields that have no analogue on the wire (e.g.
///     `avatar`, `builtin_avatar`,
///     `system_prompt_override`) are deliberately omitted from this
///     struct: they aren't exposed in the Setup panel today, so
///     accepting them on the wire would silently no-op and confuse
///     callers. They keep flowing through `RuntimeConfigUpdate` over
///     MQTT the same as before.
#[derive(Debug, Deserialize)]
struct UpdateAgentConfigRequest {
    // ── Per-agent config (`agent_config.json`) ──
    //
    // We use `Option<serde_json::Value>` as the wire carrier for each
    // per-agent field.  Note this cannot distinguish three wire
    // states — serde collapses JSON `null` and absent-field into the
    // same `None` (the same is true of `Option<T>`).  In practice
    // this is fine: the Desktop `AgentSetupTab.handleApply` never
    // sends JSON `null`, only concrete values or omits the field,
    // so:
    //
    //   1. Field absent in the JSON payload -> `None` outer ->
    //      leave the on-disk value alone (partial PUT semantics).
    //   2. Field present with a value       -> `Some(Value::...)`  ->
    //      overwrite with the deserialized value.
    //
    // If a future caller ever needs explicit-clearing semantics
    // (e.g. a CLI that wants to send `"temperature": null` to fall
    // through to the manifest default), the cleanest path is to
    // introduce an `OptionalField<T>` presence-tracking wrapper
    // around the inner `Value`.  See the test doc-comment for
    // `test_put_agent_config_persists_per_agent_fields`.
    //
    // Mirrors `AgentConfig` one-for-one; keep the two in lockstep
    // when adding new fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_iterations: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_sessions: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    temperature: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    context_window: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell_approval_threshold: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_timeout_secs: Option<serde_json::Value>,
    /// ADR-061: minimum compression ratio for levels 1-7 (0.05–0.95;
    /// 0.90 default = "compress until at most 10% remains"). Absent =
    /// leave the on-disk value alone (partial PUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    compression_ratio_threshold: Option<serde_json::Value>,
    /// ADR-071 D4: distiller runtime switch (记忆蒸馏 card). Absent =
    /// leave the on-disk value alone (partial PUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distiller_enabled: Option<serde_json::Value>,
    /// ADR-071 D4/D5: legacy single distiller model ref
    /// `{"provider_id":…,"model_id":…}` (folded into a one-element list
    /// when `distiller_models` is absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distiller_model: Option<serde_json::Value>,
    /// Ordered distiller model candidate list
    /// `[{"provider_id":…,"model_id":…}, …]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distiller_models: Option<serde_json::Value>,
    /// ADR-071 D4: distiller periodic interval (minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distiller_interval_minutes: Option<serde_json::Value>,
    /// ADR-071 D4: distiller accumulation threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distiller_accumulation_threshold: Option<serde_json::Value>,
    /// ADR-071 D4: distiller idle threshold (minutes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    distiller_idle_minutes: Option<serde_json::Value>,
    /// Memory forgetting runtime switch (记忆遗忘 card). Absent =
    /// leave the on-disk value alone (partial PUT).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_forgetting_enabled: Option<serde_json::Value>,
    /// Episodic decay half-life in days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_forgetting_half_life_days: Option<serde_json::Value>,
    /// Retention threshold below which an episodic node becomes Dormant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_forgetting_dormant_threshold: Option<serde_json::Value>,
    /// Days a Dormant node is retained before archiving to PurgeLog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    memory_forgetting_archive_days: Option<serde_json::Value>,
    /// Per-agent LLM session language override (e.g. `"zh-CN"`, `"en"`).
    /// Replaces `UserProfile.language` in the identity-context text
    /// block when formatted for the LLM (see
    /// `SessionManager::format_user_profile_context`). Absent = leave
    /// the on-disk value alone (partial PUT); `null` = clear (fall
    /// back to `UserProfile.language`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_language: Option<serde_json::Value>,
}

impl UpdateAgentConfigRequest {
    // The previous `project()` method that built the patch list and
    // `RuntimeConfigOverrides` projection inline was deleted when
    // persistence moved to [`crate::usecases::AgentConfigService`].
    // The handler now translates the wire shape into
    // [`crate::usecases::PutAgentConfigBody::from_request_fields`]
    // and the impl returns the new on-disk config plus the
    // `RuntimeConfigOverrides` projection in lockstep. See the
    // `put_agent_config` handler for the dispatch flow.
}

// ── Per-field patch machinery moved to usecases/agent_config.rs ──────
//
// The previous `UpdateAgentConfigRequest::project()`,
// `value_to_patch`, `patch_typed`, and `FieldPatchExt` helpers all
// lived inline in this file. After the ADR-040 follow-up refactor
// the persistence + projection logic is the single audit point in
// [`crate::usecases::RuntimeAgentConfigService::put_config`]. The
// HTTP handler now translates the wire shape into
// [`crate::usecases::PutAgentConfigBody::from_request_fields`] and
// hands it to the trait; the impl runs the dispatch loop and
// returns the new on-disk state plus the `RuntimeConfigOverrides`
// projection.
//
// The shared [`FieldPatch`] enum + the `ConfigField` enum live in
// `usecases::agent_config`; both are pub-exported via
// `crate::usecases` so handlers can build the body without an
// extra indirection.

/// Broadcast a `RuntimeConfigOverrides` push to every live session.
///
/// The overrides are produced by `UpdateAgentConfigRequest::project`
/// above, which already projects the wire shape onto the in-process
/// struct (and forces boot-only fields to `None` so we never
/// invalidate in-flight conversation history pointers).
///
/// The push goes through `dispatch_tx` (per-session `(String,
/// InboundMessage)` channel), reusing the exact same routing that
/// `dispatch_inbound` uses for `UserOp::UpdateRuntimeConfig` today.
/// `gateway_loop` forwards those messages through
/// `forward_to_session_inbound`, which lands in `AgentLoop`'s
/// `drain_inbound_queue` → `apply_user_op` → `apply_runtime_config`,
/// so the next LLM iteration picks up the new temperature /
/// context_window / etc. without restarting the session.
/// Dispatch an agent-level config mutation through the Runtime's
/// single application pipeline via ONE system-level message.
///
/// The message travels `dispatch_tx` → `mqtt_dispatch_rx` →
/// `gateway_loop::dispatch_inbound`, which routes both agent-level
/// config variants (`UserOp::UpdateRuntimeConfig`,
/// `InboundMessage::UpdateBuiltinTools`) through the corresponding
/// `SessionManager::{apply_runtime_config_override,
/// apply_builtin_tools_enabled}` regardless of the routing key. Those
/// two methods are the ONLY entry points that update, in one shot:
///   1. the shared `AgentCore` **template** (so sessions opened LATER
///      inherit the new value),
///   2. the `runtime_overrides` / builtin-enable caches,
///   3. every active SessionTask's `ContextBuilder.tool_definitions`
///      (so the LLM sees the new set on the next
///      `build_chat_request`), and
///   4. mid-execution AgentLoops via the inbound fast channel
///      (`apply_user_op` → `core.apply_runtime_config`).
///      (ADR-061 §10.2: the `tool_compression_enabled` hot-reload
///      path is deleted.)
///
/// Pre-ADR-052 the HTTP layer sent one message **per session id** into
/// the AgentLoop fast channel only. That mutated the live session's
/// `all_tools` dispatch list but left every other layer (template,
/// runtime_overrides cache, other sessions' `ContextBuilder`) stale -
/// which is precisely the "hot reload does nothing" symptom this
/// function is the fix for.
///
/// Routing key `""` marks the message system-level so dispatch_inbound
/// applies it through the SessionManager pipeline above. Sending
/// exactly ONE message (instead of fanning out per session) keeps the
/// pipeline idempotent regardless of how many sessions happen to be
/// alive at push time.
///
/// Best-effort on the dispatch channel itself: a missing `dispatch_tx`
/// (agent not yet ready, Phase A pre-Phase D startup race) logs a
/// warning and returns. The on-disk file written by the UseCase layer
/// remains authoritative and is re-read at the next boot.
/// `pub(crate)` so the L2 reload endpoint (`http::prompts`) can push the
/// rebuilt main-dialog system prompt through the same agent-level pipeline
/// (ADR-063 §3.7.6 hot-reload).
pub(crate) async fn dispatch_agent_level_config(
    state: &HttpState,
    label: &str,
    msg: InboundMessage,
) {
    let tx_opt = state.dispatch_tx.lock().await.clone();
    let Some(tx) = tx_opt else {
        tracing::warn!(
            agent_id = %state.agent_id,
            label,
            "agent-level config dispatch skipped: dispatch channel not ready (on-disk file is authoritative)"
        );
        return;
    };
    match tx.send((String::new(), msg)) {
        Ok(()) => tracing::info!(
            agent_id = %state.agent_id,
            label,
            "agent-level config update dispatched to SessionManager pipeline (system-level)"
        ),
        Err(_) => tracing::warn!(
            agent_id = %state.agent_id,
            label,
            "agent-level config dispatch failed: dispatch channel closed (on-disk file is authoritative)"
        ),
    }
}

/// Push a session-scoped inbound message onto the Runtime's dispatch
/// channel from an HTTP handler.
///
/// ADR-076 §决策 4: user-triggered session actions arrive over HTTP now,
/// but they must reach `dispatch_inbound` — which is where the real work
/// lives — by the exact route the MQTT control path used
/// (`http_dispatch_rx` → `mqtt_dispatch_tx`). This helper is that
/// hand-off: HTTP does authorization and routing, the message keeps its
/// original semantics.
///
/// Returns `false` only when the channel is not up yet (Phase A → D
/// startup race), which the handler surfaces as 503. A `SessionNotOpened`
/// or similar execution failure is *not* reported here — it is published
/// on the MQTT event plane, which the Desktop already listens to.
pub(crate) async fn dispatch_session_action(
    state: &HttpState,
    label: &str,
    session_id: &str,
    msg: InboundMessage,
) -> bool {
    let tx_opt = state.dispatch_tx.lock().await.clone();
    let Some(tx) = tx_opt else {
        tracing::warn!(
            agent_id = %state.agent_id,
            session_id,
            label,
            "session action dispatch skipped: dispatch channel not ready"
        );
        return false;
    };
    match tx.send((session_id.to_string(), msg)) {
        Ok(()) => {
            tracing::info!(
                agent_id = %state.agent_id,
                session_id,
                label,
                "session action dispatched via HTTP"
            );
            true
        }
        Err(_) => {
            tracing::warn!(
                agent_id = %state.agent_id,
                session_id,
                label,
                "session action dispatch failed: dispatch channel closed"
            );
            false
        }
    }
}

/// Push a runtime-config override into the SessionManager via
/// [`dispatch_agent_level_config`].
///
/// Early-exits when `overrides` is empty so we don't dispatch a no-op
/// `UpdateRuntimeConfig` that would still trigger
/// `emit_session_state` on every active session for nothing.
async fn broadcast_runtime_overrides(state: &HttpState, overrides: &RuntimeConfigOverrides) {
    if overrides.is_empty() {
        return;
    }
    dispatch_agent_level_config(
        state,
        "runtime_config",
        InboundMessage::UserOperation(UserOp::UpdateRuntimeConfig(Box::new(overrides.clone()))),
    )
    .await;
}

/// Push a builtin-tools enabled update into the SessionManager via
/// [`dispatch_agent_level_config`].
///
/// Always sends (even with zero active sessions) so the
/// `AgentCore` template gets the new flags and sessions opened later
/// inherit them.
async fn broadcast_builtin_tools_update(
    state: &HttpState,
    entries: &[crate::agent_config::AgentToolEntry],
) {
    dispatch_agent_level_config(
        state,
        "builtin_tools",
        InboundMessage::UpdateBuiltinTools {
            entries: entries.to_vec(),
        },
    )
    .await;
}

/// `GET /agents/{id}/tools` - Tools panel (merged: builtin + mcp).
///
/// ADR-034 §7.6.5 defines the merged response schema:
/// `{tools: [BuiltinToolEntry], mcp_servers: [server_name]}`
/// (panel 3 pulls both sources in one HTTP call instead of separate ones).
///
/// ADR-040: delegates to [`AgentToolsService::get_merged_tools`] via
/// the late-bind slot. Both `agent_*` config files are read inside
/// the UseCase impl; the handler is a thin protocol converter.
async fn get_agent_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let matches = state.instance_matches(&id);
    if !matches {
        // Tolerate a misconfigured Gateway rather than 404 - see ADR-034.
        return Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "matches": false,
            "tools": [],
            "mcp_servers": [],
            "mcp_servers_defs": [],
        })));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_merged_tools(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "matches": true,
        "tools": resp.tools,
        "mcp_servers": resp.mcp_servers,
        "mcp_servers_defs": resp.mcp_servers_defs,
    })))
}

// ── Per-agent MCP activation endpoints ────────────────────────────────
//
// Win11-MCP-ToolsBugFix: the Desktop Tools panel toggles per-agent MCP server
// activation (`activeServers` in `mcpStore.ts`). The wiring lives in 3 layers:
//   1. Desktop  : PUT  /api/agents/{id}/mcp-servers   (sends {servers: ["name1", ...]})
//   2. Gateway  : pure reverse-proxy to Runtime       (see acowork-gateway proxy.rs)
//   3. Runtime  : validate id match + read-modify-write `agent_mcp.json`
//
// Before this fix the Gateway-side stub returned 200 but never persisted
// (`let _ = (..., resolved_servers)`). The Desktop optimistically updated
// its in-memory Zustand store on success, then lost the selection the next
// time the user switched tabs because the merged `/tools` endpoint read from
// the (empty) on-disk config and overwrote the Zustand copy with `[]`.

/// `GET /agents/{id}/mcp-servers` — names of MCP servers active for this agent
/// (i.e. the user's selection in the Desktop Tools panel).
async fn get_agent_mcp_servers(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }
    // ADR-040 follow-up: route through the trait. HTTP handler is now a
    // thin protocol converter (path-match + JSON shape) — the file
    // I/O, `active_names` resolution, and any future cross-field
    // validation live in [`crate::usecases::RuntimeAgentToolsService`].
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_mcp_servers(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "active_servers": resp.active_servers,
    })))
}

/// `PUT /agents/{id}/mcp-servers` — set the active MCP server selection.
///
/// Body: `{"servers": ["name1", ...]}` — just the catalog names the user ticked.
/// Acceptance rule: each name must exist in `merged()` (catalog + local). An
/// unknown name is a 400 — the frontend already filters to catalog items so
/// this protects against stale names reaching us via direct API calls.
///
/// Storage: write `active_names = Some(req.servers)` while preserving `catalog`
/// and `local` via a read-modify-write cycle (parallel to `put_agent_config`'s
/// per-field patch in this file).
async fn put_agent_mcp_servers(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateMcpServersRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }

    // ADR-040 follow-up: validation + persistence live in
    // [`crate::usecases::RuntimeAgentToolsService`]. The handler is
    // now a thin protocol converter that maps service errors to HTTP
    // status codes.
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let body = crate::usecases::PutMcpServersBody {
        servers: req.servers,
    };
    // ADR-090: compute the enable/disable diff BEFORE the service
    // overwrites `active_names`. Newly-enabled preset servers get an
    // ensure task (health_check-first → install → re-check, on THIS
    // machine); newly-disabled ones drop their stale registry entry so
    // a later re-open starts a clean run (D6: retry = reopen).
    let prev_active: Vec<String> = crate::agent_config::load_agent_mcp_config(
        std::path::Path::new(&state.work_dir),
    )
    .ok()
    .flatten()
    .map(|c| c.active_server_names())
    .unwrap_or_default();
    let newly_enabled: Vec<String> = body
        .servers
        .iter()
        .filter(|n| !prev_active.contains(n))
        .cloned()
        .collect();
    for gone in prev_active.iter().filter(|n| !body.servers.iter().any(|s| s == *n)) {
        crate::mcp_ensure::clear_status(gone);
    }
    match svc.put_mcp_servers(&id, body).await {
        Ok(resp) => {
            // ADR-069 follow-up: persistence alone is not enough — the
            // Runtime's MCP connection pool must reconnect so the newly
            // active server (e.g. `playwright`) is actually connected
            // and its tools reconciled into agent_mcp_tools.json. Fire
            // the shared notifier; the gateway loop's
            // `mcp_config_rx.changed()` branch does the background
            // reconnect. Without this, enabling a server updates
            // `active_names` but the Tools panel still shows an empty
            // expansion (no tools data).
            if let Some(notifier) = &state.mcp_notifier {
                tracing::info!(
                    instance_id = %id,
                    "PUT /mcp-servers persisted — signaling MCP config change for reconnect"
                );
                notifier.notify();
            }
            // ADR-090 D5: first-enable ensure runs in the Agent process —
            // the machine the Runtime actually spawns on. Fire-and-forget;
            // progress is polled via GET /agents/{id}/mcp-ensure/{name}.
            if !newly_enabled.is_empty() {
                crate::mcp_ensure::ensure_newly_enabled(
                    std::path::Path::new(&state.work_dir),
                    &newly_enabled,
                    state.mcp_notifier.clone(),
                );
            }
            Ok(Json(serde_json::json!({
                "agent_id": resp.agent_id,
                "active_servers": resp.active_servers,
                "ensuring": newly_enabled,
            })))
        }
        Err(crate::usecases::AgentToolsError::UnknownServers(unknown)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "unknown MCP server names (not in catalog+local)",
                "unknown": unknown,
            })),
        )),
        Err(crate::usecases::AgentToolsError::Persistence(msg)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to persist agent_mcp.json: {}", msg),
            })),
        )),
    }
}

/// `GET /agents/{id}/mcp-ensure/{name}` — ADR-090 first-enable ensure
/// status for one catalog server. `{"state":"probing"|"installing"|
/// "usable"|"failed", "error": "..."}` — 404 when the server has never
/// been ensured in this process (custom/local servers and non-stdio
/// entries never spawn an ensure task, so polling them is a client bug).
async fn get_agent_mcp_ensure(
    State(state): State<HttpState>,
    Path((id, name)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "instance_id mismatch"})),
        ));
    }
    match crate::mcp_ensure::ensure_status(&name) {
        Some(status) => Ok(Json(
            serde_json::to_value(status).unwrap_or_else(|_| serde_json::json!({"state": "failed"})),
        )),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("no ensure task for '{name}'")})),
        )),
    }
}

// ── Per-agent MCP tools endpoints (ADR-069) ──────────────────────────//
// These two routes expose the full per-tool opt-in list. The desktop
// Tools panel renders this list directly (default-collapsed MCP server
// cards, one switch per tool). The backend is the single source of
// truth: `agent_mcp_tools.json` is reconciled against the live MCP
// `tools/list` at connect time, so the list the frontend sees always
// matches what the runtime filters.

/// `GET /agents/{id}/mcp-tools` — return the full per-tool list.
async fn get_agent_mcp_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_mcp_tools(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "servers": resp.servers,
    })))
}

/// `PUT /agents/{id}/mcp-tools` — persist the complete per-server tool
/// list. Body shape is identical to the GET response (three-way
/// identity between agent_mcp_tools.json, the wire format, and the
/// desktop render). The backend writes it verbatim — the desktop's
/// list is authoritative and there is no server-side merge (ADR-069).
async fn put_agent_mcp_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<crate::usecases::PutMcpToolsBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    match svc.put_mcp_tools(&id, req).await {
        Ok(resp) => {
            // ADR-069 follow-up: after persisting the per-tool opt-in
            // list, signal the gateway loop to reconnect MCP servers so
            // the new `enabled` filter is applied to the live
            // tool_definitions immediately (same notifier the
            // `PUT /mcp-servers` handler fires).
            if let Some(notifier) = &state.mcp_notifier {
                tracing::info!(
                    instance_id = %id,
                    "PUT /mcp-tools persisted — signaling MCP config change for reconnect"
                );
                notifier.notify();
            }
            Ok(Json(serde_json::json!({
                "agent_id": resp.agent_id,
                "servers": resp.servers,
            })))
        }
        Err(crate::usecases::AgentToolsError::UnknownServers(unknown)) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "unknown MCP server names",
                "unknown": unknown,
            })),
        )),
        Err(crate::usecases::AgentToolsError::Persistence(msg)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to persist agent_mcp_tools.json: {}", msg),
            })),
        )),
    }
}

// ── Provider list endpoint ──────────────────────────────────────────────
//
// Read-through endpoint that returns the content of `agent_provider.json`
// — the provider catalog pushed by Gateway via MQTT
// (acowork/global/providers) and persisted by the MQTT handler in
// [`crate::agent_config::save_agent_provider_config_from_available`].
//
// Unlike MCP / builtin-tools, there is no user-authorable subset
// of the provider list; the entire file is Gateway-authored. The endpoint
// exists so the frontend can verify what the Runtime actually has at any
// given time — a diagnostic / consistency-check tool rather than a
// user-facing configuration surface.

/// `GET /agents/{id}/providers` — read `agent_provider.json`.
async fn get_agent_providers(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }

    let cfg = crate::agent_config::load_agent_provider_config(&state.work_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e })),
        )
    })?;

    match cfg {
        Some(provider_config) => Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "providers": provider_config.providers,
            "version": provider_config.version,
        }))),
        None => Ok(Json(serde_json::json!({
            "agent_id": state.agent_id,
            "providers": [],
            "version": 0,
        }))),
    }
}

// ── Builtin tools endpoint ─────────────────────────────────────────────
//
// Parallel structure to the MCP pair above, but persists to
// `agent_tools.json`. Lives under the `AgentToolsService` trait
// (the same one MCP uses) because the persistence
// semantics — read-modify-write with `apply_builtin_tools_patch`,
// PLATFORM_TOOLS force-enable, silent unknown-name dropping — are the
// same shape, just a different file on disk.
//
// ADR-029 §7 + mqtt.md §7.1 wire semantics: `builtin_tools: Vec<String>`
// is the **complete enabled set** — any tool currently on disk but
// absent from this list must be flipped to `enabled = false`. The
// impl owns the read-modify-write + patch construction (see
// `RuntimeAgentToolsService::put_builtin_tools`); this handler is
// just a protocol converter.

/// `GET /agents/{id}/builtin-tools` — read `agent_tools.json` (per-tool
/// entries with their enabled flag).
async fn get_agent_builtin_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let resp = svc.get_builtin_tools(&id).await;
    Ok(Json(serde_json::json!({
        "agent_id": resp.agent_id,
        "tools": resp.tools,
    })))
}

/// `PUT /agents/{id}/builtin-tools` — persist the enabled-set for
/// builtin tools. Same JSON 503 / 500 mapping as the MCP
/// counterpart (no 400 — unknown tool names are silently dropped per
/// ADR-029 §7; see the `AgentToolsService` doc for rationale).
async fn put_agent_builtin_tools(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<UpdateAgentBuiltinToolsRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!(
                    "instance_id mismatch: path '{}' does not address this runtime instance '{}'",
                    id, state.instance_id
                ),
            })),
        ));
    }
    let svc = state
        .agent_tools
        .lock()
        .await
        .as_ref()
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "agent tools service not ready"
            })),
        ))?
        .clone();
    let body = crate::usecases::PutBuiltinToolsBody {
        builtin_tools: req.builtin_tools,
    };
    match svc.put_builtin_tools(&id, body).await {
        Ok(resp) => {
            // ADR-029/ADR-052: broadcast the new enabled flags to all
            // sessions AND sync the shared AgentCore template so
            // sessions created after this PUT inherit the new flags -
            // see [`broadcast_builtin_tools_update`] doc and
            // `SessionManager::apply_builtin_tools_enabled`.
            broadcast_builtin_tools_update(&state, &resp.tools).await;
            Ok(Json(serde_json::json!({
                "agent_id": resp.agent_id,
                "tools": resp.tools,
            })))
        }
        Err(crate::usecases::AgentToolsError::Persistence(msg)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("failed to persist agent_tools.json: {}", msg),
            })),
        )),
        // No `UnknownTools` variant — see ADR-029 §7 + the trait
        // doc.  Defensive match to keep the compiler exhaustive
        // without forcing a logic-only `unreachable!`.
        Err(other) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": format!("unexpected put_builtin_tools error: {}", other),
            })),
        )),
    }
}

/// Wire-shape DTOs for the Tools-panel PUT endpoints. Kept local to
/// `server.rs` because they only travel between the Gateway proxy and the
/// Runtime HTTP server (the Desktop uses its own narrower types in
/// `apps/acowork-desktop/src/lib/types.ts`).
///
/// **Wire semantics (pattern B — "complete enabled set")**: the body's
/// list is the *full* desired state, not a patch. Anything currently on
/// disk but absent from the list is flipped to inactive / removed.
///
/// `#[serde(default)]` is required on every list field so that the
/// deserialization layer accepts bare `{}` (no keys) as well as explicit
/// `{"servers": []}` / `{"providers": []}`. Both shapes mean "explicitly
/// empty selection" and must reach the usecase layer (where the impl
/// already handles them as `Some(vec![])` — see
/// `usecases::agent_tools_impl::put_mcp_servers` line 97-98 for the MCP
/// contract). Without `#[serde(default)]`, missing-field deserialization
/// fails at the `Json<...>` extractor before the handler runs, returning
/// 400 to clients that legitimately send `{}` (e.g. a CLI clearing the
/// selection without restating the empty array).
#[derive(serde::Deserialize)]
struct UpdateMcpServersRequest {
    /// Names of MCP servers to activate (complete enabled set).
    /// Empty/absent means "no servers active".
    #[serde(default)]
    servers: Vec<String>,
}

#[derive(serde::Deserialize)]
struct UpdateAgentBuiltinToolsRequest {
    /// Names of builtin tools to enable (complete enabled set).
    /// Empty/absent means "no builtin tools enabled" (platform tools
    /// are still force-enabled by the patcher — see ADR-029 §7).
    #[serde(default)]
    builtin_tools: Vec<String>,
}

/// `GET /agents/{id}/status` — Agent Status panel (Runtime running state).
///
/// Surfaces the information the desktop needs to render its
/// Agent-Status header card: process identity, work dir, list of
/// known sessions (from `SharedLatestSession` + a quick meta scan),
/// and the active embedding model dimension.
async fn get_agent_status(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let matches = state.instance_matches(&id);

    // Pull the active session + model/embedding dim from the shared
    // state so the panel can show "what is the agent doing right now?".
    let latest_session = state.latest_session.read().ok().and_then(|g| g.clone());
    let embed_dim = state.live_embed_dim();

    Json(serde_json::json!({
        "agent_id": state.agent_id,
        "matches": matches,
        "work_dir": state.work_dir,
        "pid": std::process::id(),
        "latest_session": latest_session,
        "embed_dim": embed_dim,
    }))
}

// ── Shell Risk Rules (ADR-055) ────────────────────────────────────────

/// `GET /agents/{id}/shell-risk-rules` — read the effective shell risk rules.
///
/// Returns `{agent_id, matches, content, has_user_override}`. Content precedence:
/// 1. User override file on disk (raw text, including comments)
/// 2. Embedded defaults source (raw text, including comments)
///
/// We deliberately return the raw TOML text rather than re-rendering
/// from the parsed in-memory rules — comments and formatting from the
/// source are preserved so the editor shows the file as authored.
async fn get_shell_risk_rules(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({"error": "instance_id mismatch: path id is not this runtime's instance id"}),
            ),
        ));
    }
    let config_dir = state.work_dir.join("config");
    let path = config_dir.join("shell_risk_rules.toml");
    tracing::info!(instance_id = %id, path = %path.display(), "GET /agents/{id}/shell-risk-rules");

    // Build revision identifier embedded in the generated template. It is
    // informational only — lets the user see at a glance which binary
    // snapshot the template was generated from, so they can spot stale
    // copies across machines.
    let build_rev = format!(
        "runtime-{}-{}",
        env!("CARGO_PKG_VERSION"),
        chrono::Utc::now().format("%Y%m%d"),
    );

    // UX contract: clicking the "Edit" button creates the user file on
    // disk if it does not already exist. The created file is a template:
    //   • the binary's embedded rules as `# ...` comments (so the user
    //     can see what is already covered before adding their own), and
    //   • an empty `rules = []` body (so saving it back is a no-op and
    //     the merge-load semantics in `ShellRiskRules::load` keep the
    //     embedded rules live).
    //
    // Why create on GET rather than wait for the user to press Save:
    //   1. `has_user_override` flips to `true` immediately, which the
    //      frontend uses to render the "this is your local copy" hint
    //      and to make the file appear in the agent's file tree.
    //   2. Avoids the failure mode where the user opens the editor,
    //      walks away, and a later binary upgrade silently changes what
    //      the editor would have shown them — the file is now pinned
    //      to the binary snapshot they actually saw.
    //   3. A user who closes the editor without saving still has a
    //      file that is semantically empty (`rules = []`), which is
    //      the same observable behavior as "no file" under merge
    //      load.
    let (content, has_user_override) = if path.exists() {
        match std::fs::read_to_string(&path) {
            Ok(c) => (c, true),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "Failed to read shell risk rules");
                let template = crate::security::shell_risk::generate_user_rules_toml(&build_rev)
                    .unwrap_or_else(|err| {
                        tracing::error!(error = %err, "generate_user_rules_toml failed; falling back to embedded raw");
                        crate::security::shell_risk::ShellRiskRules::embedded_defaults().to_string()
                    });
                (template, false)
            }
        }
    } else {
        let template = match crate::security::shell_risk::generate_user_rules_toml(&build_rev) {
            Ok(s) => s,
            Err(err) => {
                tracing::error!(error = %err, "generate_user_rules_toml failed; falling back to embedded raw");
                crate::security::shell_risk::ShellRiskRules::embedded_defaults().to_string()
            }
        };
        // Create the user file on first GET. A write failure is logged
        // but does not block the editor from opening — the template is
        // still served in-memory and PUT can materialize it later.
        if let Err(e) = std::fs::create_dir_all(&config_dir) {
            tracing::warn!(path = %config_dir.display(), error = %e, "Failed to create config dir; serving template without persisting");
        } else if let Err(e) = std::fs::write(&path, &template) {
            tracing::warn!(path = %path.display(), error = %e, "Failed to materialize user rules template; will retry on PUT");
        } else {
            tracing::info!(path = %path.display(), bytes = template.len(), "Materialized shell risk rules user template on first GET");
        }
        (template, true)
    };
    Ok(Json(serde_json::json!({
        "agent_id": state.agent_id,
        "matches": true,
        "content": content,
        "has_user_override": has_user_override,
    })))
}

/// `PUT /agents/{id}/shell-risk-rules` — write user override for shell risk rules.
///
/// Validates TOML syntax before writing; on success both disk and the
/// in-memory rule cache are updated so live sessions pick up the new rules
/// without a restart.
async fn put_shell_risk_rules(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if !state.instance_matches(&id) {
        return Err((
            StatusCode::NOT_FOUND,
            Json(
                serde_json::json!({"error": "instance_id mismatch: path id is not this runtime's instance id"}),
            ),
        ));
    }
    tracing::info!(
        instance_id = %id,
        content_bytes = req.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0),
        "PUT /agents/{id}/shell-risk-rules"
    );
    let content = req.get("content").and_then(|v| v.as_str()).ok_or((
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "missing or invalid 'content' field"})),
    ))?;
    // Validate TOML syntax before writing — fail fast if the editor
    // contains a parse error so the user can fix it without restarting.
    //
    // The on-disk format (see `security/shell_risk_rules.toml`) is a
    // table-of-rules: `{ rules: [ {...}, {...} ] }`. That schema matches
    // `ShellRiskRules`, NOT a bare `Vec<ShellRiskRule>` — deserializing
    // into the Vec used to fail with "invalid type: map, expected a
    // sequence" because the top-level value is a map, not an array.
    let parsed =
        toml::from_str::<crate::security::shell_risk::ShellRiskRules>(content).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("TOML parse error: {}", e)
                })),
            )
        })?;
    // Write to disk first, then update in-memory state so GET returns
    // consistent data even if the write fails midway.
    let config_dir = state.work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to create config dir: {}", e)})),
        )
    })?;
    let path = config_dir.join("shell_risk_rules.toml");
    std::fs::write(&path, content).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to write rules: {}", e)})),
        )
    })?;
    // Update in-memory cache atomically.
    let rule_count = parsed.rules.len();
    let mut guard = state
        .shell_risk_rules
        .write()
        .unwrap_or_else(|e| e.into_inner());
    *guard = parsed;
    tracing::info!(path = %path.display(), rule_count, "Wrote shell risk rules user override");
    Ok(Json(serde_json::json!({"written": true})))
}

// ── N1: Consolidation status endpoint ──────────────────────────

/// `GET /memory/consolidation/status` - report consolidation timer state.
///
/// ADR-071 D2 extension: reports the distiller trigger state (enabled,
/// episode backlog, seconds since the last run) alongside the legacy
/// lifecycle state (idle / pending). The UI reads this to render the
/// "记忆蒸馏" card without polling the manifest.
async fn get_consolidation_status(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let timer = state
        .consolidation_timer
        .read()
        .ok()
        .and_then(|g| g.clone())
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)?;

    let idle_secs = timer.idle_secs().await;
    let episode_count = timer.episode_count().await;
    let secs_since_distill = timer.secs_since_distill().await;
    let last_distill = timer.last_distill_result().await;
    let config = timer.config();

    let last_run = last_distill.as_ref().map(|r| {
        serde_json::json!({
            "at": r.at.to_rfc3339(),
            "episodes_scanned": r.episodes_scanned,
            "total_promoted": r.total_promoted(),
            "facts_promoted": r.facts_promoted,
            "preferences_promoted": r.preferences_promoted,
            "relations_promoted": r.relations_promoted,
            "procedures_promoted": r.procedures_promoted,
            "autobio_promoted": r.autobio_promoted,
            "episodes_marked_consolidated": r.episodes_marked_consolidated,
            // Why the run produced what it produced. `error` is the difference
            // between "the distiller had nothing to do" and "the distiller
            // died every hour for a week" — both used to look like an empty
            // memory panel.
            "error": r.error,
            "funnel": {
                "scanned": r.funnel.scanned,
                "backlog_remaining": r.funnel.backlog_remaining,
                "llm_calls": r.funnel.llm_calls,
                "llm_calls_failed": r.funnel.llm_calls_failed,
                "llm_calls_truncated": r.funnel.llm_calls_truncated,
                "below_importance": r.funnel.below_importance,
                "candidates_recalled": r.funnel.candidates_recalled,
                "projected": r.funnel.projected,
                "verdict_no_merge": r.funnel.verdict_no_merge,
                "verdict_merged": r.funnel.verdict_merged,
                "verdict_superseded": r.funnel.verdict_superseded,
                "episodes_consolidated": r.funnel.episodes_consolidated,
                "episodes_deferred": r.funnel.episodes_deferred,
                "errors": r.funnel.errors,
            },
        })
    });

    Ok(Json(serde_json::json!({
        "idle_secs": idle_secs,
        "bg_task_running": true,
        // ADR-071 D1/D2: distiller trigger state.
        "distiller": {
            "enabled": config.distiller_enabled,
            "episode_backlog": episode_count,
            "secs_since_distill": secs_since_distill,
            "interval_secs": config.distiller_interval_secs,
            "accumulation_threshold": config.distiller_accumulation,
            "idle_secs": config.distiller_idle_secs,
            "last_run": last_run,
        },
        // ADR-057 §5.3 redesign: episodic forgetting state.
        "forgetting": {
            "enabled": config.forgetting_enabled,
            "half_life_days": config.forgetting_half_life_days,
            "dormant_threshold": config.forgetting_dormant_threshold,
            "archive_days": config.forgetting_archive_days,
            "interval_secs": config.forgetting_interval_secs,
        },
    })))
}

/// `POST /memory/distill` - trigger one EpisodicDistiller pass immediately.
///
/// ADR-071 D2 manual trigger: force semantics — bypasses the periodic gate
/// (interval / backlog / idle) and runs the same `run_episodic_distiller_step`
/// that the background loop uses. The response carries the distilled-run
/// summary when the run produced one.
///
/// Status codes:
/// - `200` run completed (empty episode backlog is still a completed run)
/// - `409` run not possible (memory/embedding provider unavailable)
/// - `500` run failed (distiller error)
/// - `503` AgentCore not ready yet, or the `grafeo-backend` feature is off
async fn post_memory_distill(
    State(state): State<HttpState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let core = {
        let slot = state
            .agent_core
            .read()
            .map_err(|_| err_json(StatusCode::SERVICE_UNAVAILABLE, "agent_core_lock"))?;
        slot.clone()
            .ok_or_else(|| err_json(StatusCode::SERVICE_UNAVAILABLE, "agent_core_not_ready"))?
    };
    match core.run_episodic_distill_once().await {
        Ok(Some(result)) => Ok(Json(serde_json::json!({
            "started": true,
            "episodes_scanned": result.episodes_scanned,
            "facts_promoted": result.facts_promoted,
            "preferences_promoted": result.preferences_promoted,
            "relations_promoted": result.relations_promoted,
            "procedures_promoted": result.procedures_promoted,
            "autobio_promoted": result.autobio_promoted,
            "episodes_marked_consolidated": result.episodes_marked_consolidated,
            // The counters above say what came out; the funnel says where the
            // rest went. A manual trigger is exactly when someone is watching,
            // so it answers without a second round-trip to /status.
            "funnel": result.funnel,
        }))),
        Ok(None) => Err(err_json(
            StatusCode::SERVICE_UNAVAILABLE,
            "distiller_not_available",
        )),
        Err(e) => {
            tracing::warn!(error = %e, "Manual distiller run failed");
            if e.contains("memory provider not initialized")
                || e.contains("embedding provider not initialized")
                || e.contains("distiller is disabled")
            {
                Err(err_json(StatusCode::CONFLICT, &e))
            } else {
                Err(err_json(StatusCode::INTERNAL_SERVER_ERROR, &e))
            }
        }
    }
}

/// Build an error JSON response `{"error": code}` for endpoints that return
/// `Result<Json<_>, (StatusCode, Json<_>)>`.
fn err_json(status: StatusCode, code: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({ "error": code })))
}

// ── N2: RAG status endpoint ────────────────────────────────────

/// `GET /agents/{id}/rag/status` - report RAG provider status.
async fn get_rag_status(
    State(state): State<HttpState>,
    Path(id): Path<String>,
) -> Json<serde_json::Value> {
    let _ = id;
    let rag = state.rag_provider.read().ok().and_then(|g| g.clone());

    match rag {
        Some(provider) => Json(serde_json::json!({
            "configured": true,
            "provider_name": provider.name(),
            "agent_id": state.agent_id,
        })),
        None => Json(serde_json::json!({
            "configured": false,
            "provider_name": null,
            "agent_id": state.agent_id,
        })),
    }
}

// ── N3: RAG direct query endpoint ──────────────────────────────

/// Request body for `POST /agents/{id}/rag/query`.
#[derive(Debug, Deserialize)]
struct RagQueryBody {
    query: String,
    #[serde(default)]
    top_k: Option<u32>,
    #[serde(default)]
    score_threshold: Option<f32>,
    #[serde(default)]
    filters: Option<serde_json::Value>,
}

/// `POST /agents/{id}/rag/query` - direct RAG query (bypasses LLM).
async fn post_rag_query(
    State(state): State<HttpState>,
    Path(id): Path<String>,
    body: axum::Json<RagQueryBody>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let _ = id;
    let rag = state
        .rag_provider
        .read()
        .ok()
        .and_then(|g| g.clone())
        .ok_or_else(|| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "RAG provider not configured",
                    "agent_id": state.agent_id,
                })),
            )
        })?;

    if body.query.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "query must not be empty" })),
        ));
    }

    let results = rag
        .query_with_params(
            &body.query,
            body.top_k,
            body.score_threshold,
            body.filters.clone(),
        )
        .await;

    let items: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "content": r.item.content,
                "source_url": r.item.source_url,
                "chunk_id": r.item.chunk_id,
                "score": r.item.score,
                "source_label": r.source_label,
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "query": body.query,
        "results": items,
        "result_count": items.len(),
        "provider_name": rag.name(),
    })))
}

/// Simple base64 helpers were removed in ADR-046 — the legacy
/// `POST /sessions/{sid}/documents` (which took a JSON body with a
/// base64-encoded payload) is replaced by the multipart
/// `POST /sessions/{sid}/files` endpoint that ships raw bytes. No
/// other call site in the runtime needs these helpers.
#[cfg(test)]
mod tests {
    use super::*;

    /// Wire-compat for `POST /memory/rebuild-embeddings`: the `scope` field
    /// is an extension, and a body without it (every Gateway built before
    /// this field existed, plus the model-swap flow that never sends one)
    /// must read as the only scope those clients could mean - `All`.
    #[test]
    fn rebuild_body_scope_absent_reads_as_all() {
        let body: RebuildEmbeddingsBody =
            serde_json::from_str(r#"{"endpoint":"e","model_id":"m","dimension":1024}"#)
                .expect("pre-scope wire format must still deserialize");
        assert!(matches!(body.scope, acowork_memory::admin::RebuildScope::All));

        let body: RebuildEmbeddingsBody = serde_json::from_str(
            r#"{"endpoint":"e","model_id":"m","dimension":1024,"scope":"repair"}"#,
        )
        .expect("repair scope must deserialize");
        assert!(matches!(
            body.scope,
            acowork_memory::admin::RebuildScope::Repair
        ));

        // An unknown scope is a caller bug, not a silent downgrade: the
        // request must fail rather than quietly re-embed the whole store.
        assert!(
            serde_json::from_str::<RebuildEmbeddingsBody>(
                r#"{"dimension":1024,"scope":"everything"}"#
            )
            .is_err()
        );
    }

    /// ADR-085 D4 ("扫描未完成" row): an empty `latest_session` cache
    /// is only a definitive 404 once the SESSIONS_READY stamp landed
    /// (the stamp is set after the startup scan seeded the cache).
    /// Before it — and on FAILED — the honest answer is 503.
    #[test]
    fn empty_cache_gate_follows_sessions_ready_stamp() {
        use acowork_core::mqtt_proto::AgentLifecycleState as S;
        // Standalone mode (no MQTT client, no lifecycle authority):
        // the immediate 404 behavior is preserved.
        assert!(empty_cache_is_definitive(0, false));
        // Gateway mode: only the SESSIONS_READY stamp makes the empty
        // cache definitive.
        assert!(!empty_cache_is_definitive(0, true));
        assert!(!empty_cache_is_definitive(S::Starting as u8, true));
        assert!(!empty_cache_is_definitive(S::HttpReady as u8, true));
        assert!(empty_cache_is_definitive(S::SessionsReady as u8, true));
        // FAILED: the session subsystem never became ready — 503, not
        // 404, so no caller creates a session against a failed startup.
        assert!(!empty_cache_is_definitive(S::Failed as u8, true));
    }

    /// Stable instance identity used by all in-process HTTP tests.
    /// Pre-ADR-073 these tests used the package id `com.test.agent`
    /// as both agent_id and URL path; ADR-073 requires the URL path
    /// to address the runtime's actual UUID instance id.
    const TEST_INSTANCE_ID: &str = "0a0b0c0d-1e2f-4a3b-8c7d-9e8f7a6b5c4d";

    /// Build a session-metadata service backed by on-disk session files.
    fn new_test_session_metadata(
        work_dir: &std::path::Path,
        snapshots: SharedSessionSnapshots,
        latest: SharedLatestSession,
    ) -> Arc<dyn crate::usecases::SessionMetadataService> {
        Arc::new(crate::usecases::RuntimeSessionMetadataService::new(
            work_dir.to_path_buf(),
            Arc::new(crate::usecases::agent_token_impl::NoopAgentTokenService),
            snapshots,
            latest,
        ))
    }

    /// A provider cell reporting a fixed dimension, for tests that assert the
    /// `model_dim` the memory endpoints report. `0` means "no provider bound",
    /// matching a runtime whose embed sidecar never came up.
    fn embed_cell_with_dim(
        dim: usize,
    ) -> crate::embedding::SharedEmbedProvider {
        struct FixedDim(usize);
        #[async_trait::async_trait]
        impl crate::embedding::EmbeddingProvider for FixedDim {
            fn name(&self) -> &str {
                "fixed-dim-test-stub"
            }
            async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embedding::EmbeddingError> {
                Ok(vec![0.0; self.0])
            }
            async fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, crate::embedding::EmbeddingError> {
                Ok(texts.iter().map(|_| vec![0.0; self.0]).collect())
            }
            fn dimension(&self) -> usize {
                self.0
            }
            async fn is_available(&self) -> bool {
                true
            }
        }
        let provider: Option<Arc<dyn crate::embedding::EmbeddingProvider>> = (dim > 0)
            .then(|| Arc::new(FixedDim(dim)) as Arc<dyn crate::embedding::EmbeddingProvider>);
        Arc::new(std::sync::RwLock::new(provider))
    }

    /// Build a memory-query service backed by a (possibly None) Grafeo store.
    fn new_test_memory_query(
        memory_store: SharedMemoryStore,
        embed_dim: crate::embedding::SharedEmbedProvider,
    ) -> Arc<dyn crate::usecases::MemoryQueryService> {
        Arc::new(crate::usecases::MemoryAdminAdapter::new(
            memory_store,
            embed_dim,
        ))
    }

    /// Build a workspace-query service for the test temp dir.
    fn new_test_workspace_query(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::WorkspaceQueryService> {
        Arc::new(crate::usecases::RuntimeWorkspaceQueryService::new(
            temp_dir,
            "com.test.agent".to_string(),
        ))
    }

    /// Build a workspace-mutation service for the test temp dir.
    fn new_test_workspace_mutation(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::WorkspaceMutationService> {
        Arc::new(crate::usecases::RuntimeWorkspaceMutationService::new(
            temp_dir,
        ))
    }

    /// Build a shared WorkspaceResolver for HTTP tests.
    ///
    /// The mutation handlers reload the resolver from `state.work_dir`
    /// after every successful create/update/delete, so the initial
    /// contents are irrelevant for most tests — they just need a
    /// non-panicking `SharedResolver` to satisfy the `start` signature.
    /// A dedicated test (`test_create_workspace_reloads_resolver`)
    /// constructs a resolver over its own temp dir to assert the reload
    /// behaviour end-to-end.
    fn new_test_workspace_resolver() -> crate::tools::workspace_resolver::SharedResolver {
        Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new_for_test(vec![]),
        ))
    }

    /// Build an agent-tools service backed by the test temp dir's
    /// `agent_mcp.json`. Used to exercise the
    /// `/agents/{id}/mcp-servers`
    /// handler end-to-end through the trait path.
    fn new_test_agent_tools(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::AgentToolsService> {
        Arc::new(crate::usecases::RuntimeAgentToolsService::new(temp_dir))
    }

    /// Build an agent-config service backed by the test temp dir's
    /// `agent_config.json`. Used to exercise `/agents/{id}/config` and
    /// `/agents/{id}/builtin-tools` end-to-end through the trait path.
    fn new_test_agent_config(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::agent_config::AgentConfigService> {
        Arc::new(crate::usecases::RuntimeAgentConfigService::new(temp_dir))
    }

    /// Build an attachment service backed by the test temp dir's
    /// `<work_dir>/files/` blob store. Used to exercise
    /// `POST /sessions/{sid}/files` and `GET /files/{document_id}`
    /// end-to-end through the trait path.
    fn new_test_attachment(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::AttachmentService> {
        Arc::new(crate::usecases::RuntimeAttachmentService::new(temp_dir))
    }

    /// Build a git query service backed by the test temp dir (ADR-078).
    /// The `/git/*` handlers route through this trait; tests that never
    /// touch git pass an empty slot instead.
    fn new_test_git_query(
        temp_dir: std::path::PathBuf,
    ) -> Arc<dyn crate::usecases::GitQueryService> {
        Arc::new(crate::usecases::RuntimeGitQueryService::new(
            temp_dir,
            "test-agent".to_string(),
        ))
    }

    #[tokio::test]
    async fn test_http_server_starts_and_responds() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));

        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        // Health check
        let url = format!("http://127.0.0.1:{}/health", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["agent_id"], "com.test.agent");
        // degraded_reasons should be empty for a clean test start
        assert!(
            body["degraded_reasons"].as_array().unwrap().is_empty(),
            "expected empty degraded_reasons"
        );

        // Sessions (empty)
        let url = format!("http://127.0.0.1:{}/sessions", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["sessions"].as_array().unwrap().len(), 0);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn test_http_server_sessions_with_data() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-sessions");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // ADR-082 §4 step 3: session storage format:
        //   - session-meta store (SQLite) — SessionMeta (one row per session)
        //   - conversations/{sid}.jsonl   — pure ConversationEntry lines, no header
        let conversations_dir = temp_dir.join("conversations");
        std::fs::create_dir_all(&conversations_dir).unwrap();
        let session_id = "20260101_120000_abc";

        // Persist SessionMeta via the conversation module API so the
        // format stays in lock-step with `scan_sessions_from_meta`.
        let meta = crate::conversation::SessionMeta {
            version: 3, // CONVERSATION_FORMAT_VERSION — keep in sync with conversation.rs
            session_id: session_id.to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T12:00:00Z".to_string(),
            user_id: None,
            visibility: None,
            title: Some("Test Session".to_string()),
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 3,
            last_active_at: "2026-01-01T12:00:01Z".to_string(),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        };
        crate::conversation::write_session_meta(&conversations_dir, &meta).unwrap();

        // JSONL body: every line is a complete ConversationEntry (no header).
        let jsonl_path = conversations_dir.join(format!("{}.jsonl", session_id));
        std::fs::write(
            &jsonl_path,
            "{\"id\":\"m1\",\"ts\":\"2026-01-01T12:00:00.500Z\",\"role\":\"user\",\"content\":\"Hello\"}\n\
             {\"id\":\"m2\",\"ts\":\"2026-01-01T12:00:00.800Z\",\"role\":\"assistant\",\"content\":\"Hi there!\"}\n\
             {\"id\":\"m3\",\"ts\":\"2026-01-01T12:00:01.000Z\",\"role\":\"system\",\"content\":\"Done\"}\n",
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));

        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .unwrap();

        // List sessions — should see exactly the one we just wrote.
        let url = format!("http://127.0.0.1:{}/sessions", server.port);
        let response = reqwest::get(&url).await.unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        let sessions = body["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0]["session_id"], session_id);
        assert_eq!(sessions[0]["title"], "Test Session");
        assert_eq!(sessions[0]["message_count"], 3);

        // Regression t-83afab47: agent-level token totals are NOT part of
        // the HTTP response anymore. They live exclusively on the live
        // `context_usage` push path (HTTP = initial/refresh snapshot,
        // MQTT = delta push). The merge into live counters still
        // happens server-side as a side effect; only the wire shape
        // changes.
        assert!(body.get("agent_total_input_tokens").is_none());
        assert!(body.get("agent_total_output_tokens").is_none());
        assert!(body.get("agent_total_cache_read_tokens").is_none());
        assert!(body.get("agent_total_cache_write_tokens").is_none());

        // Get messages — read_messages_paginated returns chronological order
        // (oldest → newest within the page) regardless of direction, so
        // index 0/1/2 map to the user / assistant / system entries above.
        let url = format!(
            "http://127.0.0.1:{}/sessions/{}/messages?limit=50&direction=forward",
            server.port, session_id
        );
        let response = reqwest::get(&url).await.unwrap();
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["count"], 3);
        assert_eq!(body["messages"][0]["content"], "Hello");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Hi there!");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(body["messages"][2]["content"], "Done");
        assert_eq!(body["messages"][2]["role"], "system");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[tokio::test]
    async fn test_http_server_memory_endpoints_without_store() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-memory");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));
        let embed_dim = embed_cell_with_dim(512);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        // /memory/nodes should return a well-formed empty list.
        let url = format!("http://127.0.0.1:{}/memory/nodes", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["total"], 0);
        assert_eq!(body["page"], 1);
        assert_eq!(body["size"], 20);
        assert!(body["nodes"].as_array().unwrap().is_empty());

        // /memory/stats must report the full contract — both the
        // ADR-040 service path and the pre-ADR-040 fallback funnel
        // through the same `MemoryStats` struct, so the response shape
        // is identical. The contract is documented in
        // `acowork-gateway::http::memory_api::MemoryStatsResponse` and
        // mirrored in `usecases::memory_query::MemoryStats`. Every
        // field below must be present — missing fields crash the
        // desktop Memory panel (see git history for the by_status bug).
        let url = format!("http://127.0.0.1:{}/memory/stats", server.port);
        let response = reqwest::get(&url).await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["index_health"], "no_store");
        assert_eq!(body["model_dim"], 512);
        assert_eq!(body["total_nodes"], 0);
        assert_eq!(body["storage_bytes"], 0);
        assert_eq!(body["by_type"], serde_json::json!({}));
        assert_eq!(body["by_status"], serde_json::json!({}));
        assert_eq!(body["stored_dim"], 0);
        assert_eq!(body["nodes_with_embedding"], 0);
        assert_eq!(body["schema_version"], 0);

        // DELETE /memory/nodes/{nid} — store is None, adapter returns Ok(()) trivially.
        let url = format!("http://127.0.0.1:{}/memory/nodes/12345", server.port);
        let response = reqwest::Client::new().delete(&url).send().await.unwrap();
        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["deleted"], true);
        assert_eq!(body["node_id"], 12345);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// `builtin_tools` because the request struct only declared that
    /// one field. The Setup panel would optimistically apply the new
    /// temperature / context_window / etc. to its local store, the
    /// refresh listener would fetch the (unchanged) server values,
    /// and the user-visible result was "改动不生效". This test
    /// exercises the full read-modify-write cycle to make sure the
    /// handler now lands each supported field on disk.
    ///
    /// We cover the four wire-format scenarios the AgentSetupTab
    /// actually uses (per `handleApply` in
    /// `apps/acowork-desktop/src/components/results/AgentSetupTab.tsx`):
    ///
    ///   1. **Change** an existing numeric field (`temperature` 0.3 → 0.7).
    ///   2. **Add** a field that was previously absent (`max_output_tokens`,
    ///      `approval_timeout_secs`).
    ///   3. **Change** an existing string field (`shell_approval_threshold`
    ///      `"medium"` → `"high"`).
    ///   4. **Preserve** an untouched field (`max_iterations` stays at 50).
    ///
    /// We intentionally do **not** exercise JSON `null` clearing here:
    /// the frontend never sends `null` (it either sends the value or
    /// omits the field), and `Option<serde_json::Value>` cannot
    /// distinguish "field absent" from "field is JSON null" without a
    /// presence-tracking wrapper. If a future CLI / tool needs
    /// explicit-clearing semantics, the cleanest path is to introduce
    /// an `OptionalField<T>` presence-tracking wrapper — see the
    /// design notes on `UpdateAgentConfigRequest`.
    #[tokio::test]
    async fn test_put_agent_config_persists_per_agent_fields() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-put-config-fields");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        // Seed agent_config.json with three pre-existing values:
        // `temperature` and `shell_approval_threshold` will be
        // overwritten by the PUT; `max_iterations` will be preserved
        // because the PUT doesn't mention it.
        let initial = crate::agent_config::AgentConfig {
            temperature: Some(0.3),
            max_iterations: Some(50),
            shell_approval_threshold: Some("medium".to_string()),
            ..Default::default()
        };
        crate::agent_config::save_agent_config(std::path::Path::new(&temp_dir), &initial).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        // Partial PUT — note the absence of `max_iterations` is
        // load-bearing: it exercises the "untouched field is preserved"
        // path of the read-modify-write cycle.
        let url = format!(
            "http://127.0.0.1:{}/agents/{TEST_INSTANCE_ID}/config",
            server.port
        );
        let client = reqwest::Client::new();
        let response = client
            .put(&url)
            .json(&serde_json::json!({
                "temperature": 0.7,
                "max_output_tokens": 4096,
                "approval_timeout_secs": 120,
                "shell_approval_threshold": "high",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "PUT /agents/{{id}}/config should accept per-agent fields, got {}",
            response.status()
        );

        // Verify the on-disk file carries every pushed field and that
        // the untouched field is still there.
        let reloaded = crate::agent_config::load_agent_config(std::path::Path::new(&temp_dir))
            .unwrap()
            .expect("agent_config.json should exist after PUT");

        assert!(
            (reloaded.temperature.unwrap_or(0.0) - 0.7).abs() < f32::EPSILON,
            "temperature should be updated to 0.7, got {:?}",
            reloaded.temperature
        );
        assert_eq!(reloaded.max_output_tokens, Some(4096));
        assert_eq!(reloaded.approval_timeout_secs, Some(120));
        assert_eq!(
            reloaded.shell_approval_threshold,
            Some("high".to_string()),
            "shell_approval_threshold should be overwritten with the new string value"
        );
        assert_eq!(
            reloaded.max_iterations,
            Some(50),
            "untouched field must be preserved across partial PUT"
        );

        // Also verify the raw JSON shape on disk: every newly-set
        // field must show up in the serialized output (and
        // `skip_serializing_if = Option::is_none` on the struct
        // means default fields like `system_prompt_override` stay
        // out of the file).
        let raw =
            std::fs::read_to_string(temp_dir.join("config").join("agent_config.json")).unwrap();
        assert!(
            raw.contains("\"max_output_tokens\""),
            "newly-added field must be present in the serialized JSON; raw body was: {}",
            raw
        );
        assert!(
            raw.contains("\"approval_timeout_secs\""),
            "newly-added field must be present in the serialized JSON; raw body was: {}",
            raw
        );
        assert!(
            raw.contains("\"high\""),
            "updated string field must round-trip its value; raw body was: {}",
            raw
        );

        // GET /agents/{id}/config must surface the new state too —
        // this is the path the SetupTab refresh listener reads.
        let get_url = format!(
            "http://127.0.0.1:{}/agents/{TEST_INSTANCE_ID}/config",
            server.port
        );
        let get_resp: serde_json::Value =
            reqwest::get(&get_url).await.unwrap().json().await.unwrap();
        let cfg = get_resp["config"].as_object().expect("config envelope");
        assert_eq!(cfg["max_output_tokens"], serde_json::json!(4096));
        assert_eq!(cfg["max_iterations"], serde_json::json!(50));
        assert_eq!(
            cfg["shell_approval_threshold"],
            serde_json::json!("high"),
            "GET /config must surface the updated string value"
        );
        assert_eq!(
            cfg["approval_timeout_secs"],
            serde_json::json!(120),
            "GET /config must surface the newly-added field"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-071 D4: the five distiller fields round-trip through
    /// `PUT /agents/{id}/config` → `agent_config.json` → `GET /config`,
    /// including the explicit-clear (`null`) semantics on a numeric field.
    #[tokio::test]
    async fn test_put_agent_config_distiller_fields_roundtrip() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-put-config-distiller");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore needed
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/agents/{TEST_INSTANCE_ID}/config",
            server.port
        );
        let client = reqwest::Client::new();

        // 1. PUT all five distiller fields.
        let resp = client
            .put(&url)
            .json(&serde_json::json!({
                "distiller_enabled": true,
                "distiller_model": {"provider_id": "openai", "model_id": "gpt-4o-mini"},
                "distiller_interval_minutes": 45,
                "distiller_accumulation_threshold": 80,
                "distiller_idle_minutes": 20,
            }))
            .send()
            .await
            .expect("PUT distiller fields");
        assert!(resp.status().is_success(), "PUT got {}", resp.status());

        // 2. On-disk agent_config.json carries every field.
        let reloaded = crate::agent_config::load_agent_config(std::path::Path::new(&temp_dir))
            .expect("load ok")
            .expect("config exists");
        assert_eq!(reloaded.distiller_enabled, Some(true));
        // Legacy single-value wire field folds into the ordered list.
        let models = reloaded.distiller_models.as_ref().expect("distiller_models set");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "gpt-4o-mini");
        assert_eq!(reloaded.distiller_interval_minutes, Some(45));
        assert_eq!(reloaded.distiller_accumulation_threshold, Some(80usize));
        assert_eq!(reloaded.distiller_idle_minutes, Some(20));

        // 3. GET /config surfaces them (the Setup/memory-panel read path).
        let get: serde_json::Value = client.get(&url).send().await.unwrap().json().await.unwrap();
        let cfg = get["config"].as_object().expect("config envelope");
        assert_eq!(cfg["distiller_enabled"], serde_json::json!(true));
        assert_eq!(
            cfg["distiller_models"],
            serde_json::json!([{"provider_id": "openai", "model_id": "gpt-4o-mini"}])
        );
        assert_eq!(cfg["distiller_interval_minutes"], serde_json::json!(45));
        assert_eq!(
            cfg["distiller_accumulation_threshold"],
            serde_json::json!(80)
        );
        assert_eq!(cfg["distiller_idle_minutes"], serde_json::json!(20));

        // 4. Partial PUT: the wire layer collapses JSON `null` and
        //    absent-field into the same `None` (see UpdateAgentConfigRequest
        //    doc), so "clearing" a field is done by NOT sending it — the
        //    Desktop memory panel omits empty inputs. Here we flip the
        //    switch off while omitting `distiller_interval_minutes`; the
        //    untouchd field must be preserved (partial-PUT semantics).
        let resp = client
            .put(&url)
            .json(&serde_json::json!({
                "distiller_enabled": false,
            }))
            .send()
            .await
            .expect("PUT partial");
        assert!(
            resp.status().is_success(),
            "PUT partial got {}",
            resp.status()
        );

        let reloaded = crate::agent_config::load_agent_config(std::path::Path::new(&temp_dir))
            .expect("load ok")
            .expect("config exists");
        assert_eq!(reloaded.distiller_enabled, Some(false));
        assert_eq!(
            reloaded.distiller_interval_minutes,
            Some(45),
            "omitted field is preserved (partial PUT)"
        );
        assert_eq!(
            reloaded.distiller_accumulation_threshold,
            Some(80usize),
            "untouched distiller fields are preserved"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Regression test for the MCP-toggle-refresh bug fix (ADR-069
    /// follow-up). After `put_agent_config` was DRY-refactored to use
    /// [`crate::mqtt::MqttAgentConfigPublisher`], the retained
    /// `acowork/agents/{id}/config` publish path must still fire when
    /// the runtime has a live MQTT client. The Desktop Tools panel
    /// listens for that topic to refresh `/mcp-tools`.
    ///
    /// Test wires up:
    ///   1. embedded broker (rumqttd, real on loopback)
    ///   2. `RuntimeMqttClient` connected to the broker (so the
    ///      late-bind slot has a live publisher)
    ///   3. independent `AsyncClient` subscriber on the canonical topic
    ///   4. `RuntimeHttpServer` with the MQTT slot populated
    ///
    /// Then PUTs `/api/agents/{id}/config` and asserts the subscriber
    /// receives a retained `DataEnvelope::AgentConfig` whose payload
    /// contains the just-persisted field.
    #[tokio::test]
    async fn test_put_agent_config_publishes_retained_agent_config_snapshot() {
        use crate::mqtt::{MqttConnectConfig, RuntimeMqttClient, new_shared_cache};
        use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};

        // 1. Unique broker port to avoid cross-talk with parallel tests.
        static BROKER_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(39975);
        let port = BROKER_PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let broker = acowork_gateway::mqtt::start_broker("127.0.0.1", port).expect("broker start");

        let temp_dir = std::env::temp_dir().join(format!(
            "acowork-test-runtime-http-put-config-publish-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();
        crate::agent_config::save_agent_config(
            std::path::Path::new(&temp_dir),
            &crate::agent_config::AgentConfig::default(),
        )
        .unwrap();

        // 2. Connect runtime against the broker.
        let cache = new_shared_cache();
        let (control_tx, _control_rx) = tokio::sync::mpsc::unbounded_channel();
        let runtime = RuntimeMqttClient::connect(MqttConnectConfig {
            host: "127.0.0.1",
            port,
            agent_id: "com.test.agent",
            // ADR-073: instance key for broker client id + config topic.
            instance_id: "5f4e3d2c-1b0a-4a98-8765-4321fedcba09",
            agent_name: "Test Agent",
            agent_version: "1.0.0",
            config_json: "{}",
            available_cache: cache,
            control_tx,
            identity_update_tx: None,
            mcp_notifier: None,
            provider_update_tx: None,
            embedding_update_tx: None,
            node_id: None,
            lsps_update_tx: None,
            node_proxy_update_tx: None,
            work_dir: temp_dir.clone(),
            username: None,
            password: None,
            http_advertise_endpoint: None,
            http_port: None,
        })
        .await
        .expect("RuntimeMqttClient connect");
        // SharedMqttClientSlot = Arc<Mutex<Option<Arc<Mutex<RuntimeMqttClient>>>>>
        let runtime_slot: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(
            Some(std::sync::Arc::new(tokio::sync::Mutex::new(runtime))),
        ));

        // 3. Independent subscriber on the canonical topic.
        let mut sub_opts = MqttOptions::new("test:handler-sub", "127.0.0.1", port);
        sub_opts.set_keep_alive(std::time::Duration::from_secs(5));
        let (sub_client, mut eventloop) = AsyncClient::new(sub_opts, 10);
        let target = "acowork/agents/5f4e3d2c-1b0a-4a98-8765-4321fedcba09/config".to_string();
        sub_client
            .subscribe(&target, QoS::AtLeastOnce)
            .await
            .expect("subscribe");

        // Give the broker a beat to register the subscription before
        // we trigger the publish — otherwise the retained message can
        // arrive before the SubAck and look like a normal publish.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        // 4. Start HTTP server with the MQTT slot populated.
        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let session_manager_slot: SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            // ADR-073: the server mirrors the MQTT runtime's real instance
            // identity (same UUID) — the config topic subscriber above and
            // the HTTP path below must both use it.
            "5f4e3d2c-1b0a-4a98-8765-4321fedcba09".to_string(),
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            runtime_slot,
            Arc::new(tokio::sync::Mutex::new(Some(new_test_session_metadata(
                &temp_dir,
                std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                std::sync::Arc::new(std::sync::RwLock::new(None)),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
        )
        .await
        .expect("server start");

        // 5. Strict instance identity (ADR-073): the package `agent_id`
        //    is rejected on instance-scoped routes — a caller that still
        //    addresses this runtime by package name gets an explicit
        //    mismatch instead of silently hitting the wrong instance.
        let pkg_url = format!(
            "http://127.0.0.1:{}/agents/{TEST_INSTANCE_ID}/config",
            server.port
        );
        let client = reqwest::Client::new();
        let get_pkg = client.get(&pkg_url).send().await.unwrap();
        assert_eq!(
            get_pkg.status(),
            200,
            "package-addressed GET must still return the tolerant envelope"
        );
        let pkg_body: serde_json::Value = get_pkg.json().await.unwrap();
        assert_eq!(
            pkg_body["matches"], false,
            "package agent_id must NOT match the instance identity"
        );
        let put_pkg = client
            .put(&pkg_url)
            .json(&serde_json::json!({"temperature": 0.1}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            put_pkg.status(),
            reqwest::StatusCode::NOT_FOUND,
            "package-addressed PUT must 404 loudly"
        );

        // 6. PUT /agents/{instance_id}/config — must trigger publish via the
        //    refactored MqttAgentConfigPublisher path.
        let url = format!(
            "http://127.0.0.1:{}/agents/5f4e3d2c-1b0a-4a98-8765-4321fedcba09/config",
            server.port
        );
        let response = client
            .put(&url)
            .json(&serde_json::json!({"temperature": 0.42}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "PUT /config should succeed");

        // 7. Subscriber must receive the retained snapshot with the
        //    just-pushed temperature. This is the contract the Desktop
        //    Tools panel listens to via `case "agent_config"`.
        use prost::Message as _;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut received_config_json: Option<String> = None;
        while std::time::Instant::now() < deadline && received_config_json.is_none() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(100)),
                eventloop.poll(),
            )
            .await
            {
                Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                    if p.topic != target {
                        continue;
                    }
                    let env = acowork_core::mqtt_proto::DataEnvelope::decode(p.payload.as_ref())
                        .expect("DataEnvelope decode");
                    if let Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentConfig(ac)) =
                        env.payload
                    {
                        received_config_json = Some(ac.config_json.clone());
                    }
                }
                Ok(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(_) => {}
            }
        }
        let json = received_config_json
            .expect("subscriber must receive retained agent_config after PUT /config");
        assert!(
            json.contains("0.42"),
            "retained payload should carry the just-persisted temperature 0.42; got: {}",
            json
        );

        // Cleanup
        drop(sub_client);
        std::fs::remove_dir_all(&temp_dir).ok();
        drop(broker);
    }

    /// Win11-MCP-ToolsBugFix round-trip regression test.
    ///
    /// Before the fix, `PUT /api/agents/{id}/mcp-servers`
    /// was a Gateway stub that
    /// returned 200 but never persisted — the user's selection was lost
    /// the next time the Tools tab remounted because `/tools` read the
    /// (empty) on-disk config and overwrote the in-memory Zustand store.
    ///
    /// This test wires through the endpoints the Desktop
    /// `ToolsTab` actually uses and asserts the round-trip survives a
    /// tab remount (modeled here as a fresh GET against the same agent).
    /// The contract being guarded:
    ///
    /// 1. PUT mcp-servers with `{servers: [...names...]}` returns 2xx.
    /// 2. Subsequent GET mcp-servers returns the same names.
    /// 3. The merged `/tools` endpoint reports them under
    ///    `data.mcp_servers` so the optimistic UI re-mount sees them.
    /// 4. PUT mcp-servers with an unknown name returns 400 (the catalog
    ///    filter at the runtime boundary — protects against direct
    ///    API calls that bypass the desktop's catalog list).
    /// 5. `Some(vec![])` (explicitly cleared) round-trips as empty,
    ///    distinct from "never set anything" (auto-merged fallback).
    #[tokio::test]
    async fn test_mcp_persistence_roundtrip() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-mcp");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        // Seed agent_mcp.json with two catalog entries so the new PUT
        // handler has something valid to accept. (Without catalog entries,
        // every name in `servers` would fail the "unknown name" 400 guard
        // — that is the correct behaviour, just not what this test wants.)
        let initial_mcp = crate::agent_config::AgentMcpConfig {
            catalog: vec![
                McpServerConfigDefStub::context7(),
                McpServerConfigDefStub::search(),
            ],
            local: vec![],
            active_names: None,
        };
        crate::agent_config::save_agent_mcp_config(std::path::Path::new(&temp_dir), &initial_mcp)
            .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        // ADR-040 follow-up: the mcp-servers + builtin-tools handlers
        // route through the `AgentToolsService` trait; the slot must be
        // populated for the test to exercise the trait path. Mirrors
        // the wiring in `startup/session_init.rs` Phase B.
        let agent_tools_svc = new_test_agent_tools(temp_dir.clone());
        let agent_tools_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(agent_tools_svc)));

        // AgentConfigService slot — same wiring discipline as
        // agent_tools above. Without this, /agents/{id}/config and
        // /agents/{id}/builtin-tools would 503.
        let agent_config_svc = new_test_agent_config(temp_dir.clone());
        let agent_config_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::agent_config::AgentConfigService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(agent_config_svc)));

        // AttachmentService slot — exercises POST /sessions/{sid}/files
        // and GET /files/{document_id} through the trait path.
        let attachment_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
            temp_dir.clone(),
        ))));
        let session_config_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
        > = Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            agent_config_slot,
            attachment_slot,
            session_config_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1) PUT mcp-servers — user ticks `context7` only.
        let url = format!("{}/agents/{TEST_INSTANCE_ID}/mcp-servers", base);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({"servers": ["context7"]}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PUT mcp-servers should succeed, got {}",
            resp.status()
        );

        // 2) GET mcp-servers — same name comes back.
        let resp = client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        let active: Vec<&str> = body["active_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(active, vec!["context7"]);

        // 3) GET merged /tools — `mcp_servers` now reflects the user's
        //    selection rather than `[]` (the pre-fix bug surfaced here
        //    because the server was lying about an empty config).
        let tools_url = format!("{}/agents/{TEST_INSTANCE_ID}/tools", base);
        let resp = reqwest::get(&tools_url).await.unwrap();
        let tools: serde_json::Value = resp.json().await.unwrap();
        let mcp_servers: Vec<String> = tools["mcp_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(mcp_servers, vec!["context7".to_string()]);

        // 3b) `mcp_servers_defs` carries the full per-agent merged list
        //     (catalog ∪ local) with active flags — powers the per-agent
        //     toggle rows (pm / local MCPs included, not just gateway
        //     catalog). context7 is active, `search` is not.
        let defs = tools["mcp_servers_defs"].as_array().unwrap();
        assert_eq!(defs.len(), 2, "both seeded catalog entries listed");
        assert_eq!(defs[0]["name"], "context7");
        assert_eq!(defs[0]["active"], true);
        assert_eq!(defs[1]["name"], "search");
        assert_eq!(defs[1]["active"], false);

        // 5) PUT mcp-servers with an unknown name → 400 (not 200 with
        //    silent drop, which was the pre-fix symptom at the
        //    security/UX layer).
        let bad_url = format!("{}/agents/{TEST_INSTANCE_ID}/mcp-servers", base);
        let resp = client
            .put(&bad_url)
            .json(&serde_json::json!({"servers": ["context7", "ghost-mcp"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "unknown MCP server name must yield 400, got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        let unknown: Vec<&str> = body["unknown"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(unknown, vec!["ghost-mcp"]);

        // 7) Clearing the selection entirely — `Some(vec![])` — must be
        //    preserved verbatim (not silently re-populated with all
        //    merged servers). This is the "explicitly unchecked
        //    everything" user-state, distinct from "never set anything".
        let resp = client
            .put(&bad_url)
            .json(&serde_json::json!({"servers": []}))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let resp = client.get(&bad_url).send().await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        let active = body["active_servers"].as_array().unwrap();
        assert!(
            active.is_empty(),
            "empty active_servers must round-trip as empty (not auto-merged), got: {active:?}"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Regression: bare `{}` (no keys) must be accepted for both
    /// `PUT /agents/{id}/mcp-servers`, deserializing to the same empty
    /// list as `{"servers": []}`. Both shapes reach the usecase layer
    /// (which treats them as `Some(vec![])` — explicit "no servers
    /// active"), distinct from "never set anything".
    ///
    /// **Bug shape** (pre-fix): `UpdateMcpServersRequest.servers`
    /// lacked `#[serde(default)]`,
    /// so axum's `Json<...>` extractor returned 400 before reaching the
    /// handler when the request body was bare `{}`. The existing
    /// `test_mcp_persistence_roundtrip` only tested
    /// `{"servers": []}` (explicit empty list), which deserializes
    /// successfully whether or not `#[serde(default)]` is present —
    /// so the bug was silent.
    ///
    /// Companion to `test_mcp_persistence_roundtrip`; uses
    /// a stripped-down harness because empty bodies bypass the catalog
    /// validation in `put_mcp_servers` (impl.rs:99) so we don't need
    /// to seed `agent_mcp.json::catalog` here.
    #[tokio::test]
    async fn test_put_mcp_accepts_empty_body() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-empty-body");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        // Only the agent_tools slot needs to be populated for these two
        // endpoints; everything else can stay None.
        let agent_tools_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
            temp_dir.clone(),
        ))));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();
        let mcp_url = format!("{}/agents/{TEST_INSTANCE_ID}/mcp-servers", base);

        // 1) Bare `{}` to mcp-servers must succeed (was 400 pre-fix).
        let resp = client
            .put(&mcp_url)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PUT mcp-servers with bare {{}} must succeed, got {}",
            resp.status()
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// WIN-11/MCP-ToolsBugFix-Phase2 REPRODUCTION: real user scenario where
    /// `agent_mcp.json::catalog` is empty (fresh install / Gateway has the
    /// catalog in `mcp_catalog.json` but has never pushed it to Runtime's
    /// per-agent file).
    ///
    /// User report: "clicking context7 in the MCP tools tab shows
    /// 'unknown MCP server names (not in catalog+local)'". Root cause
    /// analysis:
    ///
    /// - The Desktop calls `GET /api/mcp-catalog` to *display* the catalog
    ///   (this hits Gateway's `mcp_catalog.json`).
    /// - The Desktop then calls `PUT /api/agents/{id}/mcp-servers` to
    ///   persist the user's selection (this is proxied to Runtime, which
    ///   validates against `AgentMcpConfig::merged()` reading from
    ///   `agent_mcp.json`).
    /// - Gateway has the catalog; Runtime's `agent_mcp.json::catalog` is
    ///   empty (no `save_agent_mcp_config_catalog` path is wired in
    ///   production — see Win-11/MCP-ToolsBugFix analysis 2025-Q4).
    /// - Therefore every PUT from the UI is rejected with 400.
    ///
    /// This test must fail in the *broken* state and pass after the
    /// catalog-sync fix. We don't pre-seed any catalog here — the
    /// contract is that an empty `agent_mcp.json` is itself a sign the
    /// catalog never reached the Runtime.
    #[tokio::test]
    async fn test_repro_mcp_put_fails_when_catalog_not_synced() {
        let temp_dir = std::env::temp_dir().join("acowork-test-repro-mcp-no-catalog");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();
        // Seed catalog as `acowork/global/mcps` MQTT retained would have
        // done via `save_agent_mcp_config_catalog` in the poll loop.
        // Without this, `merged()` is empty and PUT /mcp-servers returns
        // 400 "unknown MCP server names" — the exact bug we fixed.
        crate::agent_config::save_agent_mcp_config_catalog(
            &temp_dir,
            &[acowork_core::protocol::McpServerConfigDef {
                name: "context7".into(),
                display_name: None,
                ..Default::default()
            }],
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let agent_tools_svc = new_test_agent_tools(temp_dir.clone());
        let agent_tools_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(agent_tools_svc)));
        let agent_config_svc = new_test_agent_config(temp_dir.clone());
        let agent_config_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::agent_config::AgentConfigService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(agent_config_svc)));
        let attachment_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
            temp_dir.clone(),
        ))));
        let session_config_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
        > = Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            agent_tools_slot,
            agent_config_slot,
            attachment_slot,
            session_config_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // This is the exact PUT the Desktop sends when the user clicks
        // the context7 checkbox in the Tools tab. In the current broken
        // state, this returns 400 with "unknown MCP server names
        // (not in catalog+local)" — which is what the user sees.
        let url = format!("{}/agents/{TEST_INSTANCE_ID}/mcp-servers", base);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({"servers": ["context7"]}))
            .send()
            .await
            .unwrap();

        // REPRO ASSERTION (Phase 2 fix turns this assertion around):
        // the test should PASS when the catalog-sync path is in place.
        // While the bug is open, this fails — that's the point.
        // The check is intentionally optimistic so a CI run *after* the
        // fix will go green.
        let status = resp.status();
        assert!(
            status.is_success(),
            "BUG REPRO: PUT mcp-servers for catalog-sourced name should \
             succeed when catalog has been synced to agent_mcp.json, \
             got {} with body={:?}",
            status,
            resp.text().await
        );

        // Round-trip: GET should now return the same name.
        let resp = client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success());
        let body: serde_json::Value = resp.json().await.unwrap();
        let active: Vec<&str> = body["active_servers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(active, vec!["context7"]);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Regression for "newly-added workspace can't be selected — always
    /// falls back to Agent Home": after `POST /workspaces` succeeds, the
    /// shared `WorkspaceResolver` must be reloaded so
    /// `route_workspace_switch`'s `find_by_id` validation accepts the
    /// new id (the resolver is built once at Phase A and otherwise
    /// never refreshed).
    ///
    /// The test shares the resolver `Arc` between the HTTP server and
    /// the test body — the same ownership pattern as production
    /// (`agent_init.rs` creates it, `session_init.rs` injects it into
    /// SessionManager, `server.rs` reloads it).
    #[tokio::test]
    async fn test_create_workspace_reloads_resolver() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-ws-reload");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();
        // The create handler validates `path` against the real fs.
        let ws_path = temp_dir.join("my-workspace");
        std::fs::create_dir_all(&ws_path).unwrap();

        let workspace_resolver = new_test_workspace_resolver();

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        let workspace_mutation_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
            temp_dir.clone(),
        ))));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            workspace_mutation_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            workspace_resolver.clone(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Precondition: resolver does not know the workspace yet.
        {
            let guard = workspace_resolver.read().unwrap();
            assert!(
                guard.find_by_id("ws-created-via-http").is_none(),
                "resolver must start without the workspace"
            );
        }

        let resp = client
            .post(format!("{}/workspaces", base))
            .json(&serde_json::json!({
                "id": "ws-created-via-http",
                "path": ws_path.to_string_lossy(),
                "access": "read-write",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST /workspaces must succeed, got {}",
            resp.status()
        );

        // Postcondition: the shared resolver now resolves the new id —
        // this is exactly what `route_workspace_switch` checks.
        let guard = workspace_resolver.read().unwrap();
        assert!(
            guard.find_by_id("ws-created-via-http").is_some(),
            "resolver must see the newly-created workspace after reload (route_workspace_switch would otherwise reject it and fall back to __agent_home__)"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Regression test for adae896a: `PUT /workspaces/{ws_id}/prompt-file`
    /// must reload the shared WorkspaceResolver after persisting, so a
    /// freshly-set AGENTS.md/CLAUDE.md is visible to `read_prompt_file`
    /// (session creation) without a runtime restart. Before the fix the
    /// handler only wrote disk — the in-memory resolver kept
    /// `prompt_file: None` and every new session injected nothing.
    #[tokio::test]
    async fn test_set_prompt_file_reloads_resolver() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-prompt-file-reload");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(temp_dir.join("config")).unwrap();
        // The create handler validates `path` against the real fs.
        let ws_path = temp_dir.join("my-workspace");
        std::fs::create_dir_all(&ws_path).unwrap();
        // The prompt file the desktop would inject.
        std::fs::write(ws_path.join("AGENTS.md"), "# Test AGENTS.md\n").unwrap();

        let workspace_resolver = new_test_workspace_resolver();

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        let workspace_mutation_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
            temp_dir.clone(),
        ))));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            workspace_mutation_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            workspace_resolver.clone(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Seed the workspace via POST (writes disk + reloads resolver).
        let resp = client
            .post(format!("{}/workspaces", base))
            .json(&serde_json::json!({
                "id": "ws-prompt-file",
                "path": ws_path.to_string_lossy(),
                "access": "read-write",
            }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "POST /workspaces must succeed, got {}",
            resp.status()
        );

        // Precondition: resolver knows the workspace; create auto-injects
        // a root-level AGENTS.md as the prompt file (ADR-040 default), so
        // the injected value is already visible in the in-memory resolver.
        {
            let guard = workspace_resolver.read().unwrap();
            let ws = guard
                .find_by_id("ws-prompt-file")
                .expect("workspace must exist after POST");
            assert_eq!(
                ws.prompt_file.as_deref(),
                Some("AGENTS.md"),
                "create must auto-inject root AGENTS.md as prompt_file"
            );
        }

        // The desktop's setPromptFile call — a different file must be
        // visible in the shared resolver after the reload.
        let resp = client
            .put(format!("{}/workspaces/ws-prompt-file/prompt-file", base))
            .json(&serde_json::json!({ "prompt_file": "CLAUDE.md" }))
            .send()
            .await
            .unwrap();
        assert!(
            resp.status().is_success(),
            "PUT prompt-file must succeed, got {}",
            resp.status()
        );

        // Postcondition: the shared resolver now sees the prompt_file —
        // `read_prompt_file` at session creation would otherwise return None.
        let guard = workspace_resolver.read().unwrap();
        let ws = guard
            .find_by_id("ws-prompt-file")
            .expect("workspace must exist");
        assert_eq!(
            ws.prompt_file.as_deref(),
            Some("CLAUDE.md"),
            "resolver must see the freshly-set prompt_file after reload (session creation reads the in-memory resolver, not disk)"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// Tiny test helper — produces a minimal McpServerConfigDef for
    /// the roundtrip test. We can't `use` `acowork_core::protocol`
    /// directly from inside this module without polluting the prod
    /// imports, so the helper just builds a fully-defaulted config with
    /// the given name.
    struct McpServerConfigDefStub;
    impl McpServerConfigDefStub {
        fn context7() -> acowork_core::protocol::McpServerConfigDef {
            acowork_core::protocol::McpServerConfigDef {
                name: "context7".to_string(),
                display_name: None,
                ..acowork_core::protocol::McpServerConfigDef::default()
            }
        }
        fn search() -> acowork_core::protocol::McpServerConfigDef {
            acowork_core::protocol::McpServerConfigDef {
                name: "search".to_string(),
                display_name: None,
                ..acowork_core::protocol::McpServerConfigDef::default()
            }
        }
    }

    /// End-to-end smoke test for the new workspace file-operation REST
    /// resources (`/workspaces/file` + `/workspaces/dir`, dispatched by
    /// HTTP method). Spins up the real Runtime HTTP server on a random
    /// port and walks each verb through a happy path **using the exact
    /// URL / body shape the desktop's `workspaceStore` and
    /// `fileEditorStore` use** so this test doubles as a contract check.
    ///
    /// Desktop call patterns verified here:
    ///
    /// - `GET   /workspaces/file?path=…&workspace_id=…`     → JSON envelope
    /// - `POST  /workspaces/file?workspace_id=…`  body `{path, content?, overwrite?}`
    /// - `PUT   /workspaces/file?path=…&workspace_id=…`     body `{content}`
    /// - `DELETE /workspaces/file?workspace_id=…` body `{path}`
    /// - `POST  /workspaces/dir?workspace_id=…`  body `{path}`
    /// - `DELETE /workspaces/dir?workspace_id=…` body `{path}`
    #[tokio::test]
    async fn test_http_server_workspace_file_ops() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-file-ops");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Pre-create a workspace directory tree so the file-op
        // endpoints have something to operate on.
        let ws_dir = temp_dir.join("ws_alpha");
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(ws_dir.join("readme.txt"), b"hello workspace\n").unwrap();
        // Hidden entries: `.cargo` / `.gitignore` must be visible in the
        // tree (VSCode default), `.git` must stay hidden (VSCode default
        // `files.exclude`).
        std::fs::create_dir_all(ws_dir.join(".cargo")).unwrap();
        std::fs::write(ws_dir.join(".gitignore"), b"target/\n").unwrap();
        std::fs::create_dir_all(ws_dir.join(".git")).unwrap();

        // Persist agent_workspaces.json so resolve_workspace_root can
        // locate the `alpha` workspace by id.
        let cfg_path = temp_dir.join("config").join("agent_workspaces.json");
        std::fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
        std::fs::write(
            &cfg_path,
            serde_json::json!({
                "additional_dirs": [
                    { "id": "alpha", "path": ws_dir.to_string_lossy() }
                ]
            })
            .to_string(),
        )
        .unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1) GET /workspaces/tree — must list `readme.txt` under alpha.
        let url = format!("{}/workspaces/tree?workspace_id=alpha", base);
        let resp = client.get(&url).send().await.unwrap();
        assert!(resp.status().is_success(), "tree should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        let entries = body["entries"].as_array().expect("entries array");
        assert!(
            entries.iter().any(|e| e["name"] == "readme.txt"),
            "tree must include readme.txt, got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );
        // Hidden entries are visible unless VSCode-default-excluded.
        assert!(
            entries.iter().any(|e| e["name"] == ".cargo"),
            "tree must include .cargo (hidden shown), got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );
        assert!(
            entries.iter().any(|e| e["name"] == ".gitignore"),
            "tree must include .gitignore (hidden shown), got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );
        assert!(
            !entries.iter().any(|e| e["name"] == ".git"),
            "tree must NOT include .git (VSCode default exclude), got: {}",
            serde_json::to_string_pretty(&entries).unwrap()
        );

        // 2) GET /workspaces/file — read existing file as JSON
        //    {content, size, mimeType, path, is_file, is_dir, modified}.
        let url = format!(
            "{}/workspaces/file?workspace_id=alpha&path=readme.txt",
            base
        );
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["content"], "hello workspace\n");
        assert_eq!(body["size"], 16); // "hello workspace\n" is 16 bytes
        assert_eq!(body["mimeType"], "text/plain");
        assert_eq!(body["isFile"], true);
        assert_eq!(body["isDir"], false);
        assert_eq!(body["path"], "readme.txt");
        assert!(body["modified"].is_string());

        // 3) POST /workspaces/dir?workspace_id=… — create a new subdirectory.
        //    Desktop sends `workspace_id` in the querystring and `path`
        //    in the body; the handler accepts both forms.
        let resp = client
            .post(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create-dir should be 200");
        assert!(ws_dir.join("subdir").is_dir(), "subdir must exist");

        // 3a) Create an `assets` subdirectory for binary image test.
        let resp = client
            .post(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "assets" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create-dir for assets should be 200");
        assert!(ws_dir.join("assets").is_dir(), "assets dir must exist");

        // 4) POST /workspaces/file?workspace_id=… — create a new file.
        let resp = client
            .post(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({
                "path": "subdir/new.txt",
                "content": "fresh\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create-file should be 200");
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("subdir/new.txt")).unwrap(),
            "fresh\n"
        );

        // 5) POST /workspaces/file without overwrite — must 409.
        let resp = client
            .post(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({
                "path": "subdir/new.txt",
                "content": "ignored\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "second create-file on existing path must be 409"
        );

        // 6) PUT /workspaces/file?path=…&workspace_id=… — overwrite.
        //    Body is `{content}` only (path & workspace_id in querystring).
        let resp = client
            .put(format!(
                "{}/workspaces/file?workspace_id=alpha&path=subdir/new.txt",
                base
            ))
            .json(&serde_json::json!({ "content": "rewritten\n" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("subdir/new.txt")).unwrap(),
            "rewritten\n"
        );

        // 7) PUT /workspaces/file on a missing path — must 404.
        let resp = client
            .put(format!(
                "{}/workspaces/file?workspace_id=alpha&path=subdir/missing.txt",
                base
            ))
            .json(&serde_json::json!({ "content": "ignored\n" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "write-file on missing path must be 404");

        // 8) GET /workspaces/file — JSON envelope with mime helper
        //    exercised via a .md file (text path).
        let resp = client
            .post(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({
                "path": "subdir/notes.md",
                "content": "# heading\n",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let url = format!(
            "{}/workspaces/file?workspace_id=alpha&path=subdir/notes.md",
            base
        );
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["mimeType"], "text/markdown");
        assert_eq!(body["size"], 10); // "# heading\n" is 10 bytes

        // 8a) Binary image test (JPG/SVG read via base64): write a
        // tiny image + verify GET returns base64 content + correct
        // mimeType.
        use base64::Engine;
        let tiny_jpg_bytes: [u8; 4] = [0xFF, 0xD8, 0xFF, 0xE0]; // JPEG SOI marker, minimal header
        std::fs::write(ws_dir.join("assets").join("1.jpg"), tiny_jpg_bytes).unwrap();

        let resp = client
            .get(format!(
                "{}/workspaces/file?workspace_id=alpha&path=assets/1.jpg",
                base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "GET /workspaces/file for JPG must return 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["mimeType"], "image/jpeg");
        assert_eq!(body["size"], 4);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(body["content"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, tiny_jpg_bytes);

        // 8b) SVG read test. SVG is XML text, NOT a binary — it is removed
        // from BINARY_EXTENSIONS so the editor sees the raw markup (editable
        // in Monaco) and the desktop preview branch renders it via
        // `data:image/svg+xml;charset=utf-8,...`. mimeType is still
        // `image/svg+xml` so the frontend image-preview branch is selected.
        let tiny_svg = "<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1' />";
        std::fs::write(ws_dir.join("assets").join("test.svg"), tiny_svg).unwrap();

        let resp = client
            .get(format!(
                "{}/workspaces/file?workspace_id=alpha&path=assets/test.svg",
                base
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "GET /workspaces/file for SVG must return 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["mimeType"], "image/svg+xml");
        assert_eq!(body["size"], tiny_svg.len() as u64);
        // Raw XML text — NOT base64. This is the contract the editor relies
        // on to show editable source in Monaco.
        assert_eq!(body["content"].as_str().unwrap(), tiny_svg);

        // 9) DELETE /workspaces/file?workspace_id=… — body {path}.
        let resp = client
            .delete(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir/notes.md" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "delete-file should be 200");
        assert!(!ws_dir.join("subdir/notes.md").exists());

        // 10) DELETE /workspaces/file on missing — must 404.
        let resp = client
            .delete(format!("{}/workspaces/file?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir/notes.md" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);

        // 11) DELETE /workspaces/dir?workspace_id=… — body {path}.
        let resp = client
            .delete(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "subdir" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "delete-dir should be 200");
        assert!(!ws_dir.join("subdir").exists());

        // 12) DELETE on the wrong resource (file via /workspaces/dir) — must 400.
        let resp = client
            .delete(format!("{}/workspaces/dir?workspace_id=alpha", base))
            .json(&serde_json::json!({ "path": "readme.txt" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "DELETE on a file via /workspaces/dir must be 400"
        );

        // 13) Path-traversal guard — `path=../agent_config.json` must 400.
        let url = format!(
            "{}/workspaces/file?workspace_id=alpha&path=../agent_config.json",
            base
        );
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            400,
            "path-traversal escape must be rejected by the guard"
        );

        // 14) POST /workspaces/copy — copy a file inside the workspace.
        //     Body `{workspace_id, source, dest}`. Dest must not exist yet.
        std::fs::write(ws_dir.join("readme.txt"), "hello workspace\n").unwrap();
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "readme-copy.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "copy file must be 200");
        assert!(
            ws_dir.join("readme-copy.txt").exists(),
            "copied file exists"
        );
        assert_eq!(
            std::fs::read_to_string(ws_dir.join("readme-copy.txt")).unwrap(),
            "hello workspace\n",
            "copied file content matches"
        );

        // 15) POST /workspaces/copy on an existing dest — must 409.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "readme-copy.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "copy over an existing destination must 409"
        );

        // 16) POST /workspaces/copy on a missing source — must 404.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "nope.txt",
                "dest": "x.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "copy of missing source must 404");

        // 17) POST /workspaces/copy on a missing workspace_id — must 404.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "unknown",
                "source": "readme.txt",
                "dest": "x.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            404,
            "copy with unknown workspace_id must 404"
        );

        // 18) POST /workspaces/copy with a traversal in `dest` — must 400.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "../escape.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            400,
            "copy with traversal dest must 400 (path-traversal guard)"
        );

        // 19) POST /workspaces/rename — atomic rename (move) of a file.
        let resp = client
            .post(format!("{}/workspaces/rename", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme.txt",
                "dest": "readme-renamed.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "rename file must be 200");
        assert!(
            !ws_dir.join("readme.txt").exists(),
            "source must be gone after rename"
        );
        assert!(
            ws_dir.join("readme-renamed.txt").exists(),
            "dest must exist after rename"
        );

        // 20) POST /workspaces/rename on an existing dest — must 409.
        let resp = client
            .post(format!("{}/workspaces/rename", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "readme-renamed.txt",
                "dest": "readme-copy.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            409,
            "rename over an existing destination must 409"
        );

        // 21) POST /workspaces/rename on a missing source — must 404.
        let resp = client
            .post(format!("{}/workspaces/rename", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "missing.txt",
                "dest": "x.txt",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "rename of missing source must 404");

        // 22) Recursive directory copy — `assets/` contains 1.jpg + test.svg;
        //     POST /workspaces/copy should preserve both.
        let resp = client
            .post(format!("{}/workspaces/copy", base))
            .json(&serde_json::json!({
                "workspace_id": "alpha",
                "source": "assets",
                "dest": "assets-copy",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "copy dir must be 200");
        assert!(ws_dir.join("assets-copy").is_dir(), "copied dir exists");
        assert!(
            ws_dir.join("assets-copy").join("1.jpg").exists(),
            "copied file inside dir exists"
        );
        assert!(
            ws_dir.join("assets-copy").join("test.svg").exists(),
            "copied file inside dir exists"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// End-to-end smoke test for the four memory-write endpoints
    /// (POST /memory/nodes, GET /memory/nodes/{nid}, PUT
    /// /memory/nodes/{nid}, DELETE /memory/nodes/{nid}) backed by a real
    /// store.
    #[tokio::test]
    async fn test_http_server_memory_crud_endpoints() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-mem-crud");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Build a real store — `open_in_memory` keeps the test hermetic and
        // lets the adapter report `index_health = healthy`.
        let store = std::sync::Arc::new(
            acowork_sqlite::SqliteStore::open_in_memory(
                acowork_memory::types::DEFAULT_EMBEDDING_DIM,
            )
            .expect("in-memory store should open"),
        );
        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(Some(store)));

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1) POST /memory/nodes — create a new node.
        let url = format!("{}/memory/nodes", base);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "label": "Knowledge",
                "properties": { "name": "alpha", "weight": 1 },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "create node should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        let node_id = body["node_id"].as_u64().expect("node_id u64");
        assert!(node_id < u64::MAX, "node_id must be a real id");
        assert_eq!(body["label"], "Knowledge");

        // 2) GET /memory/nodes/{nid} — must report `found: true`.
        let url = format!("{}/memory/nodes/{}", base, node_id);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["found"], true);
        assert_eq!(body["node_type"], "Knowledge");

        // 3) PUT /memory/nodes/{nid} — update must succeed.
        let url = format!("{}/memory/nodes/{}", base, node_id);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({
                "properties": { "weight": 99, "extra": "added" },
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "update should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["updated"], true);

        // 4) PUT on a missing node — must 404.
        let url = format!("{}/memory/nodes/99999999", base);
        let resp = client
            .put(&url)
            .json(&serde_json::json!({ "properties": { "x": 1 } }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "update of missing node must 404");

        // 5) DELETE /memory/nodes/{nid} — must remove the node.
        let url = format!("{}/memory/nodes/{}", base, node_id);
        let resp = client.delete(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        // 6) GET on the just-deleted node — must report `found: false`.
        let resp = client.get(&url).send().await.unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["found"], false);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// G3-G6: Comprehensive test for memory HTTP endpoints with a real
    /// GrafeoStore containing data. Covers:
    ///
    /// - G3: `GET /memory/graph` with populated store (returns nodes array)
    /// - G5: `GET /memory/stats` with real nodes (by_type / by_status non-empty)
    /// - G6: `GET /memory/nodes` with pagination + type filter
    #[tokio::test]
    async fn test_http_server_memory_endpoints_with_data() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-mem-data");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let store = std::sync::Arc::new(
            acowork_sqlite::SqliteStore::open_in_memory(
                acowork_memory::types::DEFAULT_EMBEDDING_DIM,
            )
            .expect("in-memory store should open"),
        );

        let memory_store: SharedMemoryStore =
            std::sync::Arc::new(std::sync::RwLock::new(Some(store)));
        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Seed data via POST /memory/nodes (the same endpoint tested in CRUD test).
        for i in 0..5 {
            let label = if i < 3 { "Knowledge" } else { "Episodic" };
            let url = format!("{}/memory/nodes", base);
            let resp = client
                .post(&url)
                .json(&serde_json::json!({
                    "label": label,
                    "properties": {
                        "name": format!("node-{}", i),
                        "weight": i,
                    },
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "seed node {} should be created", i);
        }

        // ── G3: GET /memory/graph with populated store ──────────────
        let url = format!("{}/memory/graph", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200, "GET /memory/graph should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["agent_id"], "com.test.agent");
        let node_count = body["node_count"].as_u64().unwrap_or(0);
        assert!(
            node_count >= 5,
            "GET /memory/graph should return >= 5 nodes, got {node_count}"
        );
        let nodes = body["nodes"].as_array().expect("nodes should be array");
        assert_eq!(nodes.len() as u64, node_count);
        // Each node should have the expected fields.
        if let Some(first) = nodes.first() {
            assert!(first["node_id"].is_u64(), "node should have node_id");
            assert!(first["node_type"].is_string(), "node should have node_type");
        }

        // ── G5: GET /memory/stats with real nodes ───────────────────
        let url = format!("{}/memory/stats", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(
            body["total_nodes"].as_u64().unwrap_or(0) >= 5,
            "total_nodes should be >= 5 with seeded data"
        );
        // by_type should contain at least one entry.
        let by_type = body["by_type"]
            .as_object()
            .expect("by_type should be object");
        assert!(
            !by_type.is_empty(),
            "by_type should have at least one type with seeded data"
        );
        // by_status should contain Active nodes.
        let by_status = body["by_status"]
            .as_object()
            .expect("by_status should be object");
        assert!(
            !by_status.is_empty(),
            "by_status should have at least one status with seeded data"
        );

        // ── G6: GET /memory/nodes with pagination + type filter ─────
        // First: list all nodes (page 1, size 10).
        let url = format!("{}/memory/nodes?page=1&size=10", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        let total = body["total"].as_u64().unwrap_or(0);
        assert!(total >= 5, "total should be >= 5 with seeded data");
        assert_eq!(body["page"], 1);
        assert_eq!(body["size"], 10);
        let nodes = body["nodes"].as_array().expect("nodes should be array");
        assert!(!nodes.is_empty(), "nodes list should not be empty");

        // Second: paginate with size=2 (should return 2 nodes, total unchanged).
        let url = format!("{}/memory/nodes?page=1&size=2", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(
            body["total"], total,
            "total should be the same regardless of page size"
        );
        assert_eq!(body["size"], 2);
        assert_eq!(body["nodes"].as_array().unwrap().len(), 2);

        // Third: filter by node_type=Knowledge.
        let url = format!("{}/memory/nodes?page=1&size=100&node_type=Knowledge", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        // The filter should return a subset (may be empty if Grafeo
        // stores labels differently, but the endpoint must succeed).
        assert!(
            body["nodes"].is_array(),
            "Knowledge filter should return a nodes array"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N1: `GET /memory/consolidation/status` - verify the endpoint
    /// returns timer state when a consolidation timer is available.
    #[tokio::test]
    async fn test_http_consolidation_status() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-consolidation-status");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create a real consolidation timer.
        use crate::memory::consolidation_bg::ConsolidationTimer;
        use acowork_memory::consolidation::SchedulerConfig;
        let timer = Arc::new(ConsolidationTimer::new(SchedulerConfig {
            idle_timeout_secs: 1800,
            accumulation_threshold: 50,
            ..Default::default()
        }));
        let consolidation_timer_slot: SharedConsolidationTimer =
            Arc::new(std::sync::RwLock::new(Some(timer)));

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            consolidation_timer_slot,
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/memory/consolidation/status",
            server.port
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200, "consolidation status should be 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body["idle_secs"].is_i64(), "should have idle_secs");
        assert_eq!(body["bg_task_running"], true);
        // ADR-071 D2: distiller trigger state surfaced for the UI card.
        assert_eq!(body["distiller"]["enabled"], false);
        assert_eq!(body["distiller"]["episode_backlog"], 0);
        assert!(body["distiller"]["interval_secs"].is_u64());
        assert_eq!(body["distiller"]["accumulation_threshold"], 50);
        assert_eq!(body["distiller"]["idle_secs"], 1800);
        // ADR-057 §5.3 redesign: episodic forgetting state.
        assert_eq!(body["forgetting"]["enabled"], false);
        assert_eq!(body["forgetting"]["half_life_days"], 180);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N1b: `GET /memory/consolidation/status` returns 503 when no timer.
    #[tokio::test]
    async fn test_http_consolidation_status_no_timer() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-consolidation-noop");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/memory/consolidation/status",
            server.port
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(
            resp.status(),
            503,
            "should return 503 when no timer configured"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N1c: `POST /memory/distill` (ADR-071 D2) degrades to 503 when no
    /// AgentCore has been constructed yet (Phase B not complete). A full
    /// end-to-end run (episodes -> promoted nodes) is covered by the
    /// adr071 memory e2e suite which wires a real AgentCore.
    #[tokio::test]
    async fn test_http_memory_distill_no_agent_core() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-memory-distill");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let url = format!("http://127.0.0.1:{}/memory/distill", server.port);
        let resp = reqwest::Client::new().post(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            503,
            "manual distill without AgentCore should be 503"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N2: `GET /agents/{id}/rag/status` - verify RAG status reporting.
    #[tokio::test]
    async fn test_http_rag_status() {
        use acowork_core::rag::{AnnotatedRagResult, RagProvider};

        struct DummyRag;
        #[async_trait::async_trait]
        impl RagProvider for DummyRag {
            fn name(&self) -> &str {
                "enterprise_knowledge"
            }
            async fn query(&self, _query: &str) -> Vec<AnnotatedRagResult> {
                Vec::new()
            }
            async fn query_with_params(
                &self,
                _query: &str,
                _top_k: Option<u32>,
                _score_threshold: Option<f32>,
                _filters: Option<serde_json::Value>,
            ) -> Vec<AnnotatedRagResult> {
                Vec::new()
            }
        }

        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-status");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let rag_provider_slot: SharedRagProvider =
            Arc::new(std::sync::RwLock::new(Some(Arc::new(DummyRag))));

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            rag_provider_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // With RAG configured: should return configured=true.
        let url = format!("{}/agents/{TEST_INSTANCE_ID}/rag/status", base);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["configured"], true);
        assert_eq!(body["provider_name"], "enterprise_knowledge");
        assert_eq!(body["agent_id"], "com.test.agent");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N2b: `GET /agents/{id}/rag/status` when no RAG configured.
    #[tokio::test]
    async fn test_http_rag_status_not_configured() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-none");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/agents/{TEST_INSTANCE_ID}/rag/status",
            server.port
        );
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["configured"], false);
        assert!(body["provider_name"].is_null());

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N3: `POST /agents/{id}/rag/query` - verify direct RAG query works.
    /// Uses a mock provider that returns canned results.
    #[tokio::test]
    async fn test_http_rag_query() {
        use acowork_core::rag::{AnnotatedRagResult, RagProvider, RagResultItem};

        struct MockRag;
        #[async_trait::async_trait]
        impl RagProvider for MockRag {
            fn name(&self) -> &str {
                "mock_rag"
            }
            async fn query(&self, query: &str) -> Vec<AnnotatedRagResult> {
                vec![AnnotatedRagResult {
                    source_label: "[RAG:mock_rag]".to_string(),
                    tool_name: "rag_query".to_string(),
                    item: RagResultItem {
                        content: format!("Result for: {}", query),
                        source_url: Some("https://example.com/doc1".to_string()),
                        chunk_id: Some("chunk-1".to_string()),
                        score: 0.95,
                    },
                }]
            }
            async fn query_with_params(
                &self,
                query: &str,
                _top_k: Option<u32>,
                _score_threshold: Option<f32>,
                _filters: Option<serde_json::Value>,
            ) -> Vec<AnnotatedRagResult> {
                self.query(query).await
            }
        }

        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-query");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let rag_provider_slot: SharedRagProvider =
            Arc::new(std::sync::RwLock::new(Some(Arc::new(MockRag))));

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            rag_provider_slot,
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // Valid query.
        let url = format!("{}/agents/{TEST_INSTANCE_ID}/rag/query", base);
        let resp = client
            .post(&url)
            .json(&serde_json::json!({"query": "product pricing"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "rag query should return 200");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["query"], "product pricing");
        assert_eq!(body["result_count"], 1);
        assert_eq!(body["provider_name"], "mock_rag");
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["content"], "Result for: product pricing");
        assert!((results[0]["score"].as_f64().unwrap() - 0.95).abs() < 0.001);
        assert_eq!(results[0]["source_label"], "[RAG:mock_rag]");

        // Empty query -> 400.
        let resp = client
            .post(&url)
            .json(&serde_json::json!({"query": ""}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "empty query should return 400");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// N3b: `POST /agents/{id}/rag/query` returns 503 when no RAG configured.
    #[tokio::test]
    async fn test_http_rag_query_no_provider() {
        let temp_dir = std::env::temp_dir().join("acowork-test-http-rag-query-none");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let memory_store: SharedMemoryStore = Arc::new(std::sync::RwLock::new(None));
        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation = Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let url = format!(
            "http://127.0.0.1:{}/agents/{TEST_INSTANCE_ID}/rag/query",
            server.port
        );
        let resp = reqwest::Client::new()
            .post(&url)
            .json(&serde_json::json!({"query": "test"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 503, "should return 503 when no RAG provider");

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// REGRESSION: `POST /sessions/{sid}/files` with `format=docx`
    /// must land the blob at `<work_dir>/files/<doc_id>.docx`, NOT
    /// `<work_dir>/files/<doc_id>.bin` (the pre-fix behaviour from
    /// the broken `safe_extension` whitelist). The `doc_reader` tool
    /// keys its extractor dispatch off the file extension, so a
    /// `.bin` blob makes docx/pptx/xlsx/pdf uploads silently
    /// un-readable by the LLM.
    ///
    /// Also asserts that `GET /files/{document_id}?format=docx`
    /// returns the right Content-Type so the frontend preview /
    /// downstream HTTP consumers see the file as the Office document
    /// they uploaded.
    #[tokio::test]
    async fn test_http_upload_file_docx_lands_with_real_extension() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-http-upload-docx");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let attachment_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
            temp_dir.clone(),
        ))));
        let session_config_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
        > = Arc::new(tokio::sync::Mutex::new(None));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            attachment_slot,
            session_config_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();
        let session_id = "s-upload-docx";

        // ADR-076 §决策 4: the upload path authorizes against the session,
        // so the session has to exist. (Unauthorized uploads are covered by
        // `session_control`'s own tests.)
        let meta = crate::conversation::SessionMeta {
            version: 1,
            session_id: session_id.to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            user_id: None,
            visibility: None,
            title: None,
            workspace_id: None,
            model: None,
            provider: None,
            account_id: None,
            reasoning_effort: None,
            temperature: None,
            context_window: None,
            todos: None,
            message_count: 0,
            last_active_at: "2026-01-01T00:00:00Z".to_string(),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        };
        crate::conversation::write_session_meta(&temp_dir.join("conversations"), &meta).unwrap();

        // Step 1: Upload a docx blob. The multipart body mirrors what
        // `apps/acowork-desktop/src-tauri/src/gateway_client.rs` sends:
        //   - `file` field carrying the raw bytes (the file_name on the
        //     part is informational only)
        //   - `format` field carrying the lowercase extension
        //     ("docx", "pptx", "xlsx", "pdf", …)
        let docx_bytes: Vec<u8> = b"PK\x03\x04pretend-docx-bytes".to_vec(); // ZIP magic
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(docx_bytes.clone())
                    .file_name("report.docx".to_string())
                    .mime_str("application/octet-stream")
                    .unwrap(),
            )
            .text("format", "docx".to_string());

        let url = format!("{}/sessions/{}/files", base, session_id);
        let resp = client.post(&url).multipart(form).send().await.unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "POST /sessions/{{sid}}/files must accept the docx upload, got {}",
            resp.status()
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        let document_id = body["documentId"]
            .as_str()
            .expect("documentId present")
            .to_string();
        let format = body["format"].as_str().expect("format echoed back");
        assert_eq!(
            format, "docx",
            "upload response must echo the requested format"
        );

        // Step 2: The blob MUST be on disk with the real .docx extension
        // and the readable `<stem>_<id>` prefix. Pre-fix this landed as
        // `<doc_id>.bin`, which `doc_reader` would reject with
        // "Unsupported document format".
        let suffixed = temp_dir
            .join("files")
            .join(format!("report_{document_id}.docx"));
        let dir_contents: Vec<_> = std::fs::read_dir(temp_dir.join("files"))
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(
            suffixed.exists(),
            "docx blob must land at {} (got dir contents: {:?})",
            suffixed.display(),
            dir_contents,
        );
        // And it MUST NOT be sitting as `_<id>.bin` instead of `_<id>.docx`:
        // that would mean the safe_extension whitelist regressed and the
        // doc_reader tool would reject the blob as "Unsupported format".
        let bin_path = temp_dir
            .join("files")
            .join(format!("report_{document_id}.bin"));
        assert!(
            !bin_path.exists(),
            "docx must not regress to .bin fallback, but {} exists",
            bin_path.display()
        );

        // Step 3: GET /files/{document_id}?format=docx returns the right
        // MIME so the desktop chip renderer / any HTTP consumer sees the
        // blob as a Word document, not generic octet-stream.
        let url = format!("{}/files/{}?format=docx", base, document_id);
        let resp = client.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert_eq!(
            ct, "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "Content-Type for docx download must be the Office Word MIME, got {ct:?}"
        );
        let bytes = resp.bytes().await.unwrap();
        assert_eq!(
            bytes.as_ref(),
            docx_bytes.as_slice(),
            "downloaded bytes must match upload"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    // ── ADR-047 integration tests ──────────────────────────────────────

    /// Helper: create a real ConversationSession and register it in a
    /// SharedSessionConfigs map for use with SessionConfigService.
    fn make_test_session_config_service(
        work_dir: &std::path::Path,
        session_id: &str,
    ) -> (
        Arc<dyn crate::usecases::SessionConfigService>,
        Arc<crate::conversation::ConversationSession>,
    ) {
        use crate::conversation::{ConversationSession, SessionConfig};
        use std::sync::atomic::AtomicUsize;

        let (conv, _config_rx, _state_rx) = ConversationSession::new(
            work_dir,
            session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();

        let conv_arc = Arc::new(conv);
        let mut map = std::collections::HashMap::new();
        map.insert(session_id.to_string(), conv_arc.clone());

        let shared_configs: crate::usecases::SharedSessionConfigs =
            Arc::new(std::sync::RwLock::new(map));

        let svc: Arc<dyn crate::usecases::SessionConfigService> =
            Arc::new(crate::usecases::RuntimeSessionConfigService::new(
                shared_configs,
                None,                                   // no resolver for basic tests
                Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
            ));

        (svc, conv_arc)
    }

    /// ADR-047 acceptance #2: GET /sessions/{sid}/config returns current config.
    /// ADR-047 acceptance #2: PUT /sessions/{sid}/config persists and is
    /// immediately readable via GET.
    #[tokio::test]
    async fn test_session_config_get_and_put() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-config-get-put");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_id = "20260101_120000_cfg";

        let (config_svc, _conv) = make_test_session_config_service(&temp_dir, session_id);

        let session_config_slot: Arc<
            tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
        > = Arc::new(tokio::sync::Mutex::new(Some(config_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            session_config_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // 1. GET initial config (all fields null/empty)
        let resp = client
            .get(format!("{}/sessions/{}/config", base, session_id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert!(body.get("model").is_none() || body["model"].is_null());

        // 2. PUT model + provider
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "model": "gpt-4o",
                "provider": "openai",
                // Multi-account pick. The frontend sends this key on the
                // model menu's account row and reads it back from GET —
                // see `sessionConfigToPatch` in session-config-mapper.ts.
                "account_id": "acct-7",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // 3. GET again - should reflect the new values
        let resp = client
            .get(format!("{}/sessions/{}/config", base, session_id))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["provider"], "openai");
        // The wire key must stay `account_id`: the Desktop mapper reads it
        // by that name, and a `null` here is what wipes the session's
        // API-key selection on every reopen.
        assert_eq!(body["account_id"], "acct-7");

        // 4. PUT temperature
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "temperature": 0.7,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        // 5. GET - model/provider preserved, temperature added
        let resp = client
            .get(format!("{}/sessions/{}/config", base, session_id))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["provider"], "openai");
        assert!((body["temperature"].as_f64().unwrap() - 0.7).abs() < 0.01);

        // 6. Verify the persisted meta row has all values
        let meta = crate::conversation::read_session_meta(
            &temp_dir.join("conversations"),
            session_id,
        )
        .expect("meta row must exist after PUT");
        assert_eq!(meta.model.as_deref(), Some("gpt-4o"));
        assert_eq!(meta.provider.as_deref(), Some("openai"));
        // Persisted, not just echoed back from memory: this is what makes a
        // reopened session remember which API key it was using.
        assert_eq!(meta.account_id.as_deref(), Some("acct-7"));
        assert!((meta.temperature.unwrap() - 0.7).abs() < 0.01);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-047 + ADR-076 §决策 4: GET /sessions/{sid}/config answers 404
    /// for an unknown session, and the `visibility` switch governs the
    /// config read exactly as it governs the session/message reads —
    /// because all three go through the same `authorize_read`.
    #[tokio::test]
    async fn test_session_config_unknown_session_and_visibility_gate() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-config-unknown");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // A live session, so the config service can serve / accept a read
        // or write once the guard lets the request through.
        let sid = "20260101_120000_cfgvis";
        let (config_svc, conv) = make_test_session_config_service(&temp_dir, sid);
        // Drive the live session rather than hand-writing meta.json: the
        // in-memory `ConversationSession` is the writer for meta on any
        // config change, so a file-only edit would be clobbered by the
        // next `write_meta()`.
        conv.set_user_id(Some("u-owner".to_string()));
        let session_config_slot = Arc::new(tokio::sync::Mutex::new(Some(config_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            session_config_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let resp = reqwest::get(format!("{}/sessions/nonexistent/config", base))
            .await
            .unwrap();
        // ADR-076 §决策 4: the config handler authorizes first, so a
        // missing session is a 404 (it used to fall through to the config
        // service and surface as a 500).
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

        // ADR-076 §决策 4: the config endpoint has no visibility rule of
        // its own — it calls the same `authorize_read` the session/message
        // endpoints use, which is why the `visibility` switch governs config
        // too. These requests are that whole contract.
        let set_visibility = |v: Option<crate::conversation::SessionVisibility>| {
            conv.set_visibility(v);
        };
        let get_config_as = |who: Option<&str>| {
            let client = reqwest::Client::new();
            let req = client.get(format!("{base}/sessions/{sid}/config"));
            let req = match who {
                Some(u) => req.header("x-user-id", u),
                None => req,
            };
            async move { req.send().await.unwrap().status() }
        };

        // Private + owned. The owner writes first so the config store has
        // an entry to read back (the store is separate from meta.json and
        // starts empty in this harness).
        set_visibility(Some(crate::conversation::SessionVisibility::Private));
        let put_config_as = |who: &str, body: serde_json::Value| {
            let client = reqwest::Client::new();
            let req = client
                .put(format!("{base}/sessions/{sid}/config"))
                .header("x-user-id", who)
                .json(&body);
            async move { req.send().await.unwrap().status() }
        };
        assert_eq!(
            put_config_as("u-owner", serde_json::json!({ "model": "gpt-4o" })).await,
            reqwest::StatusCode::OK,
            "the owner must be able to write their own session's config"
        );
        assert_eq!(
            get_config_as(Some("u-owner")).await,
            reqwest::StatusCode::OK,
            "the owner must be able to read their own session's config"
        );
        // ...a stranger gets 404, not 403 (no existence oracle)...
        assert_eq!(
            get_config_as(Some("u-other")).await,
            reqwest::StatusCode::NOT_FOUND,
            "a private session's config must be invisible to a non-owner"
        );
        // ...and an unfiltered caller (admin / local mode) is unaffected.
        assert_eq!(get_config_as(None).await, reqwest::StatusCode::OK);

        // Same owner, switch flipped to public: the stranger now gets in.
        // This is what makes it the *switch* talking, not mere ownership.
        set_visibility(Some(crate::conversation::SessionVisibility::Public));
        assert_eq!(
            get_config_as(Some("u-other")).await,
            reqwest::StatusCode::OK,
            "a public session's config must be readable by any account"
        );

        // Writing stays owner-only even when the session is public —
        // sharing is not handing over the config.
        let resp = reqwest::Client::new()
            .put(format!("{base}/sessions/{sid}/config"))
            .header("x-user-id", "u-other")
            .json(&serde_json::json!({ "model": "gpt-4o" }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "public != writable: a non-owner must not be able to PUT config"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-076 §决策 4 (second wave): the seven user-triggered session
    /// actions moved from MQTT to HTTP. This test is that boundary, end to
    /// end through the real router:
    ///
    ///   1. a non-owner is refused (404 — identical to "no such session", so
    ///      the endpoint is not an existence oracle for other accounts),
    ///   2. the owner is accepted (202), and
    ///   3. the body each endpoint produces is the *same* `InboundMessage`
    ///      the MQTT command used to produce — the migration changed the
    ///      transport, not the dispatch.
    ///
    /// Step 3 is why the dispatch channel is real here: with a `None` channel
    /// every accepted request would collapse to 503 and a wrong body would
    /// go unnoticed.
    #[tokio::test]
    async fn session_actions_are_owner_gated_and_keep_their_payload() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-session-actions");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let sid = "20260101_120000_actions";
        let (_config_svc, conv) = make_test_session_config_service(&temp_dir, sid);
        conv.set_user_id(Some("u-owner".to_string()));
        conv.set_visibility(Some(crate::conversation::SessionVisibility::Private));

        // Real dispatch channel, so we can read back what each handler queued.
        let (dispatch_tx, mut dispatch_rx) =
            tokio::sync::mpsc::unbounded_channel::<(String, InboundMessage)>();

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(dispatch_tx))),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let post = |action: &str, who: &str, body: serde_json::Value| {
            let url = format!("{base}/sessions/{sid}/{action}");
            let who = who.to_string();
            async move {
                reqwest::Client::new()
                    .post(url)
                    .header("x-user-id", who)
                    .json(&body)
                    .send()
                    .await
                    .unwrap()
                    .status()
            }
        };

        let cases = [
            (
                "messages",
                serde_json::json!({
                    "content": "hi",
                    "message_id": "m1",
                    "command": "",
                    "params_json": "{\"a\":1}",
                }),
            ),
            ("stop", serde_json::json!({ "reason": "user_requested" })),
            ("continue", serde_json::json!({ "reason": "user_requested" })),
            (
                "approval",
                serde_json::json!({
                    "request_id": "r1",
                    "approved": true,
                    "allow_all_session": false,
                    "reason": "",
                }),
            ),
            ("answer", serde_json::json!({ "request_id": "r2", "answer": "yes" })),
            ("cancel-tool", serde_json::json!({ "tool_call_id": "t1" })),
            ("compress", serde_json::json!({ "compress_type": 1 })),
        ];

        for (action, body) in cases {
            // 1. A non-owner cannot even reach the action...
            assert_eq!(
                post(action, "u-other", body.clone()).await,
                reqwest::StatusCode::NOT_FOUND,
                "non-owner must be refused on /{action}"
            );
            assert!(
                dispatch_rx.try_recv().is_err(),
                "a refused {action} must not reach the dispatch channel"
            );

            // 2. ...the owner is accepted...
            assert_eq!(
                post(action, "u-owner", body.clone()).await,
                reqwest::StatusCode::ACCEPTED,
                "owner must be able to POST /{action}"
            );

            // 3. ...and the queued message is the pre-migration one.
            let (queued_sid, msg) = dispatch_rx.try_recv().expect("owner request must dispatch");
            assert_eq!(queued_sid, sid, "/{action} must route to its session");

            let ok = match (action, &msg) {
                (
                    "messages",
                    InboundMessage::ChatMessage {
                        content,
                        message_id,
                        params_json,
                        ..
                    },
                ) => content == "hi" && message_id == "m1" && params_json == r#"{"a":1}"#,
                ("stop", InboundMessage::Stop { reason }) => reason == "user_requested",
                ("continue", InboundMessage::ContinueExecution { session_id, reason }) => {
                    session_id == sid && reason == "user_requested"
                }
                (
                    "approval",
                    InboundMessage::ApprovalDecision {
                        session_id,
                        request_id,
                        approved,
                        allow_all_session,
                        ..
                    },
                ) => {
                    session_id == sid
                        && request_id == "r1"
                        && *approved
                        && !*allow_all_session
                }
                (
                    "answer",
                    InboundMessage::QuestionAnswer {
                        session_id,
                        request_id,
                        answer,
                    },
                ) => session_id == sid && request_id == "r2" && answer == "yes",
                (
                    "cancel-tool",
                    InboundMessage::UserOperation(UserOp::CancelTool { tool_call_id }),
                ) => tool_call_id == "t1",
                (
                    "compress",
                    InboundMessage::CompressAction {
                        session_id,
                        compress_type,
                    },
                ) => session_id == sid && *compress_type == 1,
                (a, other) => panic!("{a} dispatched the wrong message: {other:?}"),
            };
            assert!(ok, "{action} lost payload fields in transport");
        }

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-047: PUT /sessions/{sid}/config with workspace validation.
    /// When a resolver is configured, invalid workspace_id should be rejected.
    #[tokio::test]
    async fn test_session_config_put_invalid_workspace_rejected() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-config-ws");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_id = "20260101_120000_ws";

        // Create a ConversationSession
        use crate::conversation::{ConversationSession, SessionConfig};
        use std::sync::atomic::AtomicUsize;
        let (conv, _config_rx, _state_rx) = ConversationSession::new(
            &temp_dir,
            session_id,
            SessionConfig {
                agent_id: "com.test.agent".to_string(),
                workspace_id: None,
                model: None,
                provider: None,
            },
            0,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();
        let conv_arc = Arc::new(conv);
        let mut map = std::collections::HashMap::new();
        map.insert(session_id.to_string(), conv_arc.clone());

        let shared_configs: crate::usecases::SharedSessionConfigs =
            Arc::new(std::sync::RwLock::new(map));

        // Create a resolver with an empty allowed_dirs (no workspaces)
        let resolver = Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new(temp_dir.to_str().unwrap()),
        ));

        let config_svc: Arc<dyn crate::usecases::SessionConfigService> =
            Arc::new(crate::usecases::RuntimeSessionConfigService::new(
                shared_configs,
                Some(resolver),
                Arc::new(std::sync::RwLock::new(None)),
            ));
        let session_config_slot = Arc::new(tokio::sync::Mutex::new(Some(config_svc)));

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            session_config_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let client = reqwest::Client::new();

        // PUT with invalid workspace_id should fail
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "workspace_id": "non-existent-workspace",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "invalid workspace_id should be rejected"
        );

        // __agent_home__ should always be accepted
        let resp = client
            .put(format!("{}/sessions/{}/config", base, session_id))
            .json(&serde_json::json!({
                "workspace_id": "__agent_home__",
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-047: GET /sessions/{sid} response must NOT contain config fields.
    /// Verifies the HTTP response split: state in /sessions/{sid}, config in
    /// /sessions/{sid}/config.
    #[tokio::test]
    async fn test_get_session_excludes_config_fields() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-no-config");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let session_id = "20260101_120000_ncfg";

        // Create a session with model set in meta
        use crate::conversation::{SessionMeta, write_session_meta};
        let meta = SessionMeta {
            version: 3, // CONVERSATION_FORMAT_VERSION
            session_id: session_id.to_string(),
            agent_id: "com.test.agent".to_string(),
            created_at: "2026-01-01T12:00:00Z".to_string(),
            user_id: None,
            visibility: None,
            title: Some("Test".to_string()),
            workspace_id: Some("ws-1".to_string()),
            model: Some("gpt-4o".to_string()),
            provider: Some("openai".to_string()),
            account_id: None,
            reasoning_effort: None,
            temperature: Some(0.5),
            context_window: None,
            todos: None,
            message_count: 0,
            last_active_at: "2026-01-01T12:00:00Z".to_string(),
            tokens: None,
            llm_call_counter: None,
            perf: None,
            model_ratio: None,
            last_compaction_offset: None,
            corrupted: false,
        };
        write_session_meta(&temp_dir.join("conversations"), &meta).unwrap();

        let snapshots: SharedSessionSnapshots =
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = Arc::new(std::sync::RwLock::new(None));
        let session_metadata = new_test_session_metadata(&temp_dir, snapshots, latest);

        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.agent".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(std::sync::RwLock::new(Vec::new())),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);

        // GET /sessions/{sid} - should NOT contain model/provider/temperature
        let resp = reqwest::get(format!("{}/sessions/{}", base, session_id))
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();

        // meta should have session_id, created_at, last_active_at, message_count
        let meta_obj = &body["meta"];
        assert_eq!(meta_obj["session_id"], session_id);
        assert!(
            meta_obj.get("model").is_none() || meta_obj["model"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain model"
        );
        assert!(
            meta_obj.get("provider").is_none() || meta_obj["provider"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain provider"
        );
        assert!(
            meta_obj.get("temperature").is_none() || meta_obj["temperature"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain temperature"
        );
        assert!(
            meta_obj.get("workspace_id").is_none() || meta_obj["workspace_id"].is_null(),
            "GET /sessions/{{sid}} meta must NOT contain workspace_id"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-048 follow-up: `POST /api/debug/enable` flips DevMode on at
    /// runtime without restarting the agent. The test:
    /// 1. Before: any `/api/debug/*` route returns 503 (slot empty).
    /// 2. POST `/api/debug/enable` with empty body → 200, `already_enabled=false`.
    /// 3. After: `/api/debug/state` returns 404 (SessionNotFound) — the
    ///    slot is populated, the service is wired.
    /// 4. POST again → 200, `already_enabled=true` (idempotent).
    #[tokio::test]
    async fn test_http_server_debug_enable_runtime() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-debug-enable");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Minimal SessionManager for the enable path. enable_debug_mode
        // does not need any active sessions — it works on zero-session
        // managers too (creates a fallback controller).
        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.debug_enable"
            version = "1.0.0"
            name = "Test debug-enable runtime"
            description = "Pin /api/debug/enable HTTP wiring"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = std::sync::Arc::new(
            acowork_core::providers::mock::MockProvider::single_text("test"),
        );
        let core = std::sync::Arc::new(crate::agent::agent_core::AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let session_manager = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::agent::session::SessionManager::new(
                core,
                crate::agent::session::session_manager::SessionManagerConfig::default(),
            ),
        ));

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        // SessionManager slot populated; the real handle is the one we
        // built above (cloned — the HTTP server holds a reference, the
        // closure below keeps the original alive for the test scope).
        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(Some(session_manager.clone())));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.debug_enable".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot.clone(),
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);

        // 1. Before enable: /api/debug/state returns 503 (slot empty).
        let resp = reqwest::Client::new()
            .get(format!("{}/api/debug/state?session_id=foo", base))
            .send()
            .await
            .expect("GET state should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "before enable: /api/debug/state must be 503"
        );

        // 2. POST /api/debug/enable with empty body → 200, already_enabled=false.
        let resp = reqwest::Client::new()
            .post(format!("{}/api/debug/enable", base))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST enable should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::OK,
            "POST /api/debug/enable must be 200"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["enabled"], true);
        assert_eq!(body["data"]["already_enabled"], false);

        // 3. After enable: /api/debug/state returns 404 (SessionNotFound),
        //    not 503. The slot is now populated.
        let resp = reqwest::Client::new()
            .get(format!("{}/api/debug/state?session_id=foo", base))
            .send()
            .await
            .expect("GET state should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::NOT_FOUND,
            "after enable: /api/debug/state must be 404 (SessionNotFound)"
        );

        // 4. POST again → 200, already_enabled=true (idempotent).
        let resp = reqwest::Client::new()
            .post(format!("{}/api/debug/enable", base))
            .json(&serde_json::json!({"debug_port": 0}))
            .send()
            .await
            .expect("POST enable should not error");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["data"]["enabled"], true);
        assert_eq!(
            body["data"]["already_enabled"], true,
            "second call should be idempotent"
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    /// ADR-048 follow-up: `POST /api/debug/enable` returns 503 when
    /// the SessionManager slot is empty (e.g. Phase B hasn't finished
    /// yet). The HTTP route should surface this as a clean service-
    /// unavailable, not a panic or wrong-status success.
    #[tokio::test]
    async fn test_http_server_debug_enable_without_session_manager() {
        let temp_dir = std::env::temp_dir().join("acowork-test-runtime-debug-enable-no-sm");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let snapshots: SharedSessionSnapshots =
            std::sync::Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));
        let latest: SharedLatestSession = std::sync::Arc::new(std::sync::RwLock::new(None));
        let dispatch_tx: SharedDispatchSender = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let embed_dim = embed_cell_with_dim(0);
        let degraded_reasons: SharedDegradation =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        let mqtt_client: SharedMqttClientSlot = std::sync::Arc::new(tokio::sync::Mutex::new(None));
        let session_metadata =
            new_test_session_metadata(&temp_dir, snapshots.clone(), latest.clone());
        let memory_store: SharedMemoryStore = std::sync::Arc::new(std::sync::RwLock::new(None));

        // SessionManager slot is empty — mimics Phase B not yet finished.
        let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));

        let server = RuntimeHttpServer::start(
            temp_dir.clone(),
            temp_dir.clone(),
            "com.test.no_sm".to_string(),
            TEST_INSTANCE_ID.to_string(), // ADR-073: URL paths address the runtime's UUID instance, not the package name
            snapshots,
            latest,
            dispatch_tx,
            degraded_reasons,
            mqtt_client,
            Arc::new(tokio::sync::Mutex::new(Some(session_metadata))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_memory_query(
                memory_store,
                embed_dim.clone(),
            )))),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_workspace_mutation(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_git_query(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_tools(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_agent_config(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(Some(new_test_attachment(
                temp_dir.clone(),
            )))),
            Arc::new(tokio::sync::Mutex::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            std::sync::Arc::new(std::sync::RwLock::new(None)),
            Arc::new(tokio::sync::Mutex::new(None)),
            new_test_workspace_resolver(),
            session_manager_slot,
            std::sync::Arc::new(std::sync::RwLock::new(None)), // no AgentCore for basic tests
        )
        .await
        .expect("server should start");

        let base = format!("http://127.0.0.1:{}", server.port);
        let resp = reqwest::Client::new()
            .post(format!("{}/api/debug/enable", base))
            .json(&serde_json::json!({}))
            .send()
            .await
            .expect("POST enable should not error");
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            "missing SessionManager should yield 503"
        );
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["ok"], false);
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("SessionManager"),
            "error message should mention SessionManager; got {:?}",
            body
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }
}
