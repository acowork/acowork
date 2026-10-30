//! Manifest hand-off to the memory service (ADR-101 P3).
//!
//! The Runtime no longer owns a memory store, so it cannot write Identity /
//! Capability nodes itself. What it still owns is the **manifest**, the
//! authoritative static self-knowledge of the agent, so on startup it forwards
//! that knowledge to the memory service and lets the service decide
//! idempotency (see `acowork-memory`'s `POST /admin/bootstrap`).
//!
//! ## Why this is a thin coupling, not memory logic
//!
//! The only thing the Runtime looks at is the MCP catalog: when it contains the
//! Gateway-hosted `memory` server, the endpoint derived from that entry is
//! called once per start; when it does not (memory disabled, or the user runs a
//! third-party memory MCP), nothing here runs. The request body is a projection
//! of the manifest — no node is constructed, no store is touched, and the
//! response only feeds a log line.
//!
//! ## Package policy travels with the bootstrap
//!
//! The manifest's `[memory]` section is the package author's initial policy.
//! It rides in the same POST because the service applies it **only when the
//! instance is bootstrapped for the first time** — the panel's saved choices
//! live in the service (`consolidation.json`) and must never be overwritten by
//! a restart.

use acowork_core::manifest::AgentManifest;
use acowork_core::protocol::{McpServerConfigDef, McpTransportDef};
use serde_json::{json, Map, Value};

/// Catalog id of the Gateway-hosted memory MCP server.
///
/// The coupling is keyed on this string alone (ADR-101 decision 12): a
/// third-party memory MCP is a different server and is deliberately not
/// inspected.
const MEMORY_MCP_NAME: &str = "memory";

/// What one hand-off attempt achieved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootstrapOutcome {
    /// Identity nodes the service wrote (agent_id / name / description / …).
    pub identity_written: usize,
    /// Capability nodes the service wrote.
    pub capability_written: usize,
    /// True when the service skipped the import because Identity nodes
    /// already exist (idempotency).
    pub skipped_existing: bool,
    /// True when the request reached the service and answered 2xx.
    ///
    /// `false` is not an agent-start failure: the panel simply shows no
    /// bootstrap facts until the next start.
    pub delivered: bool,
}

/// Derive the service base URL (`…/api/memory`) from the agent's MCP catalog.
///
/// The catalog entry points at the service's MCP surface
/// (`…/api/memory/mcp`), while the admin route sits beside it. Deriving the
/// base by removing the `/mcp` suffix — rather than string-replacing the
/// host — keeps the Gateway prefix intact and works unchanged behind a relay.
///
/// `None` means "no official memory service in this agent's catalog" and the
/// caller does nothing.
pub fn service_base(catalog: &[McpServerConfigDef]) -> Option<String> {
    let entry = catalog
        .iter()
        .find(|c| c.name == MEMORY_MCP_NAME && c.transport == McpTransportDef::Http)?;
    let url = entry.url.as_deref()?.trim_end_matches('/');
    url.strip_suffix("/mcp")
        .map(|base| base.trim_end_matches('/').to_string())
        .filter(|base| !base.is_empty())
}

/// `POST {base}/admin/bootstrap?instance_id=…` — best-effort import.
///
/// A failure is logged and reported through [`BootstrapOutcome::delivered`];
/// it never aborts agent startup. The service answers the counters, so the
/// outcome reflects what was actually written rather than what was sent.
pub async fn import_manifest(
    client: &reqwest::Client,
    base: &str,
    instance_id: &str,
    node_token: &str,
    manifest: &AgentManifest,
) -> BootstrapOutcome {
    let url = format!("{base}/admin/bootstrap");
    let mut req = client
        .post(&url)
        .query(&[("instance_id", instance_id)])
        .json(&body_from_manifest(manifest));
    if !node_token.trim().is_empty() {
        // ADR-076: the admin route sits behind the Gateway's auth middleware;
        // the node credential is what lets a machine actor through.
        req = req.header(acowork_core::auth::NODE_TOKEN_HEADER, node_token);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "memory bootstrap request failed");
            return BootstrapOutcome::default();
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(%status, body = %body, "memory bootstrap rejected");
        return BootstrapOutcome::default();
    }
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let outcome = BootstrapOutcome {
        identity_written: body["identity_written"].as_u64().unwrap_or(0) as usize,
        capability_written: body["capability_written"].as_u64().unwrap_or(0) as usize,
        skipped_existing: body["skipped_existing"].as_bool().unwrap_or(false),
        delivered: true,
    };
    tracing::info!(
        identity_count = outcome.identity_written,
        capability_count = outcome.capability_written,
        skipped_existing = outcome.skipped_existing,
        "memory bootstrap hand-off finished"
    );
    outcome
}

/// Project the manifest into the service's bootstrap body.
///
/// Deliberately narrower than `AgentManifest`: the service must not grow a
/// dependency on packaging surface it does not read. Blank values are dropped
/// — an empty statement is not a memory.
fn body_from_manifest(manifest: &AgentManifest) -> Value {
    let mut capabilities = Map::new();
    for (key, cap) in &manifest.capabilities {
        capabilities.insert(
            key.clone(),
            json!({ "description": cap.description.clone() }),
        );
    }
    let mut body = Map::new();
    body.insert("agent_id".into(), json!(manifest.agent_id));
    body.insert("name".into(), json!(manifest.name));
    body.insert("description".into(), json!(manifest.description));
    if let Some(ref dn) = manifest.display_name {
        body.insert("display_name".into(), json!(dn));
    }
    if let Some(ref role) = manifest.role {
        body.insert("role".into(), json!(role));
    }
    body.insert("capabilities".into(), Value::Object(capabilities));
    if let Some(policy) = memory_policy(&manifest.memory) {
        body.insert("memory".into(), policy);
    }
    Value::Object(body)
}

