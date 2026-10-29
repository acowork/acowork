//! MCP install tool — install and verify an MCP server via scratch test.
//!
//! This tool writes MCP server config to the `local` list in agent_mcp.json,
//! then performs a scratch test (temporary connect → initialize → list_tools → disconnect).
//! On success, the config is retained. On failure, it is rolled back (removed from local).
//!
//! After successful install, the tool notifies the main loop via
//! [`McpConfigNotifier`](crate::mcp_notify::McpConfigNotifier) that the
//! config has changed, triggering MCP server reconnection.

use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};
use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::agent_config::{load_agent_mcp_config, save_agent_mcp_config};
use crate::mcp_notify::McpNotifyRef;

/// MCP install tool — adds an MCP server to the agent's local config,
/// validates it via scratch test, rolls back on failure, and notifies
/// the main loop via [`McpConfigNotifier`](crate::mcp_notify::McpConfigNotifier)
/// on success.
///
/// Uses `agent_home` (not session `work_dir`) for config persistence.
/// MCP configs are per-agent, stored in `{agent_home}/config/agent_mcp.json`,
/// not per-project. When a workspace is open, `current_work_dir` points
/// to the project root, but MCP install must always write to the agent home
/// to ensure configs survive workspace switches and are correctly loaded
/// on startup/reconnect.
pub struct McpInstallTool {
    notifier: McpNotifyRef,
    /// Agent home directory — the authoritative location for MCP config persistence.
    /// Set at construction time from `config().work_dir`. Always required;
    /// MCP configs are per-agent, stored in `{agent_home}/config/agent_mcp.json`,
    /// never per-project.
    agent_home: String,
}

impl McpInstallTool {
    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "mcp_install".to_string(),
            description: "Install an MCP server by writing its config to agent_mcp.json \
                (local list), then verifying the connection via a scratch test. \
                On success, the config is saved and the MCP tools will be available \
                after the next config reload. On failure, the config is rolled back. \
                Parameters: name (unique identifier), transport (stdio/sse/http), \
                command (executable path), args (command arguments), env (environment \
                variables), url (for SSE/HTTP transport)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "MCP server name (unique identifier, e.g. 'docling')"
                    },
                    "transport": {
                        "type": "string",
                        "enum": ["stdio", "sse", "http"],
                        "description": "Transport type: 'stdio' (default), 'sse', or 'http'"
                    },
                    "command": {
                        "type": "string",
                        "description": "Command to launch the MCP server (e.g. 'uvx', 'npx', '/path/to/binary')"
                    },
                    "args": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Command arguments (e.g. ['docling-mcp-server'] or ['-y', '@mcp/server-github'])"
                    },
                    "env": {
                        "type": "object",
                        "additionalProperties": { "type": "string" },
                        "description": "Environment variables for the server (e.g. {'API_KEY': 'xxx'})"
                    },
                    "url": {
                        "type": "string",
                        "description": "Server URL (required for SSE/HTTP transport)"
                    }
                },
                "required": ["name", "transport", "command"]
            }),
        }
    }
}

impl McpInstallTool {
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
impl Tool for McpInstallTool {
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

