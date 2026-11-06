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
    /// Platform default for a **newly created agent**: the Create wizard
    /// pre-selects exactly these, so a fresh package declares them in
    /// `[[tools]]` (ADR-029 keeps the Runtime opt-in — an undeclared tool
    /// starts disabled, so "not pre-selected" really means "the agent
    /// cannot call it").
    ///
    /// The flag lives in the catalog, not in the wizard, because "which
    /// tools a baseline agent needs" is a platform fact shared with every
    /// client — a second copy in one frontend is the drift this catalog
    /// exists to prevent.
    pub default_enabled: bool,
}

/// Catalog of builtin tools the Runtime ships with.
///
/// Kept in sync with `acowork-runtime/src/tools/builtin/mod.rs`
/// `all_builtin_tools` by hand — the Runtime's registration list is
/// the dynamic one (depends on platform, LSP relay presence, etc.),
/// and a const-time derivation would require `inventory` / linker
/// tricks that are not worth their weight for a wizard checkbox list.
///
/// Memory tools are absent by the same rule as `codebase` (ADR-101):
/// `memory_store` / `memory_recall` are no longer built-ins — they are
/// tools of the Gateway-hosted memory MCP server, opted in through
/// `agent_mcp_tools.json` under their prefixed names (ADR-069), so a
/// builtin checkbox for them could never take effect.
pub const BUILTIN_TOOLS: &[BuiltinToolInfo] = &[
    // `default_enabled` = pre-selected in the Create wizard. The baseline is
    // "a working agent": read/write files, run a shell, talk to other agents,
    // read the bound chat, ask the user, track a todo list, load a skill.
    // Everything else (search, network, doc reader, MCP management, RAG) is a
    // per-agent decision the author opts into.
    BuiltinToolInfo { name: "file_read",       description: "Read file contents (with line range)",                     group: Some("Filesystem"),    default_enabled: false },
    BuiltinToolInfo { name: "file_write",      description: "Create or overwrite files",                                group: Some("Filesystem"),    default_enabled: true  },
    BuiltinToolInfo { name: "file_edit",       description: "Edit files via line-based patches",                        group: Some("Filesystem"),    default_enabled: true  },
    BuiltinToolInfo { name: "glob_search",     description: "Find files matching a glob pattern",                       group: Some("Filesystem"),    default_enabled: false },
    BuiltinToolInfo { name: "content_search",  description: "Search file contents by regex",                             group: Some("Filesystem"),    default_enabled: false },
    BuiltinToolInfo { name: "doc_reader",      description: "Read structured documents (PDF, DOCX, ...)",              group: Some("Filesystem"),    default_enabled: false },
    BuiltinToolInfo { name: "http_request",    description: "Make raw HTTP requests",                                    group: Some("Network"),       default_enabled: false },
    BuiltinToolInfo { name: "web_fetch",       description: "Fetch and extract readable content from web pages",        group: Some("Network"),       default_enabled: false },
    BuiltinToolInfo { name: "intent_send",     description: "Send messages to other agents via the Intent router",      group: Some("Communication"), default_enabled: true  },
    BuiltinToolInfo { name: "chat_history_read", description: "Read the bound Chat session's history (user-agent / agent-agent / group chat)", group: Some("Communication"), default_enabled: true },
    BuiltinToolInfo { name: "ask_user_question", description: "Ask the user a structured multi-option question",        group: Some("Communication"), default_enabled: true  },
    BuiltinToolInfo { name: "todo_write",      description: "Maintain a structured todo list for the task at hand",      group: Some("Workflow"),      default_enabled: true  },
    BuiltinToolInfo { name: "mcp_install",     description: "Install an MCP server into the agent's MCP config",        group: Some("MCP"),           default_enabled: false },
    BuiltinToolInfo { name: "mcp_uninstall",   description: "Remove an MCP server from the agent's MCP config",          group: Some("MCP"),           default_enabled: false },
    BuiltinToolInfo { name: "rag_query",       description: "Query a RAG knowledge base (requires a RAG manifest entry)",group: Some("Advanced"),      default_enabled: false },
    BuiltinToolInfo { name: "load_skill",      description: "Load the full instructions of a named skill on demand",     group: Some("Advanced"),      default_enabled: true  },
    // Shell tools are registered per host (`platform::detected_shells`), so the
    // manifest convention is to declare every variant and let the Runtime drop
    // the unresolvable ones (`init_tools_config_from_manifest`).
    BuiltinToolInfo { name: "bash",            description: "Execute commands via Git Bash (Windows)",                   group: Some("Shell"),         default_enabled: false },
    BuiltinToolInfo { name: "powershell",      description: "Execute commands via PowerShell (Windows)",                 group: Some("Shell"),         default_enabled: true  },
    BuiltinToolInfo { name: "shell",           description: "Execute commands via the system shell (Linux / macOS)",      group: Some("Shell"),         default_enabled: true  },
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

    /// The pre-selected set is a product decision, not an implementation
    /// detail: a fresh agent created from the wizard can only call what its
    /// `[[tools]]` declares (ADR-029 opt-in), so this list IS the baseline
    /// capability of every new Agent. Pinned so widening or narrowing it is a
    /// deliberate, reviewed edit rather than a stray `true`.
    #[test]
    fn default_enabled_set_is_pinned() {
        let mut actual: Vec<&str> = BUILTIN_TOOLS
            .iter()
            .filter(|t| t.default_enabled)
            .map(|t| t.name)
            .collect();
        actual.sort_unstable();
        let mut expected = vec![
            "ask_user_question",
            "chat_history_read",
            "file_edit",
            "file_write",
            "intent_send",
            "load_skill",
            "powershell",
            "shell",
            "todo_write",
        ];
        expected.sort_unstable();
        assert_eq!(actual, expected);
    }

    /// A default-on tool that the catalog does not list (or lists twice) would
    /// pre-select a checkbox the wizard never renders — an invisible
    /// declaration the author cannot undo.
    #[test]
    fn every_default_enabled_tool_is_a_catalog_entry() {
        for name in BUILTIN_TOOLS
            .iter()
            .filter(|t| t.default_enabled)
            .map(|t| t.name)
        {
            assert!(
                BUILTIN_TOOLS.iter().filter(|t| t.name == name).count() == 1,
                "{name} must appear exactly once in the catalog"
            );
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
        // ADR-101 moved memory out of the Runtime: `memory_store` /
        // `memory_recall` are MCP tools of the hosted memory service and are
        // opted in per ADR-069 under their prefixed names
        // (`mcp_memory__memory_recall`), not as builtins.
        for name in ["memory_recall", "memory_store"] {
            assert!(
                !BUILTIN_TOOLS.iter().any(|t| t.name == name),
                "{name} must not be offered as a builtin tool (ADR-101)"
            );
        }
    }
}

