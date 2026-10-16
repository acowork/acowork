//! Cross-session shared state for Agent Runtime.
//!
//! `AgentCore` holds all resources that are shared across sessions:
//! runtime config, manifest, LLM provider, tool registry,
//! Gateway model capabilities, Grafeo memory store, and the shared
//! streaming-lines map. These resources persist for the lifetime of
//! the agent process and are independent of any individual session.
//!
//! Per-session state (session_id, chunk channel, notification control,
//! JSONL counters, workspace, retry UX, approval) lives in
//! [`SessionCore`](super::session_core::SessionCore).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use acowork_core::protocol::{ModelCapabilitiesInfo, ProtocolType, ProviderKeyEntry, ProviderListItem};
use acowork_core::providers::traits::{Provider, UsageInfo};
use acowork_core::rag::RagProvider;
use acowork_core::tools::traits::Tool;
use acowork_memory::MemoryProvider;
use acowork_memory::admin::MemoryAdminService;
use acowork_memory::consolidation::SchedulerConfig;
use acowork_memory::types::EpisodicDecayConfig;

use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::config::RuntimeConfig;
use crate::debug::DebugObserverSlot;
use crate::embedding::EmbeddingProvider;
use crate::memory::{MemoryManager, MemoryManagerConfig};
use crate::security::approval_gate::ApprovalGate;
use acowork_core::ShellApprovalThreshold;

/// A builtin tool entry — wraps the raw `Arc<dyn Tool>` together with
/// its per-agent `enabled` flag (ADR-029).
///
/// `enabled == false` tools are still present in
/// [`AgentCore::builtin_tools`] (so the frontend can render the full
/// list with checkboxes), but they are filtered out of
/// [`AgentCore::all_tools`] (the LLM dispatch list).
///
/// Clone is cheap because `Arc<dyn Tool>` is internally reference-counted.
///
/// **ADR-030 C3**: visibility was raised from `pub(crate)` to `pub` so that
/// `SessionMessage::AddDynamicBuiltinTool` (a `pub` enum) can carry this
/// type as a field. The runtime is a single crate (`acowork-runtime`) and
/// does not re-export `BuiltinToolEntry`, so this is effectively an
/// internal-only widening.
#[derive(Clone)]
pub struct BuiltinToolEntry {
    /// The tool implementation (cloned from the registry).
    pub tool: Arc<dyn Tool>,
    /// Whether this tool is enabled for the current agent.
    pub enabled: bool,
}

impl BuiltinToolEntry {
    /// Tool name (delegated to the underlying `Tool`).
    ///
    /// Returns a `String` rather than `&str` for backward compatibility
    /// with the many call sites that store the result in a local
    /// variable and compare against it. The `Tool` trait's `name()` API
    /// returns `String` so this is a thin allocation each call; given
    /// that builtin_tools is small (tens of entries), this is fine.
    pub fn name(&self) -> String {
        self.tool.name()
    }

    /// Tool specification (delegated to the underlying `Tool`).
    pub fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
        self.tool.spec()
    }
}

/// Cross-session shared state for the agent loop.
///
/// Fields here are immutable or rarely mutated at runtime (e.g. provider swap
/// via model_switch), and are shared across all sessions of the same agent.
/// Per-session state lives in [`super::session_core::SessionCore`].
pub struct AgentCore {
    /// Runtime configuration
    pub(crate) config: RuntimeConfig,
    /// Agent manifest (declarative .agent package metadata)
    pub(crate) manifest: acowork_core::AgentManifest,
    /// LLM Provider
    pub(crate) provider: Arc<dyn Provider>,
    /// Built-in tool registry — the static base set used as the seed for
    /// rebuilding `all_tools` whenever `mcp_tools` or `builtin_tools`
    /// enabled flags change. These are the tools shipped with the
    /// runtime binary and registered in `crate::tools::builtin`. Each
    /// entry carries its own `enabled` flag (ADR-029); only `enabled`
    /// entries are merged into [`Self::all_tools`] by `rebuild_all_tools`.
    /// MCP tools are kept separate in [`Self::mcp_tools`].
    pub(crate) builtin_tools: Vec<BuiltinToolEntry>,
    /// MCP (Model Context Protocol) tool wrappers, populated when MCP servers
    /// have been connected. These are merged into [`all_tools`] at rebuild time.
    pub(crate) mcp_tools: Option<Vec<Arc<dyn Tool>>>,
    /// Merged tool list for dispatch — contains enabled built-in tools + MCP tools.
    pub(crate) all_tools: Vec<Arc<dyn Tool>>,
    /// Global provider list — full metadata including models, capabilities,
    /// base_url, protocol_type, compact_model for all configured providers.
    pub(crate) global_provider_list: Arc<RwLock<Vec<ProviderListItem>>>,
    /// Provider list version for diff sync with Gateway.
    ///
    /// Shared via `Arc<RwLock<...>>` for the same reason as
    /// [`Self::provider_compact_models`] — written by the SessionManager
    /// template on `acowork/global/providers` updates and read (if ever)
    /// through the same Arc by every session clone.
    pub(crate) provider_list_version: Arc<RwLock<u64>>,
    /// Provider key vault (in-memory only, never persisted).
    pub(crate) provider_key_vault: Arc<RwLock<HashMap<String, Vec<ProviderKeyEntry>>>>,

    /// Per-agent compatibility cache, shared across all provider instances
    /// (including those rebuilt by `build_provider_for`).  `None` when no
    /// provider was available at startup (noop fallback).
    pub(crate) compat_cache: Option<Arc<crate::providers::compat::CompatCache>>,

    /// Provider→compact_model mapping from provider_list at AgentHello.
    ///
    /// Shared across sessions via `Arc<RwLock<...>>` (ADR-063 §3.7.5
    /// pattern): `SessionManager::update_global_provider_list` writes
    /// through the RwLock, and every session's deep-cloned AgentCore
    /// shares the same inner Arc, so in-flight sessions see the change
    /// on their next `resolve_distill_model()` call. A plain HashMap
    /// would be snapshotted per session and go stale.
    pub(crate) provider_compact_models: Arc<RwLock<HashMap<String, Option<String>>>>,
    /// ADR-056: Ordered global compact-model candidate list — each entry is
    /// `(provider_id, model_id)`. Set from
    /// `AvailableProviders.default_compact_models` at session init.
    /// Top-priority tier in the distillation fallback chain
    /// (`resolve_distill_model`): entries are tried in order; when all are
    /// unusable the runtime falls back to `provider_compact_models`
    /// (provider tier) and finally the session's current chat model.
    /// Empty means no global override.
    ///
    /// Shared across sessions via `Arc<RwLock<...>>` for the same reason
    /// as [`Self::provider_compact_models`] — the global pick must reach
    /// in-flight sessions, not just sessions created after the change.
    pub(crate) default_compact_models: Arc<RwLock<Vec<(String, String)>>>,
    /// LLM temperature override (from Gateway config via agent_config.json).
    /// Level 1 in the resolution chain.
    pub(crate) temperature_override: Option<f32>,
    /// LLM temperature from manifest.toml [llm].temperature (Layer 2).
    /// Seeded at agent startup in cli.rs; independent of temperature_override
    /// so the resolution chain is self-contained in AgentCore.
    pub(crate) manifest_temperature: Option<f32>,
    /// Per-agent context window cap (from agent_config.json, set via Agent Setup panel).
    /// Layer 1 in the resolution chain. `0` is invalid (ADR-074 §6 — the old
    /// "no limit" sentinel is abolished) and the chain skips this layer.
    pub(crate) context_window_override: Option<u64>,
    /// ADR-061: minimum compression ratio for levels 1-7 (from
    /// agent_config.json, set via Agent Setup panel). `None` = use
    /// [`crate::agent::compression_constants::MIN_COMPRESSION_RATIO`] (0.90
    /// default = "compress until at most 10% remains").
    pub(crate) compression_ratio_threshold: Option<f64>,
    /// ADR-071 D4/D6: runtime distiller settings (`agent_config.json`).
    /// Layer 1 of the distiller config chain, above the manifest
    /// `[memory.distiller]` section. Written by the memory-panel PUT
    /// (`distiller_enabled` / `distiller_models` / …) and applied when the
    /// consolidation pipeline starts or is rebuilt.
    pub(crate) distiller_runtime: DistillerRuntimeSettings,
    /// Episodic forgetting runtime settings (`agent_config.json`). Layer 1
    /// of the forgetting config chain (defaults live in
    /// [`acowork_memory::EpisodicDecayConfig::default`]). Written by the
    /// memory-panel PUT (`memory_forgetting_*`) and applied when the
    /// consolidation pipeline starts or is rebuilt.
    pub(crate) forgetting_runtime: ForgettingRuntimeSettings,
    /// Context window cap from manifest.toml [llm].context_window (Layer 2).
    /// Seeded at agent startup in cli.rs; independent of context_window_override
    /// so the resolution chain is self-contained in AgentCore.
    pub(crate) manifest_context_window: Option<u64>,
    /// Approval timeout in seconds for loop approval. None = use system default (300).
    pub(crate) approval_timeout_secs: Option<u64>,
    /// ADR-032: number of recent tool results preserved raw at every trigger
    /// System prompt override (from Gateway config).
    pub(crate) system_prompt_override: Option<String>,
    /// Per-agent LLM session language override (e.g. `"zh-CN"`, `"en"`).
    /// When `Some(_)`, replaces `UserProfile.language` in the
    /// identity-context text block formatted for the LLM (see
    /// `SessionManager::format_user_profile_context`). `None` = follow
    /// the global `UserProfile.language`. Decoupled from the desktop
    /// UI language (i18n stays bound to `UserProfile.language`).
    pub(crate) session_language_override: Option<String>,
    /// Per-agent skill registry (`skills/*/SKILL.md`), loaded once in
    /// Phase A and injected into AgentCore in Phase B. The authoritative
    /// source for resolving a per-turn chat `command` (skill name) into
    /// skill instructions (see `SessionManager::resolve_skill_instructions`).
    /// Shared across session clones; empty registry = no skills installed.
    pub(crate) skill_registry: Arc<crate::skills::parser::SkillRegistry>,
    /// Agent-specific compaction prompt (from `prompts/summary.md` in the
    /// .agent package). `None` (the inner `Option`) = use the built-in
    /// [`crate::prompt::COMPACTION_SYSTEM_PROMPT`] fallback.
    ///
    /// This is the package-declared summarization directive for context
    /// compaction and episode distillation. It is deliberately independent
    /// of `system_prompt_override` (which covers the MAIN dialog prompt
    /// only) — compaction is a summarization task with its own directive,
    /// and per-agent rules belong to the package, not the runtime config.
    ///
    /// ADR-063 §3.7.5: wrapped in `Arc<RwLock<...>>` so the Debug panel
    /// L2 reload (`DebugService::reload_prompts`) can write through any
    /// `Arc<AgentCore>` clone held by a running session, not just the
    /// canonical `AgentCore` in `SessionManager`. `Clone for AgentCore`
    /// shares the inner `Arc` (reference +1) — writes from one clone are
    /// visible to all others.
    pub(crate) compaction_prompt: Arc<std::sync::RwLock<Option<String>>>,

    // ── ADR-063: 7 additional package-level LLM prompt overrides ──
    //
    // Each follows the same `Arc<RwLock<Option<String>>>` pattern as
    // `compaction_prompt` (see doc-comment above for L2 reload semantics).
    // Field naming maps 1-to-1 to entries in
    // `crate::package::prompt_builder::OVERRIDABLE_PROMPTS`; see ADR-063
    // §3.2 for the full list and resolution chain.
    /// Override for `crate::prompt::COMPACT_PROMPT`
    /// (`prompts/compact-template.md`). Authors MUST preserve the
    /// `{messages_text}` placeholder — the runtime substitution is the
    /// same `format!` pattern used for the built-in constant.
    pub(crate) compact_template: Arc<std::sync::RwLock<Option<String>>>,

    /// Override for `crate::prompt::TITLE_PROMPT`
    /// (`prompts/title.md`). Authors may use `{language}` and
    /// `{user_message}` placeholders.
    pub(crate) title_prompt: Arc<std::sync::RwLock<Option<String>>>,

    /// Override for `acowork-memory::manager::DEFAULT_ABSTENTION_PROMPT`
    /// (`prompts/abstention.md`). Same reload caveat.
    ///
    /// **ADR-068 note**: Three grafeo-specific override fields
    /// (`extraction_prompt`, `conflict_classification_prompt`,
    /// `generalization_prompt`) were removed in this revision because
    /// ADR-068 rewrites the LLM→memory boundary — LLM writes only
    /// Episode nodes via `memory_store`; grafeo's distillation is
    /// driven by the offline `EpisodicDistiller` pipeline (which
    /// owns its own internal prompts and constants). Those constants stay
    /// private to the distiller module in `acowork-memory`.
    pub(crate) abstention_prompt: Arc<std::sync::RwLock<Option<String>>>,

    // ── ADR-071 D7/D9: per-agent EpisodicDistiller prompt overrides ──
    //
    // Follows the same `Arc<RwLock<Option<String>>>` pattern as the
    // ADR-063 fields above. The ADR-068 revision removed three
    // grafeo-specific overrides (`extraction.md`,
    // `conflict-classification.md`, `generalization.md`) because the
    // LLM→memory boundary was rewritten to be Episode-only; ADR-071
    // re-adds two distiller overrides because the offline
    // `EpisodicDistiller` pipeline now needs per-agent prompt control
    // (independent of the compaction/title prompts). Resolved via
    // `distiller_scheduler_config()`, which projects this slot onto
    // `DistillerConfig.merge_prompt_override`.
    /// Override for the built-in `MERGE_SYSTEM_PROMPT` in `acowork-memory`
    /// (the distiller's merge decision, `prompts/distiller-merge.md`).
    /// Inner `None` = use the built-in constant.
    ///
    /// ADR-071 D7 defined two slots (extraction + judge); the projection +
    /// merge rewrite collapsed both pipelines into one LLM call, so one slot
    /// remains.
    pub(crate) distiller_merge_prompt: Arc<std::sync::RwLock<Option<String>>>,

    /// Memory store (SQLite, shared across all sessions of this agent).
    /// ADR-051 P4: Primary field is `memory_provider` (trait object).
    /// `memory_admin` is the admin interface for HTTP endpoints and
    /// embedding migration.
    pub(crate) memory_provider: Option<Arc<dyn MemoryProvider>>,
    /// Admin service for HTTP memory endpoints and embedding migration.
    /// ADR-051 P4: Replaces the former `grafeo_store` compat field.
    pub(crate) memory_admin: Option<Arc<dyn MemoryAdminService>>,
    /// The open SQLite store, when the SQLite backend is selected (ADR-082 §4).
    ///
    /// Kept so the conversation index can share the same connection (and
    /// file) instead of opening a second one — `memory/private.sqlite` holds
    /// memory nodes, session meta and the conversation index together.
    /// `None` on the grafeo backend.
    pub(crate) sqlite_store: Option<Arc<acowork_sqlite::SqliteStore>>,
    /// RAG provider (enterprise knowledge retrieval, orthogonal to MemoryProvider).
    /// ADR-051 C3: New field, independent from memory_provider.
    pub(crate) rag_provider: Option<Arc<dyn RagProvider>>,
    /// Debug observer slot — Production (no-op) or Dev (real observer).
    pub(crate) debug_observer: DebugObserverSlot,
    /// ADR-048: shared bypass-injection channel for debug handles.
    ///
    /// SessionManager writes `DebugHandles` here while the agent loop is
    /// running (its message channel is blocked inside `run()`), and the
    /// agent loop drains it at the start of each iteration via
    /// [`Self::take_pending_debug_handles`].
    ///
    /// Held here — not on the observer slot — because the observer's
    /// `set_pending_injection` is a no-op while the slot is `Production`,
    /// which is exactly the state during a mid-loop `EnableDebugMode`.
    /// `disable_debug_mode` clears the shared slot (session_manager.rs),
    /// so a stale handles pointer cannot be applied after teardown.
    pub(crate) pending_debug_handles:
        Option<Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>>>,
    /// ADR-046: blob store for `file_upload` / `image_upload` items
    /// (uploaded PDFs, images, …). Populated in Phase B of `session_init`
    /// via [`AgentCore::set_attachment_service`]; the agent loop reads
    /// this slot to convert `image_upload` items into multimodal
    /// `ContentPart::ImageUrl` payloads before the LLM call.
    pub(crate) attachment_service: Option<Arc<dyn crate::usecases::AttachmentService>>,
    /// Approval gate for shell command risk confirmation.
    pub(crate) approval_gate: Option<Arc<dyn ApprovalGate>>,
    /// Shell approval threshold: Low / Medium / High / Auto-approve.
    pub(crate) shell_approval_threshold: ShellApprovalThreshold,
    /// Shell risk rules (loaded from config dir on startup).
    pub(crate) shell_risk_rules: crate::security::shell_risk::ShellRiskRules,
    /// ADR-078 follow-up: slots the git-refresh nudge needs to publish.
    ///
    /// `None` until Phase B binds them (same late-bind pattern as
    /// [`Self::approval_gate`]); the tool-execution path treats `None`
    /// as "publish nothing", which is correct for CLI mode and for the
    /// window before Phase B finishes.
    ///
    /// Kept as slots rather than constructor arguments because the MQTT
    /// client and the workspace resolver are both created in Phase A,
    /// *before* `AgentCore` is built — but they reach the tool path only
    /// through this struct, and threading them through
    /// `new_with_observer` would touch every construction site (tests
    /// included) for no ordering benefit.
    pub(crate) git_nudge: Option<crate::agent::git_nudge::GitNudgeSlots>,
    /// Memory session handle — shared between agent loop and memory tools.
    pub(crate) memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    /// Embedding provider for vector-based memory retrieval.
    ///
    /// This field is the *private* view of the provider. Anything outside this
    /// clone must read [`Self::embedding_provider_shared`] instead, because
    /// `AgentCore` is `Clone` and the runtime publishes an `Arc<AgentCore>`
    /// snapshot for the conversation indexer and the HTTP layer: a write here
    /// never reaches those readers.
    pub(crate) embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    /// The provider as one shared cell, identical across every clone of this
    /// core. See [`crate::embedding::SharedEmbedProvider`].
    pub(crate) embedding_provider_shared: crate::embedding::SharedEmbedProvider,
    /// P3-1: Retrieval quality metrics aggregator (shared across sessions).
    /// ADR-051 C3: Replaced grafeo MetricsAggregator with Runtime-internal
    /// RetrievalMetricsAggregator (data from acowork_memory::RetrievalMetrics).
    pub(crate) metrics_aggregator: Arc<std::sync::Mutex<crate::memory::RetrievalMetricsAggregator>>,
    // ADR-051 C3: consolidation_scheduler field removed.
    // Consolidation control is delegated to MemoryProvider trait.
    // Background task handle kept for lifecycle management.
    pub(crate) consolidation_bg_task: Option<crate::memory::ConsolidationBgTask>,
    /// ADR-051 P4: Runtime-internal consolidation timer.
    /// Replaces grafeo's `ConsolidationScheduler`. The timer implements
    /// idle-timeout + accumulation-threshold scheduling policy without
    /// holding a store reference. `notify_consolidation_active()` resets
    /// the idle timer so consolidation doesn't run during active use.
    pub(crate) consolidation_timer: Option<Arc<crate::memory::ConsolidationTimer>>,
    /// ADR-028: cumulative input tokens across every LLM call made by this
    /// agent process. Sourced from `accumulate_llm_usage` on every LLM call
    /// and from `merge_token_totals` on every `list_sessions` scan. Not
    /// persisted; on process restart the next session-list scan rebuilds the
    /// baseline via atomic-max merge with the on-disk `SessionTokens`.
    ///
    /// `Arc`-wrapped so the four counters are genuinely shared by every
    /// `AgentCore` clone (template / per-session task / distillation spawn —
    /// see the manual `Clone` impl). A bare `AtomicU64` would be *copied*
    /// on clone: a per-session task would then report only its own slice of
    /// the agent-wide total, and the `list_sessions` disk-sum merge (which
    /// lands on the template) could never reach an already-running session
    /// (regression t-83afab47 follow-up: Agent Status "累计输入 Token"
    /// showed ≈ the Session Status total).
    pub(crate) agent_total_input_tokens: Arc<AtomicU64>,
    /// ADR-028: cumulative output tokens across every LLM call made by this
    /// agent process. See [`Self::agent_total_input_tokens`] for semantics.
    pub(crate) agent_total_output_tokens: Arc<AtomicU64>,
    /// ADR-066: cumulative prompt-cache read tokens across every LLM call
    /// made by this agent process (OpenAI `cached_tokens` / Anthropic
    /// `cache_read_input_tokens`).  Same persistence / restart semantics
    /// as [`Self::agent_total_input_tokens`].
    pub(crate) agent_total_cache_read_tokens: Arc<AtomicU64>,
    /// ADR-066: cumulative prompt-cache write tokens across every LLM
    /// call made by this agent process (Anthropic
    /// `cache_creation_input_tokens`; OpenAI has no concept → always 0).
    pub(crate) agent_total_cache_write_tokens: Arc<AtomicU64>,
}

