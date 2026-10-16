//! Built-in tools module
//!
//! Phase 1: 13 built-in tools per design doc (12-tool-system.md)
//! Phase 4 (S4.4): +1 RAG tool (rag_query, registered in agent_init.rs when
//!                   manifest declares a RAG tool entry)
//!
//! | Tool | Permission |
//! |------|------------|
//! | memory_recall | memory:read |
//! | memory_store | memory:write |
//! | http_request | network:<url> |
//! | web_fetch | network:<url> |
//! | shell | filesystem:exec |
//! | file_read | filesystem:read:<path> |
//! | file_write | filesystem:write:<path> |
//! | file_edit | filesystem:write:<path> |
//! | doc_reader | filesystem:read:<path> |
//! | glob_search | filesystem:read:<path> |
//! | content_search | filesystem:read:<path> |
//! | intent_send | intent:send:<target> |
//! | chat_history_read | (no permission table entry) — participation is enforced server-side (design 27 §9) |
//! | rag_query | rag:query + network:<rag_url> (registered in agent_init.rs) |
//! | context_abandon | context:write - RETIRED (dead code, not registered) |
//! | ask_user_question | (no permission — LLM-initiated, always allowed) |
//! | codebase | (no permission table entry) - TEMPORARILY UNREGISTERED |
//!
//! ponytail: the `codebase` tool is temporarily out of the builtin
//! registry — it is neither registered at startup nor pushed
//! dynamically by `SessionManager::handle_lsp_relay_update`, and it is
//! absent from the wizard catalog (`acowork_core::tools::BUILTIN_TOOLS`).
//! The source stays (`tools/builtin/codebase.rs`) with its unit tests
//! live. To bring it back: re-add the `if let Some(endpoint)` push in
//! `all_builtin_tools`, the `Some(_) =>` arm of
//! `handle_lsp_relay_update`, `"codebase"` in
//! `CONDITIONALLY_REGISTERED_TOOL_NAMES`, and its `BUILTIN_TOOLS`
//! entry.

pub mod ask_user_question;
pub mod chat_history_read;
pub mod codebase;
pub mod content_search;
pub mod context_abandon;
pub mod doc_reader;
pub mod file_edit;
pub mod file_read;
pub mod file_write;
pub mod glob_search;
pub mod http_request;
pub mod intent_send;
pub mod load_skill;
pub mod mcp_install;
pub mod mcp_uninstall;
pub mod memory_recall;
pub mod memory_store;
pub mod rag_query;
pub mod shell;
pub mod todo_write;
pub mod web_fetch;

/// Names of builtin tools that may be **conditionally registered** —
/// present in the codebase and fully valid, but only constructed when a
/// runtime dependency is available at the moment of registration:
///
/// - `rag_query` — requires a manifest `[[tools]]` entry of `type = "rag"`
///
/// At startup (`agent_init.rs`) these tools can be absent from the code
/// registry even though the user has a persisted entry for them in
/// `agent_tools.json`. The persistence layer
/// ([`crate::agent_config::merge_tools_config`]) must NOT treat such an
/// entry as "removed by a Runtime upgrade" and drop it — dropping (and
/// then writing the merge result back to disk) would erase the user's
/// enable/disable preference. This constant is the single source of
/// truth for that distinction.
///
/// ponytail: `codebase` was removed from this list together with its
/// registration (temporarily unregistered — see the module header).
/// That is deliberate: a persisted `codebase` entry must now be dropped
/// by the startup merge and written back, so the stale row disappears
/// from the Tools panel instead of lingering as an unrenderable ghost.
pub const CONDITIONALLY_REGISTERED_TOOL_NAMES: &[&str] = &["rag_query"];

// ── Shared limits ──────────────────────────────────────────────────────

/// Maximum number of lines any single fragment-reader call may return.
///
/// Shared by [`file_read`] (mandatory `start_line` / `end_line`) and
/// [`doc_reader`] plain-text fallback (optional `start_line` /
/// `end_line`).  The cap prevents one tool call from dumping megabytes of
/// source text into the LLM context — when more is needed, paginate
/// across multiple calls (the file_read error message gives the LLM an
/// explicit paginate template).
///
/// Set high enough (100 lines ≈ 2-4 KB of typical source) to keep most
/// real-world reads in one round-trip, low enough that two concurrent
/// near-cap results still fit well under [`crate::tools::output::MAX_OUTPUT_BYTES`]
/// (32 KB).
///
/// History: was 400, lowered to 100 to cut per-call token cost (most reads
/// never approached 400 lines; truncation by the 32 KB wrapper rarely fired).
pub const MAX_LINES_PER_CALL: usize = 100;

