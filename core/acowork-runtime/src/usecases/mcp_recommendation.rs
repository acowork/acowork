//! ADR-104: derivation of the Tools-panel "recommended MCP servers"
//! view from `agent_mcp.json` (manifest projection) + the merged install
//! list.
//!
//! Kept as a pure function on purpose: the five-state lifecycle is the
//! contract between the Runtime and every client (Desktop tools tab,
//! mobile), so it must be testable without a filesystem or an HTTP server.

use crate::agent_config::AgentMcpConfig;
use crate::usecases::agent_tools::{McpRecommendationState, McpRecommendationView};
use acowork_core::manifest::{McpServerDecl, McpSourceKind};

/// Project `cfg.recommended` into view rows, in declaration order.
///
/// `installed` is the set of names present in `cfg.merged()` (catalog ∪
/// local); `active` is the set the user ticked for this agent. Dismissed
/// rows are still returned — the client needs them to offer "restore" and
/// to count "N ignored"; the Desktop hides them behind a disclosure.
pub fn build_recommendation_views(
    cfg: &AgentMcpConfig,
    installed: &std::collections::HashSet<String>,
    active: &std::collections::HashSet<String>,
) -> Vec<McpRecommendationView> {
    cfg.recommended
        .iter()
        .filter_map(|decl| {
            let key = decl.key()?.to_string();
            let state = recommendation_state(cfg, &key, installed, active);
            Some(McpRecommendationView {
                source: source_label(decl).to_string(),
                preset: decl.preset.clone(),
                hosted: decl.hosted.clone(),
                url: decl.url.clone(),
                transport: decl.transport.clone(),
                tools: decl.tools.clone(),
                required_secrets: decl.required_secrets.clone(),
                required: decl.required,
                why: decl.why.clone(),
                key,
                state,
            })
        })
        .collect()
}

/// State precedence: dismissed > active > installed > not installed.
///
/// Dismissal wins over everything because it is the only *user* intent in
/// the list — an installation or activation that happened earlier must not
/// silently un-hide a suggestion the user explicitly ignored.
pub fn recommendation_state(
    cfg: &AgentMcpConfig,
    key: &str,
    installed: &std::collections::HashSet<String>,
    active: &std::collections::HashSet<String>,
) -> McpRecommendationState {
    if cfg.is_dismissed(key) {
        McpRecommendationState::Dismissed
    } else if active.contains(key) {
        McpRecommendationState::Active
    } else if installed.contains(key) {
        McpRecommendationState::InstalledInactive
    } else {
        McpRecommendationState::NotInstalled
    }
}

/// Wire label of the declaration form.
fn source_label(decl: &McpServerDecl) -> &'static str {
    match decl.source_kind() {
        Some(McpSourceKind::Preset) => "preset",
        Some(McpSourceKind::Hosted) => "hosted",
        Some(McpSourceKind::InlineRemote) => "inline",
        None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};
    use std::collections::HashSet;

    fn decl_manifest(toml_body: &str) -> acowork_core::manifest::AgentManifest {
        acowork_core::manifest::AgentManifest::from_toml(&format!(
            r#"
agent_id = "com.test.rec"
version = "1.0.0"
name = "Rec"
description = "recommendation test"
author = "test"
runtime_version = "0.1.0"
{toml_body}
"#
        ))
        .expect("manifest should parse")
    }

    fn mcp_def(name: &str) -> McpServerConfigDef {
        McpServerConfigDef {
            name: name.to_string(),
            display_name: None,
            transport: McpTransportDef::Stdio,
            url: None,
            command: "npx".to_string(),
            args: vec![],
            env: std::collections::HashMap::new(),
            headers: std::collections::HashMap::new(),
            tool_timeout_secs: None,
            install: None,
            required_secrets: vec![],
            origin: None,
        }
    }

    fn cfg_for(toml_body: &str) -> AgentMcpConfig {
        let manifest = decl_manifest(toml_body);
        AgentMcpConfig {
            recommended: manifest.mcp_servers,
            ..Default::default()
        }
    }

    fn set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn not_installed_when_absent_from_catalog_and_local() {
        let cfg = cfg_for("[[mcp_servers]]\npreset = \"brave-search\"");
        let views = build_recommendation_views(&cfg, &set(&[]), &set(&[]));
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].key, "brave-search");
        assert_eq!(views[0].source, "preset");
        assert_eq!(views[0].state, McpRecommendationState::NotInstalled);
    }

    #[test]
    fn installed_inactive_then_active() {
        let cfg = cfg_for("[[mcp_servers]]\npreset = \"brave-search\"");
        let views = build_recommendation_views(&cfg, &set(&["brave-search"]), &set(&[]));
        assert_eq!(
            views[0].state,
            McpRecommendationState::InstalledInactive
        );

        let views = build_recommendation_views(
            &cfg,
            &set(&["brave-search"]),
            &set(&["brave-search"]),
        );
        assert_eq!(views[0].state, McpRecommendationState::Active);
    }

    #[test]
    fn dismissal_outranks_installed_and_active() {
        let mut cfg = cfg_for("[[mcp_servers]]\npreset = \"brave-search\"");
        cfg.dismissed = vec!["brave-search".into()];
        let views = build_recommendation_views(
            &cfg,
            &set(&["brave-search"]),
            &set(&["brave-search"]),
        );
        assert_eq!(views[0].state, McpRecommendationState::Dismissed);
    }

    #[test]
    fn hosted_and_inline_sources_are_labelled() {
        let cfg = cfg_for(
            r#"[[mcp_servers]]
hosted = "pm"
tools = ["pm_claim_task"]

[[permissions]]
type = "Network"

[[mcp_servers]]
name = "corp-kb"
transport = "http"
url = "https://mcp.corp.example.com/mcp"
required_secrets = ["CORP_MCP_TOKEN"]
required = true
why = "internal knowledge base""#,
        );
        let views = build_recommendation_views(&cfg, &set(&[]), &set(&[]));
        assert_eq!(views.len(), 2);
        assert_eq!(views[0].source, "hosted");
        assert_eq!(views[0].hosted.as_deref(), Some("pm"));
        assert_eq!(views[0].tools, vec!["pm_claim_task".to_string()]);
        assert_eq!(views[1].source, "inline");
        assert_eq!(views[1].key, "corp-kb");
        assert_eq!(
            views[1].url.as_deref(),
            Some("https://mcp.corp.example.com/mcp")
        );
        assert_eq!(views[1].required_secrets, vec!["CORP_MCP_TOKEN".to_string()]);
        assert!(views[1].required);
        assert_eq!(views[1].why.as_deref(), Some("internal knowledge base"));
    }

    #[test]
    fn empty_recommended_yields_no_views() {
        let cfg = AgentMcpConfig::default();
        assert!(build_recommendation_views(&cfg, &set(&[]), &set(&[])).is_empty());
    }

    /// A locally installed server still counts as "installed" — the merged
    /// list is catalog ∪ local, so an agent that installed its own copy must
    /// not be told to install again.
    #[test]
    fn local_install_counts_as_installed() {
        let mut cfg = cfg_for("[[mcp_servers]]\npreset = \"brave-search\"");
        cfg.local.push(mcp_def("brave-search"));
        let installed: HashSet<String> = cfg.merged().into_iter().map(|s| s.name).collect();
        let views = build_recommendation_views(&cfg, &installed, &set(&[]));
        assert_eq!(
            views[0].state,
            McpRecommendationState::InstalledInactive
        );
    }
}