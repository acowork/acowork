//! manifest.toml data structures for .agent packages
//!
//! The AgentManifest is the core declaration of every .agent package.
//! It defines the agent's identity, capabilities, permissions, and
//! configuration in a declarative TOML format.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::permission::Permission;

/// Skill injection mode for system prompt.
///
/// Controls how skill definitions are injected into the Agent's system prompt.
/// This is an internal runtime representation derived from `[skills]` config.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SkillMode {
    /// Do not inject any skill content into system prompt.
    /// Skills are loaded on-demand via command or trigger.
    #[default]
    Manual,
    /// Inject a compact summary list (name + description) of available skills.
    /// Full instructions are loaded on-demand when a skill is activated.
    Progressive,
}

/// Skill configuration section in manifest.toml
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SkillsConfig {
    /// Whether to use progressive skill injection mode.
    /// When `true`, a compact summary (name + description) of available skills
    /// is injected into the system prompt. When `false` (default), no skill
    /// content is injected.
    #[serde(default)]
    pub progressive: bool,
}

impl SkillsConfig {
    /// Convert the boolean flag to the internal `SkillMode` enum.
    pub fn mode(&self) -> SkillMode {
        if self.progressive {
            SkillMode::Progressive
        } else {
            SkillMode::Manual
        }
    }
}

/// Manifest `[config]` section — package-author defaults for
/// `agent_config.json` (the Runtime-side per-agent config file).
///
/// This is the **package's factory default**, not the user's setting. The
/// Runtime seeds `agent_config.json` from it on first start and never
/// overwrites a field the user already set (see
/// `seed_agent_config_from_manifest`). Every field is optional: `None`
/// means "the author has no opinion" and the next layer of the chain
/// (RuntimeConfig / platform constant) decides.
///
/// Only *behaviour* knobs belong here. Security boundaries
/// (`shell_approval_threshold`, `approval_timeout_secs`), host resource
/// quotas (`max_sessions`), identity preferences (`session_language`) and
/// user-owned overrides (`system_prompt_override`) are deliberately
/// excluded — a package must not be able to lower an approval threshold
/// or claim a user preference.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ManifestConfigDefaults {
    /// LLM sampling temperature (0.0–2.0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Per-agent context window cap in tokens. 0 is invalid (ADR-074 §6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Per-model output token cap (the `agent_config.json`
    /// `max_output_tokens` semantics — a ceiling, not a request override).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Max LLM iterations per run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u32>,
    /// ADR-061 compaction target, expressed as the saved share
    /// (0.90 = "compress until at most 10% remains"). Valid 0.05–0.95.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression_ratio_threshold: Option<f64>,
}

impl ManifestConfigDefaults {
    /// True when the author declared nothing — the `[config]` section is
    /// then omitted on serialization and the Runtime seeds no value.
    pub fn is_empty(&self) -> bool {
        self.temperature.is_none()
            && self.context_window.is_none()
            && self.max_output_tokens.is_none()
            && self.max_iterations.is_none()
            && self.compression_ratio_threshold.is_none()
    }

    /// Range-check every declared value. Returns the offending field name
    /// and the accepted range so a packaging error names the fix.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(t) = self.temperature
            && !(0.0..=2.0).contains(&t)
        {
            return Err(format!("[config].temperature must be 0.0–2.0, got {t}"));
        }
        if let Some(cw) = self.context_window
            && cw == 0
        {
            return Err(
                "[config].context_window must be > 0 (0 is not a \"no limit\" sentinel, ADR-074 §6)"
                    .to_string(),
            );
        }
        if let Some(m) = self.max_output_tokens
            && m == 0
        {
            return Err("[config].max_output_tokens must be > 0".to_string());
        }
        if let Some(mi) = self.max_iterations
            && mi == 0
        {
            return Err("[config].max_iterations must be > 0".to_string());
        }
        if let Some(r) = self.compression_ratio_threshold
            && !(0.05..=0.95).contains(&r)
        {
            return Err(format!(
                "[config].compression_ratio_threshold must be 0.05–0.95, got {r}"
            ));
        }
        Ok(())
    }
}

/// Where a `[[mcp_servers]]` declaration resolves its server from.
///
/// Exactly one of the three forms is legal (see [`McpServerDecl::validate`]).
/// The distinction matters because it decides who may install and where the
/// credentials live — a package never carries executable content itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpSourceKind {
    /// Reference to a platform preset (`preset = "brave-search"`). Resolved
    /// by the client's / Gateway's preset registry; installation goes
    /// through the ADR-072 pipeline and lands in the machine-wide catalog.
    Preset,
    /// Reference to a Gateway-hosted MCP that is auto-injected into every
    /// agent's catalog (`hosted = "pm"` — pm / doc / memory). Nothing to
    /// install; the declaration only states which tools the agent wants.
    Hosted,
    /// Self-contained remote server (`transport = "http"` + `url`).
    /// Requires a `Network` permission covering the URL.
    InlineRemote,
}

/// One `[[mcp_servers]]` entry — a **recommendation**, never an install
/// action.
///
/// The package says "this agent works better with these servers"; the user
/// decides whether to install (`Preset` form) and whether to activate. The
/// Runtime projects the list into `agent_mcp.json` under `recommended`
/// (never into `catalog` or `local`), and activation stays a user action
/// (ADR-090).
///
/// Granularity is deliberately *server + tool-name whitelist*: no tool
/// input schemas are copied here, because the MCP server's own
/// `tools/list` is the single source of truth for those.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct McpServerDecl {
    /// Identity key. Defaults to the `preset` / `hosted` id when omitted;
    /// it is the join key against the catalog / `agent_mcp.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Platform preset id (see `McpSourceKind::Preset`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Gateway-hosted server id (see `McpSourceKind::Hosted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted: Option<String>,
    /// Transport for the inline form. `Some(Http | Sse)` only — stdio is
    /// rejected because it would let a package name an executable to run
    /// on the user's machine while wearing a "recommendation" label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<crate::protocol::McpTransportDef>,
    /// Inline remote endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Secret names the server needs before it is usable (ADR-090 D4).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_secrets: Vec<String>,
    /// Tool-name whitelist the agent expects. Becomes the initial enabled
    /// set in `agent_mcp_tools.json` once the user activates the server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// Soft requirement: when unmet the UI shows "capability limited". It
    /// never blocks startup or chat.
    #[serde(default)]
    pub required: bool,
    /// Author's reason, shown verbatim in the Tools panel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

impl McpServerDecl {
    /// Resolved identity key: explicit `name`, else the preset / hosted id.
    pub fn key(&self) -> Option<&str> {
        self.name
            .as_deref()
            .or(self.preset.as_deref())
            .or(self.hosted.as_deref())
    }

    /// Which of the three declaration forms this entry uses.
    pub fn source_kind(&self) -> Option<McpSourceKind> {
        if self.preset.is_some() {
            Some(McpSourceKind::Preset)
        } else if self.hosted.is_some() {
            Some(McpSourceKind::Hosted)
        } else if self.transport.is_some() || self.url.is_some() {
            Some(McpSourceKind::InlineRemote)
        } else {
            None
        }
    }

    /// Validate one declaration. `permissions` is the manifest's declared
    /// permission list — the inline form must be covered by a `Network`
    /// permission for its URL.
    pub fn validate(&self, permissions: &[Permission]) -> Result<(), String> {
        let label = self.key().unwrap_or("<unnamed>");
        let kind = self.source_kind().ok_or_else(|| {
            format!(
                "mcp_servers `{label}`: must declare exactly one of `preset`, `hosted`, \
                 or `transport` + `url`"
            )
        })?;

        match kind {
            McpSourceKind::Preset => {
                if self.hosted.is_some() || self.url.is_some() || self.transport.is_some() {
                    return Err(format!(
                        "mcp_servers `{label}`: `preset` cannot be combined with `hosted` / inline fields"
                    ));
                }
            }
            McpSourceKind::Hosted => {
                if self.url.is_some() || self.transport.is_some() {
                    return Err(format!(
                        "mcp_servers `{label}`: `hosted` cannot be combined with inline fields"
                    ));
                }
            }
            McpSourceKind::InlineRemote => {
                if self.preset.is_some() || self.hosted.is_some() {
                    return Err(format!(
                        "mcp_servers `{label}`: inline form cannot be combined with `preset` / `hosted`"
                    ));
                }
                match self.transport {
                    Some(crate::protocol::McpTransportDef::Http)
                    | Some(crate::protocol::McpTransportDef::Sse) => {}
                    Some(crate::protocol::McpTransportDef::Stdio) => {
                        return Err(format!(
                            "mcp_servers `{label}`: inline stdio is not allowed — a package must not \
                             name an executable to run on the user's machine. Reference a preset instead."
                        ));
                    }
                    None => {
                        return Err(format!(
                            "mcp_servers `{label}`: inline form requires `transport` (http or sse)"
                        ));
                    }
                }
                let url = self.url.as_deref().unwrap_or_default();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(format!(
                        "mcp_servers `{label}`: inline `url` must be an http(s) URL"
                    ));
                }
                let covered = permissions.iter().any(|p| match p {
                    Permission::Network(None) => true,
                    Permission::Network(Some(scope)) => url.starts_with(scope.as_str()),
                    _ => false,
                });
                if !covered {
                    return Err(format!(
                        "mcp_servers `{label}`: inline url `{url}` is not covered by a `Network` permission"
                    ));
                }
            }
        }

