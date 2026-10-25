//! MQTT node control plane (ADR-055 §6.2).
//!
//! Topic family (see `acowork_core::node` for the builders):
//!
//! ```text
//! acowork/nodes/{id}/status                          plain "online"/"offline"
//!                                                    Retained + LWT (QoS 1)
//! acowork/nodes/{id}/info                            NodeInfo envelope Retained
//! acowork/nodes/{id}/events                          node-level NodeEvent
//! acowork/nodes/{id}/control/{cmd}                   node-level commands (ping)
//! acowork/nodes/{id}/agents/{aid}/control/{cmd}      agent lifecycle commands
//! acowork/nodes/{id}/agents/{aid}/events             per-agent NodeEvent
//! ```
//!
//! Ownership model (ADR-033 §5): the Gateway OWNS and publishes
//! commands; the Node OWNS and publishes execution results. Every
//! command carries a `request_id` echoed in the reply; QoS 1
//! duplicates are filtered by [`dedup::RequestDedup`].

pub mod dedup;
pub mod install_exec;
pub mod mqtt;

use std::sync::Arc;

use rumqttc::{AsyncClient, QoS};
use tokio::sync::{Mutex, RwLock};

use prost::Message as _;

use acowork_core::mqtt_proto::{
    data_envelope, node_control_command, DataEnvelope, NodeControlCommand, NodeEnroll, NodeEvent,
    NodeInfo,
};
use acowork_core::node::{
    node_agent_events_topic, node_agent_installed_topic, node_enroll_topic, node_events_topic,
    node_info_topic, node_lsps_topic, node_ready_topic, node_sidecar_status_topic,
    node_status_topic, NODE_PROTOCOL_VERSION,
};
use acowork_core::AgentInstanceId;

use crate::config::{system_hostname, NodeConfig};
use crate::error::NodeError;
use crate::identity::{EnrollmentState, NodeIdentity};
use crate::process::spawn::kill_agent_process;
use crate::process::ProcessManager;
use crate::state::{NodeState, SharedNodeState};

use dedup::RequestDedup;
use mqtt::{NodeMqttClient, NodeMqttMessageCallback, SharedNodeMqttCredentials};

/// How often the daemon re-publishes the retained `info` heartbeat
/// (§6.15 — keeps `agent_count` / liveness fresh and lets the
/// Gateway detect "online but stuck" via timestamp).
const INFO_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// The node control plane daemon.
pub struct NodeControlPlane {
    config: NodeConfig,
    identity: NodeIdentity,
    state: SharedNodeState,
    client: NodeMqttClient,
}

