//! RuntimeAgentToolsService — implements [`AgentToolsService`].
//!
//! ADR-040 follow-up: the Tools-panel persistence endpoints
//! (`/agents/{id}/mcp-servers`, `/agents/{id}/builtin-tools`) used to call
//! `agent_config::*` directly
//! from the HTTP handlers, bypassing the UseCase trait layer. This
//! module consolidates those operations behind a single struct so the
//! HTTP handlers become thin protocol converters — consistent with
//! `RuntimeWorkspaceMutationService` and the rest of the layer.
//!
//! ## State
//!
//! Holds the agent `work_dir` resolved at boot (no async resource
//! dependencies), so the service can be constructed immediately after
//! the workspace services in `session_init.rs` Phase B and doesn't
//! require a late-bind slot's typical "wait for Phase B" setup. We
//! still publish it through a slot so future post-Phase-A consumers
//! (e.g. desktop snapshot streaming) can grab it.
//!
//! ## File layout
//!
//! All disk I/O goes through [`crate::agent_config`] module functions
//! (`load_agent_mcp_config` / `save_agent_mcp_config` and
//! `load_agent_tools_config` / `save_agent_tools_config` for
//! builtin tools). This service is the **single audit point** for the
//! four endpoints — path construction, atomic write-tmp-rename, the
//! `active_names` validation rule for MCP, and the
//! read-modify-write cycle for builtin tools all live here, not in
//! the handlers.
//!
//! ## Validation
//!
//! - `put_mcp_servers` validates every requested name against
//!   `AgentMcpConfig::merged()` (catalog + local) and rejects the
//!   whole batch if any name is unknown. Atomic semantics, no partial
//!   writes.
//! - `put_builtin_tools` does **not** reject unknown tool names — it
//!   silently drops them per ADR-029 §7. The rationale: registered
//!   builtin tools evolve across releases, so the desktop's local
//!   cache can carry names that no longer exist; rejecting them would
//!   force a hard desync. The same `apply_builtin_tools_patch` helper
//!   used by the MQTT `RuntimeConfigUpdate` path runs here for
//!   identical semantics.

use std::collections::HashSet;
use std::path::PathBuf;

use async_trait::async_trait;

use crate::agent_config;
use crate::usecases::agent_tools::{
    AgentToolsError, AgentToolsService, BuiltinToolsResponse, McpServersResponse, McpToolsResponse,
    MergedToolsResponse, PutBuiltinToolsBody, PutMcpRecommendationBody, PutMcpServersBody,
    PutMcpToolsBody,
};

/// Concrete [`AgentToolsService`] backed by the
/// `workspace/config/agent_mcp.json` and `agent_tools.json` files.
pub struct RuntimeAgentToolsService {
    work_dir: PathBuf,
}

impl RuntimeAgentToolsService {
    pub fn new(work_dir: PathBuf) -> Self {
        Self { work_dir }
    }

    /// Single construction point for the `GET /mcp-servers` payload:
    /// active names + the ADR-104 recommendation views derived from the
    /// same loaded config, so the two halves can never disagree.
    fn mcp_servers_snapshot(&self, agent_id: &str) -> McpServersResponse {
        let cfg = agent_config::load_agent_mcp_config(&self.work_dir)
            .ok()
            .flatten()
            .unwrap_or_default();
        let active_servers = cfg.active_server_names();
        let installed: HashSet<String> = cfg.merged().into_iter().map(|s| s.name).collect();
        let active: HashSet<String> = active_servers.iter().cloned().collect();
        let recommended = crate::usecases::mcp_recommendation::build_recommendation_views(
            &cfg, &installed, &active,
        );
        McpServersResponse {
            agent_id: agent_id.to_string(),
            active_servers,
            recommended,
        }
    }
}

#[async_trait]
impl AgentToolsService for RuntimeAgentToolsService {
    async fn get_mcp_servers(&self, agent_id: &str) -> McpServersResponse {
        self.mcp_servers_snapshot(agent_id)
    }

    async fn put_mcp_recommendation(
        &self,
        agent_id: &str,
        key: &str,
        body: PutMcpRecommendationBody,
    ) -> Result<McpServersResponse, AgentToolsError> {
        let mut cfg = agent_config::load_agent_mcp_config(&self.work_dir)
            .map_err(AgentToolsError::Persistence)?
            .unwrap_or_default();

        // Only declared recommendations are addressable: a stale key from a
        // previous package version must not resurrect a phantom row.
        if cfg.recommendation(key).is_none() {
            return Err(AgentToolsError::UnknownRecommendation(key.to_string()));
        }

        cfg.dismissed.retain(|k| k != key);
        if body.dismissed {
            cfg.dismissed.push(key.to_string());
        }
        agent_config::save_agent_mcp_config(&self.work_dir, &cfg)
            .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            key,
            dismissed = body.dismissed,
            "RuntimeAgentToolsService::put_mcp_recommendation: dismissal persisted"
        );

