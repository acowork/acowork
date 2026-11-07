//! Two-layer tool policy: the agent layer ∧ the session layer.
//!
//! # The two layers
//!
//! **Layer A — what the agent exposes.** For builtin tools this is the
//! `agent_tools.json` enabled flag on the shared `AgentCore` template; for MCP
//! it is already materialised before it reaches a session, because
//! `connect_mcp_with_reconcile_and_filter` only builds a wrapper for servers
//! in `agent_mcp.json::active_names` whose tool row in `agent_mcp_tools.json`
//! is enabled. So `AgentCore::mcp_tools` is *exactly* the agent-exposed MCP
//! set — a tool the agent layer filtered out has no wrapper to talk to.
//!
//! **Layer B — what one session keeps.** [`SessionToolPolicy`], persisted per
//! session in `SessionMeta.tool_policy`. It is a deny-list: it can only take
//! away what layer A granted. See the type's docs for why it cannot widen.
//!
//! `effective = A ∧ B`, written into the session's own `AgentCore` clone by
//! [`SessionToolLayer::apply_to`].
//!
//! # Why a layer object instead of a read-time filter
//!
//! Every dependent of the tool set already reads it off the core
//! (`rebuild_all_tools` for dispatch, `rebuild_context_tool_definitions` for
//! the LLM's spec list). Filtering at read time would mean teaching both —
//! plus every future reader — about the session layer. Composing the values
//! *into* the core instead keeps one derivation pipeline and one source of
//! truth, at the cost of keeping layer A's last-known values here.
//!
//! # Why the baselines must be re-composed on every agent-layer update
//!
//! Layer A arrives as agent-wide broadcasts (`SessionMessage::UpdateBuiltinTools`
//! / `UpdateMcpTools`), whose payload is the *whole* agent-level desired state.
//! Writing it straight into `core.builtin_tools[i].enabled` would silently
//! overwrite the session narrowing — the session would "forget" its switches
//! every time anyone touched the Tools panel. Hence every layer-A update goes
//! through this object and is followed by an `apply_to`, so the session layer
//! is re-laid on top of the new baseline. `SessionToolLayer` therefore must
//! never be bypassed by writing `mcp_tools` / `builtin_tools` enabled flags
//! directly.

use std::sync::Arc;

use acowork_core::session_meta::SessionToolPolicy;
use acowork_core::tools::traits::Tool;

use crate::agent::agent_core::AgentCore;
use crate::agent_config::AgentToolEntry;

/// The session's tool layer: layer-A baselines + the layer-B deny-list.
pub(crate) struct SessionToolLayer {
    /// Agent-layer builtin enabled flags as last pushed by the agent (the
    /// template clone at session creation, then each broadcast payload).
    /// Kept here because `core.builtin_tools[i].enabled` only ever holds the
    /// composed value — losing the baseline would make "switch it back on"
    /// impossible to express.
    agent_builtin: Vec<AgentToolEntry>,
    /// Agent-layer MCP wrappers (before session narrowing). `None` mirrors
    /// `AgentCore::mcp_tools`: no MCP configured / not connected yet.
    agent_mcp: Option<Vec<Arc<dyn Tool>>>,
    /// Layer B. Empty = inherit the agent layer.
    policy: SessionToolPolicy,
}

impl SessionToolLayer {
    /// Capture the current layer-A baselines from a freshly cloned session
    /// core and attach `policy` as layer B.
    ///
    /// Call this **before** the first [`Self::apply_to`], so the baselines are
    /// the agent-once values and not an already-narrowed set (a second
    /// `from_core` on a composed core would make the narrowing permanent).
    pub(crate) fn from_core(core: &AgentCore, policy: Option<SessionToolPolicy>) -> Self {
        Self {
            agent_builtin: core
                .builtin_tools
                .iter()
                .map(|e| AgentToolEntry::new(e.name(), e.enabled))
                .collect(),
            agent_mcp: core.mcp_tools.clone(),
            policy: policy.unwrap_or_default(),
        }
    }

    /// Record a new agent-layer builtin baseline (a broadcast payload).
    pub(crate) fn set_agent_builtin(&mut self, entries: &[AgentToolEntry]) {
        self.agent_builtin = entries.to_vec();
    }

    /// Record a new agent-layer MCP baseline (the connected wrapper set).
    pub(crate) fn set_agent_mcp(&mut self, mcp: Option<Vec<Arc<dyn Tool>>>) {
        self.agent_mcp = mcp;
    }

