//! Agent package builder with data isolation support.
//!
//! Builds an `.agent` ZIP package from an installed agent directory,
//! respecting `PackageOptions` to control which data items are included.
//! Private data (conversations, Episodes, private KnowledgeNodes) is
//! excluded by default to protect user privacy.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use acowork_core::packaging::{PackageOptions, should_exclude_path};

use crate::error::Result;

// ---------------------------------------------------------------------------
// Package builder
// ---------------------------------------------------------------------------

/// Build an `.agent` package from an installed agent directory.
///
/// Walks the agent directory, applies exclusion rules based on `options`,
/// and writes the resulting ZIP to `output_path`. If `options` is `None`,
/// uses `PackageOptions::default()` (which excludes private data).
///
/// # Directory traversal rules
///
/// | Path pattern        | Behavior                                |
/// |---------------------|-----------------------------------------|
/// | `memory/`           | Always excluded (agent-private DB)      |
/// | `workspace/`        | Always excluded                         |
/// | `runtime/`          | Always excluded                         |
/// | `*.log`, `*.tmp`    | Always excluded                         |
/// | `conversations/`    | Excluded unless `include_conversations` |
/// | `config/`           | Excluded unless `include_config`        |
/// | Everything else     | Included                                |
///
/// # Agent-private data
///
/// `memory/` holds the agent-private SQLite store (ADR-082) and is always
/// excluded: a distributable package carries no agent state.
pub fn build_agent_package(
    agent_dir: &Path,
    output_path: &Path,
    options: Option<&PackageOptions>,
) -> Result<()> {
    let opts = options.cloned().unwrap_or_default();
    build_agent_package_with_options(agent_dir, output_path, &opts)
}

/// Build an `.agent` package with explicit `PackageOptions`.
fn build_agent_package_with_options(
    agent_dir: &Path,
    output_path: &Path,
    options: &PackageOptions,
) -> Result<()> {
    let output_file = fs::File::create(output_path)?;
    let mut archive = zip::ZipWriter::new(output_file);
    let zip_options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // Walk the agent directory and add files that pass the exclusion filter.
    walk_and_add(agent_dir, agent_dir, &mut archive, zip_options, options)?;

    archive.finish()?;
    Ok(())
}

/// Recursively walk a directory and add files to the ZIP archive
/// that pass the exclusion filter.
fn walk_and_add(
    base_dir: &Path,
    current_dir: &Path,
    archive: &mut zip::ZipWriter<fs::File>,
    zip_options: zip::write::SimpleFileOptions,
    options: &PackageOptions,
) -> Result<()> {
    let entries = fs::read_dir(current_dir)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let relative = path.strip_prefix(base_dir).unwrap_or(&path);
        let relative_str = relative.to_string_lossy();

        // Apply exclusion rules
        if should_exclude_path(&relative_str, options) {
            continue;
        }

        if path.is_dir() {
            walk_and_add(base_dir, &path, archive, zip_options, options)?;
        } else {
            // Add file to archive
            let mut file = fs::File::open(&path)?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)?;

            // Use forward slashes for ZIP paths (cross-platform compatibility)
            let archive_path = relative_str.replace('\\', "/");
            archive.start_file(&archive_path, zip_options)?;
            archive.write_all(&buffer)?;
        }
    }

    Ok(())
}

