//! System prompt builder (from prompts/ + skills/)
//!
//! Assembles the complete system prompt from:
//! 1. prompts/system.md — Agent identity definition
//! 2. prompts/constraints.md — Behavioral constraints
//! 3. prompts/*.md — Additional prompt sections
//! 4. skills/*/SKILL.md — Skill instructions
//!
//! # Package-level prompt overrides (ADR-063)
//!
//! The 5 files listed in [`OVERRIDABLE_PROMPTS`] are the canonical
//! "package-declared overrides" for the hardcoded LLM prompt constants in
//! [`crate::prompt`] and the downstream grafeo/memory modules. When a
//! `.agent` package provides one of these files in `prompts/`, it
//! replaces the built-in default at runtime via the
//! `AgentCore.<field>` resolution chain (see ADR-063 §3.2).
//!
//! These files are also **deliberately excluded** from the main dialog
//! system prompt assembled here — they are task-specific directives (a
//! summarization rule, a search directive, a title-style preference, …)
//! that would pollute every LLM call if folded into the dialog identity
//! section. Exclusion and "loaded as override" use the **same exact
//! filename match** so the two views never disagree.
//!
//! [`COMPACTION_PROMPT_FILE`] (`prompts/summary.md`) is the original entry
//! in this list (ADR-053). ADR-063 originally added 8 more (total 9); the
//! 3 grafeo-specific overrides (`extraction.md`,
//! `conflict-classification.md`, `generalization.md`) were removed by
//! ADR-068 because LLM→memory is now exclusively mediated by the
//! Episode-only `memory_store` path — grafeo's offline
//! `EpisodicDistiller` owns its own internal prompts.

use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::error::{Result, RuntimeError};
use crate::skills::parser::SkillRegistry;

/// Filename of the agent-specific compaction prompt inside `prompts/`.
///
/// When present, this file replaces the built-in
/// [`crate::prompt::COMPACTION_SYSTEM_PROMPT`] as the system prompt for
/// context compaction and episode distillation. It is a summarization
/// directive, not a dialog identity section, so [`build_system_prompt_with_mode`]
/// skips it when assembling the main system prompt.
pub const COMPACTION_PROMPT_FILE: &str = "summary.md";

/// Canonical list of package-level LLM prompt overrides (ADR-063).
///
/// Each tuple is `(filename_in_prompts_dir, semantic_description)`. Files
/// in this list are loaded by [`load_optional_prompt`] and **excluded**
/// from the main dialog system prompt assembled by
/// [`build_system_prompt_with_mode`] — keeping the exclusion set, the
/// "task-instruction override" set, and the Debug panel list (`§3.7`) all
/// in sync via this single source of truth.
///
/// Adding a new overridable prompt requires **three** coordinated changes:
/// 1. Add an entry here.
/// 2. Add the corresponding `Arc<RwLock<Option<String>>>` field on
///    [`crate::agent::AgentCore`] (see ADR-063 §3.7.5).
/// 3. Add the LLM-call-site resolution (`core.<field>.read().unwrap()
///    .as_deref().unwrap_or(<builtin const>)`).
///
/// Tests in this module pin all three behaviours against this list.
pub const OVERRIDABLE_PROMPTS: &[(&str, &str)] = &[
    // ADR-053 — compaction / distillation system prompt
    (
        "summary.md",
        "compaction/distillation system prompt (ADR-053)",
    ),
    // ADR-063 — Runtime prompt.rs constants (2)
    (
        "compact-template.md",
        "COMPACT_PROMPT (must keep {messages_text} placeholder)",
    ),
    ("title.md", "TITLE_PROMPT"),
    // ADR-068 — only the abstention override remains of the original ADR-063
    // grafeo/memory set. The other three grafeo
    // overrides (`extraction.md`, `conflict-classification.md`,
    // `generalization.md`) were removed: see module-level docs.
    // ADR-100 — the memory side no longer carries a default to override: the
    // retrieval-side abstention chain was removed with decision 5, so an
    // absent `abstention.md` means no guidance is injected at all.
    ("abstention.md", "abstention guidance (no built-in default)"),
    // ADR-100 — `distiller-merge.md` retired: the EpisodicDistiller prompt is
    // owned by the memory service, which reads it from its own instance
    // directory. The Runtime no longer lists or dispatches it.
    // ADR-099 — Chat Router persona. Replaces ONLY the persona section of
    // the chat-layer system prompt; the Runtime-owned role/tool/decision
    // kernel is not overridable. Excluded from the execute system prompt by
    // the same `is_overridable_prompt` rule as the entries above.
    ("chat.md", "chat router persona (ADR-099)"),
];

/// O(1) lookup set of overridable filenames. Built lazily on first access
/// via [`overridable_filenames`] so the cold-start cost is one allocation
/// of five `&str`s — cheaper than scanning [`OVERRIDABLE_PROMPTS`] on every
/// file in `prompts/`.
fn overridable_filenames() -> &'static HashSet<&'static str> {
    use std::sync::OnceLock;
    static CACHE: OnceLock<HashSet<&'static str>> = OnceLock::new();
    CACHE.get_or_init(|| OVERRIDABLE_PROMPTS.iter().map(|(f, _)| *f).collect())
}

