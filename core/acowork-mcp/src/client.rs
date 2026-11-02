// Adapted from zeroclaw/src/tools/mcp_client.rs
// ACowork deviation: uses acowork-mcp's own transport/protocol modules
// SPDX-License-Identifier: MIT OR Apache-2.0

use indexmap::IndexMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use acowork_core::timeout_config::constants;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::future::join_all;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

use crate::config::McpServerConfig;
use crate::protocol::{JsonRpcRequest, MCP_PROTOCOL_VERSION, McpToolDef, McpToolsListResult};
use crate::transport::{McpTransportConn, create_transport_with};

/// Timeout for receiving a response from an MCP server during init/list.
const RECV_TIMEOUT: Duration = constants::MCP_RECV;

/// Default timeout for tool calls when not configured per-server.
const DEFAULT_TOOL_TIMEOUT: Duration = constants::MCP_DEFAULT_TOOL;

/// Maximum allowed tool call timeout — hard safety ceiling.
const MAX_TOOL_TIMEOUT: Duration = constants::MCP_MAX_TOOL;

// ── Internal server state ──────────────────────────────────────────────────

/// Read-only server metadata (no Mutex needed for reads).
struct McpServerMeta {
    config: McpServerConfig,
    tools: Vec<McpToolDef>,
}

/// Mutable transport state, locked only during IO.
struct McpTransportState {
    /// `None` means the transport has been disconnected / not yet connected.
    transport: Option<Box<dyn McpTransportConn>>,
}

impl McpTransportState {
    /// Replace the transport with a new one, returning the old transport.
    fn replace(&mut self, new: Box<dyn McpTransportConn>) -> Option<Box<dyn McpTransportConn>> {
        self.transport.replace(new)
    }
}

// ── McpClient ──────────────────────────────────────────────────────────────

/// A live connection to one MCP server (any transport, e.g. stdio or HTTP).
///
/// After [`McpClient::connect`], the server has been initialized and its
/// tool list has been fetched. Call [`McpClient::call_tool`] for execution.
///
/// If the transport connection is lost (e.g. the MCP server process crashes),
/// `is_alive` is set to `false` and subsequent `call_tool` calls will
/// automatically attempt to reconnect before dispatching the request.
/// Use [`McpClient::reconnect`] explicitly for manual reconnection.
///
/// Internal structure separates read-only metadata from mutable transport
/// state so that `tools()` and `name()` are not blocked by in-flight IO.
pub struct McpClient {
    meta: Arc<McpServerMeta>,
    transport: Arc<Mutex<McpTransportState>>,
    next_id: Arc<AtomicU64>,
    /// Whether the transport is believed to be alive.
    /// Set to `false` on transport errors, set back to `true` on successful reconnect.
    is_alive: Arc<AtomicBool>,
}

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            meta: Arc::clone(&self.meta),
            transport: Arc::clone(&self.transport),
            next_id: Arc::clone(&self.next_id),
            is_alive: Arc::clone(&self.is_alive),
        }
    }
}

