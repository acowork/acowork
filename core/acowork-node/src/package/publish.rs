//! Package publishing — prepare and build/sign .agent packages
//! (migrated from gateway `package_manager/publish.rs`, ADR-055 §6.20).
//!
//! S4.2: Publish prepare — validate manifest completeness, check
//! prompts, optional skills format check, cleanup operations.
//! S4.3: Publish build — package agent directory into .agent ZIP,
//! optional signing.

use std::path::Path;

use acowork_core::packaging::PackageOptions;

use crate::error::{NodeError, Result};
use crate::state::NodeState;

// ── S4.2: Publish prepare ──────────────────────────────────────────────

/// Result of a publish prepare operation
#[derive(Debug, Clone, serde::Serialize)]
pub struct PrepareResult {
    /// Individual check results
    pub checks: Vec<CheckItem>,
    /// Warning messages (non-blocking)
    pub warnings: Vec<String>,
    /// Error messages (blocking)
    pub errors: Vec<String>,
    /// Whether any cleanup was performed (dev flag removed, etc.)
    pub cleaned: bool,
}

/// A single check item from prepare
#[derive(Debug, Clone, serde::Serialize)]
pub struct CheckItem {
    /// Check name (e.g. "manifest", "prompts", "skills")
    pub name: String,
    /// "ok" or "error" or "warning"
    pub status: String,
    /// Optional detail message
    pub detail: Option<String>,
}