/// ADR-071 D4/D6: runtime distiller settings, layered over the manifest
/// `[memory.distiller]` section.
///
/// These fields are the `agent_config.json` projection (the Desktop
/// memory panel writes them). `None` = "not set at runtime → fall
/// through to the manifest default". They feed the consolidation
/// pipeline's `SchedulerConfig` + model resolution chain.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DistillerRuntimeSettings {
    /// Master switch (overrides `[memory.distiller].enabled`).
    pub enabled: Option<bool>,
    /// Ordered distiller model candidate list (overrides the manifest
    /// pair). Tried in order; when all are unusable the chain continues
    /// manifest pair → global `default_compact_models` → first provider
    /// model. Empty provider_id = the agent's active provider.
    pub models: Vec<acowork_core::protocol::CompactModelRef>,
    /// Periodic trigger interval in minutes (overrides manifest).
    pub interval_minutes: Option<u64>,
    /// Unconsolidated-episode backlog threshold (overrides manifest).
    pub accumulation_threshold: Option<usize>,
    /// Idle threshold in minutes (overrides manifest).
    pub idle_minutes: Option<u64>,
}

/// Runtime settings for episodic memory forgetting, projected from
/// `agent_config.json` (the Desktop "记忆遗忘" card writes them). `None` =
/// "not set at runtime → fall through to system defaults". Forgetting is
/// opt-in: `enabled` defaults to false.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ForgettingRuntimeSettings {
    /// Master switch (default off).
    pub enabled: Option<bool>,
    /// Episodic decay half-life in days.
    pub half_life_days: Option<u64>,
    /// Retention threshold below which an episodic node becomes Dormant.
    pub dormant_threshold: Option<f32>,
    /// Days a Dormant node is retained before archiving to the PurgeLog.
    pub archive_days: Option<u64>,
}

/// Map a manifest `[memory.distiller]` section to a [`DistillerConfig`].
///
/// ADR-068 M4/M7: fields that are absent keep the `DistillerConfig` defaults;
/// returns `None` when no section is present (distiller stays disabled).
fn manifest_distiller_to_config(
    manifest: &acowork_core::manifest::ManifestDistillerConfig,
) -> Option<acowork_memory::consolidation::DistillerConfig> {
    let base = acowork_memory::consolidation::DistillerConfig::default();
    Some(acowork_memory::consolidation::DistillerConfig {
        batch_size: manifest.batch_size.unwrap_or(base.batch_size),
        min_importance: manifest.min_importance.unwrap_or(base.min_importance),
        // Recall width and threshold are not manifest-exposed: they are tuned
        // against the store's own similarity distribution, not per agent.
        // ADR-071 D7: the prompt override slot is layered on top of this
        // manifest-derived config at the AgentCore slots, so it stays `None`.
        ..base
    })
}

/// ADR-093 follow-up: which resolution tier
/// [`AgentCore::resolve_summary_llm`] ended up using. Returned alongside
/// the resolved `(Provider, model)` so the HTTP handler and logs can
/// tell the two paths apart without re-parsing the model id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SummaryModelTier {
    /// First usable entry from the global `default_compact_models`
    /// list (ADR-056).
    GlobalDefault,
    /// Session's current `(model, provider_id)` — the pre-existing
    /// fallback, preserved as the second tier.
    SessionCurrent,
}

impl AgentCore {
    // ── ADR-063 §3.7.5: accessor methods for LLM call sites ──
    //
    // Each accessor:
    //   1. Acquires a read lock briefly to clone the inner `Option<String>`.
    //   2. Returns `Option<String>` so call sites compose naturally with
    //      `.as_deref().unwrap_or(BUILTIN_CONSTANT)` exactly as they did
    //      before the ADR-063 Arc<RwLock<>> refactor (ADR-053 used the
    //      direct `Option<String>` field with the same idiom).
    //   3. Holds the lock only for the `clone()` — a few microseconds.
    //      LLM call sites do not hold the lock across network round-trips.
    //
    // For the runtime-level 3 fields (fallback / compact-template /
    // title), the accessor is plumbed by the calling code at the LLM
    // invocation site. For the grafeo/memory 4 fields, the accessor is
    // **not yet consumed** — wiring those requires trait-level injection
    // (ADR-063 §6.1, deferred) so the process-level singletons in those
    // crates can pick the value up via `AgentCore::shared()`. The fields
    // are loaded into the struct (Phase A/B) and are PUT-writable via the
    // HTTP API, but live cache invalidation requires the trait refactor.

    /// `prompts/summary.md` accessor (ADR-053 + ADR-063 §3.7.5).
    pub fn compaction_prompt(&self) -> Option<String> {
        self.compaction_prompt.read().unwrap().clone()
    }

    /// Override accessor — `prompts/compact-template.md` (runtime-level,
    /// used by `episode_distill::compact_full_context`). See ADR-063 §3.7.5.
    pub fn compact_template(&self) -> Option<String> {
        self.compact_template.read().unwrap().clone()
    }

    /// Override accessor — `prompts/title.md` (runtime-level, used by
    /// `episode_distill::compact_session_title_with_llm`). See ADR-063 §3.7.5.
    pub fn title_prompt(&self) -> Option<String> {
        self.title_prompt.read().unwrap().clone()
    }

    /// Override accessor — `prompts/abstention.md` (memory, deferred).
    /// See ADR-063 §6.1.
    pub fn abstention_prompt(&self) -> Option<String> {
        self.abstention_prompt.read().unwrap().clone()
    }

