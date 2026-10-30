//! Per-agent runtime configuration persistence.
//!
//! Stores per-agent config (max_output_tokens, max_iterations,
//! temperature, system_prompt_override, shell_approval_threshold)
//! as JSON in `{work_dir}/config/agent_config.json`.
//!
//! Also stores per-agent MCP server config in `{work_dir}/config/agent_mcp.json`
//! (dual-list format: `catalog` from Gateway + `local` from agent-installed tools)
//!
//! Model selection is per-session (ADR-012), persisted in JSONL SessionMetadata.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use indexmap::IndexMap;

use acowork_core::manifest::McpServerDecl;
use acowork_core::protocol::McpServerConfigDef;

/// Per-agent MCP config with dual-list format.
///
/// `catalog` is managed by Gateway (pushed via RuntimeConfigUpdate).
/// `local` is managed by mcp_install / mcp_uninstall tools.
/// Both lists are merged at startup and when applying MCP connections.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentMcpConfig {
    /// Gateway-managed catalog MCPs (pushed via RuntimeConfigUpdate).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub catalog: Vec<McpServerConfigDef>,
    /// Agent-installed local MCPs (managed by mcp_install / mcp_uninstall tools).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub local: Vec<McpServerConfigDef>,
    /// ADR-? / Win11-MCP-ToolsBugFix: user-selected subset of catalog names that
    /// are **active** for this agent. When `None` (e.g. older files written
    /// before the field existed) the merged list is treated as fully active
    /// for backward compatibility. When `Some(_)`, only matching entries from
    /// `merged()` are reported as active — e.g. `Some(vec![]) == "no servers
    /// active"`. This is the field persisted by `PUT /agents/{id}/mcp-servers`
    /// (per-agent activation toggle in the Desktop Tools panel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_names: Option<Vec<String>>,
    /// MCP servers the **package author** recommends (ADR-104), projected
    /// verbatim from `manifest.toml`'s `[[mcp_servers]]` on every Runtime
    /// start.
    ///
    /// This list is a *projection*, not state: it is rewritten from the
    /// manifest each boot (so an upgraded package refreshes its own
    /// recommendations) and it never grants anything. A server in here is
    /// exactly as usable as one the user typed into the Tools panel — it
    /// still has to be installed (preset scans) or activated per agent.
    /// Keeping it out of `catalog` / `local` is what makes "recommended"
    /// survive a user's delete of the catalog entry: the recommendation is
    /// re-derived from the package, the installation is not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended: Vec<McpServerDecl>,
    /// Recommendation keys (`McpServerDecl::key`) the user dismissed with
    /// "ignore" in the Tools panel. Purely a UI state with no runtime
    /// effect: the entry stays in `recommended` (it is manifest-derived and
    /// therefore not ours to delete) and the panel hides it. Kept so
    /// dismissal survives a restart, which is the whole point of a
    /// dismissal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dismissed: Vec<String>,
}

impl AgentMcpConfig {
    /// Merge catalog + local into a single flat list for MCP connection.
    /// Local entries take precedence over catalog entries with the same name.
    pub fn merged(&self) -> Vec<McpServerConfigDef> {
        let mut result = self.catalog.clone();
        // Local entries override catalog entries with the same name
        for local_entry in &self.local {
            if let Some(pos) = result.iter().position(|c| c.name == local_entry.name) {
                result[pos] = local_entry.clone();
            } else {
                result.push(local_entry.clone());
            }
        }
        result
    }

    /// Check whether a name exists in either catalog or local.
    pub fn contains_name(&self, name: &str) -> bool {
        self.catalog.iter().any(|c| c.name == name) || self.local.iter().any(|l| l.name == name)
    }

    /// Check whether a name exists in catalog only.
    pub fn is_catalog(&self, name: &str) -> bool {
        self.catalog.iter().any(|c| c.name == name)
    }

    /// Names of servers that should be considered **active** for this agent.
    ///
    /// Resolution order:
    /// 1. If `active_names` is `Some`, return that list verbatim (preserving
    ///    caller's ordering for the Tools panel display).
    /// 2. If `active_names` is `None` (file without the field — initial state
    ///    or legacy migration), return an empty list — no servers are active
    ///    by default. The user must explicitly enable servers via the Tools
    ///    panel.
    ///
    /// `Some(vec![])` therefore means **no servers active** (the user
    /// explicitly unchecked every catalog item).
    pub fn active_server_names(&self) -> Vec<String> {
        match &self.active_names {
            Some(names) => names.clone(),
            None => vec![],
        }
    }

    /// Catalog ∪ local, filtered by `active_server_names()`.
    ///
    /// **This is the single source of truth for "which MCP servers should
    /// be connected for this agent".** All MCP connection code paths
    /// (startup auto-connect, MCP config change watcher, etc.) MUST
    /// go through this method — never `merged()` directly — otherwise
    /// the user's Tools-panel toggles are silently ignored.
    ///
    /// Tool names whose entry appears in `merged()` but not in
    /// `active_server_names()` are filtered out. When
    /// `active_server_names()` is empty (either `None` legacy state or
    /// `Some(vec![])` user-unchecked-everything), the result is empty.
    pub fn active_merged(&self) -> Vec<McpServerConfigDef> {
        let active_names = self.active_server_names();
        let active: std::collections::HashSet<&str> =
            active_names.iter().map(String::as_str).collect();
        self.merged()
            .into_iter()
            .filter(|s| active.contains(s.name.as_str()))
            .collect()
    }

    /// Manifest-declared tool whitelist per recommended server, as
    /// `server_key -> [tool names]`.
    ///
    /// Consumed by `merge_mcp_tools_config` to seed `agent_mcp_tools.json`:
    /// a server with a non-empty whitelist starts with exactly those tools
    /// enabled (ADR-104 replaced the hardcoded `PM_DEFAULT_ENABLED_TOOLS`
    /// with this package-declared list). A server with no entry, or an
    /// empty whitelist, keeps the previous "everything enabled, user opts
    /// out" baseline — an empty whitelist is "no opinion", not "no tools".
    pub fn recommended_tools_map(&self) -> IndexMap<String, Vec<String>> {
        let mut out: IndexMap<String, Vec<String>> = IndexMap::new();
        for decl in &self.recommended {
            let Some(key) = decl.key() else { continue };
            if decl.tools.is_empty() {
                continue;
            }
            out.insert(key.to_string(), decl.tools.clone());
        }
        out
    }

    /// True when the user dismissed the recommendation with this key.
    pub fn is_dismissed(&self, key: &str) -> bool {
        self.dismissed.iter().any(|d| d == key)
    }

    /// Recommendation with this key, if the package declares one.
    pub fn recommendation(&self, key: &str) -> Option<&McpServerDecl> {
        self.recommended
            .iter()
            .find(|d| d.key() == Some(key))
    }
}

/// Per-agent configuration persisted to workspace/config/agent_config.json.
///
/// On first start, defaults are generated from manifest.toml and AgentHelloResult.
/// User modifications via the Desktop App are persisted here by the Runtime
/// when Gateway pushes a RuntimeConfigUpdate.
///
/// MCP server configurations are stored separately in agent_mcp.json
/// (see `load_agent_mcp_config` / `save_agent_mcp_config`) per the
/// `agent_*.json` naming convention for per-agent config snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentConfig {
    /// Max output tokens per request (None = use global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,

    /// Max LLM iterations per run (None = use global default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,

    /// LLM sampling temperature (0.0 = deterministic, 2.0 = max creative).
    ///
    /// Resolution chain at runtime (Layer 1 = highest priority):
    /// 1. **this field** — user's agent-level setting (set via Agent Setup panel)
    /// 2. `manifest.llm.temperature` — package author default (shipped with agent)
    /// 3. `crate::config::DEFAULT_TEMPERATURE` — hardcoded final fallback (0.3)
    ///
    /// `None` means "I don't have an opinion" — fall through to the next level.
    /// The user can clear this value in the UI to revert to the manifest default
    /// (analogous to how `avatar` reverts when cleared).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    /// Per-agent context window size limit in tokens.
    ///
    /// Resolution chain at runtime (Layer 1 = highest priority):
    /// 1. **this field** — user's agent-level setting (set via Agent Setup panel)
    /// 2. `manifest.llm.context_window` — package author default
    /// 3. `crate::config::DEFAULT_CONTEXT_WINDOW` — hardcoded final fallback (200K)
    ///
    /// `None` means "I don't have an opinion" — fall through to the next level.
    /// `Some(0)` is invalid under ADR-074 §6 (the old "no limit" sentinel is
    /// abolished) — the resolution chain skips this layer. Existing `0` in a
    /// config file is tolerated on load (not repaired, see ADR-074 §9) but
    /// never treated as an effective value.
    /// The user can clear this value in the UI to revert to the manifest default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,

    /// System prompt override (None = use manifest-compiled prompt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,

    /// Shell command approval threshold ("low" | "medium" | "high" | "auto_approve"; legacy "never" accepted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<String>,

    /// Maximum number of conversation sessions to keep on disk.
    /// When exceeded at session creation, the oldest sessions are archived.
    /// None = use system default (1000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,

    // NOTE: the avatar did NOT live here. ADR-009 §5 moved the user's
    // avatar / display-name pick into `AgentOverrides`
    // (`{instance_id}.overrides.json`, sibling of the package dir) — see
    // `acowork_core::agent_overrides` and `http::avatar`. Putting it back
    // in `agent_config.json` would (a) make it the Gateway's business
    // again and (b) lose it on every upgrade, since `work_dir` sits
    // inside the instance dir.
    /// Approval timeout in seconds for loop approval. None = use system default (300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout_secs: Option<u64>,

    /// Minimum compression ratio for ADR-061 context compaction (levels 1-7).
    ///
    /// Expressed as the *saved* share: 0.90 means "compress until at most
    /// 10% of the history remains" (e.g. 200K → 20K). `None` = use
    /// `crate::agent::compression_constants::MIN_COMPRESSION_RATIO` (0.90
    /// default). Configured via the Agent Setup panel; valid range 0.05–0.95.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_ratio_threshold: Option<f64>,

    // ADR-101: the distiller schedule and the episodic-decay policy belong to
    // the memory service (its own `consolidation.json` is the write target), so
    // the ten `distiller_*` / `memory_forgetting_*` fields that used to sit here
    // are gone. `agent_config.json` files written before the move still parse —
    // serde drops the unknown keys and the next save rewrites the file clean.

    /// Per-agent LLM session language override (e.g. `"zh-CN"`, `"en"`).
    ///
    /// When `Some(_)`, this value replaces `UserProfile.language` in the
    /// identity-context text block handed to the LLM (see
    /// `SessionManager::format_user_profile_context`), so the agent
    /// answers in the user's chosen language *for this agent only* —
    /// independent of the global UI language set in
    /// `Settings → Profile`. `None` = fall through to
    /// `UserProfile.language` (the existing default behaviour).
    ///
    /// This only affects the language hint baked into the system prompt;
    /// it does NOT change the desktop UI language (i18n stays bound to
    /// `UserProfile.language`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_language: Option<String>,

    // ── Global-search retrieval config (ADR-095) ─────────────────────
    //
    // Layer 1 of the search retrieval policy chain, same convention as the
    // distiller/forgetting fields above. `None` (the default, and what every
    // pre-ADR-095 `agent_config.json` contains) means "use the ADR-specified
    // starting points", which are today's production behaviour for
    // conversation and the `memory_recall` 4:1 tendency for memory. The
    // analyzer is inert unless `analyzer_mode` is set — shipping this code
    // must not change retrieval (ADR-095 D6 / §7 rollback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<crate::search::policy::SearchPolicyConfig>,

    // ── Chat Router (ADR-099 / design 29) ────────────────────────────
    //
    // Layer 1 of the router config chain; `None` = the ADR-099 defaults
    // (enabled, compact model, 8 s timeout, 24-message window, 10
    // candidates). Every field is optional so an existing
    // `agent_config.json` keeps working untouched, and tests can switch
    // the router off without touching a code path.
    /// Chat Router tuning knobs. `None` = design defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_router: Option<ChatRouterConfig>,
}

