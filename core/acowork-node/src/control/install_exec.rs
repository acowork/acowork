//! Node-side install executor (ADR-073 install atomicity).
//!
//! [`InstallGate`](crate::package::install_gate::InstallGate) owns
//! **when** an install runs (one slot per node); this module owns
//! **what** running one means on a Node: resolve the package source,
//! land it in `{packages_dir}/{agent_id}/{instance_id}/` through the
//! shared [`install_package`](crate::package::install::install_package)
//! path, and publish the retained inventory entry the Gateway
//! aggregates.
//!
//! Kept out of `package::install_gate` on purpose: the gate is a pure
//! serialization mechanism (no filesystem, no HTTP, no MQTT) so its
//! invariants stay unit-testable against a fake executor, while every
//! side effect lives here behind the
//! [`InstallExecutor`](crate::package::install_gate::InstallExecutor)
//! seam.

use std::path::PathBuf;
use std::sync::Arc;

use acowork_core::agent_instance_id::AgentInstanceId;
use acowork_core::install::{InstallOutcome, InstallSource, InstallTicket};
use acowork_core::node::node_agent_installed_topic;
use async_trait::async_trait;
use tokio::sync::RwLock;

use super::dispatcher;
use crate::config::NodeConfig;
use crate::identity::NodeIdentity;
use crate::package::install_gate::{InstallExecutor, InstallOutcomeSink};
use crate::state::{NodeState, SharedNodeState};

/// Resolve an existing instance of `agent_id` from the install table.
///
/// ADR-073: several instances of one package are legal, so this answers
/// an *existence* question ("is there at least one?") — never an
/// identity one. The lowest instance id wins purely so the answer is
/// stable across calls when a package has multiple copies.
pub(crate) fn existing_instance_of(
    state: &NodeState,
    agent_id: &str,
) -> Option<AgentInstanceId> {
    state
        .installed_agents
        .values()
        .filter(|installed| installed.agent_id == agent_id)
        .filter_map(
            |installed| match AgentInstanceId::from_string(installed.instance_id.clone()) {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(
                        instance_id = %installed.instance_id,
                        error = %e,
                        "Install table holds a malformed instance id — ignoring it"
                    );
                    None
                }
            },
        )
        .min_by(|a, b| a.as_str().cmp(b.as_str()))
}

/// Map a terminal install outcome onto the NodeEvent reply vocabulary.
///
/// Status contract with the Gateway (`mqtt::dispatch`):
/// - `ok` → operation Completed
/// - `error` → operation Failed (uncertain: the caller must re-check by
///   operation id, never blindly retry)
///
/// A declarative no-op is `ok`: nothing was queued and nothing failed.
pub fn terminal_reply(outcome: &InstallOutcome) -> (&'static str, String) {
    match outcome {
        InstallOutcome::Ready { instance_id } => {
            ("ok", format!("installed instance {instance_id}"))
        }
        InstallOutcome::AlreadySatisfied { instance_id } => {
            ("ok", format!("already satisfied by instance {instance_id}"))
        }
        InstallOutcome::Failed { message } => ("error", format!("install failed: {message}")),
    }
}

/// Publishes the terminal outcome of every queued install.
///
/// This is the second half of the install reply: the immediate reply says
/// "queued" (`in_progress` — the install has not run yet), and this event
/// completes the operation once the gate's single slot has actually
/// processed it. Without it the Gateway would leave the operation
/// `Running` until it expired.
pub struct NodeInstallOutcomeSink {
    node_id: String,
}

impl NodeInstallOutcomeSink {
    pub fn new(node_id: String) -> Self {
        Self { node_id }
    }
}

#[async_trait]
impl InstallOutcomeSink for NodeInstallOutcomeSink {
    async fn on_terminal(&self, ticket: &InstallTicket, outcome: &InstallOutcome) {
        let (status, message) = terminal_reply(outcome);
        // ADR-073: the events topic variable is the instance identity.
        let topic = acowork_core::node::node_agent_events_topic(
            &self.node_id,
            ticket.instance_id.as_str(),
        );
        let event = acowork_core::mqtt_proto::NodeEvent {
            node_id: self.node_id.clone(),
            // ADR-059 §6: the queued job carries the caller's operation
            // id, so this event completes that operation.
            request_id: ticket.operation_id.as_str().to_string(),
            status: status.to_string(),
            message,
            result_json: None,
        };
        if let Err(e) = dispatcher::publish(topic, event).await {
            tracing::warn!(
                operation_id = %ticket.operation_id,
                error = %e,
                "Failed to publish terminal install event"
            );
        }
    }
}

/// One install, as the Node performs it.
pub struct NodeInstallExecutor {
    state: SharedNodeState,
    /// `{home}/packages` — the root every instance lands under.
    packages_dir: PathBuf,
    node_id: String,
    /// Live identity: the package download carries the node token
    /// (`ADR-055 Phase 5a`), read per download so a token swapped in
    /// after enrollment is picked up without a restart.
    identity: Arc<RwLock<NodeIdentity>>,
}

impl NodeInstallExecutor {
    pub fn new(
        state: SharedNodeState,
        config: &NodeConfig,
        node_id: String,
        identity: Arc<RwLock<NodeIdentity>>,
    ) -> Self {
        Self {
            state,
            packages_dir: config.packages_dir(),
            node_id,
            identity,
        }
    }

