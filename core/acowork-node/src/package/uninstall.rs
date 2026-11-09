//! Package uninstallation (migrated from gateway
//! `package_manager/uninstall.rs`, ADR-055 §6.20).
//!
//! The Gateway-side cron cleanup (S3.3) is intentionally NOT carried
//! over — cron scheduling is a Gateway global-resource concern; the
//! Gateway removes cron entries when it processes the uninstall result.

use std::path::Path;

use crate::error::{NodeError, Result};
use crate::state::NodeState;

/// Uninstall a .agent package
///
/// ADR-073: the target is located by instance identity (`instance_id`,
/// falling back to the package id for legacy commands).
pub fn uninstall_package(
    instance_id: &str,
    agent_id: &str,
    _install_dir: &Path,
    state: &mut NodeState,
) -> Result<()> {
    // ADR-073: the install table is keyed by instance identity.
    let key = instance_id.to_string();

    // Check if agent is installed
    let info = state
        .installed_agents
        .get(&key)
        .ok_or_else(|| NodeError::AgentNotFound(key.clone()))?
        .clone();

    // Check if agent is running
    if state.is_running(&key) {
        return Err(NodeError::AgentAlreadyRunning(key.clone()));
    }

    // Remove install directory
    let agent_dir = Path::new(&info.install_path);
    if agent_dir.exists() {
        std::fs::remove_dir_all(agent_dir)
            .map_err(|e| NodeError::Package(format!("Failed to remove install dir: {}", e)))?;
    }

    // Remove from state
    state.remove_installed(&key);

    tracing::info!("Uninstalled agent instance: {} ({})", key, agent_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uninstall_not_installed() {
        let mut state = NodeState::new(16);
        let install_dir = Path::new("/tmp/nonexistent");
        let result = uninstall_package("", "com.test.unknown", install_dir, &mut state);
        assert!(result.is_err());
    }
}