    /// ADR-071 D7/D9: override accessor — `prompts/distiller-merge.md`.
    /// Consumed by `distiller_scheduler_config()` when assembling the
    /// `DistillerConfig` handed to the grafeo EpisodicDistiller.
    pub fn distiller_merge_prompt(&self) -> Option<String> {
        self.distiller_merge_prompt.read().unwrap().clone()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_observer(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<BuiltinToolEntry>,
        observer: DebugObserverSlot,
    ) -> Self {
        let shell_approval_threshold =
            ShellApprovalThreshold::from_str_loose(&config.shell_approval_threshold)
                .unwrap_or_default();
        let manifest_temperature = manifest.llm.temperature;
        let manifest_context_window = manifest.llm.context_window;
        tracing::debug!(
            ?manifest_temperature,
            ?manifest_context_window,
            "AgentCore: seeding manifest_temperature and manifest_context_window from manifest [llm]"
        );

        // Compute initial `all_tools` now (enabled builtin + MCP later).
        // We can't fully rebuild yet because MCP hasn't been connected,
        // but seeding with the enabled builtin subset keeps dispatch
        // working from the very first LLM call.
        let initial_all_tools: Vec<Arc<dyn Tool>> = builtin_tools
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.tool.clone())
            .collect();

        Self {
            config: config.clone(),
            manifest,
            provider,
            builtin_tools,
            mcp_tools: None,
            all_tools: initial_all_tools,
            global_provider_list: Arc::new(RwLock::new(Vec::new())),
            provider_list_version: Arc::new(RwLock::new(0)),
            provider_key_vault: Arc::new(RwLock::new(HashMap::new())),
            compat_cache: None,
            provider_compact_models: Arc::new(RwLock::new(HashMap::new())),
            default_compact_models: Arc::new(RwLock::new(Vec::new())),
            temperature_override: None,
            manifest_temperature,
            context_window_override: None,
            compression_ratio_threshold: None,
            distiller_runtime: DistillerRuntimeSettings::default(),
            forgetting_runtime: ForgettingRuntimeSettings::default(),
            manifest_context_window,
            approval_timeout_secs: None,
            system_prompt_override: None,
            session_language_override: None,
            skill_registry: Arc::new(crate::skills::parser::SkillRegistry::new()),
            // ADR-053: populated in Phase B of session_init from
            // prompts/summary.md (see `load_compaction_prompt`).
            // ADR-063 §3.7.5: wrapped in `Arc<RwLock<...>>` so the L2
            // reload (DebugService::reload_prompts) can write through any
            // Arc<AgentCore> clone held by a running session, not just the
            // canonical one in SessionManager.
            compaction_prompt: Arc::new(std::sync::RwLock::new(None)),
            // ADR-063: 7 additional overridable prompt fields. Each
            // starts as `None` and is populated in Phase B from
            // `AgentBootContext.<field>` (loaded in Phase A from
            // `prompts/<file>.md`). The L2 reload (`reload_prompts`)
            // overwrites these via the `RwLock::write` guard.
            compact_template: Arc::new(std::sync::RwLock::new(None)),
            title_prompt: Arc::new(std::sync::RwLock::new(None)),
            abstention_prompt: Arc::new(std::sync::RwLock::new(None)),
            // ADR-071 D7: distiller prompt overrides — same lazy-fill
            // pattern, populated in Phase B / cli.rs standalone path and
            // hot-reloadable via `reload_prompts_into_core`.
            distiller_merge_prompt: Arc::new(std::sync::RwLock::new(None)),
            memory_provider: None,
            memory_admin: None,
            sqlite_store: None,
            rag_provider: None,
            memory_session: None,
            debug_observer: observer,
            pending_debug_handles: None,
            approval_gate: None,
            shell_approval_threshold,
            shell_risk_rules: crate::security::shell_risk::ShellRiskRules::load(
                std::path::Path::new(&config.work_dir),
            )
            .unwrap_or_default(),
            // Bound by Phase B via `set_git_nudge` once the MQTT client
            // slot and the workspace resolver are both available.
            git_nudge: None,
            embedding_provider: None,
            embedding_provider_shared: Arc::new(std::sync::RwLock::new(None)),
            metrics_aggregator: Arc::new(std::sync::Mutex::new(
                crate::memory::RetrievalMetricsAggregator::with_defaults(1.0),
            )),
            consolidation_bg_task: None,
            consolidation_timer: None,
            // ADR-046: blob store slot is empty by default; Phase B
            // populates it via `set_attachment_service` once the work_dir
            // is known. Until then, multimodal image parts cannot be
            // derived — chat messages fall back to plain text.
            attachment_service: None,
            // ADR-028: counters start at 0; the next `list_sessions` scan
            // rebuilds the baseline via `merge_token_totals`.
            agent_total_input_tokens: Arc::new(AtomicU64::new(0)),
            agent_total_output_tokens: Arc::new(AtomicU64::new(0)),

            // ADR-066: cache counters follow the same lifecycle.
            agent_total_cache_read_tokens: Arc::new(AtomicU64::new(0)),
            agent_total_cache_write_tokens: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn new(
        config: RuntimeConfig,
        manifest: acowork_core::AgentManifest,
        provider: Arc<dyn Provider>,
        builtin_tools: Vec<BuiltinToolEntry>,
    ) -> Self {
        Self::new_with_observer(
            config,
            manifest,
            provider,
            builtin_tools,
            DebugObserverSlot::production(),
        )
    }

    /// Rebuild `all_tools` from the current `builtin_tools` (filtered by
    /// `enabled`) plus `mcp_tools`. Called whenever either set changes:
    /// - Agent startup (after `builtin_tools` is initialized with flags)
    /// - MCP connect/disconnect
    /// - `RuntimeConfigUpdate.builtin_tools_enabled` toggle
    ///   (tool-result compression is retired, so there is no
    ///   platform-tool hot-reload path anymore)
    pub(crate) fn rebuild_all_tools(&mut self) {
        let mut merged: Vec<Arc<dyn Tool>> = self
            .builtin_tools
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.tool.clone())
            .collect();
        if let Some(ref mcp) = self.mcp_tools {
            merged.extend(mcp.clone());
        }
        self.all_tools = merged;
    }

    /// Rewrite `builtin_tools` enabled flags from a desired
    /// `(name, enabled)` list - the **single policy** behind every
    /// builtin-tools enabled write path (SessionTask hot-update via
    /// `apply_builtin_tools_update`, SessionManager template sync via
    /// `apply_builtin_tools_enabled`).
    ///
    /// Policy (all of it delegated to
    /// [`crate::agent_config::apply_builtin_tools_patch`] with an empty
    /// patch, so the persistence layer and the in-memory layer can
    /// never diverge):
    /// - Names not currently registered are dropped (defensive against
    ///   drift between the persisted file and the code registry).
    /// - Only the `enabled` flag is rewritten; the wrapped `tool` impl
    ///   (security decorators applied during `ToolRegistry::activate`)
    ///   is preserved.
    ///
    /// Does NOT rebuild `all_tools` / `ContextBuilder.tool_definitions`;
    /// dependent refreshes are owned by the caller (one refresh site
    /// per layer - see `session_task::refresh_builtin_tools_dependents`).
    pub(crate) fn apply_builtin_enabled_entries(
        &mut self,
        entries: &[crate::agent_config::AgentToolEntry],
    ) {
        let resolved = crate::agent_config::apply_builtin_tools_patch(entries, &[]);
        let resolved_map: std::collections::HashMap<String, bool> = resolved
            .iter()
            .map(|e| (e.name.clone(), e.enabled))
            .collect();
        for entry in self.builtin_tools.iter_mut() {
            let name = entry.name().to_string();
            if let Some(&enabled) = resolved_map.get(&name) {
                entry.enabled = enabled;
            }
        }
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
    pub fn manifest(&self) -> &acowork_core::AgentManifest {
        &self.manifest
    }
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    // ── ADR-028: Agent-scoped cumulative token usage ───────────────────
    //
    // Counter semantics:
    // - `accumulate_llm_usage` adds a fresh LLM call's token usage to the
    //   in-process totals.
    // - `merge_token_totals` performs an atomic-max merge with a scanned
    //   total (summed from every on-disk `SessionMeta.tokens`). This both
    //   (a) recovers the historical baseline on first session-list scan
    //   after startup, and (b) reconciles any on-disk writers that updated
    //   `SessionTokens` behind us (defence in depth).
    // - `agent_token_totals` snapshots both counters for the
    //   `ContextUsageInfo` push.
    //
    // ADR-066: the four-tuple is `(input, output, cache_read,
    // cache_write)` — the cache dimensions mirror the Provider-reported
    // `UsageInfo` cache fields and the persisted `SessionTokens`
    // totals.
    //
    // Not persisted: a process restart zeroes the counters, and the
    // next `list_sessions` fetch rebuilds the baseline via scan + merge.
    // See ADR-028 §"Why no startup seed" for the rationale.

    /// Add an LLM call's token usage to the running agent-scoped totals.
    ///
    /// Mirrors [`crate::conversation::ConversationSession::accumulate_llm_usage`]:
    /// a zero-input call is still recorded (the LLM did happen), but its
    /// `prompt_tokens` contribution is skipped to avoid masking a reliable
    /// baseline with a Provider-fallback zero ("宁可 miss 也不估计").
    ///
    /// ADR-066: `cache_read_tokens` follows the same zero-skip gate as
    /// `prompt_tokens` (a reliable `cache_read` requires a reliable
    /// input).  `cache_write_tokens` is always accumulated — Anthropic
    /// cache writes are real, billable events independent of the
    /// regular input count.
    pub fn accumulate_llm_usage(&self, usage: &UsageInfo) {
        if usage.prompt_tokens > 0 {
            self.agent_total_input_tokens
                .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.saturating_add(usage.prompt_tokens))
                })
                .ok();
            // ADR-066: cache_read shares the zero-skip gate.
            self.agent_total_cache_read_tokens
                .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.saturating_add(usage.cache_read_tokens))
                })
                .ok();
        }
        self.agent_total_output_tokens
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(usage.completion_tokens))
            })
            .ok();
        // ADR-066: cache_write always accumulates (no zero-skip gate).
        self.agent_total_cache_write_tokens
            .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(usage.cache_write_tokens))
            })
            .ok();
    }

    /// Atomically merge a freshly-scanned total into the in-process counter
    /// using `max(counter, scanned)`.
    ///
    /// Idempotent: calling with the same `(in, out, cache_read,
    /// cache_write)` tuple repeatedly is a no-op once the counters have
    /// been raised. Race-safe with concurrent `accumulate_llm_usage`
    /// calls — whichever side sees the larger value wins, so a slow
    /// scan followed by a fresh LLM call won't accidentally downgrade
    /// the counter to the (stale) scan result.
    ///
    /// Pass `None` for any dimension whose scan yielded no data
    /// (e.g. agents with no persisted sessions yet, or legacy meta
    /// files without cache fields).
    pub fn merge_token_totals(
        &self,
        scanned: (Option<u64>, Option<u64>, Option<u64>, Option<u64>),
    ) {
        if let Some(inp) = scanned.0 {
            self.agent_total_input_tokens
                .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.max(inp))
                })
                .ok();
        }
        if let Some(out) = scanned.1 {
            self.agent_total_output_tokens
                .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.max(out))
                })
                .ok();
        }
        // ADR-066: cache_read + cache_write follow the same atomic-max
        // merge pattern.
        if let Some(cache_read) = scanned.2 {
            self.agent_total_cache_read_tokens
                .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.max(cache_read))
                })
                .ok();
        }
        if let Some(cache_write) = scanned.3 {
            self.agent_total_cache_write_tokens
                .try_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                    Some(cur.max(cache_write))
                })
                .ok();
        }
    }

    /// Snapshot the current agent-scoped cumulative token totals.
    ///
    /// ADR-066: returns `(input_tokens, output_tokens, cache_read_tokens,
    /// cache_write_tokens)`. Always present (zero is a valid baseline
    /// before the first LLM call). Callers embed these in
    /// `ContextUsageInfo` so the frontend can show a "agent-total"
    /// line in the Results Panel before any per-session cumulative
    /// figure exists.
    pub fn agent_token_totals(&self) -> (u64, u64, u64, u64) {
        (
            self.agent_total_input_tokens.load(Ordering::Acquire),
            self.agent_total_output_tokens.load(Ordering::Acquire),
            self.agent_total_cache_read_tokens.load(Ordering::Acquire),
            self.agent_total_cache_write_tokens.load(Ordering::Acquire),
        )
    }

    pub fn gateway_model_capabilities(&self) -> HashMap<String, ModelCapabilitiesInfo> {
        let list = self.global_provider_list.read().unwrap();
        let mut map = HashMap::new();
        for provider in list.iter() {
            for model in &provider.models {
                map.insert(model.id.clone(), model.capabilities.clone());
            }
        }
        map
    }

    pub fn max_output_tokens_limit_for_model(&self, model_id: &str) -> u64 {
        let list = self.global_provider_list.read().unwrap();
        for provider in list.iter() {
            for model in &provider.models {
                if model.id == model_id {
                    return model.max_output_tokens_limit;
                }
            }
        }
        16_384
    }

    pub fn update_provider(&mut self, new_provider: Arc<dyn Provider>, model: String) {
        let old_name = self.provider.name().to_string();
        self.provider = new_provider;
        tracing::info!(
            old_provider = %old_name,
            new_provider = %self.provider.name(),
            model = %model,
            "LLM provider updated at runtime (model_switch)"
        );
    }

    pub fn update_embedding_provider(&mut self, new_provider: Arc<dyn EmbeddingProvider>) {
        let old_name = self
            .embedding_provider
            .as_ref()
            .map(|p| p.name())
            .unwrap_or("none")
            .to_string();
        let new_name = new_provider.name().to_string();
        let new_dim = new_provider.dimension();
        if let Ok(mut cell) = self.embedding_provider_shared.write() {
            *cell = Some(new_provider.clone());
        }
        self.embedding_provider = Some(new_provider);
        tracing::info!(
            old_provider = %old_name,
            new_provider = %new_name,
            new_dimension = new_dim,
            "Embedding provider updated"
        );
    }

    /// The provider every clone of this core currently agrees on.
    ///
    /// Readers that outlive a single clone - the conversation indexer, the HTTP
    /// stats endpoint - must go through this rather than the
    /// [`Self::embedding_provider`] field, which is per-clone.
    pub(crate) fn live_embedding_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedding_provider_shared
            .read()
            .ok()
            .and_then(|cell| cell.clone())
    }

    /// Clear the embedding provider (set to `None`).
    ///
    /// Called when the embed sidecar goes down (SidecarEndpointUpdate with
    /// empty endpoint). The memory store should gracefully degrade -
    /// embedding-dependent operations will fail until a new provider is
    /// pushed (ADR-030 review ISSUE-2 fix).
    pub fn clear_embedding_provider(&mut self) {
        let old_name = self
            .embedding_provider
            .as_ref()
            .map(|p| p.name())
            .unwrap_or("none")
            .to_string();
        if let Ok(mut cell) = self.embedding_provider_shared.write() {
            *cell = None;
        }
        self.embedding_provider = None;
        tracing::info!(
            old_provider = %old_name,
            "Embedding provider cleared (embed sidecar unavailable)"
        );
    }

    pub fn update_gateway_model_capabilities(
        &mut self,
        model_id: &str,
        caps: ModelCapabilitiesInfo,
    ) {
        tracing::info!(
            model = %model_id,
            context_window = caps.context_window,
            max_output_tokens = caps.max_output_tokens,
            supports_tool_calling = caps.supports_tool_calling,
            supports_reasoning = ?caps.supports_reasoning,
            cost = ?caps.cost.as_ref().map(|c| (c.input_per_million, c.output_per_million)),
            caps_name = ?caps.name,
            source = "gateway",
            "AgentCore received model capabilities from Gateway"
        );
        let mut list = self.global_provider_list.write().unwrap();
        for provider in list.iter_mut() {
            for model in provider.models.iter_mut() {
                if model.id == model_id {
                    model.capabilities = caps;
                    return;
                }
            }
        }
    }

    pub fn update_max_output_tokens_limit(&mut self, limit: u64) {
        tracing::info!(
            new_limit = limit,
            "AgentCore max_output_tokens_limit updated from Gateway (all models)"
        );
        let mut list = self.global_provider_list.write().unwrap();
        for provider in list.iter_mut() {
            for model in provider.models.iter_mut() {
                model.max_output_tokens_limit = limit;
            }
        }
    }

    pub fn apply_runtime_config(&mut self, overrides: &RuntimeConfigOverrides) {
        if let Some(limit) = overrides.max_output_tokens {
            tracing::info!(
                new = limit,
                "runtime config: max_output_tokens updated (all models)"
            );
            self.update_max_output_tokens_limit(limit);
        }
        if let Some(n) = overrides.max_iterations {
            tracing::info!(
                old = self.config.max_iterations,
                new = n,
                "runtime config: max_iterations updated"
            );
            self.config.max_iterations = n;
        }
        if let Some(temp) = overrides.temperature {
            tracing::info!(
                old = ?self.temperature_override,
                new = temp,
                "runtime config: temperature updated"
            );
            self.temperature_override = Some(temp);
        }
        if let Some(cw) = overrides.context_window {
            tracing::info!(
                old = ?self.context_window_override,
                new = cw,
                "runtime config: context_window updated"
            );
            self.context_window_override = Some(cw);
        }
        if overrides.system_prompt_override.is_some() {
            tracing::info!(
                has_override = overrides
                    .system_prompt_override
                    .as_ref()
                    .map(|s| !s.is_empty())
                    .unwrap_or(false),
                "runtime config: system_prompt_override updated"
            );
            self.system_prompt_override = overrides.system_prompt_override.clone();
        }
        if overrides.session_language.is_some() {
            tracing::info!(
                old = ?self.session_language_override,
                new = ?overrides.session_language,
                "runtime config: session_language updated"
            );
            self.session_language_override = overrides.session_language.clone();
        }
        if let Some(ref threshold) = overrides.shell_approval_threshold {
            let new_threshold =
                ShellApprovalThreshold::from_str_loose(threshold).unwrap_or_default();
            tracing::info!(
                old = ?self.shell_approval_threshold,
                new = ?new_threshold,
                "runtime config: shell_approval_threshold updated"
            );
            self.shell_approval_threshold = new_threshold;
        }
        if let Some(timeout) = overrides.approval_timeout_secs {
            tracing::info!(
                old = ?self.approval_timeout_secs,
                new = timeout,
                "runtime config: approval_timeout_secs updated"
            );
            self.approval_timeout_secs = Some(timeout);
        }
        if let Some(threshold) = overrides.compression_ratio_threshold {
            tracing::info!(
                old = ?self.compression_ratio_threshold,
                new = threshold,
                "runtime config: compression_ratio_threshold updated"
            );
            self.compression_ratio_threshold = Some(threshold);
        }
        // ADR-071 D4/D6: distiller runtime settings. Each `Some` overwrites
        // the stored value; `None` (field absent on the wire) leaves the
        // previous runtime value alone — matching the global agent_config.json
        // partial-PUT semantics used by every other live-editable field.
        // When anything changed and the consolidation pipeline is already
        // running, hot-swap the scheduler policy on the shared timer
        // (ADR-071 D8: RwLock config, takes effect on the next loop tick —
        // no background-task teardown needed).
        let mut distiller_changed = false;
        if let Some(v) = overrides.distiller_enabled
            && self.distiller_runtime.enabled != Some(v)
        {
            self.distiller_runtime.enabled = Some(v);
            distiller_changed = true;
        }
        if let Some(ref v) = overrides.distiller_models
            && self.distiller_runtime.models != *v
        {
            self.distiller_runtime.models = v.clone();
            distiller_changed = true;
        }
        if let Some(v) = overrides.distiller_interval_minutes
            && self.distiller_runtime.interval_minutes != Some(v)
        {
            self.distiller_runtime.interval_minutes = Some(v);
            distiller_changed = true;
        }
        if let Some(v) = overrides.distiller_accumulation_threshold
            && self.distiller_runtime.accumulation_threshold != Some(v)
        {
            self.distiller_runtime.accumulation_threshold = Some(v);
            distiller_changed = true;
        }
        if let Some(v) = overrides.distiller_idle_minutes
            && self.distiller_runtime.idle_minutes != Some(v)
        {
            self.distiller_runtime.idle_minutes = Some(v);
            distiller_changed = true;
        }
        if distiller_changed {
            tracing::info!(
                enabled = ?self.distiller_runtime.enabled,
                models = ?self.distiller_runtime.models,
                interval_minutes = ?self.distiller_runtime.interval_minutes,
                accumulation_threshold = ?self.distiller_runtime.accumulation_threshold,
                idle_minutes = ?self.distiller_runtime.idle_minutes,
                "runtime config: distiller settings updated (ADR-071 D6) — rebuilding consolidation pipeline if running"
            );
            self.update_consolidation_scheduler_config_if_running();
        }
        // Memory forgetting runtime settings. Each `Some` overwrites the
        // stored value; `None` leaves the previous runtime value alone —
        // matching the distiller partial-PUT semantics above. When anything
        // changed and the consolidation pipeline is running, hot-swap the
        // scheduler policy (the forgetting scan gate re-reads config each
        // tick, so the change takes effect on the next poll).
        let mut forgetting_changed = false;
        if let Some(v) = overrides.memory_forgetting_enabled
            && self.forgetting_runtime.enabled != Some(v)
        {
            self.forgetting_runtime.enabled = Some(v);
            forgetting_changed = true;
        }
        if let Some(v) = overrides.memory_forgetting_half_life_days
            && self.forgetting_runtime.half_life_days != Some(v)
        {
            self.forgetting_runtime.half_life_days = Some(v);
            forgetting_changed = true;
        }
        if let Some(v) = overrides.memory_forgetting_dormant_threshold
            && self.forgetting_runtime.dormant_threshold != Some(v)
        {
            self.forgetting_runtime.dormant_threshold = Some(v);
            forgetting_changed = true;
        }
        if let Some(v) = overrides.memory_forgetting_archive_days
            && self.forgetting_runtime.archive_days != Some(v)
        {
            self.forgetting_runtime.archive_days = Some(v);
            forgetting_changed = true;
        }
        if forgetting_changed {
            tracing::info!(
                enabled = ?self.forgetting_runtime.enabled,
                half_life_days = ?self.forgetting_runtime.half_life_days,
                dormant_threshold = ?self.forgetting_runtime.dormant_threshold,
                archive_days = ?self.forgetting_runtime.archive_days,
                "runtime config: memory forgetting settings updated — rebuilding consolidation pipeline if running"
            );
            self.update_consolidation_scheduler_config_if_running();
            // ADR-057 §5.3 redesign: `memory_recall` reads its
            // MemoryManagerConfig from the shared MemorySessionHandle
            // (auto-inject builds a fresh manager each turn, so it was
            // already live after the core update above). Refresh the handle
            // so the retrieval-side time-decay takes effect for in-flight
            // sessions immediately instead of at the next runtime restart.
            if let Some(session) = &self.memory_session {
                session.set_memory_config(self.memory_manager_config());
            }
        }
    }

    pub fn init_memory_provider(&mut self, work_dir: &std::path::Path) {
        if self.memory_provider.is_some() {
            tracing::debug!("init_memory_provider: already initialized, skipping");
            return;
        }
        let memory_dir = work_dir.join(acowork_core::workspace::MEMORY_DIR);
        if let Err(e) = std::fs::create_dir_all(&memory_dir) {
            tracing::warn!(error = %e, dir = %memory_dir.display(), "Failed to create memory directory, memory features disabled");
            return;
        }
        self.init_sqlite_backend(work_dir);
    }

    /// Create and initialise the workspace store as the memory provider
    /// (ADR-082 D1).
    fn init_sqlite_backend(&mut self, work_dir: &std::path::Path) {
        let db_path = acowork_core::workspace::store_path(work_dir);
        let embedding_dim = self.memory_embedding_dim();
        match acowork_sqlite::SqliteStore::open(&db_path, embedding_dim) {
            Ok(store) => {
                let store = std::sync::Arc::new(store);
                // ADR-082 §4 step 3: the same .sqlite file also carries
                // session meta. Install it as the process backend.
                self.install_session_meta_backend(store.clone(), work_dir);
                // Keep a handle so the conversation index shares this
                // connection instead of opening a second one.
                self.sqlite_store = Some(store.clone());
                self.install_memory_backend(store, &db_path)
            }
            Err(e) => tracing::warn!(
                error = %e,
                path = %db_path.display(),
                "Failed to open SQLite memory store, memory features disabled"
            ),
        }
    }

    /// Route session-meta storage at the SQLite store (ADR-082 §4 step 3).
    ///
    /// The store is registered before the session manager starts, so every
    /// later read/write goes to SQLite.
    fn install_session_meta_backend(
        &self,
        store: std::sync::Arc<acowork_sqlite::SqliteStore>,
        work_dir: &std::path::Path,
    ) {
        let sm_store = std::sync::Arc::new(acowork_sqlite::SqliteSessionMetaStore::new(store));
        crate::conversation::install_session_meta_backend(
            &work_dir.join("conversations"),
            sm_store,
        );
    }

    /// Expected embedding width, warning when the provider is unavailable.
    fn memory_embedding_dim(&self) -> usize {
        self.embedding_provider
            .as_ref()
            .map(|p| p.dimension())
            .unwrap_or_else(|| {
                let default_dim = acowork_memory::types::DEFAULT_EMBEDDING_DIM;
                tracing::warn!(
                    default_dim,
                    "⚠️ Embedding provider unavailable - opening the memory store with the                      default dim {}. If the on-disk store was created with a different dim,                      vector search will fail and memory will fall back to text-only search.                      Restart runtime after the embedding service is back online to use vector                      search.",
                    default_dim
                );
                default_dim
            })
    }

    /// Wire an opened store into the session, admin handle and consolidation
    /// pipeline.
    ///
    /// Generic over the concrete store so the backend choice never leaks past
    /// this function — every memory consumer downstream already holds a trait
    /// object (ADR-051).
    fn install_memory_backend<T>(&mut self, store: std::sync::Arc<T>, db_path: &std::path::Path)
    where
        T: acowork_memory::MemoryProvider + acowork_memory::MemoryAdminService + 'static,
    {
        // Backend-agnostic node count: the trait reports it, so this does not
        // reach into engine internals any more.
        let existing = store
            .stats()
            .map(|stats| stats.episode_count + stats.node_count)
            .unwrap_or(0);
        tracing::info!(path = %db_path.display(), existing_nodes = existing, "Memory store opened");

        let quality = self.memory_quality_config();
        if let Err(e) = store.apply_quality_config(&quality) {
            tracing::warn!(error = %e, "Failed to apply memory quality config, using defaults");
        }

        self.bootstrap_autobiographical_from_manifest(&*store);
        if let Some(ref session) = self.memory_session {
            session.set_provider(store.clone());
            // Config consistency: the memory_recall tool reads the
            // SAME MemoryManagerConfig as auto-inject (ADR-062 M5),
            // so per-agent quality settings apply to both paths.
            session.set_memory_config(self.memory_manager_config());
        }
        self.memory_admin = Some(store.clone());
        self.memory_provider = Some(store);
        self.start_consolidation_pipeline();
    }

    pub fn memory_provider(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.memory_provider.as_ref()
    }

    /// The open workspace store (ADR-082 §4).
    ///
    /// Consumed by the conversation-index bootstrap so the index shares the
    /// memory file and connection.
    pub fn sqlite_store(&self) -> Option<Arc<acowork_sqlite::SqliteStore>> {
        self.sqlite_store.clone()
    }

    /// Admin service for HTTP memory endpoints and embedding migration.
    /// ADR-051 P4: Replaces the former `grafeo_store()` accessor.
    pub fn memory_admin(&self) -> Option<&Arc<dyn MemoryAdminService>> {
        self.memory_admin.as_ref()
    }

    /// Backward-compat alias for memory_provider(). ADR-051 C3.
    pub fn memory_store(&self) -> Option<&Arc<dyn MemoryProvider>> {
        self.memory_provider.as_ref()
    }

    fn bootstrap_autobiographical_from_manifest(&self, provider: &dyn MemoryProvider) {
        // ADR-068 M8 — bootstrap scope is intentionally narrow (Identity +
        // Capability only; see module docs). The logic lives in the free
        // function `bootstrap_autobio::bootstrap_autobiographical_from_manifest`
        // so the e2e suite can drive it without a full AgentCore (ADR-068 E1).
        let outcome = crate::agent::bootstrap_autobio::bootstrap_autobiographical_from_manifest(
            &self.manifest,
            provider,
        );
        tracing::debug!(?outcome, "Manifest autobiographical bootstrap finished");
    }

    /// Resolve the agent's `MemoryManagerConfig` (manifest overrides +
    /// defaults).
    ///
    /// Shared by `init_memory_manager` (auto-inject path) and the
    /// `memory_recall` tool (via `MemorySessionHandle`) so both retrieval
    /// paths use identical quality settings (ADR-062 M5).
    pub(crate) fn memory_manager_config(&self) -> MemoryManagerConfig {
        // auto-inject defaults OFF: the LLM recalls memories explicitly via
        // the `memory_recall` tool (same user-message query), so first-turn
        // injection would duplicate already-present context. Per-agent
        // opt-in via `[memory.quality].auto_inject_enabled = true`.
        // The override is read straight from the manifest — it is a
        // `MemoryManagerConfig` field, not part of `MemoryQualityConfig`.
        let manifest_auto_inject = self
            .manifest
            .memory
            .quality
            .as_ref()
            .and_then(|q| q.auto_inject_enabled);
        MemoryManagerConfig {
            quality: self.memory_quality_config(),
            auto_inject_enabled: manifest_auto_inject.unwrap_or(false),
            // Retrieval-side forgetting: runtime agent_config.json layer
            // (forgetting_enabled / half_life_days) → system defaults.
            // Reuses the scan-side `EpisodicDecayConfig` so both paths share
            // one type + one retention curve (ADR-057 §5.3 redesign).
            forgetting: EpisodicDecayConfig {
                enabled: self.forgetting_runtime.enabled.unwrap_or(false),
                half_life_days: self
                    .forgetting_runtime
                    .half_life_days
                    .unwrap_or_else(|| EpisodicDecayConfig::default().half_life_days),
                ..EpisodicDecayConfig::default()
            },
            ..MemoryManagerConfig::default()
        }
    }

    pub fn init_memory_manager(&self) -> MemoryManager {
        MemoryManager::new(self.memory_manager_config())
    }

    /// Resolve the agent's memory quality config (ADR-062 D2).
    ///
    /// Source: the `.agent` manifest `[memory.quality]` section (package
    /// author default). An absent section yields `MemoryQualityConfig::default()`
    /// ("zero configuration = current behaviour").
    pub(crate) fn memory_quality_config(&self) -> acowork_memory::quality::MemoryQualityConfig {
        self.manifest
            .memory
            .quality
            .clone()
            .map(Into::into)
            .unwrap_or_default()
    }

    /// Resolve the EpisodicDistiller config from the agent manifest
    /// `[memory.distiller]` section (ADR-068 M4/M7).
    ///
    /// Returns `Some(config)` only when the section is present; fields that
    /// are absent keep the `DistillerConfig` defaults. The caller gates the
    /// step itself on `enabled`.
    pub(crate) fn distiller_config(
        &self,
    ) -> Option<acowork_memory::consolidation::DistillerConfig> {
        self.manifest
            .memory
            .distiller
            .as_ref()
            .and_then(manifest_distiller_to_config)
    }

    /// ADR-071 D4/D6: build the effective distiller scheduler policy from
    /// the runtime `agent_config.json` layer (Layer 1) over the manifest
    /// `[memory.distiller]` section (Layer 2), falling back to
    /// [`SchedulerConfig`] defaults for any field both layers leave unset.
    ///
    /// The `[memory.distiller]` manifest section already supplies
    /// interval / accumulation / idle fields (ADR-071 D1 additions), so the
    /// merge keeps the per-agent package defaults while letting the Desktop
    /// memory panel override them at runtime without an agent restart.
    pub(crate) fn distiller_scheduler_config(&self) -> SchedulerConfig {
        let manifest = self.manifest.memory.distiller.as_ref();
        // Minutes → seconds (SchedulerConfig uses seconds internally).
        let minutes_to_secs = |v: Option<u64>| v.map(|m| m.saturating_mul(60));
        let default = SchedulerConfig::default();
        // ADR-071 D7: the per-agent prompt override (distiller-merge.md) is
        // layered on top of the manifest-derived DistillerConfig. A package
        // without the `[memory.distiller]` section still gets a
        // `Some(config)` when an override file exists, so the distiller never
        // silently uses built-in prompts the package author replaced.
        let mut distiller_config = self.distiller_config();
        let merge_override = self.distiller_merge_prompt();
        if merge_override.is_some() {
            let mut cfg = distiller_config.take().unwrap_or_default();
            cfg.merge_prompt_override = merge_override;
            distiller_config = Some(cfg);
        }
        SchedulerConfig {
            distiller_enabled: self
                .distiller_runtime
                .enabled
                .unwrap_or_else(|| self.manifest.memory.distiller_enabled()),
            distiller_config,
            distiller_interval_secs: minutes_to_secs(self.distiller_runtime.interval_minutes)
                .or_else(|| manifest.and_then(|m| m.interval_minutes.map(|v| v.saturating_mul(60))))
                .unwrap_or(default.distiller_interval_secs),
            distiller_accumulation: self
                .distiller_runtime
                .accumulation_threshold
                .or_else(|| manifest.and_then(|m| m.accumulation_threshold))
                .unwrap_or(default.distiller_accumulation),
            distiller_idle_secs: minutes_to_secs(self.distiller_runtime.idle_minutes)
                .or_else(|| manifest.and_then(|m| m.idle_minutes.map(|v| v.saturating_mul(60))))
                .unwrap_or(default.distiller_idle_secs),
            // Episodic forgetting: Layer 1 runtime overrides (agent_config.json)
            // → system defaults (no manifest section — forgetting is a
            // runtime-only, opt-in feature).
            forgetting_enabled: self
                .forgetting_runtime
                .enabled
                .unwrap_or(default.forgetting_enabled),
            forgetting_half_life_days: self
                .forgetting_runtime
                .half_life_days
                .unwrap_or(default.forgetting_half_life_days),
            forgetting_dormant_threshold: self
                .forgetting_runtime
                .dormant_threshold
                .unwrap_or(default.forgetting_dormant_threshold),
            forgetting_archive_days: self
                .forgetting_runtime
                .archive_days
                .unwrap_or(default.forgetting_archive_days),
            ..default
        }
    }

    /// ADR-071 D6: apply a live distiller config change to the running
    /// consolidation pipeline without restarting the background task.
    ///
    /// The shared `ConsolidationTimer` holds the scheduler policy in an
    /// internal `RwLock`; swapping the value takes effect on the next loop
    /// tick (≤60s). The timer's idle/backlog/last-run state is preserved, so
    /// a config change never spuriously fires (or delays) a distillation.
    pub(crate) fn update_consolidation_scheduler_config_if_running(&self) {
        let Some(timer) = self.consolidation_timer.as_ref() else {
            tracing::debug!(
                "Distiller config changed but consolidation timer not running (pipeline not started yet)"
            );
            return;
        };
        timer.update_config(self.distiller_scheduler_config());
    }

    /// ADR-071 D4/D5 (+ list follow-up): resolve the ordered distiller model
    /// candidate list used by the EpisodicDistiller.
    ///
    /// Chain (mirrors the summary/distill model selection used by
    /// `resolve_distill_model` in the session layer):
    ///   1. `agent_config.json` `distiller_models` (runtime, Layer 1) — list
    ///   2. manifest `[memory.distiller].model_*` (Layer 2) — single, prepended
    ///   3. global `default_compact_models` (ADR-056) — list
    ///   4. first model in the provider list (legacy fallback)
    ///
    /// Entries are `(provider_id, model_id)`; an empty `provider_id` means
    /// "the agent's active provider" (manifest pairs and the legacy fallback
    /// historically carried only a model id). `ProviderLlmAdapter` walks the
    /// list in order, rebuilding a provider only for cross-provider
    /// candidates.
    pub(crate) fn resolve_distiller_targets(&self) -> Vec<acowork_core::protocol::CompactModelRef> {
        use acowork_core::protocol::CompactModelRef;
        let mut out: Vec<CompactModelRef> = Vec::new();
        let push = |pid: String, mid: String, out: &mut Vec<CompactModelRef>| {
            if !out
                .iter()
                .any(|r| r.provider_id == pid && r.model_id == mid)
            {
                out.push(CompactModelRef {
                    provider_id: pid,
                    model_id: mid,
                });
            }
        };
        // Layer 1: agent_config.json (runtime memory-panel choice, ordered).
        for m in &self.distiller_runtime.models {
            push(m.provider_id.clone(), m.model_id.clone(), &mut out);
        }
        // Layer 2: manifest [memory.distiller] (single author-declared pair;
        // empty/absent provider = active provider, same as before).
        if let Some(d) = self.manifest.memory.distiller.as_ref()
            && let Some(mid) = d.model_id.as_ref()
        {
            push(
                d.model_provider_id.clone().unwrap_or_default(),
                mid.clone(),
                &mut out,
            );
        }
        // Layer 3: global compact-model candidate list (Harness pick).
        for (pid, mid) in self.default_compact_models.read().unwrap().iter() {
            push(pid.clone(), mid.clone(), &mut out);
        }
        // Layer 4: first model in the provider list (legacy convention —
        // model id against the active provider).
        if out.is_empty() {
            let list = self.global_provider_list.read().unwrap();
            let first = list.iter().flat_map(|p| p.models.iter()).next();
            match first {
                Some(m) => push(String::new(), m.id.clone(), &mut out),
                None => push(String::new(), "default".to_string(), &mut out),
            }
        }
        out
    }

    /// Build the distiller LLM adapter from [`Self::resolve_distiller_targets`]:
    /// one [`DistillCandidate`] per target, with a per-candidate provider
    /// (cross-provider targets rebuild their own `Provider`, mirroring
    /// `SessionCore::build_provider_for` minus the session retry-UX wiring).
    /// Targets whose provider cannot be built are skipped; if nothing
    /// survives, the active provider is used so the adapter always has ≥1
    /// candidate.
    pub(crate) fn build_distiller_llm(&self) -> Arc<crate::memory::llm_adapter::ProviderLlmAdapter> {
        use crate::memory::llm_adapter::{DistillCandidate, ProviderLlmAdapter};
        let targets = self.resolve_distiller_targets();
        let mut candidates: Vec<DistillCandidate> = Vec::with_capacity(targets.len());
        for t in targets {
            let provider = if t.provider_id.is_empty() {
                self.provider.clone()
            } else {
                match self.build_compact_provider(&t.provider_id) {
                    Some(p) => p,
                    None => {
                        tracing::warn!(
                            provider_id = %t.provider_id,
                            model_id = %t.model_id,
                            "Distiller target provider unavailable, skipping candidate"
                        );
                        continue;
                    }
                }
            };
            candidates.push(DistillCandidate {
                provider,
                model: t.model_id,
            });
        }
        if candidates.is_empty() {
            candidates.push(DistillCandidate {
                provider: self.provider.clone(),
                model: "default".to_string(),
            });
        }
        Arc::new(ProviderLlmAdapter::with_candidates(candidates))
    }

    /// ADR-093: narrow `(Provider, model)` snapshot for the search-summary
    /// endpoint. Resolves the session's current model/provider pair
    /// ([`SessionManager::current_model_and_provider`] semantics live in
    /// the session layer; here we only rebuild the provider instance from
    /// the resolved id) and falls back to the agent's active provider when
    /// the id is unknown or absent. Reuses the same provider factory path
    /// as compaction — no duplicated model-priority rules in HTTP handlers.
    pub(crate) fn summary_llm_snapshot(
        &self,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> (Arc<dyn acowork_core::providers::traits::Provider>, String) {
        let provider = match provider_id {
            Some(pid) if !pid.is_empty() => self
                .build_compact_provider(pid)
                .unwrap_or_else(|| self.provider.clone()),
            _ => self.provider.clone(),
        };
        let model = model
            .filter(|m| !m.is_empty())
            .unwrap_or("default")
            .to_string();
        (provider, model)
    }

    /// ADR-093 follow-up: resolve the `(Provider, model)` to use for the
    /// `POST /search/summary` LLM call, in priority order:
    ///
    /// 1. **Global `default_compact_models` list** — same ordered list
    ///    consulted by episodic distillation / context compaction
    ///    (ADR-056). The first candidate whose provider is available
    ///    (`is_compact_provider_available`) AND whose model resolves in
    ///    the global capabilities map wins. Each skip is logged at
    ///    `debug`; the chosen tier logs at `info`.
    /// 2. **Session's current `(model, provider_id)`** — the fallback
    ///    that the original `summary_llm_snapshot` unconditionally used.
    ///    Always succeeds: even when the session has no model, the
    ///    provider defaults to the agent's active one and the model id
    ///    becomes `"default"`.
    ///
    /// Deliberately *not* running a context-window check (unlike
    /// `resolve_distill_model`): the summary prompt is bounded by
    /// `MAX_SUMMARY_CHARS` (a few hundred tokens), so a candidate that
    /// survived the capability lookup will trivially fit any model
    /// window. Adding the check would just be cargo-culted code.
    pub(crate) fn resolve_summary_llm(
        &self,
        current_provider_id: Option<&str>,
        current_model: Option<&str>,
    ) -> (
        Arc<dyn acowork_core::providers::traits::Provider>,
        String,
        SummaryModelTier,
    ) {
        // Level 1: global compact-model candidate list, in user order.
        // Same filtering contract as `resolve_distill_model` (provider
        // availability + capability lookup) but no context-window gate.
        let globals: Vec<(String, String)> =
            self.default_compact_models.read().unwrap().clone();
        for (pid, mid) in &globals {
            if !self.is_compact_provider_available(pid) {
                tracing::debug!(
                    provider_id = %pid,
                    model_id = %mid,
                    "summary: global compact candidate provider unavailable, skipping"
                );
                continue;
            }
            if self.get_model_capabilities(mid).is_none() {
                tracing::debug!(
                    model_id = %mid,
                    "summary: global compact candidate model not in capabilities, skipping"
                );
                continue;
            }
            if let Some(provider) = self.build_compact_provider(pid) {
                tracing::info!(
                    provider_id = %pid,
                    model_id = %mid,
                    tier = "global_default",
                    "summary: using global default compact model"
                );
                return (provider, mid.clone(), SummaryModelTier::GlobalDefault);
            }
            tracing::debug!(
                provider_id = %pid,
                "summary: global compact candidate provider factory returned None, skipping"
            );
        }
        // Level 2: session current (or agent default if session is
        // empty). This is the previous behaviour preserved verbatim.
        let (provider, model) =
            self.summary_llm_snapshot(current_provider_id, current_model);
        tracing::info!(
            provider_id = ?current_provider_id,
            model_id = %model,
            tier = "session_current",
            "summary: using session current model"
        );
        (provider, model, SummaryModelTier::SessionCurrent)
    }

    /// Build a standalone `Provider` for a compact/distill target id.
    /// Same construction as `SessionCore::build_provider_for` but without
    /// session retry-UX wiring (the background pipeline has no session to
    /// render retry state into).
    fn build_compact_provider(
        &self,
        provider_id: &str,
    ) -> Option<Arc<dyn acowork_core::providers::traits::Provider>> {
        let provider_meta = self
            .global_provider_list
            .read()
            .unwrap()
            .iter()
            .find(|p| p.id == provider_id)
            .cloned()?;
        let api_key = {
            let vault = self.provider_key_vault.read().unwrap();
            crate::agent::session_core::resolve_provider_key(
                vault.get(provider_id).map(|v| v.as_slice()),
                None,
            )
        };
        let timeouts =
            Some(crate::providers::router::ProviderTimeouts::from(&self.config));
        let wiring = crate::providers::router::ProviderWiring {
            provider_id: Some(provider_meta.id.clone()),
            compat_cache: self.compat_cache.clone(),
        };
        let raw = crate::providers::router::create_provider_with_wiring(
            &provider_meta.id,
            &provider_meta.protocol_type,
            api_key.as_deref(),
            if provider_meta.base_url.is_empty() {
                None
            } else {
                Some(&provider_meta.base_url)
            },
            timeouts,
            wiring,
        );
        let retry_config =
            crate::providers::reliable::RetryConfig::from(&self.config.timeouts.retry);
        Some(Arc::new(crate::providers::reliable::ReliableProvider::new(
            raw, retry_config,
        )))
    }

    /// ADR-071 D2: run one EpisodicDistiller pass on demand (manual trigger).
    ///
    /// Force semantics: bypasses the periodic gate (`ConsolidationTimer`
    /// interval / backlog / idle checks) and runs `run_episodic_distiller_step`
    /// immediately — the exact same execution path as the background loop.
    ///
    /// The effective `distiller_enabled` switch is STILL respected (opt-in,
    /// ADR-068 + ADR-071 D4 runtime layer): a manual trigger on a distiller
    /// that is explicitly disabled is refused instead of silently producing
    /// semantic-layer nodes. The UI only exposes the manual button while the
    /// distiller is enabled, and the endpoint defends the same invariant
    /// server-side.
    ///
    /// Returns `Ok(Some(result))` after a run and `Err(...)` when the
    /// distiller is disabled or the memory/embedding provider is unavailable.
    pub(crate) async fn run_episodic_distill_once(
        &self,
    ) -> Result<Option<acowork_memory::consolidation::DistillerResult>, String> {
        use crate::memory::consolidation_bg::run_episodic_distiller_step_once;
        use acowork_memory::consolidation::{SchedulerConfig, ConsolidationLlm};

        if !self.distiller_scheduler_config().distiller_enabled {
            return Err(
                "distiller is disabled (agent_config.json / manifest [memory.distiller].enabled = false)"
                    .to_string(),
            );
        }
        let provider = self
            .memory_provider
            .clone()
            .ok_or_else(|| "memory provider not initialized".to_string())?;
        let embedding = self
            .embedding_provider
            .clone()
            .ok_or_else(|| "embedding provider not initialized".to_string())?;
        let llm: Arc<dyn ConsolidationLlm> = self.build_distiller_llm();
        // Manual run uses the effective runtime-over-manifest distiller
        // parameters when present; defaults otherwise. Enabled is forced
        // true (the gate above already checked the effective switch).
        let scheduler_config = SchedulerConfig {
            distiller_enabled: true,
            ..self.distiller_scheduler_config()
        };
        let result =
            run_episodic_distiller_step_once(provider, llm, embedding, scheduler_config).await;
        // Manual runs also reset the interval gate so the periodic scheduler
        // does not immediately re-trigger right after a manual run, and store
        // the run summary for the status endpoint (ADR-071 D2).
        if let Some(ref timer) = self.consolidation_timer {
            match &result {
                Some(result) => {
                    let record = crate::memory::consolidation_bg::DistillRunRecord::from_result(
                        chrono::Utc::now(),
                        result,
                    );
                    timer.record_distill_result(&record).await;
                }
                None => {
                    // Record the failure, not just the timestamp: a run that
                    // died must be distinguishable from a run that had
                    // nothing to do.
                    timer
                        .record_distill_result(
                            &crate::memory::consolidation_bg::DistillRunRecord::from_error(
                                chrono::Utc::now(),
                                "distiller run failed (see logs)",
                                Default::default(),
                            ),
                        )
                        .await;
                }
            }
        }
        Ok(result)
    }

    pub fn start_consolidation_pipeline(&mut self) {
        let Some(ref provider) = self.memory_provider else {
            tracing::debug!("Cannot start consolidation: memory provider not initialized");
            return;
        };
        let Some(ref embedding) = self.embedding_provider else {
            tracing::warn!(
                "Cannot start consolidation pipeline: embedding provider not available. Background memory consolidation (generalization, conflict resolution) is disabled until embedding service is back."
            );
            return;
        };
        if self.consolidation_bg_task.is_some() {
            tracing::debug!("Consolidation pipeline already running");
            return;
        }
        use crate::memory::consolidation_bg::{ConsolidationParams, start_consolidation_pipeline};
        use std::time::Duration;
        let llm_adapter = self.build_distiller_llm();
        // ADR-068 M4/M7 + ADR-071 D4/D6: resolve the distiller switch and
        // scheduler parameters from the runtime `agent_config.json` layer
        // (memory-panel) over the manifest `[memory.distiller]` section.
        // Off-by-default (opt-in): an absent section / `enabled = false`
        // keeps the distiller disabled unless the runtime layer turns it on.
        let scheduler_config = self.distiller_scheduler_config();
        let params = ConsolidationParams {
            provider: provider.clone(),
            llm_adapter,
            embedding_provider: embedding.clone(),
            scheduler_config,
            poll_interval: Duration::from_secs(60),
            work_dir: Some(std::path::PathBuf::from(&self.config.work_dir)),
        };
        let (timer, bg_task) = start_consolidation_pipeline(params);
        self.consolidation_bg_task = Some(bg_task);
        self.consolidation_timer = Some(timer);
        tracing::info!("Consolidation background pipeline started");
    }

    pub async fn notify_consolidation_active(&self) {
        // ADR-051 P4: Reset the Runtime-internal ConsolidationTimer's idle
        // timer so consolidation doesn't run during active use.
        if let Some(ref timer) = self.consolidation_timer {
            timer.notify_active().await;
        }
    }

    pub(crate) fn get_model_capabilities(&self, model_name: &str) -> Option<ModelCapabilitiesInfo> {
        let list = self.global_provider_list.read().unwrap();
        for provider in list.iter() {
            for model in &provider.models {
                if model.id == model_name {
                    return Some(model.capabilities.clone());
                }
            }
        }
        if !list.is_empty() {
            let available: Vec<&str> = list
                .iter()
                .flat_map(|p| p.models.iter().map(|m| m.id.as_str()))
                .collect();
            tracing::warn!(model = %model_name, available = ?available, "Model capabilities not found for '{}'", model_name);
        }
        None
    }

    pub fn get_provider(&self, provider_id: &str) -> Option<ProviderListItem> {
        let list = self.global_provider_list.read().unwrap();
        list.iter().find(|p| p.id == provider_id).cloned()
    }

    /// First account's API key for a provider.
    ///
    /// Back-compat accessor for callers that predate multi-account support
    /// (e.g. `is_default_compact_provider_available`). Callers that know
    /// which account they want should use
    /// [`Self::get_provider_account_api_key`].
    pub fn get_provider_api_key(&self, provider_id: &str) -> Option<String> {
        self.get_provider_account_api_key(provider_id, None)
    }

    /// API key for a specific account of a provider.
    ///
    /// `account_id == None` falls back to the provider's first account
    /// (pre-multi-account behaviour). `Some(id)` that matches no account
    /// returns `None`; the `build_provider_for` path treats that as
    /// "account gone" and falls back to the first account.
    pub fn get_provider_account_api_key(
        &self,
        provider_id: &str,
        account_id: Option<&str>,
    ) -> Option<String> {
        let vault = self.provider_key_vault.read().unwrap();
        let accounts = vault.get(provider_id)?;
        match account_id {
            Some(aid) => accounts
                .iter()
                .find(|e| e.account_id == aid)
                .map(|e| e.api_key.clone()),
            None => accounts.first().map(|e| e.api_key.clone()),
        }
    }

    /// ADR-056: Whether the global default compact model's provider is
    /// actually usable right now. A provider is usable when either:
    ///
    ///   1. it has a non-empty API key in the in-memory vault (cloud
    ///      providers), or
    ///   2. it is a local provider — no key required, reachable via a
    ///      local base_url (Ollama native protocol, or any base_url
    ///      pointing at localhost / 127.0.0.1 / 0.0.0.0 / ::1).
    ///
    /// This is what `resolve_distill_model` consults before accepting a
    /// global candidate — without the local-provider branch, a user-chosen
    /// Ollama default (ADR-056 §2.3 "chat 用 deepseek,蒸馏用本地 qwen2.5:0.5b")
    /// would always be rejected and the feature would never fire.
    pub fn is_compact_provider_available(&self, pid: &str) -> bool {
        // 1) Cloud provider with a configured key.
        if self
            .get_provider_api_key(pid)
            .map(|k| !k.is_empty())
            .unwrap_or(false)
        {
            return true;
        }
        // 2) Local provider — no key required, but must still be present in
        //    the provider list with a reachable local base_url.
        let list = self.global_provider_list.read().unwrap();
        list.iter().find(|p| p.id == pid).is_some_and(|p| {
            matches!(p.protocol_type, ProtocolType::Ollama)
                || crate::providers::is_local_base_url(&p.base_url)
        })
    }

    pub fn set_debug_mode(&mut self, observer: crate::debug::DebugObserverImpl) {
        tracing::info!(
            is_dev = crate::debug::observer::DebugObserver::is_dev_mode(&observer),
            "AgentCore::set_debug_mode called (observer pipeline)"
        );
        self.debug_observer = DebugObserverSlot::dev(observer);
    }

    /// Tear down the DevMode observer on this `AgentCore`, restoring
    /// production-mode semantics. Symmetric counterpart to
    /// [`Self::set_debug_mode`] — used by the runtime
    /// `POST /api/debug/disable` flow (ADR-048 follow-up) to exit
    /// DevMode without an agent restart.
    ///
    /// After this call:
    /// - The `debug_observer` slot is `Production`; all `on_*` hooks
    ///   become no-ops at the compiler-discriminated level (same
    ///   zero-overhead guarantee as a never-enabled agent).
    /// - The pending-injection slot is also cleared, so any in-flight
    ///   rewind/patch dispatched before disable arrives cannot fire
    ///   after the toggle.
    pub fn clear_debug_mode(&mut self) {
        if !self.debug_observer.is_dev_mode() {
            tracing::debug!("AgentCore::clear_debug_mode: no-op (observer already in Production)");
            return;
        }
        tracing::info!("AgentCore::clear_debug_mode: dropping DevMode observer");
        self.debug_observer = DebugObserverSlot::production();
    }

    pub fn set_debug_pending_injection(
        &mut self,
        ch: Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>>,
    ) {
        // ADR-048: hold the bypass-injection channel at the AgentCore level.
        // The observer slot's `set_pending_injection` is a NO-OP while the
        // observer is `Production` (which is almost always the case here —
        // `session_task.rs` calls this in `SessionTask::new` before any
        // DevMode is active). Relying on the observer slot alone silently
        // dropped the channel, so a mid-loop `EnableDebugMode` written into
        // `session_handle.pending_debug_handles` was never visible to the
        // running agent loop. The agent loop drains this channel at the
        // start of each iteration via [`Self::take_pending_debug_handles`].
        //
        // We still forward to the observer slot as well: for a session that
        // is ALREADY Dev, `DebugObserverImpl::check_pending_injection` can
        // observe a late bypass write (rewind/patch injection), and the
        // observer's own pending slot remains the mirror for the Dev case.
        self.debug_observer.set_pending_injection(ch.clone());
        self.pending_debug_handles = Some(ch);
    }

    /// Take any bypass-injected debug handles, clearing the slot.
    ///
    /// Called at the start of every agent-loop iteration. Returns the
    /// handles written by SessionManager's `push_debug_mode_to_existing_sessions`
    /// (the mid-loop `EnableDebugMode` path) if any, so the caller can
    /// build a DevMode observer and inject it via [`Self::set_debug_mode`].
    ///
    /// Returns `None` when the lock is contended (the writer holds it); in
    /// that case the caller should retry next iteration rather than block
    /// the agent loop.
    pub(crate) fn take_pending_debug_handles(&self) -> Option<crate::debug::DebugHandles> {
        let ch = self.pending_debug_handles.as_ref()?;
        match ch.try_lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        }
    }

    pub fn debug_observer(&self) -> &DebugObserverSlot {
        &self.debug_observer
    }
    pub fn debug_observer_mut(&mut self) -> &mut DebugObserverSlot {
        &mut self.debug_observer
    }
    pub fn is_dev_mode(&self) -> bool {
        self.debug_observer.is_dev_mode()
    }
    pub fn approval_gate(&self) -> Option<&Arc<dyn ApprovalGate>> {
        self.approval_gate.as_ref()
    }
    pub fn set_approval_gate(&mut self, gate: Arc<dyn ApprovalGate>) {
        self.approval_gate = Some(gate);
    }

    /// ADR-078 follow-up: bind the handles the git-refresh nudge needs.
    /// Called from Phase B once the MQTT client slot and the workspace
    /// resolver both exist.
    pub(crate) fn set_git_nudge(&mut self, slots: crate::agent::git_nudge::GitNudgeSlots) {
        self.git_nudge = Some(slots);
    }

    /// ADR-046: bind the blob store used to read uploaded files. Called
    /// from Phase B of `session_init` once the workspace services are
    /// in place. `None` means image uploads are silently dropped at the
    /// multimodal layer (the metadata system entry is still written to
    /// JSONL — the agent will see the filename but not the picture).
    pub fn attachment_service(&self) -> Option<&Arc<dyn crate::usecases::AttachmentService>> {
        self.attachment_service.as_ref()
    }
    pub fn set_attachment_service(&mut self, svc: Arc<dyn crate::usecases::AttachmentService>) {
        self.attachment_service = Some(svc);
    }
    pub fn shell_approval_threshold(&self) -> &ShellApprovalThreshold {
        &self.shell_approval_threshold
    }

    /// Resolve the user-configured **total context cap**:
    /// Resolve the effective context window budget for history trimming.
    ///
    /// ADR-074: `resolved_cap` is injected by the caller — per-session
    /// `AgentLoop`s pass `resolve_effective_context_window(...)` (session
    /// meta → agent_config → manifest → DEFAULT → min(model window)); this
    /// method stays session-agnostic (invariant 1, §1.5).
    ///
    /// This is the single denominator for every context-usage percentage
    /// (trigger thresholds, UI usage report, HistoryManager::max_tokens).
    /// It treats the resolved cap as a **total** context window and subtracts
    /// the model's output reserve *inside* the cap:
    /// `input = min(provider_input, min(cap, window)) − output_reserve`
    /// (see [`ModelCapabilitiesInfo::input_budget_with_cap`]).
    ///
    /// So a 250K cap + 32K output reserve yields a 218K input budget — not
    /// 250K (the pre-fix behaviour, where the reserve was swallowed by the
    /// cap). `0` is no longer a legal "no limit" sentinel (ADR-074 §6); the
    /// resolution chain never emits it, and `None` here is only defensive.
    pub fn context_trim_budget_with(&self, resolved_cap: Option<u64>, model_name: &str) -> u64 {
        let max_output_limit = self.max_output_tokens_limit_for_model(model_name);

        self.get_model_capabilities(model_name)
            .map(|caps| {
                let effective = caps.input_budget_with_cap(resolved_cap, max_output_limit);
                tracing::debug!(
                    model = %model_name,
                    context_window = caps.context_window,
                    max_input_tokens = ?caps.max_input_tokens,
                    max_output_tokens_limit = max_output_limit,
                    output_reserve = caps.output_reserve(max_output_limit),
                    resolved_cap = resolved_cap.unwrap_or(0),
                    effective,
                    "Computed usable input context budget (output reserve subtracted inside cap)"
                );
                effective
            })
            .unwrap_or_else(|| {
                let fallback = std::cmp::min(
                    resolved_cap.unwrap_or(crate::config::DEFAULT_CONTEXT_WINDOW),
                    self.config.history_max_tokens,
                );
                tracing::debug!(
                    model = %model_name,
                    resolved_cap = resolved_cap.unwrap_or(0),
                    fallback,
                    "No model capabilities for '{}', using capped history_max_tokens as fallback",
                    model_name
                );
                fallback
            })
    }
}

