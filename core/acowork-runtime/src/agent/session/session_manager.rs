//! SessionManager: lifecycle management for multiple concurrent sessions.
//!
//! Provides creation, destruction, and message routing for SessionTasks.
//! Each session runs as an independent tokio task, ensuring that one
//! session's work never blocks another.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acowork_core::Budget;
use acowork_core::protocol::ProtocolType;
use acowork_core::tools::traits::Tool;
use futures_util::FutureExt;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::agent::agent_core::AgentCore;
use crate::agent::inbound::{InboundMessage, UserOp};
use crate::agent::loop_::SessionChunkEvent;
use crate::agent::session::session_handle::SessionHandle;
use crate::agent::session::session_task::{SessionMessage, SessionTask};
use crate::agent::session_state::{
    SessionState, SessionStatus, SharedLatestSession, SharedSessionSnapshots,
};
use crate::agent_config::AgentConfig;
use crate::cancellation::CancelHandle;
use crate::config::DEFAULT_TEMPERATURE;
use crate::conversation::{ConversationSession, read_session_meta};
use crate::debug::controller::DebugController;
use crate::error::{Result, RuntimeError};
use crate::tools::mcp_manager::McpConnectionFailure;
use crate::tools::mcp_manager::McpManager;
use crate::tools::workspace_resolver::{WorkspaceResolver, format_workspace_context_for_session};
use acowork_mcp::client::McpRegistry;
use acowork_mcp::wrapper::McpToolWrapper;

/// Session lifecycle state observable from outside the manager (ADR-038).
///
/// Source of truth:
/// - `Active`: session is currently loaded in the in-memory `sessions` map.
/// - `Closed`: JSONL + meta files exist on disk, but the session is not in memory.
/// - `NotFound`: neither in memory nor on disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLifecycleState {
    NotFound,
    Closed,
    Active,
}

/// Outcome of an explicit `open()` call (ADR-038).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOpenOutcome {
    /// Session was already in memory; this call was a no-op.
    AlreadyActive,
    /// Session was on disk (Closed); we just resumed it into memory.
    ResumedFromDisk,
}

/// Configuration for SessionManager.
#[derive(Debug, Clone)]
pub struct SessionManagerConfig {
    /// Channel capacity for each session's inbound message queue
    pub inbound_channel_capacity: usize,
    /// System prompt to use for all sessions
    pub system_prompt: String,
    /// Per-session token budget
    pub per_session_budget: Budget,
    /// History max tokens per session
    pub history_max_tokens: u64,
    /// ADR-021: Single chunk sender for control events.
    /// When set, each session's AgentLoop forwards control events here
    /// so the caller can relay them to Gateway.
    pub chunk_tx: Option<mpsc::Sender<SessionChunkEvent>>,
    /// Full tool specs (name, schema) for ALL registered built-in tools.
    /// Stored so that tool definitions can be hot-rebuilt when `active_tools`
    /// changes without requiring access to the ToolRegistry (which is behind Arc).
    pub full_tool_specs: Vec<(String, serde_json::Value)>,
    /// Identity context string injected by Gateway for ContextBuilder.
    pub identity_context: Option<String>,
    /// LLM protocol type derived from models.dev (used for image token estimation)
    pub protocol_type: ProtocolType,

    /// Shared session snapshot map for the Runtime HTTP pull API.
    /// When `Some`, SessionManager registers each session's snapshot Arc
    /// here on creation and removes it on session destruction, so the
    /// Runtime HTTP server can serve `GET /sessions/{sid}/state`.
    pub session_snapshots: Option<SharedSessionSnapshots>,

    /// Shared latest session Arc for the Runtime HTTP pull API.
    /// When `Some`, SessionManager writes to this on every session creation
    /// and startup scan completion, so the Runtime HTTP server's
    /// `GET /sessions/latest` always returns the authoritative answer
    /// without file-system scanning.
    pub latest_session: Option<SharedLatestSession>,

    /// ADR-047: Shared session config map for `SessionConfigService`.
    /// When `Some`, SessionManager registers each session's
    /// `Arc<ConversationSession>` here on creation and removes it on
    /// session destruction, so the HTTP server can serve
    /// `GET/PUT /sessions/{sid}/config` without going through the
    /// serial inference queue.
    pub session_configs: Option<crate::usecases::SharedSessionConfigs>,

    /// ADR-063: the agent's `.agent` package directory (where
    /// `prompts/<file>.md` lives). Populated by
    /// `build_session_manager_config` from `AgentBootContext.loaded.package_dir`;
    /// `enable_debug_mode` passes it to `RuntimeDebugService::new_with_agent`
    /// so the L2 reload endpoint can re-read the 9 canonical filenames
    /// on demand. Empty `PathBuf` (= `Default::default()`) means no
    /// package configured — pre-R7 tests that exercise the trait
    /// without the reload path stay green; production wiring always
    /// sets this.
    pub package_dir: PathBuf,
}

impl Default for SessionManagerConfig {
    fn default() -> Self {
        Self {
            inbound_channel_capacity: 64,
            system_prompt: String::new(),
            per_session_budget: Budget {
                daily_tokens: None,
                monthly_tokens: None,
                daily_cost_usd: None,
                monthly_cost_usd: None,
                exceeded_action: "warn".to_string(),
            },
            history_max_tokens: 128_000,
            chunk_tx: None,
            full_tool_specs: Vec::new(),
            identity_context: None,
            protocol_type: ProtocolType::default(),
            session_snapshots: None,
            latest_session: None,
            session_configs: None,
            // ADR-063: empty default keeps pre-R7 tests green; production
            // wiring always populates this in `build_session_manager_config`.
            package_dir: PathBuf::new(),
        }
    }
}

/// Accumulated runtime config overrides pushed by Gateway via
/// `RuntimeConfigUpdate`. Applied on top of the shared `AgentCore` template
/// each time a new session is spawned, so config changes remain effective
/// for sessions created *after* the push (not only for sessions that were
/// already alive when the push arrived).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct RuntimeConfigOverrides {
    pub max_output_tokens: Option<u64>,
    pub max_iterations: Option<u32>,
    pub temperature: Option<f32>,
    pub context_window: Option<u64>,
    pub system_prompt_override: Option<String>,
    pub shell_approval_threshold: Option<String>,
    pub approval_timeout_secs: Option<u64>,
    /// ADR-061: minimum compression ratio for levels 1-7 (default 0.90 =
    /// "compress until at most 10% remains"). `None` = use the built-in
    /// default. See [`crate::agent::compression_constants::MIN_COMPRESSION_RATIO`].
    pub compression_ratio_threshold: Option<f64>,
    /// ADR-071 D4: EpisodicDistiller runtime switch (`agent_config.json`).
    pub distiller_enabled: Option<bool>,
    /// ADR-071 D4/D5: ordered distiller model candidate list
    /// (`agent_config.json`). `Some(vec![])` clears the runtime layer.
    pub distiller_models: Option<Vec<acowork_core::protocol::CompactModelRef>>,
    /// ADR-071 D4: distiller interval (minutes).
    pub distiller_interval_minutes: Option<u64>,
    /// ADR-071 D4: distiller accumulation threshold.
    pub distiller_accumulation_threshold: Option<usize>,
    /// ADR-071 D4: distiller idle threshold (minutes).
    pub distiller_idle_minutes: Option<u64>,
    /// Memory forgetting runtime switch (`agent_config.json`). Default off.
    pub memory_forgetting_enabled: Option<bool>,
    /// Episodic decay half-life (days).
    pub memory_forgetting_half_life_days: Option<u64>,
    /// Retention threshold below which an episodic node becomes Dormant.
    pub memory_forgetting_dormant_threshold: Option<f32>,
    /// Days a Dormant node is retained before archiving to the PurgeLog.
    pub memory_forgetting_archive_days: Option<u64>,
    /// Per-agent LLM session language override (e.g. `"zh-CN"`, `"en"`).
    /// When `Some(_)`, replaces `UserProfile.language` in the
    /// identity-context text block sent to the LLM (see
    /// `format_user_profile_context`). `None` = follow the global
    /// `UserProfile.language` (the existing default behaviour).
    pub session_language: Option<String>,
}

impl RuntimeConfigOverrides {
    /// Returns true when no override value has been set.
    pub fn is_empty(&self) -> bool {
        self.max_output_tokens.is_none()
            && self.max_iterations.is_none()
            && self.temperature.is_none()
            && self.context_window.is_none()
            && self.system_prompt_override.is_none()
            && self.shell_approval_threshold.is_none()
            && self.approval_timeout_secs.is_none()
            && self.compression_ratio_threshold.is_none()
            && self.distiller_enabled.is_none()
            && self.distiller_models.is_none()
            && self.distiller_interval_minutes.is_none()
            && self.distiller_accumulation_threshold.is_none()
            && self.distiller_idle_minutes.is_none()
            && self.memory_forgetting_enabled.is_none()
            && self.memory_forgetting_half_life_days.is_none()
            && self.memory_forgetting_dormant_threshold.is_none()
            && self.memory_forgetting_archive_days.is_none()
            && self.session_language.is_none()
    }

    /// Merge in a newer push. `Some` values replace; `None` preserves the
    /// previously cached override.
    pub fn merge(&mut self, other: &Self) {
        if other.max_output_tokens.is_some() {
            self.max_output_tokens = other.max_output_tokens;
        }
        if other.max_iterations.is_some() {
            self.max_iterations = other.max_iterations;
        }
        if other.temperature.is_some() {
            self.temperature = other.temperature;
        }
        if other.context_window.is_some() {
            self.context_window = other.context_window;
        }
        if other.system_prompt_override.is_some() {
            self.system_prompt_override = other.system_prompt_override.clone();
        }
        if other.shell_approval_threshold.is_some() {
            self.shell_approval_threshold = other.shell_approval_threshold.clone();
        }
        if other.approval_timeout_secs.is_some() {
            self.approval_timeout_secs = other.approval_timeout_secs;
        }
        if other.compression_ratio_threshold.is_some() {
            self.compression_ratio_threshold = other.compression_ratio_threshold;
        }
        if other.distiller_enabled.is_some() {
            self.distiller_enabled = other.distiller_enabled;
        }
        if other.distiller_models.is_some() {
            self.distiller_models = other.distiller_models.clone();
        }
        if other.distiller_interval_minutes.is_some() {
            self.distiller_interval_minutes = other.distiller_interval_minutes;
        }
        if other.distiller_accumulation_threshold.is_some() {
            self.distiller_accumulation_threshold = other.distiller_accumulation_threshold;
        }
        if other.distiller_idle_minutes.is_some() {
            self.distiller_idle_minutes = other.distiller_idle_minutes;
        }
        if other.memory_forgetting_enabled.is_some() {
            self.memory_forgetting_enabled = other.memory_forgetting_enabled;
        }
        if other.memory_forgetting_half_life_days.is_some() {
            self.memory_forgetting_half_life_days = other.memory_forgetting_half_life_days;
        }
        if other.memory_forgetting_dormant_threshold.is_some() {
            self.memory_forgetting_dormant_threshold = other.memory_forgetting_dormant_threshold;
        }
        if other.memory_forgetting_archive_days.is_some() {
            self.memory_forgetting_archive_days = other.memory_forgetting_archive_days;
        }
        if other.session_language.is_some() {
            self.session_language = other.session_language.clone();
        }
    }

    /// Apply this override onto an `AgentConfig` for persistence to disk.
    /// `Some` values are written; `None` preserves the on-disk value
    /// (so a partial push does not clobber unrelated fields).
    ///
    /// This is the **single source of truth** for the
    /// `RuntimeConfigOverrides → AgentConfig` mapping. Both the boot path
    /// (read-modify-write via `apply_runtime_config_override`) and the
    /// live-edit path (`RuntimeConfigUpdate` push from Gateway) should
    /// funnel through here to avoid schema-drift when new override
    /// fields are added.
    pub fn apply_to(&self, cfg: &mut AgentConfig) {
        if let Some(v) = self.max_output_tokens {
            cfg.max_output_tokens = Some(v);
        }
        if let Some(v) = self.max_iterations {
            cfg.max_iterations = Some(v);
        }
        if let Some(v) = self.temperature {
            cfg.temperature = Some(v);
        }
        if let Some(v) = self.context_window {
            cfg.context_window = Some(v);
        }
        if let Some(v) = &self.system_prompt_override {
            cfg.system_prompt_override = Some(v.clone());
        }
        if let Some(ref v) = self.shell_approval_threshold {
            cfg.shell_approval_threshold = Some(v.clone());
        }
        if let Some(v) = self.approval_timeout_secs {
            cfg.approval_timeout_secs = Some(v);
        }
        if let Some(v) = self.compression_ratio_threshold {
            cfg.compression_ratio_threshold = Some(v);
        }
        if let Some(v) = self.distiller_enabled {
            cfg.distiller_enabled = Some(v);
        }
        if let Some(v) = &self.distiller_models {
            cfg.distiller_models = Some(v.clone());
        }
        if let Some(v) = self.distiller_interval_minutes {
            cfg.distiller_interval_minutes = Some(v);
        }
        if let Some(v) = self.distiller_accumulation_threshold {
            cfg.distiller_accumulation_threshold = Some(v);
        }
        if let Some(v) = self.distiller_idle_minutes {
            cfg.distiller_idle_minutes = Some(v);
        }
        if let Some(v) = self.memory_forgetting_enabled {
            cfg.memory_forgetting_enabled = Some(v);
        }
        if let Some(v) = self.memory_forgetting_half_life_days {
            cfg.memory_forgetting_half_life_days = Some(v);
        }
        if let Some(v) = self.memory_forgetting_dormant_threshold {
            cfg.memory_forgetting_dormant_threshold = Some(v);
        }
        if let Some(v) = self.memory_forgetting_archive_days {
            cfg.memory_forgetting_archive_days = Some(v);
        }
        if let Some(ref v) = self.session_language {
            cfg.session_language = Some(v.clone());
        }
    }
}

impl From<&AgentConfig> for RuntimeConfigOverrides {
    /// Project the subset of `AgentConfig` fields that participate in the
    /// runtime override chain. Other `AgentConfig` fields (max_sessions,
    /// avatar, etc.) are applied separately via their own code paths.
    ///
    /// Pass-through projection: every field maps 1:1 from `AgentConfig`.
    fn from(cfg: &AgentConfig) -> Self {
        Self {
            max_output_tokens: cfg.max_output_tokens,
            max_iterations: cfg.max_iterations,
            temperature: cfg.temperature,
            context_window: cfg.context_window,
            system_prompt_override: cfg.system_prompt_override.clone(),
            shell_approval_threshold: cfg.shell_approval_threshold.clone(),
            approval_timeout_secs: cfg.approval_timeout_secs,
            compression_ratio_threshold: cfg.compression_ratio_threshold,
            distiller_enabled: cfg.distiller_enabled,
            distiller_models: cfg.distiller_models.clone(),
            distiller_interval_minutes: cfg.distiller_interval_minutes,
            distiller_accumulation_threshold: cfg.distiller_accumulation_threshold,
            distiller_idle_minutes: cfg.distiller_idle_minutes,
            memory_forgetting_enabled: cfg.memory_forgetting_enabled,
            memory_forgetting_half_life_days: cfg.memory_forgetting_half_life_days,
            memory_forgetting_dormant_threshold: cfg.memory_forgetting_dormant_threshold,
            memory_forgetting_archive_days: cfg.memory_forgetting_archive_days,
            session_language: cfg.session_language.clone(),
        }
    }
}

/// Debug mode handles injected at runtime when Gateway pushes
/// EnableDebugMode. Stored on SessionManager so that sessions
/// created *after* debug mode is enabled inherit the debug
/// controller, event sender, and notify handles.
///
/// Re-exported from `crate::debug::DebugHandles` for convenience.
use crate::debug::DebugHandles;

/// Lifecycle manager for multiple concurrent sessions.
///
/// Owns a shared `Arc<AgentCore>` template and creates `SessionTask`s
/// on demand. Each session gets an independent `SessionState` while
/// sharing the provider, tools, and config from the core template.
pub struct SessionManager {
    /// Shared agent core template for cloning into sessions
    core: Arc<AgentCore>,
    /// Active session handles, keyed by session ID
    sessions: HashMap<String, SessionHandle>,
    /// Configuration for session creation
    config: SessionManagerConfig,
    /// Runtime config overrides (accumulated from Gateway pushes) that
    /// must be re-applied to every newly created session.
    pub runtime_overrides: RuntimeConfigOverrides,
    /// Last `UserProfile` received via `update_user_identity`. Cached
    /// so that live-edits to `AgentConfig.session_language` can
    /// re-format the identity-context block for in-flight sessions
    /// (the formatted text is the only thing cached on every
    /// `ContextBuilder`; without this we couldn't re-render it).
    /// `None` until the first Gateway `UserProfileUpdate` push lands.
    last_user_profile: Option<acowork_core::protocol::UserProfile>,
    /// MCP tool wrappers, built when MCP servers are connected.
    /// Merged into each new session's tools at creation time.
    mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    /// ADR-030 C3: Dynamic builtin tools registered via SidecarEndpointUpdate
    /// (e.g. `codebase` when LSP relay becomes ready). Stored here so that
    /// **new** sessions created after a sidecar push also inherit them -
    /// mirrors the `mcp_tools` pattern (ADR-030 review ISSUE-1 fix).
    dynamic_builtin_tools: Vec<crate::agent::agent_core::BuiltinToolEntry>,
    /// MCP connection manager.
    mcp_manager: McpManager,
    /// Per-session pending workspace reference.
    /// When a session's workspace was deleted from the resolver,
    /// the session_id → old_ws_id mapping is moved here so it can be
    /// reconciled if the workspace is re-added.
    pending_workspaces: HashMap<String, String>,
    /// Default workspace ID for new sessions (no persisted workspace).
    /// Falls back to "__agent_home__" when no last_active workspace is set.
    default_workspace_id: String,
    /// Shared WorkspaceResolver for resolving workspace_id → filesystem path.
    /// Set once via `set_resolver()` after construction. When available,
    /// `set_session_workspace()` will also send `SetWorkDir` to the session
    /// so that `AgentCore::current_work_dir` is kept in sync automatically.
    resolver: Option<Arc<std::sync::RwLock<WorkspaceResolver>>>,
    /// Runtime-injected debug handles (set when Gateway pushes EnableDebugMode).
    /// When Some, new sessions inherit the debug controller, event sender,
    /// and notify handles. Existing sessions restart via urgent_interrupt
    /// and pick up these handles on their next agent_loop.run().
    pub(crate) runtime_debug_handles: Option<DebugHandles>,
    /// Per-session debug controllers, shared with DebugEventBus consumers
    /// request routing. Each session adds its controller when created with
    /// debug mode active.
    pub(crate) debug_controllers:
        Arc<tokio::sync::RwLock<HashMap<String, Arc<tokio::sync::Mutex<DebugController>>>>>,
    /// ADR-048: per-session event senders, populated when DevMode is
    /// enabled. `RuntimeDebugService` reads from this map to route
    /// handler-emitted events through the correct per-session sender
    /// (so each event is tagged with the originating session_id).
    pub(crate) debug_event_senders:
        Arc<tokio::sync::RwLock<HashMap<String, crate::debug::DebugEventSender>>>,
    /// ADR-048: the late-bind Debug service that HTTP routes consult
    /// via `debug_service_slot` in `AgentBootContext`. Built by
    /// `enable_debug_mode` once per-session controllers and senders
    /// are populated. `None` outside DevMode; HTTP routes return 503
    /// in that case.
    debug_service: Option<Arc<crate::usecases::RuntimeDebugService>>,
    /// Per-session urgent_stop Notify handles.
    /// Keyed by session_id; fire_urgent_stop() looks up the target session's
    /// Notify and wakes only that session's tokio::select! branches.
    urgent_stops: HashMap<String, Arc<Notify>>,
    /// ADR-044: Per-session cancellation tokens (the *new* single source of
    /// truth for "stop this session now"). Keyed by `session_id` so external
    /// signal sources — MQTT `ControlAction::StopGeneration` dispatcher, debug
    /// server `Stop`, CLI cancel, test harness — can locate the target
    /// session's token and call `cancel(CancellationReason::...)` on it.
    ///
    /// **Phase 3 (current state)**: registered in parallel with
    /// `urgent_stops`, with the MQTT `ControlAction::StopGeneration`
    /// dispatcher now firing `cancel(CancellationReason::UserStop)` here
    /// before forwarding the inbound message — see
    /// `startup/gateway_loop.rs::dispatch_inbound`. The token is the
    /// *active* stop-path for production code; `urgent_stops` remains
    /// wired only as a rollback safety net.
    ///
    /// **Phase 4**: `urgent_stops` removed; this map is the sole stop-path
    /// index. Until then both maps stay in sync via the same lifecycle
    /// hooks (insert on session creation, remove on close/delete/eviction).
    ///
    /// ADR-044 §4.5: keys are session IDs, values are `Arc<parking_lot::Mutex<CancelHandle>>`
    /// slots — *not* `CancelHandle` clones. The slot indirection is what
    /// makes the per-request boundary safe: external dispatchers read
    /// through the Arc on every cancel call and observe the latest
    /// generation (whatever `run_inner::begin_new_request` last wrote).
    /// Storing a plain clone here would freeze the generation to whatever
    /// was in the slot at registration time — the exact bug §4.5 fixes.
    cancel_handles: HashMap<String, Arc<parking_lot::Mutex<CancelHandle>>>,
    /// ADR-083: per-session compaction-cancel slots, keyed by `session_id`.
    ///
    /// A compaction cancel (`compress_type = 3`) must reach the compaction
    /// that is currently running — but the session task is *blocked* inside
    /// `compact_history_if_needed().await` while it runs, so an inbox message
    /// cannot deliver it. The MQTT dispatcher therefore fires this handle
    /// directly (same pattern as `cancel_handles` for Stop), and the
    /// compaction's `tokio::select!` observes the cancellation.
    ///
    /// Kept separate from `cancel_handles` so cancelling a compaction never
    /// crosses semantics with cancelling the whole request.
    compaction_cancel_handles: HashMap<String, Arc<parking_lot::Mutex<CancelHandle>>>,
    /// Per-session committed_lines counter, shared between the writer thread
    /// (ConversationWriter) and the session's SessionCore. Each session gets its
    /// own independent counter; `committed_lines_for(session_id)` returns the
    /// count for the correct JSONL file.
    session_committed_lines: HashMap<String, Arc<std::sync::atomic::AtomicUsize>>,
    /// ADR-025: Per-session delivery cursor — the backend tracks how much
    /// data has been delivered to the frontend.  The frontend never sends
    /// coordinates; it polls with `incremental=true` and the backend uses
    /// this cursor to determine what to return.
    session_delivery_cursors:
        std::sync::RwLock<HashMap<String, crate::conversation::DeliveryCursor>>,
    /// Shared streaming lines map (keyed by session_id), cloned into each
    /// SessionCore and used by the HTTP handler for `read_messages_since`.
    streaming_lines: crate::conversation::StreamingStateMap,

    /// Latest session ID and title, determined during the startup session scan
    /// (by `last_active_at` descending). Set once after the background scan
    /// completes; `None` until the scan finishes or if no sessions exist.
    latest_session: SharedLatestSession,
    /// ADR-047: Shared session config map for `SessionConfigService`.
    session_configs: crate::usecases::SharedSessionConfigs,
    /// ADR-063: the agent's `.agent` package directory. See
    /// `SessionManagerConfig::package_dir` for the rationale; copied out
    /// at construction so `enable_debug_mode` can hand it to
    /// `RuntimeDebugService::new_with_agent` without holding onto the
    /// whole config (which owns the `chunk_tx` mpsc Sender consumed
    /// elsewhere).
    package_dir: PathBuf,
    /// Design 27 §3.2/§6.1: intent-context → execution-session routing
    /// table (chat/conv keys). Loaded from the work dir so a restart
    /// keeps follow-up turns on the session that holds the conversation.
    /// Shared (`Arc<Mutex<_>>`) with every session task so an intent turn
    /// can stamp its rotation bookkeeping (§2.1) at turn end.
    intent_map: Arc<std::sync::Mutex<super::intent_routing::IntentSessionMap>>,
}

