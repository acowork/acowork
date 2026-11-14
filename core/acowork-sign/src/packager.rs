//! Agent package builder with data isolation support.
//!
//! Builds an `.agent` ZIP package from an installed agent directory,
//! respecting `PackageOptions` to control which data items are included.
//! Private data (conversations, Episodes, private KnowledgeNodes) is
//! excluded by default to protect user privacy.

use std::fs;
use std::io::{Read, Write};
use std::path::Path;

use acowork_core::AgentManifest;
use acowork_core::packaging::{PackageOptions, should_exclude_path};

use crate::error::{Result, SignError};

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
    // ADR-104: refuse to emit a package whose declarations reference skills it
    // does not ship. Publishing is the last moment where the author still has
    // the sources in front of them; failing at install time instead would
    // surface the same defect to a user who cannot fix it.
    validate_declaration_graph(agent_dir)?;
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
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(archive_entry_time(options)?);

    // Walk the agent directory and add files that pass the exclusion filter.
    walk_and_add(agent_dir, agent_dir, &mut archive, zip_options, options)?;

    archive.finish()?;
    Ok(())
}

/// Declaration-graph gate for packaging (ADR-104).
///
/// Checks the half of the graph the packager can see:
///
/// - `manifest.toml` must parse and pass `AgentManifest::validate()`;
/// - every `capabilities.*.skills` entry must name a directory under
///   `skills/` in the package.
///
/// The tool-dependency half of the graph needs `SKILL.md` frontmatter, whose
/// parser lives in the Runtime, so it is checked at install/refresh time
/// instead (warnings only — a missing tool degrades one skill, it does not
/// make the package invalid).
fn validate_declaration_graph(agent_dir: &Path) -> Result<()> {
    let manifest_path = agent_dir.join("manifest.toml");
    let manifest_str = fs::read_to_string(&manifest_path)?;
    let manifest = AgentManifest::from_toml(&manifest_str)
        .map_err(|e| SignError::InvalidPackage(format!("invalid manifest.toml: {e}")))?;

    // Skill identity = the `skills/<dir>/` name (ADR-104 §Capability): the
    // manifest links to skills by name, and the directory name IS that name.
    let mut skill_names: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(agent_dir.join("skills")) {
        for entry in entries.flatten() {
            if entry.path().is_dir()
                && let Some(name) = entry.file_name().to_str()
            {
                skill_names.push(name.to_string());
            }
        }
    }

    let refs: Vec<&str> = skill_names.iter().map(String::as_str).collect();
    let errors = manifest.validate_capability_skills(&refs);
    if errors.is_empty() {
        return Ok(());
    }
    Err(SignError::InvalidPackage(format!(
        "manifest capabilities reference skills that are not in the package: {} (shipped skills: {})",
        errors.join("; "),
        if refs.is_empty() {
            "<none>".to_string()
        } else {
            refs.join(", ")
        }
    )))
}

/// Timestamp stored in every archive entry.
///
/// `zip` gives a fresh entry the wall clock (`DateTime::default_for_write`), so
/// two builds of one source tree produced different bytes — and because the
/// signature covers the archive, different signatures as well. A package could
/// not be reproduced from its commit, and two builders could not agree on what
/// they shipped. Entry order is now sorted and the compression method is
/// fixed; this was the last clock in the path.
///
/// `PackageOptions::timestamp_epoch` pins it explicitly, else
/// `SOURCE_DATE_EPOCH` (the reproducible-builds convention) does: CI and the
/// determinism guard pass the source commit's timestamp, so "same commit, same
/// bytes" becomes something anyone can check. With neither, the stamp is the
/// real build time, which is what a local build wants to show.
pub(crate) fn archive_entry_time(options: &PackageOptions) -> Result<zip::DateTime> {
    let pinned: Option<Result<i64>> = match options.timestamp_epoch {
        Some(epoch) => Some(Ok(epoch)),
        None => std::env::var("SOURCE_DATE_EPOCH")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(|raw| {
                raw.trim().parse::<i64>().map_err(|_| {
                    SignError::InvalidPackage(format!(
                        "SOURCE_DATE_EPOCH is not a unix timestamp: {raw}"
                    ))
                })
            }),
    };
    let epoch: i64 = match pinned {
        Some(v) => v?,
        None => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };
    entry_time_from_epoch(epoch)
}

