//! acowork-runtime — Agent Runtime library
//!
//! Unified execution engine for .agent packages.

pub mod agent;
pub mod agent_config;
pub mod cancellation;
pub mod cli;
pub mod config;
pub mod conversation;
pub mod conversation_index;
pub mod debug;
pub mod embedding;
pub mod episode_distill;
pub mod error;
pub mod http;
pub mod mcp_ensure;
pub mod mcp_notify;
pub mod memory;
pub mod mqtt;
pub mod package;
pub mod platform;
pub mod prompt;
pub mod providers;
pub mod search;
pub mod security;
pub mod skills;
pub mod startup;
#[cfg(test)]
pub mod test_support;
pub mod token;
pub mod tools;
pub mod usecases;
pub mod util;
pub mod workspace;
