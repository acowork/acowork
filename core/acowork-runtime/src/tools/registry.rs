//! Tool registry — tool pool registration + activation
//!
//! Two-step process:
//! 1. `all_builtin_tools()` builds the complete tool pool
//! 2. `activate()` applies security decorators, filtering by per-agent
//!    `enabled` flags from `agent_tools.json` (ADR-029).
use crate::agent::agent_core::BuiltinToolEntry;
use crate::tools::workspace_resolver::SharedResolver;
use acowork_core::AgentManifest;
use acowork_core::tools::traits::Tool;
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(test)]
use crate::tools::workspace_resolver::WorkspaceResolver;

/// Tool registry
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl BuiltinToolEntry {
    /// Decide the canonical `enabled` flag for a builtin tool.
    ///
    /// The persisted user preference is honored verbatim — there is no
    /// second override at this layer. Per-agent enable flags live in
    /// `agent_tools.json` and are loaded by the persistence layer.
    ///
    /// See ADR-029 (per-agent builtin tools).
    pub(crate) fn with_resolved_enabled(user_persisted_enabled: bool, tool: Arc<dyn Tool>) -> Self {
        Self {
            enabled: user_persisted_enabled,
            tool,
        }
    }
}

impl ToolRegistry {
    /// Create new empty registry
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    /// Register a tool
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Get tool by name
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// Get all registered tools (regardless of `enabled` flag)
    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    /// List all registered tool names (unconditional, regardless of enabled)
    pub fn tool_names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Activate tools with security decorators, filtering by per-agent
    /// `enabled` flags. The return value is `Vec<BuiltinToolEntry>`
    /// — both for direct LLM dispatch (filter `enabled` again to get
    /// `Vec<Arc<dyn Tool>>`) and for storing in
    /// `AgentCore.builtin_tools`.
    ///
    /// Tools NOT in the registry but listed in `enabled_entries` are
    /// silently skipped (defensive — see ADR-029 §"Initialization").
    ///
    /// Steps:
    /// 1. Build a name -> enabled map from `enabled_entries` (which
    ///    already excludes platform-protected tools — see
    ///    [`crate::agent_config::merge_tools_config`]).
    /// 2. Decide `enabled` via
    ///    [`BuiltinToolEntry::with_resolved_enabled`]; the caller's
    ///    persisted preference wins verbatim (no platform-protection
    ///    override at this layer — it never needs to fire because the
    ///    filter upstream already removed those names).
    /// 3. Wrap both enabled and disabled tools with security decorators
    ///    (path guard + rate limiter) so disabled tools remain
    ///    introspectable for re-enable at runtime.
    pub(crate) fn activate(
        &self,
        _manifest: &AgentManifest,
        resolver: &SharedResolver,
        max_calls_per_minute: u32,
        enabled_entries: &[crate::agent_config::AgentToolEntry],
    ) -> Vec<BuiltinToolEntry> {
        // Build a name -> enabled map from user-persisted state.
        // Tools not present in the map default to disabled (opt-in).
        let user_map: HashMap<String, bool> = enabled_entries
            .iter()
            .map(|e| (e.name.clone(), e.enabled))
            .collect();

        self.tools
            .iter()
            .map(|tool| {
                let name = tool.name();
                let user_wants = user_map.get(&name).copied().unwrap_or(false);
                let wrapped = crate::tools::wrappers::wrap_with_security_decorators(
                    tool.clone(),
                    resolver.clone(),
                    max_calls_per_minute,
                );
                BuiltinToolEntry::with_resolved_enabled(user_wants, wrapped)
            })
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::items_after_test_module)]
#[cfg(test)]
mod tests {

    use super::*;
    use acowork_core::tools::traits::{ToolResult, ToolSpec};
    use async_trait::async_trait;
    use serde_json::Value;
    struct MockTool {
        name: String,
    }