impl Clone for AgentCore {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            manifest: self.manifest.clone(),
            provider: self.provider.clone(),
            builtin_tools: self.builtin_tools.clone(),
            mcp_tools: self.mcp_tools.clone(),
            all_tools: self.all_tools.clone(),
            global_provider_list: self.global_provider_list.clone(),
            provider_list_version: Arc::clone(&self.provider_list_version),
            provider_key_vault: self.provider_key_vault.clone(),
            compat_cache: self.compat_cache.clone(),
            provider_compact_models: Arc::clone(&self.provider_compact_models),
            default_compact_models: Arc::clone(&self.default_compact_models),
            temperature_override: self.temperature_override,
            manifest_temperature: self.manifest_temperature,
            context_window_override: self.context_window_override,
            compression_ratio_threshold: self.compression_ratio_threshold,
            distiller_runtime: self.distiller_runtime.clone(),
            forgetting_runtime: self.forgetting_runtime.clone(),
            manifest_context_window: self.manifest_context_window,
            approval_timeout_secs: self.approval_timeout_secs,
            system_prompt_override: self.system_prompt_override.clone(),
            session_language_override: self.session_language_override.clone(),
            skill_registry: Arc::clone(&self.skill_registry),
            // ADR-063 §3.7.5: these 9 fields share the inner `Arc` across
            // clones (reference +1), so a write through one clone is
            // visible to all clones. This is the foundation that makes
            // `DebugService::reload_prompts` work without forcing every
            // session to re-derive its `Arc<AgentCore>` from the canonical
            // `AgentCore` in `SessionManager`.
            compaction_prompt: Arc::clone(&self.compaction_prompt),
            compact_template: Arc::clone(&self.compact_template),
            title_prompt: Arc::clone(&self.title_prompt),
            abstention_prompt: Arc::clone(&self.abstention_prompt),
            distiller_merge_prompt: Arc::clone(&self.distiller_merge_prompt),
            memory_provider: self.memory_provider.clone(),
            memory_admin: self.memory_admin.clone(),
            sqlite_store: self.sqlite_store.clone(),
            rag_provider: self.rag_provider.clone(),
            memory_session: self.memory_session.clone(),
            debug_observer: self.debug_observer.clone_production(),
            // ADR-048: the pending-debug channel is intentionally NOT
            // carried across clones — it is per-session state bound by
            // `SessionTask::new` via `set_debug_pending_injection`. A
            // template (or another session's clone) must not inherit a
            // sibling's bypass channel.
            pending_debug_handles: None,
            approval_gate: self.approval_gate.clone(),
            shell_approval_threshold: self.shell_approval_threshold,
            shell_risk_rules: self.shell_risk_rules.clone(),
            // Agent-scoped: every session clone shares the same handles,
            // so a nudge from one session is published exactly once.
            git_nudge: self.git_nudge.clone(),
            embedding_provider: self.embedding_provider.clone(),
            // Deliberately `Arc::clone`, not a fresh cell: the published
            // snapshot and the session's working copy must see one provider,
            // or an adopted model reaches only one of them.
            embedding_provider_shared: Arc::clone(&self.embedding_provider_shared),
            metrics_aggregator: self.metrics_aggregator.clone(),
            consolidation_bg_task: None, // sessions don't own bg task
            consolidation_timer: self.consolidation_timer.clone(), // shared timer for idle reset
            attachment_service: self.attachment_service.clone(),
            // ADR-028: agent-scoped counters are genuinely SHARED across
            // clones — `Arc::clone` keeps all four pointing at the same
            // `AtomicU64`s, so an `accumulate_llm_usage` / `merge_token_totals`
            // on any clone (template, session task, distillation spawn) is
            // immediately visible to every other clone.
            agent_total_input_tokens: Arc::clone(&self.agent_total_input_tokens),
            agent_total_output_tokens: Arc::clone(&self.agent_total_output_tokens),

