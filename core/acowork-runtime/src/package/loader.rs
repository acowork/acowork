//! .bot ZIP package loader + manifest validation
//!
//! Loads .bot packages from ZIP files or extracted directories.
//! Validates manifest, verifies signature (delegated to acowork-sign),
//! and extracts prompt files.

use std::fs;
use std::path::{Path, PathBuf};

use acowork_core::AgentManifest;

use crate::error::{Result, RuntimeError};

/// Loaded package information
#[derive(Debug)]
pub struct LoadedPackage {
    /// Parsed and validated manifest
    pub manifest: AgentManifest,
    /// Path to extracted package directory
    pub package_dir: PathBuf,
}

/// Load .bot package from ZIP file or directory
pub fn load_package(package_path: &Path) -> Result<LoadedPackage> {
    if !package_path.exists() {
        return Err(RuntimeError::Package(format!(
            "Package path does not exist: {}",
            package_path.display()
        )));
    }

    let package_dir = if package_path.is_file() {
        // ZIP file — extract to temp directory
        extract_zip_package(package_path)?
    } else if package_path.is_dir() {
        // Already extracted directory
        package_path.to_path_buf()
    } else {
        return Err(RuntimeError::Package(format!(
            "Package path is neither a file nor a directory: {}",
            package_path.display()
        )));
    };

    // Parse manifest.toml
    let manifest = load_manifest(&package_dir)?;

    tracing::info!(
        agent_id = %manifest.agent_id,
        version = %manifest.version,
        name = %manifest.name,
        "Manifest loaded"
    );

    // Validate manifest
    manifest
        .validate()
        .map_err(|e| RuntimeError::Package(e.to_string()))?;

    // Verify signature (delegated to acowork-sign)
    // Phase 1: signature verification is optional — unsigned packages are allowed
    // but logged. Strict verification will be enforced in Phase 2.
    if has_signing_block(&package_dir) {
        tracing::info!("Package has signing block — signature present");
        // TODO: delegate to acowork-sign for verification
        // verify_package_signature(&package_dir)?;
    } else {
        tracing::warn!("Package is unsigned — skipping signature verification");
    }

    Ok(LoadedPackage {
        manifest,
        package_dir,
    })
}

/// Extract ZIP package to a temporary directory
fn extract_zip_package(zip_path: &Path) -> Result<PathBuf> {
    let file = fs::File::open(zip_path)
        .map_err(|e| RuntimeError::Package(format!("Failed to open ZIP file: {e}")))?;

    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| RuntimeError::Package(format!("Failed to read ZIP archive: {e}")))?;

    // Extract to a temp directory named after the ZIP file
    let zip_stem = zip_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("agent-package");
    let extract_dir = std::env::temp_dir().join(format!("acowork-agent-{zip_stem}"));

    // Clean up previous extraction if exists
    if extract_dir.exists() {
        fs::remove_dir_all(&extract_dir)
            .map_err(|e| RuntimeError::Package(format!("Failed to clean extract dir: {e}")))?;
    }

    fs::create_dir_all(&extract_dir)
        .map_err(|e| RuntimeError::Package(format!("Failed to create extract dir: {e}")))?;

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| RuntimeError::Package(format!("Failed to read ZIP entry {i}: {e}")))?;

        let out_path = match entry.enclosed_name() {
            Some(path) => extract_dir.join(path),
            None => continue,
        };

        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| RuntimeError::Package(format!("Failed to create dir: {e}")))?;
        } else {
            // Create parent directory if needed
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent).map_err(|e| {
                    RuntimeError::Package(format!("Failed to create parent dir: {e}"))
                })?;
            }

            let mut outfile = fs::File::create(&out_path)
                .map_err(|e| RuntimeError::Package(format!("Failed to create file: {e}")))?;

            std::io::copy(&mut entry, &mut outfile)
                .map_err(|e| RuntimeError::Package(format!("Failed to extract file: {e}")))?;
        }
    }

    tracing::debug!(dir = %extract_dir.display(), "ZIP extracted");
    Ok(extract_dir)
}

