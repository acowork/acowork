//! MQTT module — embedded broker, client, and event bus (ADR-033).
//!
//! This module implements the MQTT-based event bus that replaces the
//! previous gRPC + WebSocket protocol stack. In Phase 1, the broker
//! runs alongside the existing gRPC server (dual-channel coexistence).
//!
//! ## Architecture
//!
//! - **Broker** (`broker.rs`): rumqttd embedded in-process, listening on
//!   `127.0.0.1:19875` (MQTT 3.1.1 / TCP).
//! - **Gateway Publisher** (`client.rs` + `global_resources_publisher.rs`):
//!   Gateway's own MQTT client (`client_id: gateway:publisher`) that
//!   publishes `acowork/global/{kind}` Retained topics.
//! - **ACL** (`acl.rs`): Access control configuration (permissive in Phase 1).
//! - **Agent Registry** (`agent_registry.rs`): Online status tracking via LWT.
//! - **Dispatch** (`dispatch.rs`): Plain-text MQTT message dispatcher
//!   (http_port registration + agent status update) with topic wildcard matcher.
//!
//! See `docs/zh/protocols/mqtt.md` for the full protocol specification.

pub mod acl;
pub mod agent_registry;
pub mod bootstrap_publisher;
pub mod broker;
pub mod client;
pub mod dispatch;
pub mod enrollment;
pub mod global_resources_builders;
pub mod global_resources_publisher;
pub mod inventory_notifier;
pub mod node_control;
pub mod node_registry;
pub mod readiness;
pub mod sidecar;
pub mod tcp_filter;

// Re-export key types
pub use acl::{AclConfig, AclConfigError, AclPermission, AclRule};
pub use bootstrap_publisher::{BootstrapPublisher, BootstrapPublisherHandle, BootstrapPublisherOptions, TOPIC_BOOTSTRAP};
pub use broker::{
    start_broker, start_broker_with_auth, BrokerAuth, MqttBrokerError, MqttBrokerHandle,
    RemoteMqttAuth, RemoteMqttListener,
};
pub use client::{GatewayMqttClient, GatewayMqttClientError, MqttMessageCallback, MqttQoS};
pub use global_resources_publisher::{MqttGlobalResourcesPublisher, MqttPublisherHandle, MqttPublisherTrigger};
pub use inventory_notifier::{
    InventoryNotifierHandle, InventoryNotifierTrigger, MqttInventoryNotifier, TOPIC_INVENTORY,
};
pub use node_registry::{new_shared_registry as new_shared_node_registry, NodeRegistry, SharedNodeRegistry};
pub use enrollment::{
    new_shared_enrollment_store, new_shared_node_token_store, EnrollmentTokenStore, NodeTokenStore,
    SharedEnrollmentTokenStore, SharedNodeTokenStore, TokenValidation,
};