        let mut seen = std::collections::HashSet::new();
        for t in &self.tools {
            if t.trim().is_empty() {
                return Err(format!(
                    "mcp_servers `{label}`: tool names must not be empty"
                ));
            }
            if !seen.insert(t.as_str()) {
                return Err(format!(
                    "mcp_servers `{label}`: duplicate tool `{t}`"
                ));
            }
        }
        let mut seen = std::collections::HashSet::new();
        for s in &self.required_secrets {
            if s.trim().is_empty() {
                return Err(format!(
                    "mcp_servers `{label}`: secret names must not be empty"
                ));
            }
            if !seen.insert(s.as_str()) {
                return Err(format!(
                    "mcp_servers `{label}`: duplicate secret `{s}`"
                ));
            }
        }
        Ok(())
    }
}

/// Minimal skill projection used by [`AgentManifest::validate_graph`].
///
/// Deliberately not `acowork_runtime::skills::SkillDefinition`: `acowork-core`
/// must not depend on the Runtime, and the graph check only needs the name
/// plus the declared tool dependencies (both come from `SKILL.md`
/// frontmatter, parsed by whoever owns the package contents).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDecl {
    /// Skill name (the `skills/<dir>` name and the `SKILL.md` `name`).
    pub name: String,
    /// Tools the skill needs (`SKILL.md` `tool_deps`).
    pub tool_deps: Vec<String>,
}

/// Result of [`AgentManifest::validate_graph`].
///
/// `errors` are package defects (a dangling reference — the package is
/// wrong and must be fixed). `warnings` are hints (the declaration is
/// incomplete but the package still runs).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManifestGraphReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

impl ManifestGraphReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// True when there is nothing to report at all.
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty() && self.warnings.is_empty()
    }
}

/// One capability entry of a [`ManifestDraft`].
///
/// Separate from [`CapabilityDef`] only because a draft is author-facing: the
/// table key (`action`) is part of the row, so the wizard can hold an ordered
/// list and edit an action's name in place.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DraftCapability {
    /// `[capabilities.<action>]` table key.
    pub action: String,
    #[serde(default)]
    pub description: String,
    /// Skill names this capability is implemented by (ADR-104).
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
}

/// Author-facing description of a manifest that does not exist yet.
///
/// ## Why this is in `core` and not in the caller
///
/// Building a manifest from partial author input is a **domain** operation: it
/// decides which fields get defaults, which combinations are refused, and which
/// compulsory checks run. Any surface that creates packages needs the same
/// answers — the Desktop wizard today, a mobile/CLI authoring path later — and
/// a second implementation is guaranteed to disagree with the first. Keeping
/// one [`ManifestDraft::build`] also makes the pair "preview" / "create"
/// produce byte-identical manifests by construction.
///
/// Nothing here touches a filesystem: skill *content* lives in
/// `skills/<name>/`, so the caller passes the parsed [`SkillDecl`]s it actually
/// ships and the draft is validated against exactly those.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ManifestDraft {
    pub agent_id: String,
    pub name: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Search/attribute tags (ADR-104).
    #[serde(default)]
    pub labels: Vec<String>,
    /// `[config]` defaults. `None` writes no table: an absent default is not
    /// the same statement as an explicit one.
    #[serde(default)]
    pub config: Option<ManifestConfigDefaults>,
    /// Builtin tool names (`[[tools]]` with `type = "builtin"`).
    #[serde(default)]
    pub tools: Vec<String>,
    /// Recommended MCP servers (ADR-104).
    #[serde(default)]
    pub mcp_servers: Vec<McpServerDecl>,
    #[serde(default)]
    pub capabilities: Vec<DraftCapability>,
    /// Marks the package as locally authored (skips marketplace/sign checks).
    #[serde(default)]
    pub dev: bool,
}

impl ManifestDraft {
    /// Turn the draft into a validated manifest.
    ///
    /// `skills` are the skill declarations the package will actually ship —
    /// used for the ADR-104 graph check (a capability may only link a skill
    /// that is really there).
    ///
    /// Errors come back as one joined message. `AgentManifest::validate`
    /// itself stops at its first defect, so this is not "every problem at
    /// once" — but the draft-level checks (identity, duplicate actions) and
    /// the graph check all accumulate into that message instead of hiding
    /// behind whichever validation error happened to fire first.
    pub fn build(&self, skills: &[SkillDecl]) -> Result<AgentManifest, String> {
        let mut problems: Vec<String> = Vec::new();

        let agent_id = self.agent_id.trim().to_string();
        let name = self.name.trim().to_string();
        if agent_id.is_empty() {
            problems.push("agent_id must not be empty".to_string());
        }
        if name.is_empty() {
            problems.push("name must not be empty".to_string());
        }
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }

        let version = self
            .version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or(DEFAULT_DRAFT_VERSION)
            .to_string();
        let author = self
            .author
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or(DEFAULT_DRAFT_AUTHOR)
            .to_string();
        let description = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{name} agent"));

        let mut capabilities = std::collections::HashMap::new();
        for cap in &self.capabilities {
            let action = cap.action.trim();
            if action.is_empty() {
                problems.push("a capability row has an empty action name".to_string());
                continue;
            }
            if capabilities.contains_key(action) {
                problems.push(format!("duplicate capability `{action}`"));
                continue;
            }
            capabilities.insert(
                action.to_string(),
                CapabilityDef {
                    description: cap.description.clone(),
                    skills: cap.skills.clone(),
                    input_schema: cap.input_schema.clone(),
                    output_schema: cap.output_schema.clone(),
                },
            );
        }

        let manifest = AgentManifest {
            agent_id,
            version,
            name,
            display_name: self.display_name.clone().filter(|s| !s.trim().is_empty()),
            role: self.role.clone().filter(|s| !s.trim().is_empty()),
            avatar: None,
            builtin_avatar: None,
            description,
            author,
            runtime_version: DEFAULT_DRAFT_RUNTIME_VERSION.to_string(),
            permissions: Vec::new(),
            triggers: Vec::new(),
            llm: Default::default(),
            memory: Default::default(),
            identity_deps: Vec::new(),
            tools: self
                .tools
                .iter()
                .map(|n| ToolDeclaration {
                    tool_type: "builtin".to_string(),
                    name: n.clone(),
                    config: None,
                    rag: None,
                })
                .collect(),
            capabilities,
            resources: Default::default(),
            sandbox: Default::default(),
            dev: self.dev,
            skills: Default::default(),
            config: self.config.clone().unwrap_or_default(),
            labels: self.labels.clone(),
            mcp_servers: self.mcp_servers.clone(),
        };

        if let Err(e) = manifest.validate() {
            problems.push(e.to_string());
        }
        // Graph check runs even when something else already failed: a
        // dangling capability→skill edge stays a real defect, and a wizard
        // that showed it only after the previous fix would waste a round trip.
        problems.extend(manifest.validate_graph(skills).errors);

        if problems.is_empty() {
            Ok(manifest)
        } else {
            Err(problems.join("; "))
        }
    }
}

/// `runtime_version` written by draft-built manifests (`0.1.0`).
const DEFAULT_DRAFT_RUNTIME_VERSION: &str = "0.1.0";
/// Default `version` when the draft omits one.
const DEFAULT_DRAFT_VERSION: &str = "0.1.0";
/// Default `author` when the draft omits one.
const DEFAULT_DRAFT_AUTHOR: &str = "ACowork User";

/// MCP servers the platform injects into every agent's catalog. A skill may
/// reference their tools (`mcp_memory__memory_store`) without the package
/// having to declare the server.
const PLATFORM_HOSTED_MCP: &[&str] = &["memory", "pm", "doc"];

