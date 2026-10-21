//! Agent boot context — intermediate state passed between startup phases.
//!
//! `AgentBootContext` carries all per-agent resources produced by Phase A
//! and consumed by Phase B, C, and D.  It avoids propagating dozens of
//! local variables through function signatures.

use std::sync::Arc;

use crate::agent::session::SessionManagerConfig;
use crate::agent::session_state::{SharedLatestSession, SharedSessionSnapshots};
use crate::config::RuntimeConfig;
use acowork_core::protocol::{AgentProviderConfig, ProtocolType};

/// Intermediate context produced by Phase A (per-agent initialization).
///
/// Contains all resources that are shared across sessions and needed by
/// subsequent phases.  Fields are `pub` so Phase B/C/D can consume them
/// without extra accessor indirection.
pub(crate) struct AgentBootContext {
    // Package & manifest
    pub loaded: crate::package::loader::LoadedPackage,

    // ── ADR-040: gRPC path removed. Only MQTT transport remains. ───

    // ADR-033: MQTT client (None when MQTT not available).
    // These are populated during startup and will be consumed when
    // session routing via control_handler is completed (Phase 4).
    #[allow(dead_code)]
    pub mqtt_client: Option<crate::mqtt::RuntimeMqttClient>,
    #[allow(dead_code)]
    pub available_cache: Option<crate::mqtt::SharedAvailableCache>,
    /// ADR-042: receiver for `acowork/global/user_profile` retained updates.
    /// Consumed by `gateway_loop::mqtt_only_loop` and forwarded to
    /// `SessionManager::update_user_identity`.
    pub identity_update_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<acowork_core::protocol::UserProfile>>,
    /// Receiver for `acowork/global/providers` updates.
    /// Consumed by `gateway_loop::mqtt_only_loop` and forwarded to
    /// `SessionManager::update_global_provider_list`.
    pub provider_update_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::ProviderUpdate>>,
    /// Receiver for `acowork/global/embedding_models` updates (ADR-033).
    /// Consumed by `gateway_loop::mqtt_only_loop` and forwarded to
    /// `SessionManager::handle_embedding_config_update` so sessions rebuild
    /// their embedding provider when the embed sidecar becomes ready or
    /// the active model switches.
    pub embedding_update_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::EmbeddingUpdate>>,
    /// Receiver for node LSP relay state changes (ADR-055 §6.7,
    /// Phase 4). Consumed by `gateway_loop::mqtt_only_loop` and
    /// forwarded to `SessionManager::handle_lsp_relay_update`.
    pub lsps_update_rx:
        Option<tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::LspRelayUpdate>>,
    /// Control command receiver (from MQTT control topics)
    pub control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>>,
    #[allow(dead_code)]
    pub runtime_http_port: Option<u16>,

    // LLM provider
    pub provider: Arc<dyn acowork_core::providers::traits::Provider>,
    /// Startup-resolved model (kept for future Phase 1/2/3 use).
    #[allow(dead_code)]
    pub resolved_model: String,
    /// All available models from the startup provider (kept for future Phase 1/2/3 use).
    #[allow(dead_code)]
    pub available_models: Vec<String>,
    pub protocol_type: ProtocolType,
    /// Provider ID resolved at startup (used to detect session mismatch).
    pub gateway_current_provider_id: Option<String>,

    /// Per-agent compatibility cache (persisted to
    /// `{work_dir}/config/provider_compat.json`).  Shared across all
    /// providers created for this agent, including those rebuilt by
    /// `build_provider_for` during 429-retry / session-resume.
    pub compat_cache: Option<Arc<crate::providers::compat::CompatCache>>,

    // Embedding provider (None when Tier-1 ONNX provider construction failed —
    // memory then degrades to text-only search via memory::manager fallback)
    pub emb_provider: Option<Arc<dyn crate::embedding::EmbeddingProvider>>,

    // Tools
    /// ADR-029: builtin tools as `BuiltinToolEntry` (each carries its
    /// `enabled` flag). Phase B reads this directly to seed
    /// [`crate::agent::AgentCore::builtin_tools`] via
    /// [`crate::agent::AgentCore::new`].
    pub active_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
    /// Full builtin specs including disabled ones (keyed by name),
    /// used by `/api/agents/{id}/builtin-tools` GET responses.
    pub full_tool_specs: Vec<(String, serde_json::Value)>,

    // ADR-034 §8 Phase 6 cleanup: legacy standalone-mode gRPC path removed.
    pub system_prompt: String,

    /// ADR-094: Progressive-mode skills summary (name + description list),
    /// carried separately from `system_prompt` so the caller can mount it
    /// as a distinct `ContextBuilder` section (`skill_instructions`).
    /// `None` in Manual mode or when the package has no `skills/`.
    pub skills_summary: Option<String>,