/// Chat Router configuration (ADR-099 / design 29).
///
/// Model resolution chain: `model` -> the agent's compact-model chain
/// (`default_compact_models` -> first provider model) -> the agent's
/// default model. `enabled = false` is the kill switch: a Chat message is
/// routed by the deterministic rule of ADR-098 §6.1 (registry key → mapped
/// session → create-on-miss) inline on the inbound task, never queued, which
/// is what the E2E suites rely on. It is not byte-identical to that era:
/// T3 (config-conflict rotation) is retired by ADR-099 D3, so an incoming
/// `session_config` now updates the mapped session — effective from its next
/// turn — instead of forcing a new one.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatRouterConfig {
    /// Master switch. `None` = enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Explicit router model id. `None` = compact model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Explicit router provider. `None` = whichever provider serves `model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Router call timeout in ms. `None` = 60,000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Chat messages projected into the router context. `None` = 32.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_window: Option<usize>,
    /// Maximum candidates shown to the router. `None` = 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_candidates: Option<usize>,
}

impl ChatRouterConfig {
    pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
    pub const DEFAULT_HISTORY_WINDOW: usize = 32;
    pub const DEFAULT_MAX_CANDIDATES: usize = 10;

    /// Master switch, defaulting to on.
    pub fn is_enabled(&self) -> bool {
        self.enabled.unwrap_or(true)
    }

    /// Clamped call timeout; a zero / absurd value must never disable the
    /// router by making every call time out instantly. The ceiling stays
    /// strictly below `provider_request_timeout_ms` (default 600s,
    /// `timeouts` in runtime.toml) so a slow provider surfaces as a
    /// provider error — the router's own deadline firing first would erase
    /// that distinction.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
            .unwrap_or(Self::DEFAULT_TIMEOUT_MS)
            .clamp(1_000, 300_000)
    }

    /// Clamped history window. The lower bound keeps enough turns for a
    /// follow-up to be recognisable; the upper bound keeps one router turn
    /// cheap regardless of what a hand-edited config says.
    pub fn history_window(&self) -> usize {
        self.history_window
            .unwrap_or(Self::DEFAULT_HISTORY_WINDOW)
            .clamp(4, 200)
    }

    /// Clamped candidate count.
    pub fn max_candidates(&self) -> usize {
        self.max_candidates
            .unwrap_or(Self::DEFAULT_MAX_CANDIDATES)
            .clamp(1, 50)
    }
}

// ADR-094 note: there is no `skills_auto_load` field on
// `AgentConfig` any more. The Progressive/Manual mode decision
// is driven entirely by `agent_tools.json::load_skill.enabled`
// — see `cli::resolve_skill_mode` for the priority chain.

impl AgentConfig {
    /// Effective Chat Router config: the stored block or the design defaults.
    pub fn chat_router_config(&self) -> ChatRouterConfig {
        self.chat_router.clone().unwrap_or_default()
    }
}

/// Filename for per-agent config in the workspace config directory.
const AGENT_CONFIG_FILE: &str = "agent_config.json";

/// Build the path to the agent config file.
fn config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_CONFIG_FILE)
}

/// Load per-agent config from workspace/config/agent_config.json.
///
/// Returns `None` if the file does not exist (first start).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_config(work_dir: &Path) -> Result<Option<AgentConfig>, String> {
    let path = config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        "Loaded agent config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent config to workspace/config/agent_config.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_config(work_dir: &Path, cfg: &AgentConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        "Saved agent config to workspace"
    );

    Ok(())
}

// ── Manifest → agent_config.json seeding (ADR-104) ─────────────────────

impl AgentConfig {
    /// Apply a manifest's declared defaults to every field this file owns.
    ///
    /// **The only place a manifest value enters `agent_config.json`.** The
    /// manifest is a *seed*, not a second source of truth: a field the file
    /// already sets is never touched, so the user's choice always wins and
    /// this function is idempotent (calling it twice changes nothing the
    /// second time).
    ///
    /// Returns `true` when at least one field was filled, i.e. the caller
    /// should persist.
    pub fn seed_from_manifest(
        &mut self,
        defaults: &acowork_core::manifest::ManifestConfigDefaults,
    ) -> bool {
        let mut changed = false;
        fill_if_empty(&mut self.temperature, defaults.temperature, &mut changed);
        fill_if_empty(
            &mut self.context_window,
            defaults.context_window,
            &mut changed,
        );
        fill_if_empty(
            &mut self.max_output_tokens,
            defaults.max_output_tokens,
            &mut changed,
        );
        fill_if_empty(
            &mut self.max_iterations,
            defaults.max_iterations,
            &mut changed,
        );
        fill_if_empty(
            &mut self.compression_ratio_threshold,
            defaults.compression_ratio_threshold,
            &mut changed,
        );
        changed
    }
}

/// The "manifest never overwrites a user value" invariant, in one place.
fn fill_if_empty<T: Copy>(slot: &mut Option<T>, value: Option<T>, changed: &mut bool) {
    if slot.is_none()
        && let Some(v) = value
    {
        *slot = Some(v);
        *changed = true;
    }
}

/// Load `agent_config.json`, seed it from the manifest, persist when the
/// seed changed anything. Mirrors `seed_agent_config_from_manifest` for
/// call sites that do not already hold a loaded config (install, package
/// upgrade). The startup path calls [`AgentConfig::seed_from_manifest`]
/// directly because it also resolves the non-manifest fallbacks.
pub fn seed_agent_config_from_manifest(
    work_dir: &Path,
    manifest: &acowork_core::manifest::AgentManifest,
) -> Result<bool, String> {
    let defaults = manifest.config_defaults();
    if defaults.is_empty() {
        return Ok(false);
    }
    let mut cfg = load_agent_config(work_dir)?.unwrap_or_default();
    if !cfg.seed_from_manifest(&defaults) {
        return Ok(false);
    }
    save_agent_config(work_dir, &cfg)?;
    tracing::info!(
        work_dir = %work_dir.display(),
        "Seeded agent_config.json from manifest [config]"
    );
    Ok(true)
}

/// Project the manifest's `[[mcp_servers]]` into `agent_mcp.json`'s
/// `recommended` list.
///
/// The list is *derived state*: it is rewritten on every call so a package
/// upgrade refreshes its own recommendations, and entries the user dismissed
/// are dropped from `dismissed` as soon as the package stops declaring them
/// (otherwise a stale key would silence a future recommendation forever).
/// Nothing else in the file is touched — `catalog` / `local` / `active_names`
/// keep their own owners (ADR-090).
///
/// Returns `true` when the file changed and was persisted.
pub fn project_recommended_mcp_servers(
    work_dir: &Path,
    manifest: &acowork_core::manifest::AgentManifest,
) -> Result<bool, String> {
    let mut cfg = load_agent_mcp_config(work_dir)?.unwrap_or_default();

    let declared: Vec<McpServerDecl> = manifest
        .mcp_servers
        .iter()
        .filter(|d| d.key().is_some())
        .cloned()
        .collect();
    let declared_keys: std::collections::HashSet<&str> =
        declared.iter().filter_map(|d| d.key()).collect();
    let dismissed: Vec<String> = cfg
        .dismissed
        .iter()
        .filter(|k| declared_keys.contains(k.as_str()))
        .cloned()
        .collect();

    if cfg.recommended == declared && cfg.dismissed == dismissed {
        return Ok(false);
    }

    cfg.recommended = declared;
    cfg.dismissed = dismissed;
    save_agent_mcp_config(work_dir, &cfg)?;
    tracing::info!(
        work_dir = %work_dir.display(),
        recommended = cfg.recommended.len(),
        dismissed = cfg.dismissed.len(),
        "Projected manifest [[mcp_servers]] into agent_mcp.json"
    );
    Ok(true)
}

// ── Per-agent builtin-tools config (ADR-029) ───────────────────────────

/// Tools whose operation is required for the Chat ↔ Execution session
/// architecture to function. Disabling them via the user-facing toggle
/// would silently break `chat_history_read` (context retrieval) or
/// `intent_send` (agent-to-agent communication). They are always included
/// in the dispatch list regardless of the persisted `enabled` flag.
///
/// See [`AgentCore::rebuild_all_tools`] which consumes this list.
pub const PROTECTED_TOOL_NAMES: &[&str] = &["chat_history_read", "intent_send"];

/// Returns `true` when `name` must always be enabled (cannot be turned off
/// by the user or a manifest omission).
pub fn is_protected_tool(name: &str) -> bool {
    PROTECTED_TOOL_NAMES.contains(&name)
}

/// Per-agent builtin tools enable/disable configuration.
///
/// Persisted to `{work_dir}/config/agent_tools.json`.
/// Contains ALL builtin tools — each with an `enabled` flag. The file
/// is the single source of truth once it exists; on first start it is
/// generated from `manifest.toml` `[[tools]]` declarations (or, if no
/// `[[tools]]` is present, every builtin tool is enabled by default).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentToolsConfig {
    /// All builtin tools with their enable status.
    /// Must include every registered builtin tool — call sites
    /// reconcile this list against `all_builtin_tools()` at load time.
    #[serde(default)]
    pub tools: Vec<AgentToolEntry>,
}

/// A single builtin tool entry in `agent_tools.json`.
///
/// Wire/JSON shape (uniform across persisted file and API response):
/// `{ "name": "...", "enabled": bool }`. No extra presentation hint
/// fields — `AgentToolEntry` is plain persisted state.
///
/// **Platform-protected tools** ([`PROTECTED_TOOL_NAMES`]) are always
/// included in the dispatch list and LLM tool definitions regardless of
/// their persisted `enabled` flag. The persistence layer still stores
/// the user's intent, but `rebuild_all_tools` and
/// `rebuild_context_tool_definitions` override it for these names.
//
/// See ADR-029 (per-agent builtin tools) and ADR-052 (tool compression).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolEntry {
    /// Tool name (matches `Tool::name()`).
    pub name: String,
    /// Whether this tool is enabled for the agent.
    /// Defaults to `true` when missing in JSON for backward-compatible
    /// additions of new fields.
    #[serde(default = "default_tool_entry_enabled")]
    pub enabled: bool,
}

fn default_tool_entry_enabled() -> bool {
    true
}

impl AgentToolEntry {
    /// Construct a new entry with the given enabled flag.
    pub fn new(name: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            enabled,
        }
    }
}

/// Filename for per-agent builtin-tools config in the workspace config dir.
const AGENT_TOOLS_CONFIG_FILE: &str = "agent_tools.json";

/// Build the path to the agent builtin-tools config file.
fn tools_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_TOOLS_CONFIG_FILE)
}

/// Load per-agent builtin-tools config from
/// `workspace/config/agent_tools.json`.
///
/// Returns `Ok(None)` if the file does not exist (first start — caller
/// must then initialize from `manifest.toml` or default-enable every
/// tool).
pub fn load_agent_tools_config(work_dir: &Path) -> Result<Option<AgentToolsConfig>, String> {
    let path = tools_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentToolsConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;

    tracing::info!(
        work_dir = %work_dir.display(),
        tool_count = cfg.tools.len(),
        "Loaded agent builtin-tools config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent builtin-tools config to
/// `workspace/config/agent_tools.json`. Atomic write-tmp-rename.
pub fn save_agent_tools_config(work_dir: &Path, cfg: &AgentToolsConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = tools_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent tools config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        tool_count = cfg.tools.len(),
        "Saved agent builtin-tools config to workspace"
    );

    Ok(())
}

/// Ensure a tool entry exists in `agent_tools.json`.
///
/// If the tool is already present, its `enabled` flag is preserved
/// (respecting the user's preference). If absent, a new entry is
/// appended with `default_enabled`.
///
/// Used by the `SidecarEndpointUpdate` handler to persist dynamically
/// registered tools so that the frontend tool panel and
/// `ConfigSnapshot` queries reflect tools that were not available at
/// startup. Currently unreferenced (`codebase`, the only dynamic tool,
/// is temporarily unregistered) — kept because the dynamic-registration
/// plumbing in `SessionManager` is intact.
///
/// No-op if `agent_tools.json` does not exist yet (it will be created
/// at next startup via the normal `agent_init.rs` flow).
pub fn ensure_tool_in_config(work_dir: &Path, tool_name: &str, default_enabled: bool) {
    match load_agent_tools_config(work_dir) {
        Ok(Some(mut cfg)) => {
            if let Some(existing) = cfg.tools.iter().find(|e| e.name == tool_name) {
                // Already present - respect user's enabled/disabled preference.
                tracing::info!(
                    tool = tool_name,
                    existing_enabled = existing.enabled,
                    default_enabled,
                    "ensure_tool_in_config: tool already present, preserving existing enabled flag"
                );
                return;
            }
            cfg.tools
                .push(AgentToolEntry::new(tool_name, default_enabled));
            tracing::info!(
                tool = tool_name,
                default_enabled,
                "Adding dynamically registered tool to agent_tools.json"
            );
            if let Err(e) = save_agent_tools_config(work_dir, &cfg) {
                tracing::warn!(
                    error = %e,
                    tool = tool_name,
                    "Failed to persist dynamic tool addition to agent_tools.json"
                );
            }
        }
        Ok(None) => {
            tracing::warn!(
                tool = tool_name,
                work_dir = %work_dir.display(),
                "agent_tools.json not found; skipping dynamic tool persistence \
                 (will be created at next startup)"
            );
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                tool = tool_name,
                "Failed to load agent_tools.json for dynamic tool persistence"
            );
        }
    }
}

