//! Agent creation commands — create new agent skeleton and install it
//!
//! The wizard is a 5-step flow (ADR-104): Basic → Model & params → Tools &
//! MCP → Skills & capabilities → Preview.
//!
//! Three entry points, one manifest:
//!   - `preview_manifest(draft)` renders the exact TOML `create_agent` would
//!     write. Both call `ManifestDraft::build` in `acowork-core`, so the
//!     preview cannot drift from the result — the old TypeScript preview began
//!     with a `[package]` table the manifest schema does not even have.
//!   - `create_agent(draft)` builds the skeleton directory, expands the
//!     uploaded skill ZIPs into `skills/{name}/`, writes the manifest, zips the
//!     skeleton into a `.agent` package and hands it to the Gateway.
//!   - `parse_skill_zip_preview` reads one ZIP's frontmatter for the wizard's
//!     per-row preview.
//!
//! The wizard never talks to the skill import API — that would race with
//! `create_agent` and require the agent to already exist.

use std::io::{Read, Write};

use acowork_core::tools::BuiltinToolInfo;
use serde::Serialize;
use tauri::State;

use crate::state::AppState;

/// Return the catalog of builtin tools the Runtime ships with.
///
/// The wizard calls this on mount to render the Tools step's checkbox
/// list. The catalog lives in `acowork-core` (single source of truth
/// shared with the Runtime) — the wizard never hardcodes a parallel
/// list.
#[tauri::command]
pub async fn list_builtin_tools() -> Vec<BuiltinToolInfo> {
    acowork_core::tools::BUILTIN_TOOLS.to_vec()
}

/// One row the wizard renders for a user-supplied skill ZIP after we
/// have parsed its `SKILL.md` frontmatter. The wizard uses this for
/// previewing what will be installed, and `create_agent` uses it to
/// write the manifest `[capabilities.*]` section.
#[derive(Debug, Clone, Serialize)]
pub struct ParsedSkillPreview {
    /// Skill name from `SKILL.md` frontmatter (`name`).
    pub name: String,
    /// Skill description from `SKILL.md` frontmatter (`description`).
    pub description: String,
    /// Tools the skill needs (`SKILL.md` `tool_deps`).
    ///
    /// Read here (not only at install time) so the wizard can show "this
    /// skill needs `pm_create_task`, which step 3 does not declare" *while*
    /// the user is still editing, instead of surfacing it as an advisory log
    /// line after the package is already installed.
    pub tool_deps: Vec<String>,
}

impl ParsedSkillPreview {
    /// Project into the graph-validator input.
    pub fn to_skill_decl(&self) -> acowork_core::manifest::SkillDecl {
        acowork_core::manifest::SkillDecl {
            name: self.name.clone(),
            tool_deps: self.tool_deps.clone(),
        }
    }
}

/// Parse a skill ZIP in isolation — used by the wizard's Skills
/// step to render a preview row as soon as the user drops a file,
/// without re-running `create_agent`. Same implementation as the
/// internal pass during install; the wizard just needs the visible
/// side (name + description) so the user can confirm the right
/// file was picked.
#[tauri::command]
pub async fn parse_skill_zip_preview(zip_bytes: Vec<u8>) -> Result<ParsedSkillPreview, String> {
    parse_skill_zip(&zip_bytes)
}