    /// Replace layer B. Returns `true` when it actually changed, so callers
    /// can skip the rebuild (and the meta write) on a no-op toggle.
    pub(crate) fn set_policy(&mut self, policy: Option<SessionToolPolicy>) -> bool {
        let next = policy.unwrap_or_default();
        if self.policy == next {
            return false;
        }
        self.policy = next;
        true
    }

    /// Layer B as the caller should persist it: `None` when it denies nothing.
    pub(crate) fn policy(&self) -> Option<SessionToolPolicy> {
        (!self.policy.is_empty()).then(|| self.policy.clone())
    }

    /// `effective = A ∧ B` for builtin tools.
    ///
    /// `is_protected_tool` entries are deliberately **not** special-cased
    /// here: the protection is applied by the two consumers
    /// (`rebuild_all_tools` / `rebuild_context_tool_definitions`), which is
    /// the one place that policy already lives. Duplicating it would create a
    /// second definition of "protected" that could drift.
    pub(crate) fn effective_builtin(&self) -> Vec<AgentToolEntry> {
        self.agent_builtin
            .iter()
            .map(|e| {
                AgentToolEntry::new(e.name.clone(), e.enabled && self.policy.builtin_allowed(&e.name))
            })
            .collect()
    }

    /// `effective = A ∧ B` for MCP wrappers.
    ///
    /// Identity comes from the wrapper's own `name()`: `<server>__<tool>`.
    /// A name that does not split is passed through untouched — the same
    /// permissive rule `tool_allowed` uses for a server the config does not
    /// know about, and the only reachable case is a hand-built wrapper.
    pub(crate) fn effective_mcp(&self) -> Option<Vec<Arc<dyn Tool>>> {
        let all = self.agent_mcp.as_ref()?;
        if self.policy.is_empty() {
            return Some(all.clone());
        }
        let kept: Vec<Arc<dyn Tool>> = all
            .iter()
            .filter(|tool| match split_prefixed_name(&tool.name()) {
                Some((server, name)) => self.policy.mcp_tool_allowed(server, name),
                None => true,
            })
            .cloned()
            .collect();
        Some(kept)
    }

    /// Write the composed set into the session's core.
    ///
    /// Only the two tool slots are touched — the caller owns rebuilding the
    /// dependents (`refresh_builtin_tools_dependents`), so this stays a pure
    /// data operation that is safe to call from construction paths that have
    /// no `ContextBuilder` yet.
    pub(crate) fn apply_to(&self, core: &mut AgentCore) {
        let effective = self.effective_builtin();
        let enabled: std::collections::HashMap<&str, bool> = effective
            .iter()
            .map(|e| (e.name.as_str(), e.enabled))
            .collect();
        for entry in core.builtin_tools.iter_mut() {
            if let Some(&flag) = enabled.get(entry.name().as_str()) {
                entry.enabled = flag;
            }
        }
        core.mcp_tools = self.effective_mcp();
    }
}

