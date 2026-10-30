//! MCP (Model Context Protocol) manager — connection lifecycle and tool injection.
//!
//! Manages MCP server connections and provides [`McpToolWrapper`] instances
//! that implement the built-in [`Tool`](acowork_core::tools::traits::Tool) trait,
//! enabling MCP tools to be dispatched transparently alongside native ACowork tools.

use std::path::Path;
use std::sync::Arc;

use acowork_core::protocol::McpServerConfigDef;
use acowork_core::tools::traits::Tool;
use acowork_mcp::client::McpRegistry;
use acowork_mcp::wrapper::McpToolWrapper;
use indexmap::IndexMap;

use crate::agent_config::{AgentMcpToolsConfig, McpToolDescriptor, tool_enabled_in};

/// Re-export from acowork-mcp so SessionManager can reference it.
pub use acowork_mcp::client::McpConnectionFailure;

/// Result of an asynchronous MCP server connection attempt.
///
/// Produced by a background task and applied to SessionManager
/// via [`SessionManager::apply_mcp_connection_result`].
pub type McpConnectResult = (
    Arc<McpRegistry>,
    Vec<McpToolWrapper>,
    Vec<(String, serde_json::Value)>,
    Vec<McpConnectionFailure>,
);

/// MCP connection manager.
///
/// Holds a shared [`McpRegistry`] handed to it via [`Self::set_registry`]
/// after a background connect through
/// [`connect_mcp_with_reconcile_and_filter`].
pub struct McpManager {
    registry: Option<Arc<McpRegistry>>,
}

impl McpManager {
    /// Create an empty MCP manager (no servers connected).
    pub fn new() -> Self {
        Self { registry: None }
    }

    /// Get the current MCP registry, if any servers are connected.
    pub fn registry(&self) -> Option<&Arc<McpRegistry>> {
        self.registry.as_ref()
    }

    /// Check whether any MCP servers are connected.
    pub fn is_connected(&self) -> bool {
        self.registry.as_ref().is_some_and(|r| !r.is_empty())
    }

    /// Set the registry directly (used when MCP connection results are
    /// produced by a background task and applied asynchronously).
    pub fn set_registry(&mut self, registry: Arc<McpRegistry>) {
        self.registry = Some(registry);
    }

