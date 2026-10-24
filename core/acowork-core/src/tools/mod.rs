//! Tool system module

pub mod schema;
pub mod traits;

pub use traits::{Tool, ToolResult, ToolSpec};

/// One entry in the catalog of builtin tools the Runtime knows about.
///
/// Used by the Desktop `CreateWizard` to render a multi-select step
/// without having to maintain a parallel hardcoded list — the catalog
/// is the single source of truth shared with the Runtime
/// ([`acowork_runtime::tools::builtin::all_builtin_tools`]). The
/// `name` field matches the corresponding `Tool::name()` value, so a
/// wizard-selected name always round-trips through
/// `[[tools]] name = "..."` in the manifest.
///
/// The list is **static and exhaustive**: it intentionally covers tools
/// the Runtime registers conditionally (`rag_query` needs a RAG
/// manifest entry, shell tools need a matching binary on the host).
/// Wizard-time we cannot know which of those will actually be live when
/// the agent starts; the Runtime already drops unresolvable entries
/// from `agent_tools.json` via `init_tools_config_from_manifest` (only
/// names present in the live registry survive), so the worst-case effect
/// of the user selecting a tool that turns out to be unavailable is a
/// silent no-op rather than a manifest validation failure.
///
/// `codebase` (LSP-backed) is deliberately absent: it is temporarily
/// unregistered in the Runtime (see `tools/builtin/mod.rs`), so listing
/// it would offer the user a checkbox that can never take effect.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BuiltinToolInfo {
    /// Tool name as registered in the Runtime (`Tool::name()`).
    pub name: &'static str,
    /// One-line description shown in the wizard's tool list.
    pub description: &'static str,
    /// Optional grouping for the wizard UI; `None` puts the tool in
    /// the default group. Lets us surface a small "Communication" /
    /// "Memory" / "Shell" visual cluster without hardcoding labels in
    /// the wizard.
    pub group: Option<&'static str>,
}

/// Catalog of builtin tools the Runtime ships with.
///
/// Kept in sync with `acowork-runtime/src/tools/builtin/mod.rs`
/// `all_builtin_tools` by hand — the Runtime's registration list is
/// the dynamic one (depends on platform, LSP relay presence, etc.),
/// and a const-time derivation would require `inventory` / linker
/// tricks that are not worth their weight for a wizard checkbox list.
///
/// Memory tools are absent by the same rule as `codebase` (ADR-100):
/// `memory_store` / `memory_recall` are no longer built-ins — they are
/// tools of the Gateway-hosted memory MCP server, opted in through
/// `agent_mcp_tools.json` under their prefixed names (ADR-069), so a
/// builtin checkbox for them could never take effect.
pub const BUILTIN_TOOLS: &[BuiltinToolInfo] = &[
    BuiltinToolInfo { name: "file_read",       description: "Read file contents (with line range)",                     group: Some("Filesystem") },
    BuiltinToolInfo { name: "file_write",      description: "Create or overwrite files",                                group: Some("Filesystem") },
    BuiltinToolInfo { name: "file_edit",       description: "Edit files via line-based patches",                        group: Some("Filesystem") },
    BuiltinToolInfo { name: "glob_search",     description: "Find files matching a glob pattern",                       group: Some("Filesystem") },
    BuiltinToolInfo { name: "content_search",  description: "Search file contents by regex",                             group: Some("Filesystem") },
    BuiltinToolInfo { name: "doc_reader",      description: "Read structured documents (PDF, DOCX, ...)",              group: Some("Filesystem") },
    BuiltinToolInfo { name: "http_request",    description: "Make raw HTTP requests",                                    group: Some("Network") },
    BuiltinToolInfo { name: "web_fetch",       description: "Fetch and extract readable content from web pages",        group: Some("Network") },
    BuiltinToolInfo { name: "intent_send",     description: "Send messages to other agents via the Intent router",      group: Some("Communication") },
    BuiltinToolInfo { name: "chat_history_read", description: "Read Chat session (group / agent-agent) history you participate in", group: Some("Communication") },
    BuiltinToolInfo { name: "ask_user_question", description: "Ask the user a structured multi-option question",        group: Some("Communication") },
    BuiltinToolInfo { name: "todo_write",      description: "Maintain a structured todo list for the task at hand",      group: Some("Workflow") },
    BuiltinToolInfo { name: "mcp_install",     description: "Install an MCP server into the agent's MCP config",        group: Some("MCP") },
    BuiltinToolInfo { name: "mcp_uninstall",   description: "Remove an MCP server from the agent's MCP config",          group: Some("MCP") },
    BuiltinToolInfo { name: "rag_query",       description: "Query a RAG knowledge base (requires a RAG manifest entry)",group: Some("Advanced") },
    BuiltinToolInfo { name: "load_skill",      description: "Load the full instructions of a named skill on demand",     group: Some("Advanced") },
    BuiltinToolInfo { name: "bash",            description: "Execute commands via Git Bash (Windows)",                   group: Some("Shell") },
    BuiltinToolInfo { name: "powershell",      description: "Execute commands via PowerShell (Windows)",                 group: Some("Shell") },
    BuiltinToolInfo { name: "shell",           description: "Execute commands via the system shell (Linux / macOS)",     group: Some("Shell") },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_tool_names_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for t in BUILTIN_TOOLS {
            assert!(seen.insert(t.name), "duplicate builtin tool name: {}", t.name);
        }
    }

    #[test]
    fn builtin_tool_catalog_excludes_unregistered_codebase() {
        // The wizard catalog is what the frontend renders. `codebase` is
        // temporarily out of the Runtime registry, so offering a checkbox
        // for it would produce a tool the agent can never call.
        assert!(
            !BUILTIN_TOOLS.iter().any(|t| t.name == "codebase"),
            "codebase must not be offered in the wizard catalog"
        );
    }

    #[test]
    fn builtin_tool_catalog_excludes_service_owned_memory_tools() {
        // ADR-100 moved memory out of the Runtime: `memory_store` /
        // `memory_recall` are MCP tools of the hosted memory service and are
        // opted in per ADR-069 under their prefixed names
        // (`mcp_memory__memory_recall`), not as builtins.
        for name in ["memory_recall", "memory_store"] {
            assert!(
                !BUILTIN_TOOLS.iter().any(|t| t.name == name),
                "{name} must not be offered as a builtin tool (ADR-100)"
            );
        }
    }
}
