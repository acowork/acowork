//! acowork-mcp — MCP (Model Context Protocol) client library.
//!
//! Provides protocol types, transport abstraction, and a client for connecting
//! to MCP tool servers. Adapted from zeroclaw's MCP implementation.
//! SPDX-License-Identifier: MIT OR Apache-2.0

pub mod client;
pub mod config;
pub mod install;
pub mod protocol;
pub mod transport;
pub mod wrapper;

pub use client::{McpClient, McpConnectionFailure, McpRegistry};
pub use config::{McpConfig, McpServerConfig, McpTransport};
pub use install::{
    DependencyStatus, InstallOutput, build_spawn_env, derive_install_command, derive_spawn_config,
    ensure_runtime, health_check, health_check_with_output, probe_runtime, resolves_lazily,
    run_install, run_install_with_output,
};
pub use protocol::{JsonRpcRequest, JsonRpcResponse, MCP_PROTOCOL_VERSION, McpToolDef};
pub use transport::{McpTransportConn, ServerOutputSink, create_transport};
pub use wrapper::McpToolWrapper;