    /// Disconnect from all MCP servers and release resources.
    ///
    /// Closes transport connections (kills stdio child processes, releases
    /// HTTP connection pools). After calling disconnect, the manager is
    /// reset to the empty state. Call
    /// [`connect_mcp_with_reconcile_and_filter`] and [`Self::set_registry`]
    /// again before using MCP tools.
    pub async fn disconnect(&mut self) {
        if let Some(registry) = self.registry.take() {
            registry.disconnect().await;
            tracing::info!("MCP manager: disconnected from all servers");
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::McpTransportDef;

    #[test]
    fn mcp_manager_default_is_not_connected() {
        let mgr = McpManager::default();
        assert!(!mgr.is_connected());
        assert!(mgr.registry().is_none());
    }

    #[test]
    fn config_def_is_shared_type() {
        // McpServerConfigDef is now used directly by acowork-mcp,
        // no separate conversion step needed.
        let def = McpServerConfigDef {
            name: "test-server".to_string(),
            display_name: None,
            transport: McpTransportDef::Stdio,
            url: None,
            command: "test-cmd".to_string(),
            args: vec!["--verbose".to_string()],
            env: Default::default(),
            headers: Default::default(),
            tool_timeout_secs: Some(30),
            install: None,
            required_secrets: vec![],
            origin: None,
        };
        assert_eq!(def.name, "test-server");
        assert_eq!(def.command, "test-cmd");
        assert_eq!(def.args, vec!["--verbose"]);
        assert_eq!(def.tool_timeout_secs, Some(30));
        assert!(matches!(def.transport, McpTransportDef::Stdio));
        assert!(def.url.is_none());
    }
}

/// Parse a prefixed MCP tool name into `(server_name, tool_name)`.
///
/// Format produced by `McpRegistry::connect_all`:
///   `mcp_<server_name>__<tool_name>`
fn split_prefixed_tool(prefixed: &str) -> Option<(&str, &str)> {
    let stripped = prefixed.strip_prefix("mcp_")?;
    stripped.split_once("__")
}

/// Decide whether a single `(server, tool)` pair should be exposed to
/// the LLM. ADR-069 flat-per-tool semantics:
///
/// 1. Server absent from `tools_cfg` entirely → permissive
///    "expose everything". Conservative behaviour for brand-new
///    servers the user hasn't yet configured; the next reconcile
///    (after the first successful `connect_all`) materialises the
///    flat list into the file via `merge_mcp_tools_config`.
/// 2. Server present, tool row missing from the per-server flat list
///    → conservative "expose nothing".
/// 3. Server present, tool row present → use `row.enabled` directly.
fn tool_allowed(tools_cfg: &AgentMcpToolsConfig, server_name: &str, tool_name: &str) -> bool {
    match tools_cfg.servers.get(server_name) {
        None => true,
        Some(rows) => tool_enabled_in(rows, tool_name).unwrap_or(false),
    }
}

/// Extract the live `tools/list` for every connected MCP server.
///
/// ADR-069 follow-up: returns `IndexMap`, not `HashMap`. `merge_mcp_tools_config`
/// iterates this map and writes the result to `agent_mcp_tools.json`;
/// `HashMap` would reshuffle the per-tool order on every reconnect
/// (which `PUT /mcp-tools` triggers), making the Tools-panel tool list
/// reshuffle whenever the user toggles a single tool.
pub fn collect_server_tools_from_registry(
    registry: &McpRegistry,
) -> IndexMap<String, Vec<McpToolDescriptor>> {
    let mut out: IndexMap<String, Vec<McpToolDescriptor>> = IndexMap::new();
    for prefixed in registry.tool_names() {
        let Some((server_name, tool_name)) = split_prefixed_tool(&prefixed) else {
            continue;
        };
        let Some(def) = registry.get_tool_def(&prefixed) else {
            continue;
        };
        out.entry(server_name.to_string())
            .or_default()
            .push(McpToolDescriptor {
                name: tool_name.to_string(),
                description: def.description,
            });
    }
    out
}

/// Load persisted config -> reconcile with the live registry -> write
/// back -> return the merged config used for the subsequent filter
/// pass.
pub fn reconcile_and_persist_mcp_tools(
    work_dir: &Path,
    registry: &McpRegistry,
) -> AgentMcpToolsConfig {
    let server_tools = collect_server_tools_from_registry(registry);
    let persisted = crate::agent_config::load_agent_mcp_tools_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "reconcile: failed to load persisted");
            None
        })
        .unwrap_or_default();
    // ADR-104: the default-enabled subset per server comes from the
    // package's `[[mcp_servers]].tools` whitelist, projected into
    // `agent_mcp.json`'s `recommended` list at startup.
    let recommended_tools = crate::agent_config::load_agent_mcp_config(work_dir)
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "reconcile: failed to load agent_mcp.json");
            None
        })
        .map(|cfg| cfg.recommended_tools_map())
        .unwrap_or_default();
    let merged =
        crate::agent_config::merge_mcp_tools_config(&persisted, &server_tools, &recommended_tools);
    if let Err(e) = crate::agent_config::save_agent_mcp_tools_config(work_dir, &merged) {
        tracing::warn!(
            error = %e,
            "reconcile: failed to persist merged config; using in-memory copy"
        );
    } else {
        tracing::info!(
            server_count = merged.servers.len(),
            tool_count = merged.servers.values().map(|v| v.len()).sum::<usize>(),
            "reconcile: persisted agent_mcp_tools.json"
        );
    }
    merged
}