            // ADR-066: cache counters inherit the same sharing.
            agent_total_cache_read_tokens: Arc::clone(&self.agent_total_cache_read_tokens),
            agent_total_cache_write_tokens: Arc::clone(&self.agent_total_cache_write_tokens),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use acowork_core::protocol::{ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry};
    use acowork_core::providers::mock::MockProvider;

    /// Add a single dummy tool entry with the given name + initial
    /// enabled flag. Used by tests that need at least one entry in
    /// `builtin_tools` to exercise the enabled-rewrite path.
    fn seed_one_tool(core: &mut AgentCore, name: &str, enabled: bool) {
        struct DummyTool(String);
        #[async_trait::async_trait]
        impl acowork_core::tools::traits::Tool for DummyTool {
            fn name(&self) -> String {
                self.0.clone()
            }
            fn spec(&self) -> acowork_core::tools::traits::ToolSpec {
                acowork_core::tools::traits::ToolSpec {
                    name: self.0.clone(),
                    description: "test".to_string(),
                    input_schema: serde_json::json!({}),
                }
            }
            async fn execute(
                &self,
                _params: serde_json::Value,
                _work_dir: Option<&str>,
            ) -> acowork_core::error::Result<acowork_core::tools::traits::ToolResult> {
                Ok(acowork_core::tools::traits::ToolResult {
                    ok: true,
                    content: String::new(),
                    error: None,
                    token_usage: None,
                })
            }
        }
        core.builtin_tools
            .push(BuiltinToolEntry::with_resolved_enabled(
                enabled,
                Arc::new(DummyTool(name.to_string())),
            ));
    }

