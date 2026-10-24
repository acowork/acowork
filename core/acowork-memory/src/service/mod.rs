//! acowork-memory service layer (ADR-100).
//!
//! Turns this crate from a Runtime-embedded capability into a standalone
//! process supervised by the Gateway, exactly like acowork-doc / acowork-pm /
//! acowork-user: the binary binds its own port (default **18084**) and serves
//! REST (memory panel + admin) and MCP (`POST /mcp`) from one axum router.
//!
//! The service half is strictly additive to the library half: Runtime and
//! other in-process consumers keep using [`crate::MemoryProvider`] /
//! [`crate::SqliteStore`] directly until P3 removes that path.
//!
//! Layout:
//! - [`cli`] / [`config`] — process arguments + `{data_dir}/config.toml`
//! - [`state`] — per-instance store registry (ADR-100 decision 3)
//! - [`mcp`] — `memory_store` / `memory_recall` over the stateless JSON-RPC
//!   subset
//! - [`api`] — panel-facing REST endpoints ported from the Runtime
//! - [`embedding`] — the OpenAI-compatible client both the steady-state
//!   retrieval path and the index rebuild use
//! - [`resources`] — the Gateway global-resources pull (LLM providers +
//!   embedding endpoint) that feeds the pipeline credentials
//! - [`llm`] — the OpenAI-compatible chat client behind the distiller's
//!   [`crate::consolidation::ConsolidationLlm`]
//! - [`pipeline`] — distiller + forgetting background loops, per instance

pub mod api;
pub mod cli;
pub mod config;
pub mod embedding;
pub mod error;
pub mod health;
pub mod llm;
pub mod mcp;
pub mod pipeline;
pub mod resources;
pub mod server;
pub mod state;

pub use cli::Cli;
pub use config::{MemoryServiceConfig, default_data_dir};
pub use error::{Result as ServiceResult, ServiceError};
pub use server::MemoryService;
pub use state::MemoryState;