/// Resolve the Gateway-published `{node_token}` placeholder into a real
/// credential, on the in-memory copy of the MCP config only.
///
/// ## Why this exists
///
/// Under `AUTH_MODE=multi_user` the Gateway's `auth_middleware` gates every
/// `/api/*` route, and the pm / doc MCP endpoints are no exception — they
/// are Gateway reverse-proxies like any other. A Runtime that connects
/// without a machine credential gets a 401 on `tools/list`, which the
/// failure path swallows: the tool list comes back empty,
/// `agent_mcp_tools.json` reconciles to zero rows, and the Desktop Tools
/// panel renders the server with **no expandable row and no error** — it
/// just looks switched off. So the credential is not optional here.
///
/// ## Why it is safe
///
/// Two boundaries keep the credential from leaking:
///
/// 1. The resolved value is a `String` in a `McpServerConfigDef` that
///    lives only for the duration of this call. It is deliberately
///    **never** written back to `agent_mcp.json`: that file is persisted
///    to the agent's workspace *and* surfaced in the Tools panel, so a
///    token landing there would be readable by anything that can read
///    the workspace. The placeholder itself is all that ever reaches
///    disk (the MQTT handler substitutes only `{instance_id}` when it
///    writes the catalog).
/// 2. Substitution is anchored to the **catalog section** of
///    `agent_mcp.json` — the part written solely by the MQTT handler
///    from the Gateway-published `acowork/global/mcps` — and requires
///    the entry to match a catalog entry on name AND url. A `local`
///    entry (Tools panel / `PUT /mcp-servers` / workspace edit) that
///    hand-types the placeholder must never be substituted: the node
///    credential is node-scoped and unlocks every Gateway `/api/*`
///    route, so a user-chosen URL carrying the template would
///    otherwise exfiltrate it (privilege escalation). The url match
///    also defeats a `local` entry shadowing a catalog name.
///
/// ## Why the template and not a direct value
///
/// The Gateway cannot substitute a real token: the credential is
/// node-scoped and minted at enrollment, so the Gateway has no
/// per-Runtime copy to send. It publishes the placeholder, and the
/// Runtime — which received the real value from the Node at spawn — fills
/// it in. This mirrors the existing `{instance_id}` convention.
fn resolve_node_token_template(
    work_dir: &Path,
    configs: &[McpServerConfigDef],
    node_token: Option<&str>,
) -> Vec<McpServerConfigDef> {
    let Some(token) = node_token.filter(|t| !t.is_empty()) else {
        // No credential (standalone Runtime, or a node that never
        // enrolled). Leave the placeholder in place: the request will 401
        // and the existing failure reporting surfaces it. Substituting an
        // empty string would be worse — it would look authenticated and
        // fail somewhere less legible.
        return configs.to_vec();
    };
    // Trust anchor: only entries the Gateway itself published into the
    // catalog section of agent_mcp.json may receive the credential. The
    // catalog is written solely by the MQTT handler from
    // `acowork/global/mcps`; `local` entries are user- or agent-authored
    // and must never be substituted even when they hand-type the
    // placeholder — the node token unlocks every Gateway `/api/*` route,
    // so leaking it to a user-chosen URL is privilege escalation.
    // Matching on name AND url also defeats a `local` entry shadowing a
    // catalog name with a different endpoint.
    let catalog: Vec<McpServerConfigDef> = crate::agent_config::load_agent_mcp_config(work_dir)
        .ok()
        .flatten()
        .map(|c| c.catalog)
        .unwrap_or_default();
    let is_gateway_published = |c: &McpServerConfigDef| {
        catalog.iter().any(|cat| {
            cat.name == c.name
                && cat.url == c.url
                && cat
                    .headers
                    .get(acowork_core::auth::NODE_TOKEN_HEADER)
                    .map(String::as_str)
                    == Some(acowork_core::auth::NODE_TOKEN_TEMPLATE)
        })
    };
    configs
        .iter()
        .map(|c| {
            let mut c = c.clone();
            if is_gateway_published(&c)
                && let Some(v) = c.headers.get_mut(acowork_core::auth::NODE_TOKEN_HEADER)
                && v == acowork_core::auth::NODE_TOKEN_TEMPLATE
            {
                *v = token.to_string();
            }
            c
        })
        .collect()
}

/// Connect + reconcile + filter, all in one pass.
///
/// `node_token` is the machine credential the Node injected at spawn
/// (`--mqtt-password`, which carries the same node_token value the Node
/// itself uses). It is substituted into the Gateway-published
/// `{node_token}` header placeholder so pm / doc MCP survive
/// `AUTH_MODE=multi_user`; see [`resolve_node_token_template`]. Pass
/// `None` in standalone mode, where no Gateway credential exists.
pub async fn connect_mcp_with_reconcile_and_filter(
    work_dir: &Path,
    configs: &[McpServerConfigDef],
    node_token: Option<&str>,
) -> McpConnectResult {
    let configs = resolve_node_token_template(work_dir, configs, node_token);
    let (registry, failures) = McpRegistry::connect_all(&configs)
        .await
        .expect("connect_all is non-fatal and should never fail");
    let registry = Arc::new(registry);
    let merged = reconcile_and_persist_mcp_tools(work_dir, &registry);

    let mut wrappers = Vec::new();
    let mut specs = Vec::new();
    let mut filtered_out: usize = 0;

    for prefixed_name in registry.tool_names() {
        let prefixed = prefixed_name.clone();
        let Some((server_name, tool_name)) = split_prefixed_tool(&prefixed) else {
            if let Some(def) = registry.get_tool_def(&prefixed) {
                let wrapper = McpToolWrapper::new(prefixed.clone(), def, registry.clone());
                let spec = wrapper.spec();
                let serialized = serde_json::to_value(&spec).unwrap_or_default();
                specs.push((spec.name.clone(), serialized));
                wrappers.push(wrapper);
            }
            continue;
        };

        if !tool_allowed(&merged, server_name, tool_name) {
            filtered_out += 1;
            continue;
        }

        if let Some(def) = registry.get_tool_def(&prefixed) {
            let wrapper = McpToolWrapper::new(prefixed.clone(), def, registry.clone());
            let spec = wrapper.spec();
            let serialized = serde_json::to_value(&spec).unwrap_or_default();
            specs.push((spec.name.clone(), serialized));
            wrappers.push(wrapper);
        }
    }

    tracing::info!(
        server_count = registry.server_count(),
        exposed_tool_count = wrappers.len(),
        filtered_out,
        failure_count = failures.len(),
        "MCP startup: connect_mcp_with_reconcile_and_filter applied ADR-069 reconcile+filter"
    );

    (registry, wrappers, specs, failures)
}

