//! Memory panel wire types (ADR-100).
//!
//! These are the **single source of truth** for the memory-query wire format
//! (HTTP JSON shape documented in `docs/protocols/http.md` §7.7, plus the
//! canonical `MemoryStatsResponse` shape mirrored in
//! `acowork-gateway::http::memory_api`). They moved here from
//! `acowork-runtime::usecases::memory_query` when the memory capability became
//! a standalone service, because both halves speak them: the Runtime handlers
//! being deleted in P3 re-export them, and the service's REST/MCP surfaces
//! produce them.
//!
//! They are `Serialize`/`Deserialize` so handlers can ferry them through
//! `serde_json::to_value` directly — no separate JSON-shaping layer that could
//! drift from the public contract. **Field names are frozen**: the Desktop
//! memory panel and global search parse them.
//!
//! Only response/state shapes live here. Request-side parsing is owned by the
//! endpoint handlers (`service::api::*`), which mirror the Runtime's query
//! structs one-to-one; a second, unused copy of those here would be exactly the
//! drift this module is meant to prevent.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Lightweight memory node for list responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub node_id: u64,
    pub node_type: String,
    /// Secondary classification (see [`crate::AdminNodeRecord::sub_type`]).
    /// `None` for Episodic/Procedural nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<String>,
    pub content: String,
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

/// Response for listing memory nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNodeListResponse {
    pub nodes: Vec<MemoryNode>,
    pub total: u64,
    pub page: u32,
    pub size: u32,
    pub model_dim: u64,
}

/// Per-source retrieval signals for one hit.
///
/// Ranks are **in-source** (position inside each candidate pool), not
/// comparable across sources; `fused_score` is the in-source RRF signal and
/// must not be presented as a similarity.
///
/// Defined in `acowork-core` because the Runtime's conversation scope reports
/// the same shape; re-exported here so every memory-side producer and consumer
/// keeps importing it from the memory crate.
pub use acowork_core::retrieval::RetrievalEvidence;

/// Memory store statistics.
///
/// This struct is the single source of truth for the `GET /memory/stats` wire
/// format. The HTTP handler serializes it directly with
/// `serde_json::to_value`, so there is exactly one producer of the shape and
/// the panel can never see two variants of it. Field names + types mirror the
/// canonical `MemoryStatsResponse` defined in
/// `acowork-gateway::http::memory_api`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryStats {
    /// Total number of memory nodes across all labels.
    pub total_nodes: u64,
    /// On-disk storage size in bytes. Reserved for future use; currently 0.
    pub storage_bytes: u64,
    /// Node count grouped by memory label (`Knowledge` / `Episodic` / etc.).
    pub by_type: HashMap<String, u64>,
    /// Node count grouped by status (`Active` / `Dormant` / `purged`).
    /// The desktop Memory panel reads `by_status["Active"]` /
    /// `by_status["Dormant"]` to drive its status cards.
    pub by_status: HashMap<String, u64>,
    /// Vector-index health string (`healthy` / `no_store` / `error: …`).
    pub index_health: String,
    /// Embedding dimension of the persisted index. 0 if no index.
    /// Compared against `model_dim` by the desktop to detect dimension
    /// mismatches and surface the "Rebuild Index" banner.
    pub stored_dim: u64,
    /// Number of nodes with a non-NULL `embedding` field. Compared against
    /// `total_nodes` to detect missing embeddings.
    pub nodes_with_embedding: u64,
    /// Vectors stored at a width other than `stored_dim`, and therefore
    /// invisible to vector search. Mirrored from
    /// [`crate::AdminStats::vectors_of_other_dim`]; the desktop Memory panel
    /// shows its "Rebuild Index" banner on this being non-zero.
    pub vectors_of_other_dim: u64,
    /// Rows a rebuild with `scope = "all"` would re-embed (every row in the
    /// migration label set, conversation index included). The desktop panel
    /// shows this on the banner so the promised row count equals the
    /// progress bar's total; 0 on engines that cannot pre-count.
    pub rebuild_rows_all: u64,
    /// Rows a rebuild with `scope = "repair"` would re-embed against
    /// `model_dim` - vectors of a foreign width plus rows with no vector at
    /// all, over the migration label set. This is the exact number the
    /// repair rebuild's progress total will report, so the banner cannot
    /// promise one count and process another.
    pub rebuild_rows_repair: u64,
    /// Embedding dimension of the active embedding model (0 if none).
    pub model_dim: u64,
    /// On-disk schema version of the underlying store (`PRAGMA user_version`
    /// for SQLite; 0 when the engine has no equivalent knob).
    pub schema_version: u64,
}

/// Result of an embedding-dimension rebuild.
///
/// Mirrors [`crate::MemoryAdminService::migrate_embedding_dimension`]'s
/// [`crate::RebuildStats`] so the HTTP handler can ferry it through
/// `serde_json::to_value` without engine-specific types.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RebuildReport {
    /// Total nodes scanned.
    pub total_scanned: u64,
    /// Nodes successfully re-embedded.
    pub rebuilt: u64,
    /// Nodes skipped because they had no embedding vector.
    pub skipped_no_embedding: u64,
    /// Nodes skipped because they had no content.
    pub skipped_no_content: u64,
    /// Nodes that failed during re-embedding.
    pub errors: u64,
    /// Human-readable summary message for the UI.
    pub message: String,
}

