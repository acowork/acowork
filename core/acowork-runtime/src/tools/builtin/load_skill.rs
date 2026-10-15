//! `load_skill` builtin tool — load one skill's full instructions on demand.
//!
//! ADR-094. In the default skill mode (`Manual`), the agent's SKILL.md
//! content is NOT prepended to the system prompt — the LLM only learns
//! about available skills via the `build_skill_summary()` block that
//! `Progressive` mode injects, OR (in `Manual` mode) via the user
//! pasting the skill content into the chat.
//!
//! This tool gives the LLM an explicit "go get the full instructions"
//! action: it takes a skill `name` and returns that skill's full
//! Markdown body (`SKILL.md` frontmatter `instructions:` field).
//!
//! ## Implementation reuse
//!
//! The tool shares the parser with the public `GET /agents/{id}/skills/{name}`
//! HTTP endpoint (see [`crate::http::skills`]). Both call
//! `SkillRegistry::load_from_dir()` on every invocation so a newly
//! imported or hand-edited SKILL.md shows up without a Runtime restart.
//! The single parser is the ADR-009 §V-A invariant: only the Runtime
//! reads `{package_dir}/skills/`, no Gateway-side second copy.
//!
//! ## No HTTP hop
//!
//! Although the user-visible `GET /agents/{id}/skills/{name}` is the
//! canonical external surface, the LLM-invoked `load_skill` tool
//! calls `SkillRegistry::load_from_dir()` directly inside the Runtime
//! process. The two paths share the same parser (single source of
//! truth) but skip the HTTP overhead — no need to serialize into
//! `SkillDetailResponse` and deserialize out again for an in-process
//! call.
//!
//! ## Permission
//!
//! Read-only. No permission table entry required beyond what the
//! builtin registry already enforces.

use std::path::PathBuf;
use std::sync::Arc;

use acowork_core::tools::traits::{Tool, ToolResult, ToolSpec};
use async_trait::async_trait;
use serde_json::Value;

use crate::skills::parser::SkillRegistry;

/// `load_skill` builtin tool.
///
/// Loads the full Markdown body of a single skill by name, on LLM
/// demand. See module-level docs for the ADR-094 rationale.
pub struct LoadSkillTool {
    /// Agent's `.agent` package directory (where `skills/<name>/SKILL.md`
    /// lives). Distinct from the session `work_dir` — package files are
    /// read-only, runtime state is not.
    package_dir: PathBuf,
}

impl LoadSkillTool {
    /// Construct a `LoadSkillTool` for a given agent package.
    ///
    /// `package_dir` is the directory the manifest was loaded from; the
    /// tool reads `{package_dir}/skills/` lazily on every invocation so
    /// freshly-edited SKILL.md files are picked up immediately (the
    /// pre-existing `http::skills::load_registry()` contract).
    pub fn new(package_dir: PathBuf) -> Self {
        Self { package_dir }
    }

    pub fn spec_value() -> ToolSpec {
        ToolSpec {
            name: "load_skill".to_string(),
            description: "Load the full instructions for a named skill. \
                Use this AFTER the system prompt summary (in Progressive skill mode) \
                has identified a relevant skill: the summary only carries the name + \
                one-paragraph description, NOT the full step-by-step instructions. \
                Call this tool to pull the complete SKILL.md body before executing \
                the skill's workflow. Returns a 'not found' error if the skill name \
                does not match any registered SKILL.md."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Skill name (the SKILL.md frontmatter `name` field). \
                            Must match exactly. The names of available skills are listed \
                            in the system prompt when Progressive mode is on; in Manual \
                            mode, ask the user to confirm the exact name."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    /// Build a `SkillRegistry` from disk. Mirrors the existing helper
    /// in `http::skills::load_registry` (single parser — see ADR-009 §V-A);
    /// we re-implement the two lines inline here because (a) the HTTP
    /// helper takes a `&HttpState` which is not relevant to a builtin
    /// tool, and (b) the shared behaviour is small enough that an
    /// additional indirection would obscure more than it deduplicates.
    fn load_registry(&self) -> SkillRegistry {
        let skills_dir = self.package_dir.join("skills");
        SkillRegistry::load_from_dir(&skills_dir).unwrap_or_else(|e| {
            tracing::warn!(
                dir = %skills_dir.display(),
                error = %e,
                "load_skill: failed to load skill registry"
            );
            SkillRegistry::new()
        })
    }
}

#[async_trait]
impl Tool for LoadSkillTool {
    fn spec(&self) -> ToolSpec {
        Self::spec_value()
    }

    async fn execute(
        &self,
        params: Value,
        _work_dir: Option<&str>,
    ) -> acowork_core::error::Result<ToolResult> {
        // skills are in the package dir, not the workspace — _work_dir
        // is intentionally ignored (the file_read/mcp_install split is
        // workspace-vs-agent-home; skills are package-level).
        let name = match params.get("name").and_then(|v| v.as_str()) {
            Some(s) if !s.trim().is_empty() => s.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some("Missing required parameter 'name'".to_string()),
                    token_usage: None,
                });
            }
        };