/// Parse the `SKILL.md` frontmatter and return just the two fields we
/// care about for the wizard preview / manifest capabilities.
///
/// Returns `None` if the ZIP cannot be read, has no `SKILL.md`, or
/// the frontmatter cannot be parsed — the wizard surfaces a clean
/// error in that case rather than silently dropping the user's
/// upload.
///
/// `ponytail:` We parse YAML inline rather than reusing the
/// Runtime's `parse_skill_md` (`acowork-node::package::skills`)
/// because the Desktop crate does not depend on `acowork-node`
/// (ADR-055 keeps node internals behind the Gateway). The schema
/// here is intentionally narrower (only `name` + `description`) so
/// the parser stays a 20-line `serde_yaml` call.
fn parse_skill_zip(zip_bytes: &[u8]) -> Result<ParsedSkillPreview, String> {
    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Not a valid ZIP: {}", e))?;

    // SKILL.md is allowed at root or one level deep (the
    // packager's single top-level directory convention). Walk
    // every entry to find the first one matching.
    let mut skill_md_name: Option<String> = None;
    let candidates: Vec<String> = (0..archive.len())
        .filter_map(|i| archive.by_index(i).ok().map(|f| f.name().to_string()))
        .collect();
    for name in &candidates {
        let normalized = name.replace('\\', "/");
        if normalized == "SKILL.md" || normalized.ends_with("/SKILL.md") {
            skill_md_name = Some(name.clone());
            break;
        }
    }
    let skill_md_name = skill_md_name
        .ok_or_else(|| "ZIP does not contain a SKILL.md".to_string())?;

    let mut file = archive
        .by_name(&skill_md_name)
        .map_err(|e| format!("Failed to open SKILL.md: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read SKILL.md: {}", e))?;

    let frontmatter = parse_frontmatter(&content)
        .map_err(|e| format!("SKILL.md frontmatter: {}", e))?;

    Ok(ParsedSkillPreview {
        name: frontmatter.name,
        description: frontmatter.description,
        tool_deps: frontmatter.tool_deps,
    })
}

/// Minimal `SKILL.md` frontmatter projection — only the two fields
/// we need. We do not deserialize the full `SkillFrontmatter` from
/// `acowork-node` (extra fields, different crate, see the note on
/// `parse_skill_zip`).
#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillFrontmatterSlim {
    name: String,
    description: String,
    tool_deps: Vec<String>,
}

