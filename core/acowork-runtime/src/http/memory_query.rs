//! Shared memory query business logic.
//!
//! ADR-051 P4: All business logic (pagination, filtering, keyword search,
//! stats aggregation, consolidation trigger) has been moved to the
//! `MemoryAdminService` trait implementation in the storage backend
//! (`acowork-sqlite`). This
//! module now provides thin wrappers that call the trait and convert
//! results to the intermediate types consumed by the HTTP handlers and
//! the `MemoryAdminAdapter`.
//!
//! All entry points take `Option<&Arc<dyn MemoryAdminService>>` and report
//! graceful "no store" responses when the store has not been
//! initialised yet.

use std::collections::HashMap;
use std::sync::Arc;

use acowork_memory::admin::{
    AdminListNodesOutput, AdminListNodesParams, AdminNodeDetail, AdminStats, MemoryAdminService,
};
use serde::Serialize;

use crate::usecases::memory_query::MemoryStats;

// ── Intermediate types (consumed by HTTP handlers) ────────────────────

/// One row in a memory list response.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct MemoryNodeRecord {
    pub node_id: u64,
    pub node_type: String,
    /// Secondary classification inside the storage layer (Knowledge
    /// sub_type or Autobiographical category). `None` for Episodic/Procedural.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<String>,
    pub content: String,
    /// Raw `confidence` property on the node (0.0 when the node type has no
    /// such property — e.g. Episodic). Passed through verbatim, never derived.
    pub confidence: f64,
    /// Raw `importance` property on the node (0.0 when the node type has no
    /// such property — e.g. Procedural/Autobiographical). Passed through
    /// verbatim, never derived.
    pub importance: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
}

/// Result of a list-nodes query.
#[derive(Debug, Clone)]
pub(crate) struct ListNodesOutput {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub nodes: Vec<MemoryNodeRecord>,
    #[allow(dead_code)]
    pub rejected_unfiltered: Option<u64>,
}

/// Result of a stats query.
pub(crate) type StatsOutput = MemoryStats;

/// Result of a single-node GET query.
#[derive(Debug, Clone)]
pub(crate) struct GetNodeOutput {
    pub node_id: u64,
    pub found: bool,
    pub node_type: String,
    pub sub_type: Option<String>,
    pub content: String,
    /// Raw `confidence` property on the node (0.0 when the node type has no
    /// such property — e.g. Episodic). Passed through verbatim, never derived.
    pub confidence: f64,
    /// Raw `importance` property on the node (0.0 when the node type has no
    /// such property — e.g. Procedural/Autobiographical). Passed through
    /// verbatim, never derived.
    pub importance: f64,
    pub decay_score: f64,
    pub created_at: i64,
    pub last_accessed_at: i64,
    pub access_count: u32,
    pub status: String,
    pub properties: HashMap<String, serde_json::Value>,
    pub message: String,
}

/// Parameters for [`list_nodes`].
#[derive(Debug, Clone, Default)]
pub(crate) struct ListNodesParams {
    pub page: u32,
    pub size: u32,
    pub node_type: String,
    /// Sub-classification filter (Fact / Preference / Relation / Procedure for
    /// Knowledge; Identity / Capability / Limitation / Preference / History /
    /// Relationship for Autobiographical). Empty string = no filter.
    pub sub_type: String,
    pub keyword: String,
    pub time_range: String,
}

// ── Conversion helpers ───────────────────────────────────────────────

/// Convert a [`GetNodeOutput`] to JSON.
pub(crate) fn get_output_to_json(out: &GetNodeOutput) -> serde_json::Value {
    let mut obj = serde_json::json!({
        "node_id": out.node_id,
        "found": out.found,
        "node_type": out.node_type,
        "content": out.content,
        "confidence": out.confidence,
        "importance": out.importance,
        "decay_score": out.decay_score,
        "created_at": out.created_at,
        "last_accessed_at": out.last_accessed_at,
        "access_count": out.access_count,
        "status": out.status,
        "properties": out.properties,
        "message": out.message,
    });
    if let Some(ref st) = out.sub_type {
        obj["sub_type"] = serde_json::Value::String(st.clone());
    }
    obj
}

