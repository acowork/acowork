//! Local agent package operations (ADR-055 §6.20 — migrated from the
//! Gateway's `package_manager/*` plus the skills / manifest / avatar
//! file operations from `http/{skills_api,agents}.rs`).
//!
//! After Phase 2b the Gateway process contains no `std::fs` call
//! touching any package or workspace path (ADR-034 rule 3,
//! zero-exception enforcement); all of it lives here and executes on
//! the node's own filesystem.

pub mod clone;
pub mod install;
pub mod install_gate;
pub mod publish;
pub mod skills;
pub mod uninstall;
pub mod upgrade;

use std::path::Path;

use acowork_core::mqtt_proto::InstalledAgentInfo;

use crate::state::{InstalledAgent, NodeHttpState, NodeState};

/// Build the retained `InstalledAgentInfo` payload for a locally-installed
/// agent (ADR-055 §6.5). Returns `None` if the manifest fails to serialize
/// (should not happen for a valid installed package).
pub fn build_installed_info(installed: &InstalledAgent) -> Option<acowork_core::mqtt_proto::InstalledAgentInfo> {
    let manifest_toml = installed.manifest.to_toml().ok()?;
    Some(acowork_core::mqtt_proto::InstalledAgentInfo {
        agent_id: installed.agent_id.clone(),
        instance_id: installed.instance_id.clone(),
        version: installed.version.clone(),
        name: installed.name.clone(),
        install_path: installed.install_path.clone(),
        manifest_toml,
        overrides_json: read_overrides_json(installed),
    })
}

/// Read the instance's `{instance_id}.overrides.json` **verbatim**.
///
/// ADR-009 §5: the file is written by the Runtime and is opaque to the
/// Node — we copy the bytes into the retained inventory so a stopped
/// agent still reports the user's avatar / display name (the Runtime
/// has no process to ask). An unreadable file degrades to "no
/// overrides" so a permissions problem can never block an install from
/// being reported.
fn read_overrides_json(installed: &InstalledAgent) -> String {
    let Some(path) = acowork_core::overrides_path(
        Path::new(&installed.install_path),
        &installed.instance_id,
    ) else {
        return String::new();
    };
    match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Cannot read instance overrides — publishing empty overrides"
            );
            String::new()
        }
    }
}

/// [`build_installed_info`] with the manifest re-read from disk.
///
/// The install table holds the **install-time** manifest snapshot; a
/// publish-flow rewrite (`PUT /manifest/avatar`) changes the file only.
/// The retained inventory describes the package, so it must be built
/// from the file — the same rule `package_http::get_avatar` follows
/// when serving.
pub fn build_installed_info_fresh(installed: &InstalledAgent) -> Option<InstalledAgentInfo> {
    let mut fresh = installed.clone();
    let manifest_path = Path::new(&installed.install_path).join("manifest.toml");
    if let Ok(raw) = std::fs::read_to_string(&manifest_path)
        && let Ok(manifest) = acowork_core::AgentManifest::from_toml(&raw)
    {
        fresh.manifest = manifest;
    }
    build_installed_info(&fresh)
}

/// Re-publish this instance's retained inventory entry (ADR-055 §6.5).
///
/// That entry carries `manifest_toml` + `overrides_json` — the durable
/// copy the Gateway rebuilds its agent list from, and the only copy for
/// a **stopped** agent. Every write that changes either must refresh
/// it, otherwise the Gateway (and a Gateway restart) keeps reading the
/// pre-write value until this node reconnects.
///
/// Called from the two write paths this node can see: its own
/// `PUT /manifest/avatar` and the `PUT /avatar-config` it reverse
/// proxies to the Runtime.
pub async fn republish_installed_info(state: &NodeHttpState, instance_id: &str) {
    let entry = {
        let node = state.node.read().await;
        node.installed_agents.get(instance_id).cloned()
    };
    let Some(entry) = entry else {
        tracing::debug!(instance_id, "Not installed here — nothing to republish");
        return;
    };
    let Some(info) = build_installed_info_fresh(&entry) else {
        tracing::warn!(instance_id, "Cannot rebuild installed info — skipping republish");
        return;
    };
    let node_id = state.identity.read().await.node_id.clone();
    let topic = acowork_core::node::node_agent_installed_topic(&node_id, instance_id);
    if let Err(e) = crate::control::dispatcher::publish_installed_info(topic, info).await {
        tracing::warn!(instance_id, error = %e, "Failed to republish installed info");
    }
}