/// Upper bound on `labels` — keeps the Agent list chips and the future
/// marketplace facets usable.
pub const MAX_MANIFEST_LABELS: usize = 12;
/// Lower bound of a label's length (two chars, so `ai` is legal and `x` is not).
pub const MIN_LABEL_LEN: usize = 2;
/// Upper bound of a label's length.
pub const MAX_LABEL_LEN: usize = 32;

/// True when `label` is a lowercase kebab slug within the length bounds.
///
/// Shared by [`AgentManifest::validate`] and the Desktop create wizard so the
/// client-side chip input cannot accept something the manifest will reject.
pub fn is_valid_label(label: &str) -> bool {
    let len = label.chars().count();
    if !(MIN_LABEL_LEN..=MAX_LABEL_LEN).contains(&len) {
        return false;
    }
    label
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// Complete manifest.toml structure for .agent packages
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentManifest {
    /// Reverse-domain identifier (e.g., "com.example.weather").
    ///
    /// ADR-073: this field represents PACKAGE identity ONLY — the
    /// equivalent of a Java Class / npm package name / Docker image
    /// name. It is immutable and globally unique per package, but it
    /// is NOT a runtime instance identifier. One package may be
    /// installed many times, each install producing a distinct
    /// `AgentInstanceId` (UUID v4) generated by the Gateway. Never use
    /// `agent_id` as a HashMap key for installed/running instances.
    pub agent_id: String,
    /// Semantic version (e.g., "1.0.0")
    pub version: String,
    /// Human-readable name
    pub name: String,
    /// Short display name for chat UI (defaults to `name` if absent)
    #[serde(default)]
    pub display_name: Option<String>,
    /// Agent role / job title (e.g. "Project Manager")
    #[serde(default)]
    pub role: Option<String>,
    /// Path to avatar image within the .agent package (e.g. "assets/avatar.png")
    #[serde(default)]
    pub avatar: Option<String>,
    /// Builtin avatar index selected at packaging time (e.g. "icon-05" or "5").
    /// Used as the default avatar for first install when `avatar` is not set.
    /// Resolved by the client against its bundled builtin icon set; an
    /// unrecognised value falls back to a random builtin icon at install time.
    #[serde(default)]
    pub builtin_avatar: Option<String>,
    /// Short description
    pub description: String,
    /// Author identifier
    pub author: String,
    /// Minimum runtime version required
    pub runtime_version: String,
    /// Declared permissions
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// Activation triggers (cron, event, etc.)
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    /// LLM provider configuration (optional — provider/model come from resource_cache)
    #[serde(default)]
    pub llm: LlmConfig,
    /// Memory system configuration
    #[serde(default)]
    pub memory: MemoryConfig,
    /// Dependencies on System Agent identity fields
    #[serde(default)]
    pub identity_deps: Vec<String>,
    /// Tool declarations
    #[serde(default)]
    pub tools: Vec<ToolDeclaration>,
    /// Capability advertisements for Intent routing
    #[serde(default)]
    pub capabilities: HashMap<String, CapabilityDef>,
    /// Resource limits
    #[serde(default)]
    pub resources: ResourceLimits,
    /// Sandbox configuration
    #[serde(default)]
    pub sandbox: SandboxConfig,
    /// Whether developer mode is enabled
    #[serde(default)]
    pub dev: bool,
    /// Skill configuration
    #[serde(default)]
    pub skills: SkillsConfig,
    /// Package-author defaults for `agent_config.json` (the `[config]`
    /// section). Seeded by the Runtime on first start; never overwrites a
    /// value the user already set.
    #[serde(default, skip_serializing_if = "ManifestConfigDefaults::is_empty")]
    pub config: ManifestConfigDefaults,
    /// Retrieval / discovery tags (`labels = ["agile", "requirements"]`).
    ///
    /// A label is a **filter dimension only** — it is not a capability (an
    /// addressable action), not a skill name, and not the display `role`.
    /// Format and count are enforced by [`AgentManifest::validate`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Recommended MCP servers (`[[mcp_servers]]`).
    ///
    /// A recommendation, not a dependency: the user confirms installation
    /// and activation (ADR-090). Projected into `agent_mcp.json`
    /// `recommended`, never into `catalog` / `local`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServerDecl>,
}

impl AgentManifest {
    /// Returns the effective skill mode for this agent.
    pub fn skill_mode(&self) -> SkillMode {
        self.skills.mode()
    }

    /// Parse manifest from TOML string
    pub fn from_toml(toml_str: &str) -> Result<Self, ManifestError> {
        let manifest: Self = toml::from_str(toml_str)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Serialize manifest to TOML string
    pub fn to_toml(&self) -> Result<String, ManifestError> {
        let toml_str = toml::to_string_pretty(self)?;
        Ok(toml_str)
    }

    /// Validate manifest fields
    pub fn validate(&self) -> Result<(), ManifestError> {
        // agent_id must follow reverse-domain format
        if self.agent_id.is_empty() {
            return Err(ManifestError::Validation("agent_id cannot be empty".into()));
        }
        if !self.agent_id.contains('.') {
            return Err(ManifestError::Validation(
                "agent_id must follow reverse-domain format (e.g., com.example.agent)".into(),
            ));
        }

        // version must be valid semver-like
        if self.version.is_empty() {
            return Err(ManifestError::Validation("version cannot be empty".into()));
        }

        // name cannot be empty
        if self.name.is_empty() {
            return Err(ManifestError::Validation("name cannot be empty".into()));
        }

        // runtime_version cannot be empty
        if self.runtime_version.is_empty() {
            return Err(ManifestError::Validation(
                "runtime_version cannot be empty".into(),
            ));
        }

        // Provider/model selection is now governed by global_provider_list
        // and the Gateway's ProviderListUpdate, not by manifest fields.

        // `[config]` ranges — a packaging error, not a runtime one.
        self.config
            .validate()
            .map_err(ManifestError::Validation)?;

        self.validate_labels()?;
        self.validate_mcp_servers()?;
        self.validate_capabilities()?;

        Ok(())
    }

    /// Labels: lowercase kebab slugs, bounded count, no duplicates.
    ///
    /// Bounds are enforced here (not at display time) so a bad package
    /// fails at packaging / install with a fixable message instead of
    /// producing chips nobody can filter on.
    fn validate_labels(&self) -> Result<(), ManifestError> {
        if self.labels.len() > MAX_MANIFEST_LABELS {
            return Err(ManifestError::Validation(format!(
                "labels: at most {MAX_MANIFEST_LABELS} allowed, got {}",
                self.labels.len()
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for label in &self.labels {
            if !is_valid_label(label) {
                return Err(ManifestError::Validation(format!(
                    "labels: `{label}` is invalid — lowercase letters, digits and hyphens only, \
                     {}–{} chars",
                    MIN_LABEL_LEN, MAX_LABEL_LEN
                )));
            }
            if !seen.insert(label.as_str()) {
                return Err(ManifestError::Validation(format!(
                    "labels: duplicate `{label}`"
                )));
            }
        }
        Ok(())
    }

    /// `[[mcp_servers]]`: each entry must pick exactly one form and the
    /// identity keys must be unique.
    fn validate_mcp_servers(&self) -> Result<(), ManifestError> {
        let mut seen = std::collections::HashSet::new();
        for decl in &self.mcp_servers {
            decl.validate(&self.permissions)
                .map_err(ManifestError::Validation)?;
            if let Some(key) = decl.key()
                && !seen.insert(key)
            {
                return Err(ManifestError::Validation(format!(
                    "mcp_servers: duplicate server `{key}`"
                )));
            }
        }
        Ok(())
    }

    /// `[capabilities.*]`: action names must be usable as intent actions.
    fn validate_capabilities(&self) -> Result<(), ManifestError> {
        for (action, def) in &self.capabilities {
            if action.trim().is_empty() || action.len() > 64 {
                return Err(ManifestError::Validation(format!(
                    "capabilities: action name `{action}` must be 1–64 chars"
                )));
            }
            if def.description.trim().is_empty() {
                return Err(ManifestError::Validation(format!(
                    "capabilities.`{action}`: description must not be empty"
                )));
            }
        }
        Ok(())
    }

    /// Check if a specific tool is declared in the manifest
    pub fn has_tool(&self, tool_name: &str) -> bool {
        self.tools.iter().any(|t| t.name == tool_name)
    }

    /// Get tool declaration by name
    pub fn get_tool(&self, tool_name: &str) -> Option<&ToolDeclaration> {
        self.tools.iter().find(|t| t.name == tool_name)
    }

    /// Names of all `[[tools]]` declarations in the manifest (builtin or rag).
    ///
    /// Used by ADR-029 to bootstrap `agent_tools.json`: any tool name
    /// in this list is seeded as `enabled = true` on first start.
    /// RAG tools are intentionally excluded — RAG is opt-in via the
    /// gateway, not via the runtime registry.
    pub fn builtin_tool_names(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|t| t.tool_type == "builtin" || t.tool_type.is_empty())
            .map(|t| t.name.clone())
            .collect()
    }

    /// True if the manifest contains at least one `[[tools]]` entry.
    pub fn has_any_tool_declaration(&self) -> bool {
        !self.tools.is_empty()
    }

    /// Get all cron triggers from the manifest
    pub fn cron_triggers(&self) -> Vec<&Trigger> {
        self.triggers
            .iter()
            .filter(|t| t.trigger_type == "cron")
            .collect()
    }

    /// Get the raw builtin_avatar value from the manifest, if any.
    /// Returns the value as-authored (e.g. "icon-05", "5", "05"); the client
    /// is responsible for normalising it against its bundled icon set.
    pub fn builtin_avatar(&self) -> Option<&str> {
        self.builtin_avatar.as_deref()
    }

    /// Get the first RAG tool configuration, if any.
    ///
    /// Returns `Some((tool_name, &RagToolConfig))` if a `[[tools]]` entry
    /// with `type = "rag"` is declared, otherwise `None`.
    pub fn rag_config(&self) -> Option<(&str, &RagToolConfig)> {
        self.tools
            .iter()
            .find(|t| t.is_rag())
            .and_then(|t| t.rag.as_ref().map(|rag| (t.name.as_str(), rag)))
    }

    /// Check if this manifest declares any RAG tool
    pub fn has_rag(&self) -> bool {
        self.tools.iter().any(|t| t.is_rag())
    }

    /// Effective package-author defaults for `agent_config.json`.
    ///
    /// `[config]` wins; the deprecated `[llm]` fields (`temperature`,
    /// `context_window`, `max_tokens`) are still read so existing packages
    /// keep working — see ADR-104 §Migration. `[llm].max_tokens` maps onto
    /// `max_output_tokens`, which is a **ceiling** semantics, not the old
    /// request-level hard override.
    pub fn config_defaults(&self) -> ManifestConfigDefaults {
        ManifestConfigDefaults {
            temperature: self.config.temperature.or(self.llm.temperature),
            context_window: self.config.context_window.or(self.llm.context_window),
            max_output_tokens: self
                .config
                .max_output_tokens
                .or(self.llm.max_tokens.map(u64::from)),
            max_iterations: self.config.max_iterations,
            compression_ratio_threshold: self.config.compression_ratio_threshold,
        }
    }

    /// Identity keys of the declared `[[mcp_servers]]`, in declaration order.
    pub fn mcp_server_keys(&self) -> Vec<&str> {
        self.mcp_servers.iter().filter_map(|d| d.key()).collect()
    }

    /// Names declared as `preset` or `hosted` references — the two forms the
    /// Runtime can resolve without the package carrying server details.
    pub fn recommended_mcp_keys(&self) -> Vec<&str> {
        self.mcp_servers
            .iter()
            .filter(|d| {
                matches!(
                    d.source_kind(),
                    Some(McpSourceKind::Preset) | Some(McpSourceKind::Hosted)
                )
            })
            .filter_map(|d| d.key())
            .collect()
    }

    /// Cross-reference check over the capability / skill / tool / mcp graph.
    ///
    /// This is the check that makes the four declaration kinds cohere:
    ///
    /// - `capabilities.*.skills` must name skills that ship in the package
    ///   → **error** (a dangling reference is a packaging defect);
    /// - every skill's `tool_deps` should be reachable from `[[tools]]` or
    ///   `[[mcp_servers]].tools`, or be a tool of a platform-hosted MCP
    ///   (`mcp_memory__*`) → **warning** (the agent still runs, it just will
    ///   not have the tool the skill expects).
    ///
    /// Capability → skill reference check, on skill *names* only.
    ///
    /// Split out because the packager (`acowork-sign`) can list the
    /// `skills/<dir>/` directories but cannot parse `SKILL.md` frontmatter —
    /// that parser lives in the Runtime, which core must not depend on. This
    /// half is the part every layer can check, so it is the part that is an
    /// **error**; [`Self::validate_graph`] adds the tool-dependency half as
    /// warnings on top of it.
    pub fn validate_capability_skills(&self, skill_names: &[&str]) -> Vec<String> {
        let known: std::collections::HashSet<&str> = skill_names.iter().copied().collect();
        let mut errors = Vec::new();
        for (action, def) in &self.capabilities {
            for skill in &def.skills {
                if !known.contains(skill.as_str()) {
                    errors.push(format!(
                        "capabilities.`{action}` references skill `{skill}`, which is not in the package"
                    ));
                }
            }
        }
        errors
    }

    /// Called from the packager (publish time) and the installer (install
    /// time) with the skills actually present in the package.
    pub fn validate_graph(&self, skills: &[SkillDecl]) -> ManifestGraphReport {
        let mut report = ManifestGraphReport::default();

        // 1. capability → skill references must resolve.
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        report.errors.extend(self.validate_capability_skills(&names));

        // 2. skill tool_deps must be reachable from the declarations.
        let mut available: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for tool in &self.tools {
            available.insert(tool.name.as_str());
        }
        for decl in &self.mcp_servers {
            for tool in &decl.tools {
                available.insert(tool.as_str());
            }
        }

        for skill in skills {
            for dep in &skill.tool_deps {
                if available.contains(dep.as_str()) {
                    continue;
                }
                if let Some(server) = dep.strip_prefix("mcp_").and_then(|r| r.split_once("__")) {
                    let (server_name, _tool) = server;
                    if PLATFORM_HOSTED_MCP.contains(&server_name)
                        || self
                            .recommended_mcp_keys()
                            .iter()
                            .any(|k| k.eq_ignore_ascii_case(server_name))
                    {
                        // Platform-injected or already recommended — reachable.
                        continue;
                    }
                }
                report.warnings.push(format!(
                    "skill `{}` depends on `{dep}`, which is not declared in [[tools]] or \
                     [[mcp_servers]].tools",
                    skill.name
                ));
            }
        }

        report
    }
}

/// LLM provider configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmConfig {
    /// Maximum tokens in response
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// LLM sampling temperature (0.0 = deterministic, 2.0 = max creative).
    /// Per-agent package-level default. Resolution chain at runtime:
    /// per-session override → agent_config.json override → this manifest value
    /// → hardcoded `DEFAULT_TEMPERATURE` (0.3).
    /// When `None`, falls through to the next level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Per-agent context window size limit in tokens.
    /// 0 means "no limit" (use model's full context window).
    /// Resolution chain at runtime:
    /// agent_config.json → this manifest value → DEFAULT_CONTEXT_WINDOW (200K).
    /// When `None`, falls through to the next level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Fallback providers in priority order
    #[serde(default)]
    pub fallback_providers: Vec<String>,
    /// Multiple provider configurations (key = provider name)
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    /// Routing configuration
    #[serde(default)]
    pub routing: Option<RoutingConfig>,
    /// Budget configuration for LLM usage
    #[serde(default)]
    pub budget: Option<LlmBudget>,
}

/// Configuration for a specific LLM provider
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Model identifier for this provider
    pub model: String,
    /// API key reference (e.g., "vault:openai_key")
    #[serde(default)]
    pub api_key_ref: Option<String>,
    /// Custom base URL
    #[serde(default)]
    pub base_url: Option<String>,
    /// Provider-specific parameters
    #[serde(default)]
    pub params: Option<HashMap<String, serde_json::Value>>,
}

/// Routing strategy configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    /// Routing strategy: cost_priority / quality_priority / latency_priority
    #[serde(default = "default_routing_strategy")]
    pub strategy: String,
    /// Fallback provider order (overrides fallback_providers)
    #[serde(default)]
    pub fallback_order: Vec<String>,
    /// Enable automatic provider switching on failure
    #[serde(default = "default_true")]
    pub auto_switch: bool,
}

fn default_routing_strategy() -> String {
    "quality_priority".to_string()
}

fn default_true() -> bool {
    true
}

/// LLM budget configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmBudget {
    /// Maximum tokens per request
    #[serde(default)]
    pub max_tokens_per_request: Option<u64>,
    /// Maximum cost per request in USD
    #[serde(default)]
    pub max_cost_per_request_usd: Option<f64>,
    /// Maximum output tokens
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    /// Action when budget exceeded: stop / fallback_to_local / warn
    #[serde(default = "default_budget_action")]
    pub exceeded_action: String,
}

fn default_budget_action() -> String {
    "stop".to_string()
}

/// Memory system configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Whether memory is enabled for this agent
    #[serde(default = "default_memory_enabled")]
    pub enabled: bool,
    /// Retention period in days
    #[serde(default)]
    pub retention_days: Option<u32>,
    /// Memory quality overrides (ADR-062 D2), parsed from the
    /// `[memory.quality]` manifest section.
    #[serde(default)]
    pub quality: Option<ManifestMemoryQuality>,
    /// EpisodicDistiller overrides (ADR-068 M4/M7), parsed from the
    /// `[memory.distiller]` manifest section. `None` = distiller disabled
    /// with [`DistillerConfig`] defaults.
    ///
    /// ADR-068 review revision: the distiller is **opt-in**. It is disabled
    /// unless the manifest explicitly sets `[memory.distiller].enabled = true`
    /// (see [`MemoryConfig::distiller_enabled`]).
    #[serde(default)]
    pub distiller: Option<ManifestDistillerConfig>,
}