/// Live progress of a background `POST /memory/rebuild-embeddings` run.
///
/// Serialized verbatim by `GET /memory/rebuild-progress` so the Gateway can
/// poll and forward `rebuilt/total` to the UI (the rebuild can take minutes,
/// which is why it is not a synchronous call). Entry-presence is the state:
/// an instance that has never run a rebuild has **no** progress to report and
/// the endpoint answers 404.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RebuildProgress {
    pub rebuilt: u64,
    pub total: u64,
    /// `true` once the background task has finished (success or error).
    pub done: bool,
    /// Set when the run fails; `None` while running or on success.
    pub error: Option<String>,
}

impl RebuildProgress {
    /// The reserved slot written before the background task starts, so the
    /// first poll already sees a live run instead of a 404.
    pub fn running() -> Self {
        Self {
            rebuilt: 0,
            total: 0,
            done: false,
            error: None,
        }
    }
}

#[cfg(test)]
mod retrieval_evidence_tests {
    use super::*;
    use acowork_core::retrieval::MatchedBy;

    fn evidence() -> RetrievalEvidence {
        RetrievalEvidence {
            matched_by: MatchedBy::Both,
            vector_rank: Some(1),
            text_rank: Some(0),
            cosine: Some(0.72),
            fused_score: 0.032,
            fused_rank: Some(0),
        }
    }

    /// The payload is locate keys *plus* evidence, never evidence *instead of*
    /// locate keys. Dropping `node_id` made every memory preview fetch node 0
    /// and spin forever, so this is a regression guard, not a shape preference.
    #[test]
    fn merge_into_adds_evidence_without_touching_locate_keys() {
        let mut payload = serde_json::Map::new();
        payload.insert("node_id".to_string(), serde_json::json!(42u64));
        payload.insert("node_type".to_string(), serde_json::json!("Knowledge"));
        payload.insert("sub_type".to_string(), serde_json::json!(null));

        evidence().merge_into(&mut payload);

        assert_eq!(payload.get("node_id").and_then(|v| v.as_u64()), Some(42));
        assert_eq!(
            payload.get("node_type").and_then(|v| v.as_str()),
            Some("Knowledge")
        );
        assert!(
            payload.contains_key("sub_type"),
            "sub_type must survive, even as null"
        );
        assert_eq!(
            payload.get("matched_by").and_then(|v| v.as_str()),
            Some("both")
        );
        assert_eq!(payload.get("fused_rank").and_then(|v| v.as_u64()), Some(0));
    }

    /// Cosine stays out of the merged payload for the same reason it stays out
    /// of `to_payload`: an uncalibrated similarity must not reach the UI.
    #[test]
    fn merge_into_does_not_expose_cosine() {
        let mut payload = serde_json::Map::new();
        payload.insert("node_id".to_string(), serde_json::json!(7u64));
        evidence().merge_into(&mut payload);
        assert!(
            !payload.contains_key("cosine"),
            "cosine must not be published to the list payload"
        );
    }

    /// The panel contract is `MemoryNode` JSON; a rename that changes a field
    /// name or drops `sub_type` omission would break the Desktop store.
    #[test]
    fn memory_node_json_shape_is_frozen() {
        let node = MemoryNode {
            node_id: 3,
            node_type: "Episodic".to_string(),
            sub_type: None,
            content: "did a thing".to_string(),
            confidence: 0.5,
            importance: 0.0,
            decay_score: 1.0,
            created_at: 1_700_000_000,
            last_accessed_at: 1_700_000_001,
            access_count: 2,
            status: "Active".to_string(),
        };
        let v = serde_json::to_value(&node).unwrap();
        assert_eq!(v["node_id"], 3);
        assert_eq!(v["node_type"], "Episodic");
        assert_eq!(v["status"], "Active");
        assert_eq!(v["decay_score"], 1.0);
        assert!(
            v.get("sub_type").is_none(),
            "sub_type is omitted, not null, when absent"
        );

        let resp = MemoryNodeListResponse {
            nodes: vec![node],
            total: 1,
            page: 1,
            size: 20,
            model_dim: 384,
        };
        let v = serde_json::to_value(&resp).unwrap();
        for key in ["nodes", "total", "page", "size", "model_dim"] {
            assert!(v.get(key).is_some(), "list response must carry `{key}`");
        }
    }

    /// The progress payload is what the panel's rebuild bar reads, so a
    /// renamed or dropped field silently breaks the polling loop.
    #[test]
    fn rebuild_progress_shape_is_frozen() {
        let p = RebuildProgress::running();
        let v = serde_json::to_value(&p).unwrap();
        for key in ["rebuilt", "total", "done", "error"] {
            assert!(v.get(key).is_some(), "progress must carry `{key}`");
        }
        assert_eq!(v["done"], false);
        assert!(v["error"].is_null());
    }
}