impl McpClient {
    /// Connect to the server, perform the initialize handshake, and fetch the tool list.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        Self::connect_with(config, None).await
    }

    /// Same as [`McpClient::connect`], forwarding the server process's stderr
    /// to `sink` while the connection lives — used by the first-enable health
    /// check, where a lazy `npx`/`uvx` download prints its progress there.
    pub async fn connect_with(
        config: McpServerConfig,
        sink: Option<crate::transport::ServerOutputSink>,
    ) -> Result<Self> {
        Self::connect_with_timeout(config, sink, Some(RECV_TIMEOUT)).await
    }

    /// Same as [`McpClient::connect_with`] with an explicit handshake bound.
    ///
    /// The wait is a property of *the command being spawned*, not of the MCP
    /// protocol: a lazily resolved package downloads during the first spawn, so
    /// a fixed cap cannot tell a healthy first launch from a hang. `None`
    /// leaves the handshake unbounded, which is what a caller that watches
    /// output activity wants — it enforces its own "no progress" deadline
    /// (the Runtime's probe does exactly that, ADR-090 §5.4).
    pub async fn connect_with_timeout(
        config: McpServerConfig,
        sink: Option<crate::transport::ServerOutputSink>,
        recv_timeout: Option<Duration>,
    ) -> Result<Self> {
        let mut transport = create_transport_with(&config, sink).with_context(|| {
            format!(
                "failed to create transport for MCP server `{}`",
                config.name
            )
        })?;

        // ── Initialize handshake ──────────────────────────────────────────
        let init_req = JsonRpcRequest::new(
            1u64,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "acowork",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );

        let init_resp = match recv_timeout {
            Some(limit) => timeout(limit, transport.send_and_recv(&init_req))
                .await
                .with_context(|| {
                    format!(
                        "MCP server `{}` timed out waiting for initialize response",
                        config.name
                    )
                })??,
            None => transport.send_and_recv(&init_req).await?,
        };

        if init_resp.error.is_some() {
            bail!(
                "MCP server `{}` rejected initialize: {:?}",
                config.name,
                init_resp.error
            );
        }

        // Notify server that client is initialized (notification, best-effort)
        let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
        let _ = transport.send_and_recv(&notif).await;

        // ── Fetch tool list ──────────────────────────────────────────────
        let list_req = JsonRpcRequest::new(2u64, "tools/list", json!({}));

        let list_resp = match recv_timeout {
            Some(limit) => timeout(limit, transport.send_and_recv(&list_req))
                .await
                .with_context(|| {
                    format!(
                        "MCP server `{}` timed out waiting for tools/list response",
                        config.name
                    )
                })??,
            None => transport.send_and_recv(&list_req).await?,
        };

        let result = list_resp
            .result
            .ok_or_else(|| anyhow!("tools/list returned no result from `{}`", config.name))?;
        let tool_list: McpToolsListResult = serde_json::from_value(result)
            .with_context(|| format!("failed to parse tools/list from `{}`", config.name))?;

        let tool_count = tool_list.tools.len();

        let inner = McpServerMeta {
            config,
            tools: tool_list.tools,
        };

        tracing::info!(
            "MCP server `{}` connected — {} tool(s) available",
            inner.config.name,
            tool_count
        );

        Ok(Self {
            meta: Arc::new(inner),
            transport: Arc::new(Mutex::new(McpTransportState {
                transport: Some(transport),
            })),
            next_id: Arc::new(AtomicU64::new(3)), // IDs 1 and 2 were used for init + list
            is_alive: Arc::new(AtomicBool::new(true)),
        })
    }

    /// Close the connection to this MCP server.
    ///
    /// After calling disconnect, further `call_tool` calls will attempt
    /// auto-reconnection. For stdio transport this shuts down stdin and
    /// kills the child process (via `kill_on_drop`). For HTTP transport
    /// this is a no-op.
    pub async fn disconnect(&self) {
        self.is_alive.store(false, Ordering::SeqCst);
        let mut state = self.transport.lock().await;
        if let Some(mut transport) = state.transport.take()
            && let Err(e) = transport.close().await
        {
            tracing::warn!(
                "Error closing MCP server `{}`: {:#}",
                self.meta.config.name,
                e
            );
        }
    }

    /// Whether the transport connection is believed to be alive.
    pub fn is_alive(&self) -> bool {
        self.is_alive.load(Ordering::SeqCst)
    }

    /// Attempt to reconnect to this MCP server.
    ///
    /// Performs a full handshake (initialize + tools/list) with the original
    /// config. On success, `is_alive` is set back to `true` and the transport
    /// is replaced. On failure, the client remains in the disconnected
    /// state and the error is returned.
    pub async fn reconnect(&self) -> Result<()> {
        let config = &self.meta.config;
        tracing::info!("MCP server `{}`: attempting reconnect", config.name);

        // Perform a full connect (handshake + tools/list) to get a fresh client
        let new_client = Self::connect(config.clone()).await?;

        // Replace the transport under the lock.
        // The old transport (if any) is closed (best-effort) and dropped.
        {
            let mut new_state = new_client.transport.lock().await;
            let mut state = self.transport.lock().await;
            // New client always has Some(transport) after successful connect
            let new_transport = new_state
                .transport
                .take()
                .expect("new client must have a transport after connect");
            if let Some(mut old) = state.replace(new_transport) {
                let _ = old.close().await;
            }
        }

        // Reset the request ID counter to align with the new connection
        let new_next_id = new_client.next_id.load(Ordering::Relaxed);
        self.next_id.store(new_next_id, Ordering::Relaxed);

        self.is_alive.store(true, Ordering::SeqCst);
        tracing::info!("MCP server `{}`: reconnected successfully", config.name);
        Ok(())
    }

    /// Tools advertised by this server.
    pub fn tools(&self) -> Vec<McpToolDef> {
        self.meta.tools.clone()
    }

    /// Server display name.
    pub fn name(&self) -> String {
        self.meta.config.name.clone()
    }

    /// Call a tool on this server. Returns the raw JSON result.
    ///
    /// Only the transport Mutex is held during IO — metadata reads
    /// (`tools()`, `name()`) are never blocked by in-flight calls.
    ///
    /// If the transport is disconnected, returns an error without attempting
    /// auto-reconnect (the caller should use `reconnect()` explicitly or via
    /// the registry's `call_tool` which handles auto-reconnect).
    /// Transport errors mark this client as not alive.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = JsonRpcRequest::new(
            id,
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
        );

        let tool_timeout = self
            .meta
            .config
            .tool_timeout_secs
            .unwrap_or(DEFAULT_TOOL_TIMEOUT.as_secs())
            .min(MAX_TOOL_TIMEOUT.as_secs());

        let mut state = self.transport.lock().await;
        let transport = match state.transport.as_mut() {
            Some(t) => t,
            None => {
                self.is_alive.store(false, Ordering::SeqCst);
                bail!(
                    "MCP server `{}` is disconnected — transport not available",
                    self.meta.config.name
                );
            }
        };

        let result = timeout(
            Duration::from_secs(tool_timeout),
            transport.send_and_recv(&req),
        )
        .await;

        match result {
            Ok(Ok(resp)) => {
                if let Some(err) = resp.error {
                    bail!("MCP tool `{tool_name}` error {}: {}", err.code, err.message);
                }
                Ok(resp.result.unwrap_or(serde_json::Value::Null))
            }
            Ok(Err(e)) => {
                // Transport-level error — mark as not alive
                self.is_alive.store(false, Ordering::SeqCst);
                Err(e).with_context(|| {
                    format!(
                        "MCP server `{}` error during tool call `{tool_name}`",
                        self.meta.config.name
                    )
                })
            }
            Err(_) => {
                // Timeout — don't mark as not alive (server may just be slow)
                Err(anyhow!(
                    "MCP server `{}` timed out after {}s during tool call `{tool_name}`",
                    self.meta.config.name,
                    tool_timeout
                ))
            }
        }
    }
}