/// Check if a conversations directory exists and has JSONL files.
///
/// Useful for UI to show data sizes before packaging.
pub fn has_conversation_data(agent_dir: &Path) -> bool {
    let conv_dir = agent_dir.join("conversations");
    if !conv_dir.is_dir() {
        return false;
    }

    match fs::read_dir(&conv_dir) {
        Ok(mut entries) => entries.any(|e| {
            e.map(|entry| {
                entry
                    .path()
                    .extension()
                    .map(|ext| ext == "jsonl")
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        }),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_agent_dir(dir: &Path) {
        // Create the standard agent directory structure
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::create_dir_all(dir.join("skills/search")).unwrap();
        fs::create_dir_all(dir.join("conversations")).unwrap();
        fs::create_dir_all(dir.join("memory")).unwrap();
        fs::create_dir_all(dir.join("workspace")).unwrap();
        fs::create_dir_all(dir.join("runtime")).unwrap();
        fs::create_dir_all(dir.join("config")).unwrap();

        // Write some test files
        fs::write(dir.join("manifest.toml"), b"agent_id = \"com.test\"").unwrap();
        fs::write(dir.join("prompts/system.md"), b"You are a test agent.").unwrap();
        fs::write(dir.join("skills/search/skill.md"), b"Search skill.").unwrap();
        fs::write(
            dir.join("conversations/20260501_abc.jsonl"),
            b"{\"_type\":\"session_meta\",\"session_id\":\"20260501_abc\"}\n{\"role\":\"user\",\"content\":\"Hello\"}\n",
        )
        .unwrap();
        fs::write(dir.join("memory/private.sqlite"), b"<binary data>").unwrap();
        fs::write(dir.join("workspace/state.json"), b"{}").unwrap();
        fs::write(dir.join("runtime/lock.pid"), b"12345").unwrap();
        fs::write(dir.join("config/settings.toml"), b"key = \"value\"").unwrap();
        fs::write(dir.join("debug.log"), b"log entry").unwrap();
        fs::write(dir.join("temp.tmp"), b"temp data").unwrap();
    }

    fn list_archive_entries(zip_path: &Path) -> Vec<String> {
        let file = fs::File::open(zip_path).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        let mut names = Vec::new();
        for i in 0..archive.len() {
            names.push(archive.by_index(i).unwrap().name().to_string());
        }
        names.sort();
        names
    }

    #[test]
    fn test_package_with_conversations_excluded() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pkg-conv-excluded");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");
        create_test_agent_dir(&agent_dir);

        let output_path = tmp_dir.join("output.agent");
        let opts = PackageOptions::default(); // conversations excluded by default

        build_agent_package_with_options(&agent_dir, &output_path, &opts).unwrap();

        let entries = list_archive_entries(&output_path);

        // Should include normal files
        assert!(entries.contains(&"manifest.toml".to_string()));
        assert!(entries.contains(&"prompts/system.md".to_string()));
        assert!(entries.contains(&"skills/search/skill.md".to_string()));

        // Should NOT include conversations
        assert!(
            !entries.iter().any(|e| e.starts_with("conversations/")),
            "conversations should be excluded: {:?}",
            entries
        );

        // Should NOT include always-excluded dirs
        assert!(!entries.iter().any(|e| e.starts_with("memory/")));
        assert!(!entries.iter().any(|e| e.starts_with("workspace/")));
        assert!(!entries.iter().any(|e| e.starts_with("runtime/")));

        // Should NOT include log/tmp files
        assert!(!entries.iter().any(|e| e.ends_with(".log")));
        assert!(!entries.iter().any(|e| e.ends_with(".tmp")));

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_package_with_conversations_included() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pkg-conv-included");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");
        create_test_agent_dir(&agent_dir);

        let output_path = tmp_dir.join("output.agent");
        let opts = PackageOptions {
            include_conversations: true,
            ..Default::default()
        };

        build_agent_package_with_options(&agent_dir, &output_path, &opts).unwrap();

        let entries = list_archive_entries(&output_path);

        // Should include conversations when opted in
        assert!(
            entries.iter().any(|e| e.starts_with("conversations/")),
            "conversations should be included when opted in: {:?}",
            entries
        );

        // Still should NOT include always-excluded dirs
        assert!(!entries.iter().any(|e| e.starts_with("memory/")));
        assert!(!entries.iter().any(|e| e.starts_with("workspace/")));
        assert!(!entries.iter().any(|e| e.starts_with("runtime/")));

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_package_with_config_included() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pkg-config-included");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");
        create_test_agent_dir(&agent_dir);

        let output_path = tmp_dir.join("output.agent");
        let opts = PackageOptions {
            include_config: true,
            ..Default::default()
        };

        build_agent_package_with_options(&agent_dir, &output_path, &opts).unwrap();

        let entries = list_archive_entries(&output_path);

        // Should include config when opted in
        assert!(
            entries.iter().any(|e| e.starts_with("config/")),
            "config should be included when opted in: {:?}",
            entries
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_package_always_excludes_memory_workspace_runtime() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pkg-always-exclude");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");
        create_test_agent_dir(&agent_dir);

        let output_path = tmp_dir.join("output.agent");
        // Every option the type offers is switched on: a `memory/` directory
        // left over from the pre-ADR-101 layout still must not travel
        // (ADR-101 decision 11).
        let opts = PackageOptions {
            include_conversations: true,
            include_config: true,
        };

        build_agent_package_with_options(&agent_dir, &output_path, &opts).unwrap();

        let entries = list_archive_entries(&output_path);

        assert!(
            !entries.iter().any(|e| e.starts_with("memory/")),
            "memory/ should always be excluded"
        );
        assert!(
            !entries.iter().any(|e| e.starts_with("workspace/")),
            "workspace/ should always be excluded"
        );
        assert!(
            !entries.iter().any(|e| e.starts_with("runtime/")),
            "runtime/ should always be excluded"
        );
        assert!(
            !entries.iter().any(|e| e.ends_with(".log")),
            "*.log should always be excluded"
        );
        assert!(
            !entries.iter().any(|e| e.ends_with(".tmp")),
            "*.tmp should always be excluded"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_package_with_none_options_uses_default() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pkg-none-opts");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");
        create_test_agent_dir(&agent_dir);

        let output_path = tmp_dir.join("output.agent");

        build_agent_package(&agent_dir, &output_path, None).unwrap();

        let entries = list_archive_entries(&output_path);

        // With None → default options → conversations excluded
        assert!(
            !entries.iter().any(|e| e.starts_with("conversations/")),
            "conversations should be excluded with default options"
        );

        // Normal files should be included
        assert!(entries.contains(&"manifest.toml".to_string()));

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn test_has_conversation_data() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-has-conv");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");

        // No conversations dir
        assert!(!has_conversation_data(&agent_dir));

        // Empty conversations dir
        fs::create_dir_all(agent_dir.join("conversations")).unwrap();
        assert!(!has_conversation_data(&agent_dir));

        // With JSONL file
        fs::write(agent_dir.join("conversations/20260501_abc.jsonl"), b"test").unwrap();
        assert!(has_conversation_data(&agent_dir));

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}