/// Remove a tool entry from `agent_tools.json`.
///
/// Used by the `SidecarEndpointUpdate` handler when a sidecar goes
/// away (e.g. LSP Relay stopped) so the frontend tool panel no longer
/// shows the now-unavailable tool.
///
/// No-op if the file or the tool entry does not exist.
pub fn remove_tool_from_config(work_dir: &Path, tool_name: &str) {
    match load_agent_tools_config(work_dir) {
        Ok(Some(mut cfg)) => {
            let before = cfg.tools.len();
            cfg.tools.retain(|e| e.name != tool_name);
            if cfg.tools.len() < before {
                tracing::info!(
                    tool = tool_name,
                    "Removing dynamically unregistered tool from agent_tools.json"
                );
                if let Err(e) = save_agent_tools_config(work_dir, &cfg) {
                    tracing::warn!(
                        error = %e,
                        tool = tool_name,
                        "Failed to persist dynamic tool removal to agent_tools.json"
                    );
                }
            }
        }
        Ok(None) => {
            // File doesn't exist - nothing to remove.
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                tool = tool_name,
                "Failed to load agent_tools.json for dynamic tool removal"
            );
        }
    }
}

/// Merge a persisted config with the current set of code-registered
/// builtin tools, producing a fresh list of entries keyed off the
/// *code* registry (not the persisted file) so unknown tools are
/// silently dropped on next save.
///
/// Rules:
/// - Tools present in both -> use persisted `enabled` (user's choice
///   is the single source of truth; never overwrite)
/// - Tools only in code (newly added by Runtime upgrade) -> appended
///   with `enabled = false` (opt-in: new tools require explicit
///   enablement via manifest or frontend)
/// - Tools only in persisted file -> dropped **unless** the name is a
///   conditionally-registered builtin tool (see
///   [`crate::tools::builtin::CONDITIONALLY_REGISTERED_TOOL_NAMES`]:
///   `rag_query`). Those are valid tools that
///   may simply not be registered *this boot* (their dependency arrives
///   asynchronously after startup), so their persisted `enabled` flag is
///   preserved — dropping it would erase the user's preference. Only
///   genuinely-unknown names (removed by a Runtime upgrade) are dropped.
/// - Platform-protected tool filtering was retired along with the
///   `context_retrieve` / `context_abandon` tool surface (`context_abandon`
///   alone survives, as dead code); the registry now passes entries
///   through verbatim.
pub fn merge_tools_config(
    code_tool_names: &[String],   // from `all_builtin_tools()` registry
    persisted: &[AgentToolEntry], // from agent_tools.json
) -> Vec<AgentToolEntry> {
    let persisted_map: std::collections::HashMap<&str, bool> = persisted
        .iter()
        .map(|e| (e.name.as_str(), e.enabled))
        .collect();

    let code_set: std::collections::HashSet<&str> =
        code_tool_names.iter().map(|s| s.as_str()).collect();

    // Conditionally-registered builtin tools (`rag_query`) are valid
    // tools that may simply not be in the code registry *this boot*
    // (its RAG manifest entry can arrive asynchronously after
    // startup). Their persisted entries must survive the merge, or the
    // startup write-back below would erase the user's preference.
    // See [`crate::tools::builtin::CONDITIONALLY_REGISTERED_TOOL_NAMES`].
    let conditional_set: std::collections::HashSet<&str> =
        crate::tools::builtin::CONDITIONALLY_REGISTERED_TOOL_NAMES
            .iter()
            .copied()
            .collect();

    // Log tools that are in persisted but NOT in code registry. Only
    // genuinely-unknown names (removed by a Runtime upgrade) are
    // dropped; conditionally-registered tools are preserved below.
    for entry in persisted {
        if !code_set.contains(entry.name.as_str()) {
            if conditional_set.contains(entry.name.as_str()) {
                tracing::info!(
                    tool = %entry.name,
                    enabled = entry.enabled,
                    "merge_tools_config: preserving conditionally-registered tool not in code registry this boot"
                );
            } else {
                tracing::warn!(
                    tool = %entry.name,
                    enabled = entry.enabled,
                    "merge_tools_config: DROPPING persisted tool not in code registry"
                );
            }
        }
    }
    // Log tools that are in code but NOT in persisted (new tools, default enabled=false)
    for name in code_tool_names {
        if !persisted_map.contains_key(name.as_str()) {
            tracing::info!(
                tool = %name,
                "merge_tools_config: new code-registered tool, defaulting to enabled=false"
            );
        }
    }

    let mut merged: Vec<AgentToolEntry> = code_tool_names
        .iter()
        .map(|name| {
            let user_wants = persisted_map.get(name.as_str()).copied().unwrap_or(false); // new tool → disabled (opt-in)
            AgentToolEntry::new(name, user_wants)
        })
        .collect();

    // Preserve persisted entries for conditionally-registered tools that
    // are absent from the code registry this boot. Their `enabled` flag
    // is the user's single source of truth and must survive the startup
    // merge + write-back (`agent_init.rs`). They are safe to carry in
    // the merged output: `ToolRegistry::activate` only consults
    // `enabled_entries` for tools actually in the registry, so an entry
    // for a not-yet-registered tool is inert until the tool registers.
    for entry in persisted {
        if conditional_set.contains(entry.name.as_str()) && !code_set.contains(entry.name.as_str())
        {
            merged.push(entry.clone());
        }
    }

    merged
}

/// Apply a partial `RuntimeConfigUpdate.builtin_tools_enabled` payload
/// (only listed tool names are touched) onto an existing
/// `AgentToolsConfig` in place. Returns the new full list — a copy of
/// the caller may then persist via `save_agent_tools_config`.
///
/// Tool names in `patch` that are not present in `current` are
/// silently ignored (defensive: future tools should arrive via
/// `merge_tools_config` at startup, not via this incremental path).
///
/// Platform-protected tool names are filtered out of the output: they
/// are not in `current` (the previous merge already removed them) and
/// must never enter the file even via this path — see ADR-052.
/// Apply a partial `RuntimeConfigUpdate.builtin_tools_enabled` payload
/// (only listed tool names are touched) onto an existing
/// `AgentToolsConfig` in place. Returns the new full list — a copy of
/// the caller may then persist via `save_agent_tools_config`.
///
/// Tool names in `patch` that are not present in `current` are
/// silently ignored (defensive: future tools should arrive via
/// `merge_tools_config` at startup, not via this incremental path).
pub fn apply_builtin_tools_patch(
    current: &[AgentToolEntry],
    patch: &[AgentToolEntry],
) -> Vec<AgentToolEntry> {
    let patch_map: std::collections::HashMap<&str, bool> =
        patch.iter().map(|e| (e.name.as_str(), e.enabled)).collect();

    current
        .iter()
        .map(|e| {
            let patched = patch_map.get(e.name.as_str()).copied().unwrap_or(e.enabled);
            AgentToolEntry::new(&e.name, patched)
        })
        .collect()
}

/// Initialize an `AgentToolsConfig` from a `manifest.toml`
/// `[[tools]]` tool-names list. Any builtin tool whose name appears
/// in the manifest gets `enabled = true`; everything else gets
/// `enabled = false`. Used when `agent_tools.json` is absent.
pub fn init_tools_config_from_manifest(
    code_tool_names: &[String],
    manifest_tool_names: &[String],
) -> Vec<AgentToolEntry> {
    let manifest_set: std::collections::HashSet<&str> =
        manifest_tool_names.iter().map(|s| s.as_str()).collect();

    code_tool_names
        .iter()
        .map(|name| {
            let enabled = manifest_set.contains(name.as_str());
            AgentToolEntry::new(name, enabled)
        })
        .collect()
}

/// Build the canonical list of builtin tools when no manifest
/// declaration is present: every tool enabled by default
/// (backward-compatible default behavior).
pub fn all_enabled_tools_config(code_tool_names: &[String]) -> Vec<AgentToolEntry> {
    code_tool_names
        .iter()
        .map(|name| AgentToolEntry::new(name, true))
        .collect()
}

// ── Per-agent MCP config (dual-list: catalog + local) ──────────────────

/// Filename for per-agent MCP config in the workspace config directory.
const AGENT_MCP_CONFIG_FILE: &str = "agent_mcp.json";

/// Build the path to the agent MCP config file.
fn mcp_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_MCP_CONFIG_FILE)
}

/// Load per-agent MCP config from workspace/config/agent_mcp.json.
///
/// Returns `None` if the file does not exist (no MCP servers configured).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_mcp_config(work_dir: &Path) -> Result<Option<AgentMcpConfig>, String> {
    let path = mcp_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    // Try new dual-list format first
    if let Ok(cfg) = serde_json::from_str::<AgentMcpConfig>(&raw) {
        tracing::info!(
            work_dir = %work_dir.display(),
            catalog_count = cfg.catalog.len(),
            local_count = cfg.local.len(),
            "Loaded agent MCP config (dual-list format) from workspace"
        );
        return Ok(Some(cfg));
    }

    // Fall back to old format (flat Vec) — migrate all entries to catalog
    if let Ok(old_servers) = serde_json::from_str::<Vec<McpServerConfigDef>>(&raw) {
        tracing::info!(
            work_dir = %work_dir.display(),
            old_count = old_servers.len(),
            "Migrating agent MCP config from old flat format to dual-list format"
        );
        let migrated = AgentMcpConfig {
            catalog: old_servers,
            local: Vec::new(),
            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        // Auto-save in the new format so we don't need to migrate again
        let _ = save_agent_mcp_config(work_dir, &migrated);
        return Ok(Some(migrated));
    }

    Err(format!(
        "Failed to parse {} as either AgentMcpConfig or Vec<McpServerConfigDef>",
        path.display()
    ))
}

/// Save full per-agent MCP config to workspace/config/agent_mcp.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_mcp_config(work_dir: &Path, cfg: &AgentMcpConfig) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = mcp_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent MCP config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        catalog_count = cfg.catalog.len(),
        local_count = cfg.local.len(),
        "Saved agent MCP config to workspace"
    );

    Ok(())
}

/// Add `name` to `active_names` (initializing to an empty list if unset),
/// unless already present.
///
/// Used by `mcp_install` so a freshly installed local MCP is **active by
/// default** — the user installed it because they intend to use it. The
/// Tools-panel toggle can still turn it off afterwards (`active_names`
/// persists the choice).
pub fn activate_mcp_name(active_names: &mut Option<Vec<String>>, name: &str) {
    let active = active_names.get_or_insert_with(Vec::new);
    if !active.iter().any(|n| n == name) {
        active.push(name.to_string());
    }
}

/// System-injected MCP server names that are **active by default**.
///
/// `pm` is injected by the Gateway into every agent's catalog
/// (`auto_inject_mcp = true`, design §6.1 / T3-4) so agents get `pm_*`
/// tools out of the box. The user can still toggle it off in the Tools
/// panel — the choice is persisted in `active_names` and never clobbered
/// by subsequent catalog pushes.
fn is_system_injected_mcp_name(name: &str) -> bool {
    name == "pm"
}

