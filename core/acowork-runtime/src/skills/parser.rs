//! SKILL.md parser — parses YAML frontmatter + Markdown body
//!
//! SKILL.md is the static definition layer of the Skill dual-layer model.
//! Format: YAML frontmatter (between `---` delimiters) + Markdown body.
//! Compatible with agentskills.io open standard.
//!
//! Reference: docs/13-skill-system.md §2

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parsed SKILL.md — complete skill definition
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDefinition {
    /// Skill name (unique within the Agent)
    pub name: String,
    /// Human-readable description
    pub description: String,
    /// Skill version (semantic versioning)
    #[serde(default)]
    pub version: Option<String>,
    /// Author: "agent" (Agent self-created) or "developer" (hand-written)
    #[serde(default)]
    pub author: Option<String>,
    /// Source draft ID (traceability for Agent-created skills)
    #[serde(default)]
    pub source_draft: Option<String>,
    /// Trigger words for skill matching
    pub triggers: Vec<String>,
    /// Required tools for this skill
    #[serde(default)]
    pub tool_deps: Vec<String>,
    /// Platform compatibility declarations
    #[serde(default)]
    pub platforms: Option<PlatformCompat>,
    /// Model compatibility snapshot (tested at publish time)
    #[serde(default)]
    pub tested_models: Vec<TestedModel>,
    /// Markdown body — the skill instructions
    pub instructions: String,
    /// Source file path (if loaded from disk)
    #[serde(skip)]
    pub source_path: Option<PathBuf>,
}

/// Platform compatibility declaration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlatformCompat {
    /// Desktop platform support: "required", "optional", or absent (all platforms)
    #[serde(default)]
    pub desktop: Option<String>,
    /// Mobile platform support
    #[serde(default)]
    pub mobile: Option<String>,
}

/// Model compatibility entry (snapshot from testing)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestedModel {
    /// LLM provider name
    pub provider: String,
    /// Model identifier
    pub model: String,
    /// Compatibility rating: excellent / good / limited
    #[serde(default)]
    pub rating: Option<String>,
    /// Optional note about compatibility
    #[serde(default)]
    pub note: Option<String>,
}

/// YAML frontmatter structure (deserialized from SKILL.md header)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    source_draft: Option<String>,
    #[serde(default)]
    triggers: Vec<String>,
    #[serde(default)]
    tool_deps: Vec<String>,
    #[serde(default)]
    platforms: Option<PlatformCompat>,
    #[serde(default)]
    tested_models: Vec<TestedModel>,
}

/// Skill parsing errors
#[derive(Debug, thiserror::Error)]
pub enum SkillParseError {
    #[error("Missing YAML frontmatter delimiters (---) in SKILL.md")]
    MissingFrontmatter,

    #[error("YAML frontmatter parse error: {0}")]
    YamlParse(String),

    #[error("Missing required field '{0}' in SKILL.md frontmatter")]
    MissingField(String),

    #[error("Empty instructions body in SKILL.md")]
    EmptyInstructions,