/// True iff `filename` is one of the package-level override files in
/// [`OVERRIDABLE_PROMPTS`]. Used by [`build_system_prompt_with_mode`] to
/// skip these files when assembling the main dialog system prompt.
pub fn is_overridable_prompt(filename: &str) -> bool {
    overridable_filenames().contains(filename)
}

/// Generic loader for any file in [`OVERRIDABLE_PROMPTS`].
///
/// Returns `None` when the file is missing or contains only whitespace —
/// the caller then falls back to the built-in default constant. Surrounding
/// whitespace is stripped so a file consisting only of blank lines behaves
/// like an absent file.
///
/// An existing-but-unreadable file (permissions, invalid UTF-8) also yields
/// `None` but is logged as a warning — a broken package must not be silently
/// masked by the fallback. This matches the ADR-053 [`load_compaction_prompt`]
/// behaviour and is the single shared implementation now.
///
/// # Security
///
/// The filename is matched **exactly** against [`OVERRIDABLE_PROMPTS`] —
/// any caller passing a path with `/`, `\`, or `..` will hit the
/// "unknown prompt" branch and return `None`. Callers that need stronger
/// validation (e.g. before writing) should additionally call
/// [`is_overridable_prompt`] (see `http::prompts` write handler).
pub fn load_optional_prompt(package_dir: &Path, filename: &str) -> Option<String> {
    if !is_overridable_prompt(filename) {
        tracing::warn!(
            filename,
            "load_optional_prompt called with non-overridable filename; ignoring"
        );
        return None;
    }
    let path = package_dir.join("prompts").join(filename);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        // File (or the prompts/ dir) absent → normal fallback, no log spam.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        // File exists but is unreadable (permissions) or not valid UTF-8 —
        // surface it so a broken package is not silently masked.
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read overridable prompt; falling back to built-in default"
            );
            return None;
        }
    };
    let trimmed = content.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Load the agent-specific compaction prompt from `prompts/summary.md`.
///
/// Thin wrapper over [`load_optional_prompt`] — kept as a separate symbol
/// for ADR-053 backward compatibility (every existing call site uses this
/// name; see `startup/agent_init.rs` Phase A and `startup/session_init.rs`
/// Phase B).
///
/// Behaviour is identical to pre-ADR-063:
/// - Missing file or whitespace-only file → `None` → caller falls back to
///   the built-in [`crate::prompt::COMPACTION_SYSTEM_PROMPT`].
/// - Unreadable file → `None` with a `tracing::warn!` so a broken package
///   is not silently masked.
/// - Filename is matched **exactly** (`summary.md`) — the same criterion
///   [`build_system_prompt_with_mode`] uses to exclude it from the main
///   dialog system prompt.
pub fn load_compaction_prompt(package_dir: &Path) -> Option<String> {
    load_optional_prompt(package_dir, COMPACTION_PROMPT_FILE)
}

/// L2 reload: re-read every file in [`OVERRIDABLE_PROMPTS`] from
/// `package_dir/prompts/` and write the result into the matching
/// `Arc<RwLock<Option<String>>>` field on the given [`AgentCore`].
///
/// Single source of truth for the `filename → AgentCore field` dispatch
/// table — shared by:
///
/// - The HTTP `POST /agents/{id}/prompts/reload` handler in
///   `http::prompts`, which serves the Debug panel "刷新" button without
///   requiring DevMode to be enabled (the reload is a **package-level**
///   operation, not a debug-session one).
/// - `DebugService::reload_prompts` (`usecases::debug_service_impl`),
///   which Phase C boot calls once after `enable_debug_mode` to
///   refresh the just-loaded defaults from disk.
///
/// `Err(_)` is reserved for "the dispatch table itself is broken" —
/// i.e. a future maintainer added a filename to [`OVERRIDABLE_PROMPTS`]
/// without teaching this function about the matching field. Per-file
/// I/O problems stay `Ok` with a `None` value written into the slot,
/// matching [`load_optional_prompt`] semantics so a broken package
/// never silently poisons a session.
///
/// [`AgentCore`]: crate::agent::agent_core::AgentCore
pub fn reload_prompts_into_core(
    package_dir: &Path,
    core: &Arc<crate::agent::agent_core::AgentCore>,
) -> std::result::Result<(), String> {
    for (filename, _desc) in OVERRIDABLE_PROMPTS {
        let loaded = load_optional_prompt(package_dir, filename);
        // Dispatch is explicit (not a macro) because the field names
        // differ per entry and the Arc handles are independent. The
        // catch-all `_` is intentionally omitted so a future drift
        // between OVERRIDABLE_PROMPTS and the AgentCore field set
        // surfaces as a compile error here at the call site — the
        // exact same drift that a `DebugError::Internal` would hide.
        match *filename {
            "summary.md" => {
                *core.compaction_prompt.write().unwrap() = loaded;
            }
            "compact-template.md" => {
                *core.compact_template.write().unwrap() = loaded;
            }
            "title.md" => {
                *core.title_prompt.write().unwrap() = loaded;
            }
            "abstention.md" => {
                *core.abstention_prompt.write().unwrap() = loaded;
            }
            "chat.md" => {
                *core.chat_persona.write().unwrap() = loaded;
            }
            other => {
                return Err(format!(
                    "reload_prompts_into_core: unknown overridable filename `{other}` — \
                     update the dispatch table in package/prompt_builder.rs \
                     in lockstep with AgentCore's field set"
                ));
            }
        }
    }
    Ok(())
}