        // ── Step 1: Parse and validate parameters ──
        let name = params["name"].as_str().unwrap_or("").trim();
        if name.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Parameter 'name' is required and must be non-empty".to_string()),
                token_usage: None,
            });
        }

        let transport_str = params["transport"].as_str().unwrap_or("stdio");
        let transport = match transport_str.to_lowercase().as_str() {
            "stdio" => McpTransportDef::Stdio,
            "sse" => McpTransportDef::Sse,
            "http" => McpTransportDef::Http,
            other => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Invalid transport '{}': must be 'stdio', 'sse', or 'http'",
                        other
                    )),
                    token_usage: None,
                });
            }
        };

        let command = params["command"].as_str().unwrap_or("").trim();
        if command.is_empty() {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some("Parameter 'command' is required".to_string()),
                token_usage: None,
            });
        }

        let args: Vec<String> = params["args"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let env: std::collections::HashMap<String, String> = params["env"]
            .as_object()
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        let url = params["url"].as_str().map(String::from);

        // ── Step 2: Check for name conflict ──
        let work_dir_path = std::path::Path::new(work_dir);
        let current_config = load_agent_mcp_config(work_dir_path)
            .unwrap_or_default()
            .unwrap_or_default();

        if current_config.contains_name(name) {
            let source = if current_config.is_catalog(name) {
                "catalog"
            } else {
                "local"
            };
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!(
                    "MCP server '{}' already exists in the {} list. \
                    Use a different name or mcp_uninstall first if it's a local MCP.",
                    name, source
                )),
                token_usage: None,
            });
        }

        // ── Step 3: Build config and write to local list ──
        let new_config = McpServerConfigDef {
            name: name.to_string(),
            display_name: None,
            transport,
            url,
            command: command.to_string(),
            args,
            env,
            headers: std::collections::HashMap::new(),
            tool_timeout_secs: None,
            install: None,
            required_secrets: vec![],
            origin: None,
        };

        let mut updated_config = current_config;
        updated_config.local.push(new_config.clone());

        // Default-on: an agent-installed MCP is active right after install
        // (the user installed it because they intend to use it). The Tools
        // panel toggle can turn it off afterwards — `active_names` persists
        // the user's choice.
        crate::agent_config::activate_mcp_name(&mut updated_config.active_names, name);

        if let Err(e) = save_agent_mcp_config(work_dir_path, &updated_config) {
            return Ok(ToolResult {
                ok: false,
                content: String::new(),
                error: Some(format!("Failed to save agent_mcp.json: {}", e)),
                token_usage: None,
            });
        }

        // ── Step 4: Scratch test — temporary connect, verify, disconnect ──
        // McpServerConfig = McpServerConfigDef (type alias in acowork-mcp),
        // so we can pass the config directly without conversion.
        let scratch_result = acowork_mcp::client::McpClient::connect(new_config.clone()).await;

        match scratch_result {
            Ok(client) => {
                // Verify: fetch tool names from the connected server
                let tool_names: Vec<String> =
                    client.tools().iter().map(|t| t.name.clone()).collect();
                let tool_count = tool_names.len();

                // Disconnect the scratch client
                client.disconnect().await;

                tracing::info!(
                    mcp_name = name,
                    tool_count = tool_count,
                    "mcp_install scratch test passed — server verified"
                );

                // ── Notify main loop that config has changed ──
                if let Some(ref notifier) = self.notifier {
                    notifier.notify();
                }

                Ok(ToolResult {
                    ok: true,
                    content: format!(
                        "MCP server '{}' installed and verified. Available tools ({}): [{}]. \
                        The MCP tools will be available after the next config reload.",
                        name,
                        tool_count,
                        tool_names.join(", ")
                    ),
                    error: None,
                    token_usage: None,
                })
            }
            Err(e) => {
                // ── Rollback: remove from local list ──
                tracing::warn!(
                    mcp_name = name,
                    error = %e,
                    "mcp_install scratch test failed — rolling back config"
                );

                let mut rollback_config = load_agent_mcp_config(work_dir_path)
                    .unwrap_or_default()
                    .unwrap_or_default();
                rollback_config.local.retain(|s| s.name != name);
                let _ = save_agent_mcp_config(work_dir_path, &rollback_config);

                Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "MCP server '{}' connection test failed: {:#}. \
                        Config has been rolled back. \
                        Suggestion: check that the command '{}' is installed and accessible.",
                        name, e, command
                    )),
                    token_usage: None,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_has_required_params() {
        let spec = McpInstallTool::spec_value();
        assert_eq!(spec.name, "mcp_install");
        let schema = &spec.input_schema;
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|r| r.as_str() == Some("name")));
        assert!(required.iter().any(|r| r.as_str() == Some("transport")));
        assert!(required.iter().any(|r| r.as_str() == Some("command")));
    }
}
