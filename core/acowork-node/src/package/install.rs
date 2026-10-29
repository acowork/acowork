//! .agent package installation (migrated from gateway
//! `package_manager/install.rs`, ADR-055 §6.20).
//!
//! Install flow: read ZIP from path → signature verify → manifest
//! validate → extract to install dir.
//!
//! All installation paths converge on [`install_package`] which takes a
//! file path. Callers that receive bytes (multipart handlers) must
//! spool to a temp file first.
//!
//! Re-based from `GatewayError`/`GatewayState` to
//! [`crate::error::NodeError`]/[`crate::state::NodeState`]. The
//! Gateway-side cron registration (S3.3) is intentionally NOT carried
//! over — cron scheduling is a Gateway global-resource concern
//! (ADR-055 §6.5); the Gateway registers cron triggers from the
//! install-completed event instead.

use std::io::Read;
use std::path::Path;

use crate::error::{NodeError, Result};
use crate::state::{InstalledAgent, NodeState};

/// Install a .agent package from a file path.
///
/// This is the sole installation entry point. When `dev_mode` is true,
/// unsigned packages are allowed (for local development). In production
/// mode, packages must have a valid signature.
///
/// ADR-073 §5.6: the package lands at
/// `{install_dir}/{agent_id}/{instance_id}/`; `instance_id` is supplied
/// by the Gateway at install time (never self-invented by the node).
pub fn install_package(
    package_path: &Path,
    install_dir: &Path,
    state: &mut NodeState,
    dev_mode: bool,
    instance_id: &str,
) -> Result<InstalledAgent> {
    // 1. Read and open ZIP
    let data = std::fs::read(package_path).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read package '{}': {}",
            package_path.display(),
            e
        ))
    })?;
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader).map_err(|e| {
        NodeError::Package(format!(
            "Failed to read ZIP '{}': {}",
            package_path.display(),
            e
        ))
    })?;

    // 2. Verify package signature (delegate to acowork-sign)
    if dev_mode {
        match acowork_sign::verify::verify_package(package_path) {
            Ok(result) => {
                tracing::info!(
                    "Package signature verified: signer={}, fingerprint={}, sections={}",
                    result.signer,
                    result.certificate_fingerprint,
                    result.sections_count
                );
            }
            Err(e) => {
                tracing::warn!(
                    "Package signature verification failed (dev mode, continuing): {}",
                    e
                );
            }
        }
    } else {
        match acowork_sign::verify::verify_package(package_path) {
            Ok(result) => {
                tracing::info!(
                    "Package signature verified: signer={}, fingerprint={}, sections={}",
                    result.signer,
                    result.certificate_fingerprint,
                    result.sections_count
                );
            }
            Err(e) => {
                tracing::error!("Package signature verification failed: {}", e);
                return Err(NodeError::Package(format!(
                    "Signature verification failed: {}",
                    e
                )));
            }
        }
    }

    // 3. Extract and parse manifest.toml
    let manifest = extract_manifest(&mut archive)?;

    // 4. Check if already installed (keyed by instance_id, ADR-073)
    if state.is_installed(instance_id) {
        return Err(NodeError::Package(format!(
            "Agent instance '{}' is already installed. Use upgrade instead.",
            instance_id
        )));
    }

    // 5. Create install directory: {install_dir}/{agent_id}/{instance_id}/
    let agent_install_dir = install_dir.join(&manifest.agent_id).join(instance_id);
    std::fs::create_dir_all(&agent_install_dir)
        .map_err(|e| NodeError::Package(format!("Failed to create install dir: {}", e)))?;

    // 6. Extract all files to install directory
    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| NodeError::Package(format!("ZIP read error: {}", e)))?;
        let outpath = match file.enclosed_name() {
            Some(path) => agent_install_dir.join(path),
            None => continue,
        };

        if file.is_dir() {
            std::fs::create_dir_all(&outpath).ok();
        } else {
            if let Some(p) = outpath.parent()
                && !p.exists()
            {
                std::fs::create_dir_all(p).ok();
            }
            let mut outfile = std::fs::File::create(&outpath).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to create file '{}': {}",
                    outpath.display(),
                    e
                ))
            })?;
            std::io::copy(&mut file, &mut outfile).map_err(|e| {
                NodeError::Package(format!(
                    "Failed to write file '{}': {}",
                    outpath.display(),
                    e
                ))
            })?;
        }
    }

    // 6b. ADR-104: declaration-graph gate. The packager already refuses a
    // package whose `capabilities.*.skills` do not resolve, but a package can
    // arrive unsigned or hand-built, and installing one that points at skills
    // it does not ship would surface later as a capability that silently does
    // nothing. Roll back on failure so no half-installed directory survives.
    if let Err(errors) = check_capability_skills(&agent_install_dir, &manifest) {
        let _ = std::fs::remove_dir_all(&agent_install_dir);
        return Err(NodeError::Package(errors));
    }

    // 7. Create InstalledAgent
    let info = InstalledAgent {
        instance_id: instance_id.to_string(),
        agent_id: manifest.agent_id.clone(),
        version: manifest.version.clone(),
        name: manifest.name.clone(),
        install_path: agent_install_dir.to_string_lossy().to_string(),
        manifest,
    };

    tracing::info!("Installed agent instance: {} ({}) v{}", info.instance_id, info.agent_id, info.version);
    state.add_installed(info.clone());

    Ok(info)
}