        let registry = self.load_registry();
        let skill = match registry.get(&name) {
            Some(s) => s,
            None => {
                return Ok(ToolResult {
                    ok: false,
                    content: String::new(),
                    error: Some(format!(
                        "Skill not found: '{name}'. \
                         Use the skill name from the system prompt's 'Available Skills' summary \
                         (in Progressive mode), or ask the user to confirm."
                    )),
                    token_usage: None,
                });
            }
        };

        // Return both metadata + instructions in one shot so the LLM
        // has full context (version, triggers, tool_deps) without a
        // second round-trip.
        let body = serde_json::json!({
            "name": skill.name,
            "description": skill.description,
            "version": skill.version,
            "author": skill.author,
            "triggers": skill.triggers,
            "tool_deps": skill.tool_deps,
            "instructions": skill.instructions,
        });

        Ok(ToolResult {
            ok: true,
            content: serde_json::to_string_pretty(&body).unwrap_or_else(|_| {
                // Fall back to a minimal hand-rolled payload if serde
                // fails for any reason (shouldn't happen with
                // serde_json::Value, but be defensive — a panic here
                // would break the agent loop).
                format!(
                    "# Skill: {}\n\n{}",
                    skill.name, skill.instructions
                )
            }),
            error: None,
            token_usage: None,
        })
    }
}

// Arc is needed by `all_builtin_tools` which stores every tool as
// `Arc<dyn Tool>`. We provide a thin constructor helper for the
// caller so the `Arc` boundary is at the registration site, not
// inside the tool itself.
impl LoadSkillTool {
    /// Register the tool behind an `Arc` for insertion into the
    /// `ToolRegistry`. Thin alias to keep the call site readable.
    pub fn into_arc(self) -> Arc<dyn Tool> {
        Arc::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_skill(dir: &std::path::Path, name: &str, instructions: &str) {
        let skill_dir = dir.join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!(
                "---\nname: {name}\ndescription: {name} skill\n---\n\n{instructions}\n"
            ),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn load_skill_returns_full_instructions() {
        let tmp = std::env::temp_dir().join(format!("acowork-load-skill-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        write_skill(&tmp, "greet", "Say hello warmly. Use 2-3 sentences.");

        let tool = LoadSkillTool::new(tmp.clone());
        let result = tool
            .execute(
                serde_json::json!({ "name": "greet" }),
                None,
            )
            .await
            .expect("execute ok");

        assert!(result.ok, "tool should succeed: error={:?}", result.error);
        assert!(
            result.content.contains("greet") && result.content.contains("Say hello warmly"),
            "content should include skill metadata + body, got: {}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn load_skill_missing_name_param() {
        let tool = LoadSkillTool::new(PathBuf::from("/nonexistent"));
        let result = tool
            .execute(serde_json::json!({}), None)
            .await
            .expect("execute ok");
        assert!(!result.ok);
        assert!(result.error.unwrap_or_default().contains("name"));
    }

    #[tokio::test]
    async fn load_skill_unknown_name() {
        let tmp = std::env::temp_dir().join(format!("acowork-load-skill-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        // No skills/ subdir — registry is empty.

        let tool = LoadSkillTool::new(tmp.clone());
        let result = tool
            .execute(
                serde_json::json!({ "name": "ghost" }),
                None,
            )
            .await
            .expect("execute ok");
        assert!(!result.ok);
        assert!(result.error.unwrap_or_default().contains("ghost"));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