    /// Build a minimal AgentCore for testing context_trim_budget.
    fn make_core(
        context_window_override: Option<u64>,
        manifest_context_window: Option<u64>,
        model_caps: Option<ModelCapabilitiesInfo>,
        history_max_tokens: u64,
    ) -> AgentCore {
        let config = RuntimeConfig {
            history_max_tokens,
            ..RuntimeConfig::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.cw"
            version = "1.0.0"
            name = "Test CW"
            description = "Context window test agent"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider::single_text("test"));

        let mut core = AgentCore::new(config, manifest, provider, vec![]);
        core.context_window_override = context_window_override;
        core.manifest_context_window = manifest_context_window;

        if let Some(caps) = model_caps {
            let model = ProviderListItem {
                id: "test-provider".into(),
                base_url: "http://localhost".into(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                models: vec![ProviderModelEntry {
                    id: "test-model".into(),
                    capabilities: caps,
                    max_output_tokens_limit: 32_768,
                }],
                compact_model: None,
                custom: false,
            };
            let mut list = core.global_provider_list.write().unwrap();
            *list = vec![model];
        }
        core
    }

    fn test_model_caps(context_window: u64, max_output_tokens: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens,
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
        }
    }

    /// Test-only mirror of the per-session AgentLoop resolution flow
    /// (ADR-074 §3.2): resolve the chain (no session override) then
    /// compute the trim budget. `session_override` lets tests exercise
    /// the per-session Layer 0 without constructing a ConversationSession.
    fn resolved_trim_budget(core: &AgentCore, model: &str, session_override: Option<u64>) -> u64 {
        let caps = core.get_model_capabilities(model);
        let resolved = crate::agent::session_config::resolve_effective_context_window(
            session_override,
            core.context_window_override,
            core.manifest_context_window,
            caps.as_ref(),
        );
        core.context_trim_budget_with(Some(resolved), model)
    }

    // ── Resolution chain tests ──────────────────────────────────────

    #[test]
    fn test_resolution_chain_config_wins_over_manifest() {
        // Layer 1: config=100K (TOTAL window), Layer 2: manifest=64K → 100K wins.
        // The cap is treated as a total context window: output reserve is
        // subtracted INSIDE the cap → 100_000 − 16_384 = 83_616 input budget.
        let core = make_core(
            Some(100_000),
            Some(64_000),
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 83_616);
    }

    #[test]
    fn test_resolution_chain_manifest_over_default() {
        // Layer 1: None, Layer 2: 64K (TOTAL window) → reserve subtracted
        // inside cap → 64_000 − 16_384 = 47_616 input budget.
        let core = make_core(
            None,
            Some(64_000),
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 47_616);
    }

    #[test]
    fn test_resolution_chain_falls_back_to_default() {
        // Both None → should use DEFAULT_CONTEXT_WINDOW (200K)
        // Model is also 200K, so min(200K, 200K-16K) = 184K?
        // Actually: min(200_000, 183_616) = 183_616
        let core = make_core(None, None, Some(test_model_caps(200_000, 16_384)), 128_000);
        let budget = resolved_trim_budget(&core, "test-model", None);
        // DEFAULT_CONTEXT_WINDOW=200K > model_budget=183_616 → min = 183_616
        assert_eq!(budget, 183_616);
    }

    #[test]
    fn test_resolution_chain_default_smaller_than_model() {
        // Both None → default 200K TOTAL cap binds (model window 1M); reserve
        // subtracted inside cap → 200_000 − 16_384 = 183_616.
        let core = make_core(
            None,
            None,
            Some(test_model_caps(1_000_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 183_616);
    }

    // ── Zero value tests (ADR-074: 0 invalid → skip layer) ──────────

    #[test]
    fn test_zero_cap_is_invalid_falls_to_default() {
        // config=0 is no longer "no limit" (ADR-074 §6) — invalid → falls
        // to DEFAULT_CONTEXT_WINDOW (200K) → min(500K, 200K) − 16_384.
        let core = make_core(
            Some(0),
            None,
            Some(test_model_caps(500_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 183_616);
    }

    #[test]
    fn test_zero_cap_with_small_model() {
        // config=0 invalid → DEFAULT 200K, but the small model window
        // (32K) binds: 32_000 − 4_096 = 27_904.
        let core = make_core(Some(0), None, Some(test_model_caps(32_000, 4_096)), 128_000);
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 27_904);
    }

    // ── min(user_cap, model_budget) tests ──────────────────────────

    #[test]
    fn test_user_cap_smaller_than_model() {
        // User sets 50K TOTAL, model has 200K → input budget = 50K cap −
        // 16_384 output reserve = 33_616.
        let core = make_core(
            Some(50_000),
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 33_616);
    }

    #[test]
    fn test_model_smaller_than_user_cap() {
        // User sets 500K, model has 128K → use model's budget
        let core = make_core(
            Some(500_000),
            None,
            Some(test_model_caps(128_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        // model_budget = 128_000 - 16_384 = 111_616
        assert_eq!(budget, 111_616);
    }

    #[test]
    fn test_user_cap_exactly_equals_model_budget() {
        // User sets 183_616 (the old "200K−16_384 model budget" number).
        // With the cap now treated as a TOTAL window the output reserve is
        // subtracted again inside the cap → 183_616 − 16_384 = 167_232.
        let core = make_core(
            Some(183_616),
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 167_232);
    }

    // ── No model capabilities fallback tests ────────────────────────

    #[test]
    fn test_no_model_caps_falls_back_to_history_max_tokens() {
        // No model capabilities → use history_max_tokens capped by user setting
        let core = make_core(Some(80_000), None, None, 64_000);
        let budget = resolved_trim_budget(&core, "test-model", None);
        // user_cap=80K, history_max_tokens=64K → min(80K, 64K) = 64K
        assert_eq!(budget, 64_000);
    }

    #[test]
    fn test_no_model_caps_zero_user_cap() {
        // No model capabilities + user_cap=0 (invalid) → DEFAULT 200K →
        // min(200K, history 128K) = 128K.
        let core = make_core(Some(0), None, None, 128_000);
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 128_000);
    }

    #[test]
    fn test_no_model_caps_default_fallback() {
        // No model capabilities, no user cap → min(DEFAULT_CONTEXT_WINDOW=200K, history=128K) = 128K
        let core = make_core(None, None, None, 128_000);
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 128_000);
    }

    // ── Manifest layer tests ────────────────────────────────────────

    #[test]
    fn test_manifest_context_window_is_honored() {
        // Config=None, Manifest=150K TOTAL, Model=200K → input budget =
        // 150_000 − 16_384 = 133_616.
        let core = make_core(
            None,
            Some(150_000),
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 133_616);
    }

    #[test]
    fn test_manifest_context_window_capped_by_model() {
        // Config=None, Manifest=300K, Model=128K → min(300K, 111K) = 111K
        let core = make_core(
            None,
            Some(300_000),
            Some(test_model_caps(128_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 111_616);
    }

    #[test]
    fn test_manifest_zero_is_invalid_falls_to_default() {
        // Manifest=0 is no longer "no limit" (ADR-074 §6) — invalid →
        // DEFAULT_CONTEXT_WINDOW (200K) → min(500K, 200K) − 16_384.
        let core = make_core(
            None,
            Some(0),
            Some(test_model_caps(500_000, 16_384)),
            128_000,
        );
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 183_616);
    }

    // ── Edge case tests ─────────────────────────────────────────────

    #[test]
    fn test_user_cap_of_one_token() {
        // Extreme: 1 token (TOTAL). 1 is below FLOOR so the resolution
        // chain never emits it — but the budget math must still saturate
        // to 0 (reserve exceeds the capped window), so pin it directly.
        let core = make_core(
            Some(1),
            None,
            Some(test_model_caps(128_000, 16_384)),
            128_000,
        );
        let budget = core.context_trim_budget_with(Some(1), "test-model");
        assert_eq!(budget, 0);
    }

    #[test]
    fn test_model_budget_zero_edge_case() {
        // Model with 0 context_window (invalid but defensive)
        let core = make_core(Some(100_000), None, Some(test_model_caps(0, 0)), 128_000);
        let budget = resolved_trim_budget(&core, "test-model", None);
        // effective_input_budget: 0 - 0 = 0; min(100K, 0) = 0
        assert_eq!(budget, 0);
    }

    // ── max_output_tokens_limit interaction ──────────────────────────

    #[test]
    fn test_max_output_tokens_limit_affects_model_budget() {
        // Model: 200K, max_output=16K, limit=8K → output reserve =
        // min(16_384, 8_192) = 8_192. User cap=100K TOTAL →
        // 100_000 − 8_192 = 91_808 input budget.
        let core = make_core(
            Some(100_000),
            None,
            Some(test_model_caps(200_000, 16_384)),
            128_000,
        );
        // Override max_output_tokens_limit for the model to 8K
        {
            let mut list = core.global_provider_list.write().unwrap();
            for p in list.iter_mut() {
                for m in p.models.iter_mut() {
                    m.max_output_tokens_limit = 8_192;
                }
            }
        }
        let budget = resolved_trim_budget(&core, "test-model", None);
        assert_eq!(budget, 91_808);
    }

    // ── apply_builtin_enabled_entries (ADR-052 §3.5 shared policy) ────
    //
    // The tests below pin the policy helper that BOTH the per-session
    // hot-update path (`session_task::apply_builtin_tools_update`) and
    // the SessionManager template-sync path
    // (`SessionManager::apply_builtin_tools_enabled`) funnel through.
    // Without these, the two paths could drift silently and reintroduce
    // the "registry vs persistence" bug 2 regression.

    use crate::agent_config::AgentToolEntry;

    #[test]
    fn apply_builtin_enabled_entries_drops_unknown_names() {
        // Defensive: an `entries` payload that mentions a tool name not
        // currently in `builtin_tools` must be silently dropped, not
        // panic and not auto-register it (that would let a hostile
        // `agent_tools.json` smuggle arbitrary tools into the registry).
        let mut core = make_core(Some(8192), None, None, 0);
        seed_one_tool(&mut core, "shell", true);
        let before = core.builtin_tools.len();

        core.apply_builtin_enabled_entries(&[
            AgentToolEntry::new("shell", false),
            AgentToolEntry::new("totally_made_up_tool", true),
        ]);

        assert_eq!(
            core.builtin_tools.len(),
            before,
            "unknown names must not introduce new registry entries"
        );
    }

    #[test]
    fn apply_builtin_enabled_entries_preserves_wrapped_tool_impl() {
        // The shared policy only rewrites the `enabled` flag. The
        // wrapped `tool` impl (security decorators applied during
        // `ToolRegistry::activate`) must survive untouched.
        let mut core = make_core(Some(8192), None, None, 0);
        seed_one_tool(&mut core, "shell", true);

        let shell_before: Arc<dyn acowork_core::tools::traits::Tool> = core
            .builtin_tools
            .iter()
            .find(|e| e.name() == "shell")
            .unwrap()
            .tool
            .clone();

        core.apply_builtin_enabled_entries(&[AgentToolEntry::new("shell", false)]);

        let shell_after: Arc<dyn acowork_core::tools::traits::Tool> = core
            .builtin_tools
            .iter()
            .find(|e| e.name() == "shell")
            .unwrap()
            .tool
            .clone();
        assert!(
            Arc::ptr_eq(&shell_before, &shell_after),
            "the wrapped tool impl must be the same Arc (security decorators preserved)"
        );
    }

    // ── Consolidation timer integration tests (ADR-051 P4) ──────────

    /// G1: Verify that `start_consolidation_pipeline` stores the timer
    /// in `AgentCore.consolidation_timer` and that `notify_consolidation_active`
    /// resets the timer's idle clock.
    ///
    /// This is the integration-level regression for the P0 bug where the
    /// timer was created but immediately dropped (not stored), causing
    /// `notify_consolidation_active` to be a no-op and consolidation to
    /// fire unconditionally on every idle-timeout tick.
    #[tokio::test]
    async fn test_consolidation_timer_stored_and_notify_resets_idle() {
        use acowork_core::embedding::EmbeddingProvider;

        let mut core = make_core(Some(8192), None, None, 0);

        // A real in-memory store as the memory provider.
        let store: Arc<dyn acowork_memory::MemoryProvider> = Arc::new(
            acowork_sqlite::SqliteStore::open_in_memory(768).unwrap(),
        );
        core.memory_provider = Some(store);

        // Set up a dummy embedding provider (required by start_consolidation_pipeline).
        struct DummyEmbeddingProvider;
        #[async_trait::async_trait]
        impl EmbeddingProvider for DummyEmbeddingProvider {
            fn name(&self) -> &str {
                "dummy"
            }
            async fn embed(
                &self,
                _text: &str,
            ) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                Ok(vec![0.0; 384])
            }
            async fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
            }
            fn dimension(&self) -> usize {
                384
            }
            async fn is_available(&self) -> bool {
                true
            }
        }
        core.embedding_provider = Some(Arc::new(DummyEmbeddingProvider));

        // Before starting: timer is None.
        assert!(core.consolidation_timer.is_none());
        assert!(core.consolidation_bg_task.is_none());

        // Start the consolidation pipeline.
        core.start_consolidation_pipeline();

        // After starting: timer must be stored.
        let timer = core
            .consolidation_timer
            .clone()
            .expect("consolidation_timer must be Some after start_consolidation_pipeline");
        assert!(core.consolidation_bg_task.is_some());

        // Wait a moment to let some "idle" time accumulate.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Call notify_consolidation_active - should reset idle timer.
        core.notify_consolidation_active().await;

        // Verify idle is near 0 via public accessor.
        let idle_secs = timer.idle_secs().await;
        assert!(
            idle_secs < 5,
            "Idle should be < 5s after notify_consolidation_active, got {idle_secs}s"
        );

        // Legacy `should_run()` Pending-trigger assertion removed with the
        // method (ADR-057 §5.3 redesign — Pending has no producer).

        // Clean up: abort the bg task.
        if let Some(bg) = core.consolidation_bg_task.take() {
            bg.abort();
        }
    }

    /// G1b: Verify that `clone_for_session` shares the same timer Arc.
    /// When a session clone calls `notify_consolidation_active`, the
    /// original AgentCore's timer must also be reset (shared Arc).
    #[tokio::test]
    async fn test_consolidation_timer_shared_across_session_clone() {
        use acowork_core::embedding::EmbeddingProvider;

        let mut core = make_core(Some(8192), None, None, 0);

        let store: Arc<dyn acowork_memory::MemoryProvider> = Arc::new(
            acowork_sqlite::SqliteStore::open_in_memory(768).unwrap(),
        );
        core.memory_provider = Some(store);

        struct DummyEmbeddingProvider;
        #[async_trait::async_trait]
        impl EmbeddingProvider for DummyEmbeddingProvider {
            fn name(&self) -> &str {
                "dummy"
            }
            async fn embed(
                &self,
                _text: &str,
            ) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                Ok(vec![0.0; 384])
            }
            async fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
            }
            fn dimension(&self) -> usize {
                384
            }
            async fn is_available(&self) -> bool {
                true
            }
        }
        core.embedding_provider = Some(Arc::new(DummyEmbeddingProvider));

        core.start_consolidation_pipeline();
        let original_timer = core.consolidation_timer.clone().unwrap();

        // Clone the core (simulates session creation).
        let session_core = core.clone();

        // The session clone must share the same timer Arc.
        let session_timer = session_core
            .consolidation_timer
            .as_ref()
            .expect("session clone must have consolidation_timer");
        assert!(
            Arc::ptr_eq(session_timer, &original_timer),
            "session clone must share the same timer Arc (not a copy)"
        );

        // Session clone resets the timer.
        session_core.notify_consolidation_active().await;

        // The original core's timer must reflect the reset.
        let idle_secs = original_timer.idle_secs().await;
        assert!(
            idle_secs < 5,
            "Original timer must reflect session clone's notify_active (shared Arc)"
        );

        // Clean up.
        if let Some(bg) = core.consolidation_bg_task.take() {
            bg.abort();
        }
    }

    // ── ADR-071 D7: distiller prompt override projection ─────────────

    /// D7a: the per-agent prompt override (AgentCore slot) is layered on top
    /// of the manifest-derived `DistillerConfig` — manifest fields survive
    /// the overlay.
    #[test]
    fn test_distiller_scheduler_config_projects_prompt_overrides() {
        let core =
            make_core_with_memory_toml("[memory.distiller]
enabled = true
batch_size = 20
");
        // Without an override file the slot is `None` -> manifest values.
        let cfg = core.distiller_scheduler_config();
        assert!(cfg.distiller_enabled);
        let dc = cfg.distiller_config.expect("manifest section present");
        assert_eq!(dc.batch_size, 20);
        assert!(dc.merge_prompt_override.is_none());

        // Write the distiller prompt override (as `reload_prompts_into_core`
        // would after a Debug panel save).
        *core.distiller_merge_prompt.write().unwrap() = Some("MG_OVERRIDE".to_string());

        let cfg2 = core.distiller_scheduler_config();
        let dc2 = cfg2.distiller_config.expect("manifest section present");
        assert_eq!(dc2.merge_prompt_override.as_deref(), Some("MG_OVERRIDE"));
        assert_eq!(
            dc2.batch_size, 20,
            "manifest-derived fields must survive the prompt overlay"
        );
    }

    /// D7b: even without a `[memory.distiller]` manifest section, a
    /// package that ships the distiller prompt override must still surface a
    /// `Some(DistillerConfig)` (so the override reaches the distiller) —
    /// while the opt-in `distiller_enabled` stays OFF.
    #[test]
    fn test_distiller_scheduler_config_override_without_manifest_section() {
        let core = make_core_with_memory_toml("");
        assert!(!core.manifest.memory.distiller_enabled());

        *core.distiller_merge_prompt.write().unwrap() = Some("MG_OVERRIDE".to_string());

        let cfg = core.distiller_scheduler_config();
        assert!(!cfg.distiller_enabled, "opt-in invariant must hold");
        let dc = cfg
            .distiller_config
            .expect("override must force Some(config)");
        assert_eq!(dc.merge_prompt_override.as_deref(), Some("MG_OVERRIDE"));
        // No manifest fields -> defaults for the rest.
        assert_eq!(
            dc.batch_size,
            acowork_memory::consolidation::DistillerConfig::default().batch_size
        );
    }

    // ── ADR-071 D2: manual distill trigger (AgentCore-level) ─────────

    /// G1c: A manual distill on a distiller that is explicitly OFF must be
    /// refused — opt-in invariant (ADR-068 / ADR-071): a disabled distiller
    /// must never produce semantic-layer nodes, even when force-triggered.
    #[tokio::test]
    async fn test_manual_distill_refused_when_disabled() {
        let core = make_core(Some(8192), None, None, 0);
        // Default manifest: no `[memory.distiller]` section → disabled.
        assert!(!core.manifest.memory.distiller_enabled());

        let err = core.run_episodic_distill_once().await.unwrap_err();
        assert!(
            err.contains("distiller is disabled"),
            "disabled distiller must refuse manual runs, got: {err}"
        );
    }

    /// G1d: With the distiller enabled and providers wired, a manual run
    /// completes and returns a real DistillerResult (empty backlog → zero
    /// promotions, but the pipeline runs).
    #[tokio::test]
    async fn test_manual_distill_runs_when_enabled() {
        use acowork_core::embedding::EmbeddingProvider;

        let mut core = make_core_with_memory_toml("[memory.distiller]\nenabled = true\n");
        assert!(core.manifest.memory.distiller_enabled());

        let store: Arc<dyn acowork_memory::MemoryProvider> = Arc::new(
            acowork_sqlite::SqliteStore::open_in_memory(768).unwrap(),
        );
        core.memory_provider = Some(store);

        struct DummyEmbeddingProvider;
        #[async_trait::async_trait]
        impl EmbeddingProvider for DummyEmbeddingProvider {
            fn name(&self) -> &str {
                "dummy"
            }
            async fn embed(
                &self,
                _text: &str,
            ) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                Ok(vec![0.0; 384])
            }
            async fn embed_batch(
                &self,
                texts: &[&str],
            ) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                Ok(texts.iter().map(|_| vec![0.0; 384]).collect())
            }
            fn dimension(&self) -> usize {
                384
            }
            async fn is_available(&self) -> bool {
                true
            }
        }
        core.embedding_provider = Some(Arc::new(DummyEmbeddingProvider));

        let result = core
            .run_episodic_distill_once()
            .await
            .expect("manual distill should succeed when enabled");
        let result = result.expect("manual distill returns a result");
        // Empty store: the distiller scans nothing and promotes nothing.
        assert_eq!(result.episodes_scanned, 0);
        assert_eq!(result.facts_promoted, 0);
    }