/// Save only the catalog portion of agent MCP config.
///
/// This is used by the `acowork/global/mcps` MQTT handler: Gateway pushes
/// catalog MCPs, and we must preserve the user's `active_names` selection
/// and the `local` list (agent-installed MCPs). Reads the current config,
/// replaces only `catalog`, and saves back.
///
/// Note: previously this also reset `active_names = None`, which clobbered
/// the user's Tools-panel selection on every Gateway catalog push (e.g.
/// every Gateway restart). That reset is now gone — the catalog update is
/// orthogonal to the activation toggle.
///
/// Default-on (T3-4): when the user has **never expressed an activation
/// choice** (`active_names` is `None`) and the pushed catalog contains a
/// system-injected MCP (e.g. `pm`), we materialize it as active **once**.
/// After that `active_names` is `Some(_)` and every later push preserves
/// the user's selection verbatim — so a user who toggles `pm` off is not
/// re-activated by the next Gateway restart.
pub fn save_agent_mcp_config_catalog(
    work_dir: &Path,
    catalog_servers: &[McpServerConfigDef],
) -> Result<(), String> {
    // Load current config to preserve local entries AND active_names.
    let current = load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_mcp.json, using default");
            None
        })
        .unwrap_or_default();

    let mut active_names = current.active_names;
    if active_names.is_none() {
        let system: Vec<String> = catalog_servers
            .iter()
            .filter(|s| is_system_injected_mcp_name(&s.name))
            .map(|s| s.name.clone())
            .collect();
        if !system.is_empty() {
            tracing::info!(
                system_mcps = ?system,
                "Default-activating system-injected MCPs (active_names was unset)"
            );
            active_names = Some(system);
        }
    }

    let updated = AgentMcpConfig {
        catalog: catalog_servers.to_vec(),
        local: current.local,
        active_names,
        // Manifest-derived projection: owned by
        // `project_recommended_mcp_servers`, never by the catalog push.
        recommended: current.recommended,
        dismissed: current.dismissed,
    };

    save_agent_mcp_config(work_dir, &updated)
}

/// Load active MCP configs (catalog ∪ local, filtered by `active_names`)
/// from workspace/config/agent_mcp.json.
///
/// **This is the single entry point for MCP connection code.** It honors
/// the user's Tools-panel toggles via `active_server_names()`. Returns an
/// empty vec if no servers are active (either no file, legacy state with
/// `active_names = None`, or user explicitly unchecked everything).
pub fn load_active_mcp_configs(work_dir: &Path) -> Vec<McpServerConfigDef> {
    load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_mcp.json for active-merge, using empty list");
            None
        })
        .unwrap_or_default()
        .active_merged()
}

/// Load merged MCP configs (catalog + local) from workspace/config/agent_mcp.json.
///
/// **Deprecated**: this returns the unconditional union of catalog + local
/// and IGNORES `active_names`. New code MUST use [`load_active_mcp_configs`]
/// so users' Tools-panel toggles are honored at MCP connection time.
///
/// Kept only for the persistence / introspection layer that legitimately
/// needs to see the unconditional merged list (e.g. validating that a
/// user-selected name is in fact present in catalog ∪ local). The deprecation
/// lint will catch any new caller trying to use it for connection.
#[deprecated(
    since = "0.1.0",
    note = "ignores active_names — use load_active_mcp_configs so user toggles are honored"
)]
pub fn load_merged_mcp_configs(work_dir: &Path) -> Vec<McpServerConfigDef> {
    load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to load agent_mcp.json for merge, using empty list");
            None
        })
        .unwrap_or_default()
        .merged()
}

// ── Per-agent MCP tools allowlist (ADR-069) ────────────────────────────
//
// Companion of `AgentMcpConfig` (above) which gates **server-level**
// activation. This section gates **tool-level** activation within an
// active server — the same per-tool opt-in model that ADR-029 brings to
// builtin tools, but applied per-MCP-server.
//
// File: workspace/config/agent_mcp_tools.json
// Schema (v2 — flat per-tool, three-way identity with the GET response
// and PUT request body so the frontend never hardcodes a tool list):
//
// ```jsonc
// {
//   "servers": {
//     "pm": [
//       { "name": "pm_list_my_tasks",  "enabled": true,  "description": "..." },
//       { "name": "pm_claim_task",     "enabled": true,  "description": "..." },
//       { "name": "pm_create_project", "enabled": false, "description": "..." }
//     ]
//   }
// }
// ```
//
// The frontend never hardcodes any tool name; the runtime reconciles
// persisted choices with the live `tools/list` from each connected
// server at every startup / hot-reload and writes the canonical flat
// list back to `agent_mcp_tools.json`. The frontend reads that file via
// GET and renders directly.

/// Filename for the per-agent MCP tools config.
const AGENT_MCP_TOOLS_CONFIG_FILE: &str = "agent_mcp_tools.json";

fn mcp_tools_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_MCP_TOOLS_CONFIG_FILE)
}

/// Per-agent MCP tools config (flat per-server list).
///
/// Wire shape matches the GET response and PUT request body — three-way
/// identity between `agent_mcp_tools.json`, the HTTP wire format, and
/// the desktop render.
///
/// `deny_unknown_fields` ensures a v1-shape file
/// (`{"pm": {"enabled_tools": [...]}}`) fails to parse rather than
/// silently mapping to an empty config — there is no automatic
/// migration by design (project is in active development).
///
/// ADR-069 follow-up: `servers` is `IndexMap`, not `HashMap`. Serde
/// serialises the file in iteration order; `HashMap` reshuffles on
/// every reconcile pass, making the Tools-panel tool order reshuffle
/// whenever the user toggles a single tool. `IndexMap` preserves the
/// order `merge_mcp_tools_config` produced (= MCP server's
/// `tools/list` order).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentMcpToolsConfig {
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub servers: IndexMap<String, Vec<AgentMcpToolItem>>,
}

/// Single MCP tool row inside a server's flat list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMcpToolItem {
    pub name: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AgentMcpToolItem {
    pub fn new(name: impl Into<String>, enabled: bool) -> Self {
        Self {
            name: name.into(),
            enabled,
            description: None,
        }
    }

    pub fn with_description(
        name: impl Into<String>,
        enabled: bool,
        description: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            enabled,
            description,
        }
    }
}

/// Look up a single tool's `enabled` flag from the flat config.
/// Returns `Some(enabled)` if the row exists, `None` if absent.
pub fn tool_enabled_in(tools: &[AgentMcpToolItem], tool_name: &str) -> Option<bool> {
    tools
        .iter()
        .find(|t| t.name == tool_name)
        .map(|t| t.enabled)
}

/// Minimal MCP tool descriptor used as the input shape for
/// `merge_mcp_tools_config`. Keeps `agent_config` independent of the
/// `acowork-mcp` crate's wire types.
#[derive(Debug, Clone)]
pub struct McpToolDescriptor {
    pub name: String,
    pub description: Option<String>,
}

/// Reconcile persisted MCP tool choices with the live `tools/list`
/// from each connected server. Servers absent from `server_tools` are
/// dropped. The persisted row's `enabled` flag wins when present;
/// otherwise the package-declared whitelist (`recommended_tools`) decides;
/// otherwise everything is enabled. `description` is always refreshed
/// from the live `tools/list`.
pub fn merge_mcp_tools_config(
    persisted: &AgentMcpToolsConfig,
    server_tools: &IndexMap<String, Vec<McpToolDescriptor>>,
    recommended_tools: &IndexMap<String, Vec<String>>,
) -> AgentMcpToolsConfig {
    let mut merged: AgentMcpToolsConfig = AgentMcpToolsConfig::default();

    for (server_name, defs) in server_tools {
        // ADR-104: the default-enabled subset comes from the package's
        // `[[mcp_servers]].tools` whitelist (via `agent_mcp.json`'s
        // `recommended`). No whitelist => "everything on, user opts out",
        // the pre-existing baseline for user-installed servers.
        let defaults = recommended_tools
            .get(server_name)
            .filter(|list| !list.is_empty());
        let prior = persisted.servers.get(server_name);

        let mut items: Vec<AgentMcpToolItem> = Vec::with_capacity(defs.len());
        for def in defs {
            let enabled = match prior.and_then(|rows| tool_enabled_in(rows, &def.name)) {
                Some(user_choice) => user_choice,
                None => match defaults {
                    Some(list) => list.contains(&def.name),
                    None => true,
                },
            };
            items.push(AgentMcpToolItem::with_description(
                def.name.clone(),
                enabled,
                def.description.clone(),
            ));
        }

        merged.servers.insert(server_name.to_string(), items);
    }

    merged
}

/// Load per-agent MCP tools config. Returns `None` when file missing;
/// errors when present but unparseable (no silent migration).
pub fn load_agent_mcp_tools_config(work_dir: &Path) -> Result<Option<AgentMcpToolsConfig>, String> {
    let path = mcp_tools_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let cfg: AgentMcpToolsConfig = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Failed to parse {} as AgentMcpToolsConfig (delete the file if it predates the current schema): {}",
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        server_count = cfg.servers.len(),
        "Loaded agent MCP tools config from workspace"
    );
    Ok(Some(cfg))
}

/// Save the full per-agent MCP tools config (atomic write-tmp-rename).
pub fn save_agent_mcp_tools_config(
    work_dir: &Path,
    cfg: &AgentMcpToolsConfig,
) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = mcp_tools_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent MCP tools config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        server_count = cfg.servers.len(),
        "Saved agent MCP tools config to workspace"
    );
    Ok(())
}

// ── Per-agent provider config ────────────────────────────────────────────

/// Filename for per-agent provider config in the workspace config directory.
const AGENT_PROVIDER_CONFIG_FILE: &str = "agent_provider.json";

/// Build the path to the agent provider config file.
fn provider_config_path(work_dir: &Path) -> PathBuf {
    work_dir.join("config").join(AGENT_PROVIDER_CONFIG_FILE)
}

/// Load per-agent provider config from workspace/config/agent_provider.json.
///
/// Returns `None` if the file does not exist (no providers configured yet).
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_agent_provider_config(
    work_dir: &Path,
) -> Result<Option<acowork_core::protocol::AgentProviderConfig>, String> {
    let path = provider_config_path(work_dir);
    if !path.exists() {
        return Ok(None);
    }

    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    let mut cfg: acowork_core::protocol::AgentProviderConfig = serde_json::from_str(&raw)
        .map_err(|e| format!("Failed to parse {}: {}", path.display(), e))?;
    cfg.migrate_legacy_defaults();

    tracing::info!(
        work_dir = %work_dir.display(),
        provider_count = cfg.providers.len(),
        version = cfg.version,
        "Loaded agent provider config from workspace"
    );

    Ok(Some(cfg))
}

