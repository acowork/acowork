//! Session management module for multi-session concurrency.
//!
//! Provides `SessionTask` (per-session execution actor), `SessionHandle`
//! (external interaction handle), and `SessionManager` (lifecycle manager
//! for multiple concurrent sessions).

pub mod intent_routing;
pub mod restorer;
pub(crate) mod session_handle;
pub mod session_manager;
pub mod session_task;

pub use session_manager::{
    SessionLifecycleState, SessionManager, SessionManagerConfig, SessionOpenOutcome,
};
pub use session_task::{PulledAttachment, SessionMessage};