/// Load and parse manifest.toml from package directory.
///
/// `pub(crate)` so the L2 reload endpoint (`http::prompts`) can re-read
/// the manifest to resolve the skill mode when rebuilding the main-dialog
/// system prompt (ADR-063 §3.7.6 hot-reload).
pub(crate) fn load_manifest(package_dir: &Path) -> Result<AgentManifest> {
    let manifest_path = package_dir.join("manifest.toml");

    if !manifest_path.exists() {
        return Err(RuntimeError::Package(
            "manifest.toml not found in package".to_string(),
        ));
    }

    let toml_str = fs::read_to_string(&manifest_path)
        .map_err(|e| RuntimeError::Package(format!("Failed to read manifest.toml: {e}")))?;

    AgentManifest::from_toml(&toml_str).map_err(|e| RuntimeError::Package(e.to_string()))
}

/// Check if package directory contains a signing block
fn has_signing_block(package_dir: &Path) -> bool {
    // Check for META-INF/SIGNING.BLOCK in extracted directory
    package_dir.join("META-INF").join("SIGNING.BLOCK").exists()
}

/// List prompt files in package directory
pub fn list_prompt_files(package_dir: &Path) -> Result<Vec<PathBuf>> {
    let prompts_dir = package_dir.join("prompts");
    if !prompts_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let entries = fs::read_dir(&prompts_dir)
        .map_err(|e| RuntimeError::Package(format!("Failed to read prompts dir: {e}")))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| RuntimeError::Package(format!("Failed to read dir entry: {e}")))?;

        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext == "md" || ext == "txt")
        {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

/// List skill files in package directory
pub fn list_skill_files(package_dir: &Path) -> Result<Vec<PathBuf>> {
    let skills_dir = package_dir.join("skills");
    if !skills_dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    let entries = fs::read_dir(&skills_dir)
        .map_err(|e| RuntimeError::Package(format!("Failed to read skills dir: {e}")))?;

    for entry in entries {
        let entry =
            entry.map_err(|e| RuntimeError::Package(format!("Failed to read dir entry: {e}")))?;

        let path = entry.path();
        if path.is_dir() {
            let skill_md = path.join("SKILL.md");
            if skill_md.exists() {
                files.push(skill_md);
            }
        }
    }

    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn create_test_package_dir() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-package-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::create_dir_all(dir.join("skills").join("greeting")).unwrap();

        // Write manifest.toml
        fs::write(
            dir.join("manifest.toml"),
            r#"
agent_id = "com.test.demo"
version = "1.0.0"
name = "Demo Agent"
description = "A test agent"
author = "test"
runtime_version = "0.1.0"

[llm]
provider = "openai"
model = "gpt-4"

[[tools]]
name = "calculator"
"#,
        )
        .unwrap();

        // Write prompt files
        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a helpful assistant.",
        )
        .unwrap();
        fs::write(
            dir.join("prompts").join("constraints.md"),
            "Always be polite.",
        )
        .unwrap();

        // Write skill file
        fs::write(
            dir.join("skills").join("greeting").join("SKILL.md"),
            "# Greeting Skill\nSay hello.",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_load_package_from_directory() {
        let dir = create_test_package_dir();
        let loaded = load_package(&dir).unwrap();
        assert_eq!(loaded.manifest.agent_id, "com.test.demo");
        assert_eq!(loaded.manifest.name, "Demo Agent");
        assert!(loaded.package_dir.exists());
    }

    #[test]
    fn test_load_package_nonexistent_path() {
        let result = load_package(Path::new("/nonexistent/path"));
        assert!(result.is_err());
    }

    #[test]
    fn test_load_manifest_missing() {
        let dir = std::env::temp_dir().join("acowork-test-no-manifest");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let result = load_package(&dir);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("manifest.toml"));
    }

    #[test]
    fn test_list_prompt_files() {
        let dir = create_test_package_dir();
        let files = list_prompt_files(&dir).unwrap();
        assert_eq!(files.len(), 2);
    }

    #[test]
    fn test_list_skill_files() {
        let dir = create_test_package_dir();
        let files = list_skill_files(&dir).unwrap();
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn test_has_signing_block() {
        let dir = create_test_package_dir();
        assert!(!has_signing_block(&dir));

        // Add signing block
        fs::create_dir_all(dir.join("META-INF")).unwrap();
        fs::write(dir.join("META-INF").join("SIGNING.BLOCK"), b"test").unwrap();
        assert!(has_signing_block(&dir));
    }
}