    /// G9: Verify that `AgentCore.rag_provider` is None by default
    /// and can be set / accessed. This tests the accessor path that
    /// `session_init` uses to propagate the RAG provider from boot
    /// context.
    #[test]
    fn test_rag_provider_default_none_and_settable() {
        let mut core = make_core(Some(8192), None, None, 0);

        // Default: no RAG provider.
        assert!(core.rag_provider.is_none());

        // Set a dummy RAG provider.
        use acowork_core::rag::RagProvider;
        struct DummyRag;
        #[async_trait::async_trait]
        impl RagProvider for DummyRag {
            fn name(&self) -> &str {
                "dummy_rag"
            }
            async fn query(&self, _query: &str) -> Vec<acowork_core::rag::AnnotatedRagResult> {
                Vec::new()
            }
            async fn query_with_params(
                &self,
                _query: &str,
                _top_k: Option<u32>,
                _score_threshold: Option<f32>,
                _filters: Option<serde_json::Value>,
            ) -> Vec<acowork_core::rag::AnnotatedRagResult> {
                Vec::new()
            }
        }
        core.rag_provider = Some(Arc::new(DummyRag));

        // Verify it's accessible.
        assert!(core.rag_provider.is_some());
        assert_eq!(core.rag_provider.as_ref().unwrap().name(), "dummy_rag");

        // Verify it survives clone_for_session.
        let session_core = core.clone();
        assert!(session_core.rag_provider.is_some());
        assert_eq!(
            session_core.rag_provider.as_ref().unwrap().name(),
            "dummy_rag"
        );
    }

    /// Build a minimal per-session `DebugHandles` bundle, mirroring
    /// `SessionManager::create_session` / `push_debug_mode_to_existing_sessions`.
    async fn make_debug_handles(session_id: &str) -> crate::debug::DebugHandles {
        let ctrl = Arc::new(tokio::sync::Mutex::new(
            crate::debug::controller::DebugController::new(),
        ));
        let bus = crate::debug::events::DebugEventBus::new();
        let (rw, rs, rc) = {
            let guard = ctrl.lock().await;
            (
                guard.rewind_notify_handle(),
                guard.resume_notify_handle(),
                guard.control_notify_handle(),
            )
        };
        crate::debug::DebugHandles {
            debug_ctrl: ctrl,
            debug_event_tx: bus.sender_template().for_session(session_id.to_string()),
            rewind_notify: rw,
            resume_notify: rs,
            control_notify: rc,
        }
    }

    /// Regression for the mid-loop `EnableDebugMode` bypass bug (ADR-048):
    ///
    /// `AgentCore::set_debug_pending_injection` must hold the bypass
    /// channel at the AgentCore level EVEN when the observer slot is
    /// `Production` (the observer's own `set_pending_injection` is a no-op
    /// in that state). Before this fix, `session_task.rs` bound the channel
    /// in `SessionTask::new` — which runs while the observer is Production
    /// — so a debug panel opened mid-loop could never be picked up, and
    /// iteration counters stayed frozen until the SessionTask message loop
    /// processed the queued `EnableDebugMode`.
    #[tokio::test]
    async fn test_pending_debug_handles_held_at_core_level_and_clone_isolated() {
        let mut core = make_core(Some(8192), None, None, 0);

        // Sanity: a fresh core has no pending channel.
        assert!(core.take_pending_debug_handles().is_none());

        // Simulate SessionTask::new binding the shared bypass channel while
        // the observer is still Production.
        let channel: Arc<tokio::sync::Mutex<Option<crate::debug::DebugHandles>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        core.set_debug_pending_injection(channel.clone());

        // The channel must be retained at the AgentCore level (the fix).
        assert!(core.pending_debug_handles.is_some());

        // Simulate SessionManager::push_debug_mode_to_existing_sessions
        // writing per-session handles into the shared channel while the
        // agent loop is running.
        {
            let mut pending = channel.lock().await;
            *pending = Some(make_debug_handles("sess-1").await);
        }

        // The agent loop drains it at the next iteration boundary.
        let taken = core
            .take_pending_debug_handles()
            .expect("bypass-injected handles must be drainable");
        assert_eq!(taken.debug_event_tx.session_id(), "sess-1");

        // A second drain yields nothing (slot cleared).
        assert!(core.take_pending_debug_handles().is_none());

        // Clones do NOT carry the pending channel: it is per-session state,
        // rebound by each SessionTask::new. A template clone must not
        // inherit a sibling session's bypass channel.
        let clone = core.clone();
        assert!(clone.pending_debug_handles.is_none());
        assert!(clone.take_pending_debug_handles().is_none());
    }

    // ── auto_inject manifest opt-in (per-agent switch) ─────────────

    /// Build an AgentCore whose manifest carries the given TOML fragment
    /// appended after the `[llm]` section (e.g. a `[memory.quality]`
    /// block). An empty fragment leaves `[memory]` absent entirely.
    fn make_core_with_memory_toml(extra_toml: &str) -> AgentCore {
        let config = RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(&format!(
            r#"
            agent_id = "com.test.cw"
            version = "1.0.0"
            name = "Test CW"
            description = "auto-inject opt-in test agent"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"

            {extra_toml}
            "#
        ))
        .unwrap();
        let provider = Arc::new(MockProvider::single_text("test"));
        AgentCore::new(config, manifest, provider, vec![])
    }

    #[test]
    fn test_auto_inject_defaults_off_when_manifest_absent() {
        // Zero-config = current behaviour: auto-inject stays OFF, the LLM
        // recalls memories via the explicit `memory_recall` tool.
        let core = make_core_with_memory_toml("");
        assert!(
            !core.init_memory_manager().config().auto_inject_enabled,
            "auto_inject_enabled must default to false"
        );
    }

    #[test]
    fn test_auto_inject_manifest_opt_in_true() {
        // Per-agent opt-in: `[memory.quality].auto_inject_enabled = true`.
        let core = make_core_with_memory_toml("[memory.quality]\nauto_inject_enabled = true\n");
        assert!(
            core.init_memory_manager().config().auto_inject_enabled,
            "manifest opt-in must enable auto-inject"
        );
    }

    #[test]
    fn test_auto_inject_manifest_opt_in_false() {
        // Explicit opt-out is still honored.
        let core = make_core_with_memory_toml("[memory.quality]\nauto_inject_enabled = false\n");
        assert!(
            !core.init_memory_manager().config().auto_inject_enabled,
            "manifest opt-out must disable auto-inject"
        );
    }