// ── Empty-result helpers ─────────────────────────────────────────────

fn empty_list_output(page: u32, size: u32) -> ListNodesOutput {
    ListNodesOutput {
        total: 0,
        page,
        size,
        nodes: vec![],
        rejected_unfiltered: None,
    }
}

fn empty_stats_output(embed_provider_dim: u64) -> StatsOutput {
    MemoryStats {
        total_nodes: 0,
        storage_bytes: 0,
        by_type: HashMap::new(),
        by_status: HashMap::new(),
        index_health: "no_store".to_string(),
        stored_dim: 0,
        nodes_with_embedding: 0,
        vectors_of_other_dim: 0,
        rebuild_rows_all: 0,
        rebuild_rows_repair: 0,
        model_dim: embed_provider_dim,
        // No store → no schema version either. 0 is the implicit
        // `user_version` value, mirroring SQLite's own default.
        schema_version: 0,
    }
}

// ── Thin wrappers over `dyn MemoryAdminService` ──────────────────────

/// List memory nodes with pagination, filtering, and search.
pub(crate) fn list_nodes(
    admin: Option<&Arc<dyn MemoryAdminService>>,
    params: ListNodesParams,
) -> ListNodesOutput {
    let svc = match admin {
        Some(s) => s,
        None => {
            tracing::warn!("memory list: no memory admin service available");
            return empty_list_output(params.page, params.size);
        }
    };
    let admin_params = AdminListNodesParams {
        page: params.page,
        size: params.size,
        node_type: params.node_type,
        sub_type: params.sub_type,
        keyword: params.keyword,
        time_range: params.time_range,
    };
    let out: AdminListNodesOutput = svc.list_nodes(&admin_params);
    ListNodesOutput {
        total: out.total,
        page: out.page,
        size: out.size,
        nodes: out
            .nodes
            .into_iter()
            .map(|n| MemoryNodeRecord {
                node_id: n.node_id,
                node_type: n.node_type,
                sub_type: n.sub_type,
                content: n.content,
                confidence: n.confidence,
                importance: n.importance,
                decay_score: n.decay_score,
                created_at: n.created_at,
                last_accessed_at: n.last_accessed_at,
                access_count: n.access_count,
                status: n.status,
            })
            .collect(),
        rejected_unfiltered: out.rejected_unfiltered,
    }
}

/// Semantic search over memory nodes (ADR-081 P1-1).
///
/// Thin wrapper over `MemoryAdminService::semantic_search`, mirroring
/// [`list_nodes`]'s record mapping. `embedding` is `None` when the agent
/// has no live embedding provider — the engine then falls back to BM25.
pub(crate) fn semantic_search(
    admin: Option<&Arc<dyn MemoryAdminService>>,
    query_text: &str,
    embedding: Option<&[f32]>,
    mode: &str,
    limit: usize,
) -> ListNodesOutput {
    let svc = match admin {
        Some(s) => s,
        None => {
            tracing::warn!("memory semantic_search: no memory admin service available");
            return empty_list_output(1, limit.clamp(1, 100) as u32);
        }
    };
    let records = svc.semantic_search(query_text, embedding, mode, limit);
    ListNodesOutput {
        total: records.len() as u64,
        page: 1,
        size: limit.clamp(1, 100) as u32,
        nodes: records
            .into_iter()
            .map(|n| MemoryNodeRecord {
                node_id: n.node_id,
                node_type: n.node_type,
                sub_type: n.sub_type,
                content: n.content,
                confidence: n.confidence,
                importance: n.importance,
                decay_score: n.decay_score,
                created_at: n.created_at,
                last_accessed_at: n.last_accessed_at,
                access_count: n.access_count,
                status: n.status,
            })
            .collect(),
        rejected_unfiltered: None,
    }
}

