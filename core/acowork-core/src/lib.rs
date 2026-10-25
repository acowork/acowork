//! acowork-core — Shared types, protocols, and traits for ACowork.AI
//!
//! This crate contains all types shared across the ACowork workspace:
//! - Manifest structures (`.agent` package format)
//! - Protocol messages (MQTT payload types, JSON DTOs)
//! - Tool and Provider traits
//! - Permission, Identity, Budget types
//! - Unified error types

pub mod mqtt_proto {
    #![allow(clippy::large_enum_variant)]
    include!(concat!(env!("OUT_DIR"), "/acowork.mqtt.v1.rs"));

    /// Profile/account-change signal published by `acowork-user`
    /// (ADR-084 §决策 4b).
    ///
    /// QoS 1, **non-retained**, plain-text payload: the profile-list version
    /// the change produced. The signal deliberately carries no data —
    /// `user_profiles.json` is owned by `acowork-user`, so the Gateway's only
    /// correct response is to re-pull `GET /internal/user-profiles`, and a
    /// richer payload would just be a second copy of the truth to keep in
    /// sync. Non-retained because it is a *nudge*, not a value: a missed one
    /// is healed by the next mutation and by the Gateway's startup pull.
    ///
    /// Defined in the shared contract crate, not in either process: the
    /// topic string is a wire contract between two binaries, and a one-sided
    /// rename would break profile refresh **silently** (Runtimes keep a stale
    /// `last_user_profile`) instead of failing a build.
    pub const USER_PROFILES_CHANGED_TOPIC: &str = "acowork/user/profiles/changed";

    /// Topic of the per-account user-to-user chat wake-up signal, published by
    /// `acowork-user` when a thread changes (design doc 25 §8, ADR-076 §决策 8).
    ///
    /// QoS 1, **non-retained**, **empty payload**: a pure nudge. Every client
    /// that reacts to it re-reads `GET /api/users/{uid}/chats` (and, if a
    /// thread is open, its messages) over authenticated HTTP, which owns the
    /// truth. The topic is keyed by the account id so a client subscribes
    /// exactly its own mailbox, and the payload deliberately names nothing
    /// else — the embedded broker has no subscription-level ACL (rumqttd's
    /// only hook is CONNECT, `core/acowork-gateway/src/mqtt/broker.rs`), so
    /// any authenticated account can subscribe `acowork/users/#`. A `chat_id`
    /// in the payload would publish the contact graph: a `chat_id` IS the
    /// pair of user ids.
    ///
    /// Non-retained on purpose: retention would replay the last event to a
    /// client that already caught up during its own `phase=ready` reload.
    ///
    /// Built here rather than in the publisher because the string is a wire
    /// contract between the user service and two clients; a one-sided rename
    /// would silently stop chat push instead of failing a build.
    pub fn user_chat_changed_topic(user_id: &str) -> String {
        format!("acowork/users/{user_id}/chat/changed")
    }
}

pub mod account;
pub mod addr;
pub mod agent_instance_id;
pub mod agent_overrides;
pub mod auth;
pub mod budget;
pub mod crlf;
pub mod defaults;
pub mod embedding;
pub mod error;
pub mod error_codes;
pub mod event_bus;
pub mod health;
pub mod install;
pub mod intent;
pub mod logging;
pub mod lsp;
pub mod manifest;
pub mod memory;
pub mod node;
pub mod operation;
pub mod packaging;
pub mod path_utils;
pub mod permission;
pub mod process;
pub mod protocol;
pub mod providers;
pub mod rag;
pub mod relay;
pub mod retrieval;
pub mod shutdown;
pub mod supervisor;
pub mod timeout_config;
pub mod tools;
pub mod workspace;

// Re-exports for convenience
pub use agent_instance_id::{AgentInstanceId, InstanceIdError};
pub use agent_overrides::{AgentOverrides, overrides_path};
pub use lsp::{
    LSP_INSTALL_OUTPUT_LIMIT, LSP_REGISTRY_VERSION, LspInstallRecord, LspInstallState,
    LspRegistryFile, LspRuntimeEntry, LspServerEntry, LspServersConfig, LspSource,
};
pub use manifest::{
    AgentManifest, CapabilityDef, LlmBudget, LlmConfig, ManifestConsolidationQuality,
    ManifestDedupQuality, ManifestMemoryQuality, ProviderConfig, RagToolConfig, RoutingConfig,
    SkillMode, SkillsConfig, ToolDeclaration,
};
pub use protocol::{
    ConversationEntryDto, GatewayRequest, GatewayResponse, ModelCapabilitiesInfo, ModelCostInfo,
    ModelModalities, ProtocolType, SessionInfoDto, SessionStatusDto,
};

pub use budget::{Budget, UsageReport};
pub use embedding::{EmbeddingError, EmbeddingProvider};
pub use error::AcoworkError;
pub use error_codes::{RetryHint, StructuredErrorBody, StructuredErrorCode};
pub use operation::{OperationId, OperationRecord, OperationState};
pub use packaging::{
    PACKAGE_ALWAYS_EXCLUDE_DIRS, PACKAGE_DEFAULT_EXCLUDE_DIRS, PACKAGE_EXCLUDE_PATTERNS,
    PackageOptions, should_exclude_path,
};
pub use path_utils::{is_absolute, resolve};
pub use permission::{Permission, ShellApprovalThreshold};
pub use process::{IdleTimeoutError, ProcessOutput, run_command_with_idle_timeout};
pub use providers::{
    ChatMessage, ChatRequest, ChatResponse, ContentPart, ImageUrlPart, Provider, ProviderError,
    ProviderErrorType, StreamEvent,
};
pub use rag::{
    AnnotatedRagResult, PROTOCOL_VERSION as RAG_PROTOCOL_VERSION, RagProvider, RagQueryRequest,
    RagQueryResponse, RagResultItem,
};
pub use shutdown::{Shutdown, install_signal_handlers};
pub use timeout_config::{RetryConfig, Timeouts};
pub use tools::{Tool, ToolResult, ToolSpec};