    /// ADR-053: agent-specific compaction prompt loaded from
    /// `prompts/summary.md` (package-level declaration). `None` → the
    /// built-in `COMPACTION_SYSTEM_PROMPT` fallback is used at compaction
    /// time. Loaded once in Phase A so Gateway and Standalone modes behave
    /// identically; each `AgentCore` constructor injects it into
    /// [`crate::agent::AgentCore::compaction_prompt`].
    pub compaction_prompt: Option<String>,

    /// Per-agent skill registry (`skills/*/SKILL.md`), loaded once in
    /// Phase A. The authoritative source for resolving a per-turn chat
    /// `command` (skill name) into skill instructions — the frontend only
    /// sends names. Injected into `AgentCore` in Phase B (see
    /// `AgentCore::skill_registry`); shared across session clones.
    pub skill_registry: Arc<crate::skills::parser::SkillRegistry>,

    // ── ADR-063: 7 additional package-level LLM prompt overrides ──
    //
    // Each field corresponds 1-to-1 to an entry in
    // `crate::package::prompt_builder::OVERRIDABLE_PROMPTS`. Phase A loads
    // all 8 (this one + the 7 below); Phase B injects them into
    // `AgentCore` as `Arc<RwLock<Option<String>>>` so the L1+L2 reload path
    // (Debug panel §3.7.5) can write them in place without recreating the
    // shared `Arc<AgentCore>` cloned into every session.
    //
    // The 7 entries below are loaded *only* if their file exists in
    // `prompts/` — `None` is the normal "no override" state and the LLM
    // call site resolves to the built-in constant via
    // `core.<field>.read().unwrap().as_deref().unwrap_or(const)`.
    /// Override for `crate::prompt::COMPACT_PROMPT`
    /// (package file: `prompts/compact-template.md`).
    /// Package authors MUST preserve the `{messages_text}` placeholder
    /// in their override — the runtime substitution at the call site
    /// panics with a clear error if the placeholder is missing.
    pub compact_template: Option<String>,

    /// Override for `crate::prompt::TITLE_PROMPT`
    /// (package file: `prompts/title.md`). Authors may customise
    /// `{language}` and `{user_message}` placeholders per their style.
    pub title_prompt: Option<String>,

    /// Override for `acowork-memory::manager::DEFAULT_ABSTENTION_PROMPT`
    /// (package file: `prompts/abstention.md`). Note: memory holds a
    /// process-level singleton; L2 reload is out of scope here too.
    ///
    /// **ADR-068 note**: Three grafeo-specific override fields
    /// (`extraction_prompt`, `conflict_classification_prompt`,
    /// `generalization_prompt`) were removed in this revision because
    /// ADR-068 rewrites the LLM→memory boundary — LLM writes only
    /// Episode nodes via `memory_store`; grafeo's offline
    /// `EpisodicDistiller` owns its own internal prompts.
    pub abstention_prompt: Option<String>,

    /// ADR-071 D7/D9: override for the offline distiller's merge-decision
    /// system prompt (package file: `prompts/distiller-merge.md`).
    /// `None` → the built-in `MERGE_SYSTEM_PROMPT` in
    /// `acowork_memory::consolidation::distiller` is used.
    pub distiller_merge_prompt: Option<String>,

    /// ADR-099: optional Chat Router persona (package file:
    /// `prompts/chat.md`). Replaces only the persona section of the
    /// chat-layer system prompt; `None` falls back to the manifest's
    /// `display_name` / `role` / `description`.
    pub chat_persona: Option<String>,

    // Shared handles
    pub memory_session: Arc<crate::memory::MemorySessionHandle>,
    pub mcp_notifier: Arc<crate::mcp_notify::McpConfigNotifier>,
    pub workspace_resolver: crate::tools::workspace_resolver::SharedResolver,

    /// ADR-051: RAG provider (None when manifest has no RAG tool declaration).
    /// Constructed in Phase A from manifest `RagToolConfig`; injected into
    /// `AgentCore` in Phase B. The `rag_query` tool is registered in the
    /// tool registry when this is `Some`.
    pub rag_provider: Option<Arc<dyn acowork_core::rag::RagProvider>>,

    // Context builder (standalone mode only)
    pub context_builder: Option<crate::agent::context::ContextBuilder>,

    // Session manager config fields
    pub identity_context: Option<String>,
    /// ADR-021: Single chunk channel for control events.
    pub chunk_tx: Option<tokio::sync::mpsc::Sender<crate::agent::loop_::SessionChunkEvent>>,
    pub chunk_rx: Option<tokio::sync::mpsc::Receiver<crate::agent::loop_::SessionChunkEvent>>,

    // Budget
    pub budget: acowork_core::Budget,