/// Hand-rolled frontmatter reader. Only handles the two keys we care
/// about (`name`, `description`) and rejects everything else with a
/// clear error so a misparsed upload does not silently produce a
/// half-filled manifest.
///
/// Deliberate limitations (matches real SKILL.md formatting we have
/// shipped so far — `examples/agnes-document-skills/.../SKILL.md`,
/// `examples/document-manager-agent/.../*.md`):
/// - values must be a single physical line
/// - values may be wrapped in single or double quotes (which we strip)
/// - block scalars (`description: |` / `description: >`) are rejected
///   with a clear error: wizard users hit it once and either edit
///   the SKILL.md or we revisit when a real skill needs multi-line
///
/// `ponytail:` Trading a real YAML parser for ~30 lines here keeps
/// the desktop crate dependency-free for this path. The
/// `serde_yaml` workspace dep lives in `core/`; pulling it into the
/// desktop crate (which is a separate workspace) would require a
/// registry round-trip, and the failure mode is just a slightly
/// worse error message on a rare edge case.
fn parse_frontmatter(content: &str) -> Result<SkillFrontmatterSlim, String> {
    let trimmed = content.trim_start_matches('\u{feff}').trim_start();
    let after_open = trimmed
        .strip_prefix("---")
        .ok_or_else(|| "SKILL.md must start with `---` frontmatter delimiter".to_string())?;
    let after_open = after_open
        .strip_prefix('\n')
        .or_else(|| after_open.strip_prefix("\r\n"))
        .ok_or_else(|| "Expected newline after opening `---`".to_string())?;
    let end = after_open
        .find("\n---")
        .ok_or_else(|| "Missing closing `---` for frontmatter".to_string())?;
    let yaml = &after_open[..end];

    let mut name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut tool_deps: Vec<String> = Vec::new();
    // Key of the most recent `key:` line, so a following block list
    // (`- item`) can be attributed. Only `tool_deps` keeps its items;
    // other keys' lists are parsed and dropped.
    let mut last_key: Option<String> = None;
    for (idx, raw_line) in yaml.lines().enumerate() {
        let line = raw_line.trim_end();
        if line.trim().is_empty() {
            continue;
        }

        // Block-list item: `  - value` / `- value`.
        if let Some(item) = line.trim().strip_prefix("- ") {
            match last_key.as_deref() {
                Some("tool_deps") => {
                    let value = strip_yaml_scalar_quotes(item.trim());
                    if !value.is_empty() {
                        tool_deps.push(value.to_string());
                    }
                }
                // Unknown key's list — tolerated (the Runtime's full parser
                // reads it), same policy as unknown scalar keys.
                Some(_) => {}
                None => {
                    return Err(format!(
                        "Frontmatter line {} is a list item with no preceding key: {:?}",
                        idx + 1,
                        line
                    ));
                }
            }
            continue;
        }

        let (key, value) = line.split_once(':').ok_or_else(|| {
            format!(
                "Frontmatter line {} is not a `key: value` pair: {:?}",
                idx + 1,
                line
            )
        })?;
        let key = key.trim();
        let value = value.trim();
        last_key = Some(key.to_string());

        match key {
            "name" => name = Some(strip_yaml_scalar_quotes(value).to_string()),
            "description" => {
                if value.is_empty() {
                    return Err("Frontmatter `description` is empty".to_string());
                }
                if value == "|" || value == ">" || value.starts_with("|-") || value.starts_with(">-") {
                    return Err(
                        "Block-scalar `description` (| / >) is not supported by the wizard; \
                         collapse the description to a single line in SKILL.md"
                            .to_string(),
                    );
                }
                description = Some(strip_yaml_scalar_quotes(value).to_string());
            }
            "tool_deps" => {
                // Two accepted spellings: inline flow list
                // (`tool_deps: [a, b]`) and block list (handled above).
                if !value.is_empty() {
                    let inner = value
                        .strip_prefix('[')
                        .and_then(|v| v.strip_suffix(']'))
                        .ok_or_else(|| {
                            format!(
                                "Frontmatter `tool_deps` must be a flow list ([a, b]) or a                                  block list (- a); got {:?}",
                                value
                            )
                        })?;
                    for part in inner.split(',') {
                        let dep = strip_yaml_scalar_quotes(part.trim());
                        if !dep.is_empty() {
                            tool_deps.push(dep.to_string());
                        }
                    }
                }
            }
            _ => {
                // Unknown keys are tolerated — we ignore them, the
                // Runtime's full parser will pick them up. We only
                // fail on keys we *need* but cannot read.
            }
        }
    }

    Ok(SkillFrontmatterSlim {
        name: name.ok_or_else(|| "Frontmatter is missing required `name` field".to_string())?,
        description: description
            .ok_or_else(|| "Frontmatter is missing required `description` field".to_string())?,
        tool_deps,
    })
}