/// Build the NodeInfo snapshot published on the retained info topic.
///
/// `advertise_host` is the *live* public host (§6.3.3): the configured
/// `advertise_host`, or — when `advertise_host_auto` is set — the LAN
/// IP detected at connect time / heartbeat. Pass the value held in
/// `state.live_advertise_host`.
pub fn build_node_info(
    identity: &NodeIdentity,
    config: &NodeConfig,
    advertise_host: &str,
    agent_count: u32,
) -> NodeInfo {
    NodeInfo {
        node_id: identity.node_id.clone(),
        node_name: identity.node_name.clone(),
        gateway_managed: identity.gateway_managed,
        hostname: system_hostname(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        node_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_version: NODE_PROTOCOL_VERSION,
        // Grows over the ADR-055 phases. Phase 2a: control plane only.
        capabilities: vec!["control_plane".to_string()],
        max_agents: config.max_agents,
        agent_count,
        // ADR-055 §6.3 / L7-1: the reverse-proxy base URL the Gateway
        // uses to reach node-local HTTP services (fs_browse).
        http_endpoint: config.proxy_advertise_endpoint_for(advertise_host),
    }
}

/// §6.3.3 self-healing: compute and store the node's *current* public
/// host. With `advertise_host_auto` the LAN IP is re-detected on every
/// (re)connect and every heartbeat (a cheap interface enumeration), so
/// a laptop that switches Wi-Fi hotspots converges on the new address
/// within one heartbeat — no node restart, no Runtime restart. Detection
/// failure falls back to the configured `advertise_host` (127.0.0.1),
/// which keeps the control plane loopback-functional. With a fixed
/// `advertise_host` (remote deployments) the configured value is used
/// verbatim and this is a pure no-op.
fn refresh_live_advertise_host<S: std::ops::Deref<Target = NodeState>>(
    state: S,
    config: &NodeConfig,
) -> String {
    let host = if config.advertise_host_auto {
        acowork_core::addr::detect_non_loopback_ipv4()
            .filter(|h| h != "127.0.0.1")
            .unwrap_or_else(|| config.advertise_host.clone())
    } else {
        config.advertise_host.clone()
    };
    let host = if host.is_empty() {
        config.advertise_host.clone()
    } else {
        host
    };
    *state
        .live_advertise_host
        .lock()
        .unwrap_or_else(|p| p.into_inner()) = host.clone();
    host
}

/// Extract the instance id from an agent-scoped control command
/// (ADR-073). Returns `None` for the node-scoped `Ping`.
fn agent_lifecycle_instance_id(command: Option<&node_control_command::Command>) -> Option<&str> {
    use node_control_command::Command;
    Some(match command? {
        Command::Start(c) => &c.instance_id,
        Command::Stop(c) => &c.instance_id,
        Command::Install(c) => &c.instance_id,
        Command::Uninstall(c) => &c.instance_id,
        Command::SkillsImport(c) => &c.instance_id,
        Command::Clone(c) => &c.instance_id,
        Command::Upgrade(c) => &c.instance_id,
        Command::PublishPrepare(c) => &c.instance_id,
        Command::PublishBuild(c) => &c.instance_id,
        Command::Ping(_) => return None,
    })
}

/// Decode an incoming envelope and dispatch a command to its handler.
///
/// Idempotent semantics per ADR-055 §6.2:
/// - `ping` → always succeeds (stateless);
/// - `start`/`stop` → already-running / already-exited return success;
/// - `install`/`uninstall`/`skills_import` → local package operations;
async fn handle_command(
    state: &SharedNodeState,
    config: &NodeConfig,
    command: &NodeControlCommand,
    // ADR-055 Phase 5a: the node's long-lived token, attached to
    // package downloads as `X-ACowork-Node-Token`. `None` when not
    // enrolled yet (downloads then work only against brokers with
    // auth disabled).
    node_token: Option<&str>,
) -> NodeEvent {
    use node_control_command::Command;

    let reply = |status: &str, message: String| NodeEvent {
        node_id: command.node_id.clone(),
        request_id: command.request_id.clone(),
        status: status.to_string(),
        message,
        result_json: None,
    };
    // Structured-result reply (publish prepare/build carry a JSON
    // payload in `result_json` for the Gateway to re-parse).
    let reply_json = |status: &str, message: String, result_json: String| NodeEvent {
        node_id: command.node_id.clone(),
        request_id: command.request_id.clone(),
        status: status.to_string(),
        message,
        result_json: Some(result_json),
    };

    // ADR-073 / security: every agent-scoped command must carry a
    // well-formed UUID instance id BEFORE it reaches any filesystem,
    // process, or topic side effect — a spoofed/truncated id would
    // otherwise key install dirs and MQTT topics arbitrarily. `Ping`
    // is node-scoped and has no instance id.
    if let Some(instance_id) = agent_lifecycle_instance_id(command.command.as_ref())
        && AgentInstanceId::from_string(instance_id.to_string()).is_err()
    {
        return reply(
            "error",
            format!(
                "invalid instance_id '{instance_id}' for '{}' (expected a valid UUID, ADR-073)",
                command.request_id
            ),
        );
    }

    match command.command.as_ref() {
        Some(Command::Ping(_)) => reply("ok", "pong".to_string()),
        Some(Command::Start(cmd)) => {
            // §6.3.3: pass the CURRENT proxy endpoint to the spawned
            // Runtime so its retained `http_endpoint` reflects the live
            // LAN IP even if the machine just changed networks.
            let live = refresh_live_advertise_host(state.read().await, config);
            let mut mgr = ProcessManager::new(
                config.log_file_size_mb,
                config.log_file_count,
                Some(config.gateway_mqtt_port),
                config.gateway_host.clone(),
                command.node_id.clone(),
                Some(config.proxy_advertise_endpoint_for(&live)),
                node_token.map(str::to_string),
            );
            match mgr
                .start_agent(&cmd.instance_id, &cmd.agent_id, state, cmd.dev_mode, true)
                .await
            {
                Ok(()) => {
                    state.read().await.save_snapshot(&config.home);
                    reply("ok", format!("started '{}'", cmd.instance_id))
                }
                Err(NodeError::AgentAlreadyRunning(id)) => {
                    // Idempotent: start on a running agent succeeds.
                    reply("ok", format!("'{}' already running", id))
                }
                Err(e) => reply(
                    "error",
                    format!("start '{}' failed: {}", cmd.instance_id, e),
                ),
            }
        }
        Some(Command::Stop(cmd)) => {
            // Same live endpoint as Start — the Runtime is told where
            // its reverse proxy lives when it (re)connects.
            let live = refresh_live_advertise_host(state.read().await, config);
            let mut mgr = ProcessManager::new(
                config.log_file_size_mb,
                config.log_file_count,
                Some(config.gateway_mqtt_port),
                config.gateway_host.clone(),
                command.node_id.clone(),
                Some(config.proxy_advertise_endpoint_for(&live)),
                node_token.map(str::to_string),
            );
            match mgr
                .stop_agent(&cmd.instance_id, &cmd.agent_id, state)
                .await
            {
                Ok(()) => {
                    state.read().await.save_snapshot(&config.home);
                    reply("ok", format!("stopped '{}'", cmd.instance_id))
                }
                Err(NodeError::AgentNotRunning(id)) => {
                    // Idempotent: stop on an exited agent succeeds.
                    reply("ok", format!("'{}' already stopped", id))
                }
                Err(e) => reply(
                    "error",
                    format!("stop '{}' failed: {}", cmd.instance_id, e),
                ),
            }
        }
        Some(Command::Uninstall(cmd)) => {
            let install_dir = config.packages_dir();
            // Scope the write lock to the synchronous mutation: the
            // retained-inventory clear publishes via the MQTT dispatcher
            // and must not run under the lock (a blocked publish would
            // stall every state reader — same class as the 261a8f77
            // reaper regression).
            let result = {
                let mut node = state.write().await;
                crate::package::uninstall::uninstall_package(
                    &cmd.instance_id,
                    &cmd.agent_id,
                    &install_dir,
                    &mut node,
                )
            };
            match result {
                Ok(()) => {
                    // Clear the retained inventory entry (ADR-055 §6.5) so
                    // the Gateway drops the agent from installed_agents.
                    // ADR-073: instance id is mandatory and UUID-validated
                    // at the command gate — no agent_id fallback.
                    let key = cmd.instance_id.clone();
                    let installed_topic = node_agent_installed_topic(&command.node_id, &key);
                    let _ = dispatcher::clear_installed_info(installed_topic).await;
                    reply("ok", format!("uninstalled '{}'", key))
                }
                Err(e) => reply("error", format!("uninstall '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::SkillsImport(cmd)) => {
            // ADR-073: instance id is mandatory and UUID-validated at the
            // command gate — the agent id is display/package identity only.
            let key = cmd.instance_id.clone();
            let skills_dir = {
                let node = state.read().await;
                match node.installed_agents.get(&key) {
                    Some(info) => crate::package::skills::agent_skills_dir(&info.install_path),
                    None => {
                        return reply(
                            "error",
                            format!("skills_import '{}': agent not installed", key),
                        );
                    }
                }
            };
            match crate::package::skills::install_skill_package(
                std::path::Path::new(&cmd.zip_path),
                &skills_dir,
            ) {
                Ok(name) => reply("ok", format!("skill '{}' imported", name)),
                Err(e) => {
                    reply("error", format!("skills_import '{}' failed: {}", key, e))
                }
            }
        }
        Some(Command::Install(cmd)) => {
            // ADR-073 install atomicity: this handler only validates the
            // request shape and hands it to the node's single install
            // slot (Android `PackageManagerService` model). The gate's
            // worker performs the install — serialized across every
            // caller — and publishes the retained inventory entry.
            //
            // Source resolution: a Gateway-hosted download URL
            // (ADR-055 §3.2, asynchronous) or a node-local spooled path
            // (Phase 2b single-machine). Both converge on the shared
            // `install_package` path inside the executor.
            if cmd.local_path.is_empty() && cmd.package_url.is_empty() {
                return reply(
                    "error",
                    format!(
                        "install '{}': neither package_url nor local_path provided",
                        cmd.agent_id
                    ),
                );
            }
            // ADR-073: the instance identity is Gateway-issued and used
            // verbatim (on-disk directory + Runtime flag), so a
            // malformed one must be refused before it can reach the
            // queue — never after.
            let instance_id = match AgentInstanceId::from_string(cmd.instance_id.clone()) {
                Ok(id) => id,
                Err(e) => {
                    return reply(
                        "error",
                        format!("install '{}': invalid instance_id: {}", cmd.agent_id, e),
                    );
                }
            };
            let Some(gate) = state.read().await.install_gate() else {
                return reply(
                    "error",
                    "install gate not started (node not connected yet)".to_string(),
                );
            };

            let source = if cmd.local_path.is_empty() {
                acowork_core::install::InstallSource::registry(&cmd.package_url)
            } else {
                acowork_core::install::InstallSource::local_file(&cmd.local_path)
            };
            // ADR-059 §6: the gateway-side operation id travels as the
            // command's `request_id`, so the queued job is correlated
            // with the operation the caller is tracking.
            let operation_id = acowork_core::operation::OperationId::from(command.request_id.clone());
            let mut request = if cmd.ensure {
                acowork_core::install::InstallRequest::ensure(
                    operation_id,
                    cmd.agent_id.clone(),
                    instance_id,
                    source,
                )
            } else {
                acowork_core::install::InstallRequest::install(
                    operation_id,
                    cmd.agent_id.clone(),
                    instance_id,
                    source,
                )
            }
            .with_dev_mode(cmd.dev_mode);

            // Declarative requests resolve the existence precondition
            // before queueing: an instance already on this node needs no
            // queue slot at all (the gate re-checks after dequeue for
            // the instances that appear while queued).
            if cmd.ensure {
                let state = state.read().await;
                if let Some(existing) = install_exec::existing_instance_of(&state, &cmd.agent_id) {
                    request = request.with_already_installed(existing);
                }
            }

            // Reply status contract (mqtt::dispatch on the Gateway):
            // `in_progress` = the job is queued on the node's single
            // install slot, `ok`/`error` = terminal. A queued install
            // must NOT answer `ok` — that would complete the caller's
            // operation before anything was installed. The terminal
            // event follows from [`install_exec::NodeInstallOutcomeSink`]
            // once the gate has processed the job.
            match gate.admit(request).await {
                acowork_core::install::AdmitOutcome::Accepted { ticket, .. } => reply(
                    "in_progress",
                    format!(
                        "install queued for '{}' (instance {})",
                        ticket.agent_id, ticket.instance_id
                    ),
                ),
                acowork_core::install::AdmitOutcome::Coalesced { ticket, .. } => reply(
                    "in_progress",
                    format!(
                        "install already in progress for '{}' (instance {})",
                        ticket.agent_id, ticket.instance_id
                    ),
                ),
                acowork_core::install::AdmitOutcome::AlreadySatisfied { instance_id } => reply(
                    "ok",
                    format!(
                        "install '{}' already satisfied by instance {}",
                        cmd.agent_id, instance_id
                    ),
                ),
            }
        }
        Some(Command::Clone(cmd)) => {
            // Node-local clone (ADR-055 §6.6 L2-5): source and new agent
            // live on the same node, so the package directory is copied
            // directly (cross-machine memory export/import is a separate
            // §6.18 concern).
            let mode = match cmd.mode.as_str() {
                "full" => crate::package::clone::CloneMode::Full,
                _ => crate::package::clone::CloneMode::Skeleton,
            };
            let install_dir = config.packages_dir();
            // Scoped write lock: the retained-inventory publish below
            // must run without the state guard (261a8f77 class).
            let result = {
                let mut node = state.write().await;
                crate::package::clone::clone_agent(
                    &cmd.instance_id,
                    &cmd.agent_id,
                    &cmd.new_agent_id,
                    &cmd.new_name,
                    mode,
                    &install_dir,
                    &mut node,
                )
            };
            match result {
                Ok(info) => {
                    // Publish retained inventory so the Gateway aggregates
                    // the new agent into installed_agents (and registers
                    // cron via the install-completed is_new hook).
                    if let Some(entry) = crate::package::build_installed_info(&info) {
                        let installed_topic =
                            node_agent_installed_topic(&command.node_id, &info.instance_id);
                        let _ = dispatcher::publish_installed_info(installed_topic, entry).await;
                    }
                    let json = serde_json::json!({ "install_path": info.install_path }).to_string();
                    reply_json(
                        "ok",
                        format!(
                            "cloned '{}' -> '{}' (instance {})",
                            cmd.agent_id, cmd.new_agent_id, info.instance_id
                        ),
                        json,
                    )
                }
                Err(e) => reply("error", format!("clone '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::Upgrade(cmd)) => {
            // ADR-055 §3.2/§6.6: upgrade from a Gateway-hosted download
            // URL or a node-local spooled path, then converge on
            // `upgrade_package`.
            let install_dir = config.packages_dir();
            let mut spooled: Option<std::path::PathBuf> = None;
            let source_path: std::path::PathBuf = if cmd.local_path.is_empty() {
                if cmd.package_url.is_empty() {
                    return reply(
                        "error",
                        format!(
                            "upgrade '{}': neither package_url nor local_path provided",
                            cmd.agent_id
                        ),
                    );
                }
                let tmp = std::env::temp_dir().join(format!(
                    "acowork-node-upgrade-{}-{}.agent",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                ));
                match download_package(&cmd.package_url, &tmp, node_token).await {
                    Ok(()) => {
                        spooled = Some(tmp.clone());
                        tmp
                    }
                    Err(e) => {
                        return reply(
                            "error",
                            format!("upgrade '{}' download failed: {}", cmd.agent_id, e),
                        );
                    }
                }
            } else {
                std::path::PathBuf::from(&cmd.local_path)
            };

            // Scoped write lock; the upgraded entry is snapshotted under
            // the guard and the retained-inventory re-publish runs
            // outside it (261a8f77 lock-discipline class).
            let (result, upgraded_info) = {
                let mut node = state.write().await;
                let result = crate::package::upgrade::upgrade_package(
                    &cmd.instance_id,
                    &cmd.agent_id,
                    &source_path,
                    &install_dir,
                    &mut node,
                    cmd.dev_mode,
                );
                if let Some(tmp) = spooled {
                    let _ = std::fs::remove_file(&tmp);
                }
                // ADR-073: instance id is mandatory and UUID-validated at
                // the command gate — no agent_id fallback for the key.
                let key = cmd.instance_id.clone();
                let upgraded_info = node.installed_agents.get(&key).cloned();
                (result, upgraded_info)
            };

            match result {
                Ok(()) => {
                    // Republish retained inventory with the new version so
                    // the Gateway refreshes its installed_agents entry.
                    if let Some(info) = upgraded_info
                        && let Some(entry) = crate::package::build_installed_info(&info)
                    {
                        let installed_topic =
                            node_agent_installed_topic(&command.node_id, &info.instance_id);
                        let _ = dispatcher::publish_installed_info(installed_topic, entry).await;
                    }
                    reply("ok", format!("upgraded '{}'", cmd.agent_id))
                }
                Err(e) => reply("error", format!("upgrade '{}' failed: {}", cmd.agent_id, e)),
            }
        }
        Some(Command::PublishPrepare(cmd)) => {
            let mut node = state.write().await;
            match crate::package::publish::prepare_publish(
                &cmd.instance_id,
                cmd.clean,
                &mut node,
            ) {
                Ok(result) => {
                    let json = serde_json::to_string(&result).unwrap_or_else(|e| {
                        format!(r#"{{"error":"{}"}}"#, e)
                    });
                    reply_json(
                        "ok",
                        format!("publish prepare '{}' complete", cmd.instance_id),
                        json,
                    )
                }
                Err(e) => {
                    reply(
                        "error",
                        format!("publish prepare '{}' failed: {}", cmd.instance_id, e),
                    )
                }
            }
        }
        Some(Command::PublishBuild(cmd)) => {
            let output_dir = if cmd.output_dir.is_empty() {
                config.packages_dir()
            } else {
                std::path::PathBuf::from(&cmd.output_dir)
            };
            let key_dir = if cmd.key_dir.is_empty() {
                None
            } else {
                Some(std::path::Path::new(&cmd.key_dir))
            };
            let node = state.read().await;
            match crate::package::publish::build_package(
                &cmd.instance_id,
                &cmd.agent_id,
                &output_dir,
                cmd.sign,
                key_dir,
                &node,
            ) {
                Ok(result) => {
                    let json = serde_json::to_string(&result).unwrap_or_default();
                    reply_json("ok", format!("built '{}'", result.output_path), json)
                }
                Err(e) => {
                    reply("error", format!("publish build '{}' failed: {}", cmd.instance_id, e))
                }
            }
        }
        None => reply("error", "empty command payload".to_string()),
    }
}

/// Download a package from the Gateway registry URL to a local temp
/// path (ADR-055 §3.2). The caller owns cleanup of `dest`.
///
/// ADR-055 Phase 5a: when the node holds a node_token it is attached
/// as `X-ACowork-Node-Token` — the Gateway's package channel is
/// gated on that header when `mqtt.auth_enabled` is on.
async fn download_package(
    url: &str,
    dest: &std::path::Path,
    node_token: Option<&str>,
) -> Result<(), NodeError> {
    let mut request = reqwest::Client::new().get(url);
    if let Some(token) = node_token {
        request = request.header("X-ACowork-Node-Token", token);
    }
    let bytes = request
        .send()
        .await
        .map_err(|e| NodeError::Mqtt(format!("download GET failed: {e}")))?
        .error_for_status()
        .map_err(|e| NodeError::Package(format!("download HTTP error: {e}")))?
        .bytes()
        .await
        .map_err(|e| NodeError::Package(format!("download read failed: {e}")))?;
    std::fs::write(dest, &bytes)
        .map_err(|e| NodeError::Package(format!("download write failed: {e}")))?;
    Ok(())
}

/// ADR-055 Phase 5a: build the NodeEnroll request envelope.
///
/// Enrollment is idempotent and re-run on every (re)connect: a node
/// that already holds a Gateway-issued node_token presents it as the
/// enrollment credential, so the Gateway can re-sync its store (e.g.
/// after its `node_tokens.json` was lost) — the Gateway mints a fresh
/// token in that case and the `enroll_result` reply updates the local
/// identity. A one-shot-only enrollment could otherwise strand a node
/// with a token the Gateway no longer recognizes, turning every
/// reverse-proxied request into a 403 "invalid node token".
///
/// A node with NO credential (no `--token`, no persisted node_token)
/// still sends the request with an empty `enrollment_token` instead
/// of staying silent: with `mqtt.auth_enabled = false` the Gateway
/// skips token validation (ADR-055 §6.8) and mints the first
/// long-lived token, which is what lets the fail-closed node reverse
/// proxy open for the Gateway. With auth on the Gateway answers
/// `rejected` ("enrollment token required"); the node keeps failing
/// closed, matching the pre-Phase-5a behavior.
fn build_enroll_payload(identity: &NodeIdentity, config: &NodeConfig) -> DataEnvelope {
    // The one-time enrollment token (first boot, passed via CLI) wins;
    // a persisted node_token doubles as the credential on re-enroll.
    let token = config
        .token
        .as_deref()
        .filter(|t| !t.is_empty())
        .or(identity.node_token.as_deref())
        .unwrap_or_default();
    let info = build_node_info(identity, config, &config.advertise_host, 0);
    let enroll = NodeEnroll {
        node_id: identity.node_id.clone(),
        os: info.os,
        arch: info.arch,
        node_version: info.node_version,
        protocol_version: info.protocol_version,
        capabilities: info.capabilities,
        enrollment_token: token.to_string(),
    };
    DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::NodeEnroll(enroll)),
    }
}

/// Publish the enrollment request on `acowork/nodes/{id}/enroll`
/// (QoS 1, non-retained). Returns true when the request was actually
/// published. Re-run on every (re)connect by the bootstrap —
/// idempotent: the Gateway reuses the node's token when the presented
/// credential still matches (see `build_enroll_payload`).
async fn publish_enroll(
    client: &AsyncClient,
    identity: &NodeIdentity,
    config: &NodeConfig,
) -> bool {
    let envelope = build_enroll_payload(identity, config);
    let topic = node_enroll_topic(&identity.node_id);
    match client
        .publish(
            topic.clone(),
            QoS::AtLeastOnce,
            false,
            prost::Message::encode_to_vec(&envelope),
        )
        .await
    {
        Ok(()) => {
            tracing::info!(topic = %topic, "Node enrollment request published");
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, topic = %topic, "Failed to publish enrollment request");
            false
        }
    }
}

/// Handle an `acowork/nodes/{id}/enroll_result` reply: persist the
/// Gateway-issued node_token into identity.json. Returns true when
/// the topic was an enroll_result (consumed).
///
/// Idempotent (ADR-055 §6.12): a same-token reply is a no-op. When
/// the Gateway mints a NEW token (its own `node_tokens.json` was
/// lost between restarts, so a re-enroll could not reuse the old
/// one), the reply overwrites the local token — the Gateway is the
/// authority, and only `status=ok` + non-empty replies reach this
/// point, so a stale/error reply can never clear or corrupt the
/// stored credential.
async fn handle_enroll_result(
    topic: &str,
    payload: &[u8],
    identity: &Arc<RwLock<NodeIdentity>>,
    home: &std::path::Path,
) -> bool {
    let Some(node_id) = topic
        .strip_prefix("acowork/nodes/")
        .and_then(|rest| rest.strip_suffix("/enroll_result"))
    else {
        return false;
    };
    let Ok(envelope) = DataEnvelope::decode(payload) else {
        tracing::warn!(topic, "Undecodable enroll_result payload");
        return true;
    };
    let Some(data_envelope::Payload::NodeEnrollResult(result)) = envelope.payload else {
        return true;
    };
    if result.node_id != node_id {
        tracing::warn!(
            topic,
            result_node = %result.node_id,
            "enroll_result node_id does not match topic"
        );
        return true;
    }
    if result.status != "ok" || result.node_token.is_empty() {
        tracing::warn!(
            topic,
            status = %result.status,
            message = %result.message,
            "Enrollment rejected by Gateway — no node token issued"
        );
        return true;
    }
    let mut id = identity.write().await;
    if id.node_token.as_deref() == Some(result.node_token.as_str()) {
        return true; // Same token — nothing to do.
    }
    id.set_node_token(Some(result.node_token.clone()));
    match id.save(home) {
        Ok(()) => tracing::info!(
            node_id = %id.node_id,
            "Node token persisted to identity.json"
        ),
        Err(e) => tracing::warn!(error = %e, "Failed to persist node_token to identity.json"),
    }
    true
}

/// Extract (node_id, Some(instance_id)) from an agent-level control
/// topic or (node_id, None) from a node-level control topic. Returns
/// `None` for topics outside the control family.
///
/// ADR-073: the topic variable under `agents/` is the INSTANCE identity
/// (`nodes/{node}/agents/{instance_id}/control/{cmd}`) and MUST parse
/// as a UUIDv4 — package-id topics are rejected (the UUID validation
/// gate at the command entry will reject them anyway).
fn parse_control_topic(topic: &str, own_node_id: &str) -> Option<(String, Option<String>)> {
    let agent_prefix = format!("acowork/nodes/{own_node_id}/agents/");
    let node_prefix = format!("acowork/nodes/{own_node_id}/control/");
    if let Some(rest) = topic.strip_prefix(&agent_prefix) {
        // rest = {instance_id}/control/{cmd}
        //
        // Routing decision only — we intentionally do NOT validate the
        // UUID shape of `instance_id` here. Doing so would silently
        // drop malformed agent-scoped commands and prevent the
        // handler from emitting an error reply, defeating ADR-073's
        // "reject before any side effect" contract (every handler
        // re-validates `instance_id` via `AgentInstanceId::from_string`
        // before touching FS / process / queue).
        let mut parts = rest.splitn(3, '/');
        let instance_id = parts.next().unwrap_or("");
        let control = parts.next().unwrap_or("");
        if !instance_id.is_empty() && control == "control" {
            return Some((own_node_id.to_string(), Some(instance_id.to_string())));
        }
        return None;
    }
    if topic.strip_prefix(&node_prefix).is_some() {
        return Some((own_node_id.to_string(), None));
    }
    None
}

impl NodeControlPlane {
    /// Run the daemon: connect, bootstrap on every (re)connect,
    /// answer control commands, republish the info heartbeat, and
    /// shut down cleanly on Ctrl-C.
    pub async fn run(config: NodeConfig) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let gateway_addr = config.gateway_addr();
        let identity = NodeIdentity::load_or_create(
            &config.home,
            config.name.as_deref(),
            Some(&gateway_addr),
            config.gateway_managed,
        )?;
        let node_id = identity.node_id.clone();
        tracing::info!(
            node_id = %node_id,
            gateway = %gateway_addr,
            "Node Agent starting"
        );

        let node_state = {
            let mut s = crate::state::NodeState::new(config.max_agents);
            // Rebuild the local install table from the packages dir so a
            // restart re-discovers installed agents (ADR-055 §6.5).
            crate::package::restore_installed_agents(&mut s, &config.packages_dir());
            s
        };
        let state: SharedNodeState = Arc::new(RwLock::new(node_state));
        let dedup: Arc<Mutex<RequestDedup>> = Arc::new(Mutex::new(RequestDedup::default()));

        // ADR-055 §6.19: re-adopt orphan Runtimes left running after a
        // Node restart (a Node crash does NOT kill Runtimes, §6.10).
        // Runs AFTER the install table is restored (the adoption gate)
        // and BEFORE the reverse proxy starts, so `/agents/{id}/*`
        // routes immediately; it is also before the control-topic
        // subscription (which happens on ConnAck), so no start/stop
        // command races the reconciliation window (§6.19 point 5).
        let readopted = {
            let mgr = ProcessManager::new(
                config.log_file_size_mb,
                config.log_file_count,
                Some(config.gateway_mqtt_port),
                config.gateway_host.clone(),
                node_id.clone(),
                Some(config.proxy_advertise_endpoint()),
                identity.node_token.clone(),
            );
            mgr.readopt_orphans(&state).await
        };
        let readopted: Arc<Mutex<Option<Vec<String>>>> =
            Arc::new(Mutex::new(Some(readopted)));

        // Bootstrap (re-run on every ConnAck — clean_session means the
        // broker dropped our subscriptions on disconnect): publish
        // retained status=online + info, re-subscribe control filters,
        // mark identity enrolled, refresh the snapshot file.
        let bs_node_id = node_id.clone();
        let bs_config = config.clone();
        let bs_state = state.clone();
        let bs_identity = Arc::new(RwLock::new(identity.clone()));
        let bs_home = config.home.clone();
        let bs_gateway_addr = gateway_addr.clone();
        let bs_readopted = readopted.clone();
        // Cloned BEFORE the bootstrap closure moves it in — the
        // message callback (created after connect) also needs it.
        let dispatch_identity = bs_identity.clone();
        // The node HTTP server (proxy auth, Phase 5a) also reads the
        // live identity — cloned before message_callback moves it.
        let http_identity = bs_identity.clone();
        // The install executor reads the token per download, so it needs
        // its own handle (cloned before the bootstrap closure takes one).
        let install_identity = bs_identity.clone();
        // So does the memory migration pass: the credential is re-read per
        // retry round, because a first boot mints it WHILE the pass waits.
        let migration_identity = bs_identity.clone();

        // ADR-055 Phase 5a: live CONNECT credential — starts as the
        // node_token (reconnect) or the enrollment token (first boot)
        // and is swapped to the node_token when the enroll_result
        // reply arrives (a consumed enrollment token would be rejected
        // on reconnect).
        let mqtt_credentials: SharedNodeMqttCredentials = Arc::new(Mutex::new(
            identity
                .node_token
                .clone()
                .or_else(|| config.token.clone()),
        ));
        let bs_credentials = mqtt_credentials.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let node_id = bs_node_id.clone();
            let config = bs_config.clone();
            let state = bs_state.clone();
            let identity = bs_identity.clone();
            let home = bs_home.clone();
            let gateway_addr = bs_gateway_addr.clone();
            let readopted = bs_readopted.clone();
            tokio::spawn(async move {
                let status_topic = node_status_topic(&node_id);
                let info_topic = node_info_topic(&node_id);

                if let Err(e) = client
                    .publish(
                        status_topic.clone(),
                        QoS::AtLeastOnce,
                        true,
                        "online".as_bytes(),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to publish node status=online");
                }

                // §6.3.3: re-detect the live LAN IP on every (re)connect
                // and store it in shared state, so Start/Stop/heartbeat
                // publish NodeInfo with the CURRENT address (a Wi-Fi
                // hotspot switch is healed here without a restart).
                let live_host = refresh_live_advertise_host(state.read().await, &config);
                let agent_count = state.read().await.agents.len() as u32;
                let info = build_node_info(
                    &identity.read().await.clone(),
                    &config,
                    &live_host,
                    agent_count,
                );
                let envelope = DataEnvelope {
                    version: 1,
                    payload: Some(data_envelope::Payload::NodeInfo(info)),
                };
                if let Err(e) = client
                    .publish(
                        info_topic,
                        QoS::AtLeastOnce,
                        true,
                        prost::Message::encode_to_vec(&envelope),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to publish node info");
                }

                // ADR-055 §6.5: publish the retained installed-package
                // inventory so the Gateway can rebuild its installed_agents
                // view without scanning the packages dir (L2-9). Re-run on
                // every (re)connect because clean_session drops the broker's
                // retained set on a fresh broker.
                let installed: Vec<_> = state
                    .read()
                    .await
                    .installed_agents
                    .values()
                    .cloned()
                    .collect();
                for entry in installed {
                    let Some(info) = crate::package::build_installed_info(&entry) else {
                        continue;
                    };
                    let installed_topic = node_agent_installed_topic(&node_id, &entry.instance_id);
                    let envelope = DataEnvelope {
                        version: 1,
                        payload: Some(data_envelope::Payload::InstalledAgentInfo(info)),
                    };
                    if let Err(e) = client
                        .publish(
                            installed_topic,
                            QoS::AtLeastOnce,
                            true,
                            prost::Message::encode_to_vec(&envelope),
                        )
                        .await
                    {
                        tracing::warn!(error = %e, "Failed to publish installed-agent info");
                    }
                }

                // ADR-055 §6.7: re-assert the retained per-node LSP
                // relay endpoint on every (re)connect — clean_session
                // drops the broker's retained set on a fresh broker,
                // and the sidecar supervisor's transition publishes may
                // have happened before this ConnAck. Ready state comes
                // from the shared process state (None → unavailable).
                let lsp_ready = state
                    .read()
                    .await
                    .lsp_relay_process
                    .as_ref()
                    .map(|p| p.ready)
                    .unwrap_or(false);
                if let Err(e) = crate::sidecar::publish_lsps_state(
                    &client,
                    &node_id,
                    &config.advertise_host,
                    config.lsp_relay_port,
                    lsp_ready,
                )
                .await
                {
                    tracing::warn!(error = %e, "Failed to publish node LSP relay state");
                }

                // ADR-055 Phase 5a: request a long-lived node token.
                // Idempotent on every (re)connect — a node that already
                // holds a token presents it so the Gateway can re-sync
                // its store (one-shot enrollment would strand a node
                // with an orphaned token after a Gateway store loss).
                // Snapshot first: an argument temporary (guard) would
                // otherwise live across the awaited publish and stall
                // the identity writer (261a8f77 lock-discipline class).
                let identity_snapshot = identity.read().await.clone();
                publish_enroll(&client, &identity_snapshot, &config).await;

                // Control subscriptions: node-level + agent-level.
                for filter in [
                    format!("acowork/nodes/{node_id}/control/#"),
                    format!("acowork/nodes/{node_id}/agents/+/control/#"),
                    // ADR-055 Phase 5a: enrollment reply (own node).
                    format!("acowork/nodes/{node_id}/enroll_result"),
                ] {
                    if let Err(e) = client.subscribe(filter, QoS::AtLeastOnce).await {
                        tracing::warn!(error = %e, "Failed to subscribe node control filter");
                    }
                }

                // ADR-059 §7.2: NodeReady — the control-plane readiness
                // signal. Published AFTER CONNECT + every control
                // subscription is submitted (rumqttc queues in order, so
                // the broker sees the SUBSCRIBEs before this PUBLISH).
                // Retained so a Gateway (re)start converges without
                // polling; re-published on every (re)connect. Only the
                // two protocol fields travel — no generation counters
                // (ADR-059 §7.2 OCP boundary).
                let ready_topic = acowork_core::node::node_ready_topic(&node_id);
                let ready_envelope = DataEnvelope {
                    version: 1,
                    payload: Some(data_envelope::Payload::NodeReady(
                        acowork_core::mqtt_proto::NodeReady {
                            node_id: node_id.clone(),
                            protocol_version: 1,
                        },
                    )),
                };
                if let Err(e) = client
                    .publish(
                        ready_topic,
                        QoS::AtLeastOnce,
                        true,
                        prost::Message::encode_to_vec(&ready_envelope),
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to publish NodeReady (retained)");
                }

                // Mark enrolled (idempotent) + persist.
                {
                    let mut id = identity.write().await;
                    id.mark_enrolled(&gateway_addr);
                    let _ = id.save(&home);
                }
                {
                    let mut s = state.write().await;
                    s.set_connected(true, Some(gateway_addr));
                    s.save_snapshot(&home);
                }
                // ADR-055 §6.19: emit the `node_readopted` diagnostic
                // event once, if this boot re-adopted any orphans. The
                // Runtime's own MQTT reconnect + retained `http_endpoint`
                // replay already converges the Gateway (§6.19 point 6);
                // this event is for observability only.
                if let Ok(mut guard) = readopted.try_lock()
                    && let Some(ids) = guard.take()
                    && !ids.is_empty()
                {
                    let event = NodeEvent {
                        node_id: node_id.clone(),
                        request_id: "node_readopted".to_string(),
                        status: "ok".to_string(),
                        message: format!(
                            "re-adopted {} orphan Runtime(s): {}",
                            ids.len(),
                            ids.join(", ")
                        ),
                        result_json: None,
                    };
                    let envelope = DataEnvelope {
                        version: 1,
                        payload: Some(data_envelope::Payload::NodeEvent(event)),
                    };
                    let _ = client
                        .publish(
                            node_events_topic(&node_id),
                            QoS::AtLeastOnce,
                            false,
                            prost::Message::encode_to_vec(&envelope),
                        )
                        .await;
                }
                tracing::info!(node_id = %node_id, "Node bootstrap complete");
            });
        });

        // Incoming control command dispatch.
        let dispatch_node_id = node_id.clone();
        let dispatch_dedup = dedup.clone();
        let dispatch_state = state.clone();
        let dispatch_config = config.clone();
        let dispatch_home = config.home.clone();
        let dispatch_credentials = bs_credentials.clone();
        let message_callback: NodeMqttMessageCallback = Arc::new(move |topic, payload| {
            let node_id = dispatch_node_id.clone();
            let dedup = dispatch_dedup.clone();
            let state = dispatch_state.clone();
            let config = dispatch_config.clone();
            let identity = dispatch_identity.clone();
            let home = dispatch_home.clone();
            let credentials = dispatch_credentials.clone();
            tokio::spawn(async move {
                // ADR-055 Phase 5a: enrollment reply — persist the
                // Gateway-issued node_token into identity.json and
                // switch the live CONNECT credential so a reconnect
                // never re-presents the (now consumed) enrollment
                // token.
                if handle_enroll_result(&topic, &payload, &identity, &home).await {
                    // Snapshot the token before the awaited credential
                    // swap: a guard held across await (if-let scrutinee
                    // temporary) would stall the identity writer
                    // (261a8f77 lock-discipline class).
                    let token = identity.read().await.node_token.clone();
                    if let Some(token) = token {
                        *credentials.lock().await = Some(token);
                    }
                    return;
                }
                if let Err(e) = handle_incoming(
                    &node_id,
                    &topic,
                    &payload,
                    &dedup,
                    &state,
                    &config,
                    &identity,
                )
                .await
                {
                    tracing::warn!(topic = %topic, error = %e, "Failed to process node control message");
                }
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &node_id,
            mqtt_credentials,
            // ADR-059 §7.2: poll-task disconnect clears this retained
            // snapshot (daemon only — the one-shot enrollment path
            // below passes None: it never announced NodeReady).
            Some(node_ready_topic(&node_id)),
            bootstrap,
            Some(message_callback),
        )
        .await?;

        // Route command replies through this connection.
        dispatcher::install(client.shared_handle());

        // ADR-073 install atomicity: one install slot per node. Started
        // here rather than in `NodeState::new()` because the gate's
        // worker publishes retained inventory — it needs the client
        // created above. Until this runs, an install command is refused
        // with a clear error instead of being silently queued.
        {
            let executor: Arc<dyn crate::package::install_gate::InstallExecutor> =
                Arc::new(install_exec::NodeInstallExecutor::new(
                    state.clone(),
                    &config,
                    node_id.clone(),
                    install_identity,
                ));
            // The sink completes the caller's operation once the slot has
            // processed the job — the immediate reply only says "queued".
            let sink: Arc<dyn crate::package::install_gate::InstallOutcomeSink> =
                Arc::new(install_exec::NodeInstallOutcomeSink::new(node_id.clone()));
            let gate = crate::package::install_gate::InstallGate::new();
            state.write().await.set_install_gate(gate.clone());
            tokio::spawn(async move { gate.run_worker(executor, sink).await });
        }

        // ADR-101 decision 10: hand the memories that still live in each
        // instance's workspace store over to the memory service, once. A
        // background task — the Gateway spawns that service as one of its own
        // startup steps, so the pass waits for it (bounded retry) instead of
        // delaying the control plane, and an instance already dealt with is
        // recognised from its `.migrated` sentinel.
        {
            let migration_state = state.clone();
            let migration_config = config.clone();
            tokio::spawn(crate::memory_migration::startup_pass(
                migration_state,
                migration_config,
                migration_identity,
            ));
        }

        // ADR-055 §6.4: start the node reverse proxy (`:19900`) so the
        // Gateway reaches every local Runtime through one port. It is a
        // best-effort service — if the port is taken (e.g. another node
        // instance) the control plane still runs, only HTTP reverse
        // proxying is disabled.
        let proxy_state = state.clone();
        let proxy_bind = format!("{}:{}", config.proxy_bind, config.proxy_port);
        let proxy_node_id = node_id.clone();
        // The rename route (ADR-075 D4) writes `identity.json` under
        // `config.home` and rebuilds the info snapshot with it.
        let proxy_config = config.clone();

        // ── ADR-091: LSP registry + relay control ──
        // Load + merge + persist BEFORE any route or relay child can read the
        // registry, so the first boot is what creates
        // `{node_home}/lsp_servers.json` (§5.6 steps 1-2).
        let lsp_asset_dir = acowork_core::lsp::lsp_asset_dir_from_env();
        let lsp_registry =
            crate::lsp_registry::LspRegistry::load(&config.home, lsp_asset_dir.clone()).await;
        // ADR-089 D1: spawn and probe share one bind host. ADR-091 D16: the
        // runtime dir and the preset asset dir are two separate variables.
        let lsp_bind_host = crate::sidecar::lsp_relay::relay_bind_host(
            config.expose_lsp_relay,
            &config.advertise_host,
        );
        let lsp_dirs = crate::sidecar::lsp_relay::LspRelayDirs {
            config_dir: config.home.clone(),
            asset_dir: lsp_asset_dir,
        };
        // Config-restart channel (§5.6): the write routes send requests, the
        // supervisor owns the receiver and debounces them.
        let (restart_tx, restart_rx) = tokio::sync::mpsc::unbounded_channel();
        let lsp_relay = crate::state::LspRelayControl {
            bind_host: lsp_bind_host.clone(),
            port: config.lsp_relay_port,
            restart: restart_tx,
        };
        let lsp_registry_for_supervisor = lsp_registry.clone();
        // Signals that the node HTTP server (hosting `/health`) is
        // bound — the LSP relay sidecar start waits for this so the
        // relay's parent-health watchdog has a live target from birth.
        let (health_tx, health_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            // ADR-055 §6.4 + L7-1: the node HTTP server hosts the
            // `/agents/{id}/*` reverse proxy, the `/fs/browse` remote
            // filesystem browser and (ADR-009 §5) the read-only agent
            // asset service on the same `:19900` listener.
            // ADR-055 Phase 5a §6.8: the node HTTP router carries the
            // live identity so the proxy can validate inbound
            // `X-ACowork-Node-Token` against the issued node_token.
            let node_http_state = crate::state::NodeHttpState {
                node: proxy_state.clone(),
                identity: http_identity,
                config: proxy_config,
                lsp: lsp_registry,
                lsp_relay,
            };
            let app = crate::proxy::router(node_http_state.clone())
                .merge(crate::fs_browse::router(node_http_state.clone()))
                .merge(crate::package_http::router(node_http_state.clone()))
                .merge(crate::node_api::router(node_http_state.clone()))
                // ADR-091 §5.5: the eight `/lsp/...` registry routes.
                .merge(crate::lsp_api::router(node_http_state));
            let listener = match tokio::net::TcpListener::bind(&proxy_bind).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        addr = %proxy_bind,
                        error = %e,
                        "Node reverse proxy failed to bind — agent HTTP reverse-proxy disabled"
                    );
                    return;
                }
            };
            tracing::info!(
                addr = %proxy_bind,
                node_id = %proxy_node_id,
                "Node reverse proxy listening"
            );
            let _ = health_tx.send(());
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!(error = %e, "Node reverse proxy terminated");
            }
        });

        // ADR-055 §6.7: start the node-local LSP relay sidecar once the
        // node HTTP server is up (its `/health` is the relay's
        // self-exit watchdog target). Best-effort — a failed bind
        // leaves the node running without codebase tooling.
        if health_rx.await.is_ok() {
            let sup_cfg = crate::sidecar::lsp_relay_supervisor::LspRelaySupervisorConfig {
                data_dir: config.home.clone(),
                port: config.lsp_relay_port,
                health_url: format!("http://127.0.0.1:{}/health", config.proxy_port),
                node_id: node_id.clone(),
                advertise_host: config.advertise_host.clone(),
                // ADR-089 D1: bind + probe share one host (loopback
                // unless the operator set --expose-lsp-relay).
                bind_host: lsp_bind_host,
                // ADR-091 D16: injected on the relay command, not inherited.
                dirs: lsp_dirs,
                lsp_registry: lsp_registry_for_supervisor,
            };
            crate::sidecar::lsp_relay_supervisor::start_lsp_relay_supervisor(
                sup_cfg,
                state.clone(),
                restart_rx,
            );
        }

        // Process-level self-watchdog: probe our own `/health` from a
        // plain OS thread (independent of the tokio runtime) so a hung
        // HTTP accept loop still gets detected — see `spawn_http_watchdog`
        // for the incident background.
        spawn_http_watchdog(config.proxy_port);

        let plane = Self {
            config,
            identity,
            state,
            client,
        };

        plane.run_loop().await
    }
}

