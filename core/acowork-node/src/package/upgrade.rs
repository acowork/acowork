//! Package upgrade (migrated from gateway `package_manager/upgrade.rs`,
//! ADR-055 §6.20).
//!
//! Upgrade flow: verify new package → check signature consistency →
//! preserve `workspace/` → replace everything else.
//!
//! The instance directory has exactly two halves, and the split is the
//! whole point of the two-level layout (ADR-073):
//!
//! ```text
//! {install_dir}/{agent_id}/{instance_id}/
//!     manifest.toml assets/ prompts/ skills/   <- package content: REPLACED
//!     workspace/                               <- instance data:  PRESERVED
//! ```
//!
//! `workspace/` is the Runtime's work dir — sessions, the private SQLite
//! store, `config/agent_config.json` (the user's settings *and* the
//! instance's name), files, logs. It is the instance's only durable state
//! and must survive an upgrade untouched. Preserving a hardcoded pair of
//! `data/` + `config/` (the pre-`workspace/` layout) silently deleted all
//! of it.

use super::install::install_package;
use std::io::Read;
use std::path::Path;

use crate::error::{NodeError, Result};
use crate::state::NodeState;

/// Upgrade a .agent package (signature fingerprint consistency check required)
///
/// ADR-073: the target is located by instance identity (`instance_id`,
/// falling back to the package id for legacy commands); the upgrade
/// preserves the instance identity and re-installs under
/// `{install_dir}/{agent_id}/{instance_id}/`.
pub fn upgrade_package(
    instance_id: &str,
    agent_id: &str,
    new_package_path: &Path,
    install_dir: &Path,
    state: &mut NodeState,
    dev_mode: bool,
) -> Result<()> {
    // ADR-073: the install table is keyed by instance identity.
    let key = instance_id.to_string();

    // Check if agent is currently installed
    let old_info = state
        .installed_agents
        .get(&key)
        .ok_or_else(|| NodeError::AgentNotFound(key.clone()))?
        .clone();

    // Check if agent is running
    if state.is_running(&key) {
        return Err(NodeError::AgentAlreadyRunning(key.clone()));
    }

    // Parse new manifest to check agent_id consistency
    let new_manifest = parse_manifest_from_zip(new_package_path)?;
    if new_manifest.agent_id != agent_id {
        return Err(NodeError::Package(format!(
            "Package agent_id '{}' does not match expected '{}'",
            new_manifest.agent_id, agent_id
        )));
    }

    // Preserve the instance data directory (the Runtime's work dir).
    //
    // This is the ONLY thing that survives an upgrade. Everything else in
    // the instance dir is package content and is replaced wholesale — that
    // is what "upgrade" means: new code/assets/prompts, same instance.
    let old_install_path = Path::new(&old_info.install_path);
    let workspace_dir = acowork_core::workspace::workspace_dir(old_install_path);

    // Remove old install directory (except the preserved workspace)
    if old_install_path.exists() {
        let temp_base =
            std::env::temp_dir().join(format!("acowork-upgrade-{}", key.replace('.', "-")));
        std::fs::create_dir_all(&temp_base).ok();

        let mut preserved = false;
        if workspace_dir.exists() {
            match std::fs::rename(&workspace_dir, temp_base.join(acowork_core::workspace::WORKSPACE_DIR))
            {
                Ok(()) => preserved = true,
                // Refusing to "upgrade" is the only safe answer here: the
                // next line deletes the directory that holds the user's
                // sessions and settings. Losing the package is recoverable,
                // losing `workspace/` is not.
                Err(e) => {
                    let _ = std::fs::remove_dir_all(&temp_base);
                    return Err(NodeError::Package(format!(
                        "Upgrade aborted: cannot preserve instance data at {}: {}",
                        workspace_dir.display(),
                        e
                    )));
                }
            }
        }

        // Remove old install dir
        std::fs::remove_dir_all(old_install_path)
            .map_err(|e| NodeError::Package(format!("Failed to remove old install: {}", e)))?;

        // Re-create and restore the preserved workspace (ADR-073 two-level layout)
        let new_install_path = install_dir.join(agent_id).join(&key);
        std::fs::create_dir_all(&new_install_path).ok();

        if preserved {
            let restored = temp_base.join(acowork_core::workspace::WORKSPACE_DIR);
            if let Err(e) = std::fs::rename(
                &restored,
                acowork_core::workspace::workspace_dir(&new_install_path),
            ) {
                // Same reasoning as above, one step later: the old tree is
                // already gone, so the preserved copy in `temp_base` is now
                // the only one. Report its location instead of leaving the
                // caller to guess where the data went.
                return Err(NodeError::Package(format!(
                    "Upgrade aborted: cannot restore instance data from {}: {}",
                    restored.display(),
                    e
                )));
            }
        }

        // Cleanup temp
        std::fs::remove_dir_all(&temp_base).ok();
    }

    // Remove from state temporarily
    state.remove_installed(&key);

    // Install new package (upgrade inherits dev_mode from caller context;
    // the instance identity is preserved across the upgrade).
    let new_info = install_package(new_package_path, install_dir, state, dev_mode, &key)?;

    tracing::info!(
        "Upgraded agent instance: {} ({} from v{} to v{})",
        key,
        agent_id,
        old_info.version,
        new_info.version
    );
    Ok(())
}