    #[error("IO error reading SKILL.md: {0}")]
    Io(#[from] std::io::Error),
}

/// Parse a SKILL.md content string into a SkillDefinition
///
/// Expects YAML frontmatter between `---` delimiters, followed by Markdown body.
///
/// # Example
///
/// ```text
/// ---
/// name: weekly-report
/// description: Generate weekly report
/// triggers:
///   - weekly report
/// tool_deps:
///   - memory_recall
/// ---
///
/// # Weekly Report Skill
///
/// 1. Recall this week's work...
/// ```
pub fn parse_skill_md(content: &str) -> Result<SkillDefinition, SkillParseError> {
    // Split by --- delimiters to extract frontmatter and body
    let trimmed = content.trim_start();

    // Must start with ---
    if !trimmed.starts_with("---") {
        return Err(SkillParseError::MissingFrontmatter);
    }

    // Find the closing ---
    let rest = trimmed.strip_prefix("---").unwrap_or(trimmed);
    let end_delimiter = rest.find("\n---");

    let (frontmatter_str, instructions) = match end_delimiter {
        Some(pos) => {
            let fm = &rest[..pos];
            let body = &rest[pos + 4..]; // skip \n---
            (fm, body.trim().to_string())
        }
        None => return Err(SkillParseError::MissingFrontmatter),
    };

    // Parse YAML frontmatter
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(frontmatter_str)
        .map_err(|e| SkillParseError::YamlParse(e.to_string()))?;

    // Validate required fields.
    // `triggers` is intentionally NOT required: a skill with no triggers is
    // still valid — it just won't be auto-matched by `find_by_trigger`, and
    // can only be invoked manually via the chat-box selector (which the
    // runtime resolves by `name`). We log a warning so authors know the
    // skill won't surface via natural-language trigger matching.
    if frontmatter.name.is_empty() {
        return Err(SkillParseError::MissingField("name".to_string()));
    }
    if frontmatter.description.is_empty() {
        return Err(SkillParseError::MissingField("description".to_string()));
    }
    if frontmatter.triggers.is_empty() {
        tracing::warn!(
            skill = %frontmatter.name,
            "SKILL.md has no triggers — skill will load but won't be auto-matched; \
             it can only be invoked manually via the chat-box selector"
        );
    }
    if instructions.is_empty() {
        return Err(SkillParseError::EmptyInstructions);
    }

    Ok(SkillDefinition {
        name: frontmatter.name,
        description: frontmatter.description,
        version: frontmatter.version,
        author: frontmatter.author,
        source_draft: frontmatter.source_draft,
        triggers: frontmatter.triggers,
        tool_deps: frontmatter.tool_deps,
        platforms: frontmatter.platforms,
        tested_models: frontmatter.tested_models,
        instructions,
        source_path: None,
    })
}

/// Load a SKILL.md file from disk
pub fn load_skill_md(path: &Path) -> Result<SkillDefinition, SkillParseError> {
    let content = std::fs::read_to_string(path)?;
    let mut skill = parse_skill_md(&content)?;
    skill.source_path = Some(path.to_path_buf());
    Ok(skill)
}

/// Skill registry — manages loaded skills for an Agent
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillRegistry {
    /// Map of skill name → SkillDefinition
    skills: HashMap<String, SkillDefinition>,
}

impl SkillRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Load all skills from a `skills/` directory
    ///
    /// Expected structure:
    /// ```text
    /// skills/
    ///   ├── weekly-report/
    ///   │   └── SKILL.md
    ///   └── deploy/
    ///       └── SKILL.md
    /// ```
    pub fn load_from_dir(skills_dir: &Path) -> Result<Self, SkillParseError> {
        let mut registry = Self::new();

        if !skills_dir.exists() {
            tracing::debug!("Skills directory does not exist: {:?}", skills_dir);
            return Ok(registry);
        }

        let entries = std::fs::read_dir(skills_dir).map_err(SkillParseError::Io)?;

        for entry in entries {
            let entry = entry.map_err(SkillParseError::Io)?;
            let path = entry.path();

            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    match load_skill_md(&skill_md) {
                        Ok(skill) => {
                            tracing::info!("Loaded skill '{}' from {:?}", skill.name, skill_md);
                            registry.skills.insert(skill.name.clone(), skill);
                        }
                        Err(e) => {
                            tracing::warn!("Failed to parse skill from {:?}: {}", skill_md, e);
                            // Continue loading other skills rather than failing entirely
                        }
                    }
                }
            }
        }

        Ok(registry)
    }

    /// Register a skill definition
    pub fn register(&mut self, skill: SkillDefinition) {
        self.skills.insert(skill.name.clone(), skill);
    }

    /// Get a skill by name
    pub fn get(&self, name: &str) -> Option<&SkillDefinition> {
        self.skills.get(name)
    }

    /// Find skills matching a trigger word
    pub fn find_by_trigger(&self, trigger: &str) -> Vec<&SkillDefinition> {
        let trigger_lower = trigger.to_lowercase();
        self.skills
            .values()
            .filter(|skill| {
                skill
                    .triggers
                    .iter()
                    .any(|t| t.to_lowercase() == trigger_lower)
            })
            .collect()
    }

    /// Find skills that depend on a specific tool
    pub fn find_by_tool_dep(&self, tool_name: &str) -> Vec<&SkillDefinition> {
        self.skills
            .values()
            .filter(|skill| skill.tool_deps.iter().any(|t| t == tool_name))
            .collect()
    }

    /// Get all registered skills
    pub fn all_skills(&self) -> Vec<&SkillDefinition> {
        self.skills.values().collect()
    }

    /// Get the number of registered skills
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Check if the registry is empty
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Check if a skill's tool dependencies are satisfied
    ///
    /// Returns a list of missing tool names
    pub fn check_tool_deps(&self, skill_name: &str, available_tools: &[&str]) -> Vec<String> {
        match self.skills.get(skill_name) {
            Some(skill) => skill
                .tool_deps
                .iter()
                .filter(|dep| !available_tools.contains(&dep.as_str()))
                .cloned()
                .collect(),
            None => Vec::new(),
        }
    }

    /// Build skill instructions for System Prompt injection
    ///
    /// Generates a formatted string combining all loaded skill instructions,
    /// ready to be appended to the Agent's system prompt.
    pub fn build_skill_instructions(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut output = String::from("## Skills\n\n");
        for skill in self.skills.values() {
            output.push_str(&format!("### {}\n\n", skill.name));
            output.push_str(&skill.instructions);
            output.push_str("\n\n");
        }
        output
    }

    /// Build the system-prompt block for a single skill by name — the
    /// per-turn command-injection path. The frontend sends only the skill
    /// NAME in the chat `command` field; the runtime owns the instructions
    /// (this registry is the authoritative source). Returns `None` when
    /// `name` is not registered (unknown command — the skill is ignored).
    /// Block shape matches the per-skill block of [`Self::build_skill_instructions`].
    pub fn instructions_for(&self, name: &str) -> Option<String> {
        let skill = self.get(name)?;
        Some(format!("### {}\n\n{}", skill.name, skill.instructions))
    }

    /// Build a compact skill summary for system prompt injection
    ///
    /// Generates a formatted string containing only skill names and descriptions,
    /// used in Progressive mode to keep the system prompt compact while still
    /// making the LLM aware of available skills.
    pub fn build_skill_summary(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut output = String::from("## Available Skills\n\n");
        // Deterministic order (name-sorted). `self.skills` is a
        // `HashMap`, so iterating `values()` directly yields a
        // per-instance random order: the same skill set would render
        // differently across boots and across every
        // `UpdateSkillInstructions` hot-reload. This summary is injected
        // into Block A of the system prompt (ADR-060), so an unstable
        // order silently rewrites the prompt bytes and misses the prompt
        // cache. Same fix/comment as `http::skills::list_from_registry`.
        let mut skills: Vec<&SkillDefinition> = self.skills.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        for skill in skills {
            output.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_skill_md() -> &'static str {
        r#"---
name: weekly-report
description: 汇总本周工作内容生成结构化周报
version: "1.0.0"
author: developer
triggers:
  - 周报
  - weekly report
  - 总结本周
tool_deps:
  - memory_recall
  - file_write
tested_models:
  - provider: openai
    model: gpt-4o
    rating: excellent
  - provider: ollama
    model: qwen3:8b
    rating: good
    note: "需要扁平化指令适配"
---

# Weekly Report Skill

## 执行步骤

1. 使用 `memory_recall` 检索本周的对话和工作记录
2. 按项目分类整理完成事项
3. 生成结构化周报
4. 使用 `file_write` 保存到用户指定路径
"#
    }

    #[test]
    fn test_parse_skill_md_basic() {
        let skill = parse_skill_md(sample_skill_md()).unwrap();
        assert_eq!(skill.name, "weekly-report");
        assert_eq!(skill.description, "汇总本周工作内容生成结构化周报");
        assert_eq!(skill.version, Some("1.0.0".to_string()));
        assert_eq!(skill.author, Some("developer".to_string()));
        assert!(skill.source_draft.is_none());
    }

    #[test]
    fn test_parse_skill_md_triggers() {
        let skill = parse_skill_md(sample_skill_md()).unwrap();
        assert_eq!(skill.triggers.len(), 3);
        assert!(skill.triggers.contains(&"周报".to_string()));
        assert!(skill.triggers.contains(&"weekly report".to_string()));
        assert!(skill.triggers.contains(&"总结本周".to_string()));
    }

    #[test]
    fn test_parse_skill_md_tool_deps() {
        let skill = parse_skill_md(sample_skill_md()).unwrap();
        assert_eq!(skill.tool_deps.len(), 2);
        assert!(skill.tool_deps.contains(&"memory_recall".to_string()));
        assert!(skill.tool_deps.contains(&"file_write".to_string()));
    }

    #[test]
    fn test_parse_skill_md_tested_models() {
        let skill = parse_skill_md(sample_skill_md()).unwrap();
        assert_eq!(skill.tested_models.len(), 2);
        assert_eq!(skill.tested_models[0].provider, "openai");
        assert_eq!(skill.tested_models[0].model, "gpt-4o");
        assert_eq!(skill.tested_models[0].rating, Some("excellent".to_string()));
        assert_eq!(skill.tested_models[1].provider, "ollama");
        assert_eq!(
            skill.tested_models[1].note,
            Some("需要扁平化指令适配".to_string())
        );
    }

    #[test]
    fn test_parse_skill_md_instructions() {
        let skill = parse_skill_md(sample_skill_md()).unwrap();
        assert!(skill.instructions.contains("执行步骤"));
        assert!(skill.instructions.contains("memory_recall"));
        assert!(skill.instructions.contains("file_write"));
    }

    #[test]
    fn test_parse_skill_md_minimal() {
        let content = r#"---
name: hello
description: Say hello
triggers:
  - hi
---

Hello!
"#;
        let skill = parse_skill_md(content).unwrap();
        assert_eq!(skill.name, "hello");
        assert!(skill.tool_deps.is_empty());
        assert!(skill.tested_models.is_empty());
        assert!(skill.version.is_none());
        assert_eq!(skill.instructions, "Hello!");
    }

    #[test]
    fn test_parse_skill_md_missing_frontmatter() {
        let content = "No frontmatter here";
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillParseError::MissingFrontmatter)));
    }

    #[test]
    fn test_parse_skill_md_missing_name() {
        let content = r#"---
description: No name
triggers:
  - test
---

Body
"#;
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillParseError::YamlParse(_))));
    }

    #[test]
    fn test_parse_skill_md_empty_triggers_loads_with_warning() {
        // A skill with no triggers is still valid: it loads into the
        // registry (so manual command injection works) but won't be
        // auto-matched by `find_by_trigger`. This is the common shape for
        // "manual-only" skills like /ponytail-review.
        let content = r#"---
name: ponytail-review
description: Code review focused on over-engineering
---

Body
"#;
        let skill = parse_skill_md(content).expect("empty triggers should not reject the skill");
        assert_eq!(skill.name, "ponytail-review");
        assert!(skill.triggers.is_empty());
        assert!(skill.instructions.contains("Body"));
    }

    #[test]
    fn test_parse_skill_md_empty_instructions() {
        let content = r#"---
name: test
description: Test skill
triggers:
  - test
---
"#;
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillParseError::EmptyInstructions)));
    }

    #[test]
    fn test_parse_skill_md_invalid_yaml() {
        let content = r#"---
name: [invalid yaml
description: Test
triggers:
  - test
---

Body
"#;
        let result = parse_skill_md(content);
        assert!(matches!(result, Err(SkillParseError::YamlParse(_))));
    }

    #[test]
    fn test_parse_skill_md_with_platforms() {
        let content = r#"---
name: deploy
description: Deploy service
triggers:
  - deploy
platforms:
  desktop: required
  mobile: optional
---

Deploy steps...
"#;
        let skill = parse_skill_md(content).unwrap();
        assert!(skill.platforms.is_some());
        let platforms = skill.platforms.unwrap();
        assert_eq!(platforms.desktop, Some("required".to_string()));
        assert_eq!(platforms.mobile, Some("optional".to_string()));
    }

    #[test]
    fn test_parse_skill_md_with_source_draft() {
        let content = r#"---
name: auto-skill
description: Agent-created skill
author: agent
source_draft: draft-abc123
triggers:
  - auto
---

Auto skill instructions.
"#;
        let skill = parse_skill_md(content).unwrap();
        assert_eq!(skill.author, Some("agent".to_string()));
        assert_eq!(skill.source_draft, Some("draft-abc123".to_string()));
    }

    // ── SkillRegistry tests ──────────────────────────────────────────────

    #[test]
    fn test_skill_registry_new() {
        let registry = SkillRegistry::new();
        assert!(registry.is_empty());
    }

    #[test]
    fn test_skill_registry_register_and_get() {
        let mut registry = SkillRegistry::new();
        let skill = parse_skill_md(sample_skill_md()).unwrap();
        registry.register(skill);

        assert_eq!(registry.len(), 1);
        assert!(registry.get("weekly-report").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_skill_registry_find_by_trigger() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        let results = registry.find_by_trigger("周报");
        assert_eq!(results.len(), 1);

        let no_results = registry.find_by_trigger("nonexistent");
        assert!(no_results.is_empty());
    }

    #[test]
    fn test_skill_registry_find_by_trigger_case_insensitive() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        let results = registry.find_by_trigger("Weekly Report");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn test_skill_registry_find_by_tool_dep() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        let results = registry.find_by_tool_dep("memory_recall");
        assert_eq!(results.len(), 1);

        let no_results = registry.find_by_tool_dep("shell");
        assert!(no_results.is_empty());
    }

    #[test]
    fn test_skill_registry_check_tool_deps() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        // All deps available
        let missing = registry.check_tool_deps("weekly-report", &["memory_recall", "file_write"]);
        assert!(missing.is_empty());

        // Missing one dep
        let missing = registry.check_tool_deps("weekly-report", &["memory_recall"]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0], "file_write");
    }

    #[test]
    fn test_skill_registry_build_skill_instructions() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        let instructions = registry.build_skill_instructions();
        assert!(instructions.contains("## Skills"));
        assert!(instructions.contains("### weekly-report"));
        assert!(instructions.contains("执行步骤"));
    }

    #[test]
    fn test_skill_registry_instructions_for_single_skill() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        // Known skill → formatted instruction block (name + body).
        let instructions = registry.instructions_for("weekly-report").unwrap();
        assert!(instructions.contains("### weekly-report"));
        assert!(instructions.contains("执行步骤"));

        // Unknown skill / empty name → None (caller falls back to no
        // skill injection instead of trusting a client-supplied name).
        assert!(registry.instructions_for("no-such-skill").is_none());
        assert!(registry.instructions_for("").is_none());
    }

    #[test]
    fn test_skill_registry_empty_instructions() {
        let registry = SkillRegistry::new();
        assert!(registry.build_skill_instructions().is_empty());
    }

    #[test]
    fn test_skill_registry_build_skill_summary() {
        let mut registry = SkillRegistry::new();
        registry.register(parse_skill_md(sample_skill_md()).unwrap());

        let summary = registry.build_skill_summary();
        assert!(summary.contains("## Available Skills"));
        assert!(summary.contains("weekly-report"));
        assert!(summary.contains("汇总本周工作内容生成结构化周报"));
        // Instructions should NOT appear in summary
        assert!(!summary.contains("执行步骤"));
    }

    #[test]
    fn test_skill_registry_empty_summary() {
        let registry = SkillRegistry::new();
        assert!(registry.build_skill_summary().is_empty());
    }

    /// ADR-094: the summary is injected into Block A of the system prompt,
    /// so its byte layout must be stable across boots and across every
    /// `UpdateSkillInstructions` hot-reload. `self.skills` is a
    /// `HashMap`, so without an explicit sort two runs over the same
    /// skill set render — and hash — differently, which silently
    /// invalidates the ADR-060 prompt cache. Pin the exact name-sorted
    /// render (5 skills → 120 orderings, exactly one passes).
    #[test]
    fn test_skill_registry_summary_is_name_sorted() {
        let mut registry = SkillRegistry::new();
        // Register deliberately out of alphabetical order.
        for name in ["delta", "alpha", "charlie", "bravo", "echo"] {
            let md =
                format!("---\nname: {name}\ndescription: {name} skill\n---\n\nBody of {name}.\n");
            registry.register(parse_skill_md(&md).unwrap());
        }

        assert_eq!(
            registry.build_skill_summary(),
            "## Available Skills\n\n\
             - **alpha**: alpha skill\n\
             - **bravo**: bravo skill\n\
             - **charlie**: charlie skill\n\
             - **delta**: delta skill\n\
             - **echo**: echo skill\n"
        );
    }
}