impl SessionManager {
    /// Create a new SessionManager with the given shared core and config.
    pub fn new(core: Arc<AgentCore>, config: SessionManagerConfig) -> Self {
        // Extract shared Arc before config is moved into self.config.
        let latest_session = config
            .latest_session
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(None)));

        let session_configs = config
            .session_configs
            .clone()
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(HashMap::new())));

        // ADR-063: clone out the package directory BEFORE `config` is
        // moved into `self.config` (struct literal consumes it). Same
        // pattern as the two Arc clones above.
        let package_dir = config.package_dir.clone();

        // Design 27 §3.2: routing table lives next to the agent's other
        // work-dir state; load-or-empty, never fatal.
        let intent_map = super::intent_routing::IntentSessionMap::load(
            std::path::Path::new(&core.config.work_dir),
        );

        Self {
            core,
            sessions: HashMap::new(),
            config,
            runtime_overrides: RuntimeConfigOverrides::default(),
            last_user_profile: None,
            mcp_tools: None,
            dynamic_builtin_tools: Vec::new(),
            mcp_manager: McpManager::new(),
            pending_workspaces: HashMap::new(),
            default_workspace_id: "__agent_home__".to_string(),
            resolver: None,
            runtime_debug_handles: None,
            debug_controllers: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            debug_event_senders: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
            debug_service: None,
            urgent_stops: HashMap::new(),
            cancel_handles: HashMap::new(),
            compaction_cancel_handles: HashMap::new(),
            session_committed_lines: HashMap::new(),
            session_delivery_cursors: std::sync::RwLock::new(HashMap::new()),
            streaming_lines: Arc::new(std::sync::RwLock::new(HashMap::new())),
            latest_session,
            session_configs,
            package_dir,
            intent_map: Arc::new(std::sync::Mutex::new(intent_map)),
        }
    }

    /// Set the shared WorkspaceResolver.
    ///
    /// Must be called once after construction (before any session is created)
    /// so that `set_session_workspace()` can resolve workspace IDs to actual
    /// filesystem paths and send `SetWorkDir` to sessions.
    pub fn set_resolver(&mut self, resolver: Arc<std::sync::RwLock<WorkspaceResolver>>) {
        self.resolver = Some(resolver);
    }

    /// Design 27 §6.1 + §2.1: resolve (or create) the execution session that
    /// owns an intent context for this agent. `key` comes from
    /// `intent_routing::routing_key`. A mapped session that is still Active
    /// is reused; a rotation is taken when §2.1 says so —
    /// T2 (the previous turn compacted) or T1 (idle past the threshold) —
    /// or when the mapping is stale (closed / evicted). The successor starts
    /// empty with a pointer handoff naming the old session; the conversation
    /// itself lives in the Chat session store, so nothing is copied.
    /// Returns `(session_id, handoff_note)` — the caller prepends the note to
    /// the routed intent when a rotation just happened. `seed` is the
    /// initiator's optional `session_config` suggestion (design 27 §4);
    /// clamped to this agent's provider list (§13-6) and applied only when
    /// a session is actually created (fresh mapping or rotation successor) —
    /// a reused session keeps its own configuration.
    pub async fn route_intent_session(
        &mut self,
        key: &str,
        seed: Option<super::intent_routing::SessionSeed>,
    ) -> Result<(String, Option<String>)> {
        let work_dir = std::path::PathBuf::from(&self.core.config.work_dir);
        let (mapped, rotation) = {
            let map = self
                .intent_map
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mapped = map.get(key).map(|e| e.session_id.clone());
            let rotation =
                mapped.as_ref().and_then(|_| {
                    map.needs_rotation(key, super::intent_routing::idle_rotate_threshold(&work_dir))
                });
            (mapped, rotation)
        };
        let seed = seed.and_then(|s| {
            s.clamp_to_responder(&self.core.global_provider_list.read().unwrap())
        });
        if let Some(sid) = &mapped {
            if self.get_session(sid).is_some() {
                match rotation {
                    None => return Ok((sid.clone(), None)),
                    Some(reason) => {
                        // §2.1 crash recovery: allocate + persist the
                        // successor id *before* creating the session, so a
                        // crash between the two steps replays to the same
                        // id — never a second successor, never a new orphan.
                        let planned = {
                            self.intent_map
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .plan_rotation(key)?
                        };
                        let new_sid = self
                            .create_agent_owned_session_with_id(
                                None,
                                Some(&planned),
                                seed.as_ref(),
                            )
                            .await?;
                        self.intent_map
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .rekey(key, new_sid.clone())
                            .map_err(|e| {
                                RuntimeError::Config(format!("intent routing persist failed: {e}"))
                            })?;
                        tracing::info!(
                            key,
                            old_session = %sid,
                            new_session = %new_sid,
                            reason = ?reason,
                            "Intent session rotated (design 27 §2.1)"
                        );
                        // §2.1 Handoff grading: T2 carries the old session's
                        // last persisted compaction summary (already an LLM
                        // distillation — no second LLM call at rotation);
                        // T1 stays a pointer (design: no conversation summary).
                        let summary = (reason == super::intent_routing::RotateReason::T2Compaction)
                            .then(|| {
                                crate::conversation::last_compaction_summary(&work_dir, sid)
                            })
                            .flatten();
                        return Ok((new_sid, Some(super::intent_routing::rotation_handoff(
                            reason,
                            sid,
                            summary.as_deref(),
                            super::intent_routing::handoff_max_tokens(&work_dir),
                        ))));
                    }
                }
            }
            tracing::info!(
                key,
                stale_session = %sid,
                "Intent routing entry points at a non-active session — opening a fresh one"
            );
        }
        let new_sid = self
            .create_agent_owned_session(None, seed.as_ref())
            .await?;
        self.intent_map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), new_sid.clone())
            .map_err(|e| RuntimeError::Config(format!("intent routing persist failed: {e}")))?;
        Ok((new_sid, None))
    }

    /// Design 27 §6.2 T2-a: a PM task reaching a terminal state ends its
    /// logical execution context — close the mapped session, no successor,
    /// drop the mapping. Idempotent: unknown keys are a no-op.
    pub async fn close_intent_context(&mut self, key: &str) -> Result<()> {
        let mapped = self
            .intent_map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map(|e| e.session_id.clone());
        if let Some(sid) = mapped {
            // `close_session` errors when the session already exited —
            // idempotent close, log-and-continue.
            if let Err(e) = self.close_session(&sid).await {
                tracing::debug!(key, session_id = %sid, error = %e, "intent context session already closed");
            }
            self.intent_map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key)
                .map_err(|e| RuntimeError::Config(format!("intent routing persist failed: {e}")))?;
            tracing::info!(key, session_id = %sid, "Intent execution context closed (task terminal, design 27 §2.1 T2-a)");
        }
        Ok(())
    }

    /// Create a new session, spawning it as an independent tokio task.
    ///
    /// Returns the session ID on success.
    pub async fn create_session(&mut self) -> Result<String> {
        let session_id = Uuid::new_v4().to_string();
        self.create_session_with_id(session_id).await
    }

    /// Create a new session with a specific ID.
    ///
    /// Useful for testing or when the session ID needs to be deterministic.
    pub async fn create_session_with_id(&mut self, session_id: String) -> Result<String> {
        self.create_session_with_id_and_conversation(session_id, None, None)
            .await
    }

    /// Create a new session with a specific ID and optional conversation session.
    ///
    /// When `conversation` is provided, the session is initialized with JSONL
    /// persistence enabled. This is used for the initial session on cold start
    /// when a previous conversation is resumed.
    ///
    /// `committed_lines` must be the same `Arc<AtomicUsize>` that was passed to
    /// the `ConversationSession`'s writer thread. It is shared between the
    /// session's AgentCore and the background writer so that
    /// `notify_new_data_available` and HTTP poll handlers always read the
    /// correct per-session line count.
    pub async fn create_session_with_id_and_conversation(
        &mut self,
        session_id: String,
        conversation: Option<ConversationSession>,
        committed_lines: Option<Arc<std::sync::atomic::AtomicUsize>>,
    ) -> Result<String> {
        // Read the persisted workspace_id and model/provider before the conversation
        // is moved into SessionState, so we can restore them.
        let persisted_workspace_id = conversation
            .as_ref()
            .and_then(|c| c.workspace_id())
            .map(|w| w.to_string());

        let (inbound_tx, inbound_rx) = mpsc::channel(self.config.inbound_channel_capacity);

        // Snapshot the title (and other resume-only fields) before `conversation`
        // is moved into `build_initial_session_state`. We need it after the
        // move to seed `latest_session`.
        let resumed_title = conversation.as_ref().and_then(|c| c.title());

        // ADR-047: wrap conversation in Arc so it can be shared between
        // SessionState (owned by SessionTask/AgentLoop) and SessionHandle
        // (owned by SessionManager). This enables config mutations to bypass
        // the serial inference queue.
        let conversation_arc = conversation.map(Arc::new);

        let session_state = self.build_initial_session_state(conversation_arc.clone());

        // Shared channel for bypass-injecting debug handles into AgentCore
        // while the agent loop is running (its message channel is blocked).
        let pending_debug_handles: Arc<tokio::sync::Mutex<Option<DebugHandles>>> =
            Arc::new(tokio::sync::Mutex::new(None));

        // If debug mode is active, create a per-session DebugController and
        // register it in self.debug_controllers so the debug service can
        // read this session's state via getState. The global runtime_debug_handles
        // carries a shared controller — we must NOT reuse it because each session
        // needs its own independent iteration/phase.
        // The notify handles (rewind/resume) also come from the per-session
        // controller so the debug server's notify_one() calls align with SessionTask.
        let per_session_debug = if let Some(ref handles) = self.runtime_debug_handles {
            let ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
            let (per_rewind, per_resume, per_control) = {
                let guard = ctrl.lock().await;
                (
                    guard.rewind_notify_handle(),
                    guard.resume_notify_handle(),
                    guard.control_notify_handle(),
                )
            };
            self.debug_controllers
                .write()
                .await
                .insert(session_id.clone(), ctrl.clone());
            // ADR-048: also register the per-session event sender in the
            // shared map so `RuntimeDebugService` (built by
            // `enable_debug_mode`) can find it. The template sender from
            // `runtime_debug_handles` is per-session-tagged, so we share
            // the channel handle but bind it to the new session_id.
            let per_session_sender = handles.debug_event_tx.for_session(session_id.clone());
            self.debug_event_senders
                .write()
                .await
                .insert(session_id.clone(), per_session_sender.clone());
            Some(DebugHandles {
                debug_ctrl: ctrl,
                debug_event_tx: handles.debug_event_tx.for_session(session_id.clone()),
                rewind_notify: per_rewind,
                resume_notify: per_resume,
                control_notify: per_control,
            })
        } else {
            None
        };

        // Create per-session workspace Arcs — the single source of truth.
        // SessionCore and SessionHandle share these Arcs, so SessionManager
        // can read/write workspace state synchronously without channel delay.
        let initial_workspace = persisted_workspace_id
            .clone()
            .unwrap_or_else(|| self.default_workspace_id.clone());
        let workspace_id: Arc<std::sync::RwLock<String>> =
            Arc::new(std::sync::RwLock::new(initial_workspace.clone()));
        self.pending_workspaces.remove(&session_id);
        // Store per-session committed_lines for HTTP handler access.
        if let Some(ref cl) = committed_lines {
            self.session_committed_lines
                .insert(session_id.clone(), cl.clone());
        }
        let initial_work_dir = if let Some(ref resolver) = self.resolver {
            let guard = resolver.read().unwrap();
            if initial_workspace == "__agent_home__" {
                guard.agent_home().to_string()
            } else {
                guard
                    .find_by_id(&initial_workspace)
                    .map(|d| d.path.clone())
                    .unwrap_or_else(|| guard.agent_home().to_string())
            }
        } else {
            self.core.config.work_dir.clone()
        };
        let current_work_dir: Arc<std::sync::RwLock<Option<String>>> =
            Arc::new(std::sync::RwLock::new(Some(initial_work_dir)));

        // For sessions without a persistent conversation, create a dummy
        // committed_lines counter.  This session won't produce JSONL writes
        // (no writer thread), so the counter stays at 0 — which is accurate.
        let session_committed_lines =
            committed_lines.unwrap_or_else(|| Arc::new(std::sync::atomic::AtomicUsize::new(0)));

        // Extract the snapshot Arc before session_state is moved into SessionTask.
        // The snapshot is already populated with persistent data by
        // build_initial_session_state; we just need to set session_id.
        //
        // ADR-039: workspace_id is no longer mirrored into the runtime
        // snapshot — it lives in `data/meta/{session_id}.json` and is
        // broadcast through the `session_meta` MQTT channel.
        let snapshot_arc = session_state.snapshot.clone();
        {
            if let Ok(mut snap) = snapshot_arc.write() {
                snap.session_id = session_id.clone();
            }
        }

        let (mut task, agent_inbound_tx) = SessionTask::new(
            self.core.clone(),
            session_state,
            inbound_rx,
            self.config.system_prompt.clone(),
            self.config.chunk_tx.clone(),
            session_id.clone(),
            self.config.identity_context.clone(),
            self.config.protocol_type.clone(),
            self.mcp_tools.clone(),
            self.dynamic_builtin_tools.clone(),
            per_session_debug,
            pending_debug_handles.clone(),
            self.runtime_overrides.clone(),
            current_work_dir.clone(),
            session_committed_lines,
            self.streaming_lines.clone(),
            self.intent_map.clone(),
        );

        // ADR-014: Create watch channel for session status.
        // Keep `status_tx` so we can clone it into the panic-handler closure
        // below (the panic handler must be able to push `SessionStatus::Errored`
        // after `task` itself has been dropped).
        let (status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        task.set_status_tx(status_tx.clone());

        // Register per-session urgent_stop Notify so fire_urgent_stop()
        // only wakes this session's tokio::select! branches.
        if let Some(notify) = task.urgent_stop_notify() {
            self.urgent_stops.insert(session_id.clone(), notify);
        }

        // ADR-044 §4.5: Register the per-session Arc slot (not a `CancelHandle`
        // clone). Phase 3 external callers (MQTT dispatcher, debug server)
        // look up the slot via [`Self::cancel_handle`] and read the
        // *current* generation through `Arc::lock()` on every dispatch —
        // never a stale clone. Inserted unconditionally — the slot is
        // always allocated in `SessionCore::new`, so no `Option` unwrap is
        // needed here.
        self.cancel_handles
            .insert(session_id.clone(), task.cancel_handle_arc());

        // ADR-083: register the compaction-cancel slot so the MQTT
        // `compress_type = 3` dispatcher can fire it while the session task
        // is blocked inside `compact_history_if_needed().await`.
        self.compaction_cancel_handles
            .insert(session_id.clone(), task.compaction_cancel_handle_arc());

        // Spawn the session task with panic isolation.
        // catch_unwind ensures that if SessionTask::run() panics, we log the
        // panic with the session_id before the task terminates. Without this,
        // tokio::spawn silently swallows the panic and the only symptom is a
        // "Session channel closed" warning with no root cause.
        let sid = session_id.clone();
        let panic_status_tx = status_tx.clone();
        let panic_chunk_tx = self.config.chunk_tx.clone();
        let join_handle = tokio::spawn(async move {
            let result = std::panic::AssertUnwindSafe(task.run())
                .catch_unwind()
                .await;
            if let Err(panic_err) = result {
                handle_session_task_panic(
                    &sid,
                    panic_err,
                    &panic_status_tx,
                    panic_chunk_tx.as_ref(),
                );
            }
        });

        // ADR-047: Register the session's ConversationSession Arc in the
        // shared config map so SessionConfigService can apply config
        // changes without going through the serial inference queue.
        // Must happen before `conversation_arc` is moved into SessionHandle.
        if let Some(conv) = &conversation_arc {
            self.session_configs
                .write()
                .unwrap()
                .insert(session_id.clone(), conv.clone());
        }

        let handle = SessionHandle {
            session_id: session_id.clone(),
            inbound_tx,
            agent_inbound_tx,
            join_handle,
            status_rx,
            last_active_at: std::sync::Mutex::new(std::time::Instant::now()),
            pending_debug_handles: pending_debug_handles.clone(),
            snapshot: snapshot_arc.clone(),
            workspace_id,
            current_work_dir,
            conversation: conversation_arc,
        };

        self.sessions.insert(session_id.clone(), handle);
        tracing::info!(session_id = %session_id, "SessionManager: created new session");

        // Register the session's snapshot Arc in the shared map so the
        // Runtime HTTP server can serve GET /sessions/{sid}/state.
        if let Some(ref snapshots) = self.config.session_snapshots {
            snapshots
                .write()
                .unwrap()
                .insert(session_id.clone(), snapshot_arc);
        }

        // Every newly created or resumed session becomes the "latest" by
        // definition. For resumed sessions we propagate the persisted
        // title from `ConversationSession` so `/sessions/latest` returns
        // the title the user previously set, instead of `null`. For
        // brand-new sessions the title is `None` until the first
        // user-driven rename.
        //
        // This single source of truth replaces the previous design where
        // `session_init.rs` would seed the latest-session entry first and
        // then this function would clobber it with `None`.
        self.set_latest_session(session_id.clone(), resumed_title);

        // Initialize per-session workspace.
        // For resumed sessions, restore the persisted workspace_id from JSONL metadata.
        // New sessions default to last_active workspace (or agent home fallback).
        // Note: the workspace mapping was already pre-registered above for
        // initial_work_dir resolution. This call persists workspace_id to JSONL
        // and sends SetWorkDir (redundant with direct init, but harmless).
        self.set_session_workspace(&session_id, &initial_workspace);

        // Apply workspace context + prompt file from the resolver.
        //
        // This is the single source of truth for per-session workspace state
        // injection. `set_resolver()` is a hard precondition for session
        // creation (see its doc); `update_session_workspace_context` will
        // panic via `.expect()` if it was not called (programming error).
        // By injecting at creation time, every session path — initial
        // session, "New Chat", lazy resume — bootstraps identically with no
        // caller-side follow-up required.
        self.update_session_workspace_context(&session_id);

        // Provider list / capabilities / max-output limits are now read
        // on demand from the shared `AgentCore.global_provider_list`
        // populated at AgentHello and updated via ProviderListUpdate. No
        // per-session replay is required — sessions query AgentCore directly.

        Ok(session_id)
    }

    /// Create a new session from frontend request, handling everything in one call.
    ///
    /// Generates a session ID, creates the JSONL file with initial metadata,
    /// writes the index entry, spawns the session task, and enables
    /// notifications — all in a single atomic operation.
    ///
    /// Returns the new session ID on success.
    /// ADR-076 §决策 4: `user_id` is the creating account, or `None`
    /// under `AUTH_MODE=local` (no account system). It is recorded once
    /// on the session meta and never reassigned.
    pub async fn create_frontend_session(
        &mut self,
        workspace_id: Option<&str>,
        model: Option<&str>,
        provider: Option<&str>,
        user_id: Option<&str>,
    ) -> Result<String> {
        self.create_frontend_session_inner(workspace_id, model, provider, user_id, None)
            .await
    }

    /// Design 27 §6.1: like `create_frontend_session`, but the session is
    /// owned by the **agent account** (`last_user_profile` — the account
    /// bound to this Runtime via `UserProfileUpdate`), not the requesting
    /// human. An intent execution session is agent-private working state:
    /// the human surface is the Chat session, and the Session-history list
    /// must not fill up with background responder sessions. Falls back to
    /// the caller-provided owner when no profile is cached (and to
    /// ownerless/local mode when neither exists).
    pub async fn create_agent_owned_session(
        &mut self,
        requester_user_id: Option<&str>,
        seed: Option<&super::intent_routing::SessionSeed>,
    ) -> Result<String> {
        self.create_agent_owned_session_with_id(requester_user_id, None, seed)
            .await
    }

    /// `create_agent_owned_session` with a pre-allocated session id — used
    /// by intent rotation replay (§2.1 crash recovery: the id was planned
    /// and persisted before the session existed).
    pub async fn create_agent_owned_session_with_id(
        &mut self,
        requester_user_id: Option<&str>,
        preset_session_id: Option<&str>,
        seed: Option<&super::intent_routing::SessionSeed>,
    ) -> Result<String> {
        let agent_user = self
            .last_user_profile
            .as_ref()
            .map(|p| p.user_id.clone())
            .filter(|id| !id.is_empty());
        self.create_frontend_session_inner(
            None,
            seed.and_then(|s| s.model.as_deref()),
            seed.and_then(|s| s.provider.as_deref()),
            agent_user.as_deref().or(requester_user_id),
            preset_session_id,
        )
        .await
    }

    async fn create_frontend_session_inner(
        &mut self,
        workspace_id: Option<&str>,
        model: Option<&str>,
        provider: Option<&str>,
        user_id: Option<&str>,
        preset_session_id: Option<&str>,
    ) -> Result<String> {
        // A pre-assigned id (§2.1 rotation replay) reopens the planned id
        // instead of generating a new one. `ConversationSession::new`
        // truncates an existing file — safe here because a planned id can
        // only exist as a not-yet-used successor (rekey clears the plan
        // before any turn ever ran on it).
        let session_id = preset_session_id
            .map(|s| s.to_string())
            .unwrap_or_else(crate::conversation::generate_session_id);
        let committed_lines = Self::new_committed_lines();

        // Create the JSONL file and index entry
        // ADR-024: read agent_config.json for max_sessions override;
        // fall back to RuntimeConfig default if absent.
        let max_sessions = crate::agent_config::load_agent_config(std::path::Path::new(
            &self.core.config.work_dir,
        ))
        .unwrap_or_default()
        .unwrap_or_default()
        .max_sessions
        .unwrap_or(self.core.config.max_sessions);

        let (conv, config_rx, state_rx) = crate::conversation::ConversationSession::new(
            std::path::Path::new(&self.core.config.work_dir),
            &session_id,
            crate::conversation::SessionConfig {
                agent_id: self.core.config.agent_id.clone(),
                workspace_id: workspace_id.map(|s| s.to_string()),
                model: model.map(|s| s.to_string()),
                provider: provider.map(|s| s.to_string()),
            },
            max_sessions,
            committed_lines.clone(),
        )?;

        // ADR-076 §决策 4: stamp the owner before the session task starts,
        // so the very first meta write already carries it and a concurrent
        // list can never observe an ownerless session it just created.
        conv.set_user_id(user_id.map(|u| u.to_string()));

        // ADR-076 §决策 4: an owned session starts **private**.
        //
        // `visibility: None` reads as *public* (`is_readable_by`), and it
        // used to also be what an identified creator got by omission — so
        // in a multi-user deployment every account could read every new
        // session, including the ones with the system agent. `None` should
        // only mean what it has to: sessions that predate accounts, which
        // have no owner to restrict them to (the flag is ignored for those
        // anyway). An owned session's creator always knows who it is, so
        // it gets the conservative value explicitly at birth.
        //
        // The per-session switch is still the way out, and because the
        // value is on disk rather than implied, that switch now shows the
        // truth instead of claiming "public" for a session nobody can read.
        if user_id.is_some() {
            conv.set_visibility(Some(crate::conversation::SessionVisibility::Private));
        }

        // ADR-043: Spawn config + state change relays.
        if let Some(chunk_tx) = self.config.chunk_tx.clone() {
            crate::startup::subsystems::spawn_config_change_relay(
                config_rx,
                chunk_tx.clone(),
                conv.clone(),
                session_id.clone(),
                // Wrap self.core in a fresh SharedAgentCore slot for the
                // relay. self.core is already fully constructed (this path
                // runs inside SessionManager which owns the Arc), so the
                // slot is immediately populated.
                std::sync::Arc::new(std::sync::RwLock::new(Some(self.core.clone()))),
            );
            crate::startup::subsystems::spawn_state_change_relay(
                state_rx,
                chunk_tx,
                conv.clone(),
                session_id.clone(),
            );
        }

        // Spawn the session task
        self.create_session_with_id_and_conversation(
            session_id.clone(),
            Some(conv),
            Some(committed_lines),
        )
        .await?;

        // ADR-035 Phase 3: EnableNotify removed — push drives all streaming,
        // no front/back suppression mechanism remains.

        Ok(session_id)
    }

    /// Build a fully-initialized SessionState for a new or resumed session.
    /// All per-session fields are set synchronously before this returns.
    /// Caller must hold an Arc<AgentCore> with global_provider_list populated.
    fn build_initial_session_state(
        &self,
        conversation: Option<Arc<ConversationSession>>,
    ) -> SessionState {
        let mut initial_model = conversation.as_ref().and_then(|c| c.model());
        let mut initial_provider = conversation.as_ref().and_then(|c| c.provider());
        let mut initial_account = conversation.as_ref().and_then(|c| c.account_id());

        // Fall back to Runtime-internal default when the session has no
        // explicit model/provider (new agent, first session ever created).
        // current_model_and_provider() atomically returns the (model, provider)
        // pair from the most recently active session, or the first entry from
        // global_provider_list if no session has ever been activated.
        if initial_model.is_none() || initial_provider.is_none() {
            let (fallback_model, fallback_provider) = self.current_model_and_provider();

            // Persist the fallback to JSONL so the session carries
            // consistent (model, provider) metadata into the next
            // `OpenSession` activation — the proto `SessionOpened` event
            // reports these fields directly to the frontend.
            if let (Some(conv), Some(model)) = (&conversation, &fallback_model)
                && conv.model().is_none()
            {
                conv.update_model_provider(model, fallback_provider.as_deref());
            }
            initial_model = fallback_model;
            initial_provider = fallback_provider;
        }

        // Same fallback chain for the multi-account pick: when the new
        // session has no explicit account_id, inherit the most recently
        // active session's pick so the model-menu highlight + right-panel
        // alias row reflect the operator's last choice immediately, rather
        // than staying blank until the user re-picks. No fallback is
        // forced for never-picked cases (current_account_id returns None
        // then) — single-account providers and operators who never picked
        // an account continue to start with no pick, matching pre-change
        // behaviour.
        if initial_account.is_none()
            && let Some(fallback_account) = self.current_account_id()
        {
            // Persist the inherited value to JSONL when a conversation
            // exists (production `create_frontend_session` path), so
            // the next resume keeps it. The cold-start
            // `create_session()` path has no conversation to persist
            // to — that's fine, the in-memory SessionState +
            // SessionRuntimeSnapshot carry the value (the snapshot is
            // the runtime mirror the frontend reads), and the session
            // will get a conv attached on the first user message
            // anyway.
            if let Some(conv) = &conversation
                && conv.account_id().is_none()
            {
                conv.update_account_id(&fallback_account);
            }
            initial_account = Some(fallback_account);
        }

        // Resume path: rebuild HistoryManager from the JSONL log so the LLM
        // sees the prior conversation on the first new turn after cold-start.
        // This is gated by ACOWORK_DISABLE_SESSION_RESUME=1 for ops debugging.
        let restored = if std::env::var("ACOWORK_DISABLE_SESSION_RESUME")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
        {
            tracing::warn!("ACOWORK_DISABLE_SESSION_RESUME set; skipping JSONL history restore");
            None
        } else {
            conversation.as_ref().and_then(|conv| {
                let path = conv.session_path();
                // ADR-024: read compaction offset from per-session meta file.
                let compaction_abs = path.parent().and_then(|conversations_dir| {
                    crate::conversation::read_session_meta(conversations_dir, conv.session_id())
                        .ok()
                        .and_then(|m| m.last_compaction_offset)
                });
                match crate::agent::session::restorer::restore_history_from_jsonl(
                    path,
                    compaction_abs,
                ) {
                    Ok(outcome) if !outcome.messages.is_empty() => {
                        tracing::info!(
                            session_id = %conv.session_id(),
                            replayed = outcome.replayed_entry_count,
                            skipped = outcome.skipped_entry_count,
                            had_compaction = outcome.had_compaction,
                            messages = outcome.messages.len(),
                            "Session resume: restored history from JSONL"
                        );
                        Some(outcome)
                    }
                    Ok(_) => {
                        // New session or empty file — nothing to restore.
                        None
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %conv.session_id(),
                            error = %e,
                            "Session resume: failed to restore history; starting empty"
                        );
                        None
                    }
                }
            })
        };

        let mut session_state = SessionState::new(
            self.config.history_max_tokens,
            self.config.per_session_budget.clone(),
            conversation,
        );

        // ADR-012: Set per-session model/provider on SessionState (only if we have one).
        if let Some(m) = initial_model.as_ref() {
            session_state.set_model(m.clone());
            // Update HistoryManager::max_tokens to the model's actual effective
            // input budget rather than the static config.history_max_tokens (128K).
            // Without this, trim_fifo would clamp history at 128K which may be
            // far below the model's actual context window, making auto compaction
            // at 80% threshold unreachable.
            //
            // ADR-074: resolve the session-effective cap (Layer 0 override
            // from ConversationSession over the per-agent chain) BEFORE
            // set_max_tokens, or the first frame trims against the agent
            // window instead of the session override.
            let session_override = session_state
                .conversation()
                .and_then(|c| c.context_window());
            let caps = self.core.get_model_capabilities(m);
            let resolved = crate::agent::session_config::resolve_effective_context_window(
                session_override,
                self.core.context_window_override,
                self.core.manifest_context_window,
                caps.as_ref(),
            );
            let budget = self.core.context_trim_budget_with(Some(resolved), m);
            session_state.history_mut().set_max_tokens(budget);

            // Three-level priority chain for reasoning_effort:
            // 1. Persisted session value (from JSONL metadata, via ConversationSession)
            // 2. Provider capabilities default_reasoning_effort
            // 3. None (provider does not support thinking control)
            let persisted_effort = session_state
                .conversation()
                .and_then(|c| c.reasoning_effort());

            if let Some(ref effort_str) = persisted_effort {
                // Session already has a persisted value; restore it.
                let effort =
                    acowork_core::providers::traits::ReasoningEffort::from_str_loose(effort_str);
                session_state.set_reasoning_effort(effort);
            } else {
                // No persisted value: initialize from provider capabilities default.
                // If default_reasoning_effort is None but supports_reasoning is true,
                // fall back to Auto so the user sees the reasoning effort control.
                let caps = self.core.get_model_capabilities(m);
                let provider_default = caps
                    .as_ref()
                    .and_then(|c| c.default_reasoning_effort.clone());
                let effort = provider_default
                    .as_deref()
                    .and_then(acowork_core::providers::traits::ReasoningEffort::from_str_loose)
                    .or_else(|| {
                        // Model supports reasoning but has no explicit default → Auto
                        if caps
                            .as_ref()
                            .and_then(|c| c.supports_reasoning)
                            .unwrap_or(false)
                        {
                            Some(acowork_core::providers::traits::ReasoningEffort::Auto)
                        } else {
                            None
                        }
                    });
                session_state.set_reasoning_effort(effort.clone());
                // Write back to ConversationSession so future resumes have a value.
                if let Some(conv) = session_state.conversation() {
                    let effort_str = effort.as_ref().map(|e| e.to_string());
                    conv.update_reasoning_effort(effort_str);
                }
            }
        }
        if let Some(p) = initial_provider.as_ref() {
            session_state.set_provider(p.clone());
        }
        if let Some(a) = initial_account.as_ref() {
            session_state.set_account_id(a.clone());
        }

        // ADR-060 §6.1: restore the persisted todo snapshot from meta so a
        // session restart keeps the Block C task list (previously lost on
        // restart). Replace semantics; the mirror back into
        // `ConversationSession::set_todos` is a no-op when content is equal.
        if let Some(todos) = session_state.conversation().and_then(|c| c.todos()) {
            session_state.update_todos(todos, false);
            tracing::debug!(
                session_id = %session_state.conversation().map(|c| c.session_id().to_string()).unwrap_or_default(),
                todo_count = session_state.todos.len(),
                "Session resume: restored todo list from meta"
            );
        }

        // Propagate temperature override to the session via the per-agent chain:
        //   runtime_overrides → agent_config.json (Layer 1) → manifest (Layer 2) → DEFAULT_TEMPERATURE (Layer 3).
        // Always set a concrete value so the model actually receives the configured
        // temperature and the status panel can display it accurately.
        let temperature = self
            .runtime_overrides
            .temperature
            .or(self.core.temperature_override)
            .or(self.core.manifest_temperature)
            .or(Some(DEFAULT_TEMPERATURE));
        session_state.set_temperature(temperature);

        // Install the restored history *after* set_max_tokens has been applied
        // so any downstream budget comparison uses the model-correct budget.
        // No trim happens here — see the restore note below.
        if let Some(outcome) = restored {
            session_state
                .history_mut()
                .load_restored(outcome.messages, outcome.last_injected_todo_call_id);

            // Restore the token-counting scene from meta: the last
            // API-counted input (authoritative anchor for the resumed
            // history) and the last calibrated chars/token ratio. Without
            // this the JSONL replay re-estimates with a messages-only count
            // that is strictly smaller than the real prompt, so the first
            // post-resume request would slip past the compaction threshold.
            if let Some(conv) = session_state.conversation() {
                let last_input = conv.tokens().map(|t| t.last_input);
                let model_ratio = conv.model_ratio();
                session_state
                    .history_mut()
                    .restore_anchor(last_input, model_ratio);
            }

            // NOTE: restore does not perform placeholder compression OR
            // lossless trim. History is loaded from JSONL as-is so the
            // post-resume state matches the user's last-active view — the
            // 90-message lossless trim we used to run here silently
            // discarded entire rounds and surfaced as `~109K` usage in
            // the status panel even when the persisted `last_input`
            // reported `167K`. Active-session protection is enough:
            //   - `execute_single_iteration` calls `trim_history_to_budget`
            //     (absolute budget floor → `compact_history_if_needed(force=true)`,
            //     LLM-summary compaction) before every `build_chat_request`,
            //     including the very first call after cold start.
            //   - `check_context_overflow_and_trim` covers the 90% threshold.
            // Both routes preserve semantics via LLM summary, so the
            // "resumed under a smaller model" edge case is handled by the
            // same fail-closed path (compaction failure → session Idle)
            // instead of a silent lossless drop.
            //
            // If a compaction summary was restored, the session is logically
            // already in a "post-compaction" state — mark it so session-close
            // tail distillation respects the boundary.
            if outcome.had_compaction {
                session_state.is_compacted = true;
            }
        }

        // Populate the shared runtime snapshot with the initial context_usage
        // (computed from persisted tokens) so the frontend can read it via
        // fetchSessionState() immediately, without waiting for
        // emit_session_state() to run.
        //
        // ADR-039: model + provider are mirrored here as runtime-cached
        // values (not authoritative — see ADR-039 (revised)). reasoning_effort
        // and temperature are NOT mirrored — they live in
        // `data/meta/{session_id}.json` and are broadcast through the
        // `session_meta` MQTT channel.
        {
            let model_name = session_state.model().map(|s| s.to_string());
            let provider_name = session_state.provider().map(|s| s.to_string());

            // Build context_usage from persisted session tokens (if available).
            // See `context::build_persisted_ctx_usage` for the
            // caps-resolution + cumulative-patch sequence this used to
            // spell out by hand (regression t-83afab47 refactor).
            let context_usage = session_state
                .conversation()
                .and_then(|conv| crate::agent::context::build_persisted_ctx_usage(&self.core, conv))
                .and_then(|ctx| serde_json::to_string(&ctx).ok());

            if let Ok(mut snap) = session_state.snapshot.write() {
                snap.model = model_name;
                snap.provider = provider_name;
                snap.account_id = session_state.account_id().map(|s| s.to_string());
                snap.context_usage = context_usage;
                // session_id is set by the caller after
                // build_initial_session_state returns.
            }
        }

        session_state
    }

    /// Close a session by ID, disabling notifications then sending Close.
    ///
    /// Triggers distillation but preserves the JSONL history file.
    /// Returns an error if the session does not exist.
    pub async fn close_session(&mut self, session_id: &str) -> Result<()> {
        let handle = self
            .sessions
            .remove(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        // ADR-035 Phase 3: DisableNotify removed — push drives all streaming.

        // Send Close signal; ignore errors (session may have already stopped)
        let _ = handle.inbound_tx.send(SessionMessage::Close).await;

        // Clean up per-session mappings
        self.pending_workspaces.remove(session_id);
        self.urgent_stops.remove(session_id);
        self.cancel_handles.remove(session_id);
        self.compaction_cancel_handles.remove(session_id);
        self.session_committed_lines.remove(session_id);
        self.session_delivery_cursors
            .write()
            .unwrap()
            .remove(session_id);

        tracing::info!(session_id = %session_id, "SessionManager: closed session");
        Ok(())
    }

    /// Look up a session's **current request's** [`CancelHandle`] by `session_id`.
    ///
    /// ADR-044 §4.5: external signal sources (MQTT `ControlAction::StopGeneration`
    /// dispatcher in `startup/gateway_loop.rs`, debug server `Stop` handler,
    /// CLI cancel command, test harness) use this to obtain a handle and call
    /// `cancel(CancellationReason::UserStop { ... })` on it. Returns `None`
    /// when the session is not registered (evicted, never created, or
    /// already closed) — callers should treat that as "no session to stop"
    /// and log a warning rather than crash.
    ///
    /// **Always reads the *current* generation**: the stored value is an
    /// `Arc<parking_lot::Mutex<CancelHandle>>` slot, not a handle clone.
    /// We take a brief `parking_lot::Mutex` lock to clone the handle the
    /// slot currently holds, so callers always see whatever
    /// `run_inner::begin_new_request` last wrote — never a stale clone
    /// from session creation time.
    ///
    /// **Symmetry with the legacy `urgent_stops` map**: this is the canonical
    /// successor of the `fire_urgent_stop()`-style helper. The MQTT
    /// dispatcher calls `cancel(UserStop { source: ChatPanel { agent_id, session_id }, reason })`
    /// here at the start of its stop-message handler; the legacy `urgent_stops`
    /// map remains for incremental rollback and is removed in Phase 4.
    pub fn cancel_handle(&self, session_id: &str) -> Option<CancelHandle> {
        let slot = self.cancel_handles.get(session_id)?;
        Some(slot.lock().clone())
    }

    /// ADR-083: read the **current** compaction-cancel handle for a session.
    ///
    /// Used by the `compress_type = 3` dispatcher in
    /// `startup/gateway_loop.rs`. Same slot semantics as
    /// [`Self::cancel_handle`] — always the latest generation — and `None`
    /// when the session is unknown (evicted / never created), in which case
    /// the caller treats it as a no-op.
    pub fn compaction_cancel_handle(&self, session_id: &str) -> Option<CancelHandle> {
        let slot = self.compaction_cancel_handles.get(session_id)?;
        Some(slot.lock().clone())
    }

    /// Return the `agent_id` of the owning runtime, for use as the
    /// `StopSource::ChatPanel { agent_id, ... }` payload.
    ///
    /// ADR-044 Phase 3: external callers (MQTT dispatcher, debug server,
    /// CLI) need a stable handle to identify which agent initiated the
    /// cancel when constructing [`CancellationReason::UserStop`]. Returns
    /// a borrowed `&str` from the [`AgentCore`](crate::agent::AgentCore)
    /// template so no allocation is needed at the hot path.
    pub fn agent_id(&self) -> &str {
        &self.core.config.agent_id
    }

    /// Delete a session: close the task, drop the meta row, and delete the JSONL file.
    ///
    /// This is an atomic operation from the caller's perspective — after this
    /// returns, the session no longer exists in memory or on disk (unless the
    /// join times out, in which case the task continues but its meta row and
    /// JSONL file are still cleaned up).
    ///
    /// Works for sessions both in memory and already evicted from memory
    /// (e.g., idle eviction, reaped handles, or previous Runtime restarts).
    /// When the session is not in `self.sessions`, the Close/join steps are
    /// skipped and only the persisted resources are cleaned up.
    ///
    /// A 30-second timeout is applied to the session task join.  If the task
    /// does not finish within this window (e.g. distillation hangs), resources
    /// are still cleaned up and the method returns successfully — the
    /// background task's eventual `Drop` cannot re-create the row because
    /// `ConversationSession::write_meta` returns early once the JSONL is gone.
    pub async fn delete_session(&mut self, session_id: &str) {
        // 1. If in memory, close the task cleanly (with timeout)
        if let Some(handle) = self.sessions.remove(session_id) {
            // Remove session snapshot from shared map (if registered)
            if let Some(ref snapshots) = self.config.session_snapshots {
                snapshots.write().unwrap().remove(session_id);
            }
            // ADR-047: Remove from shared config map.
            self.session_configs.write().unwrap().remove(session_id);
            // ADR-035 Phase 3: DisableNotify removed.
            let _ = handle.inbound_tx.send(SessionMessage::Close).await;

            // Clean up in-memory mappings
            self.pending_workspaces.remove(session_id);
            self.urgent_stops.remove(session_id);
            self.cancel_handles.remove(session_id);
            self.compaction_cancel_handles.remove(session_id);
            self.session_committed_lines.remove(session_id);
            self.session_delivery_cursors
                .write()
                .unwrap()
                .remove(session_id);

            // Wait for the task to finish so that Drop runs before we
            // finalize on-disk state.
            const TIMEOUT: Duration = Duration::from_secs(30);
            match tokio::time::timeout(TIMEOUT, handle.join_handle).await {
                Ok(Ok(())) => {
                    tracing::info!(session_id = %session_id, "Session task shut down cleanly");
                }
                Ok(Err(join_err)) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %join_err,
                        "Session task panicked during close"
                    );
                }
                Err(_elapsed) => {
                    tracing::warn!(
                        session_id = %session_id,
                        timeout = ?TIMEOUT,
                        "Session task did not finish within timeout, proceeding with resource cleanup"
                    );
                }
            }
        } else {
            // Session already evicted — just clean up remaining mappings
            self.pending_workspaces.remove(session_id);
            self.urgent_stops.remove(session_id);
            self.session_committed_lines.remove(session_id);
            self.session_delivery_cursors
                .write()
                .unwrap()
                .remove(session_id);
            tracing::info!(session_id = %session_id, "Session already evicted, skipping task close");
        }

        // 2. ADR-082 §4 step 3: drop the session-meta row from the store
        // (SQLite). The legacy JSON sidecar no longer exists — leaving the
        // row behind is what would keep a deleted session in `/sessions`
        // and in `find_latest_session`.
        let conversations_dir =
            std::path::Path::new(&self.core.config.work_dir).join("conversations");
        if let Err(e) = crate::conversation::delete_session_meta(&conversations_dir, session_id) {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to delete session meta row"
            );
        }

        // 3. Delete the JSONL file.
        let file_path = conversations_dir.join(format!("{}.jsonl", session_id));
        match std::fs::remove_file(&file_path) {
            Ok(()) => {
                tracing::info!(session_id = %session_id, "Deleted session JSONL file");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::warn!(session_id = %session_id, "Session JSONL file already deleted");
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    path = %file_path.display(),
                    error = %e,
                    "Failed to delete session JSONL file"
                );
            }
        }

        // 4. If the deleted session was the "latest", recompute the latest
        //    from the remaining sessions (or clear it if none remain).
        //    Without this, `/sessions/latest` keeps returning the deleted
        //    session_id and the frontend tries to resume a session whose
        //    JSONL is gone — the exact "deleted session reloaded as latest"
        //    failure. `find_latest_session` reads the session-meta store
        //    (SQLite), which is already consistent here because step 2 above
        //    dropped this session's row, and `write_meta` refuses to re-create
        //    a row once the JSONL is gone (see `ConversationSession::write_meta`).
        if let Some((latest_id, _)) = self.latest_session()
            && latest_id == session_id
        {
            match crate::conversation::find_latest_session(&conversations_dir) {
                Some(new_id) => {
                    let title = read_session_meta(&conversations_dir, &new_id)
                        .ok()
                        .and_then(|m| m.title);
                    self.set_latest_session(new_id.clone(), title);
                    tracing::info!(
                        session_id = %session_id,
                        new_latest = %new_id,
                        "Deleted session was latest; recomputed latest from disk"
                    );
                }
                None => {
                    *self.latest_session.write().unwrap() = None;
                    tracing::info!(
                        session_id = %session_id,
                        "Deleted session was latest; no sessions remain, cleared latest"
                    );
                }
            }
        }

        tracing::info!(session_id = %session_id, "SessionManager: deleted session");
    }

    /// Send a message to a specific session.
    ///
    /// Returns an error if the session does not exist, the channel is full
    /// (transient backpressure — caller should retry or drop the message),
    /// or the channel is closed (SessionTask has died).
    ///
    /// **Full vs Closed distinction**: when the channel is merely full,
    /// Per-turn skill command injection: resolve a chat `command` (skill
    /// name) into the skill's instruction block via the agent's
    /// `SkillRegistry` (loaded Phase A, injected Phase B). The registry is
    /// the authoritative source — the frontend only ever sends names.
    /// Returns `None` for unknown/empty commands so the caller can fall
    /// back to no skill injection.
    pub(crate) fn resolve_skill_instructions(&self, command: &str) -> Option<String> {
        self.core.skill_registry.instructions_for(command)
    }

    /// ADR-034 Phase 7: Send a message to a session by ID.
    ///
    /// NOTE: when the session's channel is full (backpressure), the
    /// session handle is NOT removed — the session is healthy but
    /// experiencing backpressure. When the channel is closed (e.g. the
    /// SessionTask panicked), the stale handle IS auto-removed so
    /// subsequent calls get a clean "Session not found" instead of
    /// "channel closed".
    pub fn send_to_session(&mut self, session_id: &str, msg: SessionMessage) -> Result<()> {
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;

        match handle.send(msg) {
            Ok(()) => Ok(()),
            Err(send_err) => match send_err.as_ref() {
                tokio::sync::mpsc::error::TrySendError::Full(_) => {
                    // Transient backpressure — session is healthy, do NOT evict.
                    // The caller may retry or drop the message depending on context.
                    tracing::warn!(
                        session_id = %session_id,
                        "Session channel full (backpressure) — message dropped, session NOT evicted"
                    );
                    Err(RuntimeError::Config(format!(
                        "Session channel full: {}",
                        session_id
                    )))
                }
                tokio::sync::mpsc::error::TrySendError::Closed(_) => {
                    // Channel closed — the SessionTask has died (panic / eviction race).
                    // Auto-remove the stale handle so the next attempt gets a clean
                    // "Session not found" error instead of "channel closed".
                    let was_finished = handle.join_handle.is_finished();
                    self.sessions.remove(session_id);
                    if let Some(ref snapshots) = self.config.session_snapshots {
                        snapshots.write().unwrap().remove(session_id);
                    }
                    self.session_configs.write().unwrap().remove(session_id);
                    self.urgent_stops.remove(session_id);
                    self.session_committed_lines.remove(session_id);
                    self.session_delivery_cursors
                        .write()
                        .unwrap()
                        .remove(session_id);
                    tracing::warn!(
                        session_id = %session_id,
                        task_finished = was_finished,
                        "Session channel closed — auto-removing dead session handle"
                    );
                    Err(RuntimeError::Config(format!(
                        "Session not found: {}",
                        session_id
                    )))
                }
            },
        }
    }

    /// ADR-038: Observe the lifecycle state of a session.
    ///
    /// - `Active` if a session handle exists in the in-memory map.
    /// - `Closed` if the session-meta store holds a row but no handle is loaded.
    /// - `NotFound` if neither exists.
    ///
    /// Lifecycle is now explicit (ADR-038 §3): there is no lazy-resume
    /// helper anymore — the `open_session` MQTT control command is the
    /// single entry point for transitioning Closed/NotFound → Active, and
    /// other handlers route through `dispatch_inbound` (which publishes
    /// `SessionNotOpened` when missing) or call [`Self::open`] directly.
    pub fn get_lifecycle_state(&self, session_id: &str, work_dir: &Path) -> SessionLifecycleState {
        if self.sessions.contains_key(session_id) {
            return SessionLifecycleState::Active;
        }
        // ADR-082 §4 step 3: the session-meta store (SQLite) is the sole
        // source of truth post-migration. The legacy
        // `conversations/meta/{sid}.json` sidecar is bootstrap-only and no
        // longer written by the runtime — probing it here is what made a
        // valid SQLite-backed session report `NotFound`.
        let conversations_dir = work_dir.join("conversations");
        if crate::conversation::session_meta_exists(&conversations_dir, session_id) {
            return SessionLifecycleState::Closed;
        }
        SessionLifecycleState::NotFound
    }

    /// ADR-038: activate a session, distinguishing "absent" from "closed".
    ///
    /// Returns `Ok(None)` when no session with that id exists on disk, so
    /// callers can map it to their own not-found shape (404 over HTTP, a
    /// `SessionNotOpened` event over MQTT). This is the single
    /// implementation of the open state machine — `open()` is the raw
    /// primitive and does not distinguish NotFound by return value.
    pub async fn resume_session(
        &mut self,
        session_id: &str,
    ) -> Result<Option<SessionOpenOutcome>> {
        let work_dir = std::path::PathBuf::from(&self.core.config.work_dir);
        let outcome = match self.get_lifecycle_state(session_id, &work_dir) {
            SessionLifecycleState::NotFound => return Ok(None),
            SessionLifecycleState::Active => SessionOpenOutcome::AlreadyActive,
            SessionLifecycleState::Closed => self.open(session_id, &work_dir).await?,
        };
        // Every successful open — including the no-op AlreadyActive path —
        // refreshes the session's retained `SessionConfig` snapshot. This is
        // an invariant of the open state machine, not of one transport: the
        // availability fan-out only re-publishes sessions that were active at
        // the moment of a transition, so a session opened after a transition
        // would otherwise keep serving a stale retained `llm_availability`
        // forever (permanent "syncing LLM config" banner in the Desktop).
        self.republish_session_config(session_id);
        Ok(Some(outcome))
    }

    /// ADR-076 §决策 4: set the read visibility of a session.
    ///
    /// Works whether the session is live or only on disk: a live session
    /// goes through its [`ConversationSession`] (which persists meta), a
    /// closed one has its meta read-modify-written in place. Returns
    /// `false` if no such session exists.
    pub fn set_session_visibility(
        &self,
        session_id: &str,
        visibility: Option<crate::conversation::SessionVisibility>,
    ) -> bool {
        if let Some(handle) = self.sessions.get(session_id)
            && let Some(conv) = handle.conversation.as_ref()
        {
            conv.set_visibility(visibility);
            return true;
        }

        let conversations_dir =
            std::path::Path::new(&self.core.config.work_dir).join("conversations");
        let Ok(mut meta) =
            crate::conversation::read_session_meta(&conversations_dir, session_id)
        else {
            return false;
        };
        meta.visibility = visibility;
        crate::conversation::write_session_meta(&conversations_dir, &meta).is_ok()
    }

    /// ADR-038: Explicit session activation (transitions Closed/NotFound → Active).
    ///
    /// Idempotent: an already-Active session returns
    /// [`SessionOpenOutcome::AlreadyActive`] without touching disk. A Closed
    /// session is lazy-resumed from JSONL into memory, returning
    /// [`SessionOpenOutcome::ResumedFromDisk`]. A NotFound session returns an
    /// error (no meta file on disk).
    ///
    /// This replaces the implicit "lazy resume before every command" pattern.
    /// Frontend should call this explicitly via the MQTT `open_session`
    /// command; runtime dispatch paths should rely on the explicit lifecycle
    /// contract and remove their own lazy-resume fallbacks.
    pub async fn open(&mut self, session_id: &str, work_dir: &Path) -> Result<SessionOpenOutcome> {
        if self.sessions.contains_key(session_id) {
            return Ok(SessionOpenOutcome::AlreadyActive);
        }

        // Validate existence up-front so callers get a clear error instead of
        // a generic failure buried inside the resume path. ADR-082 §4 step 3:
        // the session-meta store (SQLite) is the canonical "session exists"
        // marker — the JSON sidecar is bootstrap-only and no longer written.
        let conversations_dir = work_dir.join("conversations");
        if !crate::conversation::session_meta_exists(&conversations_dir, session_id) {
            return Err(RuntimeError::Config(format!(
                "Session not found on disk: {}",
                session_id
            )));
        }

        // Each resumed session gets its own committed_lines counter.
        // The writer thread (inside ConversationSession) increments it;
        // the session's AgentCore reads it via clone_for_session.
        let committed_lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (conv, config_rx, state_rx) = ConversationSession::resume(
            work_dir,
            session_id,
            committed_lines.clone(),
        )
        .map_err(|e| {
            RuntimeError::Config(format!("Session not found on disk: {} ({})", session_id, e))
        })?;

        // ADR-043: Spawn config + state change relays.
        if let Some(chunk_tx) = self.config.chunk_tx.clone() {
            crate::startup::subsystems::spawn_config_change_relay(
                config_rx,
                chunk_tx.clone(),
                conv.clone(),
                session_id.to_string(),
                std::sync::Arc::new(std::sync::RwLock::new(Some(self.core.clone()))),
            );
            crate::startup::subsystems::spawn_state_change_relay(
                state_rx,
                chunk_tx,
                conv.clone(),
                session_id.to_string(),
            );
        }

        // ADR-028 / ADR-066: merge the resumed session's persisted token
        // totals into the AgentCore counters so the live context_usage
        // WebSocket push doesn't report agent_total < session_total
        // after a process restart.  Cache fields use `Some` only when
        // the persisted meta has them — older meta files default to 0
        // (handled by `SessionTokens::default`).
        if let Some(t) = conv.tokens() {
            self.core.merge_token_totals((
                Some(t.total_input),
                Some(t.total_output),
                Some(t.total_cache_read),
                Some(t.total_cache_write),
            ));
        }

        self.create_session_with_id_and_conversation(
            session_id.to_string(),
            Some(conv),
            Some(committed_lines),
        )
        .await?;

        tracing::info!(
            session_id = %session_id,
            "SessionManager::open: lazy-resumed session from disk"
        );
        Ok(SessionOpenOutcome::ResumedFromDisk)
    }

    /// Read `(model, provider, last_active_at_iso)` for a session from its meta file.
    ///
    /// `last_active_at_iso` is the raw ISO-8601 string from the meta file (the
    /// Runtime does not parse it to epoch seconds). Returns `(None, None, None)`
    /// when the meta file is missing or unreadable.
    /// Used by the `open_session` handler to populate the
    /// [`crate::acowork_core::mqtt_proto::SessionOpened`] ack payload.
    ///
    /// `conversations_dir` is the workspace's `…/conversations` directory
    /// (already joined) — every `conversation::*` meta helper takes that, and
    /// so does the `open_session` handler. Joining it here as well is what
    /// used to make the meta store open at `…/conversations/memory/private.sqlite`.
    pub fn session_metadata_summary(
        &self,
        session_id: &str,
        conversations_dir: &Path,
    ) -> (Option<String>, Option<String>, Option<String>) {
        match read_session_meta(conversations_dir, session_id) {
            Ok(meta) => (meta.model, meta.provider, Some(meta.last_active_at)),
            Err(_) => (None, None, None),
        }
    }

    /// Broadcast a message to all active sessions.
    ///
    /// Returns a list of session IDs that failed to receive the message
    /// (e.g., because the channel was closed).
    pub fn broadcast(&self, msg: SessionMessage) -> Vec<String> {
        let mut failed = Vec::new();
        for (session_id, handle) in &self.sessions {
            if handle.send(msg.clone()).is_err() {
                failed.push(session_id.clone());
            }
        }
        if !failed.is_empty() {
            tracing::warn!(
                failed_count = failed.len(),
                "Broadcast failed for some sessions"
            );
        }
        failed
    }

    /// Apply a runtime config override pushed by Gateway.
    ///
    /// This performs three actions atomically from the caller's perspective:
    ///   0. Rewrite the shared `AgentCore` **template** via
    ///      [`crate::agent::agent_core::AgentCore::apply_runtime_config`]
    ///      (clone-on-write; the boot path's cold-start merge stays the
    ///      only way a new session can clone a stale template).
    ///   1. Merge the override into the `runtime_overrides` cache so any
    ///      session created *after* this call also picks it up (fixing the
    ///      bug where a fresh session would clone the untouched
    ///      `Arc<AgentCore>` template and silently ignore user-applied
    ///      values such as `max_iterations`).
    ///   2. Broadcast the override as `SessionMessage::UpdateRuntimeConfig`
    ///      to all active SessionTask inboxes (each task rebuilds its
    ///      `ContextBuilder.tool_definitions` and applies core flags).
    ///   3. Also deliver `InboundMessage::UserOperation(UpdateRuntimeConfig)`
    ///      via `send_inbound()` fast channel so mid-execution AgentLoops
    ///      pick up the change immediately (their `apply_user_op` syncs
    ///      `core.builtin_tools` + `all_tools` on the in-flight snapshot).
    pub fn apply_runtime_config_override(
        &mut self,
        overrides: &RuntimeConfigOverrides,
    ) -> Vec<String> {
        // Capture pre-merge cached `session_language` so Step 0.5 can
        // detect whether THIS push actually changed it (vs. touching
        // unrelated fields like temperature). After `merge` the cache
        // is already the new value, so we'd lose the diff signal.
        let prev_session_language = self.runtime_overrides.session_language.clone();
        self.runtime_overrides.merge(overrides);

        // ── Step 0: rewrite the shared AgentCore template ─────────
        // Clone-on-write: if other sessions still hold `Arc<AgentCore>`
        // clones (they do - see `SessionTask::new`'s `(*core).clone()`),
        // the template is deep-cloned before mutation. That's expensive
        // but correct: it isolates the template from in-flight sessions,
        // each of which carries its own `core_mut` snapshot anyway. The
        // cost is bounded - `RuntimeConfigUpdate` is a rare event (user
        // toggles a setting in the Settings panel).
        // No platform-tool hot-reload path remains; runtime config
        // updates apply through the standard `apply_runtime_config`
        // hook.
        Arc::make_mut(&mut self.core).apply_runtime_config(overrides);

        // ── Step 0.5: re-format identity_context for in-flight sessions ──
        // `apply_runtime_config` above just stamped the new
        // `session_language_override` onto AgentCore, but the cached
        // `identity_context` string on `self.config` and on every
        // running session's `ContextBuilder` is still formatted with
        // the OLD language. Without this, an Agent Setup panel change
        // to `session_language` would only affect future sessions.
        // We only re-render when the override actually changed (skip
        // the no-op path so we don't spam broadcasts on every other
        // field PUT).
        if overrides.session_language.is_some()
            && overrides.session_language.as_ref() != prev_session_language.as_ref()
        {
            if let Some(profile) = self.last_user_profile.clone() {
                let lang_override = self.core.session_language_override.as_deref();
                let identity_context = format_user_profile_context(&profile, lang_override);
                self.config.identity_context = Some(identity_context.clone());
                for handle in self.sessions.values() {
                    let _ = handle.send(SessionMessage::UpdateIdentityContext {
                        identity_context: Some(identity_context.clone()),
                    });
                }
                tracing::info!(
                    new_lang = ?lang_override,
                    session_count = self.sessions.len(),
                    "Re-formatted identity_context after session_language override change"
                );
            } else {
                tracing::debug!(
                    "session_language override changed but no cached UserProfile yet; \
                     next UserProfileUpdate push will pick it up via update_user_identity"
                );
            }
        }

        // ── Step 1: broadcast to SessionTask inboxes (for tool definitions etc.) ──
        // The handler rebuilds `ContextBuilder.tool_definitions` so the
        // LLM sees the new set on the next `build_chat_request`.
        let sessions = self.broadcast(SessionMessage::UpdateRuntimeConfig(overrides.clone()));

        // ── Step 2: deliver via send_inbound() fast channel ──
        // Mid-execution AgentLoops pick up the change immediately (the
        // SessionMessage above queues until the next idle boundary).
        let user_op = UserOp::UpdateRuntimeConfig(Box::new(overrides.clone()));
        let inbound_msg = InboundMessage::UserOperation(user_op);
        for (session_id, handle) in &self.sessions {
            if let Err(e) = handle.send_inbound(inbound_msg.clone()) {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Failed to deliver UpdateRuntimeConfig via send_inbound (session channel may be full or closed)"
                );
            }
        }

        // ADR-039: temperature no longer mirrors into the runtime snapshot.
        // The override propagates through UpdateRuntimeConfig → AgentLoop →
        // SessionState.set_temperature → meta_change_tx → meta.json +
        // session_meta MQTT channel. The Gateway pull API reads from
        // data/meta/{session_id}.json so the new value is reflected on the
        // next fetchSessionState (or immediately via the live MQTT message).

        // ADR-047 3.3.3: temperature is a config field that must be
        // persisted to meta.json + trigger MQTT config notification.
        // The runtime override path above only sets the transient
        // AgentCore.temperature_override (not persisted). Here we also
        // route temperature through apply_config() so it lands in
        // ConversationSession (persisted) + config_version increment +
        // MQTT session/config retained message.
        if let Some(temp) = overrides.temperature {
            for (session_id, handle) in &self.sessions {
                if let Some(ref conv) = handle.conversation {
                    let delta = crate::agent::session_config::SessionConfigDelta {
                        temperature: Some(temp),
                        ..Default::default()
                    };
                    conv.apply_config(&delta);
                    tracing::info!(
                        session_id = %session_id,
                        temperature = temp,
                        "ADR-047: temperature persisted via apply_config (UpdateRuntimeConfig split)"
                    );
                }
            }
        }

        sessions
    }

    /// Apply MCP server configuration changes from Gateway RuntimeConfigUpdate.
    ///
    /// Connects to (or disconnects from) MCP servers and updates:
    ///   - `self.mcp_tools` — the tool wrappers for dispatch
    ///   - `self.config.full_tool_specs` — LLM-facing tool definitions
    ///   - `self.config.tool_definitions` — current active tool definitions
    ///
    /// When `configs` is `Some(vec![])`, all MCP servers are disconnected.
    /// Apply pre-connected MCP results (without performing the connection IO).
    ///
    /// This is used for startup MCP auto-connect where the actual connection
    /// is performed in a background task and results are applied asynchronously
    /// when ready — so the Gateway message loop can start immediately without
    /// blocking on MCP timeouts.
    pub fn apply_mcp_connection_result(
        &mut self,
        registry: Arc<McpRegistry>,
        wrappers: Vec<McpToolWrapper>,
        _specs: Vec<(String, serde_json::Value)>,
        failures: Vec<McpConnectionFailure>,
    ) {
        use acowork_core::tools::traits::Tool;

        // Store the registry in the MCP manager
        self.mcp_manager.set_registry(registry);

        // Store MCP tool wrappers (Arc<dyn Tool>) for dispatch
        let mcp_tool_arcs: Vec<Arc<dyn Tool>> = wrappers
            .into_iter()
            .map(|w| Arc::new(w) as Arc<dyn Tool>)
            .collect();
        self.mcp_tools = Some(mcp_tool_arcs.clone());

        // Push MCP tools to all existing sessions
        self.broadcast(SessionMessage::UpdateMcpTools {
            mcp_tools: Some(mcp_tool_arcs),
        });

        // Update full_tool_specs to include MCP tool specs
        self.rebuild_full_tool_specs_with_mcp();

        // NOTE: McpRegistry::connect_all() already logs server/tool counts.
        // We log a summary here for the SessionManager context.
        let server_count = self
            .mcp_manager
            .registry()
            .map(|r| r.server_count())
            .unwrap_or(0);
        tracing::info!(
            server_count,
            tool_count = self.mcp_tools.as_ref().map(|t| t.len()).unwrap_or(0),
            failure_count = failures.len(),
            "SessionManager: MCP servers applied (async background connect)"
        );

        // Inject system notification for connection failures
        if !failures.is_empty() {
            let failure_lines: Vec<String> = failures
                .iter()
                .map(|f| format!("- Server \"{}\": {}", f.server_name, f.error_message))
                .collect();
            let notification = format!(
                "MCP server connection failed:\n{}\n\n\
You are an AI agent. If any of the above MCP servers require dependencies \
that need to be installed, use your shell tools to install them. \
After installation, ask the user to re-enable the MCP server.",
                failure_lines.join("\n")
            );
            tracing::warn!(
                failure_count = failures.len(),
                notification_len = notification.len(),
                "SessionManager: broadcasting MCP connection failure notification"
            );
            self.broadcast(SessionMessage::SystemNotification {
                content: notification,
            });
        }
    }

    /// ADR-029 + ADR-052: Apply builtin-tools enabled flags from a Gateway
    /// `RuntimeConfigUpdate` agent-wide.
    ///
    /// Companion of [`Self::apply_runtime_config_override`] for the
    /// `agent_tools.json` toggle panel - same one-pipeline principle:
    ///   0. Rewrite the shared `AgentCore` **template** enabled flags
    ///      via [`crate::agent::agent_core::AgentCore::apply_builtin_enabled_entries`]
    ///      (clone-on-write: in-flight sessions that already deep-cloned
    ///      the template keep their own snapshot - isolated).
    ///   1. Broadcast `SessionMessage::UpdateBuiltinTools` to all active
    ///      sessions; each SessionTask rewrites its own `builtin_tools`
    ///      enabled flags via the **same** shared policy helper, then
    ///      rebuilds dispatch list + LLM `tool_definitions` atomically
    ///      (`session_task::apply_builtin_tools_update`).
    ///
    /// Without step 0, a session opened AFTER this call would deep-clone
    /// the template's stale enabled flags - the same template-drift
    /// bug `apply_runtime_config_override` was created to fix for
    /// `RuntimeConfigUpdate`. Step 1 is what makes the LLM-visible
    /// `ContextBuilder.tool_definitions` of every active session pick
    /// up the change in lock-step.
    ///
    /// Persistence (`agent_tools.json`) is the HTTP-layer UseCase's
    /// job; this function only mutates in-memory state. Per-session
    /// broadcast failures (closed session, full channel) are logged
    /// but do NOT fail the call - the template update already
    /// succeeded, and closed sessions will pick up the new state via
    /// the on-disk file on their next open.
    pub fn apply_builtin_tools_enabled(&mut self, entries: &[crate::agent_config::AgentToolEntry]) {
        tracing::info!(
            entry_count = entries.len(),
            enabled_count = entries.iter().filter(|e| e.enabled).count(),
            "SessionManager: applying builtin tools enabled list (template sync + broadcast)"
        );
        Arc::make_mut(&mut self.core).apply_builtin_enabled_entries(entries);
        let failed = self.broadcast(SessionMessage::UpdateBuiltinTools {
            entries: entries.to_vec(),
        });
        if !failed.is_empty() {
            tracing::warn!(
                failed_sessions = failed.len(),
                "apply_builtin_tools_enabled: some sessions missed the broadcast (likely closed)"
            );
        }
    }

    /// ADR-063 §3.7.6: replace the main-dialog system prompt agent-wide.
    ///
    /// Called by `gateway_loop::dispatch_inbound` when the L2 reload
    /// endpoint (`POST /agents/{id}/prompts/reload`) rebuilds the main
    /// dialog system prompt from `prompts/*.md` (including the required
    /// `system.md`). Mirrors the `apply_runtime_config_override` /
    /// `apply_builtin_tools_enabled` policy — the system prompt is
    /// **agent-scoped**, so one system-level message updates:
    ///
    /// 1. the shared `SessionManagerConfig.system_prompt` template — so
    ///    sessions opened LATER inherit the new value, and
    /// 2. every active session's `ContextBuilder` via the
    ///    `SessionMessage::UpdateSystemPrompt` broadcast — so the next
    ///    `build_chat_request` uses the new prompt without a restart.
    pub fn apply_system_prompt(&mut self, system_prompt: &str) {
        tracing::info!(
            prompt_len = system_prompt.len(),
            "SessionManager: applying main-dialog system prompt (template sync + broadcast)"
        );
        // Step 1: rewrite the shared template for future sessions.
        self.config.system_prompt = system_prompt.to_string();
        // Step 2: push to every active session's ContextBuilder.
        let failed = self.broadcast(SessionMessage::UpdateSystemPrompt {
            system_prompt: system_prompt.to_string(),
        });
        if !failed.is_empty() {
            tracing::warn!(
                failed_sessions = failed.len(),
                "apply_system_prompt: some sessions missed the broadcast (likely closed)"
            );
        }
    }

    // ── ADR-030 C3: dynamic builtin tool registration (SidecarEndpointUpdate) ──
    //
    // When a sidecar's state changes (LSP relay became ready, sidecar
    // went away, ...), `cli.rs` calls these methods. They wrap the raw
    // tool with the same security decorators used at startup (path guard
    // + rate limiter) and broadcast a `SessionMessage` so every active
    // session rebuilds its dispatch list.

    /// Register a dynamic builtin tool. The raw `Arc<dyn Tool>` is
    /// wrapped with `PathGuardedTool` and `RateLimitedTool` (matching
    /// the startup path in `ToolRegistry::activate`) and then broadcast
    /// to all sessions. Sessions replace any existing entry with the
    /// same `name()` so a sidecar endpoint re-push is idempotent.
    ///
    /// Caller passes the per-session rate limit (`max_calls_per_minute`)
    /// and the shared `SharedResolver`; both are already available at
    /// the runtime call site (`cli.rs`).
    pub fn register_dynamic_tool(
        &mut self,
        tool: Arc<dyn Tool>,
        resolver: crate::tools::workspace_resolver::SharedResolver,
        max_calls_per_minute: u32,
        enabled: bool,
    ) {
        let name = tool.name().to_string();
        let wrapped = crate::tools::wrappers::wrap_with_security_decorators(
            tool,
            resolver,
            max_calls_per_minute,
        );
        let entry = crate::agent::agent_core::BuiltinToolEntry {
            tool: wrapped,
            enabled,
        };

        // Store on the manager so new sessions created after this push
        // also inherit the tool (ADR-030 review ISSUE-1 fix).
        if let Some(existing) = self
            .dynamic_builtin_tools
            .iter()
            .position(|e| e.name() == name)
        {
            self.dynamic_builtin_tools[existing] = entry.clone();
        } else {
            self.dynamic_builtin_tools.push(entry.clone());
        }

        let failed = self.broadcast(SessionMessage::AddDynamicBuiltinTool { entry });
        tracing::info!(
            tool = %name,
            enabled,
            failed_count = failed.len(),
            "SessionManager: dynamic builtin tool registered"
        );
    }

    /// Unregister a dynamic builtin tool by name. Idempotent: removing a
    /// tool that no session has is still a no-op broadcast (sessions
    /// silently skip unknown names).
    pub fn unregister_dynamic_tool(&mut self, name: &str) {
        // Remove from the manager's stored list so new sessions don't
        // inherit a stale tool (ADR-030 review ISSUE-1 fix).
        self.dynamic_builtin_tools.retain(|e| e.name() != name);

        let failed = self.broadcast(SessionMessage::RemoveDynamicBuiltinTool {
            name: name.to_string(),
        });
        tracing::info!(
            tool = %name,
            failed_count = failed.len(),
            "SessionManager: dynamic builtin tool unregistered"
        );
    }

    /// Handle a node LSP relay state change (ADR-055 §6.7, Phase 4).
    ///
    /// Currently a **no-op**: the `codebase` tool is temporarily out of
    /// the builtin registry (see `tools/builtin/mod.rs`), so a relay
    /// becoming ready must not push it back into any session or into
    /// `agent_tools.json`. Any stale persisted `codebase` row is dropped
    /// by the startup merge instead.
    ///
    /// ponytail: the MQTT plumbing (`lsps_update_rx` →
    /// `mqtt_only_loop` → this method) is kept intact so re-enabling
    /// `codebase` is a local edit: restore the body from git history,
    /// re-add the `all_builtin_tools` branch, and put `"codebase"` back
    /// in `CONDITIONALLY_REGISTERED_TOOL_NAMES`. Deleting the whole
    /// channel would touch `context.rs`, `agent_init.rs`, `cli.rs` and
    /// `mqtt_only_loop`'s 9-argument signature for zero user-visible gain.
    pub fn handle_lsp_relay_update(&mut self, endpoint: Option<String>) {
        tracing::debug!(
            endpoint = endpoint.as_deref().unwrap_or("<none>"),
            "SessionManager: LSP relay update ignored (codebase tool temporarily unregistered)"
        );
    }

    /// Returns `(name, enabled)` pairs for all dynamically registered
    /// builtin tools (via `SidecarEndpointUpdate`).
    ///
    /// Used by the `ConfigSnapshot` builder to merge with the persisted
    /// `agent_tools.json` so the frontend tool panel reflects tools that
    /// were registered after startup.
    pub fn dynamic_builtin_tool_names(&self) -> Vec<(String, bool)> {
        self.dynamic_builtin_tools
            .iter()
            .map(|e| (e.name(), e.enabled))
            .collect()
    }

    /// Rebuild `full_tool_specs` by merging the original built-in specs with
    /// any currently connected MCP tool specs.
    fn rebuild_full_tool_specs_with_mcp(&mut self) {
        // Start from the original built-in tool specs (stored at init time).
        // We store these separately to avoid losing them on rebuild.
        let mut specs = self.config.full_tool_specs.clone();

        // Remove any previous MCP entries (prefixed with "mcp_")
        specs.retain(|(name, _)| !name.starts_with("mcp_"));

        // Add current MCP tool specs
        if let Some(ref wrappers) = self.mcp_tools {
            for tool in wrappers {
                let tool_spec = tool.spec();
                let serialized = serde_json::to_value(&tool_spec).unwrap_or_default();
                specs.push((tool_spec.name, serialized));
            }
        }

        self.config.full_tool_specs = specs;
    }

    /// Update the global provider list from a ProviderListUpdate push.
    ///
    /// Updates the shared AgentCore's `global_provider_list`, version, and
    /// `provider_key_vault`. No per-session broadcast is needed — sessions
    /// query the shared cache on demand via
    /// [`AgentCore::get_provider`] / [`AgentCore::get_model_capabilities`].
    pub fn update_global_provider_list(
        &mut self,
        provider_list: Vec<acowork_core::protocol::ProviderListItem>,
        provider_list_version: u64,
        provider_key_vault: Vec<acowork_core::protocol::ProviderKeyEntry>,
        default_compact_models: Vec<acowork_core::protocol::CompactModelRef>,
    ) {
        tracing::info!(
            provider_count = provider_list.len(),
            version = provider_list_version,
            key_count = provider_key_vault.len(),
            default_compact_count = default_compact_models.len(),
            "SessionManager: updating global provider list"
        );

        // All three fields are shared `Arc<RwLock<...>>` (ADR-063 §3.7.5
        // pattern) — writing through the RwLock is immediately visible to
        // every session's deep-cloned AgentCore, so no `Arc::get_mut`
        // sole-owner dance is needed. (That dance silently skipped the
        // update whenever debug mode or any other live Arc clone existed,
        // leaving in-flight sessions with the stale value.)
        {
            let mut pcm = self.core.provider_compact_models.write().unwrap();
            pcm.clear();
            for provider in &provider_list {
                pcm.insert(provider.id.clone(), provider.compact_model.clone());
            }
        }
        *self.core.provider_list_version.write().unwrap() = provider_list_version;
        // ADR-056: Sync the global compact-model candidate list through the
        // shared Arc — in-flight sessions see the new value on their next
        // resolve_distill_model() call, no session broadcast needed.
        *self.core.default_compact_models.write().unwrap() = default_compact_models
            .iter()
            .map(|r| (r.provider_id.clone(), r.model_id.clone()))
            .collect();

        // Replace the shared global provider list (live read-view for sessions).
        {
            let mut list = self.core.global_provider_list.write().unwrap();
            *list = provider_list;
        }

        // Notify all active sessions so they emit an updated state to the frontend.
        // The shared global_provider_list on AgentCore is already updated above,
        // so sessions can query it on demand via get_model_capabilities.
        let session_count = self.broadcast(SessionMessage::ProviderListUpdated).len();
        tracing::debug!(
            session_count = %session_count,
            "ProviderListUpdated: broadcast to all active sessions"
        );

        // Replace the shared key vault (in-memory only, never persisted).
        {
            let mut vault = self.core.provider_key_vault.write().unwrap();
            vault.clear();
            for entry in provider_key_vault {
                vault
                    .entry(entry.provider_id.clone())
                    .or_default()
                    .push(entry);
            }
        }
    }

    /// Route a model switch to a specific session (ADR-012: per-session model).
    ///
    /// ADR-047: config persistence is now synchronous via `apply_config()`,
    /// bypassing the serial inference queue. LLM-side effects (provider
    /// rebuild, context builder update) are deferred to the next turn
    /// boundary via version polling in SessionTask.
    ///
    /// The new model's default `reasoning_effort` is resolved here and
    /// pre-set on the `ConversationSession` **before** `apply_config()`
    /// so that the single `notify_config_change()` inside `apply_config`
    /// publishes a snapshot with the correct `reasoning_effort`.
    /// Previously, `apply_config` published a stale value from the
    /// previous model, and `apply_llm_effects` (deferred to the next
    /// turn boundary) cleared it too late - the frontend received the
    /// stale value and its preserve-on-null rule prevented the
    /// subsequent clear signal from taking effect.
    pub fn route_model_switch(
        &self,
        session_id: &str,
        model: String,
        provider: Option<String>,
        account_id: Option<String>,
    ) -> Result<()> {
        tracing::info!(
            session_id = %session_id,
            model = %model,
            provider = ?provider,
            "SessionManager: routing model_switch (ADR-047: synchronous apply_config)"
        );
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;
        if let Some(ref conv) = handle.conversation {
            // Resolve the new model's default reasoning_effort using the
            // same three-level priority chain as session init and HTTP GET:
            //   1. persisted (None on model switch - clears any user override)
            //   2. caps.default_reasoning_effort
            //   3. supports_reasoning -> Auto
            //   4. None (model doesn't support reasoning)
            let caps = self.core.get_model_capabilities(&model);
            // ADR-061 §13.4: reject model switches to models whose effective
            // input budget cannot run the 8-level compression loop.
            if let Some(caps) = caps.as_ref()
                && let Err(e) = crate::agent::compression_constants::validate_model_budget(caps)
            {
                tracing::warn!(
                    session_id = %session_id,
                    model = %model,
                    error = %e,
                    "model_switch rejected: context budget below MIN_BUDGET_FOR_AGENT"
                );
                return Err(e);
            }
            let default_effort =
                crate::agent::session_config::llm_effects::resolve_effective_reasoning_effort(
                    caps.as_ref(),
                    None, // model switch: no persisted override
                );
            let effort_str = default_effort.as_ref().map(|e| e.to_string());

            // Pre-set reasoning_effort without notify_config_change.
            // apply_config's notify_config_change will publish the
            // correct combined state (new model + new reasoning_effort)
            // in a single MQTT message.
            conv.set_reasoning_effort_raw(effort_str);

            let delta = crate::agent::session_config::SessionConfigDelta {
                model: Some(model.clone()),
                provider: provider.clone(),
                account_id: account_id.clone(),
                ..Default::default()
            };
            conv.apply_config(&delta);
        }

        // Mirror the switched model/provider into the per-session snapshot so
        // the next `current_model_and_provider()` call — invoked by
        // `build_initial_session_state` for newly created sessions — sees the
        // switched value immediately instead of the stale value written when
        // the session was created.
        //
        // Regression background: `apply_llm_effects` updates
        // `session.set_model` / `set_provider` (in-memory SessionState) but
        // does NOT touch the SessionRuntimeSnapshot. The snapshot is only
        // refreshed by `AgentLoop::emit_session_state`, which fires on the
        // next status transition — far too late for a `create_session` that
        // happens right after the switch. Result: new sessions fall back to
        // `global_provider_list.first()` (default provider, e.g. minimax)
        // because the filter in `current_model_and_provider()` drops every
        // session whose snapshot.model is still None.
        if let Some(mut snap) = self
            .sessions
            .get(session_id)
            .and_then(|h| h.snapshot.write().ok())
        {
            snap.model = Some(model);
            if let Some(ref provider) = provider {
                snap.provider = Some(provider.clone());
            }
            // Mirror the multi-account pick alongside model/provider so
            // `current_account_id()`'s Level-2 lookup sees the freshly
            // switched value immediately (same one-emit-cycle window as
            // the model/provider mirror above).
            if let Some(ref account) = account_id {
                snap.account_id = Some(account.clone());
            }
        }

        Ok(())
    }

    /// Route per-session reasoning effort override to the target session.
    ///
    /// ADR-047: config persistence is now synchronous via `apply_config()`,
    /// bypassing the serial inference queue. LLM-side effects are deferred
    /// to the next turn boundary.
    pub fn route_reasoning_effort(&self, session_id: &str, effort: String) -> Result<()> {
        tracing::info!(
            session_id = %session_id,
            effort = %effort,
            "SessionManager: routing reasoning_effort (ADR-047: synchronous apply_config)"
        );
        let handle = self
            .sessions
            .get(session_id)
            .ok_or_else(|| RuntimeError::Config(format!("Session not found: {}", session_id)))?;
        if let Some(ref conv) = handle.conversation {
            let delta = crate::agent::session_config::SessionConfigDelta {
                reasoning_effort: Some(effort),
                ..Default::default()
            };
            conv.apply_config(&delta);
        }
        Ok(())
    }

    /// Update user identity from Gateway UserProfileUpdate push.
    ///
    /// Formats the `UserProfile` into an `identity_context` text block
    /// and broadcasts it to all active sessions via their ContextBuilder.
    pub fn update_user_identity(&mut self, profile: Option<acowork_core::protocol::UserProfile>) {
        // Cache the profile so a later `apply_runtime_config_override`
        // (e.g. Agent Setup panel changed `session_language`) can
        // re-format the identity_context for in-flight sessions
        // without waiting for the next Gateway `UserProfileUpdate`
        // push (which might never come).
        self.last_user_profile = profile.clone();

        // Per-agent session language override (AgentConfig.session_language);
        // wins over `profile.language` so a user can pin one agent's
        // session language independently from the global UI/identity
        // language. `None` = fall through to `profile.language`.
        let lang_override = self.core.session_language_override.as_deref();
        let identity_context = profile
            .as_ref()
            .map(|p| format_user_profile_context(p, lang_override));
        tracing::info!(
            has_profile = profile.is_some(),
            ctx_len = identity_context.as_ref().map(|s| s.len()).unwrap_or(0),
            "SessionManager: updating user identity"
        );
        self.config.identity_context = identity_context.clone();
        // Broadcast updated identity to all active sessions
        for handle in self.sessions.values() {
            let _ = handle.send(SessionMessage::UpdateIdentityContext {
                identity_context: identity_context.clone(),
            });
        }
    }

    /// Handle embedding config update from Gateway (via SidecarEndpointUpdate(Embed)).
    ///
    /// When the user switches the active embedding model, the Gateway pushes
    /// this update to all running Runtimes. The Runtime rebuilds its
    /// FallbackEmbeddingProvider chain with the new ONNX provider as the
    /// first entry, following the same cache + broadcast pattern as
    /// `update_llm_config` (ADR-012).
    ///
    /// ADR-s1: when `embed_provider_id` is `Some(...)`, the endpoint above
    /// carries the cloud base URL (e.g. `https://ark.cn-beijing.volces.com/api/v3`)
    /// and `embed_api_key` is the decrypted API key — sessions construct
    /// a `RemoteEmbeddingProvider` against it instead of the local ONNX
    /// sidecar. When both are `None`, behaviour is unchanged.
    pub fn handle_embedding_config_update(
        &mut self,
        embed_endpoint: String,
        embed_model_id: String,
        embed_dimension: usize,
        embed_provider_id: Option<String>,
        embed_api_key: Option<String>,
    ) {
        tracing::info!(
            endpoint = %embed_endpoint,
            model_id = %embed_model_id,
            dimension = embed_dimension,
            cloud_provider = embed_provider_id.as_deref().unwrap_or("<local-onnx>"),
            has_api_key = embed_api_key.is_some(),
            "SessionManager: received embedding config update via SidecarEndpointUpdate"
        );

        // Broadcast to all existing sessions so they rebuild their
        // embedding provider in-place (same pattern as UpdateProvider).
        for (sid, handle) in &self.sessions {
            if handle
                .send(SessionMessage::UpdateEmbedConfig {
                    embed_endpoint: embed_endpoint.clone(),
                    embed_model_id: embed_model_id.clone(),
                    embed_dimension,
                    embed_provider_id: embed_provider_id.clone(),
                    embed_api_key: embed_api_key.clone(),
                })
                .is_err()
            {
                tracing::warn!(
                    session_id = %sid,
                    "Failed to send UpdateEmbedConfig to session (channel closed)"
                );
            }
        }
    }

    /// Clear the embedding provider on all sessions (embed sidecar went down).
    ///
    /// Broadcasts `DisableEmbedConfig` so each session clears its ONNX
    /// embedding provider. New sessions created after this call will
    /// inherit the cleared state from the shared template (ADR-030 ISSUE-2).
    pub fn clear_embedding_config(&self) {
        tracing::info!(
            "SessionManager: clearing embedding provider on all sessions (embed sidecar unavailable)"
        );
        let failed = self.broadcast(SessionMessage::DisableEmbedConfig);
        if !failed.is_empty() {
            tracing::warn!(
                failed_count = failed.len(),
                "Some sessions failed to receive DisableEmbedConfig"
            );
        }
    }

    /// Get all active session IDs.
    pub fn active_sessions(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }

    /// Snapshot every active session's `(session_id, conversation)` pair.
    ///
    /// Returns owned tuples so callers can release the [`SessionManager`]
    /// lock before calling into [`ConversationSession`]
    /// (e.g. `build_session_config_snapshot`, which is not lock-safe).
    /// Used by the chunk_relay's LLM-availability watcher to re-publish
    /// every active session's `SessionConfig` after a state transition.
    pub fn snapshot_active_conversations(
        &self,
    ) -> Vec<(
        String,
        std::sync::Arc<crate::conversation::ConversationSession>,
    )> {
        self.sessions
            .iter()
            .filter_map(|(sid, h)| h.conversation.as_ref().map(|c| (sid.clone(), c.clone())))
            .collect()
    }

    /// Re-publish one active session's `SessionConfig` snapshot.
    ///
    /// Called by [`Self::resume_session`] on every successful open (including
    /// the `AlreadyActive` path, where no config mutation happens) so the
    /// broker's **retained** snapshot for this session is refreshed. Without
    /// this, a session whose retained `llm_availability` was stamped `Loading`
    /// during the startup race keeps serving that stale value forever: the
    /// availability fan-out only re-publishes sessions that were active at the
    /// moment of a *transition*, so a session opened after the transition
    /// never gets corrected and the Desktop shows a permanent "syncing LLM
    /// config" banner. The chunk relay stamps the authoritative
    /// `reg.current()` value on every `SessionConfigChanged` publish, so this
    /// single call carries the true current availability.
    ///
    /// A no-op when the session is not active or its conversation has not
    /// been attached (logged at debug level — the two cases are
    /// indistinguishable from the caller's side).
    pub fn republish_session_config(&self, session_id: &str) {
        match self
            .sessions
            .get(session_id)
            .and_then(|handle| handle.conversation.as_ref())
        {
            Some(conv) => conv.notify_config_change(),
            None => tracing::debug!(
                session_id = %session_id,
                "republish_session_config: no live conversation (session not active \
                 or conversation not attached), skipping"
            ),
        }
    }

    /// Store the latest session info determined during the startup scan.
    ///
    /// Called from `session_init` after the background session scan completes.
    /// `title` may be `None` if the session has no title yet.
    pub fn set_latest_session(&self, session_id: String, title: Option<String>) {
        *self.latest_session.write().unwrap() = Some((session_id, title));
    }

    /// Get the latest session ID and title (by `last_active_at` descending).
    ///
    /// Returns `None` if the startup scan has not completed yet or if no
    /// sessions exist on disk.
    pub fn latest_session(&self) -> Option<(String, Option<String>)> {
        self.latest_session.read().unwrap().clone()
    }

    /// Look up a session handle by ID.
    pub fn get_session(&self, session_id: &str) -> Option<&SessionHandle> {
        self.sessions.get(session_id)
    }

    /// Get the number of active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    /// Get the session runtime snapshot for a specific session.
    ///
    /// ADR-039: persisted fields (model, provider, workspace_id,
    /// reasoning_effort, temperature) are no longer duplicated here — see
    /// `data/meta/{session_id}.json` and the `session_meta` MQTT channel.
    /// Returns `None` only if the session is not found.
    pub fn snapshot_session_state(
        &self,
        session_id: &str,
    ) -> Option<crate::agent::session_state::SessionRuntimeSnapshot> {
        self.sessions
            .get(session_id)
            .map(|handle| handle.snapshot())
    }

    /// ADR-074 §11.1: recompute + re-broadcast context usage for an IDLE
    /// session after a `context_window` change, so the UI total refreshes
    /// immediately — an idle session has no per-turn usage push to fall
    /// back on, and the number would otherwise sit on the stale window.
    ///
    /// Skips sessions with a running loop: their next per-turn push
    /// already carries the fresh window (and pushing here would duplicate
    /// it — §8-F). Reuses the "persisted tokens → snapshot
    /// `context_usage` → chunk broadcast" path from
    /// `build_initial_session_state`.
    ///
    /// Registered into `RuntimeSessionConfigService` as the late-bind
    /// `usage_recompute` callback (see `session_init.rs` Phase B). All
    /// reads are lock-free for the hot path; best-effort throughout.
    pub fn usage_recompute(&self, session_id: &str) {
        // A running loop pushes fresh usage on its own; skip to avoid
        // duplicate/stale pushes.
        if let Some(handle) = self.sessions.get(session_id)
            && handle.status().is_active()
        {
            return;
        }

        let Ok(configs) = self.session_configs.read() else {
            return;
        };
        let Some(conv) = configs.get(session_id) else {
            return;
        };
        // See `context::build_persisted_ctx_usage` for the
        // caps-resolution + cumulative-patch sequence this used to
        // spell out by hand (regression t-83afab47 refactor).
        let Some(ctx) = crate::agent::context::build_persisted_ctx_usage(&self.core, conv) else {
            return;
        };

        // (1) Write into the shared runtime snapshot (HTTP pull path).
        if let Some(ref snapshots) = self.config.session_snapshots
            && let Ok(map) = snapshots.read()
            && let Some(snap) = map.get(session_id)
            && let Ok(mut guard) = snap.write()
        {
            guard.context_usage = Some(serde_json::to_string(&ctx).unwrap_or_default());
        }

        // (2) Broadcast via the shared chunk channel (push path).
        if let Some(ref tx) = self.config.chunk_tx {
            let _ = tx.try_send(SessionChunkEvent {
                session_id: session_id.to_string(),
                event: crate::agent::loop_::ChunkEvent::ContextUsage(ctx),
            });
        }
    }

    /// Get the current status of all active sessions (ADR-014).
    ///
    /// Returns a map from session_id → SessionStatus for sessions currently
    /// running in memory. Sessions that exist only on disk (scanned by
    /// `list_sessions`) won't appear here.
    pub fn session_statuses(&self) -> Vec<(String, SessionStatus)> {
        self.sessions
            .iter()
            .map(|(id, handle)| (id.clone(), handle.status()))
            .collect()
    }

    /// Returns `true` when **any** session is currently in
    /// [`SessionStatus::is_active()`] state (Working / Thinking /
    /// Streaming / ToolExecuting / WaitingApproval / Paused).
    ///
    /// Used by the idle-watcher to suspend the auto-sleep deadline while
    /// a session is busy. The check is cheap (`O(n)` over active sessions,
    /// which is typically 1) and lock-free for the read path.
    pub fn any_session_active(&self) -> bool {
        self.sessions
            .values()
            .any(|handle| handle.status().is_active())
    }

    /// Access the shared core's manifest (ADR-012: for per-session model defaults).
    pub fn manifest(&self) -> &acowork_core::AgentManifest {
        self.core.manifest()
    }

    /// ADR-028: access the shared [`AgentCore`] so callers (e.g. the
    /// `list_sessions` HTTP handler) can update its agent-scoped token
    /// counters after each on-disk session scan. Returning the underlying
    /// `Arc` keeps the API surface minimal — callers may `merge_token_totals`
    /// or `agent_token_totals()` without taking an extra lock.
    pub fn core(&self) -> Arc<AgentCore> {
        self.core.clone()
    }

    /// ADR-021: Access the shared StreamingStateMap for incremental poll reads.
    ///
    /// Returns a reference to the `Arc<RwLock<HashMap<SessionId, StreamingLine>>>`
    /// so the CLI HTTP handler can call `read_messages_since()`.
    pub fn streaming_lines(&self) -> crate::conversation::StreamingStateMap {
        self.streaming_lines.clone()
    }

    /// ADR-047: Shared session config map for `SessionConfigService`.
    pub fn session_configs(&self) -> crate::usecases::SharedSessionConfigs {
        self.session_configs.clone()
    }

    /// ADR-047: Shared workspace resolver (for SessionConfigService).
    pub fn resolver(&self) -> Option<Arc<std::sync::RwLock<WorkspaceResolver>>> {
        self.resolver.clone()
    }

    /// ADR-022: Per-session committed line count — updated by writer thread
    /// after each disk write. Returns 0 if the session has no conversation
    /// (no writer thread, e.g. ephemeral test sessions).
    ///
    /// Use `read_messages_since`'s fallback (file scan) when this returns 0.
    pub fn committed_lines_for(&self, session_id: &str) -> usize {
        self.session_committed_lines
            .get(session_id)
            .map(|arc| arc.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// ADR-025: Get the per-session delivery cursor.
    ///
    /// If no cursor exists for this session (first incremental poll after
    /// session creation/resume), returns a cursor initialized to
    /// `{committed_lines, 0}` — meaning all existing complete lines are
    /// considered "already delivered" (they were fetched via the initial
    /// full-load request, not via incremental polling).
    pub fn get_delivery_cursor(&self, session_id: &str) -> crate::conversation::DeliveryCursor {
        {
            let cursors = self.session_delivery_cursors.read().unwrap();
            if let Some(&c) = cursors.get(session_id) {
                return c;
            }
        }
        // Initialize: all existing committed lines are "delivered".
        let initial = crate::conversation::DeliveryCursor {
            line_number: self.committed_lines_for(session_id),
            char_offset: 0,
        };
        let mut cursors = self.session_delivery_cursors.write().unwrap();
        cursors.entry(session_id.to_string()).or_insert(initial);
        initial
    }

    /// ADR-025: Advance the delivery cursor after a successful incremental poll.
    pub fn advance_delivery_cursor(
        &self,
        session_id: &str,
        line_number: usize,
        char_offset: usize,
    ) {
        let mut cursors = self.session_delivery_cursors.write().unwrap();
        cursors.insert(
            session_id.to_string(),
            crate::conversation::DeliveryCursor {
                line_number,
                char_offset,
            },
        );
    }

    /// ADR-025: Reset the delivery cursor to `total_lines`.
    ///
    /// Called after a non-incremental (full-load) request — all existing
    /// lines have been delivered via pagination, so the cursor jumps to
    /// the current end of the file.
    pub fn reset_delivery_cursor(&self, session_id: &str, total_lines: usize) {
        let mut cursors = self.session_delivery_cursors.write().unwrap();
        cursors.insert(
            session_id.to_string(),
            crate::conversation::DeliveryCursor {
                line_number: total_lines,
                char_offset: 0,
            },
        );
    }

    /// ADR-022: Create a fresh `committed_lines` Arc for a new session's
    /// writer thread. The Arc is cloned and stored in
    /// `session_committed_lines` by `create_session_with_id_and_conversation`.
    pub fn new_committed_lines() -> Arc<std::sync::atomic::AtomicUsize> {
        Arc::new(std::sync::atomic::AtomicUsize::new(0))
    }

    /// Get the name of the first available provider from the global cache.
    /// Used for budget queries in the Gateway loop and ConfigSnapshot.
    /// Returns an empty string if no providers are configured.
    pub fn provider_name(&self) -> String {
        let list = self.core.global_provider_list.read().unwrap();
        list.first().map(|p| p.id.clone()).unwrap_or_default()
    }

    /// Per-session model is owned by SessionState, not SessionManager.
    ///
    /// Returns the model from the most recently active session's snapshot.
    /// Falls back to the first model in `global_provider_list` (which mirrors
    /// the startup model selection in `agent_init.rs`). Returns `None` only
    /// when no provider is configured at all.
    pub fn current_model_name(&self) -> Option<String> {
        // 1. Try the most recently active session that has a model set.
        let from_session = self
            .sessions
            .values()
            .filter_map(|handle| {
                let snap = handle.snapshot.read().ok()?;
                let model = snap.model.clone()?;
                let ts = *handle.last_active_at.lock().ok()?;
                Some((ts, model))
            })
            .max_by_key(|(ts, _)| *ts)
            .map(|(_, model)| model);

        if from_session.is_some() {
            return from_session;
        }

        // 2. Fall back to the first model from the provider list (startup default).
        let list = self.core.global_provider_list.read().unwrap();
        list.iter()
            .flat_map(|p| p.models.iter())
            .next()
            .map(|m| m.id.clone())
    }

    /// Returns the (model, provider) pair from the most recently active session.
    ///
    /// Unlike [`current_model_name`] which only returns the model, this method
    /// atomically retrieves both model and provider from the **same** session
    /// snapshot, preventing cross-contamination when different providers
    /// expose identically-named models (e.g. "gpt-4" in both OpenAI and Azure).
    ///
    /// Resolution order (each only consulted if the previous returned None):
    ///   1. The most recently active session's
    ///      [`crate::conversation::ConversationSession`] meta. `apply_config`
    ///      updates the conv **synchronously** (memory + disk + MQTT) on
    ///      every model/provider switch — through either
    ///      `SessionConfigService::apply_config` (the production path in
    ///      `gateway_loop`) or `route_model_switch` (the slot-fallback
    ///      path). Conv is the canonical source of truth and is always
    ///      current immediately after a switch.
    ///   2. The most recently active session's [`SessionRuntimeSnapshot`]
    ///      mirror — refreshed by `AgentLoop::emit_session_state`. This
    ///      is best-effort and lags by one emit cycle (a status
    ///      transition boundary), so a switch that has only just
    ///      persisted to conv may not yet appear in the snapshot. Kept
    ///      as a defensive second-level lookup so old code paths and
    ///      tests that don't populate `session_configs` still resolve.
    ///   3. The first `(provider, model)` from `global_provider_list`.
    ///
    /// Returns `(None, None)` only when no provider is configured at all.
    pub fn current_model_and_provider(&self) -> (Option<String>, Option<String>) {
        // Level 1: ConversationSession meta (always up-to-date — the
        // canonical source of truth, mutated synchronously by
        // `apply_config` on every config change including model and
        // provider switches via `SessionConfigService::apply_config`).
        //
        // This MUST come before the snapshot lookup: the snapshot mirror
        // is only refreshed by `emit_session_state` (one AgentLoop turn
        // later), so a brand-new `create_session` called immediately
        // after a model switch would otherwise see the snapshot's stale
        // pre-switch value and a new session would inherit the wrong
        // model. With conv first, the switched value is observed
        // synchronously.
        //
        // The lifetime is constrained to this block so the
        // `RwLockReadGuard` on `session_configs` doesn't conflict with
        // the immutable borrow of `self.sessions`.
        let from_conv = {
            let configs_guard = self.session_configs.read().ok();
            match configs_guard.as_deref() {
                Some(configs) => self
                    .sessions
                    .values()
                    .filter_map(|handle| {
                        let conv = configs.get(&handle.session_id)?;
                        let model = conv.model()?;
                        let provider = conv.provider()?;
                        let ts = *handle.last_active_at.lock().ok()?;
                        Some((ts, model, provider))
                    })
                    .max_by_key(|(ts, _, _)| *ts)
                    .map(|(_, m, p)| (m, p)),
                None => None,
            }
        };
        if let Some((model, provider)) = from_conv {
            return (Some(model), Some(provider));
        }

        // Level 2: snapshot mirror (best-effort — read latency is lower
        // when it IS current, but it lags behind conv by one emit
        // cycle). Kept as a defensive fallback for code paths and
        // tests that build sessions without going through
        // `SessionConfigService`.
        if let Some((model, provider)) = self
            .sessions
            .values()
            .filter_map(|handle| {
                let snap = handle.snapshot.read().ok()?;
                let model = snap.model.clone()?;
                let provider = snap.provider.clone()?;
                let ts = *handle.last_active_at.lock().ok()?;
                Some((ts, model, provider))
            })
            .max_by_key(|(ts, _, _)| *ts)
            .map(|(_, model, provider)| (model, provider))
        {
            return (Some(model), Some(provider));
        }

        // Level 3: global provider list.
        let list = self.core.global_provider_list.read().unwrap();
        let provider = list.first().map(|p| p.id.clone());
        let model = list
            .first()
            .and_then(|p| p.models.first())
            .map(|m| m.id.clone());
        (model, provider)
    }

    /// Resolve the account_id the next newly-created session should inherit.
    ///
    /// Mirrors [`current_model_and_provider`]'s two-level resolution for
    /// the multi-account (`account_id`) counterpart so a brand-new session
    /// inherits the most recently active session's account pick — same as
    /// it already inherits `model` and `provider`.
    ///   1. [`ConversationSession`] meta via the `session_configs` map
    ///      (canonical, written synchronously by `apply_config`).
    ///   2. The per-session [`SessionRuntimeSnapshot`] mirror —
    ///      refreshed synchronously by `route_model_switch`, lags one
    ///      emit cycle otherwise. Used as the fallback when
    ///      `session_configs` is `None` (tests, single-session mode).
    ///
    /// Returns `None` (no inheritance) when no other session has an
    /// explicit account pick — the runtime's LLM path already falls back
    /// to the provider's first account via the vault resolver in that
    /// case. Inventing a default here would silently overrule the
    /// operator's "no preference" choice.
    pub fn current_account_id(&self) -> Option<String> {
        // Level 1: ConversationSession meta.
        let from_conv = {
            let configs_guard = self.session_configs.read().ok();
            match configs_guard.as_deref() {
                Some(configs) => self
                    .sessions
                    .values()
                    .filter_map(|handle| {
                        let conv = configs.get(&handle.session_id)?;
                        let account = conv.account_id();
                        let ts = *handle.last_active_at.lock().ok()?;
                        Some((ts, account))
                    })
                    .max_by_key(|(ts, _)| *ts)
                    .and_then(|(_, a)| a),
                None => None,
            }
        };
        if from_conv.is_some() {
            return from_conv;
        }
        // Level 2: SessionRuntimeSnapshot mirror.
        self.sessions
            .values()
            .filter_map(|handle| {
                let snap = handle.snapshot.read().ok()?;
                let account = snap.account_id.clone();
                let ts = *handle.last_active_at.lock().ok()?;
                Some((ts, account))
            })
            .max_by_key(|(ts, _)| *ts)
            .and_then(|(_, a)| a)
    }

    /// Reap completed sessions (remove handles for tasks that have finished).
    ///
    /// Call this periodically to avoid memory leaks from accumulated
    /// JoinHandle values for completed sessions.
    pub fn reap_finished(&mut self) {
        let finished: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, h)| h.join_handle.is_finished())
            .map(|(id, _)| id.clone())
            .collect();

        for id in finished {
            tracing::debug!(session_id = %id, "Reaping finished session handle");
            self.sessions.remove(&id);
            if let Some(ref snapshots) = self.config.session_snapshots {
                snapshots.write().unwrap().remove(&id);
            }
            self.session_configs.write().unwrap().remove(&id);
            self.session_committed_lines.remove(&id);
            self.session_delivery_cursors.write().unwrap().remove(&id);
        }
    }

    /// Extract the target session ID from request params.
    ///
    /// Every message MUST carry an explicit `session_id` — the backend is
    /// stateless with respect to "which session is current".  Returns an
    /// error when `session_id` is missing or empty so the caller can
    /// reject the message cleanly.
    pub fn require_session_id(params: &serde_json::Value) -> Result<String> {
        params
            .get("session_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                RuntimeError::Config(
                    "Missing or empty session_id parameter — every message must carry a session_id"
                        .to_string(),
                )
            })
    }

    /// Evict idle sessions from memory.
    ///
    /// A session is evicted when ALL of the following conditions are met:
    /// 1. Its status is `Idle` (not Streaming/WaitingApproval/Paused)
    /// 2. It has been idle for longer than `idle_timeout`
    ///
    /// Eviction destroys the in-memory SessionTask but leaves the JSONL
    /// file on disk. The session can be re-activated later via lazy
    /// resume: the frontend must explicitly call `POST
    /// /sessions/{sid}/open` (ADR-038, ADR-076 §决策 4), which routes
    /// through `http::session_control::post_open_session` →
    /// `SessionManager::resume_session`.
    pub async fn evict_idle_sessions(&mut self, idle_timeout: std::time::Duration) {
        let mut to_evict = Vec::new();

        for (session_id, handle) in &self.sessions {
            if handle.status() != SessionStatus::Idle {
                continue;
            }
            let elapsed = handle.last_active_at().elapsed();
            if elapsed >= idle_timeout {
                to_evict.push(session_id.clone());
            }
        }

        if to_evict.is_empty() {
            return;
        }

        for session_id in &to_evict {
            if let Some(handle) = self.sessions.remove(session_id) {
                let _ = handle.inbound_tx.send(SessionMessage::Close).await;
                if let Some(ref snapshots) = self.config.session_snapshots {
                    snapshots.write().unwrap().remove(session_id);
                }
                self.session_configs.write().unwrap().remove(session_id);
                self.urgent_stops.remove(session_id);
                self.session_committed_lines.remove(session_id);
                self.session_delivery_cursors
                    .write()
                    .unwrap()
                    .remove(session_id);
                tracing::info!(session_id = %session_id, "Evicted idle session from memory (idle > {:?})", idle_timeout);
            }
        }
        tracing::info!(evicted = to_evict.len(), "Idle session eviction complete");
    }

    // ── per-session workspace management ─────────────────────────────────

    /// Get the agent home path (derived from core config).
    pub fn agent_home(&self) -> &str {
        &self.core.config().work_dir
    }

    /// Set the current workspace for a specific session.
    ///
    /// Synchronously updates the session's workspace ID and resolved work_dir
    /// on the shared [`SessionCore`] Arc — no channel delay. Also persists the
    /// workspace_id to JSONL via async message.
    ///
    /// When `resolver` is available, resolves the workspace_id to a filesystem
    /// path and writes it to `current_work_dir` synchronously.
    pub fn set_session_workspace(&mut self, session_id: &str, workspace_id: &str) {
        // Remove from pending if the workspace is now active
        self.pending_workspaces.remove(session_id);
        tracing::info!(
            session_id = %session_id,
            workspace_id = %workspace_id,
            "SessionManager: session workspace updated (synchronous)"
        );

        if let Some(handle) = self.sessions.get(session_id) {
            // Write workspace_id synchronously — emit_session_state and
            // list_sessions will see the new value immediately.
            *handle.workspace_id.write().unwrap() = workspace_id.to_string();

            // Resolve and write current_work_dir synchronously
            if let Some(ref resolver) = self.resolver {
                let guard = resolver.read().unwrap();
                let resolved_path = if workspace_id == "__agent_home__" {
                    guard.agent_home().to_string()
                } else {
                    guard
                        .find_by_id(workspace_id)
                        .map(|d| d.path.clone())
                        .unwrap_or_else(|| guard.agent_home().to_string())
                };
                *handle.current_work_dir.write().unwrap() = Some(resolved_path);
            }

            // ADR-047: persist workspace_id to meta.json + notify MQTT
            // synchronously via apply_config(). No longer goes through
            // the serial inference queue.
            if let Some(ref conv) = handle.conversation {
                let delta = crate::agent::session_config::SessionConfigDelta {
                    workspace_id: Some(workspace_id.to_string()),
                    ..Default::default()
                };
                conv.apply_config(&delta);
            }
        }
    }

    /// Set the session workspace.
    ///
    /// Convenience alias for callers that already hold a resolver guard.
    /// Delegates to [`set_session_workspace`] which handles resolver
    /// resolution internally.
    pub fn set_session_workspace_with_resolver(&mut self, session_id: &str, workspace_id: &str) {
        self.set_session_workspace(session_id, workspace_id);
    }

    /// ADR-034 §8 Phase 2-3: consolidated workspace switch entry point.
    ///
    /// Single source of truth — every workspace switch (gRPC era's
    /// `workspace_switch`, MQTT's `WorkspaceSwitch` command, future
    /// internal callers) MUST go through this method instead of calling
    /// `set_session_workspace` / `update_session_workspace_context`
    /// directly.
    ///
    /// Combines 4 steps from the legacy gRPC-era `route_workspace_switch`
    /// (§6 P0-C / P0-D fix):
    ///   1. Validate `workspace_id` against `allowed_dirs` (resolver)
    ///   2. If invalid: register as `pending_workspace` + fall back to
    ///      `"__agent_home__"` (ADR-034 §4.3 step 4 — re-addable later)
    ///   3. Synchronously set the session's workspace_id + current_work_dir
    ///      via `set_session_workspace`
    ///   4. Push the per-session workspace context + prompt file via
    ///      `update_session_workspace_context`
    ///
    /// Original gRPC-era implementation lives in `cli.rs` (deprecated,
    /// to be removed in Phase 4).
    pub fn route_workspace_switch(&mut self, session_id: &str, workspace_id: &str) {
        // Step 1: validate against the resolver's allowed_dirs.
        let is_valid = if workspace_id == "__agent_home__" {
            true
        } else {
            match self.resolver.as_ref() {
                Some(resolver) => {
                    let guard = resolver.read().unwrap();
                    guard.find_by_id(workspace_id).is_some()
                }
                None => {
                    tracing::warn!(
                        session_id = %session_id,
                        workspace_id = %workspace_id,
                        "route_workspace_switch: resolver not set — accepting workspace_id without validation"
                    );
                    true
                }
            }
        };

        let effective_workspace_id = if is_valid {
            workspace_id.to_string()
        } else {
            // Step 2: invalid → register as pending + fallback to agent home.
            tracing::warn!(
                session_id = %session_id,
                workspace_id = %workspace_id,
                "route_workspace_switch: workspace_id not in allowed_dirs — registered as pending, fallback to __agent_home__"
            );
            self.add_pending_workspace(session_id, workspace_id);
            "__agent_home__".to_string()
        };

        // Step 3: synchronously set workspace_id + current_work_dir + JSONL persist.
        self.set_session_workspace(session_id, &effective_workspace_id);

        // Step 3.5: propagate the switch to the per-agent default so NEWLY
        // CREATED sessions inherit the user's last selected workspace.
        //
        // Regression fix (ADR-040 migration): before ADR-040 the Gateway
        // owned `agent_workspaces.json` and wrote `is_current`/`last_active`
        // on every workspace switch; the runtime read that flag at startup
        // to seed `default_workspace_id`. ADR-040 moved persistence into
        // the runtime but the switch path only updated the session — the
        // flag was never written again, so every new session fell back to
        // `__agent_home__`. Sessions (their persisted meta) are the source
        // of truth now: the default follows the switch directly.
        self.set_default_workspace_id(&effective_workspace_id);

        // Step 4: push per-session workspace context + prompt file to the SessionTask.
        self.update_session_workspace_context(session_id);

        tracing::info!(
            session_id = %session_id,
            requested = %workspace_id,
            effective = %effective_workspace_id,
            "route_workspace_switch: complete"
        );
    }

    /// Get the current workspace ID for a session.
    /// Returns `"__agent_home__"` if the session has no explicit workspace set
    /// or the session is not found.
    pub fn session_workspace_id(&self, session_id: &str) -> String {
        self.sessions
            .get(session_id)
            .map(|h| h.workspace_id.read().unwrap().clone())
            .unwrap_or_else(|| "__agent_home__".to_string())
    }

    /// Format and send workspace context to a specific session only.
    /// Also reads and sends workspace prompt file content (CLAUDE.md / AGENTS.md).
    ///
    /// The shared `WorkspaceResolver` (set via `set_resolver()`) is the
    /// single source of truth; this method acquires the read lock internally
    /// so callers don't need to manage it.
    pub fn update_session_workspace_context(&self, session_id: &str) {
        let resolver = self
            .resolver
            .as_ref()
            .expect("set_resolver must be called before any workspace context update");
        let resolver_guard = resolver.read().unwrap();
        let ws_id = self.session_workspace_id(session_id);
        let context_text = format_workspace_context_for_session(&resolver_guard, &ws_id);
        let prompt_file_content = resolver_guard.read_prompt_file(&ws_id);
        if let Some(handle) = self.sessions.get(session_id) {
            let has_prompt_file = prompt_file_content.is_some();
            let _ = handle.send(SessionMessage::UpdateWorkspaceContext { context_text });
            let _ = handle.send(SessionMessage::SetWorkspacePromptFile {
                content: prompt_file_content,
            });
            tracing::info!(
                session_id = %session_id,
                workspace_id = %ws_id,
                has_prompt_file,
                "SessionManager: sent per-session workspace context and prompt file"
            );
        } else {
            tracing::warn!(
                session_id = %session_id,
                "SessionManager: cannot update workspace context — session not found"
            );
        }
    }

    /// Set the default workspace ID for new sessions.
    /// When set to a workspace ID other than "__agent_home__", newly created
    /// sessions will use this workspace instead of agent home.
    pub fn set_default_workspace_id(&mut self, workspace_id: &str) {
        self.default_workspace_id = workspace_id.to_string();
        tracing::info!(
            default_workspace_id = %workspace_id,
            "SessionManager: default workspace updated for new sessions"
        );
    }

    /// Reconcile deleted workspaces: for all sessions whose selected workspace
    /// is no longer in the resolver's allowed list, move to pending and fallback
    /// to agent home.
    pub fn reconcile_deleted_workspaces(&mut self, resolver: &WorkspaceResolver) {
        let mut changes: Vec<(String, String)> = Vec::new();
        // Collect sessions whose workspace was deleted
        for (sid, handle) in &self.sessions {
            let ws_id = handle.workspace_id.read().unwrap().clone();
            if ws_id == "__agent_home__" {
                continue;
            }
            if resolver.find_by_id(&ws_id).is_none() {
                changes.push((sid.clone(), ws_id));
            }
        }
        for (sid, old_ws_id) in changes {
            self.pending_workspaces
                .insert(sid.clone(), old_ws_id.clone());
            if let Some(handle) = self.sessions.get(&sid) {
                *handle.workspace_id.write().unwrap() = "__agent_home__".to_string();
                *handle.current_work_dir.write().unwrap() = Some(resolver.agent_home().to_string());
                // ADR-047: persist the fallback to meta.json synchronously
                // via apply_config() so cold restarts don't re-read the
                // deleted workspace_id from metadata.
                if let Some(ref conv) = handle.conversation {
                    let delta = crate::agent::session_config::SessionConfigDelta {
                        workspace_id: Some("__agent_home__".to_string()),
                        ..Default::default()
                    };
                    conv.apply_config(&delta);
                }
            }
            tracing::info!(
                session_id = %sid,
                old_workspace_id = %old_ws_id,
                "SessionManager: workspace deleted, moved to pending + fallback to agent home"
            );
        }
    }

    /// Get the pending workspace ID for a session, if any.
    pub fn pending_workspace_id(&self, session_id: &str) -> Option<&str> {
        self.pending_workspaces.get(session_id).map(|s| s.as_str())
    }

    /// Register a pending workspace for a session (when the workspace
    /// doesn't exist in the resolver yet, but may be re-added later).
    pub fn add_pending_workspace(&mut self, session_id: &str, workspace_id: &str) {
        self.pending_workspaces
            .insert(session_id.to_string(), workspace_id.to_string());
    }

    /// Initialize debug mode at runtime (called when Gateway pushes EnableDebugMode).
    ///
    /// ADR-048: no TCP listener is started anymore. This wires up:
    /// 1. A [`crate::debug::DebugEventBus`] + per-session senders
    ///    (event path - consumed by the MQTT publisher).
    /// 2. The MQTT publisher itself, if an MQTT client is available.
    /// 3. The late-bind `RuntimeDebugService` for HTTP `/api/debug/*`.
    ///
    /// Then pushes the handles to all existing sessions via
    /// `SessionMessage::EnableDebugMode` so they can start emitting
    /// debug events immediately, without a restart.
    ///
    /// `debug_port` is retained for API stability but unused at runtime
    /// (ADR-048 removed the WS listener); it's still plumbed through the
    /// CLI / config / Gateway → Runtime handshake so existing operator
    /// scripts and dashboards keep working.
    pub async fn enable_debug_mode(
        &mut self,
        debug_port: u32,
        mqtt_client: Option<Arc<crate::mqtt::RuntimeMqttClient>>,
    ) {
        // Avoid double-init: if debug handles are already set, skip.
        if self.runtime_debug_handles.is_some() {
            tracing::warn!(
                debug_port = debug_port,
                "enable_debug_mode: debug handles already set, skipping"
            );
            return;
        }

        let event_bus = crate::debug::DebugEventBus::new();

        // ADR-048: if an MQTT client is available, subscribe a
        // publisher that forwards every TaggedEvent to the broker on
        // acowork/agents/<id>/debug/events/<event_type>.
        if let Some(mqtt_client) = mqtt_client {
            let event_rx = event_bus.subscribe();
            // ADR-073: debug event topics are per-instance.
            let instance_id = self.core.config.instance_id().to_string();
            let publisher =
                crate::mqtt::DebugEventMqttPublisher::new(instance_id, mqtt_client, event_rx);
            tokio::spawn(async move {
                publisher.run().await;
            });
            tracing::info!(
                "DebugEventMqttPublisher subscribed — debug events will be published to MQTT"
            );
        } else {
            tracing::warn!("DevMode started without MQTT client — debug events have no consumer");
        }

        let debug_event_tx = event_bus.sender_template();

        // Create debug controllers AND event senders for ALL existing
        // sessions and register them in the shared maps. New sessions
        // created while debug mode is active register their own pair at
        // creation time via `create_session_with_id_and_conversation`.
        //
        // Both halves are required by `RuntimeDebugService::get_session_state`
        // (`usecases/debug_service_impl.rs:61-78`): a missing controller
        // or a missing sender both surface as `SessionNotFound` → 404 on
        // every debug RPC. Without the sender registration below, the
        // initial session (created in Phase B before `enable_debug_mode`
        // runs) — the most common debug target — would fail every F5/F10
        // /getState/step with the exact 404 the user is seeing.
        {
            let session_ids: Vec<String> = self.sessions.keys().cloned().collect();
            let mut controllers = self.debug_controllers.write().await;
            let mut senders = self.debug_event_senders.write().await;
            for sid in session_ids {
                let debug_ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
                controllers.insert(sid.clone(), debug_ctrl);
                // Per-session sender shares the broadcast channel with
                // `runtime_debug_handles.debug_event_tx` (cheap clone)
                // but is tagged with this session_id so events emitted
                // by the AgentLoop land on the right receiver.
                let per_session_sender = debug_event_tx.for_session(sid);
                senders.insert(
                    per_session_sender.session_id().to_string(),
                    per_session_sender,
                );
            }
        }

        // Build the shared DebugHandles template from the first per-session
        // controller. The event_tx is shared across all sessions; notify handles
        // come from a per-session controller so the debug server's notify_one()
        // calls (which target per-session controllers) align with SessionTask
        // waiters. The debug_ctrl in this template is only a fallback —
        // push_debug_mode_to_existing_sessions and create_session both construct
        // per-session DebugHandles using each session's own controller.
        let template_handles = {
            let controllers = self.debug_controllers.read().await;
            if let Some(first_ctrl) = controllers.values().next() {
                let guard = first_ctrl.lock().await;
                DebugHandles {
                    debug_ctrl: first_ctrl.clone(),
                    debug_event_tx: debug_event_tx.clone(),
                    rewind_notify: guard.rewind_notify_handle(),
                    resume_notify: guard.resume_notify_handle(),
                    control_notify: guard.control_notify_handle(),
                }
            } else {
                // No sessions exist yet — create a minimal controller just for
                // its notify handles. Its iteration/phase state will never be read.
                let ctrl = Arc::new(tokio::sync::Mutex::new(DebugController::new()));
                let ctrl_for_lock = ctrl.clone();
                let (rw, rs, rc) = {
                    let guard = ctrl_for_lock.lock().await;
                    (
                        guard.rewind_notify_handle(),
                        guard.resume_notify_handle(),
                        guard.control_notify_handle(),
                    )
                };
                DebugHandles {
                    debug_ctrl: ctrl,
                    debug_event_tx: debug_event_tx.clone(),
                    rewind_notify: rw,
                    resume_notify: rs,
                    control_notify: rc,
                }
            }
        };
        self.runtime_debug_handles = Some(template_handles);

        // ADR-048: build the late-bind `RuntimeDebugService` now that
        // per-session controllers and event senders are populated. The
        // service reads from `debug_controllers` and `debug_event_senders`
        // on every call, so future session registrations still work —
        // we don't snapshot the maps.
        //
        // ADR-063: use the 4-arg `new_with_agent` constructor so the
        // service can fulfil `reload_prompts` requests (L2 reload). The
        // `agent_core` slot is an `Arc<RwLock<Option<Arc<AgentCore>>>>`
        // because `SessionManager::core` is the canonical `Arc<AgentCore>`
        // template that `Clone for AgentCore` clones from — every running
        // session's `AgentCore` share the same `Arc<RwLock<Option<String>>>`
        // inner fields, so writing here propagates to every live session.
        let agent_core_slot: Arc<std::sync::RwLock<Option<Arc<AgentCore>>>> =
            Arc::new(std::sync::RwLock::new(Some(self.core.clone())));
        let debug_svc = Arc::new(crate::usecases::RuntimeDebugService::new_with_agent(
            self.debug_controllers.clone(),
            self.debug_event_senders.clone(),
            agent_core_slot,
            self.package_dir.clone(),
        ));
        self.debug_service = Some(debug_svc);

        tracing::info!(
            "enable_debug_mode: event bus + MQTT publisher wired, handles stored for future sessions"
        );

        // Push debug handles to all existing sessions so their AgentCore
        // gets debug_ctrl/debug_event_tx injected. Without this, existing
        // sessions would continue without debug instrumentation while the
        // debug panel would show iteration:0 forever.
        self.push_debug_mode_to_existing_sessions().await;
    }

    /// Get the late-bind Debug service (built by `enable_debug_mode`).
    ///
    /// Returns `None` outside DevMode — the caller (typically the
    /// startup code that fills `debug_service_slot`) skips the slot
    /// fill in that case and the HTTP `/api/debug/*` routes return
    /// 503 with "Debug service not ready".
    pub fn debug_service(&self) -> Option<Arc<crate::usecases::RuntimeDebugService>> {
        self.debug_service.clone()
    }

    /// Tear down DevMode at runtime (ADR-048 follow-up).
    ///
    /// Symmetric counterpart to [`Self::enable_debug_mode`]. Called by
    /// the `POST /api/debug/disable` HTTP route so the user can exit
    /// DevMode from the Debug Panel without restarting the agent.
    ///
    /// What gets cleared:
    ///
    /// 1. `debug_service = None` — the next call to
    ///    `debug_service_slot` will see an empty slot and the HTTP
    ///    `/api/debug/*` routes will return 503 again.
    /// 2. `runtime_debug_handles = None` — new sessions created after
    ///    the disable call no longer inherit a DevMode observer.
    /// 3. `debug_controllers` + `debug_event_senders` maps cleared —
    ///    any leftover `DebugController` instances from the live map
    ///    lose their last references and drop. The
    ///    `RuntimeDebugService` (already gone via step 1) was the
    ///    only other reader, so this is safe.
    /// 4. `SessionMessage::DisableDebugMode` pushed to every live
    ///    session's inbound channel — `SessionTask` calls
    ///    `AgentCore::clear_debug_mode()` on the next iteration so
    ///    future `agent_loop` cycles stop emitting debug events.
    ///    Pending inbound messages ahead of `DisableDebugMode` still
    ///    run with the DevMode observer (the production migration is
    ///    strictly forward-only and does not need to retro-rewrite
    ///    history).
    /// 5. The pending-debug slot on every session is reset to `None`
    ///    so a `pending DebugHandles` left over from an in-flight
    ///    `EnableDebugMode` (delivered after the user clicked
    ///    "Exit Debug") cannot be picked up by the next agent_loop
    ///    iteration once DevMode has been torn down.
    ///
    /// Idempotent: if DevMode was never enabled, all four clears are
    /// no-ops and the call returns immediately. This matches the
    /// idempotency contract of [`Self::enable_debug_mode`].
    pub async fn disable_debug_mode(&mut self) {
        if self.runtime_debug_handles.is_none() && self.debug_service.is_none() {
            tracing::debug!(
                "disable_debug_mode: no-op (DevMode was never enabled on this Runtime)"
            );
            return;
        }

        tracing::info!("SessionManager::disable_debug_mode: tearing down DevMode");

        // (1) Drop the late-bind DebugService first — the HTTP slot
        // is read on every `/api/debug/*` request, so clearing this
        //     before we touch the underlying maps guarantees that no
        //     RPC can land mid-teardown.
        self.debug_service = None;

        // (4) Push DisableDebugMode to every session before we
        //     release the per-session controllers — the SessionTask
        //     handler in session_task.rs reads the observer pointer
        //     and calls `core.clear_debug_mode()`; we want that to
        //     happen while the controllers are still registered so
        //     any concurrent RPC that already grabbed a controller
        //     can finish without surprising the session.
        for (sid, session_handle) in &self.sessions {
            // Reset pending-debug-injection first so a stale handles
            // pointer cannot be applied after the toggle.
            {
                let mut pending = session_handle.pending_debug_handles.lock().await;
                *pending = None;
            }
            let msg = SessionMessage::DisableDebugMode;
            if session_handle.inbound_tx.send(msg).await.is_err() {
                tracing::warn!(
                    session_id = %sid,
                    "SessionManager: failed to push DisableDebugMode to session (channel closed)"
                );
            } else {
                tracing::info!(
                    session_id = %sid,
                    "SessionManager: pushed DisableDebugMode to existing session"
                );
            }
        }

        // (2) Drop the shared template — new sessions will read
        //     `None` and stay in production mode.
        self.runtime_debug_handles = None;

        // (3) Drop per-session controllers + senders. Their only
        //     remaining reader was the now-`None` `debug_service`,
        //     so dropping the maps is safe.
        {
            let mut controllers = self.debug_controllers.write().await;
            controllers.clear();
        }
        {
            let mut senders = self.debug_event_senders.write().await;
            senders.clear();
        }

        tracing::info!(
            "disable_debug_mode: DevMode torn down — debug service slot, per-session controllers/senders, and runtime handles cleared"
        );
    }

    /// Push EnableDebugMode to every existing session so they inject the
    /// debug handles into their AgentCore without a restart.
    ///
    /// Each session receives its own per-session `DebugController` (stored
    /// in `self.debug_controllers`) so that the AgentLoop's state updates
    /// are visible to the HTTP debug routes via `getState`. The notify
    /// handles (rewind/resume) also come from the per-session controller so
    /// that the debug server's `notify_one()` calls reach the correct waiter.
    async fn push_debug_mode_to_existing_sessions(&self) {
        let Some(ref handles) = self.runtime_debug_handles else {
            return;
        };
        let controllers = self.debug_controllers.read().await;
        for (sid, session_handle) in &self.sessions {
            // Use the per-session controller registered in debug_controllers,
            // NOT the global handles.debug_ctrl. The debug service reads
            // from debug_controllers for getState, so the AgentLoop must write
            // to the same instance.
            let per_session_ctrl = controllers
                .get(sid)
                .cloned()
                .unwrap_or_else(|| handles.debug_ctrl.clone());
            {
                // Trace immediately (the pointer is only valid before the
                // first await on the controller lock — capture it now so
                // it doesn't have to be carried across the lock().await
                // below, which would force the future to be !Send because
                // raw `*const ()` is not Send).
                let ctrl_ptr = Arc::as_ptr(&per_session_ctrl) as *const ();
                tracing::debug!(
                    session_id = %sid,
                    ctrl_ptr = ?ctrl_ptr,
                    found_in_map = controllers.contains_key(sid),
                    "push_debug_mode: per-session controller resolved"
                );
            }
            // Extract notify handles from the per-session controller.
            // The debug server calls ctrl.resume_notify.notify_one() on this
            // same controller instance, so SessionTask must wait on the same
            // Notify arcs.
            let (per_rewind, per_resume, per_control) = {
                let guard = per_session_ctrl.lock().await;
                (
                    guard.rewind_notify_handle(),
                    guard.resume_notify_handle(),
                    guard.control_notify_handle(),
                )
            };
            let per_session_handles = DebugHandles {
                debug_ctrl: per_session_ctrl,
                debug_event_tx: handles.debug_event_tx.for_session(sid.clone()),
                rewind_notify: per_rewind,
                resume_notify: per_resume,
                control_notify: per_control,
            };

            // Bypass path: write debug handles into pending_debug_handles so
            // that check_and_apply_pending_debug() inside execute_single_iteration
            // can pick them up EVEN when the SessionTask's message loop is blocked
            // inside agent_loop.run(). Without this, EnableDebugMode just sits in
            // the inbound channel queue and the AgentLoop never sees debug_ctrl.
            {
                let mut pending = session_handle.pending_debug_handles.lock().await;
                *pending = Some(per_session_handles.clone());
                tracing::debug!(
                    session_id = %sid,
                    "push_debug_mode: handles written to pending_debug_handles (bypass)"
                );
            }

            let msg = SessionMessage::EnableDebugMode(per_session_handles);
            if session_handle.inbound_tx.send(msg).await.is_err() {
                tracing::warn!(
                    session_id = %sid,
                    "SessionManager: failed to push EnableDebugMode to session (channel closed)"
                );
            } else {
                tracing::info!(
                    session_id = %sid,
                    "SessionManager: pushed EnableDebugMode to existing session"
                );
            }
        }
    }
}