/// Build system prompt from package files (default: Manual skill mode).
///
/// Backward-compatible wrapper that defaults to `SkillMode::Manual`,
/// meaning no skill content is injected into the system prompt.
/// For explicit mode control, use [`build_system_prompt_with_mode`].
///
/// Manual mode = no skills summary; this wrapper only returns the dialog
/// `base` so existing callers keep their `Result<String>` shape.
pub fn build_system_prompt(package_dir: &Path) -> Result<String> {
    Ok(build_system_prompt_with_mode(package_dir, acowork_core::SkillMode::Manual)?.base)
}

/// System-prompt parts assembled by [`build_system_prompt_with_mode`].
///
/// ADR-094: the dialog base (`prompts/*.md`) and the Progressive-mode
/// skills summary are returned as **separate fields** so the caller can
/// render them as independent `ContextBuilder` sections. This mirrors
/// the existing `tool_definitions` separation (also pushed as its own
/// section via `with_tools`): a single mega-string system prompt would
/// prevent `compute_section_sizes` from emitting a distinct
/// `skill_instructions` entry, which is what the usage popover and
/// Debug panel render.
///
/// Returned as a struct (not a tuple) so the call sites stay readable
/// when both fields are non-trivial strings.
pub struct SystemPromptParts {
    /// `prompts/*.md` sections joined with `---` separators. Does NOT
    /// include the skills summary.
    pub base: String,
    /// Progressive-mode skills summary (name + description list). `None`
    /// in Manual mode, when the package has no `skills/` dir, when the
    /// skills dir fails to load, or when the registry is empty.
    pub skills_summary: Option<String>,
}

/// Build system prompt from package files with explicit skill mode.
///
/// Reads prompt files in alphabetical order and concatenates them into
/// `SystemPromptParts.base`. Skill injection is returned separately
/// (`skills_summary`) so the caller can place it on a distinct
/// `ContextBuilder` section — see [`SystemPromptParts`] docs.
///
/// `skill_mode` controls skills behavior:
/// - `Manual`: `skills_summary = None`.
/// - `Progressive`: `skills_summary = Some(compact name+description list)`
///   when the package has a `skills/` dir with at least one SKILL.md.
///
/// [`COMPACTION_PROMPT_FILE`] is excluded — it is the compaction directive,
/// not a dialog section.
pub fn build_system_prompt_with_mode(
    package_dir: &Path,
    skill_mode: acowork_core::SkillMode,
) -> Result<SystemPromptParts> {
    let mut sections = Vec::new();

    // Load prompt files
    let prompts_dir = package_dir.join("prompts");
    if prompts_dir.exists() {
        let mut prompt_files = collect_markdown_files(&prompts_dir)?;
        prompt_files.sort();

        for path in &prompt_files {
            // Use filename (without extension) as section header
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown");

            // Skip package-level overrides (ADR-063): any filename in
            // `OVERRIDABLE_PROMPTS` is a task-instruction directive
            // (compaction, search, title, extraction, …) and would
            // pollute every LLM call if folded into the dialog identity
            // section. The same set is loaded by `load_optional_prompt`
            // and excluded here — exact filename match keeps the two
            // views symmetric.
            //
            // Case variants (`SUMMARY.md`, `Title.md`) and other
            // extensions (`summary.txt`) fall through as ordinary
            // prompt sections — only the canonical filenames in
            // `OVERRIDABLE_PROMPTS` are excluded.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_overridable_prompt)
            {
                continue;
            }

            let content = fs::read_to_string(path).map_err(|e| {
                RuntimeError::Package(format!(
                    "Failed to read prompt file {}: {e}",
                    path.display()
                ))
            })?;

            sections.push(format!("## {name}\n\n{content}"));
        }
    }

    // ADR-094: load skills as a SEPARATE return field, not embedded into
    // `base`. Embedding would defeat `compute_section_sizes` (it would
    // not see a `skill_instructions` section) and hide the skills usage
    // from the popover and Debug snapshot. `base` therefore only carries
    // `prompts/*.md` content.
    let skills_dir = package_dir.join("skills");
    let skills_summary = if skills_dir.exists() {
        match skill_mode {
            acowork_core::SkillMode::Manual => {
                tracing::debug!(
                    skills_dir = %skills_dir.display(),
                    "Skill mode is Manual — skipping skill injection"
                );
                None
            }
            acowork_core::SkillMode::Progressive => {
                match SkillRegistry::load_from_dir(&skills_dir) {
                    Ok(registry) => {
                        let summary = registry.build_skill_summary();
                        if summary.is_empty() {
                            None
                        } else {
                            Some(summary)
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            skills_dir = %skills_dir.display(),
                            error = %e,
                            "Failed to load skills for summary injection"
                        );
                        None
                    }
                }
            }
        }
    } else {
        None
    };

    let base = if sections.is_empty() {
        // Default system prompt if no files found
        crate::prompt::PROMPT_BUILDER_FALLBACK.to_string()
    } else {
        sections.join("\n\n---\n\n")
    };

    Ok(SystemPromptParts {
        base,
        skills_summary,
    })
}