// ── McpConnectionFailure ───────────────────────────────────────────────────

/// Describes a failed MCP server connection attempt.
///
/// Carried up through [`McpRegistry`] → [`McpManager`] → [`SessionManager`]
/// so the Runtime can inject a system notification into the LLM context,
/// letting the Agent use its shell tools to resolve missing dependencies.
#[derive(Debug, Clone)]
pub struct McpConnectionFailure {
    /// Configured server name (e.g. "docling").
    pub server_name: String,
    /// Human-readable error message (e.g. "program not found").
    pub error_message: String,
}

// ── McpRegistry ───────────────────────────────────────────────────────────

/// Registry of all connected MCP servers, with a flat tool index.
///
/// Tools are indexed by a prefixed name (`mcp_<server_name>__<tool_name>`) to
/// prevent name collisions across servers.
pub struct McpRegistry {
    servers: Vec<McpClient>,
    /// prefixed_name -> (server_index, McpToolDef)
    /// The tool def is stored directly in the index to avoid a secondary
    /// linear search in `McpServerMeta.tools`.
    ///
    /// ADR-069 follow-up: `IndexMap` (not `HashMap`) — `tool_names()` and
    /// `get_tool_def()` must preserve the order in which tools were
    /// inserted during `connect_all`, because the runtime's reconcile
    /// pass writes that order into `agent_mcp_tools.json`. A `HashMap`
    /// reshuffles the list on every reconnect (which `PUT /mcp-tools`
    /// triggers), making the Tools-panel tool order reshuffle whenever
    /// the user toggles a single tool.
    tool_index: IndexMap<String, (usize, McpToolDef)>,
}

impl McpRegistry {
    /// Connect to all configured servers in parallel. Non-fatal: failures are
    /// collected and returned alongside the registry so callers can surface
    /// them to the LLM context for self-healing.
    ///
    /// All server connections are attempted concurrently via `join_all`,
    /// reducing total startup time from `N × timeout` to a single timeout
    /// window when multiple servers are configured.
    pub async fn connect_all(
        configs: &[McpServerConfig],
    ) -> Result<(Self, Vec<McpConnectionFailure>)> {
        if configs.is_empty() {
            return Ok((
                Self {
                    servers: Vec::new(),
                    tool_index: IndexMap::new(),
                },
                Vec::new(),
            ));
        }

        // Connect to all servers in parallel
        let connect_futures: Vec<_> = configs
            .iter()
            .map(|config| async {
                let name = config.name.clone();
                let result = McpClient::connect(config.clone()).await;
                (name, result)
            })
            .collect();

        let results = join_all(connect_futures).await;

        // Build server list and tool index from successful connections
        let mut servers = Vec::new();
        let mut tool_index = IndexMap::new();
        let mut failures = Vec::new();

        for (name, result) in results {
            match result {
                Ok(server) => {
                    let server_idx = servers.len();
                    let tools = server.tools();
                    for tool in tools {
                        let prefixed = format!("mcp_{}__{}", name, tool.name);
                        tool_index.insert(prefixed, (server_idx, tool));
                    }
                    servers.push(server);
                }
                Err(e) => {
                    let error_message = format!("{:#}", e);
                    tracing::error!(
                        "Failed to connect to MCP server `{}`: {}",
                        name,
                        error_message
                    );
                    failures.push(McpConnectionFailure {
                        server_name: name,
                        error_message,
                    });
                }
            }
        }

        Ok((
            Self {
                servers,
                tool_index,
            },
            failures,
        ))
    }