/// Run all publish-preparation checks against an installed agent.
///
/// ADR-073: the target is located by instance identity (`instance_id`);
/// package `agent_id` is not needed for lookup.
pub fn prepare_publish(
    instance_id: &str,
    clean: bool,
    state: &mut NodeState,
) -> Result<PrepareResult> {
    // ADR-073: the install table is keyed by instance identity.
    let key = instance_id.to_string();

    let info = state
        .installed_agents
        .get(&key)
        .ok_or_else(|| NodeError::AgentNotFound(key.clone()))?;

    // Clone paths to avoid borrow conflicts with mutation below
    let install_path = Path::new(&info.install_path).to_path_buf();
    if !install_path.exists() {
        return Err(NodeError::Package(format!(
            "Agent install path does not exist: {}",
            install_path.display()
        )));
    }

    let mut checks = Vec::new();
    let mut warnings = Vec::new();
    let mut errors = Vec::new();
    let mut cleaned = false;

    // ── S4.2.2: Manifest completeness check ──
    {
        let manifest = &info.manifest;

        if manifest.agent_id.is_empty() {
            errors.push("agent_id is empty".to_string());
            checks.push(CheckItem {
                name: "manifest.agent_id".to_string(),
                status: "error".to_string(),
                detail: Some("agent_id is empty".to_string()),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.agent_id".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }

        if manifest.version.is_empty() {
            errors.push("version is empty".to_string());
            checks.push(CheckItem {
                name: "manifest.version".to_string(),
                status: "error".to_string(),
                detail: Some("version is empty".to_string()),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.version".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }

        if manifest.name.is_empty() {
            errors.push("name is empty".to_string());
            checks.push(CheckItem {
                name: "manifest.name".to_string(),
                status: "error".to_string(),
                detail: Some("name is empty".to_string()),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.name".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }

        if manifest.description.is_empty() {
            warnings.push("description is empty".to_string());
            checks.push(CheckItem {
                name: "manifest.description".to_string(),
                status: "warning".to_string(),
                detail: Some("description is empty".to_string()),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.description".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }

        if manifest.author.is_empty() {
            warnings.push("author is empty".to_string());
            checks.push(CheckItem {
                name: "manifest.author".to_string(),
                status: "warning".to_string(),
                detail: Some("author is empty".to_string()),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.author".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }

        if manifest.runtime_version.is_empty() {
            warnings.push("runtime_version is empty".to_string());
            checks.push(CheckItem {
                name: "manifest.runtime_version".to_string(),
                status: "warning".to_string(),
                detail: Some("runtime_version is empty".to_string()),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.runtime_version".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }

        // LLM config: provider/model now come from resource_cache.providers.
        checks.push(CheckItem {
            name: "manifest.llm".to_string(),
            status: "ok".to_string(),
            detail: None,
        });

        // Agent ID format check (reverse-domain)
        if !is_valid_agent_id(&manifest.agent_id) {
            warnings.push(format!(
                "agent_id '{}' does not follow reverse-domain format",
                manifest.agent_id
            ));
            checks.push(CheckItem {
                name: "manifest.agent_id_format".to_string(),
                status: "warning".to_string(),
                detail: Some(format!(
                    "'{}' does not follow reverse-domain format (e.g. com.example.myagent)",
                    manifest.agent_id
                )),
            });
        } else {
            checks.push(CheckItem {
                name: "manifest.agent_id_format".to_string(),
                status: "ok".to_string(),
                detail: None,
            });
        }
    }

    // ── S4.2.3: Prompts existence check ──
    {
        let prompts_dir = install_path.join("prompts");
        if !prompts_dir.exists() {
            errors.push("prompts/ directory does not exist".to_string());
            checks.push(CheckItem {
                name: "prompts".to_string(),
                status: "error".to_string(),
                detail: Some("prompts/ directory does not exist".to_string()),
            });
        } else {
            let prompt_files: Vec<_> = std::fs::read_dir(&prompts_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|ext| ext == "md" || ext == "txt")
                        .unwrap_or(false)
                })
                .collect();

            if prompt_files.is_empty() {
                warnings.push("prompts/ directory contains no .md or .txt files".to_string());
                checks.push(CheckItem {
                    name: "prompts".to_string(),
                    status: "warning".to_string(),
                    detail: Some("No .md or .txt files in prompts/".to_string()),
                });
            } else {
                checks.push(CheckItem {
                    name: "prompts".to_string(),
                    status: "ok".to_string(),
                    detail: Some(format!("{} prompt file(s)", prompt_files.len())),
                });
            }
        }
    }

    // ── S4.2.4: Skills format check (optional) ──
    {
        let skills_dir = install_path.join("skills");
        if skills_dir.exists() {
            let mut skill_checks = Vec::new();
            let entries: Vec<_> = std::fs::read_dir(&skills_dir)
                .into_iter()
                .flatten()
                .filter_map(|e| e.ok())
                .collect();

            if entries.is_empty() {
                checks.push(CheckItem {
                    name: "skills".to_string(),
                    status: "ok".to_string(),
                    detail: Some("skills/ directory is empty".to_string()),
                });
            } else {
                for entry in entries {
                    let skill_path = entry.path();
                    if skill_path.is_dir() {
                        let skill_md = skill_path.join("SKILL.md");
                        if skill_md.exists() {
                            match std::fs::read_to_string(&skill_md) {
                                Ok(content) => {
                                    let has_frontmatter = content.trim_start().starts_with("---");
                                    skill_checks.push(format!(
                                        "{}: {}",
                                        entry.file_name().to_string_lossy(),
                                        if has_frontmatter {
                                            "ok"
                                        } else {
                                            "missing frontmatter"
                                        }
                                    ));
                                    if !has_frontmatter {
                                        warnings.push(format!(
                                            "Skill '{}' SKILL.md missing YAML frontmatter (---)",
                                            entry.file_name().to_string_lossy()
                                        ));
                                    }
                                }
                                Err(_) => {
                                    warnings.push(format!(
                                        "Failed to read SKILL.md for skill '{}'",
                                        entry.file_name().to_string_lossy()
                                    ));
                                }
                            }
                        }
                    }
                }
                let has_warnings = skill_checks.iter().any(|c| c.contains("missing"));
                checks.push(CheckItem {
                    name: "skills".to_string(),
                    status: if has_warnings {
                        "warning".to_string()
                    } else {
                        "ok".to_string()
                    },
                    detail: Some(skill_checks.join("; ")),
                });
            }
        } else {
            checks.push(CheckItem {
                name: "skills".to_string(),
                status: "ok".to_string(),
                detail: Some("No skills/ directory (optional)".to_string()),
            });
        }
    }

    // Extract dev flag before cleanup (avoid borrow conflicts)
    let manifest_dev_value = info.manifest.dev;
    let _ = info; // drop reference (info is a &InstalledAgent)

    // ── S4.2.5: Cleanup operations ──
    if clean {
        // Remove dev flag from manifest
        if manifest_dev_value {
            let mut manifest = {
                let info = state
                    .installed_agents
                    .get(&key)
                    .ok_or_else(|| NodeError::AgentNotFound(key.clone()))?
                    .clone();
                info.manifest.clone()
            };
            manifest.dev = false;
            let manifest_toml = toml::to_string_pretty(&manifest).map_err(|e| {
                NodeError::Package(format!("Failed to serialize manifest: {}", e))
            })?;
            let manifest_path = install_path.join("manifest.toml");
            std::fs::write(&manifest_path, &manifest_toml)
                .map_err(|e| NodeError::Package(format!("Failed to write manifest: {}", e)))?;
            cleaned = true;
            checks.push(CheckItem {
                name: "cleanup.dev_removed".to_string(),
                status: "ok".to_string(),
                detail: Some("dev flag removed from manifest".to_string()),
            });

            // Update in-memory manifest
            if let Some(info) = state.installed_agents.get_mut(&key) {
                info.manifest = manifest;
            }
        }

        // Clear recordings/ directory
        let recordings_dir = install_path.join("recordings");
        if recordings_dir.exists() {
            std::fs::remove_dir_all(&recordings_dir).ok();
            checks.push(CheckItem {
                name: "cleanup.recordings_cleared".to_string(),
                status: "ok".to_string(),
                detail: Some("recordings/ directory cleared".to_string()),
            });
        }

        // Reset config/settings.toml to defaults if it exists
        let config_dir = install_path.join("config");
        if config_dir.exists() {
            let settings_path = config_dir.join("settings.toml");
            if settings_path.exists() {
                let default_settings = "# ACowork Agent Configuration\n# See documentation for available options\n";
                std::fs::write(&settings_path, default_settings).ok();
                checks.push(CheckItem {
                    name: "cleanup.config_reset".to_string(),
                    status: "ok".to_string(),
                    detail: Some("config/settings.toml reset to defaults".to_string()),
                });
            }
        }
    }

    Ok(PrepareResult {
        checks,
        warnings,
        errors,
        cleaned,
    })
}

// ── S4.3: Publish build ───────────────────────────────────────────────

/// Build a .agent package from an installed agent directory.
///
/// ADR-073: the target is located by instance identity (`instance_id`,
/// falling back to the package id for legacy commands).
pub fn build_package(
    instance_id: &str,
    agent_id: &str,
    output_dir: &Path,
    sign: bool,
    key_dir: Option<&Path>,
    state: &NodeState,
) -> Result<BuildResult> {
    // ADR-073: the install table is keyed by instance identity.
    let key = instance_id.to_string();

    let info = state
        .installed_agents
        .get(&key)
        .ok_or_else(|| NodeError::AgentNotFound(key.clone()))?;

    let agent_dir = Path::new(&info.install_path);
    if !agent_dir.exists() {
        return Err(NodeError::Package(format!(
            "Agent install path does not exist: {}",
            agent_dir.display()
        )));
    }

    let output_filename = format!("{}-{}.agent", agent_id, info.version);
    let output_path = output_dir.join(&output_filename);

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            NodeError::Package(format!("Failed to create output directory: {}", e))
        })?;
    }

    let opts = PackageOptions::default(); // exclude conversations/config by default
    acowork_sign::packager::build_agent_package(agent_dir, &output_path, Some(&opts))
        .map_err(|e| NodeError::Sign(format!("Failed to build package: {}", e)))?;

    let mut file_size = std::fs::metadata(&output_path)
        .map(|m| m.len())
        .unwrap_or(0);

    // Optional signing
    let mut signed = false;
    if sign {
        let key_dir = key_dir.unwrap_or_else(|| {
            // Default: use dev key from agents/.signing-keys/
            Path::new("agents/.signing-keys")
        });
        let signed_path = output_dir.join(format!("{}-{}-signed.agent", agent_id, info.version));

        match acowork_sign::sign::sign_package(
            &output_path,
            &signed_path,
            key_dir,
            acowork_sign::keygen::KeyType::Developer,
        ) {
            Ok(()) => {
                std::fs::remove_file(&output_path).ok();
                std::fs::rename(&signed_path, &output_path).map_err(|e| {
                    NodeError::Package(format!("Failed to rename signed package: {}", e))
                })?;
                signed = true;
                file_size = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(file_size);
            }
            Err(e) => {
                tracing::warn!(
                    "Package signing failed (continuing with unsigned package): {}",
                    e
                );
            }
        }
    }

    Ok(BuildResult {
        output_path: output_path.to_string_lossy().to_string(),
        signed,
        file_size,
    })
}

/// Result of building an .agent package
#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildResult {
    pub output_path: String,
    pub signed: bool,
    pub file_size: u64,
}

/// Validate agent ID format (reverse-domain style)
fn is_valid_agent_id(agent_id: &str) -> bool {
    if agent_id.is_empty() || agent_id.len() > 128 {
        return false;
    }
    if !agent_id.contains('.') {
        return false;
    }
    agent_id
        .split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::InstalledAgent;

    fn create_test_agent(state: &mut NodeState, agent_id: &str, install_path: &str) {
        let install_path = Path::new(install_path);
        std::fs::create_dir_all(install_path.join("prompts")).unwrap();
        std::fs::create_dir_all(install_path.join("skills/search")).unwrap();
        std::fs::create_dir_all(install_path.join("config")).unwrap();

        std::fs::write(
            install_path.join("manifest.toml"),
            format!(
                r#"agent_id = "{}"
version = "1.0.0"
name = "Test Agent"
description = "A test agent"
author = "Test Author"
runtime_version = "0.1.0"
dev = false
[llm]
provider = "openai"
model = "gpt-4"
"#,
                agent_id,
            ),
        )
        .unwrap();
        std::fs::write(
            install_path.join("prompts/system.md"),
            "# System Prompt\nYou are helpful.",
        )
        .unwrap();
        std::fs::write(
            install_path.join("skills/search/SKILL.md"),
            "---\nname: search\n---\n# Search",
        )
        .unwrap();
        std::fs::write(install_path.join("config/settings.toml"), "custom = true").unwrap();

        let manifest = acowork_core::AgentManifest::from_toml(&format!(
            r#"agent_id = "{}"
version = "1.0.0"
name = "Test Agent"
description = "A test agent"
author = "Test Author"
runtime_version = "0.1.0"
[llm]
provider = "openai"
model = "gpt-4"
"#,
            agent_id
        ))
        .unwrap();

        state.add_installed(InstalledAgent {
            instance_id: format!("inst-{agent_id}"),
            agent_id: agent_id.to_string(),
            version: "1.0.0".to_string(),
            name: "Test Agent".to_string(),
            install_path: install_path.to_string_lossy().to_string(),
            manifest,
        });
    }

    #[test]
    fn test_prepare_publish_valid() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-prep-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let install_dir = temp_dir.join("installed");
        let mut state = NodeState::new(16);
        create_test_agent(&mut state, "com.test.weather", &install_dir.to_string_lossy());

        let result =
            prepare_publish("inst-com.test.weather", false, &mut state)
                .unwrap();
        assert!(
            result.errors.is_empty(),
            "Unexpected errors: {:?}",
            result.errors
        );
        assert!(!result.checks.is_empty());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prepare_publish_missing_fields() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-prep-err-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let install_dir = temp_dir.join("installed");
        std::fs::create_dir_all(&install_dir).unwrap();
        std::fs::write(
            install_dir.join("manifest.toml"),
            r#"agent_id = "com.test.invalid"
version = "1.0.0"
name = "Test"
description = ""
author = ""
runtime_version = "0.1.0"
[llm]
temperature = 0.7"#,
        )
        .unwrap();

        let mut state = NodeState::new(16);
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"agent_id = "com.test.invalid"
version = "1.0.0"
name = "Test"
description = ""
author = ""
runtime_version = "0.1.0"
[llm]
temperature = 0.7
"#,
        )
        .unwrap();
        state.add_installed(InstalledAgent {
            instance_id: "inst-com.test.invalid".to_string(),
            agent_id: "com.test.invalid".to_string(),
            version: "1.0.0".to_string(),
            name: "Test".to_string(),
            install_path: install_dir.to_string_lossy().to_string(),
            manifest,
        });

        let result =
            prepare_publish("inst-com.test.invalid", false, &mut state)
                .unwrap();
        assert!(
            !result.warnings.is_empty() || !result.errors.is_empty(),
            "Should have warnings or errors for missing prompts and empty fields"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prepare_clean_removes_dev() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-prep-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let install_dir = temp_dir.join("installed");
        std::fs::create_dir_all(install_dir.join("prompts")).unwrap();
        std::fs::create_dir_all(install_dir.join("recordings")).unwrap();
        std::fs::write(install_dir.join("recordings/test.rec"), b"dummy").unwrap();

        std::fs::write(
            install_dir.join("manifest.toml"),
            r#"agent_id = "com.test.dev"
version = "1.0.0"
name = "Dev Agent"
description = "Test"
author = "test"
runtime_version = "0.1.0"
dev = true
[llm]
provider = "openai"
model = "gpt-4"
"#,
        )
        .unwrap();

        let mut state = NodeState::new(16);
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"agent_id = "com.test.dev"
version = "1.0.0"
name = "Dev Agent"
description = "Test"
author = "test"
runtime_version = "0.1.0"
dev = true
[llm]
provider = "openai"
model = "gpt-4"
"#,
        )
        .unwrap();
        state.add_installed(InstalledAgent {
            instance_id: "inst-com.test.dev".to_string(),
            agent_id: "com.test.dev".to_string(),
            version: "1.0.0".to_string(),
            name: "Dev Agent".to_string(),
            install_path: install_dir.to_string_lossy().to_string(),
            manifest,
        });

        let result = prepare_publish("inst-com.test.dev", true, &mut state).unwrap();
        assert!(result.cleaned, "Should have performed cleanup");
        assert!(
            !state.installed_agents["inst-com.test.dev"].manifest.dev,
            "dev should be cleared"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_build_package_success() {
        let temp_dir =
            std::env::temp_dir().join(format!("acowork-test-build-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();

        let install_dir = temp_dir.join("installed");
        let output_dir = temp_dir.join("output");
        let mut state = NodeState::new(16);
        create_test_agent(&mut state, "com.test.weather", &install_dir.to_string_lossy());

        let result =
            build_package("inst-com.test.weather", "com.test.weather", &output_dir, false, None, &state)
                .unwrap();
        assert!(result.output_path.ends_with(".agent"));
        assert!(result.file_size > 0);
        assert!(!result.signed);
        assert!(Path::new(&result.output_path).exists());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_build_package_not_found() {
        let state = NodeState::new(16);

        let result = build_package(
            "",
            "com.test.nonexistent",
            Path::new("/tmp"),
            false,
            None,
            &state,
        );
        assert!(result.is_err());
    }
}
