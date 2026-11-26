//! Gateway → Node control-plane client (ADR-055 §6.2 / Phase 2b).
//!
//! The Gateway is the authority for agent lifecycle: it publishes
//! `NodeControlCommand`s to
//! `acowork/nodes/{node_id}/agents/{agent_id}/control/{cmd}` and waits
//! for the correlated `NodeEvent` on
//! `acowork/nodes/{node_id}/agents/{agent_id}/events`. QoS 1 duplicates
//! are handled by the Node's request_id dedup; the Gateway correlates
//! the reply by `request_id` via a pending-request table.
//!
//! Incoming node events are routed here from
//! [`crate::mqtt::dispatch`] (the single Gateway MQTT callback).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEvent,
};
use acowork_core::node::{node_agent_control_topic, node_agent_events_topic};
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

use crate::mqtt::client::{GatewayMqttClient, MqttQoS};

/// Timeout for a node command round-trip. Covers network + Runtime
/// startup latency on the node; the Gateway surfaces a timeout error
/// so callers can retry idempotently.
///
/// Kept at 10s (was 30s): the HTTP `/start` handler awaits this inline,
/// so a Node that is slow to reply (e.g. recovering right after a host
/// suspend/resume) used to leave the Desktop hanging for 30s with zero
/// feedback. 10s still covers a genuine Runtime spawn while bounding
/// worst-case UI latency; the POST /start idempotent fast-path already
/// short-circuits the common "already running" case.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Error type for node control-plane operations.
#[derive(Debug, thiserror::Error)]
pub enum NodeControlError {
    #[error("Node '{node_id}' is offline")]
    NodeOffline { node_id: String },
    #[error("Node command failed for '{agent_id}': {message}")]
    CommandFailed { agent_id: String, message: String },
    #[error("Node command timed out (request_id {request_id})")]
    Timeout { request_id: String },
    #[error("MQTT publish error: {0}")]
    Publish(String),
    #[error("Gateway MQTT client is not available (broker disabled)")]
    NoClient,
}

/// The Gateway's client for issuing agent lifecycle commands to nodes.
#[derive(Clone)]
pub struct NodeControlClient {
    client: Arc<GatewayMqttClient>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<NodeEvent>>>>,
}

/// Where the node should read the package from.
pub enum NodePackageSource<'a> {
    /// Gateway-hosted download URL (ADR-055 §3.2).
    Url(&'a str),
    /// Node-local path of an already-spooled package (Phase 2b
    /// single-machine). Mutually exclusive with [`Self::Url`].
    LocalPath(&'a str),
}

/// One install dispatch to a node.
///
/// ADR-073 §5: the Gateway mints the instance identity and states the
/// intent; the node owns *when* it runs (one install slot per node).
/// Grouping the fields here keeps both entry points (registry URL,
/// spooled local path) building the identical `NodeInstall` payload —
/// a missing `system`/`ensure` flag would otherwise silently change
/// which lane, or which dedup rule, the install lands on.
pub struct NodeInstallDispatch<'a> {
    pub node_id: &'a str,
    /// ADR-073 instance identity, used verbatim by the node.
    pub instance_id: &'a str,
    pub agent_id: &'a str,
    pub source: NodePackageSource<'a>,
    /// Signature strictness (ADR-055 §6.20).
    pub dev_mode: bool,
    /// Declarative "ensure present" intent: if any instance of the
    /// package is already installed on the node, the request is a no-op
    /// instead of landing another copy. `false` = explicit install of
    /// one more copy.
    pub ensure: bool,
    /// ADR-059 §6 correlation id echoed in the node's `NodeEvent` reply.
    /// The blocking [`NodeControlClient::install_agent`] path generates
    /// its own request id instead, so this field is unused there.
    pub operation_id: &'a str,
}

impl NodeInstallDispatch<'_> {
    fn to_proto(&self) -> acowork_core::mqtt_proto::NodeInstall {
        let (package_url, local_path) = match self.source {
            NodePackageSource::Url(url) => (url.to_string(), String::new()),
            NodePackageSource::LocalPath(path) => (String::new(), path.to_string()),
        };
        acowork_core::mqtt_proto::NodeInstall {
            agent_id: self.agent_id.to_string(),
            package_url,
            local_path,
            dev_mode: self.dev_mode,
            instance_id: self.instance_id.to_string(),
            ensure: self.ensure,
        }
    }
}