/// Spawn a process-level watchdog thread that probes the node's own
/// `/health` endpoint with blocking sockets.
///
/// Background (2026-08 incident): after a long idle period the axum
/// accept loop can hang while the kernel still completes TCP
/// handshakes — connects succeed but no response ever arrives, leaving
/// every Gateway reverse-proxy request stuck at the 30 s client
/// timeout. Because the node HTTP server shares one tokio runtime with
/// the rest of the node, an in-runtime watchdog would hang too; a
/// plain OS thread with blocking I/O keeps ticking, so the node can
/// exit and let the Gateway supervisor (`node_manager.rs`) respawn it.
fn spawn_http_watchdog(proxy_port: u16) {
    if let Err(e) = std::thread::Builder::new()
        .name("http-watchdog".to_string())
        .spawn(move || {
            const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
            const MAX_CONSECUTIVE_FAILURES: u32 = 3;
            let mut failures: u32 = 0;
            loop {
                std::thread::sleep(PROBE_INTERVAL);
                match probe_health(proxy_port) {
                    Ok(()) => failures = 0,
                    Err(e) => {
                        failures += 1;
                        tracing::warn!(failures, error = %e, "Node /health probe failed");
                        if failures >= MAX_CONSECUTIVE_FAILURES {
                            tracing::error!(
                                "Node HTTP server unresponsive — exiting so the Gateway respawns the node"
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        })
    {
        tracing::warn!(error = %e, "Failed to spawn node HTTP watchdog");
    }
}

/// Probe `http://127.0.0.1:{port}/health` with a blocking socket.
///
/// A successful TCP connect is NOT sufficient — the hang symptom is
/// connects succeeding with no application-level response — so a probe
/// only passes when an `HTTP/1.x 200` response line arrives within the
/// read timeout.
fn probe_health(port: u16) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(3))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(3)))?;
    stream.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")?;
    let mut buf = [0u8; 64];
    let n = stream.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("unexpected /health response: {head:?}"),
        ))
    }
}

