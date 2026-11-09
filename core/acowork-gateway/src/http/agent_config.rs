//! Per-agent runtime configuration types.
//!
//! NOTE: Per-agent config persistence has been moved to Runtime
//! ({work_dir}/config/agent_config.json). Gateway only defines the
//! request/response DTOs and forwards queries to Runtime over HTTP.
//!
//! The avatar and the instance's name are agent-private data. The Runtime
//! owns them: they are fields of this same `agent_config.json` (the file
//! this module only defines DTOs for), written by `PUT /avatar-config` and
//! read by everything else. The Gateway reaches them only through the
//! reverse proxy, or through the Node's inventory projection when the agent
//! is stopped. The former Gateway-owned `avatar_cache.json` was removed with
//! that boundary — it was a second writer of the same fact.

use serde::{Deserialize, Serialize};

use acowork_core::ShellApprovalThreshold;
use acowork_core::protocol::McpServerConfigDef;

/// Effective (merged) config returned to API consumers.
#[derive(Debug, Clone, Serialize)]
pub struct AgentConfigResponse {
    pub agent_id: String,
    /// Effective max_output_tokens (per-agent override > global > hardcoded default)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Effective max_iterations
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// Effective temperature
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Source of the effective temperature value:
    /// "config" | "manifest" | "default"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature_source: Option<String>,
    /// The manifest-level temperature — for frontend placeholder display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_temperature: Option<f32>,
    /// Effective context window cap (tokens). 0 = no limit.
    /// Resolved from: agent_config.json → manifest.llm.context_window → DEFAULT_CONTEXT_WINDOW.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Source of the effective context window value:
    /// "config" | "manifest" | "default"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window_source: Option<String>,
    /// The manifest-level context window cap — for frontend placeholder display
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_context_window: Option<u64>,
    /// The manifest-compiled system prompt (read-only, loaded by caller)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// User's system prompt override (None = use manifest default)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt_override: Option<String>,
    /// Effective shell approval threshold
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_approval_threshold: Option<String>,
    /// Current model name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Current provider name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Gateway global max_output_tokens limit
    pub global_max_output_tokens: u64,
    /// Active MCP server names for this agent (from workspace config)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_mcp_servers: Vec<String>,
    /// ADR-024: max sessions limit per-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_sessions: Option<usize>,
    /// ADR-023: Approval timeout in seconds. None = use system default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_timeout_secs: Option<u64>,
    /// ADR-029: Enabled builtin tool names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub builtin_tools: Vec<String>,
    /// ADR-029: Full builtin tools list with enabled flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin_tools_all: Option<serde_json::Value>,
}

/// PUT request body for updating agent config.
#[derive(Debug, Clone, Deserialize)]
pub struct UpdateAgentConfigRequest {
    #[serde(default)]
    pub max_output_tokens: Option<u64>,
    #[serde(default, alias = "tools_limit")]
    pub max_iterations: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub system_prompt_override: Option<String>,
    #[serde(default)]
    pub shell_approval_threshold: Option<ShellApprovalThreshold>,
    #[serde(default)]
    pub mcp_servers: Option<Vec<McpServerConfigDef>>,
    /// ADR-024: max sessions limit per-agent (0 = use default).
    #[serde(default)]
    pub max_sessions: Option<usize>,
    /// ADR-026: Per-agent context window cap (0 = no limit).
    #[serde(default)]
    pub context_window: Option<u64>,
    /// ADR-023: Approval timeout in seconds. None = use system default.
    #[serde(default)]
    pub approval_timeout_secs: Option<u64>,
    /// ADR-029: Enabled builtin tool names to set.
    /// Some(vec![]) disables all builtin tools. None leaves unchanged.
    #[serde(default)]
    pub builtin_tools: Option<Vec<String>>,
}

/// Default global values used as fallback when no override exists.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16_384;
pub const DEFAULT_MAX_ITERATIONS: u32 = 500;
/// Default LLM temperature (final fallback in the chain
/// session → agent_config.json → manifest.toml [llm].temperature → here).
/// **Keep aligned** with `acowork_runtime::config::DEFAULT_TEMPERATURE`
/// so the Gateway HTTP API and Runtime resolve to the same value when both
/// manifest and override are absent.
pub const DEFAULT_TEMPERATURE: f32 = 0.3;
/// Default context window cap (tokens) — final fallback in the per-agent
/// chain: agent_config.json → manifest.llm.context_window → here.
/// **Keep aligned** with `acowork_runtime::config::DEFAULT_CONTEXT_WINDOW`.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;
pub const DEFAULT_SHELL_APPROVAL_THRESHOLD: ShellApprovalThreshold = ShellApprovalThreshold::High;

