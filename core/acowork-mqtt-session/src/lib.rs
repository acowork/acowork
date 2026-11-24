//! Shared MQTT session lifecycle module (ADR-039 Phase 2 / ADR-065).
//!
//! Exports:
//! - [`MqttSession`] – session handle bundling state + reconnect
//! - [`MqttClient`] – unified MQTT client with internalized poll loop (ADR-065 Step 3)
//! - [`MqttClientHandler`] – entity-specific behavior trait for [`MqttClient`]
//! - [`MqttClientConfig`] – entity-only client configuration + timing constants
//! - [`BootstrapAction`] – five-step idempotent bootstrap contract
//! - [`ErrClass`] – 6-type error classifier
//! - [`ErrorDescriptor`] – version-agnostic error description
//! - [`classify`] – maps `ErrorDescriptor` to `ErrClass`
//! - [`SessionState`] – state machine for MQTT connection lifecycle
//! - [`ReconnectPolicy`] – exponential backoff with jitter
//! - [`SessionStateTx`] / [`SessionStateRx`] – `tokio::sync::watch` broadcast
//! - [`ForceRestart`] – AtomicBool + Notify force-restart signal (ADR-065 Step 2)

mod bootstrap;
mod client;
mod config;
mod err_class;
mod force_restart;
pub mod power;
mod reconnect;
mod session;
mod session_state;

pub use bootstrap::BootstrapAction;
pub use client::{MqttClient, MqttClientError, MqttClientHandler};
pub use config::{
    DEFAULT_MAX_PACKET_SIZE, FATAL_BACKOFF, FATAL_STREAK_LIMIT, KEEPALIVE_INTERVAL,
    MqttClientConfig, MqttTransport, POLL_WATCHDOG_TIMEOUT, POWER_PROBE_INTERVAL,
    WAKE_DETECT_THRESHOLD,
};
pub use err_class::{classify, ErrorDescriptor, ErrorKind, ErrClass, RefusedReason};
pub use force_restart::ForceRestart;
pub use reconnect::{Backoff, ReconnectPolicy};
pub use session::MqttSession;
pub use session_state::{SessionState, SessionStateRx, SessionStateTx, StateWait};