impl NodeControlPlane {
    /// Main loop: info heartbeat + sleep/wake recovery + graceful
    /// shutdown. HTTP server responsiveness is monitored out-of-band
    /// by `spawn_http_watchdog` (separate OS thread — see the 2026-08
    /// incident comment there).
    async fn run_loop(mut self) -> Result<(), NodeError> {
        let mut heartbeat = tokio::time::interval(INFO_HEARTBEAT_INTERVAL);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // ADR-065 Step 1: sleep/wake recovery runs as an independent task
        // using the shared `run_power_probe_loop` (2 s interval, same as
        // Desktop / Runtime). System sleep freezes the whole process; after
        // wake the MQTT connection is stale (the broker timed it out while
        // we slept) and the poll task can stall until its watchdog fires
        // (2026-08 incident: 5 sleep/wake cycles, each stalling MQTT + relay
        // heartbeats for the full sleep duration). Force a fresh client +
        // EventLoop immediately instead.
        let client = self.client.clone();
        tokio::spawn(async move {
            acowork_mqtt_session::power::run_power_probe_loop(
                move || client.force_reconnect(),
                acowork_mqtt_session::power::POWER_PROBE_INTERVAL,
                "node",
            )
            .await;
        });

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    // ADR-075 D4: converge a display-name rename performed
                    // while the daemon keeps running — identity.json is
                    // the source of truth; `node_id` never changes.
                    if let Ok(Some(disk)) = NodeIdentity::load(&self.config.home)
                        && disk.node_name != self.identity.node_name
                    {
                        tracing::info!(
                            old = %self.identity.node_name,
                            new = %disk.node_name,
                            "Node display name updated from identity.json"
                        );
                        self.identity.node_name = disk.node_name;
                    }
                    // §6.3.3: refresh the live host on every heartbeat so
                    // a network change converges within one interval.
                    let live_host =
                        refresh_live_advertise_host(self.state.read().await, &self.config);
                    let agent_count = self.state.read().await.agents.len() as u32;
                    let info = build_node_info(
                        &self.identity,
                        &self.config,
                        &live_host,
                        agent_count,
                    );
                    let envelope = DataEnvelope {
                        version: 1,
                        payload: Some(data_envelope::Payload::NodeInfo(info)),
                    };
                    let topic = node_info_topic(&self.identity.node_id);
                    if let Err(e) = self
                        .client
                        .publish_envelope(&topic, &envelope, QoS::AtLeastOnce, true)
                        .await
                    {
                        tracing::warn!(error = %e, "Info heartbeat publish failed");
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutdown signal received — going offline");
                    self.shutdown().await;
                    return Ok(());
                }
            }
        }
    }

    /// Graceful shutdown: terminate all hosted Runtime processes
    /// (ADR-055 §6.10 — the ADR-018 cleanup migrated into the Node),
    /// publish retained status=offline (the LWT covers the crash path),
    /// refresh the snapshot, persist identity.
    async fn shutdown(&mut self) {
        // Terminate every tracked Runtime before we go offline. SIGTERM
        // lets each Runtime perform its own graceful exit (idle-sleep
        // bookkeeping, LWT); the broker emits their retained "offline"
        // on disconnect.
        let pids: Vec<u32> = {
            self.state
                .read()
                .await
                .agents
                .values()
                .map(|s| s.pid)
                .collect()
        };
        for pid in pids {
            if let Err(e) = kill_agent_process(pid).await {
                tracing::warn!(
                    pid,
                    error = %e,
                    "Failed to terminate Runtime during node shutdown"
                );
            }
        }
        // Clear the process table so the persisted snapshot does not
        // record running agents that we just terminated.
        self.state.write().await.agents.clear();

        // ADR-055 §6.7: terminate the node-local LSP relay sidecar and
        // clear its retained state (the relay would otherwise linger
        // until its parent-health watchdog notices the node /health is
        // gone).
        let lsp_pid = self
            .state
            .read()
            .await
            .lsp_relay_process
            .as_ref()
            .map(|p| p.pid);
        if let Some(pid) = lsp_pid
            && pid != 0
        {
            let _ = crate::sidecar::lsp_relay::kill_lsp_relay(pid).await;
        }
        {
            let mut s = self.state.write().await;
            s.lsp_relay_process = None;
        }
        let _ = dispatcher::clear_lsps_state(&self.identity.node_id).await;

        let status_topic = node_status_topic(&self.identity.node_id);
        if let Err(e) = self
            .client
            .publish_text(&status_topic, "offline", QoS::AtLeastOnce, true)
            .await
        {
            tracing::warn!(error = %e, "Failed to publish status=offline on shutdown");
        }
        // ADR-059 §7.2: clear the retained NodeReady snapshot so the
        // Gateway (and any other consumer) stops treating this node as
        // control-plane-ready. Empty retained payload = deleted message.
        let ready_topic = node_ready_topic(&self.identity.node_id);
        if let Err(e) = self
            .client
            .publish_raw(&ready_topic, Vec::new(), QoS::AtLeastOnce, true)
            .await
        {
            tracing::warn!(error = %e, "Failed to clear retained NodeReady on shutdown");
        }
        {
            let mut s = self.state.write().await;
            s.set_connected(false, None);
            s.save_snapshot(&self.config.home);
        }
        if self.identity.enrollment == EnrollmentState::Enrolled {
            // ADR-075 D4: a rename may have landed since the last heartbeat
            // (identity.json is the source of truth) — re-read the display
            // name so shutdown cannot clobber it.
            if let Ok(Some(disk)) = NodeIdentity::load(&self.config.home) {
                self.identity.node_name = disk.node_name;
            }
            let _ = self.identity.save(&self.config.home);
        }
    }

    /// Enroll-only mode (`acowork-node enroll`): publish the identity
    /// (retained info) once, then exit. The status topic is left at
    /// "offline" because no daemon is running — daemon liveness is
    /// owned by the LWT / explicit status publishes, not enrollment.
    ///
    /// ADR-055 Phase 5a: with `--token` this also performs the
    /// enrollment handshake — the Gateway replies on
    /// `enroll_result` with a long-lived node_token, which is
    /// persisted into identity.json before the command exits.
    pub async fn enroll(config: NodeConfig) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let gateway_addr = config.gateway_addr();
        let identity = NodeIdentity::load_or_create(
            &config.home,
            config.name.as_deref(),
            Some(&gateway_addr),
            config.gateway_managed,
        )?;
        let node_id = identity.node_id.clone();

        let state: SharedNodeState = Arc::new(RwLock::new(crate::state::NodeState::new(
            config.max_agents,
        )));
        let dedup: Arc<Mutex<RequestDedup>> = Arc::new(Mutex::new(RequestDedup::default()));

        // Minimal bootstrap for the one-shot enrollment: publish info
        // retained + status online, wait briefly for the QoS 1 flow,
        // then publish status offline retained and return.
        let bs_identity = Arc::new(RwLock::new(identity.clone()));
        let bs_config = config.clone();
        let bs_state = state.clone();
        let bs_home = config.home.clone();
        let bs_gateway_addr = gateway_addr.clone();
        // Cloned BEFORE the bootstrap closure moves it in — the
        // message callback (created after connect) also needs it.
        let dispatch_identity = bs_identity.clone();
        let mqtt_credentials: SharedNodeMqttCredentials = Arc::new(Mutex::new(
            identity
                .node_token
                .clone()
                .or_else(|| config.token.clone()),
        ));
        let bs_credentials = mqtt_credentials.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let identity = bs_identity.clone();
            let config = bs_config.clone();
            let state = bs_state.clone();
            let home = bs_home.clone();
            let gateway_addr = bs_gateway_addr.clone();
            tokio::spawn(async move {
                let id = identity.read().await.clone();
                let status_topic = node_status_topic(&id.node_id);
                let _ = client
                    .publish(&status_topic, QoS::AtLeastOnce, true, "online".as_bytes())
                    .await;
                let live_host = refresh_live_advertise_host(state.read().await, &config);
                let info = build_node_info(&id, &config, &live_host, 0);
                let envelope = DataEnvelope {
                    version: 1,
                    payload: Some(data_envelope::Payload::NodeInfo(info)),
                };
                let _ = client
                    .publish(
                        node_info_topic(&id.node_id),
                        QoS::AtLeastOnce,
                        true,
                        prost::Message::encode_to_vec(&envelope),
                    )
                    .await;
                // ADR-055 Phase 5a: request the long-lived node token.
                publish_enroll(&client, &id, &config).await;
                {
                    let mut id = identity.write().await;
                    id.mark_enrolled(&gateway_addr);
                    let _ = id.save(&home);
                }
                let mut s = state.write().await;
                s.set_connected(true, Some(gateway_addr));
                s.save_snapshot(&home);
            });
        });

        let dispatch_node_id = node_id.clone();
        let dispatch_dedup = dedup.clone();
        let dispatch_state = state.clone();
        let dispatch_config = config.clone();
        let dispatch_home = config.home.clone();
        let dispatch_credentials = bs_credentials.clone();
        // Kept for the final persist below — message_callback moves
        // dispatch_identity into its closure.
        let final_identity = dispatch_identity.clone();
        let message_callback: NodeMqttMessageCallback = Arc::new(move |topic, payload| {
            let node_id = dispatch_node_id.clone();
            let dedup = dispatch_dedup.clone();
            let state = dispatch_state.clone();
            let config = dispatch_config.clone();
            let identity = dispatch_identity.clone();
            let home = dispatch_home.clone();
            let credentials = dispatch_credentials.clone();
            tokio::spawn(async move {
                // ADR-055 Phase 5a: persist the node_token and switch
                // the live CONNECT credential before the one-shot
                // exits.
                if handle_enroll_result(&topic, &payload, &identity, &home).await {
                    // Snapshot the token before the awaited credential
                    // swap: a guard held across await (if-let scrutinee
                    // temporary) would stall the identity writer
                    // (261a8f77 lock-discipline class).
                    let token = identity.read().await.node_token.clone();
                    if let Some(token) = token {
                        *credentials.lock().await = Some(token);
                    }
                    return;
                }
                let _ = handle_incoming(
                    &node_id,
                    &topic,
                    &payload,
                    &dedup,
                    &state,
                    &config,
                    &identity,
                )
                .await;
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &node_id,
            mqtt_credentials,
            // One-shot enrollment never announced NodeReady; nothing to
            // clear on disconnect.
            None,
            bootstrap,
            Some(message_callback),
        )
        .await?;
        dispatcher::install(client.shared_handle());

        // Give the QoS 1 publishes time to flush through the event
        // loop before publishing offline + exiting.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let status_topic = node_status_topic(&node_id);
        let _ = client
            .publish_text(&status_topic, "offline", QoS::AtLeastOnce, true)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let mut identity = final_identity.read().await.clone();
        identity.mark_enrolled(&gateway_addr);
        identity.save(&config.home)?;
        tracing::info!(
            node_id = %identity.node_id,
            node_name = %identity.node_name,
            node_token = identity.node_token.as_deref().map(|_| "<set>").unwrap_or("<none>"),
            gateway = %gateway_addr,
            "Node enrolled (identity persisted)"
        );
        Ok(())
    }

    /// `acowork-node rename <new>` — change ONLY the display name
    /// (ADR-075 D4). `node_id` (the UUID routing key) NEVER changes, so
    /// there is no retained migration, no installed-inventory rebuild,
    /// and no daemon-stop precondition — the daemon keeps running with
    /// its `node:{uuid}` client.
    ///
    /// Precondition: the node is online (retained status = "online").
    /// Flow: connect with the temporary client_id `node:{uuid}:rename`
    /// (no LWT — the daemon's status/LWT stay untouched), read the
    /// retained status, then update identity.json + republish the
    /// info retained snapshot with the new node_name.
    pub async fn rename(config: NodeConfig, new_name: &str) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let mut identity = NodeIdentity::load(&config.home)?.ok_or_else(|| {
            NodeError::Identity("No identity.json — run `acowork-node start` first".to_string())
        })?;
        let node_id = identity.node_id.clone();
        validate_rename_target(&identity.node_name, new_name)?;

        // Temporary client_id `node:{uuid}:rename` (ADR-075 §3.3): the
        // running daemon owns `node:{uuid}` — reconnecting under the
        // same id would kick it off the broker.
        let client_id = format!("{}:rename", acowork_core::node::node_client_id(&node_id));
        let mut options = rumqttc::MqttOptions::new(
            &client_id,
            &config.gateway_host,
            config.gateway_mqtt_port,
        );
        options.set_keep_alive(std::time::Duration::from_secs(5));
        if let Some(token) = identity.node_token.as_deref() {
            options.set_credentials(client_id.clone(), token.to_string());
        }
        let (client, mut eventloop) = rumqttc::AsyncClient::new(options, 10);

        // Subscribe to the retained status AND the retained info (the
        // snapshot we are about to update — see the merge below). An
        // offline node refuses rename (the retained status is the single
        // source of liveness truth).
        let status_topic = node_status_topic(&node_id);
        let info_topic = node_info_topic(&node_id);
        for topic in [&status_topic, &info_topic] {
            client
                .subscribe(topic, QoS::AtLeastOnce)
                .await
                .map_err(|e| NodeError::Identity(format!("Subscribe failed: {e}")))?;
        }

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut status: Option<String> = None;
        let mut retained_info: Option<acowork_core::mqtt_proto::NodeInfo> = None;
        while tokio::time::Instant::now() <= deadline {
            if status.is_some() && retained_info.is_some() {
                break;
            }
            match eventloop.poll().await {
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)))
                    if p.topic == status_topic =>
                {
                    status = Some(String::from_utf8_lossy(&p.payload).trim().to_string());
                }
                Ok(rumqttc::Event::Incoming(rumqttc::Packet::Publish(p)))
                    if p.topic == info_topic =>
                {
                    if let Ok(envelope) = DataEnvelope::decode(&p.payload[..])
                        && let Some(data_envelope::Payload::NodeInfo(info)) = envelope.payload
                    {
                        retained_info = Some(info);
                    }
                }
                Ok(rumqttc::Event::Incoming(_)) | Ok(rumqttc::Event::Outgoing(_)) => {}
                Err(e) => {
                    return Err(NodeError::Identity(format!("MQTT error during rename: {e}")));
                }
            }
        }
        let status = status.ok_or_else(|| {
            NodeError::Identity(format!(
                "Timed out reading retained status for node '{node_id}' — is the broker reachable?"
            ))
        })?;
        if status != "online" {
            return Err(NodeError::Identity(format!(
                "Node '{node_id}' is not online (status = '{status}') — rename requires the node to be online"
            )));
        }

        // Persist the new display name; node_id untouched.
        let old_name = identity.node_name.clone();
        identity.node_name = new_name.to_string();
        identity.save(&config.home)?;

        // Republish the info retained snapshot with the new node_name so
        // the NodeRegistry display converges immediately (the daemon's
        // next heartbeat confirms it — and self-heals if we die here).
        // The merge is over the snapshot we just read: agent_count /
        // endpoints / live host belong to the daemon, and fabricating them
        // from a fresh empty state would lie to the Gateway for up to one
        // heartbeat interval.
        match retained_info {
            Some(mut info) => {
                info.node_id = node_id.clone();
                info.node_name = new_name.to_string();
                let envelope = DataEnvelope {
                    version: 1,
                    payload: Some(data_envelope::Payload::NodeInfo(info)),
                };
                let _ = client
                    .publish(
                        info_topic,
                        QoS::AtLeastOnce,
                        true,
                        prost::Message::encode_to_vec(&envelope),
                    )
                    .await;
            }
            None => tracing::warn!(
                node_id = %node_id,
                "No retained info snapshot to update — the daemon heartbeat will republish the new display name"
            ),
        }

        // Flush the QoS 1 publish before disconnecting (raw AsyncClient
        // sends on the event-loop poll).
        for _ in 0..10 {
            if eventloop.poll().await.is_err() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        drop(client);
        tracing::info!(
            node_id = %node_id,
            old_node_name = %old_name,
            node_name = %new_name,
            "Node display name renamed"
        );
        Ok(())
    }

    /// `acowork-node leave [--force]` — decommission the node
    /// (ADR-055 §6.13.2). Gracefully drains local Runtimes (unless
    /// `--force`), clears the retained status/info/installed topics
    /// (which drops the node from the Gateway's view), and leaves
    /// identity.json in place so the node can be re-started later.
    pub async fn leave(config: NodeConfig, force: bool) -> Result<(), NodeError> {
        config.ensure_dirs()?;

        let identity = NodeIdentity::load(&config.home)?.ok_or_else(|| {
            NodeError::Identity("No identity.json — run `acowork-node start` first".to_string())
        })?;
        let node_id = identity.node_id.clone();

        // Drain local Runtimes (unless --force): SIGTERM each running
        // agent and wait for it to exit so the broker sees their LWT.
        if !force {
            drain_running_agents(&config.home).await;
        }

        // Rebuild the install table to enumerate the retained `installed`
        // topics that must be cleared.
        let mut state = NodeState::new(config.max_agents);
        crate::package::restore_installed_agents(&mut state, &config.packages_dir());
        let installed: Vec<_> = state.installed_agents.values().cloned().collect();

        let node_id_for_cb = node_id.clone();
        let installed_for_cb = installed.clone();
        let bootstrap = Arc::new(move |client: AsyncClient| {
            let node_id = node_id_for_cb.clone();
            let installed = installed_for_cb.clone();
            tokio::spawn(async move {
                clear_retained(&client, &node_id, &installed).await;
            });
        });

        let client = NodeMqttClient::connect(
            &config.gateway_host,
            config.gateway_mqtt_port,
            &node_id,
            // ADR-055 Phase 5a: present the long-lived node token so
            // leave works against an auth-enabled broker.
            Arc::new(Mutex::new(identity.node_token.clone())),
            // One-shot leave does not announce NodeReady.
            None,
            bootstrap,
            None,
        )
        .await?;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        drop(client);

        tracing::info!(node_id = %node_id, "Node left (retained cleared)");
        Ok(())
    }
}