/// Convert a unix timestamp to the ZIP entry format, refusing a value the
/// format cannot express rather than substituting a wrong date.
fn entry_time_from_epoch(epoch: i64) -> Result<zip::DateTime> {
    use chrono::{Datelike, Timelike};

    let civil = chrono::DateTime::from_timestamp(epoch, 0).ok_or_else(|| {
        SignError::InvalidPackage(format!("timestamp out of range for a ZIP entry: {epoch}"))
    })?;
    let civil = civil.naive_utc();
    zip::DateTime::from_date_and_time(
        civil.year() as u16,
        civil.month() as u8,
        civil.day() as u8,
        civil.hour() as u8,
        civil.minute() as u8,
        civil.second() as u8,
    )
    .map_err(|e| {
        SignError::InvalidPackage(format!("cannot express timestamp {epoch} as a ZIP date: {e}"))
    })
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
    // Sorted, and only files: the archive is a published artifact, so two
    // builds of one commit must produce one byte string. `read_dir` hands back
    // entries in filesystem order, which differs between NTFS and ext4 and
    // even between two runs on the same tree, and the signature covers the
    // archive — an unordered walk makes "same commit, same bytes" untestable
    // and turns every rebuild into a spurious package change.
    let mut entries = fs::read_dir(current_dir)?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
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
        // A real manifest, not a stub: `build_agent_package` validates it
        // (ADR-104) — a fixture that could never be published would make the
        // packaging tests assert behaviour on packages the tool rejects.
        fs::write(
            dir.join("manifest.toml"),
            br#"agent_id = "com.test"
version = "1.0.0"
name = "Test Agent"
description = "packaging fixture"
author = "test"
runtime_version = "0.1.0"
"#,
        )
        .unwrap();
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

    /// One source tree, two builds, one byte string.
    ///
    /// The property the packaging path did not have: `zip` stamped a fresh
    /// entry with the wall clock and the walk followed filesystem order, so
    /// rebuilding an unchanged commit produced different bytes — and because
    /// the signature covers the archive, a different signature as well. The
    /// shell builder and the PowerShell builder shipped different packages for
    /// identical source.
    ///
    /// Pinned through the option, not `SOURCE_DATE_EPOCH`, on purpose: a test
    /// that mutates process environment races with its siblings, and an
    /// earlier version of this check passed by accident because both builds
    /// landed inside the same two-second ZIP timestamp tick.
    #[test]
    fn one_tree_builds_to_one_byte_string() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pkg-deterministic");
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let agent_dir = tmp_dir.join("agent");
        create_test_agent_dir(&agent_dir);

        let opts = PackageOptions {
            timestamp_epoch: Some(1_700_000_000),
            ..Default::default()
        };

        let first = tmp_dir.join("first.agent");
        let second = tmp_dir.join("second.agent");
        build_agent_package_with_options(&agent_dir, &first, &opts).unwrap();
        build_agent_package_with_options(&agent_dir, &second, &opts).unwrap();
        assert_eq!(
            fs::read(&first).unwrap(),
            fs::read(&second).unwrap(),
            "two builds of one tree must produce identical bytes"
        );

        // The pin must be doing the work; otherwise this test would also pass
        // if the timestamp were dropped from the archive entirely.
        let other = PackageOptions {
            timestamp_epoch: Some(1_600_000_000),
            ..Default::default()
        };
        let third = tmp_dir.join("third.agent");
        build_agent_package_with_options(&agent_dir, &third, &other).unwrap();
        assert_ne!(
            fs::read(&first).unwrap(),
            fs::read(&third).unwrap(),
            "the pinned timestamp must actually reach the archive"
        );

        // Content still dominates: one pin, two trees, two packages.
        fs::write(agent_dir.join("prompts/system.md"), "changed").unwrap();
        let fourth = tmp_dir.join("fourth.agent");
        build_agent_package_with_options(&agent_dir, &fourth, &opts).unwrap();
        assert_ne!(
            fs::read(&first).unwrap(),
            fs::read(&fourth).unwrap(),
            "a changed tree must not reuse the old package"
        );

        // The two-builds-equal check above can pass by luck when both builds
        // land in the same two-second ZIP tick, which is exactly how an
        // earlier version of this test fooled me. This half cannot: it reads
        // the stamp back out of the archive, so an ignored pin — the wall
        // clock leaking in — fails here instead of in a mirror diff months
        // later. 1_700_000_000 is 2023-11-14T22:13:20Z.
        let mut archive = zip::ZipArchive::new(std::io::BufReader::new(
            fs::File::open(&first).unwrap(),
        ))
        .unwrap();
        for i in 0..archive.len() {
            let t = archive.by_index(i).unwrap().last_modified().unwrap();
            assert_eq!(
                (t.year(), t.month(), t.day(), t.hour(), t.minute()),
                (2023, 11, 14, 22, 13),
                "entry {} carries the build clock, not the pinned timestamp",
                i
            );
        }

        let _ = fs::remove_dir_all(&tmp_dir);
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
            timestamp_epoch: None,
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

    // ── ADR-104: declaration-graph gate at packaging time ──────────────

    const VALID_MANIFEST: &str = r#"agent_id = "com.test.graph"
version = "1.0.0"
name = "Graph"
description = "graph gate test"
author = "test"
runtime_version = "0.1.0"
labels = ["test"]

[capabilities.research]
description = "Search the web"
skills = ["search"]
"#;

    fn agent_dir_with(dir: &Path, manifest: &str, skills: &[&str]) {
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts/system.md"), b"You are a test agent.").unwrap();
        fs::write(dir.join("manifest.toml"), manifest.as_bytes()).unwrap();
        for skill in skills {
            fs::create_dir_all(dir.join("skills").join(skill)).unwrap();
            fs::write(
                dir.join("skills").join(skill).join("SKILL.md"),
                b"---\nname: x\n---\nbody\n",
            )
            .unwrap();
        }
    }

    #[test]
    fn graph_gate_accepts_a_consistent_package() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-graph-ok");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(&agent_dir, VALID_MANIFEST, &["search"]);

        validate_declaration_graph(&agent_dir).expect("consistent package must pass");
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn graph_gate_rejects_dangling_capability_skill() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-graph-dangling");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(&agent_dir, VALID_MANIFEST, &["other-skill"]);

        let err = validate_declaration_graph(&agent_dir).expect_err("dangling ref must fail");
        let msg = err.to_string();
        assert!(msg.contains("search"), "error must name the missing skill: {msg}");
        assert!(
            msg.contains("other-skill"),
            "error must list what the package actually ships: {msg}"
        );
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn graph_gate_rejects_invalid_manifest() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-graph-badmanifest");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(&agent_dir, "agent_id = \"no-dots\"", &[]);

        assert!(
            validate_declaration_graph(&agent_dir).is_err(),
            "a manifest that fails validate() must not be packaged"
        );
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn graph_gate_allows_packages_without_capabilities() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-graph-nocaps");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(
            &agent_dir,
            r#"agent_id = "com.test.nocaps"