/// Handle a SessionTask panic: promote the session to [`SessionStatus::Errored`]
/// and emit a terminal [`ChunkEvent::Error`] so the frontend exits the
/// "replying…" / "agent is running" UI immediately.
///
/// **Why a standalone function**: the panic-recovery path must be unit-testable
/// end-to-end (see `tests::panic_handler_*`) without spawning a real
/// SessionTask — the function only depends on the two downstream channels.
///
/// **Robustness contract** (must never panic itself — it runs inside a panic
/// handler context):
/// - `status_tx.send()` is best-effort: `Err` only when zero receivers remain.
/// - `chunk_tx.try_send()` is non-blocking and best-effort: a full or closed
///   channel logs a warning instead of propagating an error.
fn handle_session_task_panic(
    session_id: &str,
    panic_err: Box<dyn std::any::Any + Send>,
    status_tx: &tokio::sync::watch::Sender<SessionStatus>,
    chunk_tx: Option<&tokio::sync::mpsc::Sender<SessionChunkEvent>>,
) {
    let msg = panic_err
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| panic_err.downcast_ref::<String>().map(|s| s.as_str()))
        .unwrap_or("<non-string panic payload>");
    tracing::error!(
        session_id = %session_id,
        panic.payload = %msg,
        "SessionTask panicked — promoting session to Errored state and emitting terminal error event"
    );

    // (1) Push Errored status — fires watch subscribers (HTTP /state,
    //     state-relay to chunk channel, MQTT gateway bridge).
    let errored_status = SessionStatus::Errored {
        reason: "session_task_panicked".to_string(),
        message: format!("Session task panicked: {msg}"),
        last_iteration: None, // panic may happen pre-iteration; we don't know
        recoverable: false,
    };
    let _ = status_tx.send(errored_status);

    // (2) Push a terminal Error chunk so the frontend can clear any
    //     "replying…" / "agent is running" UI immediately.
    if let Some(tx) = chunk_tx {
        let user_message = "The agent's session task terminated unexpectedly. \
             Please start a new session to continue.";
        let chunk = SessionChunkEvent {
            session_id: session_id.to_string(),
            event: crate::agent::loop_::ChunkEvent::Error {
                user_message: user_message.to_string(),
                detail: format!("SessionTask panicked: {msg}"),
                error_type: "session_task_panicked".to_string(),
                message_id: format!("panic-{session_id}"),
            },
        };
        if let Err(e) = tx.try_send(chunk) {
            tracing::warn!(
                session_id = %session_id,
                error = %e,
                "Failed to push panic-recovery chunk event (channel full or closed)"
            );
        }
    }
}