impl MemoryConfig {
    /// Whether the EpisodicDistiller step is enabled for this agent
    /// (ADR-068 M4/M7).
    ///
    /// Single source of truth for the switch: the distiller is **off by
    /// default** (opt-in). It turns on only when the manifest contains a
    /// `[memory.distiller]` section with `enabled = true`. An absent section,
    /// an empty section, or an explicit `enabled = false` all resolve to
    /// `false`.
    pub fn distiller_enabled(&self) -> bool {
        self.distiller.as_ref().map(|d| d.enabled).unwrap_or(false)
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            enabled: default_memory_enabled(),
            retention_days: None,
            quality: None,
            distiller: None,
        }
    }
}

fn default_memory_enabled() -> bool {
    true
}

/// Per-agent EpisodicDistiller configuration from the `.agent` manifest
/// `[memory.distiller]` section (ADR-068 M4/M7).
///
/// Every field is optional. The distiller is **off by default** (opt-in,
/// ADR-068 review revision): an absent section, an empty section, or a
/// section without an explicit `enabled` field all keep the distiller
/// disabled. Set `enabled = true` to turn it on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestDistillerConfig {
    /// Master switch for the EpisodicDistiller step (ADR-068 M7).
    /// Default: false (OFF — opt-in; set `true` to enable).
    pub enabled: bool,
    /// Distiller model reference (ADR-071 D4/D5): package-author initial
    /// value for `agent_config.json` `distiller_model`. `None` falls through
    /// to the global `default_compact_model` and finally the first provider
    /// model. Stored as a (provider_id, model_id) pair mirroring
    /// `protocol::CompactModelRef`.
    pub model_provider_id: Option<String>,
    pub model_id: Option<String>,
    /// Distiller periodic trigger interval in minutes (ADR-071 D1/D4).
    /// Default: 60.
    pub interval_minutes: Option<u64>,
    /// Unconsolidated-episode backlog threshold (ADR-071 D1/D4). Default: 50.
    pub accumulation_threshold: Option<usize>,
    /// Idle threshold in minutes (ADR-071 D1/D4). Default: 30.
    pub idle_minutes: Option<u64>,
    /// Max episodes scanned per distillation run (DistillerConfig.batch_size).
    pub batch_size: Option<usize>,
    /// Floor below which an episode is not consolidated at all
    /// (DistillerConfig.min_importance). The single remaining volume knob:
    /// the per-subtype evidence gates it replaces were multiplying together
    /// into a distiller that silently produced nothing. Default: 0.0.
    pub min_importance: Option<f32>,
}

