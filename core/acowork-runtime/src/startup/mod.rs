//! Agent startup sequence — structured phase orchestration.
//!
//! This module breaks the monolithic `async_main` into four clearly-defined
//! phases, each with a single responsibility:
//!
//! ```text
//! Phase A  — per-agent init  (package, gateway, provider, tools, embedding)
//! Phase B  — per-session init (conversation, AgentCore, SessionManager)
//! Phase C  — spawn subsystems (chunk_relay, MCP auto-connect, DevMode)
//! Phase D  — announce ready + run gateway loop
//! ```
//!
//! Each phase function is in its own sub-module and carries a tracing
//! `info_span` so startup duration is visible in structured logs.

pub(crate) mod agent_init;
pub(crate) mod context;
pub(crate) mod debug_enable;
pub(crate) mod gateway_loop;
pub(crate) mod global_resources_pull;
pub(crate) mod session_init;
pub(crate) mod subsystems;

// Re-export the active-pull helper so Phase A can call it without
// importing the full sub-module path.
pub(crate) use global_resources_pull::pull_global_resources_from_gateway;

// Re-export the phase entry points for convenience.
pub(crate) use agent_init::phase_a_init_agent;
pub(crate) use gateway_loop::phase_d_run;
pub(crate) use session_init::phase_b_init_session;
pub(crate) use subsystems::phase_c_spawn_subsystems;
// Re-export helpers from `cli.rs` that Phase A needs.
// These are free functions defined at the crate root of `cli.rs` and are
// not part of any `impl` block, so we alias them here for clarity.
//
// ADR-040: `connect_gateway_client` removed — gRPC path is dead.
pub(crate) mod super_mod {
    // ADR-094: `resolve_skill_mode` is still in `cli.rs` (kept there so
    // the existing call sites in `agent_init` / `http/prompts` need
    // only the function name — no signature churn). Phase A still uses
    // the re-export below.
    pub(crate) use crate::cli::resolve_skill_mode;
}