    #[async_trait]
    impl Tool for MockTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.clone(),
                description: format!("Mock tool {}", self.name),
                input_schema: serde_json::json!({"type": "object"}),
            }
        }

        async fn execute(
            &self,
            _params: Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: format!("Mock {} executed", self.name),
                error: None,
                token_usage: None,
            })
        }
    }

    fn create_registry() -> ToolRegistry {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(MockTool {
            name: "shell".to_string(),
        }));
        reg.register(Arc::new(MockTool {
            name: "calculator".to_string(),
        }));
        reg.register(Arc::new(MockTool {
            name: "weather".to_string(),
        }));
        reg.register(Arc::new(MockTool {
            name: "memory_store".to_string(),
        }));
        reg
    }

    fn manifest_with_tools(tool_names: &[&str]) -> AgentManifest {
        let tools_toml = tool_names
            .iter()
            .map(|name| format!("[[tools]]\nname = \"{}\"", name))
            .collect::<Vec<_>>()
            .join("\n");
        let toml_str = format!(
            r#"

            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[permissions]]
            type = "Shell"

            [[permissions]]
            type = "Network"

            [[permissions]]
            type = "Wasm"

            {}
            "#,
            tools_toml
        );

        AgentManifest::from_toml(&toml_str).unwrap()
    }

    #[test]
    fn test_registry_register_and_get() {
        let reg = create_registry();
        assert!(reg.get("shell").is_some());
        assert!(reg.get("calculator").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_tool_names() {
        let reg = create_registry();
        let names = reg.tool_names();
        assert!(names.contains(&"shell".to_string()));
        assert!(names.contains(&"calculator".to_string()));
    }

    #[test]
    fn test_registry_activate_returns_all_tools() {
        let reg = create_registry();
        let manifest = manifest_with_tools(&["shell", "calculator"]);
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));
        let enabled = vec![
            crate::agent_config::AgentToolEntry::new("shell", true),
            crate::agent_config::AgentToolEntry::new("calculator", true),
            crate::agent_config::AgentToolEntry::new("weather", true),
            crate::agent_config::AgentToolEntry::new("memory_store", true),
        ];
        let activated = reg.activate(&manifest, &resolver, 60, &enabled);
        assert_eq!(activated.len(), 4);
        // All enabled
        assert!(activated.iter().all(|e| e.enabled));
    }

    #[test]
    fn test_registry_activate_filters_by_enabled() {
        let reg = create_registry();
        let manifest = manifest_with_tools(&["shell", "calculator"]);
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));
        // Only enable two tools
        let enabled = vec![
            crate::agent_config::AgentToolEntry::new("shell", true),
            crate::agent_config::AgentToolEntry::new("weather", true),
            crate::agent_config::AgentToolEntry::new("calculator", false),
            crate::agent_config::AgentToolEntry::new("memory_store", false),
        ];
        let activated = reg.activate(&manifest, &resolver, 60, &enabled);
        assert_eq!(
            activated.len(),
            4,
            "all 4 are returned (disabled ones kept for introspection)"
        );
        let shell = activated.iter().find(|e| e.name() == "shell").unwrap();
        let calc = activated.iter().find(|e| e.name() == "calculator").unwrap();
        assert!(shell.enabled);
        assert!(!calc.enabled);
    }

    #[test]
    fn test_registry_activate_no_manifest_tools() {
        let reg = create_registry();
        let toml_str = r#"

            agent_id = "com.test.agent"
            version = "1.0.0"
            name = "Test Agent"
            description = "Test"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

        "#;
        let manifest = AgentManifest::from_toml(toml_str).unwrap();
        let resolver: SharedResolver =
            Arc::new(std::sync::RwLock::new(WorkspaceResolver::new("/tmp/test")));
        // No enabled_entries → all default to false (opt-in semantics)
        let activated = reg.activate(&manifest, &resolver, 60, &[]);
        assert_eq!(activated.len(), 4);
        assert!(activated.iter().all(|e| !e.enabled));
    }

    // ── BuiltinToolEntry: persisted preference is the only signal ─────
    //
    // The persistence layer (agent_config) owns the canonical
    // enable-state for each agent. The registry simply honors it —
    // there is no second override at this layer.

    use crate::agent::agent_core::BuiltinToolEntry;

    #[test]
    fn builtin_tool_entry_honors_user_preference() {
        // `with_resolved_enabled` is a thin pass-through. The persisted
        // user preference IS the canonical enable-state — there is no
        // second override at the registry layer (ADR-029).
        let off_tool: Arc<dyn Tool> = Arc::new(MockTool {
            name: "shell".to_string(),
        });
        let entry = BuiltinToolEntry::with_resolved_enabled(false, off_tool);
        assert!(!entry.enabled, "user-disabled tool stays disabled");

        let on_tool: Arc<dyn Tool> = Arc::new(MockTool {
            name: "shell".to_string(),
        });
        let entry = BuiltinToolEntry::with_resolved_enabled(true, on_tool);
        assert!(entry.enabled, "user-enabled tool stays enabled");
    }

    #[test]
    fn test_registry_default() {
        let reg = ToolRegistry::default();
        assert!(reg.all().is_empty());
    }
}
