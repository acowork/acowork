//! Agent tools configuration use case (Tools panel selection persistence).
//!
//! ADR-040 follow-up: the Tools-panel persistence HTTP handlers (added
//! for the Desktop Tools panel persistence fix) used to call
//! `agent_config::*` directly, bypassing the project's UseCase trait
//! layer. This module exposes those operations behind
//! [`AgentToolsService`] so HTTP handlers become thin protocol
//! converters — consistent with
//! [`crate::usecases::WorkspaceMutationService`],
//! [`crate::usecases::MemoryQueryService`], and the rest of the layer.
//!
//! ## Scope
//!
//! The trait covers **all persistence operations on the three
//! per-agent `agent_*` JSON files that the Tools panel mutates**:
//!
//! | HTTP endpoint | Trait method | File on disk |
//! |---|---|---|
//! | `GET  /agents/{id}/mcp-servers`   | [`AgentToolsService::get_mcp_servers`]   | `agent_mcp.json` |
//! | `PUT  /agents/{id}/mcp-servers`   | [`AgentToolsService::put_mcp_servers`]   | `agent_mcp.json` |
//! | `GET  /agents/{id}/builtin-tools` | [`AgentToolsService::get_builtin_tools`] | `agent_tools.json` |
//! | `PUT  /agents/{id}/builtin-tools` | [`AgentToolsService::put_builtin_tools`] | `agent_tools.json` |
//!
//! **Out of scope (different files, separate trait):**
//!
//! - `GET/PUT /agents/{id}/config` — mutates `agent_config.json` (the
//!   per-agent **runtime** config: temperature, context_window,
//!   max_iterations, …). Lives behind
//!   [`crate::usecases::AgentConfigService`] because that file has a
//!   separate read-modify-write contract, MQTT re-PUBLISH semantics,
//!   and live-broadcast side effects (none of which apply to the
//!   `agent_tools.json` / `agent_mcp.json` files).
//! - `GET /agents/{id}/tools` — read-only merge of the two files,
//!   frontend convenience. Trivially inlinable; not worth a trait method.
//!
//! ## Errors
//!
//! Methods that mutate disk state return
//! [`AgentToolsError::Persistence`] for any I/O or JSON failure — the
//! HTTP layer maps that to 500. Method
//! [`AgentToolsService::put_mcp_servers`] additionally returns
//! [`AgentToolsError::UnknownServers`] (HTTP 400) when a submitted
//! server name does not exist in the merged catalog (catalog + local).
//! `put_builtin_tools` deliberately **does not** validate unknown tool
//! names — it silently ignores them per ADR-029 §7 (the registered
//! tool set evolves across releases, so the front-end's local cache
//! can be stale; see `apply_builtin_tools_patch` for the merge rule).
//!
//! ## Late-bind wiring
//!
//! The implementation [`crate::usecases::RuntimeAgentToolsService`]
//! holds the `work_dir` resolved at boot (no async resource
//! dependencies), so it can be constructed immediately after the
//! workspace services in `session_init.rs` Phase B.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// All error variants that MCP config operations can produce.
///
/// The HTTP layer maps each variant to a deterministic status code so
/// the desktop can distinguish "unknown name" (400) from a generic
/// persistence failure (500) without parsing error strings.
#[derive(Debug, thiserror::Error)]
pub enum AgentToolsError {
    /// The requested MCP server name does not exist in the merged
    /// catalog (catalog entries ∪ local user-installed servers).
    /// Maps to HTTP 400 — the desktop already filters to catalog items,
    /// so this protects against stale names reaching us via direct API
    /// calls.
    #[error("unknown MCP server names (not in catalog+local): {0:?}")]
    UnknownServers(Vec<String>),

    /// The requested recommendation key is not declared by the package
    /// (stale client from a previous package version). Maps to HTTP 404 —
    /// the client refetches, it does not retry.
    #[error("unknown MCP recommendation key: {0}")]
    UnknownRecommendation(String),