/// Optional memory-quality overrides from the `.agent` manifest
/// `[memory.quality]` section (ADR-062 D2).
///
/// Every field is optional: a present field overrides the matching default in
/// `acowork_memory::quality::MemoryQualityConfig`; an absent field keeps the
/// default ("zero-config = current behaviour"). This type lives in
/// `acowork-core` (which cannot depend on `acowork-memory`) and is converted to
/// the memory-layer config at the runtime boundary (see
/// `From<ManifestMemoryQuality> for MemoryQualityConfig`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManifestMemoryQuality {
    // ADR-101 decision 5: `auto_inject_enabled` is gone along with the
    // auto-injection path it gated — recall happens through the memory tools
    // the agent calls.
    /// Exclude Dormant nodes from retrieval results (default: true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_dormant: Option<bool>,
    /// Minimum cosine similarity for retrieval results (default: 0.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cosine: Option<f32>,
    /// Dedup similarity thresholds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup: Option<ManifestDedupQuality>,
    /// Consolidation confidence gates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consolidation: Option<ManifestConsolidationQuality>,
    /// Enable keyword participation in retrieval (default: false; benchmark
    /// gated per ADR-062 D3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyword_index: Option<bool>,
}

/// Manifest mirror of `acowork_memory::quality::DedupQuality`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManifestDedupQuality {
    /// Knowledge dedup cosine-similarity threshold (default: 0.95).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_threshold: Option<f32>,
    /// Procedural dedup cosine-similarity threshold (default: 0.90).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub procedure_threshold: Option<f32>,
}

/// Manifest mirror of `acowork_memory::quality::ConsolidationQuality`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ManifestConsolidationQuality {
    /// Confidence above which instant extraction creates a node as Active
    /// (default: 0.85).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct_active_threshold: Option<f32>,
    /// Confidence above which offline consolidation upgrades Pending → Active
    /// (default: 0.7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_upgrade_threshold: Option<f32>,
    /// Confidence above which experience generalization upgrades Pending →
    /// Active (ProceduralNode path, default: 0.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generalization_active_threshold: Option<f32>,
    /// Confidence below which offline consolidation marks a node Dormant
    /// (default: 0.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dormant_confidence: Option<f32>,
    /// Minimum age (hours) before a Pending node is eligible for offline
    /// consolidation (default: 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_pending_age_hours: Option<u64>,
}

/// Tool declaration in manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDeclaration {
    /// Tool type (default: "builtin"). Use "rag" for RAG tools.
    #[serde(rename = "type", default = "default_tool_type")]
    pub tool_type: String,
    /// Tool name (must match a registered tool)
    pub name: String,
    /// Optional tool-specific configuration
    #[serde(default)]
    pub config: Option<serde_json::Value>,
    /// RAG configuration (only when tool_type = "rag")
    #[serde(default)]
    pub rag: Option<RagToolConfig>,
}

fn default_tool_type() -> String {
    "builtin".to_string()
}

impl ToolDeclaration {
    /// Check if this is a RAG tool declaration
    pub fn is_rag(&self) -> bool {
        self.tool_type == "rag"
    }
}

/// RAG tool configuration (manifest `[[tools]]` with type = "rag")
///
/// Declares an enterprise RAG knowledge base endpoint for the agent.
/// RAG is an opt-in capability — agents without this declaration behave
/// exactly as in Phase 3, zero intrusion.
///
/// Example manifest:
/// ```toml
/// [[tools]]
/// type = "rag"
/// name = "enterprise_knowledge"
///
/// [tools.rag]
/// endpoint = "https://rag.corp.example.com/v1/query"
/// collection = "product_docs"
/// auth_ref = "vault:rag_enterprise_key"
/// auth_type = "bearer"
/// max_results = 5
/// score_threshold = 0.7
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagToolConfig {
    /// RAG service endpoint URL (required)
    pub endpoint: String,
    /// Collection / index name in the RAG service (optional)
    #[serde(default)]
    pub collection: Option<String>,
    /// Vault reference for authentication credential (e.g., "vault:rag_enterprise_key")
    #[serde(default)]
    pub auth_ref: Option<String>,
    /// Authentication type: "bearer" or "api_key" (default: "bearer")
    #[serde(default = "default_auth_type")]
    pub auth_type: String,
    /// Maximum number of results per query (default: 5)
    #[serde(default = "default_max_results")]
    pub max_results: u32,
    /// Minimum score threshold for results (default: 0.7)
    #[serde(default = "default_score_threshold")]
    pub score_threshold: f32,
    /// Query timeout in seconds (default: 10)
    #[serde(default = "default_rag_timeout")]
    pub timeout_secs: u64,
}

fn default_auth_type() -> String {
    "bearer".to_string()
}

fn default_max_results() -> u32 {
    5
}

fn default_score_threshold() -> f32 {
    0.7
}

fn default_rag_timeout() -> u64 {
    10
}