/// Scan the packages directory and rebuild the local install table
/// (ADR-055 §6.5: the node is the authority for its own package
/// inventory). Mirrors the Gateway's pre-hard-cut
/// `restore_installed_agents_static` — called once at daemon startup so
/// a restart re-discovers previously installed agents without a
/// re-install.
///
/// ADR-073 §5.6: installs land in the two-level layout
/// `{packages_dir}/{agent_id}/{instance_id}/manifest.toml`. The
/// `instance_id` is the directory name and is always a UUIDv4; no
/// fallback path is supported. A pre-ADR-073 flat layout
/// (`{agent_id}/manifest.toml`) is intentionally NOT auto-migrated —
/// re-install the package to land it in the new layout.
pub fn restore_installed_agents(state: &mut NodeState, packages_dir: &Path) {
    if !packages_dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(packages_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let agent_dir = entry.path();
        if !agent_dir.is_dir() {
            continue;
        }

        let Ok(instance_entries) = std::fs::read_dir(&agent_dir) else {
            continue;
        };

        for instance_entry in instance_entries.flatten() {
            let instance_dir = instance_entry.path();
            if !instance_dir.is_dir() {
                continue;
            }
            let manifest_path = instance_dir.join("manifest.toml");
            if !manifest_path.exists() {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&manifest_path) else {
                continue;
            };
            let manifest = match acowork_core::AgentManifest::from_toml(&content) {
                Ok(manifest) => manifest,
                Err(e) => {
                    // ADR-101 one-time migration: manifests installed before
                    // memory became a hosted service declare the retired
                    // `MemoryRead` / `MemoryWrite` permissions, which the
                    // current `Permission` deserializer rejects. Without this
                    // pass every legacy install is silently skipped here and
                    // the agent list comes back empty after a restart. Strip
                    // the retired entries and rewrite the file once; anything
                    // that still fails has a different cause and is logged.
                    match migrate_retired_memory_permissions(&content).and_then(|migrated| {
                        acowork_core::AgentManifest::from_toml(&migrated)
                            .ok()
                            .map(|manifest| (migrated, manifest))
                    }) {
                        Some((migrated, manifest)) => {
                            if let Err(we) = std::fs::write(&manifest_path, &migrated) {
                                tracing::warn!(
                                    path = %manifest_path.display(),
                                    error = %we,
                                    "Migrated manifest in memory but could not rewrite it — migration re-runs next restart"
                                );
                            }
                            tracing::info!(
                                path = %manifest_path.display(),
                                "Migrated legacy manifest: stripped retired memory permissions"
                            );
                            manifest
                        }
                        None => {
                            tracing::warn!(
                                path = %manifest_path.display(),
                                error = %e,
                                "Skipping instance directory: manifest.toml failed to parse"
                            );
                            continue;
                        }
                    }
                }
            };
            // ADR-073: the directory name MUST be a valid UUIDv4. An
            // instance directory named anything else (e.g. "manifest"
            // or an old package id) is not a valid instance and is
            // skipped — re-install through the official install path.
            let raw_instance_id = instance_dir
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if acowork_core::AgentInstanceId::from_string(raw_instance_id.clone()).is_err() {
                tracing::warn!(
                    path = %instance_dir.display(),
                    "Skipping instance directory with non-UUID name; reinstall required"
                );
                continue;
            }
            let info = InstalledAgent {
                instance_id: raw_instance_id,
                agent_id: manifest.agent_id.clone(),
                version: manifest.version.clone(),
                name: manifest.name.clone(),
                install_path: instance_dir.to_string_lossy().to_string(),
                manifest,
            };
            tracing::info!(
                "Restored installed agent instance on node: {} ({}) v{}",
                info.instance_id,
                info.agent_id,
                info.version
            );
            state.add_installed(info);
        }
    }
}