    /// Failed to load, parse, or persist an `agent_mcp.json`
    /// file. Maps to HTTP 500.
    #[error("failed to persist agent tools config: {0}")]
    Persistence(String),
}

// ── Request / Response DTOs ─────────────────────────────────────────────

/// Request body for `PUT /agents/{id}/mcp-servers`.
///
/// Mirrors the desktop `mcpStore.setActiveServers({servers: [...]})`
/// wire shape (camelCase on the wire via the existing handler).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PutMcpServersBody {
    /// Catalog names the user ticked in the Tools panel. Each must
    /// resolve to a server in `cfg.merged()`.
    #[serde(default)]
    pub servers: Vec<String>,
}

/// Request body for `PUT /agents/{id}/builtin-tools`.
///
/// Wire shape matches `ToolsTab.toggleBuiltinTool` in the desktop:
/// `{ builtin_tools: [...] }` — the **complete** enabled set. The
/// runtime applies a read-modify-write cycle against `agent_tools.json`
/// so unlisted tools are flipped to `enabled = false`. Platform tools
/// (see `PLATFORM_TOOLS`) are force-enabled by the patcher.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PutBuiltinToolsBody {
    /// Names of builtin tools the user wants enabled.
    #[serde(default)]
    pub builtin_tools: Vec<String>,
}

/// Response for `GET /agents/{id}/mcp-servers`.
#[derive(Debug, Clone, Serialize)]
pub struct McpServersResponse {
    /// Echo of the requested `agent_id` for client-side routing.
    pub agent_id: String,
    /// Active server names (honors `active_names` if set on
    /// `AgentMcpConfig`; falls back to the merged catalog list when the
    /// user has never explicitly chosen).
    pub active_servers: Vec<String>,
    /// ADR-104: the package's `[[mcp_servers]]` recommendations, each with
    /// the state the Tools panel renders. Derived from `agent_mcp.json`'s
    /// `recommended` + `dismissed`, cross-referenced against the merged
    /// install list — the Runtime owns the derivation so every client sees
    /// the same five states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended: Vec<McpRecommendationView>,
}

/// Lifecycle state of one manifest recommendation, as seen by the client.
///
/// Exactly one of these is true at a time, and the client renders one row
/// per recommendation with a single primary action — that is the whole
/// point of having states instead of a boolean (`installed`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpRecommendationState {
    /// Not in `catalog` / `local` yet: the primary action is "install"
    /// (a preset scan) or, for a hosted server, nothing to install.
    NotInstalled,
    /// Installed but not ticked for this agent: the primary action is
    /// "activate".
    InstalledInactive,
    /// Installed **and** ticked: the agent already has these tools.
    Active,
    /// The user pressed "ignore": hidden from the suggestion group until
    /// restored, never re-surfaced by a restart.
    Dismissed,
}

/// One manifest-declared MCP server, projected for the Tools panel.
#[derive(Debug, Clone, Serialize)]
pub struct McpRecommendationView {
    /// Identity key (`McpServerDecl::key`) — the join key against the
    /// catalog / `agent_mcp.json`.
    pub key: String,
    /// How the declaration resolves: `preset` | `hosted` | `inline`.
    pub source: String,
    /// Preset id, when `source == "preset"`.
    pub preset: Option<String>,
    /// Hosted server id, when `source == "hosted"`.
    pub hosted: Option<String>,
    /// Endpoint URL, when `source == "inline"`.
    pub url: Option<String>,
    /// Transport, when `source == "inline"`.
    pub transport: Option<acowork_core::protocol::McpTransportDef>,
    /// Tool whitelist the package wants enabled by default.
    pub tools: Vec<String>,
    /// Secrets the server needs before it is usable.
    pub required_secrets: Vec<String>,
    /// Soft requirement — unmet never blocks; the Tools panel badges the
    /// recommendation row "required" and names any missing secret.
    pub required: bool,
    /// Author's justification, shown verbatim.
    pub why: Option<String>,
    /// Current lifecycle state.
    pub state: McpRecommendationState,
}