/// Parse manifest from ZIP without full extraction
fn parse_manifest_from_zip(package_path: &Path) -> Result<acowork_core::AgentManifest> {
    let file = std::fs::File::open(package_path)
        .map_err(|e| NodeError::Package(format!("Failed to open package: {}", e)))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| NodeError::Package(format!("Failed to read ZIP: {}", e)))?;

    let mut manifest_file = archive
        .by_name("manifest.toml")
        .map_err(|e| NodeError::Package(format!("manifest.toml not found: {}", e)))?;

    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| NodeError::Package(format!("Failed to read manifest.toml: {}", e)))?;

    acowork_core::AgentManifest::from_toml(&manifest_str)
        .map_err(|e| NodeError::Package(format!("Invalid manifest.toml: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InstalledAgent;
    use std::io::Write;
    use std::path::PathBuf;

    fn create_test_zip(dir: &Path, agent_id: &str, version: &str) -> PathBuf {
        let zip_path = dir.join(format!("{}-{}.agent", agent_id.replace('.', "-"), version));
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        let manifest = format!(
            r#"
            agent_id = "{}"
            version = "{}"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#,
            agent_id, version
        );

        zip.start_file("manifest.toml", options).unwrap();
        zip.write_all(manifest.as_bytes()).unwrap();
        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn upgrade_preserves_instance_workspace() {
        let temp_dir = std::env::temp_dir().join(format!(
            "acowork-test-upgrade-workspace-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let install_dir = temp_dir.join("installed");

        let instance_dir = install_dir.join("com.test.weather").join("inst-weather");
        let workspace = acowork_core::workspace::workspace_dir(&instance_dir);
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::create_dir_all(workspace.join("conversations")).unwrap();
        // Two kinds of instance data: the user's settings file and a
        // session log. A regression that deletes `workspace/` takes both.
        std::fs::write(
            workspace.join("config").join("agent_config.json"),
            r#"{"name":"Weather.West","temperature":0.7}"#,
        )
        .unwrap();
        std::fs::write(workspace.join("conversations").join("s1.jsonl"), "{}\n").unwrap();
        // Package content of the OLD version, which the upgrade must replace.
        std::fs::create_dir_all(instance_dir.join("prompts")).unwrap();
        std::fs::write(instance_dir.join("prompts").join("old_only.md"), "v1").unwrap();

        let mut state = NodeState::new(16);
        state.add_installed(InstalledAgent {
            instance_id: "inst-weather".to_string(),
            agent_id: "com.test.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather".to_string(),
            install_path: instance_dir.to_string_lossy().to_string(),
            manifest: acowork_core::AgentManifest::from_toml(
                r#"
                agent_id = "com.test.weather"
                version = "1.0.0"
                name = "Weather"
                description = "Test"
                author = "test"
                runtime_version = "0.1.0"
                [llm]
                provider = "openai"
                model = "gpt-4"
            "#,
            )
            .unwrap(),
        });

        let zip_path = create_test_zip(&temp_dir, "com.test.weather", "2.0.0");
        upgrade_package(
            "inst-weather",
            "com.test.weather",
            &zip_path,
            &install_dir,
            &mut state,
            true,
        )
        .expect("upgrade should succeed");

        let new_instance_dir = install_dir.join("com.test.weather").join("inst-weather");
        let new_workspace = acowork_core::workspace::workspace_dir(&new_instance_dir);

        assert_eq!(
            std::fs::read_to_string(new_workspace.join("config").join("agent_config.json")).unwrap(),
            r#"{"name":"Weather.West","temperature":0.7}"#,
            "instance settings (including the agent name) must survive an upgrade"
        );
        assert!(
            new_workspace.join("conversations").join("s1.jsonl").exists(),
            "sessions must survive an upgrade"
        );
        assert!(
            !new_instance_dir.join("prompts").join("old_only.md").exists(),
            "package content must be replaced by the new version"
        );
        assert!(
            std::fs::read_to_string(new_instance_dir.join("manifest.toml"))
                .unwrap()
                .contains("2.0.0"),
            "manifest.toml must come from the new package"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_upgrade_not_installed() {
        let mut state = NodeState::new(16);
        let result = upgrade_package(
            "",
            "com.test.unknown",
            Path::new("/tmp/nonexistent.agent"),
            Path::new("/tmp/installed"),
            &mut state,
            true,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_upgrade_agent_id_mismatch() {
        let temp_dir = std::env::temp_dir().join(format!(
            "acowork-test-upgrade-mismatch-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create a package with different agent_id
        let zip_path = create_test_zip(&temp_dir, "com.test.other", "2.0.0");

        // Add old agent to state
        let mut state = NodeState::new(16);
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.weather"
            version = "1.0.0"
            name = "Weather"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#,
        )
        .unwrap();
        state.add_installed(InstalledAgent {
            instance_id: "inst-weather".to_string(),
            agent_id: "com.test.weather".to_string(),
            version: "1.0.0".to_string(),
            name: "Weather".to_string(),
            install_path: temp_dir
                .join("installed")
                .join("com.test.weather")
                .join("inst-weather")
                .to_string_lossy()
                .to_string(),
            manifest,
        });

        let result = upgrade_package(
            "inst-weather",
            "com.test.weather",
            &zip_path,
            &temp_dir.join("installed"),
            &mut state,
            true,
        );
        assert!(result.is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