/// Save per-agent provider config to workspace/config/agent_provider.json.
///
/// Uses atomic write-tmp-rename to prevent corruption on crash.
pub fn save_agent_provider_config(
    work_dir: &Path,
    cfg: &acowork_core::protocol::AgentProviderConfig,
) -> Result<(), String> {
    let config_dir = work_dir.join("config");
    std::fs::create_dir_all(&config_dir).map_err(|e| {
        format!(
            "Failed to create config dir {}: {}",
            config_dir.display(),
            e
        )
    })?;

    let path = provider_config_path(work_dir);
    let tmp_path = path.with_extension("tmp");

    let json = serde_json::to_string_pretty(cfg)
        .map_err(|e| format!("Failed to serialize agent provider config: {}", e))?;

    std::fs::write(&tmp_path, &json)
        .map_err(|e| format!("Failed to write {}: {}", tmp_path.display(), e))?;

    std::fs::rename(&tmp_path, &path).map_err(|e| {
        format!(
            "Failed to rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        )
    })?;

    tracing::info!(
        work_dir = %work_dir.display(),
        provider_count = cfg.providers.len(),
        version = cfg.version,
        "Saved agent provider config to workspace"
    );

    Ok(())
}

/// Save provider config from an MQTT `AvailableProviders` payload.
///
/// This is the provider equivalent of `save_agent_mcp_config_catalog`:
/// called by the MQTT poll loop when it receives `acowork/global/providers`.
/// Only the `providers` list and `version` are replaced — if the file
/// exists, the new payload fully replaces the old one.
pub fn save_agent_provider_config_from_available(
    work_dir: &Path,
    providers: &[acowork_core::protocol::ProviderListItem],
    version: u64,
    default_compact_models: &[acowork_core::protocol::CompactModelRef],
) -> Result<(), String> {
    let cfg = acowork_core::protocol::AgentProviderConfig {
        providers: providers.to_vec(),
        version,
        default_compact_models: default_compact_models.to_vec(),
        default_compact_model_legacy: None,
    };
    save_agent_provider_config(work_dir, &cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};

    // ── ChatRouterConfig (ADR-099) ────────────────────────────────────

    #[test]
    fn chat_router_config_defaults_are_the_design_values() {
        let cfg = ChatRouterConfig::default();
        assert!(cfg.is_enabled(), "the router is on unless explicitly disabled");
        assert_eq!(cfg.timeout_ms(), ChatRouterConfig::DEFAULT_TIMEOUT_MS);
        assert_eq!(
            cfg.history_window(),
            ChatRouterConfig::DEFAULT_HISTORY_WINDOW
        );
        assert_eq!(
            cfg.max_candidates(),
            ChatRouterConfig::DEFAULT_MAX_CANDIDATES
        );
    }

    #[test]
    fn chat_router_config_clamps_instead_of_drifting() {
        // A zero timeout and an empty window would make every turn fall back;
        // a 500-candidate prompt would be unaffordable. All three clamp.
        let cfg = ChatRouterConfig {
            timeout_ms: Some(0),
            history_window: Some(1),
            max_candidates: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.timeout_ms(), 1_000);
        assert_eq!(cfg.history_window(), 4);
        assert_eq!(cfg.max_candidates(), 1);

        let cfg = ChatRouterConfig {
            timeout_ms: Some(u64::MAX),
            history_window: Some(usize::MAX),
            max_candidates: Some(usize::MAX),
            ..Default::default()
        };
        assert_eq!(cfg.timeout_ms(), 300_000);
        assert_eq!(cfg.history_window(), 200);
        assert_eq!(cfg.max_candidates(), 50);
    }

    #[test]
    fn chat_router_config_missing_block_means_defaults() {
        let cfg = AgentConfig::default();
        assert!(cfg.chat_router_config().is_enabled());
        assert!(cfg.chat_router.is_none());

        let cfg = AgentConfig {
            chat_router: Some(ChatRouterConfig {
                enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(!cfg.chat_router_config().is_enabled());
    }

    // ── AgentMcpConfig struct ────────────────────────────────────────

    #[test]
    fn agent_mcp_config_default_empty() {
        let cfg = AgentMcpConfig::default();
        assert!(cfg.catalog.is_empty());
        assert!(cfg.local.is_empty());
        assert!(cfg.merged().is_empty());
        assert!(!cfg.contains_name("any"));
        assert!(!cfg.is_catalog("any"));
    }

    #[test]
    fn merged_combines_catalog_and_local() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "catalog-a".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "cmd-a".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![McpServerConfigDef {
                name: "local-b".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "cmd-b".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],

            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };

        let merged = cfg.merged();
        assert_eq!(merged.len(), 2);
        let names: Vec<&str> = merged.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"catalog-a"));
        assert!(names.contains(&"local-b"));
    }

    #[test]
    fn local_overrides_catalog_by_name() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "dup-name".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "catalog-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![McpServerConfigDef {
                name: "dup-name".into(),
                display_name: None,
                transport: McpTransportDef::Http,
                url: Some("http://local:8080".into()),
                command: "local-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],

            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };

        let merged = cfg.merged();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "dup-name");
        assert_eq!(merged[0].command, "local-cmd");
    }

    #[test]
    fn contains_name_checks_both_lists() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "cat".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "x".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![McpServerConfigDef {
                name: "loc".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "y".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],

            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };

        assert!(cfg.contains_name("cat"));
        assert!(cfg.contains_name("loc"));
        assert!(!cfg.contains_name("nonexistent"));
    }

    #[test]
    fn is_catalog_only_matches_catalog_list() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "cat".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "x".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![McpServerConfigDef {
                name: "loc".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "y".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],

            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };

        assert!(cfg.is_catalog("cat"));
        assert!(!cfg.is_catalog("loc"));
        assert!(!cfg.is_catalog("nonexistent"));
    }

    // ── Serialize/Deserialize round-trip ──────────────────────────────

    #[test]
    fn serialize_deserialize_empty_agent_mcp_config() {
        let cfg = AgentMcpConfig::default();
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentMcpConfig = serde_json::from_str(&json).unwrap();
        assert!(restored.catalog.is_empty());
        assert!(restored.local.is_empty());
    }

    #[test]
    fn serialize_deserialize_full_agent_mcp_config() {
        let cfg = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "mcp1".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "/usr/bin/server".into(),
                args: vec!["--port".into(), "8080".into()],
                env: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("KEY".into(), "val".into());
                    m
                },
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: Some(30),
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![McpServerConfigDef {
                name: "local1".into(),
                display_name: None,
                transport: McpTransportDef::Sse,
                url: Some("http://example.com/sse".into()),
                command: "".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: {
                    let mut m = std::collections::HashMap::new();
                    m.insert("Auth".into(), "Bearer x".into());
                    m
                },
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],

            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };

        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentMcpConfig = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.catalog.len(), 1);
        assert_eq!(restored.catalog[0].name, "mcp1");
        assert_eq!(restored.catalog[0].tool_timeout_secs, Some(30));
        assert_eq!(restored.local.len(), 1);
        assert_eq!(restored.local[0].name, "local1");
        assert!(matches!(restored.local[0].transport, McpTransportDef::Sse));
    }

    // ── Backward compat migration ────────────────────────────────────

    #[test]
    fn backward_migration_old_flat_format() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let old_json = serde_json::json!([{
            "name": "old-server",
            "transport": "stdio",
            "command": "cmd",
            "args": []
        }]);
        let mcp_path = config_dir.join("agent_mcp.json");
        std::fs::write(&mcp_path, serde_json::to_string(&old_json).unwrap()).unwrap();

        let loaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.catalog.len(), 1);
        assert_eq!(loaded.catalog[0].name, "old-server");
        assert!(loaded.local.is_empty());

        let raw = std::fs::read_to_string(&mcp_path).unwrap();
        assert!(raw.contains("\"catalog\""));
        // Note: local may be absent when empty (skip_serializing_if)
    }

    // ── save_catalog preserves local ─────────────────────────────────

    #[test]
    fn save_catalog_preserves_local() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let initial = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "orig-cat".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "cat-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![McpServerConfigDef {
                name: "orig-loc".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "loc-cmd".into(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],

            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        save_agent_mcp_config(dir.path(), &initial).unwrap();

        let new_catalog = vec![McpServerConfigDef {
            name: "new-cat".into(),
            display_name: None,
            transport: McpTransportDef::Stdio,
            url: None,
            command: "new-cmd".into(),
            args: vec![],
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            tool_timeout_secs: None,
            install: None,
            required_secrets: vec![],
            origin: None,
        }];
        save_agent_mcp_config_catalog(dir.path(), &new_catalog).unwrap();

        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.catalog.len(), 1);
        assert_eq!(reloaded.catalog[0].name, "new-cat");
        assert_eq!(reloaded.local.len(), 1);
        assert_eq!(reloaded.local[0].name, "orig-loc");
    }

    /// Regression: Gateway pushes catalog on every restart (retained
    /// `acowork/global/mcps`). `save_agent_mcp_config_catalog` is invoked
    /// from the Runtime MQTT poll loop in response. The function MUST
    /// preserve the user's `active_names` selection (Tools panel
    /// checkboxes), not reset it. Otherwise every Gateway restart
    /// silently clears the user's MCP activation state.
    ///
    /// History: the original implementation set `active_names = None`
    /// unconditionally, which is exactly the regression this test guards
    /// against. See `mqtt/client.rs` poll loop for the call site.
    #[test]
    fn save_catalog_preserves_active_names() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        // User has previously activated context7 in the Tools panel.
        let initial = AgentMcpConfig {
            catalog: vec![McpServerConfigDef {
                name: "context7".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "npx".into(),
                args: vec!["-y".into(), "@upstash/context7-mcp".into()],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            }],
            local: vec![],
            active_names: Some(vec!["context7".into()]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        save_agent_mcp_config(dir.path(), &initial).unwrap();

        // Gateway republishes catalog with a new tool added.
        let new_catalog = vec![
            McpServerConfigDef {
                name: "context7".into(),
                display_name: None,
                transport: McpTransportDef::Stdio,
                url: None,
                command: "npx".into(),
                args: vec!["-y".into(), "@upstash/context7-mcp".into()],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            },
            McpServerConfigDef {
                name: "new-mcp".into(),
                display_name: None,
                transport: McpTransportDef::Http,
                url: Some("http://example.com".into()),
                command: String::new(),
                args: vec![],
                env: std::collections::HashMap::new(),
                headers: std::collections::HashMap::new(),
                tool_timeout_secs: None,
                install: None,
                required_secrets: vec![],
                origin: None,
            },
        ];
        save_agent_mcp_config_catalog(dir.path(), &new_catalog).unwrap();

        // After sync: catalog replaced, but active_names survives.
        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.catalog.len(), 2);
        assert_eq!(reloaded.catalog[1].name, "new-mcp");
        assert_eq!(
            reloaded.active_names,
            Some(vec!["context7".into()]),
            "active_names must survive a catalog sync (Gateway restart would otherwise wipe user MCP selection)"
        );
    }

    #[test]
    #[allow(deprecated)] // exercising the deprecated loader explicitly
    fn load_merged_mcp_configs_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let merged = load_merged_mcp_configs(dir.path());
        assert!(merged.is_empty());
    }

    // ── Default-on for system-injected MCPs (pm, design §6.1 / T3-4) ──
    //
    // The Gateway injects `pm` into every agent's catalog. It must be
    // **active by default** (agents get pm_* tools out of the box) yet
    // still toggleable — the user can turn it off and the next catalog
    // push must not re-activate it. This is materialized by writing
    // `active_names` ONCE, only when the user has never expressed a
    // choice (`active_names` is None).

    #[test]
    fn save_catalog_default_activates_pm_once() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        // Fresh agent: pm injected, active_names never set.
        let catalog = vec![mcp_def("playwright"), mcp_def("pm")];
        save_agent_mcp_config_catalog(dir.path(), &catalog).unwrap();

        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            reloaded.active_names,
            Some(vec!["pm".to_string()]),
            "pm must be default-active; playwright stays opt-in"
        );

        // Second push (e.g. Gateway restart) must preserve the choice.
        save_agent_mcp_config_catalog(dir.path(), &catalog).unwrap();
        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.active_names, Some(vec!["pm".to_string()]));
    }

    #[test]
    fn save_catalog_does_not_clobber_user_turning_pm_off() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        // User explicitly toggled EVERYTHING off (active_names = Some([])).
        let initial = AgentMcpConfig {
            catalog: vec![mcp_def("pm")],
            local: vec![],
            active_names: Some(vec![]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        save_agent_mcp_config(dir.path(), &initial).unwrap();

        // Next catalog push must NOT re-activate pm.
        save_agent_mcp_config_catalog(dir.path(), &[mcp_def("pm")]).unwrap();
        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            reloaded.active_names,
            Some(vec![]),
            "a user who turned pm off must stay off across catalog pushes"
        );
    }

    #[test]
    fn save_catalog_no_default_when_no_system_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        // No system-injected MCP → active_names stays None (opt-in).
        save_agent_mcp_config_catalog(dir.path(), &[mcp_def("playwright")]).unwrap();
        let reloaded = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(reloaded.active_names, None);
    }

    // ── mcp_install default-on (local MCPs) ─────────────────────────

    #[test]
    fn activate_mcp_name_initializes_and_dedupes() {
        // Unset → initializes to Some([name]).
        let mut active = None;
        crate::agent_config::activate_mcp_name(&mut active, "docling");
        assert_eq!(active, Some(vec!["docling".to_string()]));

        // Already present → no duplicate.
        crate::agent_config::activate_mcp_name(&mut active, "docling");
        assert_eq!(active, Some(vec!["docling".to_string()]));

        // Second name appends; existing choice preserved.
        crate::agent_config::activate_mcp_name(&mut active, "pm");
        assert_eq!(active, Some(vec!["docling".to_string(), "pm".to_string()]));

        // User explicitly turned everything off → a fresh install still
        // activates (installing implies intent to use).
        let mut off = Some(vec![]);
        crate::agent_config::activate_mcp_name(&mut off, "docling");
        assert_eq!(off, Some(vec!["docling".to_string()]));
    }

    // ── Bug 1 regression suite (MCP active_merged) ───────────────────
    //
    // These pin the semantics of `AgentMcpConfig::active_merged()` and
    // `load_active_mcp_configs()` so the user-toggles-active-but-runtime-
    // connects-all bug cannot recur. Any future change that breaks these
    // is by definition a regression.

    fn mcp_def(name: &str) -> McpServerConfigDef {
        McpServerConfigDef {
            name: name.to_string(),
            display_name: None,
            transport: McpTransportDef::Stdio,
            url: None,
            command: "npx".to_string(),
            args: vec![],
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            tool_timeout_secs: None,
            install: None,
            required_secrets: vec![],
            origin: None,
        }
    }

    #[test]
    fn active_merged_returns_only_user_selected_entries() {
        // User toggled context7 ON, playwright OFF in the Tools panel.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: Some(vec!["context7".into()]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        let active = cfg.active_merged();
        assert_eq!(active.len(), 1, "only context7 should be active");
        assert_eq!(active[0].name, "context7");
    }

    #[test]
    fn active_merged_empty_when_user_unchecked_all() {
        // User explicitly unchecked every catalog item — the "no MCP
        // active" state surfaced in Bug 1. Result MUST be empty so the
        // startup auto-connect path connects nothing.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: Some(vec![]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        assert!(cfg.active_merged().is_empty());
    }

    #[test]
    fn active_merged_empty_when_active_names_field_missing() {
        // Legacy file without `active_names` field (e.g. pre-ADR-? save).
        // `active_server_names()` returns [] for None, so we must
        // connect nothing — opt-in semantics for MCP.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        assert!(
            cfg.active_merged().is_empty(),
            "legacy file (no active_names) must default to no servers active"
        );
    }

    #[test]
    fn active_merged_local_overrides_catalog_for_same_name() {
        // Local entry shadows catalog entry with the same name and must
        // remain active even if user listed only the catalog name.
        // (active_names is a name set, not a {catalog,local} pair.)
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("context7")],
            local: vec![{
                let mut d = mcp_def("context7");
                d.command = "local-context7".to_string();
                d
            }],
            active_names: Some(vec!["context7".into()]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        let active = cfg.active_merged();
        assert_eq!(active.len(), 1);
        assert_eq!(
            active[0].command, "local-context7",
            "local must shadow catalog"
        );
    }

    #[test]
    fn active_merged_drops_unknown_active_names() {
        // If a stale `active_names` references a catalog entry that no
        // longer exists (e.g. catalog push removed it), active_merged
        // must silently skip it — not panic, not include garbage.
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("context7")],
            local: vec![],
            active_names: Some(vec!["context7".into(), "ghost_mcp".into()]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        let active = cfg.active_merged();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "context7");
    }

    #[test]
    fn load_active_mcp_configs_honors_active_names() {
        // End-to-end: write a config file where the user has unchecked
        // every catalog MCP, then verify the loader returns the empty
        // list. This is the exact Bug 1 repro path.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        let cfg = AgentMcpConfig {
            catalog: vec![mcp_def("playwright"), mcp_def("context7")],
            local: vec![],
            active_names: Some(vec![]), // user unchecked all
            recommended: Vec::new(),
            dismissed: Vec::new(), // user unchecked all
        };
        save_agent_mcp_config(dir.path(), &cfg).unwrap();

        let active = load_active_mcp_configs(dir.path());
        assert!(
            active.is_empty(),
            "Bug 1 regression: load_active_mcp_configs returned {:?}",
            active.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    // ── AgentConfig context_window serialization (ADR-026) ────────────

    #[test]
    fn agent_config_context_window_round_trip() {
        let cfg = AgentConfig {
            context_window: Some(150_000),
            ..AgentConfig::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(
            json.contains("context_window"),
            "JSON should contain context_window: {}",
            json
        );

        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.context_window, Some(150_000));
    }

    #[test]
    fn agent_config_context_window_none_omitted() {
        let cfg = AgentConfig::default();
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(
            !json.contains("context_window"),
            "context_window=None should be omitted from JSON: {}",
            json
        );
    }

    #[test]
    fn agent_config_context_window_zero_preserved() {
        // 0 is invalid at resolution (ADR-074 §6 → layer skipped) but must
        // still round-trip on disk: existing files are not repaired (ADR-074
        // §9), only the chain ignores them.
        let cfg = AgentConfig {
            context_window: Some(0),
            ..AgentConfig::default()
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(
            json.contains("context_window"),
            "context_window=0 should be serialized: {}",
            json
        );

        let restored: AgentConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.context_window, Some(0));
    }

    #[test]
    fn agent_config_context_window_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AgentConfig {
            context_window: Some(200_000),
            temperature: Some(0.5),
            ..AgentConfig::default()
        };
        save_agent_config(dir.path(), &cfg).unwrap();

        let loaded = load_agent_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.context_window, Some(200_000));
        assert_eq!(loaded.temperature, Some(0.5));
    }

    // ── AgentToolsConfig (ADR-029) ────────────────────────────────────

    fn sample_tools() -> Vec<String> {
        vec![
            "memory_recall".to_string(),
            "memory_store".to_string(),
            "http_request".to_string(),
            "web_fetch".to_string(),
            "shell".to_string(),
        ]
    }

    #[test]
    fn agent_tools_config_default_empty() {
        let cfg = AgentToolsConfig::default();
        assert!(cfg.tools.is_empty());
    }

    #[test]
    fn agent_tools_config_serialize_round_trip() {
        let cfg = AgentToolsConfig {
            tools: vec![
                AgentToolEntry::new("memory_recall", true),
                AgentToolEntry::new("http_request", false),
            ],
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentToolsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tools.len(), 2);
        assert_eq!(restored.tools[0].name, "memory_recall");
        assert!(restored.tools[0].enabled);
        assert!(!restored.tools[1].enabled);
    }

    #[test]
    fn agent_tool_entry_missing_enabled_defaults_to_true() {
        // Backward-compatible: a stripped entry (`{"name":"x"}`) must
        // deserialize to enabled=true, not the Rust default false.
        let json = r#"[{"name": "memory_recall"}]"#;
        let restored: Vec<AgentToolEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(restored.len(), 1);
        assert!(
            restored[0].enabled,
            "missing enabled should default to true"
        );
    }

    #[test]
    fn merge_tools_config_preserves_persisted_state() {
        let code = sample_tools();
        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("http_request", false),
            AgentToolEntry::new("removed_tool", true), // should be dropped
        ];
        let merged = merge_tools_config(&code, &persisted);
        let map: std::collections::HashMap<String, bool> =
            merged.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert_eq!(merged.len(), 5, "removed_tool must not appear in merge");
        assert!(map["memory_recall"]);
        assert!(!map["http_request"], "persisted false preserved");
        // Tools missing from persisted file are disabled (opt-in)
        assert!(
            !map["memory_store"],
            "missing in persisted → disabled (opt-in)"
        );
        assert!(
            !map["web_fetch"],
            "missing in persisted → disabled (opt-in)"
        );
        assert!(!map["shell"], "missing in persisted → disabled (opt-in)");
        assert!(!map.contains_key("removed_tool"));
    }

    #[test]
    fn merge_tools_config_preserves_conditionally_registered_tool() {
        // Regression for the "tool comes back enabled after restart"
        // report: `rag_query` is conditionally registered — it is absent
        // from the code registry at boot (its manifest `type = "rag"`
        // entry is resolved after startup). A persisted `enabled=false`
        // must survive the merge + write-back, or the user's choice is
        // erased and the tool is re-added enabled when it is registered.
        let code = sample_tools(); // does NOT include "rag_query"
        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("rag_query", false), // user explicitly disabled it
        ];
        let merged = merge_tools_config(&code, &persisted);
        let entry = merged.iter().find(|e| e.name == "rag_query");
        assert!(
            entry.is_some(),
            "conditionally-registered tool must survive the merge; got: {:?}",
            merged.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
        );
        assert!(
            !entry.unwrap().enabled,
            "persisted disabled flag must be preserved for rag_query"
        );
    }

    #[test]
    fn merge_tools_config_conditionally_registered_tool_in_code_uses_persisted() {
        // When `rag_query` IS registered this boot (manifest declared a
        // RAG tool entry), it must follow the persisted flag like any
        // other tool — and must not be duplicated by the conditional
        // preservation pass.
        let mut code = sample_tools();
        code.push("rag_query".to_string());
        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("rag_query", false),
        ];
        let merged = merge_tools_config(&code, &persisted);
        let entries: Vec<&AgentToolEntry> =
            merged.iter().filter(|e| e.name == "rag_query").collect();
        assert_eq!(entries.len(), 1, "rag_query must appear exactly once");
        assert!(!entries[0].enabled, "persisted disabled flag preserved");
    }

    #[test]
    fn merge_tools_config_drops_retired_codebase_entry() {
        // `codebase` is temporarily unregistered (see
        // `tools::builtin::CONDITIONALLY_REGISTERED_TOOL_NAMES`). A row
        // left over in an existing `agent_tools.json` is no longer a
        // conditionally-registered tool, so the merge must drop it and the
        // startup write-back must clear it — otherwise the Tools panel
        // would keep rendering a toggle for a tool that cannot be called.
        let code = sample_tools();
        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("codebase", true), // stale row from before
        ];
        let merged = merge_tools_config(&code, &persisted);
        assert!(
            merged.iter().all(|e| e.name != "codebase"),
            "retired codebase row must be dropped; got: {:?}",
            merged.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn merge_tools_config_still_drops_unknown_removed_tool() {
        // A genuinely-removed tool (not in the code registry and not a
        // known conditionally-registered name) must still be dropped —
        // the conditional preservation must not resurrect arbitrary names.
        let code = sample_tools();
        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("obsolete_tool", true), // removed by upgrade
        ];
        let merged = merge_tools_config(&code, &persisted);
        assert!(
            merged.iter().all(|e| e.name != "obsolete_tool"),
            "unknown removed tool must still be dropped"
        );
    }

    #[test]
    fn merge_tools_config_new_tool_defaults_disabled() {
        // Simulate a Runtime upgrade that introduced a new tool
        // `brand_new_tool` to the code registry. The persisted file
        // has no knowledge of it.
        let mut code = sample_tools();
        code.push("brand_new_tool".to_string());

        let persisted = vec![
            AgentToolEntry::new("memory_recall", true),
            AgentToolEntry::new("memory_store", true),
            AgentToolEntry::new("http_request", true),
            AgentToolEntry::new("web_fetch", true),
            AgentToolEntry::new("shell", true),
        ];
        // New tools default to disabled (opt-in: need explicit
        // enablement via manifest or frontend).
        let merged = merge_tools_config(&code, &persisted);
        let entry = merged.iter().find(|e| e.name == "brand_new_tool").unwrap();
        assert!(
            !entry.enabled,
            "new tool default must be disabled (opt-in): got enabled={}",
            entry.enabled
        );
        // Existing tools should preserve their persisted state.
        let memory_recall = merged.iter().find(|e| e.name == "memory_recall").unwrap();
        assert!(memory_recall.enabled);
    }

    #[test]
    fn init_tools_config_from_manifest_enables_listed_only() {
        let code = sample_tools();
        let manifest_tools = vec!["memory_recall".to_string(), "shell".to_string()];
        let cfg = init_tools_config_from_manifest(&code, &manifest_tools);
        let map: std::collections::HashMap<String, bool> =
            cfg.iter().map(|e| (e.name.clone(), e.enabled)).collect();
        assert!(map["memory_recall"]);
        assert!(map["shell"]);
        assert!(!map["memory_store"], "not in manifest → disabled");
        assert!(!map["http_request"]);
        assert!(!map["web_fetch"]);
    }

    #[test]
    fn init_tools_config_empty_manifest_disables_all() {
        let code = sample_tools();
        let cfg = init_tools_config_from_manifest(&code, &[]);
        assert!(cfg.iter().all(|e| !e.enabled));
    }

    #[test]
    fn all_enabled_tools_config_enables_everything() {
        let code = sample_tools();
        let cfg = all_enabled_tools_config(&code);
        assert_eq!(cfg.len(), code.len());
        assert!(cfg.iter().all(|e| e.enabled));
    }

    // ── AgentMcpToolsConfig (ADR-069) ─────────────────────────────────
    //
    // Tests for the flat per-tool allowlist model. The wire format is
    // `servers: { name -> [{name, enabled, description}] }` and the
    // `merge_mcp_tools_config` function reconciles persisted choices
    // with the live `tools/list` from each connected MCP server.

    /// Default config has no servers.
    #[test]
    fn agent_mcp_tools_config_default_is_empty() {
        let cfg = AgentMcpToolsConfig::default();
        assert!(cfg.servers.is_empty());
    }

    /// JSON round-trip preserves the flat per-tool entries exactly,
    /// including `description` when present and absence when None.
    #[test]
    fn agent_mcp_tools_config_round_trip_preserves_flat_items() {
        let mut cfg = AgentMcpToolsConfig::default();
        cfg.servers.insert(
            "pm".to_string(),
            vec![
                AgentMcpToolItem::with_description(
                    "pm_claim_task",
                    true,
                    Some("Claim a pending task".into()),
                ),
                AgentMcpToolItem::new("pm_submit_task", true),
                AgentMcpToolItem::new("pm_list_projects", false),
            ],
        );
        let json = serde_json::to_string(&cfg).unwrap();
        let restored: AgentMcpToolsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.servers.len(), 1);
        let pm = restored.servers.get("pm").unwrap();
        assert_eq!(pm.len(), 3);
        assert_eq!(pm[0].name, "pm_claim_task");
        assert!(pm[0].enabled);
        assert_eq!(pm[0].description.as_deref(), Some("Claim a pending task"));
        assert_eq!(pm[1].name, "pm_submit_task");
        assert!(pm[1].enabled);
        assert!(pm[1].description.is_none());
        assert_eq!(pm[2].name, "pm_list_projects");
        assert!(!pm[2].enabled);
    }

    /// `description` is `skip_serializing_if = "Option::is_none"`, so a
    /// tool row with no description does not add the field to JSON.
    #[test]
    fn agent_mcp_tool_item_omits_description_when_none() {
        let item = AgentMcpToolItem::new("pm_claim_task", true);
        let json = serde_json::to_string(&item).unwrap();
        assert!(!json.contains("description"));
    }

    /// `tool_enabled_in` returns `Some(enabled)` for known tools,
    /// `None` for absent ones — no tri-state gymnastics.
    #[test]
    fn tool_enabled_in_returns_flag_when_present_none_when_absent() {
        let items = vec![
            AgentMcpToolItem::new("pm_claim_task", true),
            AgentMcpToolItem::new("pm_submit_task", false),
        ];
        assert_eq!(tool_enabled_in(&items, "pm_claim_task"), Some(true));
        assert_eq!(tool_enabled_in(&items, "pm_submit_task"), Some(false));
        assert_eq!(tool_enabled_in(&items, "pm_list_projects"), None);
    }

    /// ADR-104: with an empty persisted config, the package-declared
    /// `[[mcp_servers]].tools` whitelist decides which of the 12 tools a
    /// server advertises start enabled; `description` comes from the live
    /// `tools/list`. This replaces the old hardcoded
    /// `PM_DEFAULT_ENABLED_TOOLS`.
    #[test]
    fn merge_mcp_tools_config_seeds_from_declared_whitelist() {
        let persisted = AgentMcpToolsConfig::default();
        let names = [
            "pm_list_projects",
            "pm_get_project",
            "pm_create_project",
            "pm_list_tasks",
            "pm_get_task",
            "pm_create_task",
            "pm_check_task",
            "pm_update_task",
            "pm_claim_task",
            "pm_submit_task",
            "pm_list_my_tasks",
            "pm_reparent_task",
        ];
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        let defs: Vec<McpToolDescriptor> = names
            .iter()
            .map(|n| McpToolDescriptor {
                name: (*n).to_string(),
                description: Some(format!("desc for {}", n)),
            })
            .collect();
        server_tools.insert("pm".to_string(), defs);

        let mut recommended: IndexMap<String, Vec<String>> = IndexMap::new();
        recommended.insert(
            "pm".to_string(),
            ["pm_list_my_tasks", "pm_claim_task", "pm_submit_task", "pm_check_task"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &recommended);
        let pm = merged.servers.get("pm").unwrap();
        assert_eq!(pm.len(), 12);

        let enabled_names: std::collections::HashSet<&str> = pm
            .iter()
            .filter(|t| t.enabled)
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(
            enabled_names,
            std::collections::HashSet::from([
                "pm_list_my_tasks",
                "pm_claim_task",
                "pm_submit_task",
                "pm_check_task",
            ])
        );

        assert_eq!(
            pm.iter()
                .find(|t| t.name == "pm_claim_task")
                .unwrap()
                .description
                .as_deref(),
            Some("desc for pm_claim_task")
        );
    }

    /// Non-system-injected server with empty persisted -> all tools
    /// enabled (the "opt-out" baseline; the Tools panel can flip any
    /// of them to disabled afterwards).
    #[test]
    fn merge_mcp_tools_config_enables_all_for_non_system_servers() {
        let persisted = AgentMcpToolsConfig::default();
        let defs = vec![
            McpToolDescriptor {
                name: "search".into(),
                description: None,
            },
            McpToolDescriptor {
                name: "summarize".into(),
                description: None,
            },
        ];
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert("user-installed".to_string(), defs);

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());
        let rows = merged.servers.get("user-installed").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|t| t.enabled));
    }

    /// Persisted choices win over defaults.
    #[test]
    fn merge_mcp_tools_config_preserves_user_choice_over_default() {
        let mut persisted = AgentMcpToolsConfig::default();
        persisted.servers.insert(
            "pm".to_string(),
            vec![
                AgentMcpToolItem::new("pm_claim_task", false), // user disabled
                AgentMcpToolItem::new("pm_create_project", true), // user enabled
            ],
        );
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "pm".to_string(),
            vec![
                McpToolDescriptor {
                    name: "pm_claim_task".into(),
                    description: Some("Claim".into()),
                },
                McpToolDescriptor {
                    name: "pm_create_project".into(),
                    description: Some("Create".into()),
                },
                McpToolDescriptor {
                    name: "pm_list_my_tasks".into(),
                    description: Some("List mine".into()),
                },
            ],
        );

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());
        let pm = merged.servers.get("pm").unwrap();
        assert_eq!(pm.len(), 3);
        assert!(!tool_enabled_in(pm, "pm_claim_task").unwrap());
        assert!(tool_enabled_in(pm, "pm_create_project").unwrap());
        assert!(tool_enabled_in(pm, "pm_list_my_tasks").unwrap());
    }

    /// Description is always refreshed from the live `tools/list`.
    #[test]
    fn merge_mcp_tools_config_refreshes_description_from_registry() {
        let mut persisted = AgentMcpToolsConfig::default();
        persisted.servers.insert(
            "pm".to_string(),
            vec![AgentMcpToolItem::with_description(
                "pm_claim_task",
                true,
                Some("stale description from yesterday".into()),
            )],
        );
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "pm".to_string(),
            vec![McpToolDescriptor {
                name: "pm_claim_task".into(),
                description: Some("up-to-date description".into()),
            }],
        );

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());
        let pm = merged.servers.get("pm").unwrap();
        assert_eq!(pm[0].description.as_deref(), Some("up-to-date description"));
    }

    /// Persisted rows whose `(server, tool)` pair is no longer in the
    /// live `tools/list` are dropped.
    #[test]
    fn merge_mcp_tools_config_drops_persisted_rows_no_longer_advertised() {
        let mut persisted = AgentMcpToolsConfig::default();
        persisted.servers.insert(
            "pm".to_string(),
            vec![
                AgentMcpToolItem::new("pm_claim_task", true),
                AgentMcpToolItem::new("pm_old_removed_tool", true),
            ],
        );
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "pm".to_string(),
            vec![McpToolDescriptor {
                name: "pm_claim_task".into(),
                description: None,
            }],
        );

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());
        let pm = merged.servers.get("pm").unwrap();
        assert_eq!(pm.len(), 1);
        assert_eq!(pm[0].name, "pm_claim_task");
    }

    /// Servers absent from the registry are removed from merged config.
    #[test]
    fn merge_mcp_tools_config_drops_servers_absent_from_registry() {
        let mut persisted = AgentMcpToolsConfig::default();
        persisted.servers.insert(
            "pm".to_string(),
            vec![AgentMcpToolItem::new("pm_claim_task", true)],
        );
        persisted.servers.insert(
            "docling".to_string(),
            vec![AgentMcpToolItem::new("parse_pdf", true)],
        );
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "pm".to_string(),
            vec![McpToolDescriptor {
                name: "pm_claim_task".into(),
                description: None,
            }],
        );

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());
        assert!(merged.servers.contains_key("pm"));
        assert!(!merged.servers.contains_key("docling"));
    }

    /// Full persistence round-trip: merge -> save -> load.
    #[test]
    fn merge_mcp_tools_config_persists_and_loads_back() {
        let dir = tempfile::tempdir().unwrap();
        let persisted = AgentMcpToolsConfig::default();
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "pm".to_string(),
            vec![
                McpToolDescriptor {
                    name: "pm_claim_task".into(),
                    description: Some("Claim a pending task".into()),
                },
                McpToolDescriptor {
                    name: "pm_submit_task".into(),
                    description: None,
                },
            ],
        );
        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());
        save_agent_mcp_tools_config(dir.path(), &merged).unwrap();
        let loaded = load_agent_mcp_tools_config(dir.path())
            .unwrap()
            .expect("file was just created, must load");
        assert_eq!(loaded, merged);
    }

    /// ADR-069 follow-up: server ordering must be the iteration order
    /// of the `IndexMap` input, NOT alphabetical, NOT hash-random. The
    /// downstream `agent_mcp_tools.json` file is read back by the
    /// desktop Tools panel verbatim — if this order reshuffles on
    /// every reconcile (which `PUT /mcp-tools` triggers), toggling a
    /// single tool reshuffles the entire per-MCP-server tool list in
    /// the UI.
    ///
    /// This test pins the order end-to-end:
    ///   1. `merge_mcp_tools_config` output follows the IndexMap order
    ///   2. JSON round-trip preserves the order
    ///   3. `IndexMap::new()` + `.insert()` (the order the runtime
    ///      produces via `collect_server_tools_from_registry`) is the
    ///      source of truth — alphabetical name is NOT.
    #[test]
    fn merge_mcp_tools_config_preserves_indexmap_iteration_order() {
        let persisted = AgentMcpToolsConfig::default();
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        // Insert in an order that is NOT alphabetical — proves we
        // don't silently sort.
        server_tools.insert(
            "zeta".to_string(),
            vec![
                McpToolDescriptor {
                    name: "zeta_z".into(),
                    description: None,
                },
                McpToolDescriptor {
                    name: "zeta_a".into(),
                    description: None,
                },
            ],
        );
        server_tools.insert(
            "alpha".to_string(),
            vec![McpToolDescriptor {
                name: "alpha_b".into(),
                description: None,
            }],
        );
        server_tools.insert(
            "mu".to_string(),
            vec![McpToolDescriptor {
                name: "mu_x".into(),
                description: None,
            }],
        );

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &IndexMap::new());

        // 1. Server order = insertion order (zeta, alpha, mu) — not
        //    alphabetical (alpha, mu, zeta).
        let server_order: Vec<&String> = merged.servers.keys().collect();
        assert_eq!(
            server_order,
            vec![&"zeta".to_string(), &"alpha".to_string(), &"mu".to_string()],
            "merged.servers must follow IndexMap insertion order, not hash/alphabetical"
        );

        // 2. Per-server tool order = defs insertion order.
        assert_eq!(
            merged.servers["zeta"].iter().map(|t| &t.name).collect::<Vec<_>>(),
            vec!["zeta_z", "zeta_a"],
            "per-server tool order must follow defs insertion order"
        );
        assert_eq!(
            merged.servers["alpha"]
                .iter()
                .map(|t| &t.name)
                .collect::<Vec<_>>(),
            vec!["alpha_b"]
        );

        // 3. JSON round-trip preserves the order — guards against a
        //    future regression to HashMap (which would randomise).
        let json = serde_json::to_string(&merged).expect("serialize");
        let json_value: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let servers_obj = json_value["servers"].as_object().expect("servers object");
        let json_server_order: Vec<&String> = servers_obj.keys().collect();
        assert_eq!(
            json_server_order,
            vec![&"zeta".to_string(), &"alpha".to_string(), &"mu".to_string()],
            "JSON serialisation must preserve IndexMap order — HashMap would shuffle this"
        );

        let restored: AgentMcpToolsConfig =
            serde_json::from_str(&json).expect("round-trip deserialise");
        let restored_order: Vec<&String> = restored.servers.keys().collect();
        assert_eq!(
            restored_order,
            vec![&"zeta".to_string(), &"alpha".to_string(), &"mu".to_string()],
            "round-trip deserialise must preserve order"
        );
    }

    /// Missing file -> `Ok(None)`.
    #[test]
    fn load_agent_mcp_tools_config_missing_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = load_agent_mcp_tools_config(dir.path()).unwrap();
        assert!(loaded.is_none());
    }

    /// Malformed JSON (e.g. a v1-shape file) surfaces as an error —
    /// no silent migration. Delete and let the next reconcile
    /// regenerate it.
    #[test]
    fn load_agent_mcp_tools_config_v1_shape_returns_error_no_silent_migration() {
        let dir = tempfile::tempdir().unwrap();
        let config_dir = dir.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let v1_payload = r#"{ "pm": { "enabled_tools": ["pm_claim_task"] } }"#;
        std::fs::write(config_dir.join("agent_mcp_tools.json"), v1_payload).unwrap();
        let result = load_agent_mcp_tools_config(dir.path());
        assert!(
            result.is_err(),
            "v1-shape file must not silently load as v2"
        );
    }

    // ── ADR-104: manifest → agent_config.json seeding ──────────────────

    fn decl_manifest(toml_body: &str) -> acowork_core::manifest::AgentManifest {
        acowork_core::manifest::AgentManifest::from_toml(&format!(
            r#"
agent_id = "com.test.seed"
version = "1.0.0"
name = "Seed"
description = "seed test"
author = "test"
runtime_version = "0.1.0"
{toml_body}
"#
        ))
        .expect("manifest should parse")
    }

    /// Empty file + manifest opinions => every declared knob lands.
    #[test]
    fn seed_from_manifest_fills_every_declared_field() {
        let manifest = decl_manifest(
            r#"[config]
temperature = 0.6
context_window = 128000
max_output_tokens = 16000
max_iterations = 80
compression_ratio_threshold = 0.75"#,
        );
        let mut cfg = AgentConfig::default();
        assert!(cfg.seed_from_manifest(&manifest.config_defaults()));

        assert_eq!(cfg.temperature, Some(0.6));
        assert_eq!(cfg.context_window, Some(128000));
        assert_eq!(cfg.max_output_tokens, Some(16000));
        assert_eq!(cfg.max_iterations, Some(80));
        assert_eq!(cfg.compression_ratio_threshold, Some(0.75));
    }

    /// A user value always wins — the invariant that makes the file the
    /// single source of truth.
    #[test]
    fn seed_from_manifest_never_overwrites_user_values() {
        let manifest = decl_manifest(
            r#"[config]
temperature = 0.6
context_window = 128000
max_output_tokens = 16000
max_iterations = 80"#,
        );
        let mut cfg = AgentConfig {
            temperature: Some(0.1),
            max_output_tokens: Some(999),
            max_iterations: Some(5),
            ..Default::default()
        };
        // context_window / compression_ratio_threshold are still empty.
        assert!(cfg.seed_from_manifest(&manifest.config_defaults()));

        assert_eq!(cfg.temperature, Some(0.1), "user temperature wins");
        assert_eq!(cfg.max_output_tokens, Some(999), "user cap wins");
        assert_eq!(cfg.max_iterations, Some(5), "user iteration cap wins");
        assert_eq!(cfg.context_window, Some(128000), "empty field is filled");
    }

    /// Idempotent: the second application reports "nothing changed", which
    /// is what keeps the startup path from re-writing the file every boot.
    #[test]
    fn seed_from_manifest_is_idempotent() {
        let manifest = decl_manifest("[config]\ntemperature = 0.4");
        let mut cfg = AgentConfig::default();
        assert!(cfg.seed_from_manifest(&manifest.config_defaults()));
        let snapshot = (
            cfg.temperature,
            cfg.context_window,
            cfg.max_output_tokens,
            cfg.max_iterations,
            cfg.compression_ratio_threshold,
        );
        assert!(!cfg.seed_from_manifest(&manifest.config_defaults()));
        assert_eq!(
            snapshot,
            (
                cfg.temperature,
                cfg.context_window,
                cfg.max_output_tokens,
                cfg.max_iterations,
                cfg.compression_ratio_threshold
            )
        );
    }

    /// A manifest with no `[config]` and no `[llm]` knobs seeds nothing.
    #[test]
    fn seed_from_manifest_no_opinion_changes_nothing() {
        let manifest = decl_manifest("");
        let mut cfg = AgentConfig::default();
        assert!(!cfg.seed_from_manifest(&manifest.config_defaults()));
        assert_eq!(cfg.temperature, None);
        assert_eq!(cfg.max_output_tokens, None);
    }

    /// Legacy `[llm]` keys still seed (migration path), and `[config]` wins
    /// when both are present.
    #[test]
    fn seed_from_manifest_reads_deprecated_llm_keys() {
        let manifest = decl_manifest(
            r#"[llm]
temperature = 0.2
context_window = 64000
max_tokens = 4096

[config]
temperature = 0.9"#,
        );
        let mut cfg = AgentConfig::default();
        assert!(cfg.seed_from_manifest(&manifest.config_defaults()));
        assert_eq!(cfg.temperature, Some(0.9));
        assert_eq!(cfg.context_window, Some(64000));
        assert_eq!(cfg.max_output_tokens, Some(4096));
    }

    #[test]
    fn seed_agent_config_from_manifest_persists_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = decl_manifest("[config]\ntemperature = 0.55\nmax_iterations = 33");

        assert!(seed_agent_config_from_manifest(dir.path(), &manifest).unwrap());
        let loaded = load_agent_config(dir.path()).unwrap().unwrap();
        assert_eq!(loaded.temperature, Some(0.55));
        assert_eq!(loaded.max_iterations, Some(33));

        // Second run: nothing to do, no write.
        assert!(!seed_agent_config_from_manifest(dir.path(), &manifest).unwrap());
    }

    #[test]
    fn seed_agent_config_from_manifest_skips_file_creation_without_opinion() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = decl_manifest("");
        assert!(!seed_agent_config_from_manifest(dir.path(), &manifest).unwrap());
        assert!(
            load_agent_config(dir.path()).unwrap().is_none(),
            "no manifest opinion => no file is created"
        );
    }

    // ── ADR-104: manifest → agent_mcp.json `recommended` projection ────

    #[test]
    fn project_recommended_writes_declarations_and_keeps_other_lists() {
        let dir = tempfile::tempdir().unwrap();
        let mut initial = AgentMcpConfig {
            catalog: vec![mcp_def("context7")],
            local: vec![mcp_def("agent-installed")],
            active_names: Some(vec!["context7".into()]),
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        save_agent_mcp_config(dir.path(), &initial).unwrap();

        let manifest = decl_manifest(
            r#"[[mcp_servers]]
preset = "brave-search"
tools = ["brave_web_search"]

[[mcp_servers]]
hosted = "pm"
tools = ["pm_claim_task"]"#,
        );
        assert!(project_recommended_mcp_servers(dir.path(), &manifest).unwrap());

        let cfg = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            cfg.recommended
                .iter()
                .filter_map(|d| d.key())
                .collect::<Vec<_>>(),
            vec!["brave-search", "pm"]
        );
        // The three other lists keep their owners.
        assert_eq!(cfg.catalog.len(), 1);
        assert_eq!(cfg.local.len(), 1);
        assert_eq!(cfg.active_names, Some(vec!["context7".into()]));
        let expected = cfg.recommended.clone();
        initial.recommended = expected;
        assert_eq!(cfg.recommended, initial.recommended);
    }

    #[test]
    fn project_recommended_is_idempotent_and_refreshes_on_upgrade() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = decl_manifest("[[mcp_servers]]\npreset = \"brave-search\"");
        assert!(project_recommended_mcp_servers(dir.path(), &v1).unwrap());
        assert!(
            !project_recommended_mcp_servers(dir.path(), &v1).unwrap(),
            "second projection of the same manifest must not write"
        );

        // Package upgrade drops the recommendation.
        let v2 = decl_manifest("");
        assert!(project_recommended_mcp_servers(dir.path(), &v2).unwrap());
        let cfg = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert!(cfg.recommended.is_empty());
    }

    #[test]
    fn project_recommended_drops_dismissals_of_undeclared_servers() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = decl_manifest("[[mcp_servers]]\npreset = \"brave-search\"");
        let cfg = AgentMcpConfig {
            dismissed: vec!["brave-search".into(), "ghost".into()],
            ..Default::default()
        };
        save_agent_mcp_config(dir.path(), &cfg).unwrap();

        assert!(project_recommended_mcp_servers(dir.path(), &manifest).unwrap());
        let cfg = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(
            cfg.dismissed,
            vec!["brave-search".to_string()],
            "a dismissal for a no-longer-declared server must be pruned, the live one kept"
        );
        assert!(cfg.is_dismissed("brave-search"));
        assert!(!cfg.is_dismissed("ghost"));
    }

    #[test]
    fn recommended_tools_map_skips_empty_whitelists() {
        let manifest = decl_manifest(
            r#"[[mcp_servers]]
preset = "brave-search"
tools = ["brave_web_search"]

[[mcp_servers]]
hosted = "pm""#,
        );
        let cfg = AgentMcpConfig {
            recommended: manifest.mcp_servers.clone(),
            ..Default::default()
        };
        let map = cfg.recommended_tools_map();
        assert_eq!(map.len(), 1, "an empty whitelist is 'no opinion'");
        assert_eq!(map["brave-search"], vec!["brave_web_search".to_string()]);
        assert!(cfg.recommendation("pm").is_some());
    }

    /// The whitelist is per server, and a server with no declared tools keeps
    /// the "everything on" baseline.
    #[test]
    fn merge_mcp_tools_config_applies_whitelist_per_server() {
        let persisted = AgentMcpToolsConfig::default();
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "brave-search".to_string(),
            vec![
                McpToolDescriptor {
                    name: "brave_web_search".into(),
                    description: None,
                },
                McpToolDescriptor {
                    name: "brave_news".into(),
                    description: None,
                },
            ],
        );
        server_tools.insert(
            "context7".to_string(),
            vec![McpToolDescriptor {
                name: "resolve-library-id".into(),
                description: None,
            }],
        );

        let mut recommended: IndexMap<String, Vec<String>> = IndexMap::new();
        recommended.insert("brave-search".to_string(), vec!["brave_web_search".into()]);

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &recommended);
        assert!(tool_enabled_in(&merged.servers["brave-search"], "brave_web_search").unwrap());
        assert!(
            !tool_enabled_in(&merged.servers["brave-search"], "brave_news").unwrap(),
            "a tool outside the declared whitelist starts disabled"
        );
        assert!(
            tool_enabled_in(&merged.servers["context7"], "resolve-library-id").unwrap(),
            "undeclared server keeps the everything-enabled baseline"
        );
    }

    /// A user's explicit choice still beats the package whitelist.
    #[test]
    fn merge_mcp_tools_config_user_choice_beats_whitelist() {
        let mut persisted = AgentMcpToolsConfig::default();
        persisted.servers.insert(
            "brave-search".to_string(),
            vec![
                AgentMcpToolItem::new("brave_web_search", false),
                AgentMcpToolItem::new("brave_news", true),
            ],
        );
        let mut server_tools: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
        server_tools.insert(
            "brave-search".to_string(),
            vec![
                McpToolDescriptor {
                    name: "brave_web_search".into(),
                    description: None,
                },
                McpToolDescriptor {
                    name: "brave_news".into(),
                    description: None,
                },
            ],
        );
        let mut recommended: IndexMap<String, Vec<String>> = IndexMap::new();
        recommended.insert("brave-search".to_string(), vec!["brave_web_search".into()]);

        let merged = merge_mcp_tools_config(&persisted, &server_tools, &recommended);
        assert!(!tool_enabled_in(&merged.servers["brave-search"], "brave_web_search").unwrap());
        assert!(tool_enabled_in(&merged.servers["brave-search"], "brave_news").unwrap());
    }

    /// `agent_mcp.json` round-trips the new fields, and a file written
    /// before ADR-104 (no `recommended` / `dismissed` keys) still loads.
    #[test]
    fn agent_mcp_config_recommended_round_trip_and_legacy_load() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = decl_manifest(
            "[[mcp_servers]]\npreset = \"brave-search\"\ntools = [\"brave_web_search\"]",
        );
        let cfg = AgentMcpConfig {
            recommended: manifest.mcp_servers.clone(),
            dismissed: vec!["brave-search".into()],
            ..Default::default()
        };
        save_agent_mcp_config(dir.path(), &cfg).unwrap();

        let back = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(back.recommended, cfg.recommended);
        assert_eq!(back.dismissed, cfg.dismissed);
    }

    /// Adversarial: an old file with a *flat* array is still migrated, and the
    /// migration must not invent recommendations.
    #[test]
    fn legacy_flat_migration_leaves_recommendations_empty() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        let flat = serde_json::to_string(&vec![mcp_def("context7")]).unwrap();
        std::fs::write(dir.path().join("config").join("agent_mcp.json"), flat).unwrap();

        let cfg = load_agent_mcp_config(dir.path()).unwrap().unwrap();
        assert_eq!(cfg.catalog.len(), 1);
        assert!(cfg.recommended.is_empty());
        assert!(cfg.dismissed.is_empty());
    }
}