/// Request body for `PUT /agents/{id}/mcp-recommendations/{key}`.
///
/// Only the dismissal flag is user-writable: everything else in the
/// recommendation belongs to the package (rewritten from the manifest on
/// every start), so accepting it from the client would be a lie.
#[derive(Debug, Clone, Deserialize)]
pub struct PutMcpRecommendationBody {
    /// `true` = "ignore this suggestion", `false` = "show it again".
    pub dismissed: bool,
}

/// Response for `GET /agents/{id}/builtin-tools` (and the `PUT`
/// acknowledgement). Mirrors the `agent_tools.json` shape so the
/// frontend can re-render without a second round-trip.
#[derive(Debug, Clone, Serialize)]
pub struct BuiltinToolsResponse {
    /// Echo of the requested `agent_id` for client-side routing.
    pub agent_id: String,
    /// Per-tool entries with their enabled flag — same shape as
    /// [`crate::agent_config::AgentToolsConfig::tools`].
    pub tools: Vec<crate::agent_config::AgentToolEntry>,
}

/// Per-agent MCP server view for the Tools panel.
///
/// Rendered as a toggle row in the Desktop Tools tab. `active` is
/// `true` when the server's name is currently in `active_names`.
#[derive(Debug, Clone, Serialize)]
pub struct McpServerView {
    pub name: String,
    pub transport: acowork_core::protocol::McpTransportDef,
    pub url: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub active: bool,
}

/// Response for `GET /agents/{id}/tools` - the merged Tools-panel view.
///
/// Combines both Tools-panel sources (builtin tools, MCP servers)
/// in a single round-trip so the desktop can render
/// the entire panel without chaining separate calls.
///
/// `mcp_servers` keeps the active server **names** (backward-compatible
/// with the per-agent activation store); `mcp_servers_defs` carries the
/// full **per-agent** merged list (catalog ∪ local) with active flags so
/// the panel can render switches for agent-installed and system-injected
/// servers too — not just the gateway-global catalog.
#[derive(Debug, Clone, Serialize)]
pub struct MergedToolsResponse {
    pub agent_id: String,
    pub tools: Vec<crate::agent_config::AgentToolEntry>,
    pub mcp_servers: Vec<String>,
    pub mcp_servers_defs: Vec<McpServerView>,
    /// ADR-104: same derivation as [`McpServersResponse::recommended`],
    /// carried here as well so the Desktop Tools panel renders the whole
    /// panel — builtin tools, MCP toggles, and the recommendation group —
    /// from the single `/tools` round-trip it already makes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recommended: Vec<McpRecommendationView>,
}

// ── Trait ──────────────────────────────────────────────────────────────

/// UseCase trait for the Tools-panel persistence endpoints that mutate
/// the two per-agent `agent_*` JSON files the Tools panel owns
/// (`agent_mcp.json`, `agent_tools.json`).
///
/// See the module-level docs for the rationale, the explicit list of
/// covered endpoints, and the pre-existing handlers that are
/// intentionally out of scope (notably `PUT /agents/{id}/config`,
/// which belongs to `agent_config.json` and lives behind
/// [`crate::usecases::AgentConfigService`]).
#[async_trait]
pub trait AgentToolsService: Send + Sync {
    /// `GET /agents/{id}/mcp-servers` — list active MCP server names.
    async fn get_mcp_servers(&self, agent_id: &str) -> McpServersResponse;

    /// `PUT /agents/{id}/mcp-recommendations/{key}` — dismiss or restore
    /// one manifest recommendation (ADR-104).
    ///
    /// Only the dismissal flag is persistable here; the recommendation
    /// itself is manifest-derived and rewritten on every start. Returns
    /// the full `McpServersResponse` so the client re-renders from a single
    /// authoritative payload instead of patching its own copy.
    async fn put_mcp_recommendation(
        &self,
        agent_id: &str,
        key: &str,
        body: PutMcpRecommendationBody,
    ) -> Result<McpServersResponse, AgentToolsError>;