/// Strip the retired `MemoryRead` / `MemoryWrite` permission entries from a
/// legacy manifest TOML (ADR-101). Returns `None` when the document does not
/// parse at all or holds nothing to strip — a failure with another cause must
/// not be papered over.
fn migrate_retired_memory_permissions(content: &str) -> Option<String> {
    const RETIRED: [&str; 2] = ["MemoryRead", "MemoryWrite"];
    let mut doc: toml::Table = content.parse().ok()?;
    let permissions = doc.get("permissions")?.as_array()?.clone();
    let kept: Vec<toml::Value> = permissions
        .iter()
        .filter(|entry| match entry.get("type").and_then(|t| t.as_str()) {
            Some(perm_type) => !RETIRED.contains(&perm_type),
            None => true,
        })
        .cloned()
        .collect();
    if kept.len() == permissions.len() {
        return None;
    }
    if kept.is_empty() {
        doc.remove("permissions");
    } else {
        doc.insert("permissions".to_string(), toml::Value::Array(kept));
    }
    toml::to_string(&doc).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pre-ADR-101 manifest (retired memory permissions declared) fails the
    /// current parser; the migration strips exactly those entries, keeps the
    /// rest of the document intact, and is idempotent.
    #[test]
    fn legacy_manifest_with_retired_memory_permissions_is_migrated() {
        let legacy = r#"
agent_id = "com.acowork.test"
name = "Test"
version = "0.1.0"
description = "fixture"
author = "acowork-test"
runtime_version = "0.1.0"

[[permissions]]
type = "FilesystemRead"

[[permissions]]
type = "MemoryRead"

[[permissions]]
type = "MemoryWrite"

[[tools]]
name = "weather"

[memory]
enabled = true
"#;
        assert!(
            acowork_core::AgentManifest::from_toml(legacy).is_err(),
            "the retired permissions must fail the current parser — that is the bug being migrated"
        );
        let migrated =
            migrate_retired_memory_permissions(legacy).expect("migration strips the retired entries");
        let manifest = acowork_core::AgentManifest::from_toml(&migrated).expect("migrated manifest parses");
        assert_eq!(manifest.permissions.len(), 1);
        assert!(manifest.has_tool("weather"));
        assert_eq!(manifest.agent_id, "com.acowork.test");
        assert!(
            migrate_retired_memory_permissions(&migrated).is_none(),
            "a manifest without retired entries must not be rewritten again"
        );
    }

    /// The install table holds the install-time manifest snapshot; a
    /// publish-flow rewrite changes the file only. The retained
    /// inventory must follow the file, otherwise the Gateway keeps
    /// showing the pre-rewrite avatar until this node reconnects.
    #[test]
    fn build_installed_info_fresh_follows_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-pkg-fresh-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let on_disk = "agent_id = \"com.acowork.test\"\nname = \"Test\"\nversion = \"1.0.0\"\ndescription = \"fixture\"\nauthor = \"acowork-test\"\nruntime_version = \"0.1.0\"\navatar = \"assets/on-disk.png\"\n";
        std::fs::write(dir.join("manifest.toml"), on_disk).unwrap();

        let stale = acowork_core::AgentManifest::from_toml(
            "agent_id = \"com.acowork.test\"\nname = \"Test\"\nversion = \"1.0.0\"\ndescription = \"fixture\"\nauthor = \"acowork-test\"\nruntime_version = \"0.1.0\"\navatar = \"assets/stale.png\"\n",
        )
        .unwrap();
        let entry = InstalledAgent {
            instance_id: "3f2a0c1e-0000-4000-8000-000000000001".to_string(),
            agent_id: "com.acowork.test".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: dir.to_string_lossy().to_string(),
            manifest: stale,
        };

        let info = build_installed_info_fresh(&entry).expect("info");
        assert!(info.manifest_toml.contains("assets/on-disk.png"));
        assert!(!info.manifest_toml.contains("assets/stale.png"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
