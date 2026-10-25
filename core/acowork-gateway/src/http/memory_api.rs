//! Memory management HTTP API handlers
//!
//! ADR-101: the memory endpoints are reverse-proxied to the standalone
//! `acowork-memory` service by [`crate::http::memory_proxy`]. This module
//! therefore exposes no HTTP routes of its own — registering a path here would
//! collide with that proxy and make `Router::merge()` panic at gateway startup
//! (e.g. "Overlapping method route" on `/api/agents/{id}/memory/nodes`).
//!
//! Before ADR-101 the same paths were proxied to the Runtime by
//! `crate::http::proxy::proxy_routes()`; the public contract did not change
//! with that move.
//!
//! The request/response types below remain here as the canonical contract
//! for the gateway↔desktop Memory API and are reused by tests and the
//! reverse-proxy layer when parsing payloads.

use axum::Router;
use serde::{Deserialize, Serialize};

use crate::http::routes::AppState;

/// Build the memory management router.
///
/// Per ADR-101 this router is intentionally empty: every memory path is owned
/// by [`crate::http::memory_proxy`] and forwarded to the memory service. Keeping
/// the function (rather than deleting it) preserves the `merge(...)` call in
/// `routes::build_router` and this module's contract types in one place.
pub fn memory_routes() -> Router<AppState> {
    Router::new()
}

// ── Query parameters ──────────────────────────────────────────────────

/// Query parameters for listing memory nodes
#[derive(Debug, Deserialize)]
pub struct MemoryNodesQuery {
    /// Page number (1-based, default: 1)
    pub page: Option<u32>,
    /// Page size (default: 20, max: 100)
    pub size: Option<u32>,
    /// Filter by node type: Knowledge, Episodic, Procedural, Autobiographical
    pub r#type: Option<String>,
    /// Sub-classification filter:
    /// - `Knowledge`: `Fact` | `Preference` | `Relation` | `Procedure`
    /// - `Autobiographical`: `Identity` | `Capability` | `Limitation`
    ///   | `Preference` | `History` | `Relationship`
    ///
    /// Ignored for `Episodic` / `Procedural` (no sub-classification).
    #[serde(default)]
    pub sub_type: Option<String>,
    /// Keyword search in node content
    pub keyword: Option<String>,
    /// Time range filter: 1h, 1d, 7d, 30d, all
    pub time_range: Option<String>,
}

impl MemoryNodesQuery {
    /// Get the effective page number (1-based)
    pub fn effective_page(&self) -> u32 {
        self.page.unwrap_or(1).max(1)
    }

    /// Get the effective page size (capped at 100)
    pub fn effective_size(&self) -> u32 {
        self.size.unwrap_or(20).clamp(1, 100)
    }
}

// ── Response types ────────────────────────────────────────────────────

/// A single memory node in the list response
#[derive(Serialize)]
pub struct MemoryNodeResponse {
    pub node_id: u64,
    pub node_type: String,
    /// Secondary classification inside the storage layer (Knowledge sub_type
    /// or Autobiographical category). `None` for Episodic/Procedural.
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

/// Paginated list of memory nodes
#[derive(Serialize)]
pub struct MemoryNodesListResponse {
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub nodes: Vec<MemoryNodeResponse>,
}

/// Memory statistics summary
#[derive(Serialize)]
pub struct MemoryStatsResponse {
    pub total_nodes: u64,
    pub storage_bytes: u64,
    pub by_type: std::collections::HashMap<String, u64>,
    pub by_status: std::collections::HashMap<String, u64>,
    pub index_health: String,
    /// Embedding dimension of the Grafeo HNSW vector index actually persisted on disk.
    /// 0 if the store has not yet built a vector index.
    pub stored_dim: u64,
    /// Number of memory nodes (across all labels) that currently have a
    /// non-NULL `embedding` field and therefore participate in vector search.
    /// Compare against `total_nodes` to detect missing embeddings.
    pub nodes_with_embedding: u64,
    /// Embedding dimension of the active embedding provider (model output).
    /// 0 if no embedding provider is currently configured.
    /// The desktop uses (stored_dim vs model_dim) to detect a dimension
    /// mismatch and offer a one-click "Rebuild Index" action.
    pub model_dim: u64,
    /// On-disk schema version of the underlying store (`PRAGMA user_version`
    /// for SQLite; 0 when the engine has no equivalent knob). Mirrored from
    /// `acowork-runtime::usecases::memory_query::MemoryStats`.
    pub schema_version: u64,
}

/// Response for deleting a memory node
#[derive(Serialize)]
pub struct DeleteNodeResponse {
    pub node_id: u64,
    pub deleted: bool,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_nodes_query_defaults() {
        let query = MemoryNodesQuery {
            page: None,
            size: None,
            r#type: None,
            sub_type: None,
            keyword: None,
            time_range: None,
        };
        assert_eq!(query.effective_page(), 1);
        assert_eq!(query.effective_size(), 20);
    }

    #[test]
    fn test_memory_nodes_query_capped() {
        let query = MemoryNodesQuery {
            page: Some(0),
            size: Some(200),
            r#type: None,
            sub_type: None,
            keyword: None,
            time_range: None,
        };
        assert_eq!(query.effective_page(), 1); // 0 -> 1
        assert_eq!(query.effective_size(), 100); // capped at 100
    }

    #[test]
    fn test_memory_nodes_query_sub_type_roundtrip() {
        // The query struct must deserialise the `sub_type` field for the
        // Autobiographical sub-filter that the Desktop panel sends. URL
        // deserialisation itself is exercised by axum's `Query` extractor
        // in the integration suite — here we just assert the field is
        // wired correctly through serde_json so a future rename does
        // not silently drop the value.
        let q: MemoryNodesQuery = serde_json::from_value(serde_json::json!({
            "type": "Autobiographical",
            "sub_type": "Limitation",
            "page": 1,
            "size": 20,
        }))
        .expect("memory nodes query should deserialise");
        assert_eq!(q.r#type.as_deref(), Some("Autobiographical"));
        assert_eq!(q.sub_type.as_deref(), Some("Limitation"));
        assert_eq!(q.effective_page(), 1);
        assert_eq!(q.effective_size(), 20);
    }

    #[test]
    fn test_delete_node_response_serialization() {
        let resp = DeleteNodeResponse {
            node_id: 42,
            deleted: true,
            message: "Deleted".to_string(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"node_id\":42"));
        assert!(json.contains("\"deleted\":true"));
    }

    #[test]
    fn test_memory_stats_response_serialization() {
        let resp = MemoryStatsResponse {
            total_nodes: 100,
            storage_bytes: 4096,
            by_type: std::collections::HashMap::new(),
            by_status: std::collections::HashMap::new(),
            index_health: "healthy".to_string(),
            stored_dim: 512,
            nodes_with_embedding: 100,
            model_dim: 512,
            schema_version: 7,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"total_nodes\":100"));
        assert!(json.contains("\"healthy\""));
        // Index-health fields are serialized as snake_case to match the
        // TypeScript types in apps/acowork-desktop/src/lib/types.ts.
        assert!(json.contains("\"stored_dim\":512"));
        assert!(json.contains("\"nodes_with_embedding\":100"));
        assert!(json.contains("\"model_dim\":512"));
        assert!(json.contains("\"schema_version\":7"));
    }
}