/// Compute the Progressive-mode skills summary payload from the
/// current package + workdir. Pure — no I/O side effects, no
/// dispatch — so unit tests can pin its contract with a synthetic
/// package dir, and so the HTTP layer (hot-reload handlers) can call
/// it from any of the live-update paths (system-prompt reload,
/// built-in-tools toggle, ...).
///
/// Resolution priority mirrors Phase A startup:
/// 1. `agent_tools.json::load_skill.enabled` (user runtime override —
///    toggling the `load_skill` Switch in the Desktop Tools panel)
/// 2. `manifest.skills.progressive` (package author default)
///
/// `load_skill` is the single source of truth for "should the LLM
/// know what skills exist at session start" — there's no separate
/// `skills_auto_load` config field any more. Enabling the
/// `load_skill` tool is the toggle; flipping it dispatches
/// `UpdateSkillInstructions` alongside `UpdateBuiltinTools` (see
/// `http::server::broadcast_builtin_tools_update`).
///
/// Returns:
/// - `Ok(Some(summary))` in Progressive mode with `skills/` populated
/// - `Ok(None)` in Manual mode or when the package has no `skills/` dir
///   (caller dispatches `UpdateSkillInstructions { instructions: None }`
///   to clear the live section)
/// - `Err(msg)` on manifest load or prompt build failure — caller logs
///   and skips the dispatch (on-disk file is authoritative for next boot)
pub fn compute_skill_instructions_payload(
    package_dir: &Path,
    work_dir: &Path,
) -> std::result::Result<Option<String>, String> {
    let manifest = crate::package::loader::load_manifest(package_dir)
        .map_err(|e| format!("manifest load failed: {e}"))?;
    let skill_mode = crate::cli::resolve_skill_mode(&manifest, &work_dir.to_string_lossy());
    let prompt_parts = build_system_prompt_with_mode(package_dir, skill_mode)
        .map_err(|e| format!("prompt build failed: {e}"))?;
    Ok(prompt_parts.skills_summary)
}