    /// `PUT /agents/{id}/mcp-servers` — persist the active MCP server
    /// selection. Each name in `body.servers` must resolve in
    /// `cfg.merged()`; otherwise the whole request fails with
    /// [`AgentToolsError::UnknownServers`] (no partial writes).
    async fn put_mcp_servers(
        &self,
        agent_id: &str,
        body: PutMcpServersBody,
    ) -> Result<McpServersResponse, AgentToolsError>;

    /// `GET /agents/{id}/builtin-tools` — list all builtin tools with
    /// their enabled flag. Returns an empty `tools` vec when no
    /// `agent_tools.json` exists on disk (distinct from a 500).
    async fn get_builtin_tools(&self, agent_id: &str) -> BuiltinToolsResponse;

    /// `PUT /agents/{id}/builtin-tools` — persist the **complete**
    /// enabled-set for builtin tools. Performs the standard
    /// read-modify-write cycle via
    /// [`crate::agent_config::apply_builtin_tools_patch`], which:
    ///   - flips every listed name to `enabled = true`,
    ///   - flips every **other** currently-persisted tool to
    ///     `enabled = false`,
    ///   - silently ignores names not present in the current config
    ///     (defensive: future tools arrive via `merge_tools_config` at
    ///     startup, not via this incremental path).
    ///
    /// No `UnknownTools` error variant — unlike MCP, unknown names are
    /// accepted-and-dropped (see ADR-029 §7).
    async fn put_builtin_tools(
        &self,
        agent_id: &str,
        body: PutBuiltinToolsBody,
    ) -> Result<BuiltinToolsResponse, AgentToolsError>;

    /// `GET /agents/{id}/tools` - merged Tools-panel view (builtin +
    /// MCP). Read-only aggregation of the two Tools-panel
    /// config files; the handler does not merge anything itself.
    async fn get_merged_tools(&self, agent_id: &str) -> MergedToolsResponse;

    /// `GET /agents/{id}/mcp-tools` — return the **full** per-tool
    /// list for every MCP server, each row carrying `name`, `enabled`
    /// and `description`. This is the single source of truth the
    /// desktop Tools panel renders directly — the frontend never
    /// maintains its own tool list or defaults (ADR-069).
    async fn get_mcp_tools(&self, agent_id: &str) -> McpToolsResponse;

    /// `PUT /agents/{id}/mcp-tools` — persist the complete
    /// `AgentMcpToolsConfig` sent by the desktop. The body is the
    /// **entire** per-server tool list (same shape as the GET
    /// response), so a single-row toggle carries the full list; the
    /// backend writes it verbatim to `agent_mcp_tools.json`. There is
    /// no server-side merge — the desktop's list is authoritative.
    async fn put_mcp_tools(
        &self,
        agent_id: &str,
        body: PutMcpToolsBody,
    ) -> Result<McpToolsResponse, AgentToolsError>;
}

// ── MCP tools (ADR-069) ────────────────────────────────────────────────

/// Request body for `PUT /agents/{id}/mcp-tools`.
///
/// Wire shape mirrors `agent_mcp_tools.json` exactly — three-way
/// identity between the on-disk file, the GET response, and the PUT
/// body. Each per-server entry is a flat array of every tool the
/// server advertises, each carrying its own `enabled` flag and
/// `description` (sourced from the live MCP `tools/list`).
pub type PutMcpToolsBody = crate::agent_config::AgentMcpToolsConfig;

/// Response for `GET /agents/{id}/mcp-tools`.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolsResponse {
    pub agent_id: String,
    pub servers: indexmap::IndexMap<String, Vec<crate::agent_config::AgentMcpToolItem>>,
}
