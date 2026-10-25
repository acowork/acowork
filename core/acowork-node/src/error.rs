//! Error types for acowork-node.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Config error: {0}")]
    Config(String),

    #[error("Identity error: {0}")]
    Identity(String),

    #[error("MQTT error: {0}")]
    Mqtt(String),

    #[error("MQTT client error: {0}")]
    MqttClient(#[from] crate::control::mqtt::NodeMqttClientError),

    #[error("Protocol error: {0}")]
    Protocol(String),

    /// Command reached the node but the required capability is not
    /// implemented yet (e.g. `start` before Phase 2b). Surfaced to the
    /// Gateway as a `not_implemented` NodeEvent.
    #[error("Command not implemented: {0}")]
    NotImplemented(String),

    // ── Migrated from GatewayError (ADR-055 §6.20 re-base) ──────────
    // These variants mirror the GatewayError arms used by the code
    // migrated from `package_manager/` and `lifecycle/` so the
    // migration is a mechanical error-type swap, not a semantic change.

    /// Package install / uninstall / clone / publish failure.
    #[error("Package error: {0}")]
    Package(String),

    /// Referenced agent is not installed on this node.
    #[error("Agent not found: {0}")]
    AgentNotFound(String),

    /// Agent is already running (spawn / install would conflict).
    #[error("Agent already running: {0}")]
    AgentAlreadyRunning(String),

    /// Agent is not currently running.
    #[error("Agent not running: {0}")]
    AgentNotRunning(String),

    /// Runtime process lifecycle failure (spawn/kill/health).
    #[error("Lifecycle error: {0}")]
    Lifecycle(String),

    /// Package signature / signing failure.
    #[error("Sign error: {0}")]
    Sign(String),

    /// One-shot memory migration failure (ADR-101 decision 10): the legacy
    /// workspace store could not be read, was too large to hand over, or the
    /// memory service refused it. Never retried silently — see
    /// [`crate::memory_migration`].
    #[error("Memory migration error: {0}")]
    MemoryMigration(String),
}

pub type Result<T> = std::result::Result<T, NodeError>;