/// The manifest `[memory.distiller]` section as the service's patch shape.
///
/// Only keys the service accepts are emitted (`PUT /memory/consolidation/status`
/// rejects unknown fields); minute-valued manifest knobs become seconds.
fn memory_policy(memory: &acowork_core::manifest::MemoryConfig) -> Option<Value> {
    let distiller_cfg = memory.distiller.as_ref()?;
    let mut distiller = Map::new();
    distiller.insert("enabled".into(), json!(distiller_cfg.enabled));
    if let Some(minutes) = distiller_cfg.interval_minutes {
        distiller.insert("interval_secs".into(), json!(minutes.saturating_mul(60)));
    }
    if let Some(threshold) = distiller_cfg.accumulation_threshold {
        distiller.insert("accumulation_threshold".into(), json!(threshold));
    }
    if let Some(minutes) = distiller_cfg.idle_minutes {
        distiller.insert("idle_secs".into(), json!(minutes.saturating_mul(60)));
    }
    if let Some(batch) = distiller_cfg.batch_size {
        distiller.insert("batch_size".into(), json!(batch));
    }
    if let Some(model_id) = distiller_cfg.model_id.as_deref().filter(|m| !m.trim().is_empty()) {
        distiller.insert(
            "models".into(),
            json!([{
                "provider_id": distiller_cfg.model_provider_id.clone().unwrap_or_default(),
                "model_id": model_id,
            }]),
        );
    }
    Some(json!({ "distiller": distiller }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_def(url: &str) -> McpServerConfigDef {
        McpServerConfigDef {
            name: MEMORY_MCP_NAME.to_string(),
            transport: McpTransportDef::Http,
            url: Some(url.to_string()),
            ..Default::default()
        }
    }

    /// Minimal manifest built through the production parser —
    /// `AgentManifest` has no `Default` impl, and the optional sections
    /// all fall back to their serde defaults.
    fn test_manifest() -> AgentManifest {
        AgentManifest::from_toml(
            r#"
            agent_id = "com.acowork.test"
            version = "1.0.0"
            name = "Test Agent"
            description = "desc"
            author = "acowork"
            runtime_version = "1.0.0"
            "#,
        )
        .expect("synthetic manifest parses")
    }

    #[test]
    fn service_base_is_derived_from_the_mcp_entry() {
        let catalog = vec![memory_def("http://gw:19876/api/memory/mcp")];
        assert_eq!(
            service_base(&catalog).as_deref(),
            Some("http://gw:19876/api/memory")
        );
    }

    #[test]
    fn service_base_tolerates_a_trailing_slash() {
        let catalog = vec![memory_def("http://gw:19876/api/memory/mcp/")];
        assert_eq!(
            service_base(&catalog).as_deref(),
            Some("http://gw:19876/api/memory")
        );
    }

    #[test]
    fn no_official_memory_entry_means_no_coupling() {
        // A third-party server that happens to be called "memory" is not the
        // official service; stdio and non-`/mcp` endpoints are skipped too.
        let mut stdio = memory_def("http://gw/api/memory/mcp");
        stdio.transport = McpTransportDef::Stdio;
        let catalog = vec![
            stdio,
            memory_def("http://gw/api/memory/other"),
            McpServerConfigDef {
                name: "pm".into(),
                transport: McpTransportDef::Http,
                url: Some("http://gw/api/pm/mcp".into()),
                ..Default::default()
            },
        ];
        assert_eq!(service_base(&catalog), None);
    }

    #[test]
    fn body_carries_identity_capabilities_and_policy() {
        let mut manifest = test_manifest();
        manifest.display_name = Some("Tester".into());
        manifest.capabilities.insert(
            "search".into(),
            acowork_core::manifest::CapabilityDef {
                description: "searches things".into(),
                skills: Vec::new(),
                input_schema: None,
                output_schema: None,
            },
        );
        manifest.memory.distiller = Some(acowork_core::manifest::ManifestDistillerConfig {
            enabled: true,
            model_provider_id: Some("p1".into()),
            model_id: Some("m1".into()),
            interval_minutes: Some(30),
            accumulation_threshold: Some(5),
            idle_minutes: Some(10),
            batch_size: Some(7),
            min_importance: None,
        });

        let body = body_from_manifest(&manifest);
        assert_eq!(body["agent_id"], "com.acowork.test");
        assert_eq!(body["description"], "desc");
        assert_eq!(body["display_name"], "Tester");
        assert_eq!(body["capabilities"]["search"]["description"], "searches things");
        assert_eq!(body["memory"]["distiller"]["enabled"], true);
        // Minutes are converted to the service's seconds-based patch.
        assert_eq!(body["memory"]["distiller"]["interval_secs"], 1800);
        assert_eq!(body["memory"]["distiller"]["idle_secs"], 600);
        assert_eq!(body["memory"]["distiller"]["batch_size"], 7);
        assert_eq!(body["memory"]["distiller"]["models"][0]["model_id"], "m1");
        // No forgetting section in the manifest — the service keeps its own.
        assert!(body["memory"].get("forgetting").is_none());
    }

    #[test]
    fn body_omits_absent_optional_fields() {
        let manifest = test_manifest();
        let body = body_from_manifest(&manifest);
        assert!(body.get("display_name").is_none());
        assert!(body.get("role").is_none());
        assert!(body.get("memory").is_none());
    }
}