    /// A package source resolved to a path the installer can read,
    /// plus the temp file to delete once the bytes are consumed.
    async fn resolve(&self, source: &InstallSource) -> Result<(PathBuf, Option<PathBuf>), String> {
        match source {
            InstallSource::LocalFile { path } => Ok((PathBuf::from(path), None)),
            InstallSource::Registry { url } => {
                let spool = std::env::temp_dir().join(format!(
                    "acowork-node-install-{}-{}.bot",
                    std::process::id(),
                    uuid::Uuid::new_v4()
                ));
                let token = self.identity.read().await.node_token.clone();
                super::download_package(url, &spool, token.as_deref())
                    .await
                    .map_err(|e| format!("download failed: {e}"))?;
                Ok((spool.clone(), Some(spool)))
            }
        }
    }
}

#[async_trait]
impl InstallExecutor for NodeInstallExecutor {
    async fn execute(&self, ticket: &InstallTicket) -> InstallOutcome {
        let (source_path, spooled) = match self.resolve(&ticket.source).await {
            Ok(resolved) => resolved,
            Err(message) => return InstallOutcome::Failed { message },
        };

        // Keep the write lock scoped to the synchronous install; the
        // retained-inventory publish must run outside it (same
        // lock-discipline class as 261a8f77).
        let result = {
            let mut node = self.state.write().await;
            crate::package::install::install_package(
                &source_path,
                &self.packages_dir,
                &mut node,
                ticket.dev_mode,
                ticket.instance_id.as_str(),
            )
        };
        if let Some(spool) = spooled {
            let _ = std::fs::remove_file(&spool);
        }

        match result {
            Ok(info) => {
                // Publish retained inventory (ADR-055 §6.5) — the
                // Gateway aggregates this into installed_agents.
                if let Some(entry) = crate::package::build_installed_info(&info) {
                    let topic = node_agent_installed_topic(&self.node_id, &info.instance_id);
                    if let Err(e) = dispatcher::publish_installed_info(topic, entry).await {
                        tracing::warn!(error = %e, "Failed to publish retained installed info");
                    }
                }
                // The landed instance is the ticket's by construction:
                // `install_package` is handed the identity verbatim, and
                // the scheduler's identity rule is what let it run.
                InstallOutcome::Ready {
                    instance_id: ticket.instance_id.clone(),
                }
            }
            Err(e) => InstallOutcome::Failed {
                message: e.to_string(),
            },
        }
    }

    async fn existing_instance(&self, agent_id: &str) -> Option<AgentInstanceId> {
        let state = self.state.read().await;
        existing_instance_of(&state, agent_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InstalledAgent;

    fn installed(agent_id: &str, instance_id: &str) -> InstalledAgent {
        let manifest = acowork_core::AgentManifest::from_toml(&format!(
            r#"
            agent_id = "{agent_id}"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            "#
        ))
        .unwrap();
        InstalledAgent {
            instance_id: instance_id.to_string(),
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Test Agent".to_string(),
            install_path: format!("D:/tmp/{instance_id}"),
            manifest,
        }
    }

    fn insert(state: &mut NodeState, agent_id: &str, instance_id: &str) {
        state
            .installed_agents
            .insert(instance_id.to_string(), installed(agent_id, instance_id));
    }

    /// ADR-073: two instances of one package are a legal state, so the
    /// lookup answers "is there at least one", and must answer the same
    /// way on every call (map iteration order is not stable).
    #[test]
    fn existing_instance_answers_existence_not_identity() {
        let mut state = NodeState::new(4);
        let a = AgentInstanceId::new();
        let b = AgentInstanceId::new();
        let other = AgentInstanceId::new();
        insert(&mut state, "com.acowork.senior-engineer", a.as_str());
        insert(&mut state, "com.acowork.senior-engineer", b.as_str());
        insert(&mut state, "com.test.other", other.as_str());

        let found = existing_instance_of(&state, "com.test.other").expect("present");
        assert_eq!(found.as_str(), other.as_str());

        let expected = a.as_str().min(b.as_str()).to_string();
        for _ in 0..8 {
            let found = existing_instance_of(&state, "com.acowork.senior-engineer")
                .expect("one of the two");
            assert_eq!(found.as_str(), expected);
        }

        assert!(existing_instance_of(&state, "com.test.absent").is_none());
    }

    /// A row the node cannot interpret must never satisfy a declarative
    /// request — failing closed would silently skip an install.
    #[test]
    fn malformed_instance_row_never_satisfies_an_ensure() {
        let mut state = NodeState::new(4);
        insert(&mut state, "com.test.agent", "not-a-uuid");
        assert!(existing_instance_of(&state, "com.test.agent").is_none());
    }

    #[test]
    fn terminal_reply_matches_the_gateway_status_contract() {
        let instance = AgentInstanceId::new();
        let ready = InstallOutcome::Ready {
            instance_id: instance.clone(),
        };
        assert_eq!(terminal_reply(&ready).0, "ok");

        // A declarative no-op completes the operation: nothing was queued
        // and nothing failed.
        let satisfied = InstallOutcome::AlreadySatisfied {
            instance_id: instance.clone(),
        };
        assert_eq!(terminal_reply(&satisfied).0, "ok");
        assert!(terminal_reply(&satisfied).1.contains(instance.as_str()));

        let failed = InstallOutcome::Failed {
            message: "boom".to_string(),
        };
        let (status, message) = terminal_reply(&failed);
        assert_eq!(status, "error");
        assert!(message.contains("boom"));
    }
}