/// Format a `UserProfile` into an identity context text block for the LLM system prompt.
///
/// Produces a human-readable summary like:
///   - Display Name: Alice
///   - Language: zh-CN
///   - Timezone: Asia/Shanghai
///   - City: Shanghai
///   - Country: CN
///   - Occupation: Software Engineer
///   - Communication Style: concise
///
/// `session_language_override` (per-agent `AgentConfig.session_language`)
/// wins over `profile.language` when set — this is how the Agent Setup
/// panel lets a user pin one agent's session language independently
/// from the global `UserProfile.language`. `None` = fall through to the
/// global setting (existing behaviour).
pub(crate) fn format_user_profile_context(
    profile: &acowork_core::protocol::UserProfile,
    session_language_override: Option<&str>,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("- Display Name: {}", profile.display_name));
    lines.push(format!(
        "- Language: {}",
        session_language_override.unwrap_or(&profile.language)
    ));
    lines.push(format!("- Timezone: {}", profile.timezone));
    if let Some(ref city) = profile.city {
        lines.push(format!("- City: {}", city));
    }
    if let Some(ref country) = profile.country {
        lines.push(format!("- Country: {}", country));
    }
    if let Some(ref occupation) = profile.occupation {
        lines.push(format!("- Occupation: {}", occupation));
    }
    if let Some(ref style) = profile.communication_style {
        lines.push(format!("- Communication Style: {}", style));
    }
    for (key, value) in &profile.custom {
        lines.push(format!("- {}: {}", key, value));
    }
    lines.join("\n")
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn make_tool_spec(name: &str) -> (String, serde_json::Value) {
        let schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("Tool {}", name),
                "parameters": { "type": "object", "properties": {} }
            }
        });
        (name.to_string(), schema)
    }

    // ── RuntimeConfigOverrides ─────────────────────────────────────────

    #[test]
    fn test_overrides_is_empty() {
        let ov = RuntimeConfigOverrides::default();
        assert!(ov.is_empty());
    }

    #[test]
    fn test_overrides_merge() {
        let mut ov = RuntimeConfigOverrides::default();
        ov.merge(&RuntimeConfigOverrides {
            max_output_tokens: Some(100),
            ..Default::default()
        });
        assert!(!ov.is_empty());
        assert_eq!(ov.max_output_tokens, Some(100));

        // Re-merge with Some replaces
        ov.merge(&RuntimeConfigOverrides {
            max_output_tokens: Some(200),
            ..Default::default()
        });
        assert_eq!(ov.max_output_tokens, Some(200));

        // None preserves
        ov.merge(&RuntimeConfigOverrides::default());
        assert_eq!(ov.max_output_tokens, Some(200));
    }

    /// Verifies the diff-signal used by `apply_runtime_config_override`
    /// Step 0.5 to detect an actual `session_language` change (vs. an
    /// unrelated field push that happens to carry the same value).
    /// Step 0.5 captures `self.runtime_overrides.session_language` BEFORE
    /// `merge` runs, because `merge` already overwrites it with the new
    /// value and the diff would be lost.
    #[test]
    fn test_session_language_diff_signal_pre_merge() {
        // Initial push: None → Some("en")
        let mut ov = RuntimeConfigOverrides::default();
        let prev = ov.session_language.clone();
        ov.merge(&RuntimeConfigOverrides {
            session_language: Some("en".to_string()),
            ..Default::default()
        });
        assert_eq!(prev, None, "pre-merge cache should see None");
        assert_eq!(ov.session_language.as_deref(), Some("en"));

        // No-op push: Some("en") → Some("en") — diff signal says
        // "no change", so Step 0.5 must skip re-formatting.
        let prev = ov.session_language.clone();
        ov.merge(&RuntimeConfigOverrides {
            session_language: Some("en".to_string()),
            ..Default::default()
        });
        assert_eq!(
            prev.as_deref(),
            Some("en"),
            "pre-merge cache must preserve previous value for diff"
        );
        assert_eq!(prev, ov.session_language);

        // Real change: Some("en") → Some("ja") — diff signal fires.
        let prev = ov.session_language.clone();
        ov.merge(&RuntimeConfigOverrides {
            session_language: Some("ja".to_string()),
            ..Default::default()
        });
        assert_eq!(prev.as_deref(), Some("en"));
        assert_eq!(ov.session_language.as_deref(), Some("ja"));
        assert_ne!(prev, ov.session_language);
    }

    #[test]
    fn test_overrides_session_language_round_trip() {
        // Round-trip: AgentConfig.session_language -> RuntimeConfigOverrides
        // -> AgentConfig (via the override → config mapper) preserves
        // the value end to end. This is the boot-time + live-edit
        // pipeline that the Agent Setup panel feeds.
        let cfg = crate::agent_config::AgentConfig {
            session_language: Some("zh-CN".to_string()),
            ..Default::default()
        };
        let ov = RuntimeConfigOverrides::from(&cfg);
        assert_eq!(ov.session_language.as_deref(), Some("zh-CN"));

        let mut back = crate::agent_config::AgentConfig::default();
        ov.apply_to(&mut back);
        assert_eq!(back.session_language.as_deref(), Some("zh-CN"));

        // Merge: Some replaces, None preserves.
        let mut ov2 = RuntimeConfigOverrides {
            session_language: Some("en".to_string()),
            ..Default::default()
        };
        ov2.merge(&ov); // ov.session_language = Some("zh-CN") wins
        assert_eq!(ov2.session_language.as_deref(), Some("zh-CN"));

        let mut ov3 = RuntimeConfigOverrides {
            session_language: Some("en".to_string()),
            ..Default::default()
        };
        ov3.merge(&RuntimeConfigOverrides::default()); // None preserves
        assert_eq!(ov3.session_language.as_deref(), Some("en"));
    }

    #[test]
    fn test_format_user_profile_session_language_override() {
        // The per-agent `session_language` override (AgentConfig.session_language)
        // must replace the global `UserProfile.language` line in the
        // identity context sent to the LLM. With `None` the existing
        // behaviour (use `profile.language`) is preserved.
        let profile = acowork_core::protocol::UserProfile {
            user_id: "test-user".to_string(),
            display_name: "Alice".to_string(),
            language: "zh-CN".to_string(),
            timezone: "Asia/Shanghai".to_string(),
            city: None,
            country: None,
            occupation: None,
            avatar: None,
            builtin_avatar: None,
            communication_style: None,
            custom: Default::default(),
            created_at: "2024-01-01T00:00:00Z".to_string(),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
            is_active: true,
        };

        let ctx_no_override = format_user_profile_context(&profile, None);
        assert!(
            ctx_no_override.contains("- Language: zh-CN"),
            "expected fallback to profile.language, got: {ctx_no_override}"
        );

        let ctx_with_override = format_user_profile_context(&profile, Some("en"));
        assert!(
            ctx_with_override.contains("- Language: en"),
            "expected override to win, got: {ctx_with_override}"
        );
        assert!(
            !ctx_with_override.contains("- Language: zh-CN"),
            "override should fully replace the global language line, got: {ctx_with_override}"
        );
    }

    // ── require_session_id ─────────────────────────────────────────────

    #[test]
    fn test_require_session_id_valid() {
        let params = serde_json::json!({ "session_id": "test-sid" });
        assert_eq!(
            SessionManager::require_session_id(&params).unwrap(),
            "test-sid"
        );
    }

    #[test]
    fn test_require_session_id_missing() {
        let params = serde_json::json!({});
        assert!(SessionManager::require_session_id(&params).is_err());
    }

    #[test]
    fn test_require_session_id_empty() {
        let params = serde_json::json!({ "session_id": "" });
        assert!(SessionManager::require_session_id(&params).is_err());
    }

    // ── DeliveryCursor storage tests (ADR-025) ──────────────────────
    //
    // These tests verify the cursor storage logic that SessionManager
    // uses internally (RwLock<HashMap<String, DeliveryCursor>>).  The
    // methods get/advance/reset are thin wrappers around HashMap ops,
    // but testing the lifecycle ensures correctness of the integration
    // with read_messages_since_cursor.

    use crate::conversation::DeliveryCursor;

    /// Simulates SessionManager's delivery cursor storage for testing.
    struct CursorStore {
        cursors: std::sync::RwLock<HashMap<String, DeliveryCursor>>,
        committed_lines: HashMap<String, Arc<std::sync::atomic::AtomicUsize>>,
    }

    impl CursorStore {
        fn new() -> Self {
            Self {
                cursors: std::sync::RwLock::new(HashMap::new()),
                committed_lines: HashMap::new(),
            }
        }

        /// Mirrors SessionManager::get_delivery_cursor
        fn get(&self, sid: &str) -> DeliveryCursor {
            if let Some(c) = self.cursors.read().unwrap().get(sid).cloned() {
                return c;
            }
            let cl = self
                .committed_lines
                .get(sid)
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0);
            let cursor = DeliveryCursor {
                line_number: cl,
                char_offset: 0,
            };
            self.cursors
                .write()
                .unwrap()
                .insert(sid.to_string(), cursor);
            cursor
        }

        fn set_committed_lines(&mut self, sid: &str, n: usize) {
            self.committed_lines.insert(
                sid.to_string(),
                Arc::new(std::sync::atomic::AtomicUsize::new(n)),
            );
        }

        fn advance(&mut self, sid: &str, line: usize, char_offset: usize) {
            let mut g = self.cursors.write().unwrap();
            g.insert(
                sid.to_string(),
                DeliveryCursor {
                    line_number: line,
                    char_offset,
                },
            );
        }

        fn reset(&mut self, sid: &str, total: usize) {
            self.cursors.write().unwrap().insert(
                sid.to_string(),
                DeliveryCursor {
                    line_number: total,
                    char_offset: 0,
                },
            );
        }

        fn remove(&mut self, sid: &str) {
            self.cursors.write().unwrap().remove(sid);
        }

        fn committed_lines_for(&self, sid: &str) -> usize {
            self.committed_lines
                .get(sid)
                .map(|a| a.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(0)
        }
    }

    #[test]
    fn test_cursor_get_initializes_to_committed_lines() {
        let mut store = CursorStore::new();
        store.set_committed_lines("s1", 42);

        // First get — should initialize to {42, 0}
        let c = store.get("s1");
        assert_eq!(c.line_number, 42);
        assert_eq!(c.char_offset, 0);

        // Second get — should return the stored value (not re-initialize)
        store.advance("s1", 50, 10);
        let c = store.get("s1");
        assert_eq!(c.line_number, 50);
        assert_eq!(c.char_offset, 10);
    }

    #[test]
    fn test_cursor_get_no_committed_lines_defaults_to_zero() {
        let store = CursorStore::new();
        // No committed_lines entry for "s2"
        let c = store.get("s2");
        assert_eq!(c.line_number, 0);
        assert_eq!(c.char_offset, 0);
    }

    #[test]
    fn test_cursor_advance_updates_value() {
        let mut store = CursorStore::new();

        store.advance("s1", 10, 5);
        let c = store.get("s1");
        assert_eq!(c.line_number, 10);
        assert_eq!(c.char_offset, 5);

        // Advance again — overwrites
        store.advance("s1", 15, 20);
        let c = store.get("s1");
        assert_eq!(c.line_number, 15);
        assert_eq!(c.char_offset, 20);
    }

    #[test]
    fn test_cursor_reset_sets_to_total_lines() {
        let mut store = CursorStore::new();

        // Advance to some position
        store.advance("s1", 5, 10);
        assert_eq!(store.get("s1").line_number, 5);

        // Full load resets to total_lines
        store.reset("s1", 100);
        let c = store.get("s1");
        assert_eq!(c.line_number, 100);
        assert_eq!(c.char_offset, 0);
    }

    #[test]
    fn test_cursor_remove_clears_entry() {
        let mut store = CursorStore::new();
        store.advance("s1", 10, 5);
        assert!(store.cursors.read().unwrap().contains_key("s1"));

        store.remove("s1");
        assert!(!store.cursors.read().unwrap().contains_key("s1"));

        // After removal, get re-initializes from committed_lines
        let c = store.get("s1");
        assert_eq!(c.line_number, 0); // no committed_lines → 0
    }

    #[test]
    fn test_cursor_isolation_between_sessions() {
        let mut store = CursorStore::new();

        store.advance("s1", 10, 5);
        store.advance("s2", 20, 15);

        assert_eq!(store.get("s1").line_number, 10);
        assert_eq!(store.get("s2").line_number, 20);

        // Advancing s1 doesn't affect s2
        store.advance("s1", 15, 20);
        assert_eq!(store.get("s2").line_number, 20);

        // Removing s1 doesn't affect s2
        store.remove("s1");
        assert_eq!(store.get("s2").line_number, 20);
    }

    /// E2E: Full delivery flow using CursorStore + read_messages_since_cursor.
    /// This is the closest to the real SessionManager + cli.rs path without
    /// requiring a full AgentCore.
    #[test]
    fn test_e2e_cursor_store_with_read_messages() {
        use crate::conversation::{ConversationEntry, read_messages_since_cursor};
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let entries = vec![ConversationEntry {
            id: "1".to_string(),
            ts: chrono::Utc::now().to_rfc3339(),
            role: "user".to_string(),
            content: "hello".to_string(),
            metadata: None,
            kind: None,
        }];
        let path = dir.path().join("conversations").join("e2e.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            for e in &entries {
                serde_json::to_writer(&mut f, e).unwrap();
                writeln!(f).unwrap();
            }
        }

        let mut store = CursorStore::new();
        store.set_committed_lines("e2e", 1);
        let sid = "e2e";
        let map: crate::conversation::StreamingStateMap =
            Arc::new(std::sync::RwLock::new(HashMap::new()));

        // Step 1: Full load — reset cursor to total_lines
        store.reset(sid, 1);
        let cursor = store.get(sid);
        assert_eq!(cursor.line_number, 1);

        // Step 2: Incremental poll — nothing new
        let r = read_messages_since_cursor(
            &path,
            cursor,
            50,
            &map,
            sid,
            store.committed_lines_for(sid),
        )
        .unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
        store.advance(sid, r.new_cursor.line_number, r.new_cursor.char_offset);

        // Step 3: Write new line (simulates flush + writer incrementing committed_lines)
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let e = ConversationEntry {
                id: "2".to_string(),
                ts: chrono::Utc::now().to_rfc3339(),
                role: "assistant".to_string(),
                content: "world".to_string(),
                metadata: None,
                kind: None,
            };
            serde_json::to_writer(&mut f, &e).unwrap();
            writeln!(f).unwrap();
        }
        // Simulate writer thread incrementing committed_lines
        store
            .committed_lines
            .get(sid)
            .unwrap()
            .store(2, std::sync::atomic::Ordering::Relaxed);

        // Step 4: Incremental poll — delivers new line
        let cursor = store.get(sid);
        let r = read_messages_since_cursor(
            &path,
            cursor,
            50,
            &map,
            sid,
            store.committed_lines_for(sid),
        )
        .unwrap();
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].content, "world");
        store.advance(sid, r.new_cursor.line_number, r.new_cursor.char_offset);

        // Step 5: Incremental poll — nothing new
        let cursor = store.get(sid);
        let r = read_messages_since_cursor(
            &path,
            cursor,
            50,
            &map,
            sid,
            store.committed_lines_for(sid),
        )
        .unwrap();
        assert_eq!(r.messages.len(), 0);
        assert!(!r.has_more);
    }

    // ── ADR-048: enable_debug_mode must populate BOTH halves ──────────
    //
    // Bug history: pre-fix, `enable_debug_mode` only iterated
    // `self.sessions` to insert per-session `DebugController`s into the
    // shared `debug_controllers` map. It did NOT touch
    // `debug_event_senders`. New sessions created after
    // `enable_debug_mode` ran (via `create_session_with_id_and_conversation`)
    // registered both halves, so they worked. But the initial session
    // — created in Phase B before `enable_debug_mode` runs in Phase C,
    // and the most common debug target — was left with a controller
    // but no sender. `RuntimeDebugService::get_session_state` requires
    // BOTH, so any debug RPC against the initial session (resume / step
    // / getState / ...) returned `SessionNotFound` → 404:
    //
    //   Unhandled Promise Rejection: Debug 404 Not Found:
    //     No debug session found for session_id: 20260819_231156_74e55e
    //
    // This test pins the fix: every existing session at the moment
    // `enable_debug_mode` runs must show up in BOTH maps, so the
    // paired lookup in `get_session_state` succeeds.
    //
    // Note: we don't run `create_session` here — it spawns a tokio
    // task and requires a fully-wired AgentCore / WorkDir. The bug is
    // purely about the map-population inside `enable_debug_mode`, which
    // only reads `self.sessions.keys()`. We hand-inject a dummy
    // SessionHandle keyed by the session_id we want to test, then
    // exercise `enable_debug_mode` and assert both maps agree.
    #[tokio::test]
    async fn enable_debug_mode_populates_event_senders_for_existing_sessions() {
        use crate::agent::session::session_handle::SessionHandle;
        use crate::agent::session::session_task::SessionMessage;
        use crate::agent::session_state::{SessionRuntimeSnapshot, SessionStatus};

        // 1. Build a bare SessionManager (no MQTT, no AgentCore details
        //    needed — `enable_debug_mode` is what we're testing).
        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.debug_existing"
            version = "1.0.0"
            name = "Test debug-existing"
            description = "Pin enable_debug_mode dual-map population"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let mut manager = SessionManager::new(core, SessionManagerConfig::default());

        // 2. Hand-inject a dummy session_handle into `self.sessions`
        //    to mimic the Phase B initial session. The fields are
        //    dummy because `enable_debug_mode` only reads
        //    `self.sessions.keys()` — it never touches the handle.
        let initial_session_id = "20260819_231156_74e55e".to_string();
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<SessionMessage>(1);
        let (agent_inbound_tx, _agent_inbound_rx) =
            tokio::sync::mpsc::channel::<crate::agent::inbound::InboundMessage>(1);
        let (_status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        manager.sessions.insert(
            initial_session_id.clone(),
            SessionHandle {
                session_id: initial_session_id.clone(),
                inbound_tx,
                agent_inbound_tx,
                join_handle: tokio::spawn(async {}),
                status_rx,
                last_active_at: std::sync::Mutex::new(std::time::Instant::now()),
                pending_debug_handles: Arc::new(tokio::sync::Mutex::new(None)),
                snapshot: Arc::new(std::sync::RwLock::new(SessionRuntimeSnapshot {
                    session_id: initial_session_id.clone(),
                    status: r#""idle""#.to_string(),
                    model: None,
                    provider: None,
                    account_id: None,
                    ratio: None,
                    todos_json: None,
                    context_usage: None,
                })),
                workspace_id: Arc::new(std::sync::RwLock::new("__agent_home__".to_string())),
                current_work_dir: Arc::new(std::sync::RwLock::new(None)),
                conversation: None,
            },
        );

        // 3. Pre-condition: both maps are empty before enable_debug_mode.
        assert!(
            manager.debug_controllers.read().await.is_empty(),
            "precondition: debug_controllers empty"
        );
        assert!(
            manager.debug_event_senders.read().await.is_empty(),
            "precondition: debug_event_senders empty"
        );

        // 4. Run enable_debug_mode (Phase C, no MQTT client — exercising
        //    the no-publisher branch is fine; the bug is in the
        //    map-population loop, independent of the publisher).
        manager.enable_debug_mode(19878, None).await;

        // 5. The fix: BOTH maps must contain the pre-existing session.
        let ctrl_map = manager.debug_controllers.read().await;
        let sender_map = manager.debug_event_senders.read().await;
        assert!(
            ctrl_map.contains_key(&initial_session_id),
            "controller must be registered for the initial session (was: {:?})",
            ctrl_map.keys().collect::<Vec<_>>()
        );
        let sender = sender_map
            .get(&initial_session_id)
            .unwrap_or_else(|| {
                panic!(
                    "BUG: event sender missing for initial session (was: {:?}) — would 404 on every F5/F10",
                    sender_map.keys().collect::<Vec<_>>()
                )
            });
        assert_eq!(
            sender.session_id(),
            initial_session_id,
            "per-session sender must be tagged with the originating session_id (not the template's empty id)"
        );
    }

    // ── panic handler (handle_session_task_panic) ─────────────────

    /// End-to-end: a SessionTask panic must (1) promote the watch status to
    /// `Errored` and (2) emit a terminal `ChunkEvent::Error` so the frontend
    /// exits the "replying…" state. This is the automated guard for the
    /// 2026-08-24 incident (session frozen in Thinking until runtime restart).
    #[test]
    fn panic_handler_pushes_errored_status_and_terminal_chunk() {
        let (status_tx, mut status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<SessionChunkEvent>(8);

        handle_session_task_panic(
            "sess_1",
            Box::new("end byte index 200 is not a char boundary; it is inside '原'".to_string()),
            &status_tx,
            Some(&chunk_tx),
        );

        // (1) watch 收到 Errored
        assert!(status_rx.has_changed().unwrap());
        let status = status_rx.borrow_and_update().clone();
        assert_eq!(
            status,
            SessionStatus::Errored {
                reason: "session_task_panicked".into(),
                message: "Session task panicked: end byte index 200 is not a char boundary; it is inside '原'"
                    .into(),
                last_iteration: None,
                recoverable: false,
            }
        );

        // (2) chunk 收到 terminal Error 事件
        let chunk = chunk_rx
            .try_recv()
            .expect("terminal error chunk must be emitted");
        assert_eq!(chunk.session_id, "sess_1");
        match chunk.event {
            crate::agent::loop_::ChunkEvent::Error {
                user_message,
                detail,
                error_type,
                message_id,
            } => {
                assert_eq!(error_type, "session_task_panicked");
                assert_eq!(message_id, "panic-sess_1");
                assert!(detail.contains("not a char boundary"));
                assert!(user_message.contains("new session"));
            }
            other => panic!("expected Error chunk, got {other:?}"),
        }
    }

    /// Robustness: the panic handler runs inside a panic-handler context and
    /// MUST NOT panic itself when downstream channels are closed/detached.
    #[test]
    fn panic_handler_survives_closed_channels() {
        // watch 无接收者
        let (status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        drop(status_rx);
        // chunk channel closed（前端已断开）
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<SessionChunkEvent>(8);
        drop(chunk_rx);
        handle_session_task_panic(
            "sess_2",
            Box::new("boom".to_string()),
            &status_tx,
            Some(&chunk_tx),
        );

        // chunk_tx = None 路径
        handle_session_task_panic("sess_3", Box::new("boom".to_string()), &status_tx, None);
    }

    /// Panic payload may be a `String`, a `&'static str`, or a non-string
    /// value (`panic!` with a number, custom struct, etc.) — all must be
    /// sanitized into a displayable message without panicking.
    #[test]
    fn panic_handler_non_string_payload_is_sanitized() {
        let (status_tx, mut status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        handle_session_task_panic("sess_4", Box::new(42u32), &status_tx, None);
        assert!(status_rx.has_changed().unwrap());
        let status = status_rx.borrow_and_update().clone();
        match status {
            SessionStatus::Errored { message, .. } => {
                assert!(
                    message.contains("<non-string panic payload>"),
                    "got: {message}"
                );
            }
            other => panic!("expected Errored, got {other:?}"),
        }
    }

    #[test]
    fn panic_handler_static_str_payload_is_extracted() {
        let (status_tx, mut status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        handle_session_task_panic("sess_5", Box::new("static str panic"), &status_tx, None);
        assert!(status_rx.has_changed().unwrap());
        let status = status_rx.borrow_and_update().clone();
        match status {
            SessionStatus::Errored { message, .. } => {
                assert!(message.contains("static str panic"), "got: {message}");
            }
            other => panic!("expected Errored, got {other:?}"),
        }
    }

    // ── New-session workspace inheritance (ADR-040 regression) ────────
    //
    // Bug history: before ADR-040 the Gateway wrote `is_current` /
    // `last_active` into `agent_workspaces.json` on every workspace
    // switch, and the runtime seeded `default_workspace_id` from that
    // flag at startup. ADR-040 moved workspace persistence into the
    // runtime but `route_workspace_switch` only updated the session —
    // the flag was never written again, so every new session fell back
    // to `__agent_home__` no matter what the user had selected last.
    //
    // Fix: the switch now propagates to `default_workspace_id` (the
    // per-agent in-memory default for new sessions). New sessions must
    // inherit the last selected workspace; switching back to agent home
    // must reset the default as well.
    #[tokio::test]
    async fn workspace_switch_sets_default_for_new_sessions() {
        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.ws_inherit"
            version = "1.0.0"
            name = "Test ws inherit"
            description = "Pin new-session workspace inheritance"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));

        let resolver = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new_for_test(
            vec![crate::tools::workspace_resolver::WorkspaceDir {
                id: "ws-1".to_string(),
                path: "/tmp/ws-1".to_string(),
                access: crate::tools::workspace_resolver::WorkspaceAccess::ReadWrite,
                last_active: false,
                prompt_file: None,
            }],
        )));
        let mut manager = SessionManager::new(core, SessionManagerConfig::default());
        manager.set_resolver(resolver);

        // Fresh manager: default is agent home.
        assert_eq!(manager.default_workspace_id, "__agent_home__");

        // User switches session S1 to ws-1 → the per-agent default follows.
        let s1 = manager.create_session().await.unwrap();
        manager.route_workspace_switch(&s1, "ws-1");
        assert_eq!(manager.default_workspace_id, "ws-1");

        // A brand-new session inherits ws-1 instead of agent home.
        let s2 = manager.create_session().await.unwrap();
        assert_eq!(manager.session_workspace_id(&s2), "ws-1");

        // Switching back to agent home resets the default for the next
        // new session too.
        manager.route_workspace_switch(&s1, "__agent_home__");
        assert_eq!(manager.default_workspace_id, "__agent_home__");
        let s3 = manager.create_session().await.unwrap();
        assert_eq!(manager.session_workspace_id(&s3), "__agent_home__");
    }

    // ── New-session model/provider inheritance (regression test) ──────
    //
    // Bug history: `route_model_switch` persisted the new model/provider to
    // the session meta via `conv.apply_config` and queued an
    // `apply_llm_effects` for the SessionTask, but `apply_llm_effects` only
    // updates `session.set_model` / `set_provider` (the in-memory
    // SessionState). It does NOT update the SessionRuntimeSnapshot. The
    // snapshot is refreshed by `AgentLoop::emit_session_state` on the next
    // status transition — far too late for the next `create_session`.
    //
    // Symptom: a user switches the current session to model A, then
    // creates a new session. The new session's `build_initial_session_state`
    // calls `current_model_and_provider()`, whose filter drops every
    // session whose snapshot.model is still None, falling back to
    // `global_provider_list.first()` (the default provider, e.g. minimax).
    //
    // Fix: `route_model_switch` mirrors the switched model/provider into
    // the snapshot synchronously. This test pins that contract end-to-end:
    // switch → snapshot reflects → next new session inherits.
    #[tokio::test]
    async fn model_switch_updates_snapshot_for_new_sessions() {
        let config = crate::config::RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.model_inherit"
            version = "1.0.0"
            name = "Test model inherit"
            description = "Pin new-session model inheritance"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));

        let resolver = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new_for_test(
            Vec::new(),
        )));
        let mut manager = SessionManager::new(core, SessionManagerConfig::default());
        manager.set_resolver(resolver);

        let s1 = manager.create_session().await.unwrap();

        // Pre-condition: without a configured global provider list the
        // session snapshot starts empty. (The mock provider isn't in
        // global_provider_list, so current_model_and_provider() returns
        // (None, None).)
        let pre = manager
            .snapshot_session_state(&s1)
            .expect("s1 snapshot exists");
        assert_eq!(pre.model, None);
        assert_eq!(pre.provider, None);

        // User switches S1 to model-A / provider-X.
        manager
            .route_model_switch(&s1, "model-A".to_string(), Some("provider-X".to_string()), None)
            .unwrap();

        // The per-session snapshot must reflect the switch IMMEDIATELY
        // (no waiting for the next AgentLoop emit).
        let after = manager
            .snapshot_session_state(&s1)
            .expect("s1 snapshot exists");
        assert_eq!(after.model.as_deref(), Some("model-A"));
        assert_eq!(after.provider.as_deref(), Some("provider-X"));

        // And `current_model_and_provider()` (used by the next session's
        // `build_initial_session_state`) returns the switched pair.
        assert_eq!(
            manager.current_model_and_provider(),
            (Some("model-A".to_string()), Some("provider-X".to_string()))
        );

        // A brand-new session inherits the switched model/provider —
        // not the stale fallback.
        let s2 = manager.create_session().await.unwrap();
        let s2_snap = manager
            .snapshot_session_state(&s2)
            .expect("s2 snapshot exists");
        assert_eq!(s2_snap.model.as_deref(), Some("model-A"));
        assert_eq!(s2_snap.provider.as_deref(), Some("provider-X"));
    }

    // ── Multi-account pick inheritance regression (ADR-012) ────────
    //
    // Bug history: `current_model_and_provider()` was inherited on
    // new-session creation, but the multi-account `account_id` was
    // not — a brand-new session started with `account_id = None`,
    // so the model-select menu showed no active alias highlight and
    // the right-panel session-status card had no alias row until
    // the operator manually re-picked an account. Mirror the same
    // inheritance chain for `account_id` via
    // `SessionManager::current_account_id()`.
    #[tokio::test]
    async fn account_pick_inherits_for_new_sessions() {
        // Helper: read the runtime-mirrored `account_id` for a session via
        // its SessionRuntimeSnapshot. Mirrors the production read path
        // the model-switch test pins (`snapshot_session_state`), and
        // matches what `current_account_id()` consults when
        // `session_configs` is `None` (the test config). The
        // authoritative value lives in `ConversationSession::account_id`
        // in production but is mirrored here by `route_model_switch`
        // (synchronous) and `ConversationSession::update_account_id`
        // (inheritance path on new-session creation).
        let read_account = |manager: &SessionManager, sid: &str| -> Option<String> {
            manager
                .snapshot_session_state(sid)
                .and_then(|s| s.account_id)
        };

        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::config::RuntimeConfig {
            work_dir: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.account_inherit"
            version = "1.0.0"
            name = "Test account inherit"
            description = "Pin account_id inheritance for new sessions"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let resolver = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new_for_test(
            Vec::new(),
        )));
        let mut manager = SessionManager::new(core, SessionManagerConfig::default());
        manager.set_resolver(resolver);

        // s1 picks a multi-account slot for provider-X.
        let s1 = manager.create_session().await.unwrap();
        manager
            .route_model_switch(
                &s1,
                "model-A".to_string(),
                Some("provider-X".to_string()),
                Some("acct-1".to_string()),
            )
            .unwrap();

        // Sanity: s1's snapshot mirror carries the account_id written
        // by `route_model_switch` (synchronous mirror path). This is
        // what `current_account_id()`'s Level-2 lookup consults when
        // `session_configs` is None.
        assert_eq!(
            read_account(&manager, &s1).as_deref(),
            Some("acct-1"),
        );

        // current_account_id() — the resolver consulted by
        // `build_initial_session_state` for the NEXT session —
        // returns the most recently active session's pick.  Pin this
        // BEFORE s2 lands in `self.sessions`: `build_initial_session_state`
        // runs *before* `sessions.insert`, so the resolver never sees
        // the freshly created session in its input set. If we called
        // it after `create_session()`, s2's empty snapshot would win
        // the max_by_key tie-break.
        assert_eq!(
            manager.current_account_id().as_deref(),
            Some("acct-1"),
        );

        // A brand-new session inherits the picked account_id. Its
        // runtime snapshot mirror must reflect it so the model-select
        // menu highlight and the right-panel alias row agree from the
        // very first frame.
        let s2 = manager.create_session().await.unwrap();
        let s2_snap = manager
            .snapshot_session_state(&s2)
            .expect("s2 snapshot exists");
        assert_eq!(s2_snap.model.as_deref(), Some("model-A"));
        assert_eq!(s2_snap.provider.as_deref(), Some("provider-X"));
        assert_eq!(
            read_account(&manager, &s2).as_deref(),
            Some("acct-1"),
            "inherited account_id must be mirrored into the runtime \
             snapshot so it reaches the frontend via the session_state \
             push on first emit",
        );

        // Negative case: a session created in an empty runtime (no
        // prior active session to inherit from) keeps account_id =
        // None — single-account providers and never-picked operators
        // must not be silently overridden.
        let dir2 = tempfile::TempDir::new().unwrap();
        let config2 = crate::config::RuntimeConfig {
            work_dir: dir2.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        let manifest2 = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.account_inherit_negative"
            version = "1.0.0"
            name = "Test no inherit"
            description = "No account inheritance when no prior pick"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider2 = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core2 = Arc::new(AgentCore::new(
            config2,
            manifest2,
            provider2,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let resolver2 = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new_for_test(
            Vec::new(),
        )));
        let mut manager2 = SessionManager::new(core2, SessionManagerConfig::default());
        manager2.set_resolver(resolver2);

        assert_eq!(manager2.current_account_id(), None);

        let s_fresh = manager2.create_session().await.unwrap();
        assert_eq!(
            read_account(&manager2, &s_fresh),
            None,
            "fresh runtime with no prior pick must not invent an account_id",
        );
    }

    // ── Delete-latest regression (orphan meta / stale latest marker) ──
    //
    // Bug history: `delete_session` removed the meta + JSONL files but
    // never touched the in-memory `latest_session` marker. Deleting the
    // current latest session left `/sessions/latest` pointing at a session
    // whose JSONL was gone; the frontend then tried to resume it and
    // failed. Fix: after deleting, if the deleted session was the latest,
    // recompute it from the remaining meta files on disk (or clear it when
    // none remain). This test pins that contract.
    #[tokio::test]
    async fn delete_session_recomputes_latest_when_deleting_latest() {
        use crate::conversation::{SessionMeta, write_session_meta};

        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::config::RuntimeConfig {
            work_dir: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.delete_latest"
            version = "1.0.0"
            name = "Test delete latest"
            description = "Pin latest-session cleanup on delete"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let mut manager = SessionManager::new(core, SessionManagerConfig::default());

        // Seed a surviving session (meta row + jsonl) so the recompute has
        // something to find.
        let survivor_id = "20260101_000000_survivor";
        let conversations_dir = dir.path().join("conversations");
        std::fs::create_dir_all(&conversations_dir).unwrap();
        std::fs::write(conversations_dir.join(format!("{}.jsonl", survivor_id)), "").unwrap();
        write_session_meta(
            &conversations_dir,
            &SessionMeta {
                version: 3, // CONVERSATION_FORMAT_VERSION
                session_id: survivor_id.to_string(),
                agent_id: "com.test.delete_latest".to_string(),
                created_at: "2026-01-01T00:00:00Z".to_string(),
                user_id: None,
                visibility: None,
                title: Some("Survivor".to_string()),
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
            },
        )
        .unwrap();

        // Simulate the buggy state: latest points at a session that is
        // about to be deleted.
        manager.set_latest_session("doomed".to_string(), None);
        assert_eq!(
            manager.latest_session().map(|(id, _)| id),
            Some("doomed".to_string())
        );

        // Delete the doomed session (not in memory; files absent).
        manager.delete_session("doomed").await;

        // The latest marker must be recomputed to the surviving session —
        // NOT left pointing at the deleted one.
        let latest = manager.latest_session();
        assert_eq!(
            latest.as_ref().map(|(id, _)| id.as_str()),
            Some(survivor_id),
            "latest must be recomputed from disk after deleting the latest session"
        );
        assert_eq!(
            latest.and_then(|(_, title)| title),
            Some("Survivor".to_string())
        );

        // Deleting the last remaining session clears latest entirely.
        manager.delete_session(survivor_id).await;
        assert_eq!(manager.latest_session(), None);
    }

    // ── New-session visibility default (ADR-076 §决策 4) ──────────────
    //
    // Bug history: `create_frontend_session` stamped only the owner, so a
    // session created by an identified account kept `visibility: None` —
    // and `None` reads as *public*. In a multi-user deployment that meant
    // every account could read every new session, including the ones with
    // the system agent, until someone noticed a lock icon was missing.
    //
    // Fix: an owned session is stamped `private` at birth. `None` stays
    // reserved for sessions that predate accounts (no owner to restrict
    // them to — `is_readable_by` ignores the flag for those), which is
    // also what local mode produces, so local mode is unchanged.
    //
    // Asserted on the **disk** meta, not the in-memory handle: the listing
    // and authorization paths read the file, so a value that only lives in
    // memory is the bug, not the fix.
    #[tokio::test]
    async fn owned_sessions_start_private_and_ownerless_stay_public() {
        use crate::conversation::{SessionScope, SessionVisibility, read_session_meta};

        let mut config = crate::config::RuntimeConfig::default();
        // The default `work_dir` is empty, which would write meta files
        // relative to the test runner's CWD. Keep this hermetic.
        let work_dir = std::env::temp_dir().join(format!("acowork-visibility-{}", Uuid::new_v4()));
        config.work_dir = work_dir.to_string_lossy().to_string();

        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.visibility_default"
            version = "1.0.0"
            name = "Test visibility default"
            description = "Pin the new-session visibility default"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider =
            Arc::new(acowork_core::providers::mock::MockProvider::single_text("test"));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));

        let mut manager = SessionManager::new(core, SessionManagerConfig::default());
        // `create_frontend_session` resolves a workspace, so the resolver
        // has to exist even though this test never switches one.
        manager.set_resolver(Arc::new(std::sync::RwLock::new(
            WorkspaceResolver::new_for_test(vec![]),
        )));
        let conversations = work_dir.join("conversations");

        // An identified account creates a session: private, and not
        // readable by another account.
        let owned = manager
            .create_frontend_session(None, None, None, Some("u-alice"))
            .await
            .expect("create owned session");
        let meta = read_session_meta(&conversations, &owned).expect("read owned meta");
        assert_eq!(meta.user_id.as_deref(), Some("u-alice"));
        assert_eq!(
            meta.visibility,
            Some(SessionVisibility::Private),
            "an owned session must be stamped private at creation"
        );
        assert!(meta.is_readable_by(&SessionScope::User("u-alice".to_string())));
        assert!(
            !meta.is_readable_by(&SessionScope::User("u-bob".to_string())),
            "another account must not be able to read a new session"
        );
        // admin (unfiltered scope) still sees everything.
        assert!(meta.is_readable_by(&SessionScope::Unfiltered));

        // No owner (local mode, or data predating accounts): absent, i.e.
        // public — the pre-ADR-076 behaviour must survive.
        let ownerless = manager
            .create_frontend_session(None, None, None, None)
            .await
            .expect("create ownerless session");
        let meta = read_session_meta(&conversations, &ownerless).expect("read ownerless meta");
        assert_eq!(meta.user_id, None);
        assert_eq!(
            meta.visibility, None,
            "local mode must keep writing nothing — absent means public"
        );
        assert!(meta.is_readable_by(&SessionScope::User("u-bob".to_string())));

        let _ = std::fs::remove_dir_all(&work_dir);
    }

    // ── resume_session retained-config republish (open-state-machine invariant) ──

    /// Build a `SessionManager` wired to a chunk channel over a TempDir work
    /// dir, returned together with the channel receiver. The chunk channel is
    /// what the per-session config relay publishes `SessionConfigChanged`
    /// through, so observing it proves a republish happened.
    fn republish_test_manager(
        dir: &tempfile::TempDir,
    ) -> (SessionManager, tokio::sync::mpsc::Receiver<SessionChunkEvent>) {
        let config = crate::config::RuntimeConfig {
            work_dir: dir.path().to_string_lossy().to_string(),
            ..Default::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.republish"
            version = "1.0.0"
            name = "Test republish"
            description = "Pin resume_session retained-config refresh"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text(
            "test",
        ));
        let core = Arc::new(AgentCore::new(
            config,
            manifest,
            provider,
            Vec::<crate::agent::agent_core::BuiltinToolEntry>::new(),
        ));
        let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(64);
        let mut manager = SessionManager::new(
            core,
            SessionManagerConfig {
                chunk_tx: Some(chunk_tx),
                ..Default::default()
            },
        );
        manager.set_resolver(Arc::new(std::sync::RwLock::new(
            WorkspaceResolver::new_for_test(vec![]),
        )));
        (manager, chunk_rx)
    }

    /// Let queued relay forwards land, then drain the chunk channel. Used to
    /// establish "nothing else is in flight" before asserting that an open
    /// itself produced a `SessionConfigChanged`.
    async fn settle_and_drain(rx: &mut tokio::sync::mpsc::Receiver<SessionChunkEvent>) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        while rx.try_recv().is_ok() {}
    }

    /// Wait for the next `SessionConfigChanged` (skipping other chunk
    /// events). Panics with context on timeout or channel close.
    async fn next_config_changed(
        rx: &mut tokio::sync::mpsc::Receiver<SessionChunkEvent>,
    ) -> String {
        loop {
            let evt = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                .await
                .expect("timeout waiting for SessionConfigChanged republish")
                .expect("chunk channel closed before SessionConfigChanged");
            if matches!(
                evt.event,
                crate::agent::loop_::ChunkEvent::SessionConfigChanged { .. }
            ) {
                return evt.session_id;
            }
        }
    }

    /// Regression for the stale-retained-snapshot bug: every successful open
    /// — including the `AlreadyActive` path, which mutates no config — must
    /// re-publish the session's `SessionConfig` so the broker's retained
    /// `llm_availability` cannot stay stale forever (permanent "syncing LLM
    /// config" banner in the Desktop). The invariant lives in
    /// `resume_session`, not in the HTTP handler, so any future open entry
    /// point inherits it.
    #[tokio::test]
    async fn resume_session_republishes_config_on_both_open_outcomes() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut manager, mut chunk_rx) = republish_test_manager(&dir);
        // `create_frontend_session` (the production HTTP create path) is the
        // one that attaches a `ConversationSession` and persists meta —
        // bare `create_session` has no conversation and cannot be resumed.
        let sid = manager
            .create_frontend_session(None, None, None, None)
            .await
            .unwrap();

        // Closed → ResumedFromDisk: the relay is spawned inside `open` before
        // the republish runs, so the event must reach the chunk channel.
        settle_and_drain(&mut chunk_rx).await;
        manager.close_session(&sid).await.unwrap();
        settle_and_drain(&mut chunk_rx).await;
        let outcome = manager.resume_session(&sid).await.unwrap();
        assert!(
            matches!(outcome, Some(SessionOpenOutcome::ResumedFromDisk)),
            "unexpected outcome: {outcome:?}"
        );
        assert_eq!(next_config_changed(&mut chunk_rx).await, sid);

        // AlreadyActive: no config mutation happens on this path — without
        // the invariant this open would emit nothing.
        settle_and_drain(&mut chunk_rx).await;
        let outcome = manager.resume_session(&sid).await.unwrap();
        assert!(matches!(outcome, Some(SessionOpenOutcome::AlreadyActive)));
        assert_eq!(next_config_changed(&mut chunk_rx).await, sid);
    }

    /// A `NotFound` open returns `Ok(None)` and must not publish anything —
    /// the republish is tied to *successful* opens only.
    #[tokio::test]
    async fn resume_session_notfound_does_not_republish() {
        let dir = tempfile::TempDir::new().unwrap();
        let (mut manager, mut chunk_rx) = republish_test_manager(&dir);
        let outcome = manager.resume_session("no-such-session").await.unwrap();
        assert!(outcome.is_none());
        settle_and_drain(&mut chunk_rx).await;
        assert!(
            chunk_rx.try_recv().is_err(),
            "a NotFound open must not emit any chunk event"
        );
    }

    /// `republish_session_config` is a silent-safe no-op for a session that
    /// is not active (unknown id) or whose handle has no conversation
    /// attached — it must never panic and must never publish.
    #[tokio::test]
    async fn republish_session_config_noop_without_live_conversation() {
        use crate::agent::session::session_handle::SessionHandle;
        use crate::agent::session::session_task::SessionMessage;
        use crate::agent::session_state::{SessionRuntimeSnapshot, SessionStatus};

        let dir = tempfile::TempDir::new().unwrap();
        let (mut manager, mut chunk_rx) = republish_test_manager(&dir);

        // 1. Unknown id.
        manager.republish_session_config("never-existed");

        // 2. Handle present but `conversation: None` (startup-scan shape).
        let sid = "no_conv_handle".to_string();
        let (inbound_tx, _inbound_rx) = tokio::sync::mpsc::channel::<SessionMessage>(1);
        let (agent_inbound_tx, _agent_inbound_rx) =
            tokio::sync::mpsc::channel::<crate::agent::inbound::InboundMessage>(1);
        let (_status_tx, status_rx) = tokio::sync::watch::channel(SessionStatus::Idle);
        manager.sessions.insert(
            sid.clone(),
            SessionHandle {
                session_id: sid.clone(),
                inbound_tx,
                agent_inbound_tx,
                join_handle: tokio::spawn(async {}),
                status_rx,
                last_active_at: std::sync::Mutex::new(std::time::Instant::now()),
                pending_debug_handles: Arc::new(tokio::sync::Mutex::new(None)),
                snapshot: Arc::new(std::sync::RwLock::new(SessionRuntimeSnapshot {
                    session_id: sid.clone(),
                    status: r#""idle""#.to_string(),
                    model: None,
                    provider: None,
                    account_id: None,
                    ratio: None,
                    todos_json: None,
                    context_usage: None,
                })),
                workspace_id: Arc::new(std::sync::RwLock::new("__agent_home__".to_string())),
                current_work_dir: Arc::new(std::sync::RwLock::new(None)),
                conversation: None,
            },
        );
        manager.republish_session_config(&sid);

        settle_and_drain(&mut chunk_rx).await;
        assert!(
            chunk_rx.try_recv().is_err(),
            "no-op republish paths must not emit any chunk event"
        );
    }
}