    #[test]
    fn test_forgetting_live_update_refreshes_session_handle_config() {
        // ADR-057 §5.3 redesign: toggling memory forgetting via a live
        // runtime-config PUT must refresh the shared `MemorySessionHandle`
        // config — the `memory_recall` tool reads it from there (whereas
        // auto-inject builds a fresh manager per turn). Without this
        // refresh an in-flight session would keep the boot-time config
        // (forgetting OFF) until the next runtime restart.
        let mut core = make_core_with_memory_toml("");
        let handle = Arc::new(crate::memory::MemorySessionHandle::new(None));
        core.memory_session = Some(handle.clone());
        assert!(
            handle.memory_config().is_none(),
            "handle config is unset until memory init"
        );

        // Simulate the memory-panel PUT: forgetting ON, half-life 90 days.
        let overrides = RuntimeConfigOverrides {
            memory_forgetting_enabled: Some(true),
            memory_forgetting_half_life_days: Some(90),
            ..Default::default()
        };
        core.apply_runtime_config(&overrides);

        let cfg = handle
            .memory_config()
            .expect("handle config must be refreshed after the forgetting PUT");
        assert!(
            cfg.forgetting.enabled,
            "retrieval-side decay must become live without a restart"
        );
        assert_eq!(cfg.forgetting.half_life_days, 90);

        // Auto-inject reads the same settings straight from the core.
        assert!(core.init_memory_manager().config().forgetting.enabled);
        assert_eq!(
            core.init_memory_manager()
                .config()
                .forgetting
                .half_life_days,
            90
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // ADR-063 §3.4 L2 reload — AgentCore 9 prompt fields are
    // `Arc<RwLock<Option<String>>>` so that `DebugService::reload_prompts`
    // can mutate the canonical Arc in `SessionManager` and have every
    // session that holds a `Clone` see the new value (no session restart).
    //
    // These three tests lock in the three preconditions the L2 reload
    // path relies on:
    //
    //   1. Fresh AgentCore: all 9 accessors return `None` (R3 default —
    //      nothing in memory until Phase B injects or `reload_prompts`
    //      re-reads from disk).
    //   2. Clone shares Arc (R3 §Clone for AgentCore): writing through
    //      one handle is visible from any other clone. Sessions that
    //      captured `AgentCore` before DevMode reload still see the new
    //      prompt.
    //   3. Accessor reflection: the `pub fn` accessor reads through the
    //      RwLock on every call (no caching), so reload is observable
    //      without a session re-derivation step.
    // ─────────────────────────────────────────────────────────────────────

    #[test]
    fn test_prompt_accessors_default_to_none() {
        // ADR-063 §3.4 — fresh AgentCore has no override loaded. Phase B
        // (session_init) or `reload_prompts` is the only path that
        // populates these. A `None` here means "fall back to the
        // built-in constant" at the LLM call site.
        let core = make_core(Some(8192), None, None, 0);

        assert!(
            core.compaction_prompt().is_none(),
            "compaction_prompt must default to None"
        );
        assert!(
            core.title_prompt().is_none(),
            "title_prompt must default to None"
        );
        assert!(
            core.abstention_prompt().is_none(),
            "abstention_prompt must default to None"
        );
        assert!(
            core.compact_template().is_none(),
            "compact_template must default to None"
        );
    }

    #[test]
    fn test_agent_core_clone_shares_arc_for_prompts() {
        // ADR-063 §3.4 — `Clone for AgentCore` clones the inner
        // `Arc<RwLock<Option<String>>>` (reference +1), so a write
        // through any clone is visible from every other clone. This is
        // the precondition that lets `DebugService::reload_prompts`
        // (which holds the canonical AgentCore in SessionManager) push
        // a new prompt into running sessions that already cloned the
        // AgentCore at session start.
        //
        // We write through the `pub(crate)` field directly — same
        // pattern `DebugService::reload_prompts` uses. The clone test
        // is whether the OTHER handle observes the change.
        let core = make_core(Some(8192), None, None, 0);
        let session_clone = core.clone();

        // Write through `core` — simulates `reload_prompts` doing
        // `*core.compaction_prompt.write().unwrap() = Some(...)`.
        *core.compaction_prompt.write().unwrap() = Some("override-from-canonical".to_string());

        // Read through the cloned handle that a running session would
        // hold. Must reflect the write.
        assert_eq!(
            session_clone.compaction_prompt().as_deref(),
            Some("override-from-canonical"),
            "Clone for AgentCore must share Arc<RwLock>; reload through \
             canonical must be visible to session-held clones"
        );

        // Symmetry: write through the session clone, read from canonical.
        *session_clone.title_prompt.write().unwrap() = Some("override-from-session".to_string());
        assert_eq!(
            core.title_prompt().as_deref(),
            Some("override-from-session"),
            "Write direction must be symmetric across all Arc clones"
        );
    }

    #[test]
    fn test_agent_core_clone_shares_arc_for_compact_models() {
        // Regression (compact-model switch didn't take effect): the SSE
        // session kept distilling with the stale provider after the global
        // pick was switched to minimax. Root cause — `default_compact_models`
        // and `provider_compact_models` were plain fields, so the deep clone
        // taken by `SessionTask::new` snapshotted them and a write from
        // `SessionManager::update_global_provider_list` (which holds the
        // canonical AgentCore) never reached an in-flight session.
        //
        // These fields MUST be shared `Arc<RwLock<...>>` (ADR-063 §3.7.5),
        // exactly like `compaction_prompt`, so the global pick reaches every
        // existing session on its next `resolve_distill_model()` call.
        let core = make_core(Some(8192), None, None, 0);
        let session_clone = core.clone(); // simulates an in-flight session

        // Simulate `update_global_provider_list` writing through the
        // canonical handle held by SessionManager.
        core.default_compact_models.write().unwrap()
            .push(("minimax".to_string(), "MiniMax-M2.5".to_string()));
        core.provider_compact_models
            .write()
            .unwrap()
            .insert("minimax".to_string(), Some("MiniMax-M2.5".to_string()));
        *core.provider_list_version.write().unwrap() = 42;

        // The in-flight session's clone must observe all three writes.
        assert_eq!(
            session_clone
                .default_compact_models
                .read()
                .unwrap()
                .first()
                .map(|(p, m)| (p.as_str(), m.as_str())),
            Some(("minimax", "MiniMax-M2.5")),
            "default_compact_models must be shared, not snapshotted per session"
        );
        assert_eq!(
            session_clone
                .provider_compact_models
                .read()
                .unwrap()
                .get("minimax")
                .cloned()
                .flatten()
                .as_deref(),
            Some("MiniMax-M2.5"),
            "provider_compact_models must be shared, not snapshotted per session"
        );
        assert_eq!(*session_clone.provider_list_version.read().unwrap(), 42);
    }

    #[test]
    fn test_prompt_accessor_reflects_write_through_lock() {
        // ADR-063 §3.4 — the public accessor (e.g. `title_prompt`)
        // MUST read the RwLock on every call (no caching to a local
        // String), otherwise L2 reload would be silently broken for any
        // session that derived its `AgentCore` clone before the reload.
        // Pin the behavior with an explicit sequence: read → write → read.
        let core = make_core(Some(8192), None, None, 0);

        // Initial read: None.
        assert!(core.title_prompt().is_none());

        // First reload: Some("first").
        *core.title_prompt.write().unwrap() = Some("first".to_string());
        assert_eq!(core.title_prompt().as_deref(), Some("first"));

        // Second reload: Some("second"). The accessor must NOT have
        // memoized "first" — it must re-read through the lock.
        *core.title_prompt.write().unwrap() = Some("second".to_string());
        assert_eq!(
            core.title_prompt().as_deref(),
            Some("second"),
            "accessor must re-read RwLock every call; reload must be \
             observable without re-deriving AgentCore"
        );

        // Reload to None: simulates the operator deleting the
        // `prompts/<file>.md` file and clicking Reload. The accessor
        // must now return None again so the call site falls back to
        // the built-in constant.
        *core.title_prompt.write().unwrap() = None;
        assert!(
            core.title_prompt().is_none(),
            "clearing the RwLock must propagate to the accessor so the \
             built-in fallback constant takes effect again"
        );
    }

    // ── ADR-066: agent-level cache counter tests ─────────────────────

    /// Helper: build an AgentCore with default test config and no
    /// manifest, suitable for token-counter tests.
    fn make_minimal_core() -> AgentCore {
        let config = RuntimeConfig {
            history_max_tokens: 8000,
            ..RuntimeConfig::default()
        };
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.cache"
            version = "1.0.0"
            name = "Cache counter test"
            description = "ADR-066 cache counter tests"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider::single_text("test"));
        AgentCore::new(config, manifest, provider, vec![])
    }

    /// Basic cache accumulation: four counters all advance correctly
    /// across two LLM calls (the Anthropic-style cache breakdown).
    #[test]
    fn test_accumulate_llm_usage_advances_cache_counters() {
        let core = make_minimal_core();
        assert_eq!(core.agent_token_totals(), (0, 0, 0, 0));

        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 1000,
            completion_tokens: 200,
            cache_read_tokens: 400,
            cache_write_tokens: 100,
            ..Default::default()
        });
        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 3500,
            completion_tokens: 450,
            cache_read_tokens: 1200,
            cache_write_tokens: 0,
            ..Default::default()
        });

        assert_eq!(
            core.agent_token_totals(),
            (4500, 650, 1600, 100),
            "input/output/cache_read/cache_write all accumulate"
        );
    }

    /// `cache_read` follows the same zero-skip gate as `prompt_tokens`
    /// (Provider fallback).  `cache_write` always accumulates.
    #[test]
    fn test_accumulate_llm_usage_cache_zero_input_skip() {
        let core = make_minimal_core();

        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 1000,
            completion_tokens: 100,
            cache_read_tokens: 300,
            cache_write_tokens: 50,
            ..Default::default()
        });
        // Provider fallback: prompt=0, bogus cache_read must NOT count.
        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 0,
            completion_tokens: 50,
            cache_read_tokens: 999,
            cache_write_tokens: 30,
            ..Default::default()
        });
        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 2000,
            completion_tokens: 80,
            cache_read_tokens: 800,
            cache_write_tokens: 0,
            ..Default::default()
        });

        let (_, _, cr, cw) = core.agent_token_totals();
        assert_eq!(cr, 1100, "cache_read: 300 + 800 (999 skipped)");
        assert_eq!(cw, 80, "cache_write: 50 + 30 + 0 (always accumulated)");
    }

    /// Saturating overflow on cache counters — `u64::MAX` followed by a
    /// non-zero add must cap at `u64::MAX` without panicking.
    #[test]
    fn test_accumulate_llm_usage_cache_saturating_overflow() {
        let core = make_minimal_core();

        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 1,
            completion_tokens: 0,
            cache_read_tokens: u64::MAX,
            cache_write_tokens: u64::MAX,
            ..Default::default()
        });
        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 1,
            completion_tokens: 0,
            cache_read_tokens: 5,
            cache_write_tokens: 7,
            ..Default::default()
        });

        let (_, _, cr, cw) = core.agent_token_totals();
        assert_eq!(cr, u64::MAX, "saturating_add caps cache_read at u64::MAX");
        assert_eq!(cw, u64::MAX, "saturating_add caps cache_write at u64::MAX");
    }

    /// `merge_token_totals` applies atomic-max across all four
    /// dimensions, mirroring the ADR-028 in/out semantics.  Each
    /// dimension is max'd independently — a larger scanned value
    /// always wins; a larger live value is preserved; `None` per
    /// dimension skips that dimension.
    #[test]
    fn test_merge_token_totals_cache_atomic_max() {
        let core = make_minimal_core();

        // Step 1: live counter has values.
        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 100,
            completion_tokens: 10,
            cache_read_tokens: 50,
            cache_write_tokens: 20,
            ..Default::default()
        });

        // Step 2: scan reports smaller values across the board — every
        // dimension's atomic-max preserves the live value.
        core.merge_token_totals((
            Some(50), // < live 100
            Some(5),  // < live 10
            Some(10), // < live 50
            Some(5),  // < live 20
        ));
        assert_eq!(
            core.agent_token_totals(),
            (100, 10, 50, 20),
            "per-dimension atomic max preserves live when scan is smaller"
        );

        // Step 3: scan reports larger values across the board — every
        // dimension's atomic-max adopts the scanned value.
        core.merge_token_totals((
            Some(200), // > live 100
            Some(20),  // > live 10
            Some(500), // > live 50
            Some(300), // > live 20
        ));
        assert_eq!(
            core.agent_token_totals(),
            (200, 20, 500, 300),
            "per-dimension atomic max adopts scan when it is larger"
        );

        // Step 4: passing `None` for any dimension leaves that
        // dimension's counter unchanged — supports legacy meta files
        // without cache fields.
        core.merge_token_totals((None, None, Some(600), None));
        assert_eq!(
            core.agent_token_totals(),
            (200, 20, 600, 300),
            "None dimensions are skipped (legacy meta without cache fields)"
        );

        // Step 5: independent dimensions — input is smaller, output is
        // larger.  Atomic max picks per-dimension, not as a tuple.
        core.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 50,
            completion_tokens: 1000,
            ..Default::default()
        });
        // Live now: input=200+50=250, output=20+1000=1020, cache_read=600,
        // cache_write=300.
        core.merge_token_totals((
            Some(100),  // < live 250, ignored
            Some(5000), // > live 1020, adopted
            Some(100),  // < live 600, ignored
            Some(1000), // > live 300, adopted
        ));
        assert_eq!(
            core.agent_token_totals(),
            (250, 5000, 600, 1000),
            "each dimension is max'd independently"
        );
    }

    /// Regression (t-83afab47 follow-up): the four agent-scoped counters
    /// must be *shared* by every `AgentCore` clone, not snapshotted.
    ///
    /// `SessionTask::new` builds the per-session core with
    /// `(*core).clone()`. If the counters were copied, that session's
    /// `agent_total_*` push would report only its own slice of the
    /// agent-wide total, and the cold-start `list_sessions` disk-sum merge
    /// (which lands on the template core) could never reach a session that
    /// is already running — the Agent Status panel then showed ≈ the
    /// Session Status total instead of the sum over all session metas.
    #[test]
    fn test_agent_counters_shared_across_clones() {
        let template = make_minimal_core();
        // Stands in for `SessionTask::new`'s `(*core).clone()`.
        let session_clone = template.clone();

        // Live LLM call on the template -> the session clone sees it.
        template.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 100,
            completion_tokens: 10,
            ..Default::default()
        });
        assert_eq!(
            session_clone.agent_token_totals(),
            (100, 10, 0, 0),
            "accumulate on the template must be visible to the session clone"
        );

        // Cold-start disk scan advances the template -> the session clone's
        // next push must carry the merged historical baseline.
        template.merge_token_totals((Some(50_000_000), Some(500_000), None, None));
        assert_eq!(
            session_clone.agent_token_totals(),
            (50_000_000, 500_000, 0, 0),
            "list_sessions disk merge must be visible to the session clone"
        );

        // ...and the other direction: the session task's own call advances
        // the agent-wide total seen by the template.
        session_clone.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 7,
            ..Default::default()
        });
        assert_eq!(
            template.agent_token_totals().0,
            50_000_007,
            "session clone's accumulate must be visible to the template"
        );
    }

    /// Concurrent sessions: every session task's LLM call must land in the
    /// *same* agent-wide counter immediately, so the Agent Status panel's
    /// `agent_total_*` reflects all sessions running in parallel (session A's
    /// push already includes session B's calls, and vice versa).
    #[test]
    fn test_agent_counters_shared_across_concurrent_sessions() {
        let template = make_minimal_core();
        // Two live session tasks (`SessionTask::new` clones the template).
        let session_a = template.clone();
        let session_b = template.clone();

        // Interleaved calls, exactly like two sessions reasoning at once.
        session_a.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 1_000,
            completion_tokens: 100,
            ..Default::default()
        });
        session_b.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 2_000,
            completion_tokens: 200,
            ..Default::default()
        });
        session_a.accumulate_llm_usage(&UsageInfo {
            prompt_tokens: 3_000,
            completion_tokens: 300,
            ..Default::default()
        });

        // Every reader — both sessions and the template — sees the sum of all
        // three calls (6_000 in / 600 out) at the moment it reads.
        for (who, core) in [
            ("session_a", &session_a),
            ("session_b", &session_b),
            ("template", &template),
        ] {
            assert_eq!(
                core.agent_token_totals(),
                (6_000, 600, 0, 0),
                "{who} must see the agent-wide total of all concurrent sessions"
            );
        }

        // A cold-start disk merge performed by the template (what
        // `list_sessions` does) is also immediately visible to both running
        // sessions — no re-clone, no restart.
        template.merge_token_totals((Some(50_000_000), None, None, None));
        assert_eq!(session_a.agent_token_totals().0, 50_000_000);
        assert_eq!(session_b.agent_token_totals().0, 50_000_000);
    }

    /// A provider adopted at runtime must be visible to every clone.
    ///
    /// The bug this pins: switching embedding model reached the session's own
    /// `AgentCore` but not the `Arc<AgentCore>` snapshot published for the
    /// conversation indexer and the HTTP layer, because `AgentCore` is `Clone`
    /// and the provider was a plain per-clone field. The symptom was silent:
    /// the indexer kept embedding with the previous model against a store that
    /// had already been re-embedded, its dimension guard deferred the sweep
    /// forever, and the memory panel reported a dimension the process no longer
    /// used.
    #[test]
    fn adopted_provider_is_visible_through_every_clone() {
        struct Switchable(usize);
        #[async_trait::async_trait]
        impl EmbeddingProvider for Switchable {
            fn name(&self) -> &str {
                "switchable-test"
            }
            async fn embed(&self, _t: &str) -> Result<Vec<f32>, acowork_core::embedding::EmbeddingError> {
                Ok(vec![0.0; self.0])
            }
            async fn embed_batch(&self, ts: &[&str]) -> Result<Vec<Vec<f32>>, acowork_core::embedding::EmbeddingError> {
                Ok(ts.iter().map(|_| vec![0.0; self.0]).collect())
            }
            fn dimension(&self) -> usize {
                self.0
            }
            async fn is_available(&self) -> bool {
                true
            }
        }

        let core = make_minimal_core();
        // The three real roles: the published snapshot, the session's working
        // copy, and any other clone derived before the switch.
        let published = Arc::new(core.clone());
        let mut session = core.clone();

        session.update_embedding_provider(Arc::new(Switchable(768)));
        assert_eq!(
            published.live_embedding_provider().map(|p| p.dimension()),
            Some(768),
            "readers of the published snapshot must see the adopted provider"
        );

        session.clear_embedding_provider();
        assert_eq!(
            published.live_embedding_provider().map(|p| p.dimension()),
            None,
            "clearing must propagate too, or the indexer embeds into a store              whose model is gone"
        );

        // The per-clone field is the trap the cell exists to avoid: it says
        // nothing to anybody else. Asserting it stays per-clone documents why
        // `live_embedding_provider` must be the read path for shared readers.
        let mut first = core.clone();
        let second = core.clone();
        first.update_embedding_provider(Arc::new(Switchable(512)));
        assert!(
            first.embedding_provider.is_some() && second.embedding_provider.is_none(),
            "the plain field is per-clone; shared readers must not use it"
        );
    }

    // ── resolve_summary_llm dispatch tests ──────────────────────────
    //
    // ADR-093 follow-up: the search-summary endpoint must prefer the
    // user's `default_compact_models` list (the same one compaction
    // uses — ADR-056) over the session's current model. These tests
    // cover the four cases the dispatch has to get right.

    /// Build a minimal `AgentCore` with one Ollama-protocol provider
    /// and one model in its capabilities. The local `base_url` makes
    /// `is_compact_provider_available` return true without any api-key
    /// vault entry, keeping the test self-contained.
    fn make_core_with_ollama_candidate(
        provider_id: &str,
        model_id: &str,
        context_window: u64,
    ) -> AgentCore {
        let config = RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.summary-resolver"
            version = "1.0.0"
            name = "Summary resolver test"
            description = "Tests resolve_summary_llm dispatch"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(MockProvider::single_text("test"));
        let core = AgentCore::new(config, manifest, provider, vec![]);
        let list_item = ProviderListItem {
            id: provider_id.to_string(),
            base_url: "http://localhost:11434".to_string(),
            protocol_type: acowork_core::protocol::ProtocolType::Ollama,
            models: vec![ProviderModelEntry {
                id: model_id.to_string(),
                capabilities: test_model_caps(context_window, 16_384),
                max_output_tokens_limit: 32_768,
            }],
            compact_model: None,
            custom: false,
        };
        *core.global_provider_list.write().unwrap() = vec![list_item];
        core
    }

    #[test]
    fn resolve_summary_llm_falls_back_to_session_when_candidates_empty() {
        // No global candidates → the previous behaviour (session model
        // wins) is preserved exactly.
        let core = make_core(Some(64_000), Some(64_000), Some(test_model_caps(64_000, 16_384)), 128_000);
        let (_provider, model, tier) = core.resolve_summary_llm(Some("session-pid"), Some("session-model"));
        assert_eq!(tier, SummaryModelTier::SessionCurrent);
        assert_eq!(model, "session-model");
    }

    #[test]
    fn resolve_summary_llm_falls_back_to_session_when_candidates_empty_and_no_session() {
        // Defensive: when the session slot is also empty, the resolver
        // must still return successfully (model becomes "default", the
        // agent's active provider is used). This is the pre-existing
        // `summary_llm_snapshot` contract.
        let core = make_core(Some(64_000), Some(64_000), Some(test_model_caps(64_000, 16_384)), 128_000);
        let (_provider, model, tier) = core.resolve_summary_llm(None, None);
        assert_eq!(tier, SummaryModelTier::SessionCurrent);
        assert_eq!(model, "default");
    }

    #[test]
    fn resolve_summary_llm_uses_global_candidate_when_provider_available() {
        let core = make_core_with_ollama_candidate("ollama-local", "qwen-1.5b", 32_000);
        *core.default_compact_models.write().unwrap() =
            vec![("ollama-local".to_string(), "qwen-1.5b".to_string())];

        let (_provider, model, tier) =
            core.resolve_summary_llm(Some("session-pid"), Some("session-model"));
        assert_eq!(tier, SummaryModelTier::GlobalDefault);
        assert_eq!(model, "qwen-1.5b");
    }

    #[test]
    fn resolve_summary_llm_skips_unavailable_candidate_and_uses_next() {
        // First candidate's provider is not in the list (and not a
        // configured local one) → `is_compact_provider_available`
        // returns false → resolver falls through to the next candidate.
        // Second candidate IS available → GlobalDefault wins.
        let core = make_core_with_ollama_candidate("ollama-local", "qwen-7b", 32_000);
        *core.default_compact_models.write().unwrap() = vec![
            ("missing-pid".to_string(), "missing-model".to_string()),
            ("ollama-local".to_string(), "qwen-7b".to_string()),
        ];

        let (_provider, model, tier) = core.resolve_summary_llm(None, None);
        assert_eq!(tier, SummaryModelTier::GlobalDefault);
        assert_eq!(model, "qwen-7b");
    }

    #[test]
    fn resolve_summary_llm_skips_candidate_when_model_not_in_capabilities() {
        // Provider is reachable, but the model id in the candidate row
        // does not match any model the provider exposes. The
        // capability lookup returns None → skip → fall through to the
        // next entry; if that fails too, fall through to session.
        let core = make_core_with_ollama_candidate("ollama-local", "qwen-7b", 32_000);
        *core.default_compact_models.write().unwrap() = vec![
            ("ollama-local".to_string(), "model-not-listed".to_string()),
        ];

        let (_provider, model, tier) =
            core.resolve_summary_llm(Some("session-pid"), Some("session-model"));
        assert_eq!(tier, SummaryModelTier::SessionCurrent);
        assert_eq!(model, "session-model");
    }
}