/// Capability definition for Intent routing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityDef {
    /// Human-readable description of this capability
    pub description: String,
    /// Skills that implement this capability (`skills = ["task-decomposition"]`).
    ///
    /// Documentation + validation link only: the Runtime does not load a
    /// skill because it is listed here. Skills stay disk-discovered (they
    /// can be installed at runtime), so the manifest must not become a
    /// second source of truth for them — [`AgentManifest::validate_graph`]
    /// checks that the reference resolves instead.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// JSON Schema for capability input
    #[serde(default)]
    pub input_schema: Option<serde_json::Value>,
    /// JSON Schema for capability output
    #[serde(default)]
    pub output_schema: Option<serde_json::Value>,
}

/// Resource limits for Agent process
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ResourceLimits {
    /// Maximum memory usage in MB
    #[serde(default)]
    pub max_memory_mb: Option<u64>,
    /// Maximum CPU percentage (0.0 - 100.0)
    #[serde(default)]
    pub max_cpu_percent: Option<f64>,
    /// Idle timeout in seconds before auto-stop
    #[serde(default)]
    pub idle_timeout_secs: Option<u64>,
}

/// Sandbox configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SandboxConfig {
    /// Whether sandboxing is enabled
    #[serde(default)]
    pub enabled: bool,
    /// Working directory for the agent
    #[serde(default)]
    pub work_dir: Option<String>,
    /// Paths the agent is allowed to access
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

/// Trigger definition (cron, event-based, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    /// Trigger type (e.g., "cron", "event", "manual")
    #[serde(rename = "type")]
    pub trigger_type: String,
    /// Cron schedule expression (for cron triggers)
    #[serde(default)]
    pub schedule: Option<String>,
    /// Action to fire when the trigger activates (for cron/event triggers)
    #[serde(default)]
    pub action: Option<String>,
    /// Params to include when the trigger fires (JSON, for cron/event triggers)
    #[serde(default)]
    pub params: Option<serde_json::Value>,
    /// Event name (for event triggers)
    #[serde(default)]
    pub event: Option<String>,
}