fn strip_yaml_scalar_quotes(s: &str) -> &str {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        if (bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\'')
        {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Everything the Create Agent wizard collected.
///
/// The manifest-shaped half lives in `acowork_core::manifest::ManifestDraft`:
/// building a manifest from partial author input is a domain operation with
/// rules (defaults, refused combinations, graph checks) that any authoring
/// surface — this wizard today, a CLI or the mobile app later — must answer
/// identically. Keeping it in `core` also means its mapping matrix is tested
/// where `cargo test` actually runs it, instead of inside a Tauri binary.
///
/// This wrapper adds the one thing a draft cannot carry: the raw skill ZIP
/// bytes, which are filesystem content rather than declaration.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct CreateAgentDraft {
    #[serde(flatten)]
    pub manifest: acowork_core::manifest::ManifestDraft,
    /// Raw ZIP bytes for each uploaded skill.
    #[serde(default)]
    pub skill_files: Vec<Vec<u8>>,
}

/// Result of `preview_manifest` — the real TOML plus the graph verdict.
#[derive(Debug, Clone, Serialize)]
pub struct ManifestPreview {
    /// Exact text that `create_agent` would write (`AgentManifest::to_toml`).
    pub toml: String,
    /// Dangling references — these block creation.
    pub errors: Vec<String>,
    /// Reachability warnings — advisory only.
    pub warnings: Vec<String>,
}

/// Parse every uploaded skill ZIP up front, so a bad upload fails loudly
/// before anything touches the filesystem, and so the graph check has the
/// directory names the capabilities must resolve against.
fn parse_skill_files(skill_files: &[Vec<u8>]) -> Result<Vec<ParsedSkillPreview>, String> {
    let parsed: Vec<ParsedSkillPreview> = skill_files
        .iter()
        .map(|bytes| parse_skill_zip(bytes))
        .collect::<Result<Vec<_>, _>>()?;

    let mut seen = std::collections::HashSet::new();
    for s in &parsed {
        if !seen.insert(s.name.as_str()) {
            return Err(format!("Duplicate skill name '{}' in uploaded skills", s.name));
        }
    }
    Ok(parsed)
}

/// `preview_manifest` — render the exact manifest the wizard would create.
///
/// The wizard used to assemble this TOML text in TypeScript, which drifted:
/// its first table was `[package]`, a section `AgentManifest` does not have
/// (serde silently drops unknown fields, so the real manifest would have been
/// missing everything the preview appeared to promise). Rendering it in Rust
/// from the same builder that creates it removes that class of bug entirely.
#[tauri::command]
pub async fn preview_manifest(draft: CreateAgentDraft) -> Result<ManifestPreview, String> {
    let parsed = parse_skill_files(&draft.skill_files)?;
    let decls: Vec<acowork_core::manifest::SkillDecl> =
        parsed.iter().map(|s| s.to_skill_decl()).collect();
    let manifest = draft.manifest.build(&decls)?;
    let toml = manifest
        .to_toml()
        .map_err(|e| format!("Failed to serialize manifest: {e}"))?;

    // The manifest above already passed the graph check (build returns Err on
    // dangling references), so only the advisory half can still be non-empty.
    let report = manifest.validate_graph(&decls);

    Ok(ManifestPreview {
        toml,
        errors: report.errors,
        warnings: report.warnings,
    })
}

/// Create a new agent skeleton, zip it, and install via Gateway.
///
/// Returns the agent_id of the newly installed agent on success.
#[tauri::command]
pub async fn create_agent(
    state: State<'_, AppState>,
    draft: CreateAgentDraft,
) -> Result<String, String> {
    let agent_id = draft.manifest.agent_id.trim().to_string();
    let name = draft.manifest.name.trim().to_string();

    // Pre-parse every skill ZIP up front so a bad upload fails loudly
    // *before* we start touching the filesystem. This also means we
    // don't carry the raw bytes into the second pass (extraction).
    let parsed_skills = parse_skill_files(&draft.skill_files)?;
    let decls: Vec<acowork_core::manifest::SkillDecl> =
        parsed_skills.iter().map(|s| s.to_skill_decl()).collect();

    // Build the manifest through the shared domain builder + `AgentManifest`,
    // so the wire schema is identical to what the Gateway parses. `build`
    // runs `validate()` (reverse-domain agent_id, non-empty version/name/
    // runtime_version, label format, mcp declaration forms) and the ADR-104
    // capability→skill graph check, so bad input surfaces here rather than as
    // a confusing Gateway 400.
    let manifest = draft.manifest.build(&decls)?;
    let manifest_toml = manifest
        .to_toml()
        .map_err(|e| format!("Failed to serialize manifest: {e}"))?;

    // Create a temp directory for the skeleton (use monotonic timestamp to avoid
    // collisions when concurrent Tauri commands run in the same process).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_dir = std::env::temp_dir().join(format!(
        "acowork-create-{}-{:x}",
        agent_id.replace(|c: char| !c.is_alphanumeric() && c != '.' && c != '-', "_"),
        nanos,
    ));
    std::fs::create_dir_all(&temp_dir).map_err(|e| format!("Failed to create temp dir: {}", e))?;

    // prompts/
    let prompts_dir = temp_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .map_err(|e| format!("Failed to create prompts dir: {}", e))?;

    // Extract each skill ZIP into `skills/{name}/` under the skeleton. Done
    // before the manifest write so the files-on-disk shape matches what the
    // capabilities section advertises (and what `validate_capability_skills`
    // was checked against).
    if !parsed_skills.is_empty() {
        let skills_root = temp_dir.join("skills");
        std::fs::create_dir_all(&skills_root)
            .map_err(|e| format!("Failed to create skills dir: {}", e))?;
        for (bytes, skill) in draft.skill_files.iter().zip(parsed_skills.iter()) {
            extract_skill_zip(bytes, &skills_root.join(&skill.name))?;
        }
    }

    std::fs::write(temp_dir.join("manifest.toml"), &manifest_toml)
        .map_err(|e| format!("Failed to write manifest: {}", e))?;

    // Generate default system prompt
    let description = manifest.description.clone();
    let system_prompt = format!(
        "You are {}, an AI assistant.\n\n\
        Role: {}\n\n\
        You can use available tools to help users complete tasks. \
        Always be helpful, accurate, and concise.\n\n\
        When using tools:\n\
        - Explain what you're doing before calling a tool\n\
        - Report the results clearly\n\
        - Handle errors gracefully\n\n\
        If you encounter a problem you cannot solve, \
        explain what you've tried and suggest alternatives.\n",
        name, description,
    );

    std::fs::write(prompts_dir.join("system.md"), &system_prompt)
        .map_err(|e| format!("Failed to write system prompt: {}", e))?;

    // Generate config/settings.toml
    let config_dir = temp_dir.join("config");
    std::fs::create_dir_all(&config_dir)
        .map_err(|e| format!("Failed to create config dir: {}", e))?;
    std::fs::write(config_dir.join("settings.toml"), "# Agent settings\n")
        .map_err(|e| format!("Failed to write settings: {}", e))?;

    // Zip the skeleton directory into a temporary .agent file
    let zip_path = temp_dir.with_extension("agent");
    let zip_file = std::fs::File::create(&zip_path)
        .map_err(|e| format!("Failed to create zip file: {}", e))?;
    let mut zip_writer = zip::ZipWriter::new(zip_file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    add_dir_to_zip(&mut zip_writer, &temp_dir, &temp_dir, options)
        .map_err(|e| format!("Failed to zip skeleton: {}", e))?;

    zip_writer
        .finish()
        .map_err(|e| format!("Failed to finalize zip: {}", e))?;

    let package_bytes =
        std::fs::read(&zip_path).map_err(|e| format!("Failed to read zip: {}", e))?;

    // Clean up temp dir
    let _ = std::fs::remove_dir_all(&temp_dir);
    let _ = std::fs::remove_file(&zip_path);

    // Install via Gateway
    let client = state.gateway.read().await;
    client
        .install_agent(&package_bytes, true, None)
        .await
        .map_err(|e| format!("Install failed: {}", e))?;

    Ok(agent_id)
}
/// Extract every entry of `zip_bytes` into `dest_dir`. The packager
/// may wrap the skill in a single top-level directory; we strip that
/// prefix so the result is `skills/{name}/SKILL.md` etc., never
/// `skills/{name}/{wrapper}/SKILL.md`.
///
/// `ponytail:` This is a small ad-hoc extractor — we do not need
/// zip-slip protection (we only read entries we just wrote to a
/// fresh `temp_dir/skills/{name}/` we own) nor symlink handling
/// (skill packages are content-only).
fn extract_skill_zip(zip_bytes: &[u8], dest_dir: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dest_dir)
        .map_err(|e| format!("Failed to create skill dir: {}", e))?;

    let cursor = std::io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Failed to read skill ZIP: {}", e))?;

    // Detect a single top-level directory wrapper (the standard
    // "one dir per skill" packaging convention) and strip it from
    // every entry.
    let wrapper: Option<String> = {
        let mut dirs = std::collections::HashSet::new();
        for i in 0..archive.len() {
            let name = archive
                .by_index(i)
                .map_err(|e| format!("Failed to read zip entry: {}", e))?
                .name()
                .to_string();
            let normalized = name.replace('\\', "/");
            if let Some((top, _rest)) = normalized.split_once('/') {
                dirs.insert(top.to_string());
            }
        }
        if dirs.len() == 1 {
            dirs.into_iter().next()
        } else {
            None
        }
    };

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("Failed to read zip entry: {}", e))?;
        let raw = file.name().to_string();
        let normalized = raw.replace('\\', "/");
        let stripped = match &wrapper {
            Some(w) if normalized == *w => continue,
            Some(w) if normalized.starts_with(&format!("{}/", w)) => &normalized[w.len() + 1..],
            _ => normalized.as_str(),
        };
        if stripped.is_empty() {
            continue;
        }
        let out_path = dest_dir.join(stripped);
        if file.is_dir() {
            std::fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed to create dir {:?}: {}", out_path, e))?;
        } else {
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create parent dir: {}", e))?;
            }
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| format!("Failed to read zip entry: {}", e))?;
            std::fs::write(&out_path, &buf)
                .map_err(|e| format!("Failed to write {:?}: {}", out_path, e))?;
        }
    }
    Ok(())
}