/// Check `capabilities.*.skills` against the `skills/` directories that were
/// actually extracted (ADR-104).
///
/// Returns the joined defect list on failure. Skill identity is the directory
/// name — the same key the manifest links by.
fn check_capability_skills(
    install_dir: &Path,
    manifest: &acowork_core::AgentManifest,
) -> std::result::Result<(), String> {
    if manifest.capabilities.is_empty() {
        return Ok(());
    }
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(install_dir.join("skills")) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                names.push(name.to_string());
            }
        }
    }
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let errors = manifest.validate_capability_skills(&refs);
    if errors.is_empty() {
        return Ok(());
    }
    Err(format!(
        "package '{}' declares capabilities that reference skills it does not ship: {}          (shipped: {})",
        manifest.agent_id,
        errors.join("; "),
        if refs.is_empty() {
            "<none>".to_string()
        } else {
            refs.join(", ")
        }
    ))
}

/// Extract manifest.toml from ZIP archive
fn extract_manifest(
    archive: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>,
) -> Result<acowork_core::AgentManifest> {
    let mut manifest_file = archive
        .by_name("manifest.toml")
        .map_err(|e| NodeError::Package(format!("manifest.toml not found in package: {}", e)))?;

    let mut manifest_str = String::new();
    manifest_file
        .read_to_string(&mut manifest_str)
        .map_err(|e| NodeError::Package(format!("Failed to read manifest.toml: {}", e)))?;

    let manifest = acowork_core::AgentManifest::from_toml(&manifest_str)
        .map_err(|e| NodeError::Package(format!("Invalid manifest.toml: {}", e)))?;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    /// Build a package that also ships `skills/<name>/SKILL.md` for each name.
    fn create_test_zip_with_skills(dir: &Path, manifest_toml: &str, skills: &[&str]) -> PathBuf {
        let zip_path = dir.join("test.agent");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        zip.start_file("manifest.toml", options).unwrap();
        zip.write_all(manifest_toml.as_bytes()).unwrap();

        zip.start_file("prompts/default.md", options).unwrap();
        zip.write_all(b"You are a weather agent.").unwrap();

        for skill in skills {
            zip.start_file(format!("skills/{skill}/SKILL.md"), options)
                .unwrap();
            zip.write_all(
                format!("---
name: {skill}
description: test skill
---

Body.
")
                    .as_bytes(),
            )
            .unwrap();
        }

        zip.finish().unwrap();
        zip_path
    }

    fn create_test_zip(dir: &Path, manifest_toml: &str) -> PathBuf {
        let zip_path = dir.join("test.agent");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();

        zip.start_file("manifest.toml", options).unwrap();
        zip.write_all(manifest_toml.as_bytes()).unwrap();

        zip.start_file("prompts/default.md", options).unwrap();
        zip.write_all(b"You are a weather agent.").unwrap();

        zip.finish().unwrap();
        zip_path
    }

    #[test]
    fn test_install_package_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-install-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let manifest_toml = r#"
            agent_id = "com.test.weather"
            version = "1.0.0"
            name = "Weather Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;

        let zip_path = create_test_zip(&temp_dir, manifest_toml);
        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let result = install_package(&zip_path, &install_dir, &mut state, true, "inst-1");
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_eq!(info.instance_id, "inst-1");
        assert_eq!(info.agent_id, "com.test.weather");
        assert_eq!(info.version, "1.0.0");
        assert!(state.is_installed("inst-1"));

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_install_package_already_installed() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-install-dup-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let manifest_toml = r#"
            agent_id = "com.test.dup"
            version = "1.0.0"
            name = "Dup Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;

        let zip_path = create_test_zip(&temp_dir, manifest_toml);
        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        // First install should succeed
        install_package(&zip_path, &install_dir, &mut state, true, "inst-dup").unwrap();

        // Second install should fail
        let result = install_package(&zip_path, &install_dir, &mut state, true, "inst-dup");
        assert!(result.is_err());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_install_package_missing_manifest() {
        let temp_dir = std::env::temp_dir().join(format!(
            "acowork-test-install-nomanifest-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Create ZIP without manifest.toml
        let zip_path = temp_dir.join("no-manifest.agent");
        let zip_file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(zip_file);
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("prompts/default.md", options).unwrap();
        zip.write_all(b"Hello").unwrap();
        zip.finish().unwrap();

        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let result = install_package(&zip_path, &install_dir, &mut state, true, "inst-nomanifest");
        assert!(result.is_err());

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    fn graph_manifest(agent_id: &str, skill_ref: &str) -> String {
        format!(
            r#"
            agent_id = "{agent_id}"
            version = "1.0.0"
            name = "Graph Test"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"
            [llm]
            provider = "openai"
            model = "gpt-4"
            [capabilities.decompose]
            description = "Break a task down"
            skills = ["{skill_ref}"]
        "#
        )
    }

    #[test]
    fn test_install_rejects_capability_referencing_unshipped_skill() {
        let temp_dir = std::env::temp_dir()
            .join(format!("acowork-test-install-graph-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        // Manifest links `missing-skill`, package ships `other-skill` only.
        let manifest_toml = graph_manifest("com.test.graph", "missing-skill");
        let zip_path = create_test_zip_with_skills(&temp_dir, &manifest_toml, &["other-skill"]);
        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let result = install_package(&zip_path, &install_dir, &mut state, true, "inst-graph");
        let err = result.expect_err("install must be refused");
        let msg = err.to_string();
        assert!(msg.contains("missing-skill"), "message was: {msg}");
        // The half-extracted directory must be rolled back, not left behind.
        assert!(!install_dir.join("inst-graph").exists() || !state.is_installed("inst-graph"));
        assert!(!state.is_installed("inst-graph"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_install_accepts_capability_referencing_shipped_skill() {
        let temp_dir = std::env::temp_dir()
            .join(format!("acowork-test-install-graph-ok-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let manifest_toml = graph_manifest("com.test.graph.ok", "decompose-skill");
        let zip_path = create_test_zip_with_skills(&temp_dir, &manifest_toml, &["decompose-skill"]);
        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);

        let info = install_package(&zip_path, &install_dir, &mut state, true, "inst-graph-ok")
            .expect("install must succeed");
        assert_eq!(info.agent_id, "com.test.graph.ok");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