/// Manifest parsing/validation errors
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),
    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),
    #[error("Validation error: {0}")]
    Validation(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ADR-104: ManifestDraft::build ────────────────────────────────

    fn draft() -> ManifestDraft {
        ManifestDraft {
            agent_id: "com.acme.researcher".to_string(),
            name: "Researcher".to_string(),
            ..Default::default()
        }
    }

    fn skill(name: &str, deps: &[&str]) -> SkillDecl {
        SkillDecl {
            name: name.to_string(),
            tool_deps: deps.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn draft_build_fills_only_the_required_defaults() {
        let m = draft().build(&[]).expect("minimal draft builds");
        assert_eq!(m.version, "0.1.0");
        assert_eq!(m.author, "ACowork User");
        assert_eq!(m.runtime_version, "0.1.0");
        assert_eq!(m.description, "Researcher agent");
        // Absent sections stay empty rather than being invented.
        assert!(m.labels.is_empty());
        assert!(m.mcp_servers.is_empty());
        assert_eq!(m.config, ManifestConfigDefaults::default());
        assert!(m.tools.is_empty());
        assert!(m.capabilities.is_empty());
    }

    #[test]
    fn draft_build_rejects_missing_identity() {
        let mut d = draft();
        d.agent_id = "  ".to_string();
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("agent_id"), "got: {err}");

        let mut d = draft();
        d.name = String::new();
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("name"), "got: {err}");
    }

    #[test]
    fn draft_build_rejects_a_non_reverse_domain_agent_id() {
        let mut d = draft();
        d.agent_id = "researcher".to_string();
        let err = d.build(&[]).unwrap_err();
        assert!(err.to_lowercase().contains("agent_id"), "got: {err}");
    }

    #[test]
    fn draft_build_carries_every_v2_section() {
        let mut d = draft();
        d.display_name = Some("Deep Researcher".to_string());
        d.role = Some("researcher".to_string());
        d.labels = vec!["research".to_string(), "web".to_string()];
        d.config = Some(ManifestConfigDefaults {
            temperature: Some(0.3),
            context_window: Some(200_000),
            ..Default::default()
        });
        d.tools = vec!["file_read".to_string()];
        d.mcp_servers = vec![McpServerDecl {
            preset: Some("brave-search".to_string()),
            why: Some("needs the web".to_string()),
            ..Default::default()
        }];
        d.capabilities = vec![DraftCapability {
            action: "decompose".to_string(),
            description: "Break a task down".to_string(),
            skills: vec!["task-decomposition".to_string()],
            ..Default::default()
        }];

        let m = d
            .build(&[skill("task-decomposition", &[])])
            .expect("full draft builds");
        assert_eq!(m.labels, vec!["research".to_string(), "web".to_string()]);
        assert_eq!(m.config.temperature, Some(0.3));
        assert_eq!(m.config.context_window, Some(200_000));
        assert_eq!(m.tools.len(), 1);
        assert_eq!(m.mcp_servers.len(), 1);
        assert_eq!(
            m.capabilities["decompose"].skills,
            vec!["task-decomposition".to_string()]
        );
        // Round-trips through TOML — the wizard's preview is this text.
        let toml = m.to_toml().unwrap();
        assert!(toml.contains("[config]"));
        assert!(toml.contains("labels = ["));
        assert!(toml.contains("[[mcp_servers]]"));
        let back = AgentManifest::from_toml(&toml).unwrap();
        assert_eq!(back.config, m.config);
        assert_eq!(back.mcp_servers.len(), 1);
    }

    #[test]
    fn draft_build_refuses_a_capability_linking_a_skill_it_does_not_ship() {
        let mut d = draft();
        d.capabilities = vec![DraftCapability {
            action: "decompose".to_string(),
            description: "Break a task down".to_string(),
            skills: vec!["missing-skill".to_string()],
            ..Default::default()
        }];
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("missing-skill"), "got: {err}");

        // And it builds once the skill really is shipped.
        assert!(d.build(&[skill("missing-skill", &[])]).is_ok());
    }

    #[test]
    fn draft_build_refuses_duplicate_capability_actions() {
        let mut d = draft();
        d.capabilities = vec![
            DraftCapability {
                action: "decompose".to_string(),
                description: "one".to_string(),
                ..Default::default()
            },
            DraftCapability {
                action: "decompose".to_string(),
                description: "two".to_string(),
                ..Default::default()
            },
        ];
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("duplicate capability"), "got: {err}");
    }

    #[test]
    fn draft_build_collects_the_draft_level_defects() {
        // `AgentManifest::validate` reports the first defect it finds, so the
        // guarantee here is narrower than "everything at once": the draft's
        // own checks accumulate, then `validate` contributes one, then the
        // graph check. This test pins that the draft-level ones really do
        // accumulate instead of the first one winning.
        let mut d = draft();
        d.capabilities = vec![
            DraftCapability {
                action: String::new(),
                ..Default::default()
            },
            DraftCapability {
                action: "decompose".to_string(),
                description: "Break a task down".to_string(),
                skills: vec!["missing-skill".to_string()],
                ..Default::default()
            },
        ];
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("empty action name"), "got: {err}");
        assert!(err.contains("missing-skill"), "got: {err}");
    }

    #[test]
    fn draft_build_reports_a_bad_label() {
        let mut d = draft();
        d.labels = vec!["Not A Label".to_string()];
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("label"), "got: {err}");
    }

    #[test]
    fn draft_build_rejects_an_inline_stdio_declaration() {
        let mut d = draft();
        d.mcp_servers = vec![McpServerDecl {
            name: Some("local-shell".to_string()),
            transport: Some(crate::protocol::McpTransportDef::Stdio),
            url: Some("http://x".to_string()),
            ..Default::default()
        }];
        let err = d.build(&[]).unwrap_err();
        assert!(err.contains("stdio"), "got: {err}");
    }

    #[test]
    fn draft_build_preview_and_create_are_the_same_bytes() {
        // "Preview == create" is a property of sharing one builder; this pins
        // it so a future refactor cannot reintroduce a second renderer.
        let mut d = draft();
        d.labels = vec!["research".to_string()];
        d.capabilities = vec![DraftCapability {
            action: "decompose".to_string(),
            description: "Break a task down".to_string(),
            skills: vec!["decompose-skill".to_string()],
            ..Default::default()
        }];
        let skills = [skill("decompose-skill", &[])];
        let a = d.build(&skills).unwrap().to_toml().unwrap();
        let b = d.build(&skills).unwrap().to_toml().unwrap();
        assert_eq!(a, b);
    }

    fn sample_manifest_toml() -> &'static str {
        r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "A weather query agent"
            author = "acowork"
            runtime_version = "0.1.0"

            [llm]
            max_tokens = 4096

            [memory]
            enabled = true
            retention_days = 90

            [resources]
            max_memory_mb = 512
            idle_timeout_secs = 1800

            [sandbox]
            enabled = true
            work_dir = "/tmp/agent-workdir"

            [[permissions]]
            type = "Network"
            value = "wttr.in"

            [[permissions]]
            type = "IdentityRead"

            [[tools]]
            name = "weather"

            [[tools]]
            name = "file_read"

            [[triggers]]
            type = "manual"
        "#
    }

    #[test]
    fn test_manifest_parse_minimal() {
        let toml_str = r#"
            agent_id = "com.example.weather"
            version = "1.0.0"
            name = "Weather Agent"
            description = "A weather query agent"
            author = "test"
            runtime_version = "0.1.0"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert_eq!(manifest.agent_id, "com.example.weather");
        // [llm] section is optional — provider/model now come from resource_cache
        assert!(manifest.permissions.is_empty());
        assert!(manifest.tools.is_empty());
        assert!(manifest.memory.enabled); // default true
    }

    #[test]
    fn test_manifest_parse_full() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        assert_eq!(manifest.agent_id, "com.example.weather");
        assert_eq!(manifest.version, "1.0.0");
        assert_eq!(manifest.llm.max_tokens, Some(4096));
        assert!(manifest.memory.enabled);
        assert_eq!(manifest.memory.retention_days, Some(90));
        assert_eq!(manifest.resources.max_memory_mb, Some(512));
        assert!(manifest.sandbox.enabled);
    }

    #[test]
    fn test_manifest_roundtrip() {
        let original = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        let toml_str = original.to_toml().unwrap();
        let parsed = AgentManifest::from_toml(&toml_str).unwrap();
        assert_eq!(original.agent_id, parsed.agent_id);
        assert_eq!(original.version, parsed.version);
    }

    #[test]
    fn test_manifest_validation_empty_agent_id() {
        let toml_str = r#"
            agent_id = ""
            version = "1.0.0"
            name = "Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let result = AgentManifest::from_toml(toml_str);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("agent_id"),
            "Expected agent_id error, got: {err}"
        );
    }

    #[test]
    fn test_manifest_validation_no_dots_in_id() {
        let toml_str = r#"
            agent_id = "weather"
            version = "1.0.0"
            name = "Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let result = AgentManifest::from_toml(toml_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_manifest_has_tool() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        assert!(manifest.has_tool("weather"));
        assert!(manifest.has_tool("file_read"));
        // ADR-101: memory tools left the builtin registry, so a package no longer
        // declares them here.
        assert!(!manifest.has_tool("memory_store"));
        assert!(!manifest.has_tool("shell"));
    }

    #[test]
    fn test_manifest_get_tool() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        let tool = manifest.get_tool("weather").unwrap();
        assert_eq!(tool.name, "weather");
        assert!(manifest.get_tool("nonexistent").is_none());
    }

    #[test]
    fn test_manifest_rag_tool_declaration() {
        let toml_str = r#"
            agent_id = "com.example.sales"
            version = "1.0.0"
            name = "Sales Assistant"
            description = "Enterprise sales agent with RAG"
            author = "corp"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Network"
            value = "https://rag.corp.example.com"

            [[tools]]
            type = "rag"
            name = "enterprise_knowledge"

            [tools.rag]
            endpoint = "https://rag.corp.example.com/v1/query"
            collection = "product_docs"
            auth_ref = "vault:rag_enterprise_key"
            auth_type = "bearer"
            max_results = 5
            score_threshold = 0.7
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert!(manifest.has_rag());
        let (tool_name, rag_config) = manifest.rag_config().unwrap();
        assert_eq!(tool_name, "enterprise_knowledge");
        assert_eq!(rag_config.endpoint, "https://rag.corp.example.com/v1/query");
        assert_eq!(rag_config.collection.as_deref(), Some("product_docs"));
        assert_eq!(
            rag_config.auth_ref.as_deref(),
            Some("vault:rag_enterprise_key")
        );
        assert_eq!(rag_config.auth_type, "bearer");
        assert_eq!(rag_config.max_results, 5);
        assert!((rag_config.score_threshold - 0.7).abs() < 0.001);
    }

    #[test]
    fn test_manifest_no_rag_by_default() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        assert!(!manifest.has_rag());
        assert!(manifest.rag_config().is_none());
    }

    #[test]
    fn test_manifest_tool_declaration_default_type() {
        let manifest = AgentManifest::from_toml(sample_manifest_toml()).unwrap();
        let tool = manifest.get_tool("weather").unwrap();
        assert_eq!(tool.tool_type, "builtin");
        assert!(!tool.is_rag());
    }

    #[test]
    fn test_manifest_builtin_avatar_optional() {
        // Backward-compat: legacy packages without builtin_avatar still parse.
        let toml_str = r#"
            agent_id = "com.example.legacy"
            version = "1.0.0"
            name = "Legacy"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert!(manifest.builtin_avatar.is_none());
        assert!(manifest.builtin_avatar().is_none());
    }

    #[test]
    fn test_manifest_builtin_avatar_field() {
        // New manifest can carry a builtin_avatar field. The exact form
        // ("icon-05" vs "5") is the client's concern; the schema just stores
        // the raw string.
        let toml_str = r#"
            agent_id = "com.example.branded"
            version = "1.0.0"
            name = "Branded"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            builtin_avatar = "icon-05"
        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        assert_eq!(manifest.builtin_avatar.as_deref(), Some("icon-05"));
        assert_eq!(manifest.builtin_avatar(), Some("icon-05"));
    }

    // ── ADR-068 M7 distiller switch: off by default (opt-in) ──────────────
    // The four manifest shapes must resolve through the SAME single source of
    // truth (`MemoryConfig::distiller_enabled`). Regression test for the
    // three-state split that existed when `#[serde(default)]` (empty section
    // → `ManifestDistillerConfig::default()`) disagreed with the
    // section-absent path (`Option::None` → `unwrap_or(false)` in the
    // runtime). All four shapes now resolve to OFF unless `enabled = true`.

    fn distiller_manifest(memory_section: &str) -> AgentManifest {
        let toml_str = format!(
            r#"
                agent_id = "com.example.distiller"
                version = "1.0.0"
                name = "Distiller"
                description = "Test"
                author = "test"
                runtime_version = "0.1.0"

                [memory]
                enabled = true
                {memory_section}
            "#
        );
        AgentManifest::from_toml(&toml_str).unwrap()
    }

    #[test]
    fn test_distiller_absent_section_is_off() {
        let manifest = distiller_manifest("");
        assert!(manifest.memory.distiller.is_none());
        assert!(!manifest.memory.distiller_enabled());
    }

    #[test]
    fn test_distiller_empty_section_is_off() {
        // Empty `[memory.distiller]` section: serde fills missing fields from
        // `ManifestDistillerConfig::default()` where `enabled` is now false.
        let manifest = distiller_manifest("[memory.distiller]");
        assert!(manifest.memory.distiller.is_some());
        assert!(!manifest.memory.distiller_enabled());
    }

    #[test]
    fn test_distiller_section_without_enabled_field_is_off() {
        // Section present but `enabled` omitted — same resolution as empty.
        let manifest = distiller_manifest("[memory.distiller]\nbatch_size = 50");
        assert!(manifest.memory.distiller.is_some());
        assert!(!manifest.memory.distiller_enabled());
    }

    #[test]
    fn test_distiller_explicit_false_is_off() {
        let manifest = distiller_manifest("[memory.distiller]\nenabled = false");
        assert!(!manifest.memory.distiller_enabled());
    }

    #[test]
    fn test_distiller_explicit_true_is_on() {
        let manifest = distiller_manifest("[memory.distiller]\nenabled = true");
        assert!(manifest.memory.distiller_enabled());
    }

    // ── ADR-104: [config] / labels / [[mcp_servers]] / capability skills ──

    /// Minimal valid manifest with an extra section appended.
    fn decl_manifest(extra: &str) -> AgentManifest {
        let toml_str = format!(
            r#"
agent_id = "com.example.decl"
version = "1.0.0"
name = "Decl"
description = "declaration test"
author = "test"
runtime_version = "0.1.0"
{extra}
"#
        );
        AgentManifest::from_toml(&toml_str).expect("manifest should parse")
    }

    fn decl_manifest_err(extra: &str) -> String {
        let toml_str = format!(
            r#"
agent_id = "com.example.decl"
version = "1.0.0"
name = "Decl"
description = "declaration test"
author = "test"
runtime_version = "0.1.0"
{extra}
"#
        );
        AgentManifest::from_toml(&toml_str)
            .expect_err("manifest should be rejected")
            .to_string()
    }

    #[test]
    fn old_manifest_without_new_sections_still_parses() {
        let m = decl_manifest("");
        assert!(m.config.is_empty());
        assert!(m.labels.is_empty());
        assert!(m.mcp_servers.is_empty());
        assert!(m.config_defaults().is_empty());
    }

    #[test]
    fn config_section_maps_onto_defaults() {
        let m = decl_manifest(
            r#"[config]
temperature = 0.7
max_output_tokens = 32000
max_iterations = 120
context_window = 128000
compression_ratio_threshold = 0.8"#,
        );
        let d = m.config_defaults();
        assert_eq!(d.temperature, Some(0.7));
        assert_eq!(d.max_output_tokens, Some(32000));
        assert_eq!(d.max_iterations, Some(120));
        assert_eq!(d.context_window, Some(128000));
        assert_eq!(d.compression_ratio_threshold, Some(0.8));
    }

    #[test]
    fn config_wins_over_deprecated_llm_fields() {
        let m = decl_manifest(
            r#"[llm]
temperature = 0.1
context_window = 64000
max_tokens = 4096

[config]
temperature = 0.9"#,
        );
        let d = m.config_defaults();
        assert_eq!(d.temperature, Some(0.9), "[config] must win");
        assert_eq!(d.context_window, Some(64000), "legacy [llm] still read");
        assert_eq!(
            d.max_output_tokens,
            Some(4096),
            "[llm].max_tokens maps onto the output cap"
        );
    }

    #[test]
    fn config_out_of_range_is_rejected() {
        assert!(decl_manifest_err("[config]\ntemperature = 3.0").contains("temperature"));
        assert!(decl_manifest_err("[config]\ncontext_window = 0").contains("context_window"));
        assert!(decl_manifest_err("[config]\nmax_output_tokens = 0").contains("max_output_tokens"));
        assert!(decl_manifest_err("[config]\nmax_iterations = 0").contains("max_iterations"));
        assert!(
            decl_manifest_err("[config]\ncompression_ratio_threshold = 0.99")
                .contains("compression_ratio_threshold")
        );
    }

    #[test]
    fn labels_accept_kebab_slugs() {
        let m = decl_manifest(r#"labels = ["project-management", "agile", "v2"]"#);
        assert_eq!(m.labels.len(), 3);
    }

    #[test]
    fn labels_reject_bad_shapes() {
        assert!(decl_manifest_err(r#"labels = ["Has-Upper"]"#).contains("labels"));
        assert!(decl_manifest_err(r#"labels = ["under_score"]"#).contains("labels"));
        assert!(decl_manifest_err(r#"labels = ["x"]"#).contains("labels"));
        assert!(decl_manifest_err(r#"labels = ["-lead"]"#).contains("labels"));
        assert!(decl_manifest_err(r#"labels = ["agile", "agile"]"#).contains("duplicate"));
    }

    #[test]
    fn labels_reject_too_many() {
        let many: Vec<String> = (0..MAX_MANIFEST_LABELS + 1)
            .map(|i| format!("\"tag-{i}\""))
            .collect();
        let extra = format!("labels = [{}]", many.join(", "));
        assert!(decl_manifest_err(&extra).contains("at most"));
    }

    #[test]
    fn mcp_server_forms_are_mutually_exclusive() {
        assert!(decl_manifest_err(
            r#"[[mcp_servers]]
preset = "brave-search"
hosted = "pm""#
        )
        .contains("cannot be combined"));

        assert!(decl_manifest_err(
            r#"[[mcp_servers]]
why = "nothing declared""#
        )
        .contains("exactly one of"));
    }

    #[test]
    fn mcp_server_inline_stdio_is_rejected() {
        let err = decl_manifest_err(
            r#"[[mcp_servers]]
name = "evil"
transport = "stdio"
url = "http://localhost:8080/mcp""#,
        );
        assert!(err.contains("inline stdio is not allowed"), "got: {err}");
    }

    #[test]
    fn mcp_server_inline_http_needs_network_permission() {
        let err = decl_manifest_err(
            r#"[[mcp_servers]]
name = "corp-kb"
transport = "http"
url = "https://mcp.corp.example.com/mcp""#,
        );
        assert!(err.contains("Network"), "got: {err}");

        let m = decl_manifest(
            r#"[[permissions]]
type = "Network"
value = "https://mcp.corp.example.com"

[[mcp_servers]]
name = "corp-kb"
transport = "http"
url = "https://mcp.corp.example.com/mcp"
required_secrets = ["CORP_MCP_TOKEN"]
tools = ["kb_search"]
required = true"#,
        );
        assert_eq!(m.mcp_servers.len(), 1);
        let decl = &m.mcp_servers[0];
        assert_eq!(decl.source_kind(), Some(McpSourceKind::InlineRemote));
        assert_eq!(decl.key(), Some("corp-kb"));
        assert!(decl.required);
    }

    #[test]
    fn mcp_server_key_defaults_to_preset_or_hosted_id() {
        let m = decl_manifest(
            r#"[[mcp_servers]]
preset = "brave-search"
tools = ["brave_web_search"]

[[mcp_servers]]
hosted = "pm"
tools = ["pm_claim_task"]"#,
        );
        assert_eq!(m.mcp_server_keys(), vec!["brave-search", "pm"]);
        assert_eq!(
            m.recommended_mcp_keys(),
            vec!["brave-search", "pm"],
            "preset and hosted resolve without package-side details"
        );
    }

    #[test]
    fn mcp_server_duplicate_keys_are_rejected() {
        let err = decl_manifest_err(
            r#"[[mcp_servers]]
preset = "brave-search"

[[mcp_servers]]
preset = "brave-search""#,
        );
        assert!(err.contains("duplicate server"), "got: {err}");
    }

    #[test]
    fn capability_declares_skills_link() {
        let m = decl_manifest(
            r#"[capabilities.task_decomposition]
description = "Break goals into tasks"
skills = ["task-decomposition", "sprint-planning"]"#,
        );
        let def = m.capabilities.get("task_decomposition").expect("capability");
        assert_eq!(def.skills, vec!["task-decomposition", "sprint-planning"]);
    }

    #[test]
    fn capability_without_description_is_rejected() {
        let err = decl_manifest_err(
            r#"[capabilities.x]
skills = ["y"]"#,
        );
        assert!(err.contains("description"), "got: {err}");
    }

    #[test]
    fn graph_accepts_a_consistent_declaration() {
        let m = decl_manifest(
            r#"labels = ["pm"]

[[tools]]
name = "file_edit"

[[mcp_servers]]
preset = "brave-search"
tools = ["brave_web_search"]

[capabilities.research]
description = "Search the web"
skills = ["web-research"]"#,
        );
        let skills = vec![SkillDecl {
            name: "web-research".into(),
            tool_deps: vec![
                "brave_web_search".into(),
                "mcp_memory__memory_recall".into(),
            ],
        }];
        let report = m.validate_graph(&skills);
        assert!(report.is_clean(), "unexpected: {report:?}");
    }

    #[test]
    fn graph_flags_dangling_capability_skill() {
        let m = decl_manifest(
            r#"[capabilities.research]
description = "Search the web"
skills = ["does-not-exist"]"#,
        );
        let report = m.validate_graph(&[]);
        assert!(!report.is_ok());
        assert!(report.errors[0].contains("does-not-exist"));
    }

    #[test]
    fn graph_warns_on_undeclared_tool_dep() {
        let m = decl_manifest(
            r#"[[tools]]
name = "file_read""#,
        );
        let skills = vec![SkillDecl {
            name: "web-research".into(),
            tool_deps: vec!["file_read".into(), "brave_web_search".into()],
        }];
        let report = m.validate_graph(&skills);
        assert!(report.is_ok(), "warnings must not fail the package");
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("brave_web_search"));
    }

    #[test]
    fn graph_treats_platform_hosted_mcp_tools_as_available() {
        let m = decl_manifest("");
        let skills = vec![SkillDecl {
            name: "memory-user".into(),
            tool_deps: vec![
                "mcp_memory__memory_store".into(),
                "mcp_pm__pm_submit_task".into(),
            ],
        }];
        assert!(m.validate_graph(&skills).is_clean());
    }

    #[test]
    fn config_section_round_trips_through_toml() {
        let m = decl_manifest(
            r#"labels = ["agile"]

[config]
temperature = 0.4

[[mcp_servers]]
hosted = "pm"
tools = ["pm_claim_task"]"#,
        );
        let text = m.to_toml().expect("serialize");
        let back = AgentManifest::from_toml(&text).expect("re-parse");
        assert_eq!(back.labels, m.labels);
        assert_eq!(back.config, m.config);
        assert_eq!(back.mcp_servers, m.mcp_servers);
    }
}