/// Collect memory statistics, including vector-index diagnostics.
pub(crate) fn get_stats(
    admin: Option<&Arc<dyn MemoryAdminService>>,
    embed_provider_dim: u64,
) -> StatsOutput {
    let svc = match admin {
        Some(s) => s,
        None => return empty_stats_output(embed_provider_dim),
    };
    let stats: AdminStats = svc.get_stats();
    // Pre-count the rebuild scopes with the same predicates the scoped
    // migration selects with (see `MemoryAdminService::rebuild_scope_count`).
    // The repair count is measured against the width the rebuild will
    // actually target: the active model's dimension, falling back to the
    // store's own declared dimension when no model is bound.
    let rebuild_dim = if embed_provider_dim > 0 {
        embed_provider_dim
    } else {
        stats.stored_dim
    } as usize;
    MemoryStats {
        total_nodes: stats.total_nodes,
        storage_bytes: stats.storage_bytes,
        by_type: stats.by_type,
        by_status: stats.by_status,
        index_health: stats.index_health,
        stored_dim: stats.stored_dim,
        nodes_with_embedding: stats.nodes_with_embedding,
        vectors_of_other_dim: stats.vectors_of_other_dim,
        rebuild_rows_all: svc.rebuild_scope_count(acowork_memory::admin::RebuildScope::All, rebuild_dim),
        rebuild_rows_repair: svc.rebuild_scope_count(
            acowork_memory::admin::RebuildScope::Repair,
            rebuild_dim,
        ),
        model_dim: embed_provider_dim,
        schema_version: stats.schema_version,
    }
}

/// Look up a single memory node by numeric ID.
pub(crate) fn get_node(admin: Option<&Arc<dyn MemoryAdminService>>, node_id: u64) -> GetNodeOutput {
    let svc = match admin {
        Some(s) => s,
        None => {
            tracing::warn!("memory get_node: no memory admin service available");
            return GetNodeOutput {
                node_id,
                found: false,
                node_type: String::new(),
                sub_type: None,
                content: String::new(),
                confidence: 0.0,
                importance: 0.0,
                decay_score: 0.0,
                created_at: 0,
                last_accessed_at: 0,
                access_count: 0,
                status: String::new(),
                properties: HashMap::new(),
                message: "Memory store not available".to_string(),
            };
        }
    };
    let detail: AdminNodeDetail = svc.get_node(node_id);
    GetNodeOutput {
        node_id: detail.node_id,
        found: detail.found,
        node_type: detail.node_type,
        sub_type: detail.sub_type,
        content: detail.content,
        confidence: detail.confidence,
        importance: detail.importance,
        decay_score: detail.decay_score,
        created_at: detail.created_at,
        last_accessed_at: detail.last_accessed_at,
        access_count: detail.access_count,
        status: detail.status,
        properties: detail.properties,
        message: detail.message,
    }
}

/// Delete a memory node by ID. Returns true if the node was found and deleted.
pub(crate) fn delete_node(admin: Option<&Arc<dyn MemoryAdminService>>, node_id: u64) -> bool {
    let svc = match admin {
        Some(s) => s,
        None => return false,
    };
    svc.delete_node(node_id)
}

/// Create a new memory node with the given label and property map.
pub(crate) fn create_node(
    admin: Option<&Arc<dyn MemoryAdminService>>,
    label: &str,
    properties: &HashMap<String, serde_json::Value>,
) -> crate::error::Result<u64> {
    let svc = admin
        .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
    svc.create_node(label, properties)
        .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))
}

