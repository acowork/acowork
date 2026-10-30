//! HTTP API module
//!
//! Provides REST + WebSocket API for Desktop App and CLI access.
//! Shares `Arc<RwLock<GatewayState>>` with the gRPC server.

pub mod acl;
pub mod agent_config;
pub mod agents;
pub mod auth;
pub mod auth_middleware;
pub mod bootstrap_api;
pub mod config_api;
pub mod cron_api;
pub mod debug_mqtt;
// ADR-084: the user domain (`account_api` / `users_api` / `chat_api` /
// `auth_api`) moved to the `acowork-user` crate; the Gateway only proxies it.
pub mod doc_proxy;
#[cfg(test)]
pub mod test_support;
pub mod user_proxy;
pub mod embedding_api;
pub mod fs_browse;
pub mod global_resources_api;
pub mod intent_api;
pub mod mcp_catalog_api;
pub mod mcp_presets;
pub mod lsp_admin_proxy;
pub mod lsp_proxy;
pub mod memory_api;
pub mod memory_proxy;
pub mod models_api;
pub mod nodes_api;
pub mod pm_proxy;
pub mod permission;
pub mod provider_api;
#[cfg(test)]
mod provider_probe_tests;
pub mod proxy;
pub mod publish_api;
pub mod relay_api;
pub mod restricted_mode;
pub mod routes;
pub mod server;
pub mod services_api;
pub mod settings_api;
pub mod skills_api;
pub mod vault_api;
pub mod workspaces;