        Ok(self.mcp_servers_snapshot(agent_id))
    }

    async fn put_mcp_servers(
        &self,
        agent_id: &str,
        body: PutMcpServersBody,
    ) -> Result<McpServersResponse, AgentToolsError> {
        // 1. Load current config (catalog + local + prior active_names).
        let mut cfg = agent_config::load_agent_mcp_config(&self.work_dir)
            .map_err(AgentToolsError::Persistence)?
            .unwrap_or_default();

        // 2. Validate every requested name against the merged set
        //    (catalog ∪ local). Empty body is allowed and means
        //    "explicitly no servers active" (Some(vec![])).
        if !body.servers.is_empty() {
            let merged_names: Vec<String> = cfg.merged().into_iter().map(|s| s.name).collect();
            let merged: HashSet<&str> = merged_names.iter().map(|s| s.as_str()).collect();
            let unknown: Vec<String> = body
                .servers
                .iter()
                .filter(|name| !merged.contains(name.as_str()))
                .cloned()
                .collect();
            if !unknown.is_empty() {
                return Err(AgentToolsError::UnknownServers(unknown));
            }
        }

        // 3. Atomic read-modify-write: replace active_names with the
        //    user's full selection. Catalog + local are preserved on
        //    disk (write-back via save_agent_mcp_config).
        cfg.active_names = Some(body.servers.clone());
        agent_config::save_agent_mcp_config(&self.work_dir, &cfg)
            .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            active_count = body.servers.len(),
            "RuntimeAgentToolsService::put_mcp_servers: active_names persisted"
        );

        Ok(self.mcp_servers_snapshot(agent_id))
    }

    async fn get_builtin_tools(&self, agent_id: &str) -> BuiltinToolsResponse {
        let tools = agent_config::load_agent_tools_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|c| c.tools)
            .unwrap_or_default();
        BuiltinToolsResponse {
            agent_id: agent_id.to_string(),
            tools,
        }
    }

    async fn put_builtin_tools(
        &self,
        agent_id: &str,
        body: PutBuiltinToolsBody,
    ) -> Result<BuiltinToolsResponse, AgentToolsError> {
        // Read-modify-write cycle (matches the previous handler logic
        // and the MQTT `RuntimeConfigUpdate` path in `cli.rs`):
        //
        //   1. Load the current `agent_tools.json` (or default empty if
        //      no file yet — first-run case after a fresh agent install).
        //   2. Build a patch by iterating the **current** entries and
        //      setting each entry's `enabled` based on whether its name
        //      appears in `body.builtin_tools`.  This iteration is the
        //      critical detail that prevents the "every unchecked
        //      checkbox silently re-enables on next PUT" bug — without
        //      it, `apply_builtin_tools_patch` would only flip tools
        //      named in the patch and leave everything else alone.
        //   3. Apply the patch (which force-enables PLATFORM_TOOLS and
        //      silently ignores names not in current).
        //   4. Persist via `save_agent_tools_config` (atomic
        //      write-tmp-rename).
        let current = agent_config::load_agent_tools_config(&self.work_dir)
            .map_err(AgentToolsError::Persistence)?
            .map(|c| c.tools)
            .unwrap_or_default();
        let patch: Vec<agent_config::AgentToolEntry> = current
            .iter()
            .map(|entry| {
                let enabled = body.builtin_tools.iter().any(|n| n == &entry.name);
                agent_config::AgentToolEntry::new(&entry.name, enabled)
            })
            .collect();
        let updated = agent_config::apply_builtin_tools_patch(&current, &patch);
        agent_config::save_agent_tools_config(
            &self.work_dir,
            &agent_config::AgentToolsConfig {
                tools: updated.clone(),
            },
        )
        .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            enabled_count = updated.iter().filter(|e| e.enabled).count(),
            total = updated.len(),
            "RuntimeAgentToolsService::put_builtin_tools: enabled flags persisted"
        );

        Ok(BuiltinToolsResponse {
            agent_id: agent_id.to_string(),
            tools: updated,
        })
    }

    async fn get_merged_tools(&self, agent_id: &str) -> MergedToolsResponse {
        let tools = agent_config::load_agent_tools_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|t| t.tools)
            .unwrap_or_default();

        let mcp_cfg = agent_config::load_agent_mcp_config(&self.work_dir)
            .ok()
            .flatten();

        let mcp_servers = mcp_cfg
            .as_ref()
            .map(|m| m.active_server_names())
            .unwrap_or_default();

        // Full per-agent merged list (catalog ∪ local) with active flags.
        // This powers the per-agent switches in the Desktop Tools panel so
        // agent-installed (local) and system-injected (pm) servers appear
        // there too — not just gateway-global catalog entries.
        let active_set: std::collections::HashSet<&str> =
            mcp_servers.iter().map(String::as_str).collect();
        let mcp_servers_defs = mcp_cfg
            .as_ref()
            .map(|m| {
                m.merged()
                    .into_iter()
                    .map(|s| crate::usecases::agent_tools::McpServerView {
                        name: s.name.clone(),
                        transport: s.transport,
                        url: s.url.clone(),
                        command: s.command.clone(),
                        args: s.args.clone(),
                        active: active_set.contains(s.name.as_str()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        // ADR-104: reuse the exact derivation the `/mcp-servers` route uses,
        // so the recommendation group cannot disagree between the two routes.
        let recommended = match mcp_cfg.as_ref() {
            Some(cfg) => {
                let installed: std::collections::HashSet<String> =
                    cfg.merged().into_iter().map(|s| s.name).collect();
                let active: std::collections::HashSet<String> =
                    mcp_servers.iter().cloned().collect();
                crate::usecases::mcp_recommendation::build_recommendation_views(
                    cfg, &installed, &active,
                )
            }
            None => Vec::new(),
        };

        MergedToolsResponse {
            agent_id: agent_id.to_string(),
            tools,
            mcp_servers,
            mcp_servers_defs,
            recommended,
        }
    }

    async fn get_mcp_tools(&self, agent_id: &str) -> McpToolsResponse {
        let servers = agent_config::load_agent_mcp_tools_config(&self.work_dir)
            .ok()
            .flatten()
            .map(|cfg| cfg.servers)
            .unwrap_or_default();
        McpToolsResponse {
            agent_id: agent_id.to_string(),
            servers,
        }
    }

    async fn put_mcp_tools(
        &self,
        agent_id: &str,
        body: PutMcpToolsBody,
    ) -> Result<McpToolsResponse, AgentToolsError> {
        // The desktop PUTs the entire per-server tool list (same shape
        // as GET). There is no server-side merge — the desktop's list
        // is authoritative. Write verbatim to agent_mcp_tools.json.
        agent_config::save_agent_mcp_tools_config(&self.work_dir, &body)
            .map_err(AgentToolsError::Persistence)?;

        tracing::info!(
            agent_id,
            server_count = body.servers.len(),
            tool_count = body.servers.values().map(|v| v.len()).sum::<usize>(),
            "RuntimeAgentToolsService::put_mcp_tools: persisted full tool list"
        );

        Ok(McpToolsResponse {
            agent_id: agent_id.to_string(),
            servers: body.servers,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usecases::agent_tools::McpRecommendationState;
    use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};

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

    fn manifest(toml_body: &str) -> acowork_core::manifest::AgentManifest {
        acowork_core::manifest::AgentManifest::from_toml(&format!(
            r#"
agent_id = "com.test.svc"
version = "1.0.0"
name = "Svc"
description = "service test"
author = "test"
runtime_version = "0.1.0"
{toml_body}
"#
        ))
        .expect("manifest should parse")
    }

    /// Seed `agent_mcp.json` the way startup does: project the manifest,
    /// then install one catalog entry.
    fn seeded_service(dir: &std::path::Path) -> RuntimeAgentToolsService {
        let m = manifest(
            r#"[[mcp_servers]]
preset = "brave-search"
tools = ["brave_web_search"]
why = "web research"

[[mcp_servers]]
hosted = "pm""#,
        );
        agent_config::project_recommended_mcp_servers(dir, &m).unwrap();
        let mut cfg = agent_config::load_agent_mcp_config(dir).unwrap().unwrap();
        cfg.catalog.push(mcp_def("brave-search"));
        agent_config::save_agent_mcp_config(dir, &cfg).unwrap();
        RuntimeAgentToolsService::new(dir.to_path_buf())
    }

    #[tokio::test]
    async fn get_mcp_servers_reports_the_five_state_machine() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());

        let resp = svc.get_mcp_servers("agent-1").await;
        assert_eq!(resp.recommended.len(), 2);

        let brave = &resp.recommended[0];
        assert_eq!(brave.key, "brave-search");
        assert_eq!(brave.source, "preset");
        assert_eq!(brave.tools, vec!["brave_web_search".to_string()]);
        assert_eq!(brave.why.as_deref(), Some("web research"));
        assert_eq!(
            brave.state,
            McpRecommendationState::InstalledInactive,
            "installed in the catalog but not ticked for this agent"
        );

        let pm = &resp.recommended[1];
        assert_eq!(pm.key, "pm");
        assert_eq!(pm.source, "hosted");
        assert_eq!(
            pm.state,
            McpRecommendationState::NotInstalled,
            "a hosted server is only 'not installed' while it is missing from the merged list"
        );
        assert!(resp.active_servers.is_empty());
    }

    #[tokio::test]
    async fn merged_tools_carries_the_same_recommendation_derivation() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());

        let merged = svc.get_merged_tools("agent-1").await;
        let dedicated = svc.get_mcp_servers("agent-1").await;

        assert_eq!(
            merged.recommended.len(),
            dedicated.recommended.len(),
            "the two routes must agree on the row set"
        );
        for (a, b) in merged.recommended.iter().zip(dedicated.recommended.iter()) {
            assert_eq!(a.key, b.key);
            assert_eq!(a.state, b.state);
        }
        // Sanity: the seeded agent really does declare both rows.
        assert_eq!(merged.recommended.len(), 2);
    }

    #[tokio::test]
    async fn activating_a_recommended_server_flips_it_to_active() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());

        svc.put_mcp_servers(
            "agent-1",
            PutMcpServersBody {
                servers: vec!["brave-search".into()],
            },
        )
        .await
        .unwrap();

        let resp = svc.get_mcp_servers("agent-1").await;
        assert_eq!(resp.active_servers, vec!["brave-search".to_string()]);
        assert_eq!(
            resp.recommended[0].state,
            McpRecommendationState::Active
        );
    }

    #[tokio::test]
    async fn dismiss_and_restore_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());

        let resp = svc
            .put_mcp_recommendation(
                "agent-1",
                "brave-search",
                PutMcpRecommendationBody { dismissed: true },
            )
            .await
            .unwrap();
        assert_eq!(resp.recommended[0].state, McpRecommendationState::Dismissed);

        // Survives a restart: the flag is on disk, not in memory.
        let on_disk = agent_config::load_agent_mcp_config(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(on_disk.dismissed, vec!["brave-search".to_string()]);
        // The declaration itself is untouched — it is package-owned.
        assert_eq!(on_disk.recommended.len(), 2);

        let resp = svc
            .put_mcp_recommendation(
                "agent-1",
                "brave-search",
                PutMcpRecommendationBody { dismissed: false },
            )
            .await
            .unwrap();
        assert_eq!(
            resp.recommended[0].state,
            McpRecommendationState::InstalledInactive
        );
        let on_disk = agent_config::load_agent_mcp_config(dir.path())
            .unwrap()
            .unwrap();
        assert!(on_disk.dismissed.is_empty());
    }

    #[tokio::test]
    async fn dismissing_twice_does_not_duplicate_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());
        for _ in 0..2 {
            svc.put_mcp_recommendation(
                "agent-1",
                "brave-search",
                PutMcpRecommendationBody { dismissed: true },
            )
            .await
            .unwrap();
        }
        let on_disk = agent_config::load_agent_mcp_config(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(on_disk.dismissed, vec!["brave-search".to_string()]);
    }

    #[tokio::test]
    async fn unknown_recommendation_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());
        let err = svc
            .put_mcp_recommendation(
                "agent-1",
                "ghost",
                PutMcpRecommendationBody { dismissed: true },
            )
            .await
            .expect_err("a stale key must not create a phantom dismissal");
        assert!(matches!(
            err,
            AgentToolsError::UnknownRecommendation(ref k) if k == "ghost"
        ));
        let on_disk = agent_config::load_agent_mcp_config(dir.path())
            .unwrap()
            .unwrap();
        assert!(on_disk.dismissed.is_empty());
    }

    /// The Tools panel reads `recommended` off the `PUT /mcp-servers`
    /// acknowledgement too, so one user action does not need a follow-up GET.
    #[tokio::test]
    async fn put_mcp_servers_returns_recommendations_as_well() {
        let dir = tempfile::tempdir().unwrap();
        let svc = seeded_service(dir.path());
        let resp = svc
            .put_mcp_servers(
                "agent-1",
                PutMcpServersBody {
                    servers: vec![],
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.recommended.len(), 2);
        assert!(resp.active_servers.is_empty());
    }
}