#[cfg(test)]
mod filter_tests {
    use super::*;
    use crate::agent_config::AgentMcpToolItem;
    use acowork_core::protocol::McpTransportDef;

    #[test]
    fn tool_allowed_server_absent_is_permissive() {
        let cfg = AgentMcpToolsConfig::default();
        assert!(tool_allowed(&cfg, "docling", "any_tool"));
    }

    #[test]
    fn tool_allowed_row_missing_is_conservative() {
        let mut cfg = AgentMcpToolsConfig::default();
        cfg.servers.insert(
            "pm".to_string(),
            vec![AgentMcpToolItem::new("pm_claim_task", true)],
        );
        assert!(tool_allowed(&cfg, "pm", "pm_claim_task"));
        assert!(!tool_allowed(&cfg, "pm", "pm_submit_task"));
    }

    #[test]
    fn tool_allowed_uses_row_enabled_directly() {
        let mut cfg = AgentMcpToolsConfig::default();
        cfg.servers.insert(
            "pm".to_string(),
            vec![
                AgentMcpToolItem::new("pm_claim_task", true),
                AgentMcpToolItem::new("pm_submit_task", false),
                AgentMcpToolItem::new("pm_list_projects", false),
            ],
        );
        assert!(tool_allowed(&cfg, "pm", "pm_claim_task"));
        assert!(!tool_allowed(&cfg, "pm", "pm_submit_task"));
        assert!(!tool_allowed(&cfg, "pm", "pm_list_projects"));
    }

    #[test]
    fn tool_allowed_user_disabled_wins_over_default_enabled() {
        let mut cfg = AgentMcpToolsConfig::default();
        cfg.servers.insert(
            "pm".to_string(),
            vec![
                AgentMcpToolItem::new("pm_claim_task", false),
                AgentMcpToolItem::new("pm_submit_task", true),
            ],
        );
        assert!(!tool_allowed(&cfg, "pm", "pm_claim_task"));
        assert!(tool_allowed(&cfg, "pm", "pm_submit_task"));
    }

    // ── ADR-076: `{node_token}` resolution ────────────────────────────

    /// Build a def shaped like the ones the Gateway injects for pm / doc.
    fn gateway_hosted_def(name: &str) -> McpServerConfigDef {
        let mut headers = std::collections::HashMap::new();
        headers.insert("X-MCP-Actor".to_string(), "{instance_id}".to_string());
        headers.insert(
            acowork_core::auth::NODE_TOKEN_HEADER.to_string(),
            acowork_core::auth::NODE_TOKEN_TEMPLATE.to_string(),
        );
        McpServerConfigDef {
            name: name.to_string(),
            display_name: None,
            transport: McpTransportDef::Http,
            url: Some("http://gw:19876/api/pm/mcp".to_string()),
            headers,
            ..Default::default()
        }
    }

    /// work_dir whose config/agent_mcp.json carries `catalog` — the shape
    /// the MQTT handler writes when it persists `acowork/global/mcps`.
    fn work_dir_with_catalog(catalog: Vec<McpServerConfigDef>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = crate::agent_config::AgentMcpConfig {
            catalog,
            local: vec![],
            active_names: None,
            recommended: Vec::new(),
            dismissed: Vec::new(),
        };
        crate::agent_config::save_agent_mcp_config(dir.path(), &cfg).expect("save catalog");
        dir
    }

