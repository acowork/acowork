//! acowork-gateway — Gateway library
//!
//! Long-running system process: manages Agent lifecycle, Intent routing, key distribution, budget coordination.

pub mod auth;
pub mod bootstrap;
pub mod budget;
pub mod capability;
pub mod cli;
pub mod config;
pub mod cron;
pub mod embedding_providers;
pub mod error;
pub mod gateway;
pub mod http;
pub mod intent;
pub mod interaction_store;
pub mod handlers;
pub mod lifecycle;
pub mod mqtt;
pub mod operation_store;
pub mod probe;
pub mod rate;
pub mod relay;
pub mod resource_cache;
pub mod security;
pub mod util;
pub mod vault;

/// Type alias for the tracing reload handle used to dynamically change log levels.
pub type LogReloadHandle =
    tracing_subscriber::reload::Handle<tracing_subscriber::EnvFilter, tracing_subscriber::Registry>;