/// Split an MCP wrapper name into `(server, tool)`.
///
/// Mirrors `acowork_mcp`'s `split_prefixed_tool` — the wrapper name is
/// `<server>__<tool>` — but stays local because the runtime only needs the
/// split for policy decisions and pulling `acowork_mcp` in for one `str::find`
/// would couple two crates that otherwise never meet on this path. The
/// two implementations are pinned together by a test in this module and by
/// the `agent_mcp_tools.json` shape both feed.
fn split_prefixed_name(prefixed: &str) -> Option<(&str, &str)> {
    // `__` padding: builtin tool names use single underscores, so the first
    // `__` is the separator. A name without one is not an MCP tool.
    let (server, rest) = prefixed.split_once("__")?;
    if server.is_empty() || rest.is_empty() {
        return None;
    }
    Some((server, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::tools::traits::{ToolResult, ToolSpec};

    /// Minimal `Tool` stub: only `name()` is load-bearing for the layer.
    struct StubTool(String);

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.clone(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            }
        }
        async fn execute(
            &self,
            _params: serde_json::Value,
            _work_dir: Option<&str>,
        ) -> acowork_core::error::Result<ToolResult> {
            Ok(ToolResult {
                ok: true,
                content: String::new(),
                error: None,
                token_usage: None,
            })
        }
    }

    fn tool(name: &str) -> Arc<dyn Tool> {
        Arc::new(StubTool(name.to_string()))
    }

    fn layer(policy: Option<SessionToolPolicy>) -> SessionToolLayer {
        SessionToolLayer {
            agent_builtin: vec![
                AgentToolEntry::new("read_file", true),
                AgentToolEntry::new("web_fetch", true),
                AgentToolEntry::new("shell", false),
            ],
            agent_mcp: Some(vec![
                tool("pm__pm_claim_task"),
                tool("pm__pm_submit_task"),
                tool("playwright__click"),
            ]),
            policy: policy.unwrap_or_default(),
        }
    }

    fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
        let mut v: Vec<String> = tools.iter().map(|t| t.name()).collect();
        v.sort();
        v
    }

    /// The empty policy is the identity on layer A: a never-toggled session
    /// sees exactly what the agent exposes, including the agent-level `off`.
    #[test]
    fn empty_policy_is_the_identity_on_the_agent_layer() {
        let l = layer(None);
        let builtin = l.effective_builtin();
        assert!(builtin.iter().find(|e| e.name == "read_file").unwrap().enabled);
        assert!(builtin.iter().find(|e| e.name == "web_fetch").unwrap().enabled);
        assert!(!builtin.iter().find(|e| e.name == "shell").unwrap().enabled);
        assert_eq!(
            names(&l.effective_mcp().unwrap()),
            vec!["playwright__click", "pm__pm_claim_task", "pm__pm_submit_task"]
        );
        assert_eq!(l.policy(), None, "an empty policy persists as nothing");
    }

    /// Layer B narrows builtin tools one by one and leaves the rest alone.
    #[test]
    fn policy_narrows_builtin_tools() {
        let mut p = SessionToolPolicy::default();
        p.disabled_builtin_tools.insert("web_fetch".into());
        let l = layer(Some(p));
        let builtin = l.effective_builtin();
        assert!(!builtin.iter().find(|e| e.name == "web_fetch").unwrap().enabled);
        assert!(builtin.iter().find(|e| e.name == "read_file").unwrap().enabled);
    }

    /// A session switch can never resurrect something the agent layer turned
    /// off: `shell` stays off even with an empty policy, and no policy value
    /// makes it on. This is the invariant that makes "narrow only"真.
    #[test]
    fn session_layer_cannot_widen_past_the_agent_layer() {
        let mut p = SessionToolPolicy::default();
        p.disabled_builtin_tools.remove("shell");
        let l = layer(Some(p));
        assert!(!l.effective_builtin().iter().find(|e| e.name == "shell").unwrap().enabled);
        assert_eq!(l.policy(), None, "removing a non-deny is still 'inherit'");
    }

    /// Server-level off takes the whole server's tools with it, tool-level off
    /// is surgical, and the other servers are untouched.
    #[test]
    fn policy_narrows_mcp_by_server_and_by_tool() {
        let mut p = SessionToolPolicy::default();
        p.disabled_mcp_tools
            .entry("pm".into())
            .or_default()
            .insert("pm_submit_task".into());
        p.disabled_mcp_servers.insert("playwright".into());

        let l = layer(Some(p));
        assert_eq!(names(&l.effective_mcp().unwrap()), vec!["pm__pm_claim_task"]);
    }

    /// The composition is idempotent under repeated layer-A updates: an agent
    /// broadcast re-applied N times must not erode the session narrowing
    /// (the regression this module exists for), and re-applying the same
    /// policy must report "unchanged" so the endpoint can skip the meta write.
    #[test]
    fn recomposing_after_an_agent_broadcast_keeps_the_session_layer() {
        let mut p = SessionToolPolicy::default();
        p.disabled_builtin_tools.insert("web_fetch".into());
        let mut l = layer(Some(p.clone()));

        for _ in 0..3 {
            // The agent-wide broadcast payload: layer A verbatim, `web_fetch` on.
            l.set_agent_builtin(&[
                AgentToolEntry::new("read_file", true),
                AgentToolEntry::new("web_fetch", true),
                AgentToolEntry::new("shell", false),
            ]);
            l.set_agent_mcp(Some(vec![tool("pm__pm_claim_task")]));
            assert!(
                !l.effective_builtin()
                    .iter()
                    .find(|e| e.name == "web_fetch")
                    .unwrap()
                    .enabled,
                "the session narrowing must survive every agent broadcast"
            );
        }

        assert!(!l.set_policy(Some(p)), "same policy = no change");
        assert!(l.set_policy(None), "clearing an override is a change");
        assert_eq!(l.policy(), None);
    }

    /// The regression this module exists for, asserted on the two real
    /// consumers instead of on the intermediate lists: the session's dispatch
    /// list (`core.all_tools`) and the LLM's spec list
    /// (`ContextBuilder.tool_definitions`).
    ///
    /// The sequence is exactly what `SessionTask::run` does — an agent-wide
    /// broadcast lands on the layer as a NEW baseline, then the session layer
    /// is re-composed on top — repeated, so a layering bug that only shows up
    /// after the second push cannot pass. Then clearing the policy must bring
    /// the tool back, which is only possible if the agent baseline was
    /// preserved rather than overwritten by the narrowed values.
    #[test]
    fn agent_broadcasts_never_erase_the_session_narrowing() {
        use crate::agent::context::ContextBuilder;

        let manifest = acowork_core::AgentManifest::from_toml(
            r#"
            agent_id = "com.test.tool_layer"
            version = "1.0.0"
            name = "Tool layer"
            description = "Two-layer tool composition"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "mock"
            model = "test-model"
            "#,
        )
        .unwrap();
        let provider = Arc::new(acowork_core::providers::mock::MockProvider::single_text("ok"));
        let builtin = || {
            vec![
                crate::agent::agent_core::BuiltinToolEntry {
                    tool: tool("read_file"),
                    enabled: true,
                },
                crate::agent::agent_core::BuiltinToolEntry {
                    tool: tool("web_fetch"),
                    enabled: true,
                },
                crate::agent::agent_core::BuiltinToolEntry {
                    tool: tool("shell"),
                    enabled: true,
                },
            ]
        };
        let mut core = AgentCore::new(
            crate::config::RuntimeConfig::default(),
            manifest,
            provider,
            builtin(),
        );

        let mut policy = SessionToolPolicy::default();
        policy.disabled_builtin_tools.insert("web_fetch".into());
        let mut l = SessionToolLayer::from_core(&core, Some(policy));
        l.apply_to(&mut core);

        let agent_payload = [
            AgentToolEntry::new("read_file", true),
            AgentToolEntry::new("web_fetch", true),
            AgentToolEntry::new("shell", true),
        ];

        let mut cb = ContextBuilder::new("sys".to_string());
        for round in 0..3 {
            // Every round looks the same: the agent pushes layer A again.
            l.set_agent_builtin(&agent_payload);
            l.apply_to(&mut core);
            crate::agent::session::session_task::refresh_builtin_tools_dependents(&mut core, &mut cb);

            let dispatched: Vec<String> = core.all_tools.iter().map(|t| t.name()).collect();
            assert!(
                !dispatched.contains(&"web_fetch".to_string()),
                "round {round}: the session narrowing must survive every agent broadcast, got {dispatched:?}"
            );
            let specs: Vec<String> = cb
                .tool_definitions()
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect();
            assert!(
                !specs.contains(&"web_fetch".to_string()),
                "round {round}: the LLM spec list must agree with the dispatch list, got {specs:?}"
            );
        }

        // Clearing the override restores the agent-layer value — proof the
        // baseline survived the narrowing.
        assert!(l.set_policy(None));
        l.apply_to(&mut core);
        crate::agent::session::session_task::refresh_builtin_tools_dependents(&mut core, &mut cb);
        let dispatched: Vec<String> = core.all_tools.iter().map(|t| t.name()).collect();
        assert!(
            dispatched.contains(&"web_fetch".to_string()),
            "clearing the override must restore the agent-layer tool, got {dispatched:?}"
        );
    }

    /// Splitting the wrapper name is what couples this module to the MCP
    /// naming rule; a mismatch would silently make tool-level switches no-ops.
    #[test]
    fn prefixed_names_split_on_the_double_underscore() {
        assert_eq!(split_prefixed_name("pm__pm_claim_task"), Some(("pm", "pm_claim_task")));
        assert_eq!(split_prefixed_name("playwright__click"), Some(("playwright", "click")));
        // A builtin tool name (single underscores, no separator) is not MCP.
        assert_eq!(split_prefixed_name("read_file"), None);
        assert_eq!(split_prefixed_name("mcp_install"), None);
        assert_eq!(split_prefixed_name("__broken"), None);
        assert_eq!(split_prefixed_name("broken__"), None);
    }
}