version = "1.0.0"
name = "No Caps"
description = "no capability declarations"
author = "test"
runtime_version = "0.1.0"
"#,
            &[],
        );

        validate_declaration_graph(&agent_dir).expect("no capabilities => nothing to cross-check");
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    /// The public entry point must run the gate before writing the archive —
    /// a broken package must leave no file behind.
    #[test]
    fn build_agent_package_refuses_to_write_an_invalid_package() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-graph-entry");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(&agent_dir, VALID_MANIFEST, &["not-search"]);

        let out = tmp_dir.join("out.agent");
        let err = build_agent_package(&agent_dir, &out, None).expect_err("must refuse");
        assert!(err.to_string().contains("skills"));
        assert!(!out.exists(), "no archive may be written for a rejected package");
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn build_agent_package_writes_a_valid_package() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-graph-valid");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(&agent_dir, VALID_MANIFEST, &["search"]);

        let out = tmp_dir.join("out.agent");
        build_agent_package(&agent_dir, &out, None).expect("valid package builds");
        assert!(out.exists());
        let names = list_archive_entries(&out);
        assert!(names.contains(&"manifest.toml".to_string()));
        assert!(names.contains(&"skills/search/SKILL.md".to_string()));
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    /// Two builds of one tree are one byte string.
    ///
    /// The archive is signed, so an unordered walk makes the signature depend
    /// on filesystem enumeration order: the same commit would publish different
    /// bytes per machine, per run, and every rebuild would look like a package
    /// change. This is the property the build scripts used to violate by
    /// shelling out to `zip` / `Compress-Archive` instead of this packer.
    #[test]
    fn two_builds_of_one_tree_are_one_byte_string() {
        let tmp_dir = std::env::temp_dir().join("acowork-test-pack-determinism");
        let _ = fs::remove_dir_all(&tmp_dir);
        let agent_dir = tmp_dir.join("agent");
        agent_dir_with(
            &agent_dir,
            VALID_MANIFEST,
            &["search", "index", "archive", "beta"],
        );

        let first = tmp_dir.join("first.agent");
        let second = tmp_dir.join("second.agent");
        build_agent_package(&agent_dir, &first, None).unwrap();
        build_agent_package(&agent_dir, &second, None).unwrap();

        let a = fs::read(&first).unwrap();
        let b = fs::read(&second).unwrap();
        assert_eq!(a.len(), b.len(), "same tree, same length");
        assert_eq!(a, b, "same tree, same bytes");

        // And the order is the sorted one, not whatever the directory handed back.
        let names = list_archive_entries(&first);
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "entries are written in sorted order");

        let _ = fs::remove_dir_all(&tmp_dir);
    }
}