use acowork_core::tools::traits::Tool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::mcp_notify::McpNotifyRef;
use crate::tools::workspace_resolver::SharedResolver;

/// Create the standard built-in tools (without RAG).
///
/// Shell tools are registered dynamically based on platform detection:
/// - Windows: Git Bash (bash) + PowerShell, or just PowerShell if Git not found
/// - Linux/macOS: Single "shell" tool using system shell
///
/// # Arguments
/// * `resolver` - Workspace directory resolver (single source of truth)
/// * `agent_id` - Agent ID for memory isolation and identity management
/// * `tool_http_timeout_ms` - Default HTTP timeout in milliseconds for built-in tools
/// * `memory_session` - Optional MemorySessionHandle for memory_recall and memory_store late-binding store access.
/// * `mcp_notifier` - Optional McpConfigNotifier for mcp_install/mcp_uninstall event notification.
/// * `agent_home` - Agent home directory (from `config().work_dir`). Required by mcp_install/mcp_uninstall
///   for config persistence — MCP configs are per-agent, stored in `{agent_home}/config/agent_mcp.json`,
///   not per-project. No fallback: must always be set explicitly.
///
/// `#[allow(clippy::too_many_arguments)]` follows the project convention
/// for thin pass-through facades (cf. `AgentCore::new_with_observer`): the
/// arguments are independent assembly dependencies for the tool registry.
///
/// ADR-094: `package_dir` is the agent's `.agent` package root —
/// required by the `load_skill` tool to read
/// `<package_dir>/skills/<name>/SKILL.md`. Distinct from `agent_home`
/// (which is the `work_dir` runtime state root).
#[allow(clippy::too_many_arguments)]
pub fn all_builtin_tools(
    resolver: &SharedResolver,
    agent_id: &str,
    tool_http_timeout_ms: u64,
    memory_session: Option<Arc<crate::memory::MemorySessionHandle>>,
    mcp_notifier: McpNotifyRef,
    agent_home: String,
    intent_send_config: crate::tools::builtin::intent_send::IntentSendConfig,
    package_dir: PathBuf,
) -> Vec<Arc<dyn Tool>> {
    // Register shell tools based on platform detection
    let shell_tools: Vec<Arc<dyn Tool>> = crate::platform::detected_shells()
        .into_iter()
        .map(|s| {
            Arc::new(shell::ShellTool::new(
                &s.tool_name,
                &s.display_name,
                &s.binary,
                &s.path,
                &s.arg,
            )) as Arc<dyn Tool>
        })
        .collect();

    // Clone memory_session for MemoryStoreTool before the first use
    // (MemoryRecallTool takes ownership of the original).
    let memory_session_for_store = memory_session.clone();

    let mut tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(memory_recall::MemoryRecallTool::new(
            agent_id,
            memory_session,
        )),
        Arc::new(memory_store::MemoryStoreTool::new(
            agent_id,
            memory_session_for_store,
        )),
        Arc::new(http_request::HttpRequestTool::new()),
        Arc::new(web_fetch::WebFetchTool::with_timeout(
            Duration::from_millis(tool_http_timeout_ms),
        )),
        Arc::new(file_read::FileReadTool::new()),
        Arc::new(file_write::FileWriteTool::new()),
        Arc::new(file_edit::FileEditTool::new()),
        Arc::new(doc_reader::DocReaderTool::new()),
        Arc::new(glob_search::GlobSearchTool::new(resolver)),
        Arc::new(content_search::ContentSearchTool::new(resolver)),
        Arc::new(intent_send::IntentSendTool::new(intent_send_config.clone())),
        Arc::new(chat_history_read::ChatHistoryReadTool::new(intent_send_config)),
        Arc::new(ask_user_question::AskUserQuestionTool::new()),
        Arc::new(todo_write::TodoWriteTool::new()),
        Arc::new(load_skill::LoadSkillTool::new(package_dir)),
        Arc::new(mcp_install::McpInstallTool::new(
            mcp_notifier.clone(),
            agent_home.clone(),
        )),
        Arc::new(mcp_uninstall::McpUninstallTool::new(
            mcp_notifier.clone(),
            agent_home.clone(),
        )),
    ];

    // Append platform-specific shell tools
    tools.extend(shell_tools);
    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_notify::McpConfigNotifier;
    use crate::memory::MemorySessionHandle;
    use crate::tools::workspace_resolver::WorkspaceResolver;

    /// Build a minimal set of dependencies for `all_builtin_tools` testing.
    /// Most dependencies are empty/default so the test focuses on the
    /// builtin tool registration surface.
    ///
    /// `#[allow(clippy::type_complexity)]`: the returned tuple mirrors
    /// `all_builtin_tools`'s assembly dependencies one-to-one; naming a
    /// struct for a single test helper would add indirection without value.
    #[allow(clippy::type_complexity)]
    fn make_test_deps() -> (
        SharedResolver,
        String, // agent_id
        u64,    // tool_http_timeout_ms
        Option<Arc<MemorySessionHandle>>,
        McpNotifyRef,
        String, // agent_home
        crate::tools::builtin::intent_send::IntentSendConfig,
        PathBuf, // package_dir (ADR-094: `load_skill` reads from here)
    ) {
        let dir = tempfile::tempdir().unwrap();
        let resolver = Arc::new(std::sync::RwLock::new(WorkspaceResolver::new(
            dir.path().to_str().unwrap(),
        )));
        let memory_session = Some(Arc::new(MemorySessionHandle::new(None)));
        let mcp_notifier: McpNotifyRef = Some(Arc::new(McpConfigNotifier::default()));
        let intent_cfg = crate::tools::builtin::intent_send::IntentSendConfig {
            instance_id: "3f8c2a1b-4d5e-6f7a-8b9c-0d1e2f3a4b5c".to_string(),
            gateway_host: "127.0.0.1".to_string(),
            node_token: String::new(),
        };
        (
            resolver,
            "com.test.agent".to_string(),
            30_000,
            memory_session,
            mcp_notifier,
            "/tmp/test-agent".to_string(),
            intent_cfg,
            // ADR-094: the `load_skill` tool is constructed with a
            // package dir; in tests we point it at an empty tempdir
            // (no skills/ subdir) so it returns an empty registry.
            dir.path().to_path_buf(),
        )
    }

    /// Collect tool names from a `Vec<Arc<dyn Tool>>`.
    fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        let mut names: Vec<String> = tools.iter().map(|t| t.spec().name).collect();
        names.sort();
        names
    }

    #[test]
    fn test_all_builtin_tools_does_not_register_codebase() {
        // `codebase` is temporarily unregistered (module header): it must
        // not reach the LLM tool surface. This is the smallest check that
        // fails if someone re-adds the registration without also restoring
        // the `codebase` branch in `handle_lsp_relay_update` and the
        // `BUILTIN_TOOLS` catalog entry.
        let (resolver, agent_id, timeout, mem, mcp, agent_home, intent_cfg, package_dir) =
            make_test_deps();

        let tools = all_builtin_tools(
            &resolver, &agent_id, timeout, mem, mcp, agent_home, intent_cfg, package_dir,
        );

        let names = tool_names(&tools);
        assert!(
            !names.contains(&"codebase".to_string()),
            "codebase must NOT be registered (temporarily out of the registry); got: {:?}",
            names
        );
    }

    #[test]
    fn test_all_builtin_tools_does_not_register_compression_tools() {
        // Tool compression (ADR-052) has been retired from the LLM-facing
        // tool surface (ADR-061 §10). `context_retrieve` was deleted
        // outright once it turned out to scan every session file instead of
        // the current one; `context_abandon` survives as dead code.
        let (resolver, agent_id, timeout, mem, mcp, agent_home, intent_cfg, package_dir) =
            make_test_deps();

        let tools = all_builtin_tools(
            &resolver, &agent_id, timeout, mem, mcp, agent_home, intent_cfg, package_dir,
        );

        let names = tool_names(&tools);
        assert!(
            !names.contains(&"context_abandon".to_string()),
            "context_abandon must NOT be registered (compression retired); got: {:?}",
            names
        );
    }

    #[test]
    fn test_all_builtin_tools_default_includes_core_tools() {
        // Regression: platform-tool changes only affect the compression
        // tools; all other core tools are always present.
        let (resolver, agent_id, timeout, mem, mcp, agent_home, intent_cfg, package_dir) =
            make_test_deps();

        let tools = all_builtin_tools(
            &resolver, &agent_id, timeout, mem, mcp, agent_home, intent_cfg, package_dir,
        );

        let names = tool_names(&tools);
        // Core tools that must always be present (sanity check)
        for required in [
            "memory_recall",
            "memory_store",
            "http_request",
            "web_fetch",
            "file_read",
            "file_write",
            "file_edit",
            "doc_reader",
            "glob_search",
            "content_search",
            "intent_send",
            "ask_user_question",
            "todo_write",
            "mcp_install",
            "mcp_uninstall",
            // ADR-094: load_skill is unconditionally registered
            // (it's a builtin tool, not a conditionally-registered
            // one like `rag_query` which needs a manifest RAG entry).
            "load_skill",
        ] {
            assert!(
                names.contains(&required.to_string()),
                "Core tool {required} should always be registered; got: {:?}",
                names
            );
        }
    }

    // ── Cross-constraint invariants ────────────────────────────────
    //
    // These constants are referenced from multiple tools. Drift between
    // them would silently break contracts. The static checks below catch
    // obvious misconfigurations at compile time / test time rather than
    // at runtime when an LLM session mysteriously fails.

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn max_lines_per_call_leaves_headroom_for_other_context() {
        // 100 lines × ~80 bytes (typical source) ≈ 8 KB. Two concurrent
        // fragment-reader results at the upper bound would saturate
        // the per-tool cap, leaving no room for history / system prompt.
        // The realistic guarantee is enforced by `output::truncate_output`
        // as a final safety net in both `file_read` and `doc_reader`,
        // but we still want the *nominal* case to fit comfortably.
        //
        // Bound: MAX_LINES_PER_CALL × 100 bytes (pathological source) <
        // 2 × MAX_OUTPUT_BYTES. Anything tighter would force truncation
        // on every call.
        let nominal = MAX_LINES_PER_CALL * 100;
        assert!(
            nominal <= crate::tools::output::MAX_OUTPUT_BYTES * 2,
            "MAX_LINES_PER_CALL ({}) × 100 bytes = {} must fit under 2 × MAX_OUTPUT_BYTES ({} bytes); \
             otherwise every paged fragment-read needs truncation",
            MAX_LINES_PER_CALL,
            nominal,
            crate::tools::output::MAX_OUTPUT_BYTES * 2
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn max_lines_per_call_is_a_positive_value() {
        // These are constant comparisons today, but kept as runtime
        // asserts so future constant changes produce a clear test
        // failure with the actual values, rather than a confusing
        // compile-time panic.
        assert!(MAX_LINES_PER_CALL > 0);
        // And not absurdly large either — if it's > MAX_OUTPUT_BYTES
        // then file_read's per-line summary format alone blows the cap.
        assert!(
            MAX_LINES_PER_CALL * 20 < crate::tools::output::MAX_OUTPUT_BYTES,
            "MAX_LINES_PER_CALL ({}) must be small enough that 20-char lines \
             fit under MAX_OUTPUT_BYTES ({} bytes)",
            MAX_LINES_PER_CALL,
            crate::tools::output::MAX_OUTPUT_BYTES
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn shell_budget_is_a_strict_subset_of_global_cap() {
        // Cross-file invariant: the shell tool's head+tail budget must
        // stay well under the global cap so a single shell call never
        // single-handedly blows context.
        let shell_budget =
            crate::tools::output::MAX_SHELL_HEAD_BYTES + crate::tools::output::MAX_SHELL_TAIL_BYTES;
        assert!(
            shell_budget < crate::tools::output::MAX_OUTPUT_BYTES,
            "shell head+tail budget ({} bytes) must be < MAX_OUTPUT_BYTES ({} bytes)",
            shell_budget,
            crate::tools::output::MAX_OUTPUT_BYTES
        );
    }
}
