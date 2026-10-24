//! RAG (Retrieval-Augmented Generation) provider trait and protocol types.
//!
//! Defines the standardized interface for enterprise knowledge retrieval.
//! The `RagProvider` trait is orthogonal to `MemoryProvider`:
//! - `MemoryProvider` handles user memory (preferences, history, patterns).
//! - `RagProvider` handles external knowledge (enterprise docs, product manuals).
//!
//! Since ADR-100 the two channels are consumed independently: the Agent calls
//! the `rag_query` builtin tool and the memory service's `memory_recall` tool,
//! and the LLM sees both as tool results. There is no in-loop merge any more.
//!
//! Design ref: ADR-051 §5.1

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

// ============================================================================
// Protocol version
// ============================================================================

/// Current RAG protocol version.
pub const PROTOCOL_VERSION: &str = "1.0";

// ============================================================================
// Request / Response types (migrated from acowork-runtime::tools::rag::types)
// ============================================================================

/// RAG standard query request.
///
/// Sent as `POST <endpoint>` with JSON body.
/// Enterprise RAG services must accept this format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagQueryRequest {
    /// Protocol version (currently "1.0").
    pub protocol_version: String,
    /// Query text.
    pub query: String,
    /// Collection / index name (optional, from manifest config).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collection: Option<String>,
    /// Maximum number of results to return.
    pub top_k: u32,
    /// Minimum score threshold (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score_threshold: Option<f32>,
    /// Additional filters (optional, for enterprise-specific filtering).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filters: Option<serde_json::Value>,
    /// Reserved for future protocol extensions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

impl RagQueryRequest {
    /// Create a new query request with defaults.
    pub fn new(query: String, top_k: u32) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_string(),
            query,
            collection: None,
            top_k,
            score_threshold: None,
            filters: None,
            extensions: None,
        }
    }
}

/// RAG standard query response.
///
/// Enterprise RAG services must return this format.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagQueryResponse {
    /// Protocol version (must match request).
    pub protocol_version: String,
    /// Query results, sorted by relevance (highest first).
    pub results: Vec<RagResultItem>,
    /// Reserved for future protocol extensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Value>,
}

/// A single RAG query result item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagResultItem {
    /// Content text of the result chunk.
    pub content: String,
    /// Source URL or document reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    /// Chunk identifier within the source document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_id: Option<String>,
    /// Relevance score (0.0 - 1.0, higher = more relevant).
    #[serde(default)]
    pub score: f32,
}

// ============================================================================
// Annotated result type
// ============================================================================

/// Annotated RAG result for injection into LLM context.
///
/// Wraps a [`RagResultItem`] with source annotation for the
/// Runtime's dual-channel retrieve merge.
#[derive(Debug, Clone)]
pub struct AnnotatedRagResult {
    /// The result item.
    pub item: RagResultItem,
    /// Source label for context annotation (e.g., "[RAG:enterprise_knowledge]").
    pub source_label: String,
    /// Tool name that produced this result.
    pub tool_name: String,
}

// ============================================================================
// RagProvider trait
// ============================================================================

/// RAG provider trait - standardized interface for enterprise knowledge retrieval.
///
/// Implementations:
/// - `HttpRagProvider` (current `RagClient`, in `acowork-runtime`)
/// - `LocalRagProvider` (future, local vector store)
/// - Any enterprise RAG service adapter
///
/// This trait is **orthogonal** to `MemoryProvider`: since ADR-100 the Agent
/// reaches each through its own tool (`rag_query` vs the memory service's
/// `memory_recall`), so a RAG answer never travels through the memory store.
///
/// Design ref: ADR-051 §5.1
#[async_trait]
pub trait RagProvider: Send + Sync {
    /// Query the RAG service with default parameters.
    ///
    /// Returns empty vec on timeout/error (graceful degradation).
    async fn query(&self, query_text: &str) -> Vec<AnnotatedRagResult>;

    /// Query with custom parameters (top_k, score_threshold, filters).
    async fn query_with_params(
        &self,
        query_text: &str,
        top_k: Option<u32>,
        score_threshold: Option<f32>,
        filters: Option<serde_json::Value>,
    ) -> Vec<AnnotatedRagResult>;

    /// Provider name (for source annotation, e.g. "RAG:enterprise_knowledge").
    fn name(&self) -> &str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rag_query_request_new() {
        let req = RagQueryRequest::new("product roadmap Q3".to_string(), 5);
        assert_eq!(req.protocol_version, "1.0");
        assert_eq!(req.query, "product roadmap Q3");
        assert_eq!(req.top_k, 5);
        assert!(req.collection.is_none());
        assert!(req.score_threshold.is_none());
    }

    #[test]
    fn test_rag_query_request_serialization() {
        let mut req = RagQueryRequest::new("test query".to_string(), 3);
        req.collection = Some("product_docs".to_string());
        req.score_threshold = Some(0.7);
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"protocol_version\":\"1.0\""));
        assert!(json.contains("\"query\":\"test query\""));
        assert!(json.contains("\"top_k\":3"));
        assert!(json.contains("\"collection\":\"product_docs\""));
        assert!(json.contains("\"score_threshold\":0.7"));
        assert!(!json.contains("\"filters\""));
        assert!(!json.contains("\"extensions\""));
    }

    #[test]
    fn test_rag_query_request_roundtrip() {
        let mut req = RagQueryRequest::new("test".to_string(), 10);
        req.collection = Some("docs".to_string());
        req.filters = Some(serde_json::json!({"category": "engineering"}));
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RagQueryRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.protocol_version, "1.0");
        assert_eq!(parsed.query, "test");
        assert_eq!(parsed.top_k, 10);
        assert_eq!(parsed.collection.as_deref(), Some("docs"));
    }

    #[test]
    fn test_rag_query_response_deserialization() {
        let json = r#"{
            "protocol_version": "1.0",
            "results": [
                {
                    "content": "Q3 product roadmap includes AI assistant",
                    "source_url": "https://docs.corp.example.com/roadmap",
                    "chunk_id": "roadmap-3",
                    "score": 0.92
                }
            ]
        }"#;
        let resp: RagQueryResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.protocol_version, "1.0");
        assert_eq!(resp.results.len(), 1);
        assert_eq!(
            resp.results[0].content,
            "Q3 product roadmap includes AI assistant"
        );
        assert_eq!(resp.results[0].score, 0.92);
    }

    #[test]
    fn test_rag_query_response_roundtrip() {
        let resp = RagQueryResponse {
            protocol_version: PROTOCOL_VERSION.to_string(),
            results: vec![RagResultItem {
                content: "Test content".to_string(),
                source_url: Some("https://example.com".to_string()),
                chunk_id: Some("chunk-1".to_string()),
                score: 0.85,
            }],
            extensions: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: RagQueryResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.results.len(), 1);
        assert_eq!(parsed.results[0].content, "Test content");
    }

    #[test]
    fn test_annotated_rag_result() {
        let result = AnnotatedRagResult {
            item: RagResultItem {
                content: "test".to_string(),
                source_url: None,
                chunk_id: None,
                score: 0.9,
            },
            source_label: "[RAG:enterprise_knowledge]".to_string(),
            tool_name: "enterprise_knowledge".to_string(),
        };
        assert_eq!(result.source_label, "[RAG:enterprise_knowledge]");
        assert_eq!(result.item.score, 0.9);
    }
}