/// Validate a rename target (ADR-075 D2/D4): a valid `node_name` slug
/// (which already rejects the reserved word `"local"` and consecutive
/// `--`), different from the current name. `node_id` is never touched.
fn validate_rename_target(old_name: &str, new_name: &str) -> Result<(), NodeError> {
    if !acowork_core::node::node_name_is_valid(new_name) {
        return Err(NodeError::Identity(format!(
            "Invalid node name '{new_name}': must be 2-32 chars of [a-z0-9-], \
             no consecutive '--', no leading/trailing hyphen, and must not \
             be the reserved word 'local'"
        )));
    }
    if new_name == old_name {
        return Err(NodeError::Identity(format!(
            "Node is already named '{old_name}'"
        )));
    }
    Ok(())
}

/// Clear a node's retained status/info/installed topics with zero-byte
/// publishes (MQTT delete semantics). Used by `leave` (current name).
async fn clear_retained(
    client: &AsyncClient,
    node_id: &str,
    installed: &[crate::state::InstalledAgent],
) {
    let _ = client
        .publish(node_status_topic(node_id), QoS::AtLeastOnce, true, Vec::new())
        .await;
    let _ = client
        .publish(node_info_topic(node_id), QoS::AtLeastOnce, true, Vec::new())
        .await;
    for entry in installed {
        let _ = client
            .publish(
                node_agent_installed_topic(node_id, &entry.instance_id),
                QoS::AtLeastOnce,
                true,
                Vec::new(),
            )
            .await;
    }
    // ADR-055 §6.7: also clear the node-local LSP relay retained set.
    let _ = client
        .publish(node_lsps_topic(node_id), QoS::AtLeastOnce, true, Vec::new())
        .await;
    let _ = client
        .publish(
            node_sidecar_status_topic(node_id, "lsp_relay"),
            QoS::AtLeastOnce,
            true,
            Vec::new(),
        )
        .await;
}