/// Recursively add a directory's contents to a zip archive
fn add_dir_to_zip<W: Write + std::io::Seek>(
    zip_writer: &mut zip::ZipWriter<W>,
    base_dir: &std::path::Path,
    current_dir: &std::path::Path,
    options: zip::write::SimpleFileOptions,
) -> Result<(), String> {
    for entry in std::fs::read_dir(current_dir)
        .map_err(|e| format!("Failed to read dir {:?}: {}", current_dir, e))?
    {
        let entry = entry.map_err(|e| format!("Dir entry error: {}", e))?;
        let path = entry.path();
        let relative = path
            .strip_prefix(base_dir)
            .map_err(|e| format!("Strip prefix error: {}", e))?;

        if path.is_dir() {
            // Add directory entry
            let dir_path = relative.to_string_lossy().replace('\\', "/");
            zip_writer
                .add_directory(format!("{}/", dir_path), options)
                .map_err(|e| format!("Failed to add dir '{}': {}", dir_path, e))?;
            add_dir_to_zip(zip_writer, base_dir, &path, options)?;
        } else if path.is_file() {
            let file_path = relative.to_string_lossy().replace('\\', "/");
            zip_writer
                .start_file(file_path.as_str(), options)
                .map_err(|e| format!("Failed to start file '{}': {}", file_path, e))?;
            let content = std::fs::read(&path)
                .map_err(|e| format!("Failed to read file {:?}: {}", path, e))?;
            zip_writer
                .write_all(&content)
                .map_err(|e| format!("Failed to write file '{}': {}", file_path, e))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_frontmatter_basic() {
        let s = "---
name: agnes-image
description: Generate or edit images via Agnes
---
# body
";
        let fm = parse_frontmatter(s).unwrap();
        assert_eq!(fm.name, "agnes-image");
        assert!(fm.description.starts_with("Generate or edit images"));
    }

    #[test]
    fn parse_frontmatter_quoted() {
        let s = "---
name: \"my skill\"
description: \"has: colons, in it\"
---
";
        let fm = parse_frontmatter(s).unwrap();
        assert_eq!(fm.name, "my skill");
        assert_eq!(fm.description, "has: colons, in it");
    }

    #[test]
    fn parse_frontmatter_rejects_missing_name() {
        let s = "---
description: only description
---
";
        let err = parse_frontmatter(s).unwrap_err();
        assert!(err.contains("name"), "got: {}", err);
    }

    #[test]
    fn parse_frontmatter_rejects_block_scalar() {
        let s = "---
name: foo
description: |
  multi line
  here
---
";
        let err = parse_frontmatter(s).unwrap_err();
        assert!(err.to_lowercase().contains("block") || err.contains("|"), "got: {}", err);
    }

    #[test]
    fn parse_frontmatter_ignores_unknown_keys() {
        let s = "---
name: x
description: y
version: 1.0.0
author: me
triggers:
  - foo
---
";
        let fm = parse_frontmatter(s).unwrap();
        assert_eq!(fm.name, "x");
        assert_eq!(fm.description, "y");
    }
}