/// Update (merge) properties on an existing memory node.
pub(crate) fn update_node(
    admin: Option<&Arc<dyn MemoryAdminService>>,
    node_id: u64,
    properties: &HashMap<String, serde_json::Value>,
) -> crate::error::Result<()> {
    let svc = admin
        .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
    svc.update_node(node_id, properties)
        .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_nodes_returns_empty_when_no_store() {
        let out = list_nodes(None, ListNodesParams::default());
        assert_eq!(out.total, 0);
        assert_eq!(out.page, 0);
        assert_eq!(out.size, 0);
        assert!(out.nodes.is_empty());
    }

    #[test]
    fn get_stats_returns_zero_when_no_store() {
        let out = get_stats(None, 512);
        assert_eq!(out.total_nodes, 0);
        assert_eq!(out.storage_bytes, 0);
        assert!(out.by_type.is_empty());
        assert!(out.by_status.is_empty());
        assert_eq!(out.index_health, "no_store");
        assert_eq!(out.stored_dim, 0);
        assert_eq!(out.nodes_with_embedding, 0);
        assert_eq!(out.rebuild_rows_all, 0);
        assert_eq!(out.rebuild_rows_repair, 0);
        assert_eq!(out.model_dim, 512);
        assert_eq!(out.schema_version, 0);
    }

    #[test]
    fn get_stats_serializes_to_full_contract() {
        let out = get_stats(None, 384);
        let value = serde_json::to_value(&out).expect("serialize");
        let obj = value.as_object().expect("object");
        for key in [
            "total_nodes",
            "storage_bytes",
            "by_type",
            "by_status",
            "index_health",
            "stored_dim",
            "nodes_with_embedding",
            "rebuild_rows_all",
            "rebuild_rows_repair",
            "model_dim",
            "schema_version",
        ] {
            assert!(obj.contains_key(key), "stats JSON missing field: {}", key);
        }
    }

    #[test]
    fn delete_node_reports_unavailable_without_store() {
        assert!(!delete_node(None, 42));
    }

    #[test]
    fn get_node_reports_unavailable_without_store() {
        let out = get_node(None, 42);
        assert_eq!(out.node_id, 42);
        assert!(!out.found);
        assert_eq!(out.message, "Memory store not available");
        assert!(out.properties.is_empty());
    }

    #[test]
    fn create_node_reports_unavailable_without_store() {
        let props = HashMap::from([("name".to_string(), serde_json::json!("alpha"))]);
        let err = create_node(None, "Knowledge", &props).unwrap_err();
        assert!(
            err.to_string().contains("memory store unavailable"),
            "expected unavailable error, got: {}",
            err
        );
    }

    #[test]
    fn update_node_reports_unavailable_without_store() {
        let props = HashMap::from([("x".to_string(), serde_json::json!(1))]);
        let err = update_node(None, 1, &props).unwrap_err();
        assert!(err.to_string().contains("memory store unavailable"));
    }

    #[test]
    fn get_output_to_json_includes_found_flag_and_properties() {
        let out = GetNodeOutput {
            node_id: 7,
            found: true,
            node_type: "Episodic".to_string(),
            sub_type: None,
            content: "[user] hi".to_string(),
            confidence: 0.5,
            importance: 0.7,
            decay_score: 0.9,
            created_at: 100,
            last_accessed_at: 100,
            access_count: 0,
            status: "Active".to_string(),
            properties: HashMap::new(),
            message: "ok".to_string(),
        };
        let v = get_output_to_json(&out);
        assert_eq!(v["node_id"], 7);
        assert_eq!(v["found"], true);
        assert_eq!(v["node_type"], "Episodic");
        assert_eq!(v["message"], "ok");
        // Raw fields are passed through verbatim — no derivation.
        assert_eq!(v["confidence"], 0.5);
        assert_eq!(v["importance"], 0.7);
        // sub_type is omitted when None
        assert!(v.get("sub_type").is_none());
    }

    #[test]
    fn get_output_to_json_includes_sub_type_when_present() {
        let out = GetNodeOutput {
            node_id: 9,
            found: true,
            node_type: "Knowledge".to_string(),
            sub_type: Some("preference".to_string()),
            content: "likes dark mode".to_string(),
            confidence: 0.9,
            importance: 0.5,
            decay_score: 0.5,
            created_at: 100,
            last_accessed_at: 100,
            access_count: 0,
            status: "Active".to_string(),
            properties: HashMap::new(),
            message: "ok".to_string(),
        };
        let v = get_output_to_json(&out);
        assert_eq!(v["sub_type"], "preference");
        assert_eq!(v["confidence"], 0.9);
        assert_eq!(v["importance"], 0.5);
    }
}