/// SIGTERM every running Runtime tracked in the persisted snapshot and
/// wait for each to exit (ADR-055 §6.13.2 `leave` graceful drain).
async fn drain_running_agents(home: &std::path::Path) {
    let Some(snapshot) = NodeState::load_snapshot(home) else {
        return;
    };
    for agent in snapshot.agents {
        tracing::info!(agent_id = %agent.agent_id, pid = agent.pid, "Draining Runtime for leave");
        if let Err(e) = kill_agent_process(agent.pid).await {
            tracing::warn!(pid = agent.pid, error = %e, "Failed to terminate Runtime during leave");
            continue;
        }
        // Wait up to ~10s for the process to exit so the broker observes
        // the Runtime LWT before we clear the node retained set.
        for _ in 0..40 {
            if !crate::process::spawn::is_process_alive(agent.pid) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }
}

/// Process one incoming control-plane message.
///
/// QoS 1 duplicate discipline: if the request_id was already handled,
/// re-send the cached reply without re-executing.
async fn handle_incoming(
    own_node_id: &str,
    topic: &str,
    payload: &[u8],
    dedup: &Arc<Mutex<RequestDedup>>,
    state: &SharedNodeState,
    config: &NodeConfig,
    identity: &Arc<RwLock<NodeIdentity>>,
) -> Result<(), NodeError> {
    let Some((topic_node_id, agent_id)) = parse_control_topic(topic, own_node_id) else {
        // Not a control topic (e.g. our own retained publishes echoed
        // back if subscribed broadly) — ignore silently.
        return Ok(());
    };

    let command: NodeControlCommand =
        DataEnvelope::decode(payload)
            .map_err(|e| NodeError::Protocol(format!("Bad NodeControlCommand envelope: {e}")))?
            .payload
            .and_then(|p| match p {
                data_envelope::Payload::NodeControlCommand(cmd) => Some(cmd),
                _ => None,
            })
            .ok_or_else(|| {
                NodeError::Protocol(format!(
                    "Envelope on control topic '{topic}' is not a NodeControlCommand"
                ))
            })?;

    if command.node_id != topic_node_id {
        return Err(NodeError::Protocol(format!(
            "command.node_id '{}' != topic node_id '{}'",
            command.node_id, topic_node_id
        )));
    }

    let request_id = command.request_id.clone();
    if request_id.is_empty() {
        return Err(NodeError::Protocol(
            "NodeControlCommand without request_id".to_string(),
        ));
    }

    // Determine the reply topic BEFORE the dedup check so duplicates
    // are re-sent to the same place.
    let reply_topic = match &agent_id {
        Some(aid) => node_agent_events_topic(own_node_id, aid),
        None => node_events_topic(own_node_id),
    };

    // Take the cached reply out of the dedup lock before publishing:
    // `publish_event` awaits the MQTT dispatcher, so holding the dedup
    // lock across it would stall every later control command if the
    // publish ever blocks (same lock-discipline class as the reaper
    // regression introduced by 261a8f77).
    let cached = {
        let mut d = dedup.lock().await;
        if d.contains(&request_id) {
            d.cached_reply(&request_id).cloned()
        } else {
            None
        }
    };
    if let Some(cached) = cached {
        tracing::debug!(
            request_id = %request_id,
            "Duplicate control command — re-sending cached reply"
        );
        publish_event(reply_topic, cached).await?;
        return Ok(());
    }

    let node_token = identity.read().await.node_token.clone();
    let reply = handle_command(state, config, &command, node_token.as_deref()).await;
    let reply_clone = reply.clone();
    {
        let mut d = dedup.lock().await;
        d.insert(&request_id, reply_clone);
    }
    publish_event(reply_topic, reply).await
}

/// Publish a NodeEvent envelope (QoS 1, non-retained).
///
/// Events are results, not state — retained semantics would make an
/// old result shadow new ones after a Gateway restart.
async fn publish_event(topic: String, event: NodeEvent) -> Result<(), NodeError> {
    // The message callback (sync closure) has no return channel to the
    // owning `NodeMqttClient`, so reply publishes are routed through
    // the process-wide dispatcher: the daemon installs its shared
    // client handle at startup; `publish_event` fails fast (warn log)
    // if it is missing.
    dispatcher::publish(topic, event).await
}

/// Process-wide event dispatcher for command replies.
///
/// Holds the daemon's SHARED client handle (not a snapshot of the
/// client) so replies keep flowing through the current connection
/// after a soft-restart swaps the inner `AsyncClient`.
pub(crate) mod dispatcher {
    use std::sync::{Arc, OnceLock};
    use acowork_core::mqtt_proto::{data_envelope, DataEnvelope, InstalledAgentInfo, NodeEvent};
    use rumqttc::{AsyncClient, QoS};
    use tokio::sync::Mutex;

    static CLIENT: OnceLock<Arc<Mutex<AsyncClient>>> = OnceLock::new();

    pub fn install(shared: Arc<Mutex<AsyncClient>>) {
        let _ = CLIENT.set(shared);
    }

    pub async fn publish(topic: String, event: NodeEvent) -> Result<(), crate::error::NodeError> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEvent(event)),
        };
        publish_envelope(topic, envelope, false).await
    }

    /// Publish a retained installed-agent inventory entry (ADR-055 §6.5).
    pub async fn publish_installed_info(
        topic: String,
        info: InstalledAgentInfo,
    ) -> Result<(), crate::error::NodeError> {
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::InstalledAgentInfo(info)),
        };
        publish_envelope(topic, envelope, true).await
    }

    /// Clear a retained installed-agent entry (uninstall) — an empty
    /// retained payload removes the broker's retained message.
    pub async fn clear_installed_info(topic: String) -> Result<(), crate::error::NodeError> {
        publish_raw(topic, Vec::new(), true).await
    }

    /// Publish the retained node-local LSP relay state (ADR-055 §6.7):
    /// the per-node `lsps` envelope + sidecar status topic. Delegates
    /// to [`crate::sidecar::publish_lsps_state`] with the process-wide
    /// shared handle (used by the sidecar supervisor's transitions).
    pub async fn publish_lsps_state(
        node_id: String,
        advertise_host: String,
        port: u16,
        ready: bool,
    ) -> Result<(), crate::error::NodeError> {
        let Some(shared) = CLIENT.get() else {
            tracing::warn!("Node event dispatcher not installed — dropping lsps publish");
            return Ok(());
        };
        let client = shared.lock().await.clone();
        crate::sidecar::publish_lsps_state(&client, &node_id, &advertise_host, port, ready).await
    }

    /// Clear the retained node-local LSP relay state (shutdown / leave)
    /// — empty retained payloads remove the broker's retained messages.
    pub async fn clear_lsps_state(node_id: &str) -> Result<(), crate::error::NodeError> {
        use acowork_core::node::{node_lsps_topic, node_sidecar_status_topic};
        publish_raw(node_lsps_topic(node_id), Vec::new(), true).await?;
        publish_raw(node_sidecar_status_topic(node_id, "lsp_relay"), Vec::new(), true).await
    }

    /// Publish an arbitrary retained envelope on the daemon's shared
    /// MQTT client (the node HTTP services use it to republish a
    /// snapshot after a local mutation — e.g. the ADR-075 D4 rename).
    pub async fn publish_envelope(
        topic: String,
        envelope: DataEnvelope,
        retained: bool,
    ) -> Result<(), crate::error::NodeError> {
        let payload = prost::Message::encode_to_vec(&envelope);
        publish_raw(topic, payload, retained).await
    }

    async fn publish_raw(
        topic: String,
        payload: Vec<u8>,
        retained: bool,
    ) -> Result<(), crate::error::NodeError> {
        match CLIENT.get() {
            Some(shared) => {
                let client = shared.lock().await.clone();
                client
                    .publish(topic.clone(), QoS::AtLeastOnce, retained, payload)
                    .await
                    .map_err(|e| {
                        crate::error::NodeError::Mqtt(format!("Node publish '{topic}': {e}"))
                    })
            }
            None => {
                tracing::warn!(
                    topic = %topic,
                    "Node event dispatcher not installed — dropping publish"
                );
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::node::{node_agent_control_topic, node_enroll_result_topic};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn ping_command() -> NodeControlCommand {
        NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-1".to_string(),
            command: Some(node_control_command::Command::Ping(Default::default())),
        }
    }

    /// ADR-073: any valid UUID works for lifecycle tests.
    const TEST_INSTANCE_ID: &str = "a1b2c3d4-e5f6-4a7b-8c9d-0e1f2a3b4c5d";

    fn start_command(instance_id: &str) -> NodeControlCommand {
        use acowork_core::mqtt_proto::NodeStart;
        NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-start".to_string(),
            command: Some(node_control_command::Command::Start(NodeStart {
                agent_id: "com.test.agent".to_string(),
                dev_mode: false,
                instance_id: instance_id.to_string(),
            })),
        }
    }

    fn test_state() -> SharedNodeState {
        Arc::new(RwLock::new(crate::state::NodeState::new(16)))
    }

    fn test_config() -> NodeConfig {
        let tmp = tempfile::tempdir().unwrap();
        NodeConfig {
            home: tmp.path().to_path_buf(),
            ..NodeConfig::default()
        }
    }

    // ── refresh_live_advertise_host (§6.3.3) ─────────────────────────

    #[tokio::test]
    async fn live_host_fixed_config_is_verbatim() {
        let state = test_state();
        let config = NodeConfig {
            advertise_host: "10.0.0.5".to_string(),
            advertise_host_auto: false,
            ..test_config()
        };
        let host = refresh_live_advertise_host(state.read().await, &config);
        assert_eq!(host, "10.0.0.5");
        // The live slot mirrors the configured host.
        assert_eq!(
            *state
                .read()
                .await
                .live_advertise_host
                .lock()
                .unwrap(),
            "10.0.0.5"
        );
    }

    #[tokio::test]
    async fn live_host_auto_is_consistent_and_nonempty() {
        let state = test_state();
        let config = NodeConfig {
            advertise_host: "127.0.0.1".to_string(),
            advertise_host_auto: true,
            ..test_config()
        };
        let host = refresh_live_advertise_host(state.read().await, &config);
        // Either a real LAN IP was detected, or detection failed and we
        // fell back to the placeholder — but never empty, and the shared
        // slot always matches what the caller uses for NodeInfo.
        assert!(!host.is_empty());
        assert_eq!(
            *state
                .read()
                .await
                .live_advertise_host
                .lock()
                .unwrap(),
            host
        );
    }

    #[tokio::test]
    async fn ping_answers_ok() {
        let state = test_state();
        let config = test_config();
        let reply = handle_command(&state, &config, &ping_command(), None).await;
        assert_eq!(reply.status, "ok");
        assert_eq!(reply.message, "pong");
        assert_eq!(reply.request_id, "req-1");
    }

    #[tokio::test]
    async fn control_gate_rejects_malformed_instance_id() {
        let state = test_state();
        let config = test_config();
        let reply = handle_command(&state, &config, &start_command("not-a-uuid"), None).await;
        // ADR-073: rejected at the gate — before any ProcessManager or
        // filesystem side effect — so the reply carries the validation
        // error, not a spawn attempt.
        assert_eq!(reply.status, "error");
        assert!(
            reply.message.contains("invalid instance_id"),
            "unexpected reply: {}",
            reply.message
        );
        assert!(reply.message.contains("not-a-uuid"));
    }

    #[tokio::test]
    async fn control_gate_rejects_empty_instance_id() {
        let state = test_state();
        let config = test_config();
        let reply = handle_command(&state, &config, &start_command(""), None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("invalid instance_id"));
    }

    #[tokio::test]
    async fn lifecycle_instance_id_extraction_maps_all_agent_commands() {
        use acowork_core::mqtt_proto::NodeStop;
        assert_eq!(
            agent_lifecycle_instance_id(Some(&node_control_command::Command::Start(
                acowork_core::mqtt_proto::NodeStart {
                    agent_id: "com.test.agent".to_string(),
                    dev_mode: false,
                    instance_id: TEST_INSTANCE_ID.to_string(),
                },
            ))),
            Some(TEST_INSTANCE_ID)
        );
        assert_eq!(
            agent_lifecycle_instance_id(Some(&node_control_command::Command::Stop(NodeStop {
                agent_id: "com.test.agent".to_string(),
                reason: String::new(),
                instance_id: TEST_INSTANCE_ID.to_string(),
            }))),
            Some(TEST_INSTANCE_ID)
        );
        // Node-scoped ping has no instance id → None.
        assert_eq!(
            agent_lifecycle_instance_id(Some(&node_control_command::Command::Ping(
                Default::default()
            ))),
            None
        );
    }

    #[tokio::test]
    async fn start_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-2".to_string(),
            command: Some(node_control_command::Command::Start(
                acowork_core::mqtt_proto::NodeStart {
                    agent_id: "com.example".to_string(),
                    dev_mode: false,
                    // ADR-073: valid instance id so the test exercises the
                    // real not-installed path, not the validation gate.
                    instance_id: TEST_INSTANCE_ID.to_string(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        // The agent is not installed on this empty node → error (not
        // `not_implemented`, which was the Phase 2a placeholder).
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn install_without_source_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-3".to_string(),
            command: Some(node_control_command::Command::Install(
                acowork_core::mqtt_proto::NodeInstall {
                    agent_id: "com.example".to_string(),
                    package_url: String::new(),
                    local_path: String::new(),
                    dev_mode: false,
                    // ADR-073: valid instance id — the test targets the
                    // missing-source validation, not the UUID gate.
                    instance_id: TEST_INSTANCE_ID.to_string(),
                    ensure: false,
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("package_url"));
    }

    // ── Install → gate admission (ADR-073 install atomicity) ─────────

    /// A gate with no worker running — the shape a handler test needs to
    /// observe *admission* without performing a real install.
    async fn gated_state() -> SharedNodeState {
        let state = test_state();
        state
            .write()
            .await
            .set_install_gate(crate::package::install_gate::InstallGate::new());
        state
    }

    fn install_command(
        instance_id: &str,
        agent_id: &str,
        local_path: &str,
        ensure: bool,
    ) -> NodeControlCommand {
        NodeControlCommand {
            node_id: "local".to_string(),
            request_id: uuid::Uuid::new_v4().to_string(),
            command: Some(node_control_command::Command::Install(
                acowork_core::mqtt_proto::NodeInstall {
                    agent_id: agent_id.to_string(),
                    package_url: String::new(),
                    local_path: local_path.to_string(),
                    dev_mode: false,
                    instance_id: instance_id.to_string(),
                    ensure,
                },
            )),
        }
    }

    /// The install regression from the triple-install incident: the
    /// front-end may ask twice, but a declarative request for a package
    /// this node already holds must not queue a second copy.
    #[tokio::test]
    async fn ensure_for_an_installed_package_is_not_queued() {
        let state = gated_state().await;
        let config = test_config();
        let existing = uuid::Uuid::new_v4().to_string();
        state.write().await.add_installed(crate::state::InstalledAgent {
            instance_id: existing.clone(),
            agent_id: "com.test.agent".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: "D:/tmp/sys".to_string(),
            manifest: acowork_core::AgentManifest::from_toml(
                r#"
                agent_id = "com.test.agent"
                version = "1.0.0"
                name = "Test"
                description = "t"
                author = "t"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
                "#,
            )
            .unwrap(),
        });

        let cmd = install_command(
            &uuid::Uuid::new_v4().to_string(),
            "com.test.agent",
            "D:/tmp/pkg.agent",
            true,
        );
        let reply = handle_command(&state, &config, &cmd, None).await;

        assert_eq!(reply.status, "ok");
        assert!(reply.message.contains("already satisfied"), "{}", reply.message);
        let gate = state.read().await.install_gate().unwrap();
        assert_eq!(gate.pending_len().await, 0);
        assert!(gate.is_idle().await);
    }

    /// One instance id is one install: a replayed command coalesces into
    /// the queued job instead of queueing a second one.
    #[tokio::test]
    async fn replayed_install_command_coalesces_into_the_queued_job() {
        let state = gated_state().await;
        let config = test_config();
        let instance = uuid::Uuid::new_v4().to_string();
        let cmd = install_command(&instance, "com.test.agent", "D:/tmp/pkg.agent", false);

        let first = handle_command(&state, &config, &cmd, None).await;
        let second = handle_command(&state, &config, &cmd, None).await;

        // `in_progress` (not `ok`) — the job is only queued; the
        // terminal event arrives from the outcome sink once the gate has
        // processed it.
        assert_eq!(first.status, "in_progress");
        assert!(first.message.contains("queued"), "{}", first.message);
        assert_eq!(second.status, "in_progress");
        assert!(second.message.contains("already in progress"), "{}", second.message);
        assert_eq!(state.read().await.install_gate().unwrap().pending_len().await, 1);
    }

    /// ADR-073: two instance ids are two installs of one package — both
    /// are queued (serially), neither is refused.
    #[tokio::test]
    async fn two_instances_of_one_package_are_both_admitted() {
        let state = gated_state().await;
        let config = test_config();

        for _ in 0..2 {
            let cmd = install_command(
                &uuid::Uuid::new_v4().to_string(),
                "com.test.agent",
                "D:/tmp/pkg.agent",
                false,
            );
            let reply = handle_command(&state, &config, &cmd, None).await;
            assert_eq!(reply.status, "in_progress");
            assert!(reply.message.contains("queued"), "{}", reply.message);
        }

        assert_eq!(state.read().await.install_gate().unwrap().pending_len().await, 2);
    }

    /// Without a gate there is no install slot, so the command must fail
    /// loudly rather than vanish into an unserviced queue.
    #[tokio::test]
    async fn install_before_the_gate_exists_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = install_command(
            &uuid::Uuid::new_v4().to_string(),
            "com.test.agent",
            "D:/tmp/pkg.agent",
            false,
        );

        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("install gate"), "{}", reply.message);
    }

    /// A malformed instance id is refused at admission — it must never
    /// reach the queue, where the landing directory is derived from it.
    #[tokio::test]
    async fn install_with_a_non_uuid_instance_is_refused() {
        let state = gated_state().await;
        let config = test_config();
        let cmd = install_command("com.test.agent", "com.test.agent", "D:/tmp/pkg.agent", false);

        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("invalid instance_id"), "{}", reply.message);
        assert_eq!(state.read().await.install_gate().unwrap().pending_len().await, 0);
    }

    #[tokio::test]
    async fn empty_command_answers_error() {        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-4".to_string(),
            command: None,
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn clone_source_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-clone".to_string(),
            command: Some(node_control_command::Command::Clone(
                acowork_core::mqtt_proto::NodeClone {
                    agent_id: "com.example".to_string(),
                    new_agent_id: "com.example.clone".to_string(),
                    mode: "skeleton".to_string(),
                    instance_id: TEST_INSTANCE_ID.to_string(),
                    new_name: String::new(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn upgrade_without_source_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-upgrade".to_string(),
            command: Some(node_control_command::Command::Upgrade(
                acowork_core::mqtt_proto::NodeUpgrade {
                    agent_id: "com.example".to_string(),
                    package_url: String::new(),
                    local_path: String::new(),
                    dev_mode: false,
                    instance_id: TEST_INSTANCE_ID.to_string(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
        assert!(reply.message.contains("package_url"));
    }

    #[tokio::test]
    async fn publish_prepare_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-prepare".to_string(),
            command: Some(node_control_command::Command::PublishPrepare(
                acowork_core::mqtt_proto::NodePublishPrepare {
                    agent_id: "com.example".to_string(),
                    clean: false,
                    instance_id: TEST_INSTANCE_ID.to_string(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[tokio::test]
    async fn publish_build_not_installed_answers_error() {
        let state = test_state();
        let config = test_config();
        let cmd = NodeControlCommand {
            node_id: "local".to_string(),
            request_id: "req-build".to_string(),
            command: Some(node_control_command::Command::PublishBuild(
                acowork_core::mqtt_proto::NodePublishBuild {
                    agent_id: "com.example".to_string(),
                    output_dir: String::new(),
                    sign: false,
                    key_dir: String::new(),
                    instance_id: TEST_INSTANCE_ID.to_string(),
                },
            )),
        };
        let reply = handle_command(&state, &config, &cmd, None).await;
        assert_eq!(reply.status, "error");
    }

    #[test]
    fn control_topic_parsing() {
        assert_eq!(
            parse_control_topic("acowork/nodes/local/control/ping", "local"),
            Some(("local".to_string(), None))
        );
        assert_eq!(
            parse_control_topic(
                &format!("acowork/nodes/local/agents/{TEST_INSTANCE_ID}/control/start"),
                "local"
            ),
            Some(("local".to_string(), Some(TEST_INSTANCE_ID.to_string())))
        );
        // ADR-073: parse_control_topic is a routing decision only.
        // It accepts ANY non-empty `agents/{x}/control/{cmd}` segment;
        // UUID-shape validation is the handler's job (it can then
        // publish an error reply). A package-id topic that lacks the
        // `control` keyword still does not parse.
        assert_eq!(
            parse_control_topic(
                "acowork/nodes/local/agents/com.example/control/start",
                "local"
            ),
            Some(("local".to_string(), Some("com.example".to_string())))
        );
        // Other nodes' topics must not parse for us.
        assert_eq!(parse_control_topic("acowork/nodes/other/control/ping", "local"), None);
        // Non-control topics are ignored.
        assert_eq!(parse_control_topic("acowork/nodes/local/status", "local"), None);
        assert_eq!(parse_control_topic("acowork/nodes/local/info", "local"), None);
    }

    #[test]
    fn control_topic_construction_matches_parsing() {
        let topic = node_agent_control_topic("local", TEST_INSTANCE_ID, "start");
        assert_eq!(
            parse_control_topic(&topic, "local"),
            Some(("local".to_string(), Some(TEST_INSTANCE_ID.to_string())))
        );
    }

    #[test]
    fn rename_target_valid_slugs_are_accepted() {
        assert!(validate_rename_target("gpu-server", "gpu-2").is_ok());
        assert!(validate_rename_target("gpu-server", "gpu-server-2").is_ok());
    }

    #[test]
    fn rename_target_invalid_slug_is_rejected() {
        assert!(validate_rename_target("gpu-server", "Bad_Name").is_err());
        assert!(validate_rename_target("gpu-server", "-lead").is_err());
        assert!(validate_rename_target("gpu-server", "a").is_err());
        // "local" is the Gateway-direct agent placeholder — reserved
        // (ADR-075 D2).
        assert!(validate_rename_target("gpu-server", "local").is_err());
        // Consecutive hyphens are not a valid slug.
        assert!(validate_rename_target("gpu-server", "a--b").is_err());
    }

    #[test]
    fn rename_target_same_name_is_rejected() {
        let err = validate_rename_target("gpu-server", "gpu-server").unwrap_err();
        assert!(err.to_string().contains("already named"));
    }

    // ── ADR-055 Phase 5a enrollment ───────────────────────────────

    fn test_identity() -> NodeIdentity {
        NodeIdentity {
            node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
            node_name: "gpu-1".to_string(),
            gateway_managed: false,
            node_token: None,
            gateway_addr: None,
            enrollment: EnrollmentState::Created,
            created_at: chrono::Utc::now(),
            enrolled_at: None,
        }
    }

    fn enroll_result_envelope(node_token: &str, status: &str) -> Vec<u8> {
        let result = acowork_core::mqtt_proto::NodeEnrollResult {
            node_id: "0f0e0d0c-0b0a-4009-8007-060504030201".to_string(),
            node_token: node_token.to_string(),
            status: status.to_string(),
            message: "test reply".to_string(),
        };
        let envelope = DataEnvelope {
            version: 1,
            payload: Some(data_envelope::Payload::NodeEnrollResult(result)),
        };
        prost::Message::encode_to_vec(&envelope)
    }

    #[test]
    fn build_enroll_payload_with_token() {
        let config = NodeConfig {
            token: Some("tok-1234".to_string()),
            ..test_config()
        };
        let envelope = build_enroll_payload(&test_identity(), &config);
        let data_envelope::Payload::NodeEnroll(enroll) = envelope.payload.unwrap() else {
            panic!("expected NodeEnroll payload");
        };
        assert_eq!(enroll.node_id, "0f0e0d0c-0b0a-4009-8007-060504030201");
        assert_eq!(enroll.enrollment_token, "tok-1234");
        assert_eq!(enroll.protocol_version, NODE_PROTOCOL_VERSION);
        assert!(!enroll.capabilities.is_empty());
        assert!(!enroll.os.is_empty() && !enroll.arch.is_empty());
    }

    #[test]
    fn build_enroll_payload_without_credential_keeps_empty_token() {
        // No CLI token, no persisted node_token → the payload is still
        // built, with an empty credential: the Gateway decides (auth
        // off → mint the first token; auth on → reject). Staying
        // silent here would strand the node with no token, and the
        // fail-closed reverse proxy would 403 every request.
        let envelope = build_enroll_payload(&test_identity(), &test_config());
        let data_envelope::Payload::NodeEnroll(enroll) = envelope.payload.unwrap() else {
            panic!("expected NodeEnroll payload");
        };
        assert!(enroll.enrollment_token.is_empty());

        // Holds a node_token → the token doubles as the credential
        // (idempotent re-enrollment; the Gateway can re-sync its store
        // after a loss).
        let mut identity = test_identity();
        identity.node_token = Some("existing-token".to_string());
        let config = NodeConfig {
            token: None,
            ..test_config()
        };
        let envelope = build_enroll_payload(&identity, &config);
        let data_envelope::Payload::NodeEnroll(enroll) = envelope.payload.unwrap() else {
            panic!("expected NodeEnroll payload");
        };
        assert_eq!(enroll.enrollment_token, "existing-token");
        // The CLI-provided one-time token still wins over the persisted one.
        identity.node_token = Some("persisted-token".to_string());
        let config = NodeConfig {
            token: Some("tok-1234".to_string()),
            ..test_config()
        };
        let envelope = build_enroll_payload(&identity, &config);
        let data_envelope::Payload::NodeEnroll(enroll) = envelope.payload.unwrap() else {
            panic!("expected NodeEnroll payload");
        };
        assert_eq!(enroll.enrollment_token, "tok-1234");
    }

    #[tokio::test]
    async fn enroll_result_persists_node_token_once() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let identity = Arc::new(RwLock::new(test_identity()));
        let topic = node_enroll_result_topic(&identity.read().await.node_id);
        let payload = enroll_result_envelope("tok-node-0001", "ok");

        assert!(handle_enroll_result(&topic, &payload, &identity, home).await);
        assert_eq!(
            identity.read().await.node_token.as_deref(),
            Some("tok-node-0001")
        );
        // Persisted to disk.
        let loaded = NodeIdentity::load(home).unwrap().unwrap();
        assert_eq!(loaded.node_token.as_deref(), Some("tok-node-0001"));

        // Idempotent: a same-token reply is a no-op (no overwrite churn).
        let same = enroll_result_envelope("tok-node-0001", "ok");
        assert!(handle_enroll_result(&topic, &same, &identity, home).await);
        assert_eq!(
            identity.read().await.node_token.as_deref(),
            Some("tok-node-0001")
        );

        // The Gateway is authoritative: a re-enrollment that mints a
        // NEW token (its own store was lost) updates the local token.
        let renewed = enroll_result_envelope("tok-node-0002", "ok");
        assert!(handle_enroll_result(&topic, &renewed, &identity, home).await);
        assert_eq!(
            identity.read().await.node_token.as_deref(),
            Some("tok-node-0002"),
            "status=ok reply with a new token must re-sync the identity"
        );
        let loaded = NodeIdentity::load(home).unwrap().unwrap();
        assert_eq!(loaded.node_token.as_deref(), Some("tok-node-0002"));
    }

    #[tokio::test]
    async fn enroll_result_error_reply_does_not_write_token() {
        let tmp = tempfile::tempdir().unwrap();
        let identity = Arc::new(RwLock::new(test_identity()));
        let payload = enroll_result_envelope("", "error");
        assert!(handle_enroll_result(
            &node_enroll_result_topic("gpu-1"),
            &payload,
            &identity,
            tmp.path(),
        )
        .await);
        assert_eq!(identity.read().await.node_token, None);
    }

    #[tokio::test]
    async fn enroll_result_other_topic_is_ignored() {
        let identity = Arc::new(RwLock::new(test_identity()));
        assert!(!handle_enroll_result(
            "acowork/nodes/gpu-1/info",
            &[],
            &identity,
            std::path::Path::new("/tmp"),
        )
        .await);
    }

    #[test]
    fn probe_health_accepts_200_and_rejects_silent_peer() {
        use std::io::{Read, Write};

        // A responder that returns a real HTTP 200 line.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 256];
            let _ = sock.read(&mut buf);
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });
        assert!(probe_health(port).is_ok());
        server.join().unwrap();

        // A peer that accepts the connection but never answers must
        // fail the probe (read timeout) — mirrors the hung-server
        // symptom where TCP handshakes succeed but no response comes.
        let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let silent_port = silent.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (_sock, _) = silent.accept().unwrap();
            // Outlives the probe by design; the handle is detached.
            std::thread::sleep(std::time::Duration::from_secs(2));
        });
        let t0 = std::time::Instant::now();
        assert!(probe_health(silent_port).is_err());
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "silent peer must fail via read timeout, not hang"
        );
    }
}