impl NodeControlClient {
    pub fn new(client: Arc<GatewayMqttClient>) -> Self {
        Self {
            client,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Publish a command and await the correlated NodeEvent reply.
    async fn send(&self, node_id: &str, instance_id: &str, command: NodeControlCommand) -> Result<NodeEvent, NodeControlError> {
        let request_id = command.request_id.clone();
        let cmd_name = command_name(&command);
        // ADR-073: control-plane topics are scoped to the INSTANCE
        // identity (`nodes/{node}/agents/{instance_id}/control/{cmd}`).
        let topic = node_agent_control_topic(node_id, instance_id, &cmd_name);
        let reply_topic = node_agent_events_topic(node_id, instance_id);

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);

        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeControlCommand(command)),
        };
        if let Err(e) = self
            .client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
        {
            self.pending.lock().await.remove(&request_id);
            return Err(NodeControlError::Publish(e.to_string()));
        }
        tracing::debug!(topic = %topic, reply_topic = %reply_topic, request_id = %request_id, "Published node control command");

        match tokio::time::timeout(COMMAND_TIMEOUT, rx).await {
            Ok(Ok(event)) => Ok(event),
            Ok(Err(_)) => {
                // oneshot sender dropped — should not happen; treat as timeout.
                self.pending.lock().await.remove(&request_id);
                Err(NodeControlError::Timeout { request_id })
            }
            Err(_) => {
                self.pending.lock().await.remove(&request_id);
                Err(NodeControlError::Timeout { request_id })
            }
        }
    }

    /// Route an incoming NodeEvent to a pending request (called from
    /// dispatch). Returns silently if no request is waiting on this id.
    pub async fn handle_event(&self, event: NodeEvent) {
        if let Some(tx) = self.pending.lock().await.remove(&event.request_id) {
            let _ = tx.send(event);
        }
    }

    /// Start an agent Runtime on a node.
    ///
    /// ADR-073: `instance_id` scopes the control topic; `agent_id` is the
    /// package identity carried in the payload.
    pub async fn start_agent(&self, node_id: &str, instance_id: &str, agent_id: &str, dev_mode: bool) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Start(
                    acowork_core::mqtt_proto::NodeStart {
                        agent_id: agent_id.to_string(),
                        dev_mode,
                        instance_id: instance_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Stop an agent Runtime on a node.
    pub async fn stop_agent(&self, node_id: &str, instance_id: &str, agent_id: &str, reason: &str) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Stop(
                    acowork_core::mqtt_proto::NodeStop {
                        agent_id: agent_id.to_string(),
                        reason: reason.to_string(),
                        instance_id: instance_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Uninstall an agent package on a node.
    pub async fn uninstall_agent(&self, node_id: &str, instance_id: &str, agent_id: &str) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Uninstall(
                    acowork_core::mqtt_proto::NodeUninstall {
                        agent_id: agent_id.to_string(),
                        instance_id: instance_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Install an agent package on a node from a node-local spooled path.
    ///
    /// ADR-073: `instance_id` is generated by the Gateway at install
    /// time and used verbatim by the node for the on-disk landing
    /// directory `{agent_id}/{instance_id}/` and the Runtime's
    /// `--agent-instance-id`.
    pub async fn install_agent(&self, dispatch: NodeInstallDispatch<'_>) -> Result<NodeEvent, NodeControlError> {
        let (node_id, instance_id) = (dispatch.node_id.to_string(), dispatch.instance_id.to_string());
        self.send(
            &node_id,
            &instance_id,
            NodeControlCommand {
                node_id: node_id.clone(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Install(
                    dispatch.to_proto(),
                )),
            },
        )
        .await
    }

    /// Install an agent package on a node from a Gateway-hosted download
    /// URL (ADR-055 §3.2). Fire-and-forget: the install is asynchronous —
    /// the node pulls the package from the URL and lands it on its single
    /// install slot; the Gateway observes completion via the retained
    /// `installed` inventory entry (the command's NodeEvent reply still
    /// carries the `request_id` for diagnostics, but nothing blocks on it
    /// here).
    ///
    /// ADR-059 §6: `operation_id` is carried as the command's
    /// `request_id` so the node's NodeEvent reply (same id) can be
    /// correlated back to the tracked operation.
    pub async fn install_agent_by_url(
        &self,
        dispatch: NodeInstallDispatch<'_>,
    ) -> Result<(), NodeControlError> {
        let node_id = dispatch.node_id.to_string();
        // ADR-077 removed the boot-time bundled System Agent install, which was
        // the only caller passing `operation_id = ""`. The node still requires a
        // non-empty `request_id` for dedup, so keep synthesising a UUID as a
        // defence for any future caller that has no correlation id of its own.
        // Callers that DO have a real OperationId (HTTP install/ensure, CLI
        // dispatch) keep the original correlation id.
        let operation_id = if dispatch.operation_id.is_empty() {
            uuid::Uuid::new_v4().to_string()
        } else {
            dispatch.operation_id.to_string()
        };
        let instance_id = dispatch.instance_id.to_string();
        let command = NodeControlCommand {
            node_id: node_id.clone(),
            request_id: operation_id,
            command: Some(node_control_command::Command::Install(
                dispatch.to_proto(),
            )),
        };
        let topic = node_agent_control_topic(&node_id, &instance_id, "install");
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeControlCommand(command)),
        };
        self.client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
            .map_err(|e| NodeControlError::Publish(e.to_string()))
    }

    /// Clone an installed agent to a new agent ID on the source agent's
    /// node (ADR-055 §6.6 L2-5, node-local operation).
    ///
    /// `new_agent_id` may equal `agent_id` — one package can have several
    /// instances (ADR-073). `new_name` empty keeps the source name
    /// suffixed "(clone)".
    #[allow(clippy::too_many_arguments)]
    pub async fn clone_agent(
        &self,
        node_id: &str,
        instance_id: &str,
        agent_id: &str,
        new_agent_id: &str,
        mode: &str,
        new_name: &str,
    ) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::Clone(
                    acowork_core::mqtt_proto::NodeClone {
                        agent_id: agent_id.to_string(),
                        new_agent_id: new_agent_id.to_string(),
                        mode: mode.to_string(),
                        instance_id: instance_id.to_string(),
                        new_name: new_name.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Upgrade an installed agent from a Gateway-hosted download URL
    /// (ADR-055 §3.2). Fire-and-forget — completion is observed via the
    /// retained `installed` inventory entry with the new version.
    pub async fn upgrade_agent_by_url(
        &self,
        node_id: &str,
        instance_id: &str,
        agent_id: &str,
        package_url: &str,
        dev_mode: bool,
    ) -> Result<(), NodeControlError> {
        let command = NodeControlCommand {
            node_id: node_id.to_string(),
            request_id: Uuid::new_v4().to_string(),
            command: Some(node_control_command::Command::Upgrade(
                acowork_core::mqtt_proto::NodeUpgrade {
                    agent_id: agent_id.to_string(),
                    package_url: package_url.to_string(),
                    local_path: String::new(),
                    dev_mode,
                    instance_id: instance_id.to_string(),
                },
            )),
        };
        let topic = node_agent_control_topic(node_id, instance_id, "upgrade");
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeControlCommand(command)),
        };
        self.client
            .publish_envelope(&topic, &envelope, MqttQoS::AtLeastOnce, false)
            .await
            .map_err(|e| NodeControlError::Publish(e.to_string()))
    }

    /// Run publish-preparation checks on a node. The structured result is
    /// carried in `NodeEvent.result_json` (JSON).
    pub async fn publish_prepare(
        &self,
        node_id: &str,
        instance_id: &str,
        agent_id: &str,
        clean: bool,
    ) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::PublishPrepare(
                    acowork_core::mqtt_proto::NodePublishPrepare {
                        agent_id: agent_id.to_string(),
                        clean,
                        instance_id: instance_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Build (and optionally sign) a .bot package on a node. The
    /// structured result is carried in `NodeEvent.result_json` (JSON).
    pub async fn publish_build(
        &self,
        node_id: &str,
        instance_id: &str,
        agent_id: &str,
        output_dir: &str,
        sign: bool,
        key_dir: &str,
    ) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::PublishBuild(
                    acowork_core::mqtt_proto::NodePublishBuild {
                        agent_id: agent_id.to_string(),
                        output_dir: output_dir.to_string(),
                        sign,
                        key_dir: key_dir.to_string(),
                        instance_id: instance_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Import a skills ZIP on a node (path is node-local).
    pub async fn skills_import(&self, node_id: &str, instance_id: &str, agent_id: &str, zip_path: &str) -> Result<NodeEvent, NodeControlError> {
        self.send(
            node_id,
            instance_id,
            NodeControlCommand {
                node_id: node_id.to_string(),
                request_id: Uuid::new_v4().to_string(),
                command: Some(node_control_command::Command::SkillsImport(
                    acowork_core::mqtt_proto::NodeSkillsImport {
                        agent_id: agent_id.to_string(),
                        zip_path: zip_path.to_string(),
                        instance_id: instance_id.to_string(),
                    },
                )),
            },
        )
        .await
    }

    /// Check an install-style reply: the node may answer `ok` (already
    /// terminal, e.g. the package was already there) or `in_progress`
    /// (accepted onto the node's install slot, still queued or running).
    ///
    /// Any other status is a real failure. Installs are serialized on the
    /// node, so a queued reply is a success response — the caller
    /// observes completion through the retained `installed` inventory
    /// (ADR-055 §6.5) and the terminal `NodeEvent` that completes its
    /// operation.
    pub fn check_install_reply(agent_id: &str, event: &NodeEvent) -> Result<(), NodeControlError> {
        match event.status.as_str() {
            "ok" | "in_progress" => Ok(()),
            "error" | "not_implemented" => Err(NodeControlError::CommandFailed {
                agent_id: agent_id.to_string(),
                message: event.message.clone(),
            }),
            other => Err(NodeControlError::CommandFailed {
                agent_id: agent_id.to_string(),
                message: format!("unexpected status '{}': {}", other, event.message),
            }),
        }
    }

    /// Check a node command reply. `ok` is the only success status —
    /// commands that can legitimately answer `in_progress` use
    /// [`Self::check_install_reply`] instead.
    pub fn check_reply(agent_id: &str, event: &NodeEvent) -> Result<(), NodeControlError> {
        match event.status.as_str() {
            "ok" => Ok(()),
            "error" | "not_implemented" => Err(NodeControlError::CommandFailed {
                agent_id: agent_id.to_string(),
                message: event.message.clone(),
            }),
            other => Err(NodeControlError::CommandFailed {
                agent_id: agent_id.to_string(),
                message: format!("unexpected status '{}': {}", other, event.message),
            }),
        }
    }
}

/// Derive the control topic command segment from a NodeControlCommand.
fn command_name(command: &NodeControlCommand) -> String {
    match command.command.as_ref() {
        Some(node_control_command::Command::Ping(_)) => "ping",
        Some(node_control_command::Command::Start(_)) => "start",
        Some(node_control_command::Command::Stop(_)) => "stop",
        Some(node_control_command::Command::Install(_)) => "install",
        Some(node_control_command::Command::Uninstall(_)) => "uninstall",
        Some(node_control_command::Command::SkillsImport(_)) => "skills_import",
        Some(node_control_command::Command::Clone(_)) => "clone",
        Some(node_control_command::Command::Upgrade(_)) => "upgrade",
        Some(node_control_command::Command::PublishPrepare(_)) => "publish_prepare",
        Some(node_control_command::Command::PublishBuild(_)) => "publish_build",
        None => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(status: &str) -> NodeEvent {
        NodeEvent {
            node_id: "local".to_string(),
            request_id: "op-1".to_string(),
            status: status.to_string(),
            message: String::new(),
            result_json: None,
        }
    }

    /// Installs are serialized on the node, so the reply means "accepted"
    /// — a queued install is a success, and only a real failure is an
    /// error.
    #[test]
    fn install_reply_accepts_queued_and_terminal_success() {
        assert!(NodeControlClient::check_install_reply("a", &event("ok")).is_ok());
        assert!(NodeControlClient::check_install_reply("a", &event("in_progress")).is_ok());
        assert!(NodeControlClient::check_install_reply("a", &event("error")).is_err());
    }

    /// Other commands have no "accepted" state: a generic reply check
    /// must not be loosened for them.
    #[test]
    fn generic_reply_rejects_in_progress() {
        assert!(NodeControlClient::check_reply("a", &event("in_progress")).is_err());
    }

    #[test]
    fn command_name_derivation() {
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "r".to_string(),
            command: Some(node_control_command::Command::Start(Default::default())),
        };
        assert_eq!(command_name(&cmd), "start");
    }

    #[test]
    fn command_name_phase_3b_commands() {
        let cases = [
            (node_control_command::Command::Clone(Default::default()), "clone"),
            (node_control_command::Command::Upgrade(Default::default()), "upgrade"),
            (
                node_control_command::Command::PublishPrepare(Default::default()),
                "publish_prepare",
            ),
            (
                node_control_command::Command::PublishBuild(Default::default()),
                "publish_build",
            ),
        ];
        for (command, expected) in cases {
            let cmd = NodeControlCommand {
                node_id: "local".to_string(),
                request_id: "r".to_string(),
                command: Some(command),
            };
            assert_eq!(command_name(&cmd), expected);
        }
    }

    #[test]
    fn check_reply_ok() {
        let ev = NodeEvent {
            node_id: "local".to_string(),
            request_id: "r".to_string(),
            status: "ok".to_string(),
            message: "started".to_string(),
            result_json: None,
        };
        assert!(NodeControlClient::check_reply("a", &ev).is_ok());
    }

    #[test]
    fn check_reply_error() {
        let ev = NodeEvent {
            node_id: "local".to_string(),
            request_id: "r".to_string(),
            status: "error".to_string(),
            message: "agent not installed".to_string(),
            result_json: None,
        };
        assert!(NodeControlClient::check_reply("a", &ev).is_err());
    }
}