    /// All prefixed tool names across all connected servers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Tool definition for a given prefixed name (cloned).
    /// Single HashMap lookup — no secondary linear search.
    pub fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef> {
        let (_, def) = self.tool_index.get(prefixed_name)?;
        Some(def.clone())
    }

    /// Execute a tool by prefixed name.
    ///
    /// If the target server is not alive (transport broken), attempts an
    /// automatic reconnection before dispatching the call. If reconnection
    /// fails, returns the reconnection error.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let (server_idx, def) = self
            .tool_index
            .get(prefixed_name)
            .ok_or_else(|| anyhow!("unknown MCP tool `{prefixed_name}`"))?;

        let server = &self.servers[*server_idx];
        let original_name = &def.name;

        // Auto-reconnect: if the server's transport is dead, try once
        if !server.is_alive() {
            tracing::info!(
                "MCP server `{}` is not alive, attempting auto-reconnect for tool `{}`",
                server.name(),
                prefixed_name,
            );
            server.reconnect().await?;
        }

        let result = server.call_tool(original_name, arguments).await?;
        serde_json::to_string_pretty(&result)
            .with_context(|| format!("failed to serialize result of MCP tool `{prefixed_name}`"))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }

    /// Close connections to all MCP servers.
    ///
    /// Individual close errors are logged and skipped. After calling
    /// disconnect, further `call_tool` calls will return errors.
    pub async fn disconnect(&self) {
        for server in &self.servers {
            server.disconnect().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_prefix_format() {
        let prefixed = format!("mcp_{}__{}", "filesystem", "read_file");
        assert_eq!(prefixed, "mcp_filesystem__read_file");
    }

    #[tokio::test]
    async fn connect_nonexistent_command_fails_cleanly() {
        // Use a name that cannot resolve on any OS — `acowork_xyz_definitely_not_a_real_binary`
        // is not a command on PATH (Unix) and has no `.exe` extension (Windows),
        // so spawn will always fail with either "program not found" (Unix) or
        // "MCP server closed stdout" (Windows after the wrapper exits 1).
        let config = McpServerConfig {
            name: "nonexistent".to_string(),
            command: "acowork_xyz_definitely_not_a_real_binary".to_string(),
            ..Default::default()
        };
        let result = McpClient::connect(config).await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        // The exact text differs across platforms — Unix says
        // "failed to create transport" (a std::io::Error description), while
        // Windows surfaces the process spawn error from the MCP stdio wrapper.
        // Accept any of the known error shapes rather than over-specifying.
        let accepted = [
            "failed to create transport",
            "MCP server closed stdout",
            "program not found",
            "No such file",
        ];
        assert!(
            accepted.iter().any(|needle| msg.contains(needle)),
            "expected a known spawn-failure message; got: {msg}"
        );
    }

    #[tokio::test]
    async fn connect_all_nonfatal_on_single_failure() {
        let configs = vec![McpServerConfig {
            name: "bad".to_string(),
            command: "acowork_xyz_definitely_not_a_real_binary".to_string(),
            ..Default::default()
        }];
        let (registry, failures) = McpRegistry::connect_all(&configs)
            .await
            .expect("connect_all should not fail");
        assert!(registry.is_empty());
        assert_eq!(registry.tool_count(), 0);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].server_name, "bad");
        // Same cross-platform tolerance as the sibling test above.
        let msg = &failures[0].error_message;
        let accepted = [
            "failed to create transport",
            "MCP server closed stdout",
            "program not found",
            "No such file",
        ];
        assert!(
            accepted.iter().any(|needle| msg.contains(needle)),
            "expected a known spawn-failure message; got: {msg}"
        );
    }

    #[tokio::test]
    async fn empty_registry_is_empty() {
        let (registry, failures) = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all on empty slice should succeed");
        assert!(registry.is_empty());
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
        assert!(failures.is_empty());
    }

    #[tokio::test]
    async fn empty_registry_tool_names_is_empty() {
        let (registry, _) = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        assert!(registry.tool_names().is_empty());
    }

    #[tokio::test]
    async fn empty_registry_get_tool_def_returns_none() {
        let (registry, _) = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let result = registry.get_tool_def("mcp_nonexistent__tool");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_registry_call_tool_unknown_name_returns_error() {
        let (registry, _) = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let err = registry
            .call_tool("mcp_nonexistent__tool", serde_json::json!({}))
            .await
            .expect_err("should fail for unknown tool");
        assert!(err.to_string().contains("unknown MCP tool"), "got: {err}");
    }
}
