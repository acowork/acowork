//! Agent module

pub mod agent_core;
pub mod attachment_to_image;
pub mod bootstrap_autobio;
pub mod budget_guard;
pub mod chat_router;
pub mod compression_constants;
pub mod context;

#[cfg(test)]
mod e2e_prompt_cache;
pub mod git_nudge;
pub mod history;
pub mod inbound;
pub mod intent_turn;
pub mod llm_availability;
pub mod loop_;
pub mod loop_approval;
pub mod loop_context;
pub mod loop_detector;
pub mod loop_inbound;
pub mod loop_interaction;
pub mod loop_llm;
pub mod loop_persist;
pub mod loop_session;
pub mod loop_tools;
pub mod retry;
pub use llm_availability::{LlmAvailability, LlmAvailabilityRegistry};
pub mod session;
pub mod session_config;
pub mod session_core;
pub mod session_state;