    /// Provider config loaded from agent_provider.json (for session validation).
    /// `None` when no provider config has been persisted yet (first start).
    pub provider_config: Option<AgentProviderConfig>,

    /// Shared session snapshot map, populated by Phase A and consumed by
    /// Phase B (via SessionManagerConfig). The same `Arc` is also held by
    /// the Runtime HTTP server so session state writes from AgentLoop are
    /// immediately visible to HTTP GET /sessions/{sid}/state.
    pub session_snapshots: SharedSessionSnapshots,

    /// ADR-047: Shared session config map for `SessionConfigService`.
    /// SessionManager populates this on session create/remove; the
    /// `RuntimeSessionConfigService` reads from it to serve
    /// `GET/PUT /sessions/{sid}/config`.
    pub session_configs: crate::usecases::SharedSessionConfigs,

    /// Shared latest session Arc, populated by Phase A and consumed by
    /// Phase B (via SessionManagerConfig). SessionManager writes to it on
    /// every session creation; the HTTP server reads from it for
    /// GET /sessions/latest.
    pub latest_session: SharedLatestSession,

    // Reconnect params (Gateway mode)
    pub agent_id: String,
    // ADR-034 §8 Phase 6 cleanup: gRPC reconnect param removed.
    /// ADR-033: Dispatch receiver for Runtime HTTP → agent loop.
    /// HTTP handlers send (session_id, InboundMessage); gateway loop
    /// forwards to the right session's AgentLoop.
    pub http_dispatch_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<(String, crate::agent::inbound::InboundMessage)>,
    >,

    /// ADR-033: Shared handle to the Grafeo memory store. Cloned into
    /// the Runtime HTTP server at Phase A and populated by Phase B once
    /// [`crate::agent::AgentCore::init_memory_store`] succeeds. Memory
    /// endpoints gracefully report "no store" when this is still `None`.
    pub memory_store_shared: crate::http::SharedMemoryStore,

    /// ADR-033: Shared embedding-provider dimension. Initialised to 0
    /// in Phase A and updated once the embed provider is built. Read
    /// by the memory-stats endpoint as `model_dim` for HNSW
    /// dimension-mismatch detection.
    ///
    /// The HTTP server already holds a clone of the same `Arc` passed
    /// Late-bind slot for the Runtime's MQTT client. Same lifecycle as
    /// `mqtt_client: Option<RuntimeMqttClient>` above, but the slot is
    /// the handle the HTTP server holds and that Phase C's DevMode
    /// wiring reads via `enable_debug_mode_and_fill_slot`. Phase A
    /// creates the slot and passes a clone to the HTTP server; Phase
    /// A also fills the slot once the MQTT connect succeeds. Phase C
    /// reads from it via `ctx.mqtt_client_slot`.
    pub mqtt_client_slot: crate::http::server::SharedMqttClientSlot,

    /// Startup-phase degradation reasons — non-fatal errors that
    /// degrade runtime capabilities (e.g. session persistence
    /// unavailable due to filesystem sandbox). Read by `/health`.
    /// Populated during Phase B; passed to the HTTP server in
    /// Phase A via the same `Arc`.
    pub degraded_reasons: crate::http::SharedDegradation,

    /// Late-bind slot for the `AgentCore`. Created empty in Phase A and
    /// passed to the Runtime HTTP server; populated by Phase B once
    /// `AgentCore::new` completes. The `list_sessions` handler uses it
    /// for ADR-028 agent-level token totals.
    pub agent_core_shared: crate::http::SharedAgentCore,

    /// ADR-040: Late-bind slot for session metadata service.
    pub session_metadata_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>>,
    /// ADR-040: Late-bind slot for memory query service.
    pub memory_query_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::MemoryQueryService>>>>,
    /// ADR-081 §4.2: Late-bind slot for the conversation vector index.
    pub conversation_index_slot: crate::http::SharedConversationIndex,
    /// ADR-040: Late-bind slot for workspace query service
    /// (read-only: `list_workspaces` / `list_tree` / `read_file` /
    /// `find_files` / `search_files`).
    pub workspace_query_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>>,
    /// ADR-040: Late-bind slot for workspace mutation service
    /// (workspace CRUD + file/dir mutation).
    pub workspace_mutation_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>>,
    /// ADR-078: Late-bind slot for the git query service (read-only
    /// `/git/status` `/git/diff` `/git/log`). Same sync-work_dir
    /// pattern as the workspace services — populated in Phase B.
    pub git_query_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::GitQueryService>>>>,
    /// ADR-040 follow-up: Late-bind slot for Tools-panel persistence
    /// (the `/agents/{id}/mcp-servers` and
    /// `/agents/{id}/builtin-tools` HTTP handlers). Populated in
    /// Phase B (sync — no async resource dependency like memory).
    pub agent_tools_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>>,
    /// ADR-040 follow-up: Late-bind slot for per-agent runtime config
    /// (`agent_config.json`) persistence. Mirrors `agent_tools_slot` —
    /// populated in Phase B (sync — no async resource dependency).
    pub agent_config_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>>,
    /// ADR-046: Late-bind slot for the attachment blob store
    /// (`<work_dir>/files/<document_id>`). Same Phase B pattern as
    /// `agent_tools_slot` / `agent_config_slot`.
    pub attachment_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>>,
    /// ADR-047: Late-bind slot for session config service
    /// (`GET/PUT /sessions/{sid}/config`). Populated in Phase B.
    pub session_config_slot:
        Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>>,
    /// Late-bind slot for the consolidation timer. Populated in Phase B
    /// after `AgentCore::start_consolidation_pipeline()` stores the timer.
    /// Used by `GET /memory/consolidation/status`.
    pub consolidation_timer_slot: crate::http::server::SharedConsolidationTimer,
    /// Late-bind slot for the RAG provider. Populated in Phase B from
    /// `AgentBootContext.rag_provider`. Used by `GET /agents/{id}/rag/status`
    /// and `POST /agents/{id}/rag/query`.
    pub rag_provider_slot: crate::http::server::SharedRagProvider,