/// Collect markdown/text files from a directory
fn collect_markdown_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    let entries = fs::read_dir(dir)
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

    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn create_test_package(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acowork-test-prompt-builder-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::create_dir_all(dir.join("skills").join("greeting")).unwrap();

        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a weather assistant.",
        )
        .unwrap();
        fs::write(
            dir.join("prompts").join("constraints.md"),
            "Always respond in the user's language.",
        )
        .unwrap();
        fs::write(
            dir.join("skills").join("greeting").join("SKILL.md"),
            "# Greeting\nBe friendly.",
        )
        .unwrap();

        dir
    }

    #[test]
    fn test_build_system_prompt_with_files() {
        let dir = create_test_package("default");
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("weather assistant"));
        assert!(prompt.contains("user's language"));
        // Default mode is Manual — skill content should NOT be injected
        assert!(!prompt.contains("Greeting"));
    }

    #[test]
    fn test_build_system_prompt_manual_mode() {
        let dir = create_test_package("manual");
        let parts = build_system_prompt_with_mode(&dir, acowork_core::SkillMode::Manual).unwrap();
        assert!(parts.base.contains("weather assistant"));
        assert!(parts.skills_summary.is_none());
        // Skills content must NEVER leak into the dialog base.
        assert!(!parts.base.contains("Greeting"));
        assert!(!parts.base.contains("Skill:"));
    }

    #[test]
    fn test_build_system_prompt_progressive_mode() {
        let dir = std::env::temp_dir().join("acowork-test-prompt-progressive");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::create_dir_all(dir.join("skills").join("greeting")).unwrap();

        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test assistant.",
        )
        .unwrap();

        // Write a valid SKILL.md with YAML frontmatter
        fs::write(
            dir.join("skills").join("greeting").join("SKILL.md"),
            r#"---
name: greeting
description: Greet users warmly
triggers:
  - hello
---

# Greeting Skill

Be friendly and welcoming.
"#,
        )
        .unwrap();

        let parts =
            build_system_prompt_with_mode(&dir, acowork_core::SkillMode::Progressive).unwrap();
        // Dialog base: only the prompts/ content, no skills summary mixed in.
        assert!(parts.base.contains("You are a test assistant."));
        assert!(!parts.base.contains("## Available Skills"));
        assert!(!parts.base.contains("Greet users warmly"));
        assert!(!parts.base.contains("Be friendly and welcoming."));
        // Skills summary: a separate, independent field.
        let skills = parts
            .skills_summary
            .expect("Progressive must populate skills_summary");
        assert!(skills.contains("## Available Skills"));
        assert!(skills.contains("greeting"));
        assert!(skills.contains("Greet users warmly"));
        // Full instructions should NOT appear in progressive summary.
        assert!(!skills.contains("Be friendly and welcoming."));
    }

    #[test]
    fn test_build_system_prompt_progressive_mode_no_skills_dir() {
        // Package with prompts/ only, no skills/. Progressive mode
        // must return skills_summary = None, not error.
        let dir = std::env::temp_dir().join("acowork-test-prompt-progressive-no-skills");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test assistant.",
        )
        .unwrap();
        let parts =
            build_system_prompt_with_mode(&dir, acowork_core::SkillMode::Progressive).unwrap();
        assert!(parts.base.contains("You are a test assistant."));
        assert!(parts.skills_summary.is_none());
    }

    #[test]
    fn test_build_system_prompt_empty_dir() {
        let dir = std::env::temp_dir().join("acowork-test-prompt-empty");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let prompt = build_system_prompt(&dir).unwrap();
        assert_eq!(prompt, "You are a helpful AI assistant.");
    }

    // ----- reload_prompts_into_core (ADR-063 §3.7.5/§3.7.6) -----
    //
    // Unit-level coverage for the free function that both the HTTP
    // reload handler (`POST /agents/{id}/prompts/reload`) and
    // `DebugService::reload_prompts` delegate to. Integration tests in
    // `tests/prompts_reload_e2e.rs` cover the HTTP envelope; these
    // tests pin the dispatch table (filename -> AgentCore field) and
    // the empty/partial/full override semantics.

    #[test]
    fn test_reload_prompts_into_core_with_empty_package_dir_writes_none_to_all_5_fields() {
        let core = make_test_agent_core();
        let empty_dir = std::env::temp_dir().join(format!(
            "acowork-test-reload-prompts-empty-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&empty_dir);
        fs::create_dir_all(&empty_dir).unwrap();

        // Seed each field with a sentinel so we can prove the reload
        // overwrites with `None` (rather than leaving the sentinel in
        // place because of a missed write).
        *core.compaction_prompt.write().unwrap() = Some("SENTINEL".into());
        *core.compact_template.write().unwrap() = Some("SENTINEL".into());
        *core.title_prompt.write().unwrap() = Some("SENTINEL".into());
        *core.abstention_prompt.write().unwrap() = Some("SENTINEL".into());

        reload_prompts_into_core(&empty_dir, &core).expect("empty-dir reload must succeed");

        // Every field must be `None` — no `prompts/*.md` exists, so
        // the loader returned `None` for each, and the reload wrote
        // that `None` into the slot (not the original sentinel).
        assert!(core.compaction_prompt.read().unwrap().is_none());
        assert!(core.compact_template.read().unwrap().is_none());
        assert!(core.title_prompt.read().unwrap().is_none());
        assert!(core.abstention_prompt.read().unwrap().is_none());

        let _ = fs::remove_dir_all(&empty_dir);
    }

    #[test]
    fn test_reload_prompts_into_core_with_all_overrides_writes_each_field() {
        let core = make_test_agent_core();
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-reload-prompts-all6-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();

        // One per `OVERRIDABLE_PROMPTS` entry, in the canonical order
        // (matches the table at the top of this module).
        // ADR-068: 3 grafeo overrides removed — see module docs.
        let fixtures: &[(&str, &str)] = &[
            ("summary.md", "summary-from-disk"),
            ("compact-template.md", "compact-template-from-disk"),
            ("title.md", "title-from-disk"),
            ("abstention.md", "abstention-from-disk"),
            ("chat.md", "chat-persona-from-disk"),
        ];
        for (file, content) in fixtures {
            fs::write(dir.join("prompts").join(file), content).unwrap();
        }

        reload_prompts_into_core(&dir, &core).expect("full reload must succeed");

        // Every field must equal the disk content — proves the
        // dispatch table wires each filename to the correct
        // AgentCore field.
        assert_eq!(
            core.compaction_prompt.read().unwrap().as_deref(),
            Some("summary-from-disk")
        );
        assert_eq!(
            core.compact_template.read().unwrap().as_deref(),
            Some("compact-template-from-disk")
        );
        assert_eq!(
            core.title_prompt.read().unwrap().as_deref(),
            Some("title-from-disk")
        );
        assert_eq!(
            core.abstention_prompt.read().unwrap().as_deref(),
            Some("abstention-from-disk")
        );
        assert_eq!(
            core.chat_persona.read().unwrap().as_deref(),
            Some("chat-persona-from-disk")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_reload_prompts_into_core_with_partial_overrides_writes_mixed_none_and_some() {
        let core = make_test_agent_core();
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-reload-prompts-partial-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();

        // Only seed 3 of the 6. The remaining 3 must come back as `None`.
        // ADR-068: total was 8 before removing 3 grafeo overrides.
        fs::write(dir.join("prompts").join("summary.md"), "summary-only").unwrap();
        fs::write(dir.join("prompts").join("title.md"), "title-only").unwrap();
        fs::write(dir.join("prompts").join("abstention.md"), "abstention-only").unwrap();

        reload_prompts_into_core(&dir, &core).expect("partial reload must succeed");

        assert_eq!(
            core.compaction_prompt.read().unwrap().as_deref(),
            Some("summary-only")
        );
        assert!(core.compact_template.read().unwrap().is_none());
        assert_eq!(
            core.title_prompt.read().unwrap().as_deref(),
            Some("title-only")
        );
        assert_eq!(
            core.abstention_prompt.read().unwrap().as_deref(),
            Some("abstention-only")
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_overridable_prompts_stays_at_5_entries() {
        // Sanity check: the closed-set invariant holds. A future
        // maintainer who adds an entry to `OVERRIDABLE_PROMPTS` without
        // teaching `reload_prompts_into_core` about the matching field
        // gets a compile-time reminder — the match arm is exhaustive
        // against the canonical filenames and any new entry triggers
        // an "unknown overridable filename" `Err` at runtime (the
        // match has no `_` arm). Pinning the canonical count here
        // makes a silent drift (e.g. someone shrinks the list to 4)
        // immediately visible.
        //
        // History: 8 -> 5 (ADR-068 dropped the three grafeo overrides),
        // 5 -> 7 (ADR-071 added distiller-extraction.md + distiller-judge.md),
        // 7 -> 6 (web-search removal), 6 -> 5 (projection + merge rewrite
        // collapsed the two distiller slots into distiller-merge.md),
        // 5 -> 6 (ADR-099 added the chat router persona),
        // 6 -> 5 (ADR-100 moved the distiller prompt into the memory service).
        assert_eq!(
            OVERRIDABLE_PROMPTS.len(),
            5,
            "OVERRIDABLE_PROMPTS must stay at 5 — every entry maps 1:1 to an AgentCore field; drift here breaks reload"
        );
    }

    /// Build a minimal `AgentCore` for the unit tests above. Only the
    /// 9 `Arc<RwLock<Option<String>>>` prompt fields are touched by
    /// `reload_prompts_into_core`, so a stripped-down provider /
    /// config is sufficient. Mirrors `agent_core::tests::make_core`
    /// (which is private to that module).
    fn make_test_agent_core() -> std::sync::Arc<crate::agent::agent_core::AgentCore> {
        use crate::config::RuntimeConfig;
        let config = RuntimeConfig::default();
        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.reload-prompts-unit"
            version = "1.0.0"
            name = "Test Reload Prompts"
            description = "Unit test target for reload_prompts_into_core"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .expect("manifest TOML must parse");
        let provider: std::sync::Arc<dyn acowork_core::providers::traits::Provider> =
            std::sync::Arc::new(acowork_core::providers::mock::MockProvider::single_text(
                "test",
            ));
        std::sync::Arc::new(crate::agent::agent_core::AgentCore::new(
            config,
            manifest,
            provider,
            vec![],
        ))
    }

    // ----- load_compaction_prompt -----

    /// Unique temp dir per test — the fixed-name helper below caused
    /// parallel test flakiness (two tests deleting each other's files).
    fn create_package_with_summary(name: &str, summary: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acowork-test-compaction-prompt-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test agent.",
        )
        .unwrap();
        fs::write(dir.join("prompts").join(COMPACTION_PROMPT_FILE), summary).unwrap();
        dir
    }

    #[test]
    fn test_load_compaction_prompt_missing_file_returns_none() {
        let dir = std::env::temp_dir().join("acowork-test-compaction-missing");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test agent.",
        )
        .unwrap();

        assert_eq!(load_compaction_prompt(&dir), None);
    }

    #[test]
    fn test_load_compaction_prompt_reads_file() {
        let dir = create_package_with_summary("reads-file", "Summarize with code details.\n");
        let loaded = load_compaction_prompt(&dir);
        assert_eq!(loaded.as_deref(), Some("Summarize with code details."));
    }

    #[test]
    fn test_load_compaction_prompt_whitespace_only_is_none() {
        // A file containing only blank lines behaves like an absent file —
        // the caller falls back to the built-in default prompt.
        let dir = create_package_with_summary("whitespace", "   \n\t\n  ");
        assert_eq!(load_compaction_prompt(&dir), None);
    }

    #[test]
    fn test_build_system_prompt_excludes_summary_md() {
        // summary.md must NOT leak into the main dialog system prompt.
        let dir = create_package_with_summary(
            "excludes",
            "You are a summarizer. Do NOT appear in the main system prompt.",
        );
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(!prompt.contains("summarizer"));
        assert!(!prompt.contains("## summary"));
    }

    #[test]
    fn test_build_system_prompt_still_includes_other_sections() {
        // Only summary.md is excluded; sibling prompt files keep working.
        let dir = create_package_with_summary("includes", "summarizer directive");
        fs::write(
            dir.join("prompts").join("constraints.md"),
            "Always be polite.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(prompt.contains("Always be polite."));
        assert!(!prompt.contains("summarizer directive"));
    }

    #[test]
    fn test_build_system_prompt_does_not_exclude_summary_case_variant() {
        // Symmetry with load_compaction_prompt: only the exact filename
        // `summary.md` has special semantics. A case variant like
        // `SUMMARY.md` is an ordinary prompt section — it must NOT be
        // silently dropped (on case-insensitive filesystems it would still
        // be read as the compaction prompt, so excluding it here would make
        // it invisible in BOTH places).
        let dir = std::env::temp_dir().join("acowork-test-compaction-case-variant");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test agent.",
        )
        .unwrap();
        fs::write(
            dir.join("prompts").join("SUMMARY.md"),
            "Case variant summary directive.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(prompt.contains("Case variant summary directive."));
        assert!(prompt.contains("## SUMMARY"));
    }

    #[test]
    fn test_build_system_prompt_does_not_exclude_summary_txt() {
        // Symmetry with load_compaction_prompt: only `summary.md` is
        // excluded. `summary.txt` (collected by collect_markdown_files but
        // never loaded as the compaction prompt) must surface as an
        // ordinary section rather than being silently dropped.
        let dir = std::env::temp_dir().join("acowork-test-compaction-txt-variant");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test agent.",
        )
        .unwrap();
        fs::write(
            dir.join("prompts").join("summary.txt"),
            "Txt summary directive.",
        )
        .unwrap();
        let prompt = build_system_prompt(&dir).unwrap();
        assert!(prompt.contains("test agent"));
        assert!(prompt.contains("Txt summary directive."));
        assert!(prompt.contains("## summary"));
    }

    // ── ADR-063: OVERRIDABLE_PROMPTS + is_overridable_prompt + load_optional_prompt ──

    /// Every canonical filename must be recognised. Pins the contract
    /// that `OVERRIDABLE_PROMPTS` and `is_overridable_prompt` agree.
    #[test]
    fn test_is_overridable_prompt_for_all_known_files() {
        for (filename, _desc) in OVERRIDABLE_PROMPTS {
            assert!(
                is_overridable_prompt(filename),
                "is_overridable_prompt must return true for {filename}"
            );
        }
    }

    /// Non-canonical filenames must NOT be recognised. This is the
    /// security guard for prompt reload — `reload_prompts_into_core`
    /// only reads files whose basename is listed here, so arbitrary
    /// files under `prompts/` never reach the live AgentCore.
    #[test]
    fn test_is_overridable_prompt_rejects_unknown_filenames() {
        assert!(!is_overridable_prompt("system.md"));
        assert!(!is_overridable_prompt("constraints.md"));
        assert!(!is_overridable_prompt("notes.md"));
        assert!(!is_overridable_prompt(""));
        assert!(!is_overridable_prompt("../summary.md"));
        assert!(!is_overridable_prompt("prompts/summary.md"));
        assert!(!is_overridable_prompt("summary.md.bak"));
    }

    /// Case variants are NOT overridable. Symmetric with the existing
    /// `test_build_system_prompt_does_not_exclude_summary_case_variant`
    /// test — case-sensitive matching is required so that on
    /// case-insensitive filesystems the two views (excluded from main
    /// prompt vs loaded as override) never disagree.
    #[test]
    fn test_is_overridable_prompt_is_case_sensitive() {
        assert!(!is_overridable_prompt("SUMMARY.md"));
        assert!(!is_overridable_prompt("Summary.md"));
        assert!(!is_overridable_prompt("Title.md"));
        assert!(!is_overridable_prompt("ABSTENTION.md"));
    }

    /// `load_optional_prompt` reads any of the 9 canonical files. Sanity
    /// check on top of `load_compaction_prompt`'s dedicated test.
    #[test]
    fn test_load_optional_prompt_reads_each_canonical_file() {
        let dir = std::env::temp_dir().join("acowork-test-load-optional-each");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();

        for (filename, _desc) in OVERRIDABLE_PROMPTS {
            let content = format!("Directive content for {filename}");
            fs::write(dir.join("prompts").join(filename), &content).unwrap();
        }

        for (filename, _desc) in OVERRIDABLE_PROMPTS {
            let loaded = load_optional_prompt(&dir, filename);
            let expected = format!("Directive content for {filename}");
            assert_eq!(
                loaded.as_deref(),
                Some(expected.as_str()),
                "load_optional_prompt must read {filename}"
            );
        }
    }

    /// Unknown filenames must return None. This is the silent path —
    /// callers are expected to fall back to built-in constants; we don't
    /// spam the log for routine "no override" cases but DO log a warning
    /// for malformed calls (covered by the load_optional_prompt warn
    /// path).
    #[test]
    fn test_load_optional_prompt_unknown_filename_returns_none() {
        let dir = std::env::temp_dir().join("acowork-test-load-optional-unknown");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        // Even with the file present, an unknown filename must return None.
        fs::write(dir.join("prompts").join("evil.md"), "should not load").unwrap();
        assert_eq!(load_optional_prompt(&dir, "evil.md"), None);
        assert_eq!(load_optional_prompt(&dir, ""), None);
    }

    /// Whitespace-only files behave like absent files (same contract as
    /// ADR-053's `load_compaction_prompt`).
    #[test]
    fn test_load_optional_prompt_whitespace_only_is_none() {
        let dir = std::env::temp_dir().join("acowork-test-load-optional-whitespace");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(dir.join("prompts").join("title.md"), "   \n\t\n  ").unwrap();
        assert_eq!(load_optional_prompt(&dir, "title.md"), None);
    }

    /// Backward-compat: `load_compaction_prompt` is still a valid public
    /// symbol and must continue to work. It's now a thin wrapper over
    /// `load_optional_prompt`.
    #[test]
    fn test_load_compaction_prompt_delegates_to_load_optional() {
        let dir = std::env::temp_dir().join("acowork-test-compaction-delegates");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();
        fs::write(
            dir.join("prompts").join("summary.md"),
            "Compaction directive content.",
        )
        .unwrap();

        // Both code paths must agree.
        let via_wrapper = load_compaction_prompt(&dir);
        let via_generic = load_optional_prompt(&dir, "summary.md");
        assert_eq!(via_wrapper, via_generic);
        assert_eq!(
            via_wrapper.as_deref(),
            Some("Compaction directive content.")
        );
    }

    /// Multi-file exclusion: ALL 9 canonical filenames are excluded from
    /// the main dialog system prompt. This pins §3.3 of ADR-063.
    #[test]
    fn test_build_system_prompt_excludes_all_overridable_files() {
        let dir = std::env::temp_dir().join("acowork-test-excludes-all-overridable");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("prompts")).unwrap();

        fs::write(
            dir.join("prompts").join("system.md"),
            "You are a test agent.",
        )
        .unwrap();
        for (filename, _desc) in OVERRIDABLE_PROMPTS {
            let sentinel = format!("SENTINEL FOR {filename}");
            fs::write(dir.join("prompts").join(filename), &sentinel).unwrap();
        }

        let prompt = build_system_prompt(&dir).unwrap();
        // system.md is an ordinary section and MUST appear.
        assert!(prompt.contains("test agent"));
        // Every overridable file's sentinel MUST be excluded — none of
        // them belong in the main dialog system prompt.
        for (filename, _desc) in OVERRIDABLE_PROMPTS {
            assert!(
                !prompt.contains(&format!("SENTINEL FOR {filename}")),
                "main prompt must exclude {filename} (ADR-063 §3.3)"
            );
            // And neither the filename as a section header (## summary, etc).
            let header = format!("## {}", filename.trim_end_matches(".md"));
            assert!(
                !prompt.contains(&header),
                "main prompt must not render ## header for excluded {filename}"
            );
        }
    }

    /// Non-canonical neighbour of an overridable filename is NOT excluded —
    /// it becomes an ordinary section. Symmetric with the existing
    /// `test_build_system_prompt_does_not_exclude_summary_case_variant` but
    /// generalised to all 9 entries.
    ///
    /// Implementation note: `collect_markdown_files` filters by lowercase
    /// extension (`.md`/`.txt`) so a filename with an uppercase extension
    /// like `SUMMARY.MD` would not be collected at all — the test wouldn't
    /// be able to make any statement about exclusion. We use a
    /// `<stem>-case.md` neighbour instead: it passes the lowercase-extension
    /// collector, is NOT in `OVERRIDABLE_PROMPTS`, and must therefore reach
    /// the main dialog system prompt as an ordinary section.
    #[test]
    fn test_build_system_prompt_does_not_exclude_overridable_case_variant() {
        for (filename, _desc) in OVERRIDABLE_PROMPTS.iter().take(2) {
            let dir = std::env::temp_dir().join(format!("acowork-test-excludes-case-{filename}"));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("prompts")).unwrap();
            fs::write(
                dir.join("prompts").join("system.md"),
                "You are a test agent.",
            )
            .unwrap();

            // Neighbour that LOOKS like an overridable filename but isn't:
            // same stem + `-case` suffix. Must pass the lowercase-extension
            // collector and the exact-match exclusion (because the
            // canonical name is `summary.md` / `title.md`, not
            // `summary-case.md` / `title-case.md`).
            let stem = filename.trim_end_matches(".md");
            let variant = format!("{stem}-case.md");
            fs::write(
                dir.join("prompts").join(&variant),
                format!("Case variant for {filename}."),
            )
            .unwrap();

            let prompt = build_system_prompt(&dir).unwrap();
            assert!(
                prompt.contains(&format!("Case variant for {filename}.")),
                "neighbour {variant} must surface as ordinary section (not in OVERRIDABLE_PROMPTS exact match)"
            );
        }
    }
}
