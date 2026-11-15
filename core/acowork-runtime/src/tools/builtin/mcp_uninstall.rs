//! MCP uninstall tool — remove an agent-installed MCP server from local config.
//!
//! This tool removes an MCP server from the `local` list in agent_mcp.json.
//! It refuses to uninstall catalog MCPs (managed by Gateway).
//! After successful uninstall, the tool notifies the main loop via
//! [`McpConfigNotifier`](crate::mcp_notify::McpConfigNotifier) that the
//! config has changed, triggering MCP server reconnection.

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::agent_config::{load_agent_mcp_config, save_agent_mcp_config};
use crate::mcp_notify::McpNotifyRef;

/// MCP uninstall tool — removes an MCP server from the agent's local config
/// and notifies the main loop via
/// [`McpConfigNotifier`](crate::mcp_notify::McpConfigNotifier) on success.
///
/// Uses `agent_home` (not session `work_dir`) for config persistence.
/// MCP configs are per-agent, stored in `{agent_home}/config/agent_mcp.json`,
/// not per-project. This ensures uninstall reads/writes from the same
/// location where `mcp_install` wrote, regardless of active workspace.
pub struct McpUninstallTool {
    notifier: McpNotifyRef,
    /// Agent home directory — the authoritative location for MCP config persistence.
    /// Always required; MCP configs are per-agent, stored in
    /// `{agent_home}/config/agent_mcp.json`, never per-project.
    agent_home: String,
}

impl McpUninstallTool {
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "mcp_uninstall".to_string(),
            description: "Remove an MCP server from the agent's local config. \
                Only local (agent-installed) MCPs can be uninstalled. \
                Catalog MCPs (managed by Gateway) cannot be removed — use \
                Gateway settings instead. After uninstall, the MCP tools will \
                be removed after the next config reload."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "MCP server name to uninstall (must be a local MCP)"
                    }
                },
                "required": ["name"]
            }),
        }
    }
}

impl McpUninstallTool {
    /// Create with the required agent home directory.
    ///
    /// MCP configs are per-agent — they must always be written to the
    /// agent's home directory, not the session's project workspace.
    pub fn new(notifier: McpNotifyRef, agent_home: String) -> Self {
        Self {
            notifier,
            agent_home,
        }
    }
}

#[async_trait]
impl Tool for McpUninstallTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        // MCP configs are per-agent, stored in {agent_home}/config/agent_mcp.json.
        // Always use agent_home — never fall back to the trait-provided work_dir
        // (which is the session's project workspace, not the agent home).
        let work_dir = self.agent_home.as_str();

        let name = params["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some("Parameter 'name' is required".to_string()),
                token_usage: None,
            });
        }

        let work_dir_path = std::path::Path::new(work_dir);
        let mut current_config = load_agent_mcp_config(work_dir_path)
            .unwrap_or_default()
            .unwrap_or_default();

        // ── Check: cannot uninstall catalog MCP ──
        if current_config.is_catalog(name) {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "MCP server '{}' is a catalog MCP managed by Gateway. \
                    It cannot be uninstalled via this tool. \
                    Remove it from Gateway settings instead.",
                    name
                )),
                token_usage: None,
            });
        }

        // ── Remove from local list ──
        let local_count_before = current_config.local.len();
        current_config.local.retain(|s| s.name != name);

        if current_config.local.len() == local_count_before {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "MCP server '{}' not found in the local list. \
                    It may not have been installed via mcp_install.",
                    name
                )),
                token_usage: None,
            });
        }

        // ── Save updated config ──
        if let Err(e) = save_agent_mcp_config(work_dir_path, &current_config) {
            return Ok(ToolResult {
                content_parts: None,
                ok: false,
                content: String::new(),
                error: Some(format!("Failed to save agent_mcp.json: {}", e)),
                token_usage: None,
            });
        }

        tracing::info!(
            mcp_name = name,
            "mcp_uninstall: removed MCP server from local config"
        );

        // ── Notify main loop that config has changed ──
        if let Some(ref notifier) = self.notifier {
            notifier.notify();
        }

        Ok(ToolResult {
            content_parts: None,
            ok: true,
            content: format!(
                "MCP server '{}' uninstalled from local config. \
                The MCP tools will be removed after the next config reload.",
                name
            ),
            error: None,
            token_usage: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_required_params() {
        let spec = McpUninstallTool::spec_value();
        assert_eq!(spec.name, "mcp_uninstall");
        let schema = &spec.input_schema;
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r.as_str() == Some("name")));
    }
}