    /// ADR-048: Late-bind slot for the Debug service. Populated in
    /// Phase B after SessionManager has wired up per-session debug
    /// controllers (DevMode must be active for this to be set; outside
    /// DevMode the slot stays empty and the `/api/debug/*` routes
    /// return 503 with "Debug service not ready").
    pub debug_service_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>>,

    /// Late-bind slot for `SessionManager`. Populated by Phase B once
    /// the session manager is constructed; cloned into the HTTP server
    /// via `RuntimeHttpServer::start` in Phase A so the runtime
    /// `POST /api/debug/enable` route can call
    /// `SessionManager::enable_debug_mode` without a restart. Same
    /// ADR-040 late-bind pattern as `debug_service_slot` and the other
    /// use-case slots.
    pub session_manager_slot: crate::http::server::SharedSessionManagerSlot,

    /// ADR-058: workspace FS watcher set. Created empty in Phase A
    /// (same Arc as the one cloned into the Runtime HTTP server);
    /// Phase C reconciles it against the shared `WorkspaceResolver`
    /// so every user-configured workspace gets an event publisher.
    pub workspace_watcher_set: crate::workspace::SharedWorkspaceWatcherSet,
}

/// Context produced by Phase B (per-session initialization).
///
/// Contains session-specific resources needed by Phase C and D.
pub(crate) struct SessionBootContext {
    // ADR-034 §8 Phase 6 cleanup: legacy standalone-mode gRPC path removed.
    //
    // Wrapped in `Arc<tokio::sync::Mutex<>>` so the IdleWatcher (a
    // detached background task that polls session activity on every
    // tick) can read the live state without competing for `&mut
    // session_manager` against the rest of the runtime. Locks are
    // short-lived (held only for the duration of `any_session_active`).
    pub session_manager: Arc<tokio::sync::Mutex<crate::agent::session::SessionManager>>,
    /// ADR-022: Shared committed-lines counter for the initial session.
    /// The writer thread increments this after each disk write.
    /// `AgentCore` adopts this Arc so `read_messages_since` sees the
    /// true on-disk line count.
    pub committed_lines: Arc<std::sync::atomic::AtomicUsize>,
}

/// Build a `SessionManagerConfig` from the boot context.
///
/// Called at the start of Phase B to avoid moving individual fields out of
/// `AgentBootContext` before the context might still be needed.
pub(crate) fn build_session_manager_config(
    ctx: &mut AgentBootContext,
    config: &RuntimeConfig,
) -> SessionManagerConfig {
    SessionManagerConfig {
        inbound_channel_capacity: 64,
        system_prompt: ctx.system_prompt.clone(),
        // ADR-094: forward the Progressive-mode skills summary so
        // sessions opened LATER inherit the same Progressive-mode summary
        // section. Mirrors the `system_prompt` field above.
        skill_summary: ctx.skills_summary.clone(),
        per_session_budget: ctx.budget.clone(),
        history_max_tokens: config.history_max_tokens,
        chunk_tx: ctx.chunk_tx.clone(),
        full_tool_specs: ctx.full_tool_specs.clone(),
        identity_context: ctx.identity_context.clone(),
        protocol_type: ctx.protocol_type.clone(),
        session_snapshots: Some(ctx.session_snapshots.clone()),
        latest_session: Some(ctx.latest_session.clone()),
        session_configs: Some(ctx.session_configs.clone()),
        // ADR-063: forward the package directory so `enable_debug_mode`
        // can hand it to `RuntimeDebugService::new_with_agent` (L2 reload).
        package_dir: ctx.loaded.package_dir.clone(),
    }
}