    /// The happy path: pm / doc stop 401-ing at `auth_middleware` and the
    /// `tools/list` that drives the whole ADR-069 tool list works again.
    #[test]
    fn node_token_template_is_resolved_for_gateway_hosted_mcps() {
        let defs = vec![gateway_hosted_def("pm"), gateway_hosted_def("doc")];
        let dir = work_dir_with_catalog(defs.clone());
        let out = resolve_node_token_template(dir.path(), &defs, Some("tok-abc"));
        for s in &out {
            assert_eq!(
                s.headers
                    .get(acowork_core::auth::NODE_TOKEN_HEADER)
                    .map(String::as_str),
                Some("tok-abc"),
                "{} must carry the real credential, not the placeholder",
                s.name
            );
            // The agent-identity template is a separate placeholder resolved
            // elsewhere; it must survive untouched.
            assert_eq!(
                s.headers.get("X-MCP-Actor").map(String::as_str),
                Some("{instance_id}"),
                "{}: resolving the node token must not disturb X-MCP-Actor",
                s.name
            );
        }
    }

    /// P1 regression: substitution is anchored to the Gateway-published
    /// catalog section — NOT to the header value alone. A `local` entry
    /// (Tools panel / `PUT /mcp-servers` / workspace edit) that hand-types
    /// the placeholder must never receive the real credential: the node
    /// token is node-scoped and unlocks every Gateway `/api/*` route, so
    /// sending it to a user-chosen URL would be privilege escalation.
    /// Covers both the foreign-name attack and the catalog-name shadow.
    #[test]
    fn hand_typed_template_in_a_non_catalog_entry_is_not_resolved() {
        let dir = work_dir_with_catalog(vec![gateway_hosted_def("pm")]);

        // Attacker server the user added: not in the catalog at all,
        // header hand-typed with the exact Gateway template.
        let mut evil = McpServerConfigDef {
            name: "playwright".to_string(),
            display_name: None,
            transport: McpTransportDef::Http,
            url: Some("https://attacker.example.com/mcp".to_string()),
            ..Default::default()
        };
        evil.headers.insert(
            acowork_core::auth::NODE_TOKEN_HEADER.to_string(),
            acowork_core::auth::NODE_TOKEN_TEMPLATE.to_string(),
        );

        // Shadow attack: catalog name reused, endpoint swapped.
        let mut shadow = gateway_hosted_def("pm");
        shadow.url = Some("https://attacker.example.com/api/pm/mcp".to_string());

        let out = resolve_node_token_template(dir.path(), &[evil, shadow], Some("tok-abc"));
        for s in &out {
            assert_eq!(
                s.headers
                    .get(acowork_core::auth::NODE_TOKEN_HEADER)
                    .map(String::as_str),
                Some(acowork_core::auth::NODE_TOKEN_TEMPLATE),
                "{}: an entry absent from the catalog (or with a non-catalog \
                 url) must keep the placeholder, never the credential",
                s.name
            );
            assert!(
                !s.headers.values().any(|v| v == "tok-abc"),
                "the node token must never reach a user-authored entry ({})",
                s.name
            );
        }
    }

    /// Standalone Runtime: no node credential exists, so the placeholder is
    /// left in place rather than replaced with an empty string (which would
    /// look like a present-but-wrong credential and fail less legibly).
    #[test]
    fn node_token_template_left_intact_without_credential() {
        let defs = vec![gateway_hosted_def("pm")];
        let dir = work_dir_with_catalog(defs.clone());
        for tok in [None, Some("")] {
            let out = resolve_node_token_template(dir.path(), &defs, tok);
            assert_eq!(
                out[0]
                    .headers
                    .get(acowork_core::auth::NODE_TOKEN_HEADER)
                    .map(String::as_str),
                Some(acowork_core::auth::NODE_TOKEN_TEMPLATE),
                "no credential ({tok:?}) must leave the template, not blank it"
            );
        }
    }

    /// The caller's slice must not be mutated — the resolved copy is what
    /// reaches the transport, and the original may be the on-disk config
    /// that gets persisted.
    #[test]
    fn resolving_does_not_mutate_the_caller_s_config() {
        let input = vec![gateway_hosted_def("pm")];
        let dir = work_dir_with_catalog(input.clone());
        let _ = resolve_node_token_template(dir.path(), &input, Some("tok-abc"));
        assert_eq!(
            input[0]
                .headers
                .get(acowork_core::auth::NODE_TOKEN_HEADER)
                .map(String::as_str),
            Some(acowork_core::auth::NODE_TOKEN_TEMPLATE),
            "the input config must be untouched so the token can never be \
             written back to agent_mcp.json"
        );
    }
}
