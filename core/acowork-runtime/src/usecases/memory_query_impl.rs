//! MemoryAdminAdapter - implements MemoryQueryService via MemoryAdminService.
//!
//! ADR-040: delegates to the shared `memory_query` module which provides
//! thin wrappers over `dyn MemoryAdminService`.
//!
//! ADR-051 P4: `SharedMemoryStore` is `Arc<dyn MemoryAdminService>`, never a
//! concrete store type.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::embedding::SharedEmbedProvider;
use crate::http::{SharedMemoryStore, memory_query};
use crate::usecases::memory_query::{
    CreateMemoryNodeInput, MemoryNode, MemoryNodeListResponse, MemoryNodeQuery, MemoryQueryService,
    MemoryStats, RebuildReport, RetrievalEvidence, ScoredMemoryNode, SemanticMemoryQuery,
};

pub struct MemoryAdminAdapter {
    memory_store: SharedMemoryStore,
    embed_provider: SharedEmbedProvider,
}

impl MemoryAdminAdapter {
    pub fn new(memory_store: SharedMemoryStore, embed_provider: SharedEmbedProvider) -> Self {
        Self {
            memory_store,
            embed_provider,
        }
    }

    /// The width the live embedding provider produces; 0 when none is bound.
    ///
    /// Read from the provider rather than stored alongside it: the panel's
    /// "does the index match the model" banner is only meaningful against the
    /// model actually in use, and a copy here went stale on every model switch
    /// because the session that adopts a new provider has no handle on this
    /// adapter.
    fn live_dim(&self) -> u64 {
        self.embed_provider
            .read()
            .ok()
            .and_then(|cell| cell.as_ref().map(|p| p.dimension() as u64))
            .unwrap_or(0)
    }
}

#[async_trait]
impl MemoryQueryService for MemoryAdminAdapter {
    async fn list_nodes(&self, query: &MemoryNodeQuery) -> Result<MemoryNodeListResponse> {
        let store = self.memory_store.read().ok().and_then(|g| g.clone());
        let params = memory_query::ListNodesParams {
            page: query.page,
            size: query.size,
            node_type: query.node_type.clone(),
            sub_type: query.sub_type.clone(),
            keyword: query.keyword.clone(),
            time_range: query.time_range.clone(),
        };
        let out = memory_query::list_nodes(store.as_ref(), params);
        let dim = self.live_dim();

        let nodes: Vec<MemoryNode> = out
            .nodes
            .into_iter()
            .map(|n| MemoryNode {
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
            .collect();

        Ok(MemoryNodeListResponse {
            nodes,
            total: out.total,
            page: out.page,
            size: out.size,
            model_dim: dim,
        })
    }

    async fn semantic_search(
        &self,
        query: &SemanticMemoryQuery,
    ) -> Result<MemoryNodeListResponse> {
        let store = self.memory_store.read().ok().and_then(|g| g.clone());
        let out = memory_query::semantic_search(
            store.as_ref(),
            &query.query_text,
            query.embedding.as_deref(),
            &query.mode,
            query.limit,
        );
        let dim = self.live_dim();

        let nodes: Vec<MemoryNode> = out
            .nodes
            .into_iter()
            .map(|n| MemoryNode {
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
            .collect();

        Ok(MemoryNodeListResponse {
            nodes,
            total: out.total,
            page: out.page,
            size: out.size,
            model_dim: dim,
        })
    }

    async fn semantic_search_detailed(
        &self,
        query: &SemanticMemoryQuery,
        policy: &acowork_core::retrieval::FusionPolicy,
    ) -> Result<Vec<ScoredMemoryNode>> {
        let store = self.memory_store.read().ok().and_then(|g| g.clone());
        let lexical = acowork_core::retrieval::LexicalQuery {
            raw: &query.query_text,
            terms: &query.lexical_terms,
        };
        let hits = memory_query::semantic_search_detailed(
            store.as_ref(),
            &lexical,
            query.embedding.as_deref(),
            &query.mode,
            query.limit,
            policy,
        );
        Ok(hits
            .into_iter()
            .map(|(n, f)| {
                let evidence = RetrievalEvidence {
                    matched_by: f.matched_by,
                    vector_rank: f.vector_rank,
                    text_rank: f.text_rank,
                    cosine: f.cosine,
                    fused_score: f.fused_score,
                    fused_rank: Some(f.fused_rank),
                };
                ScoredMemoryNode {
                    node: MemoryNode {
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
                    },
                    evidence: Some(evidence),
                }
            })
            .collect())
    }

    async fn get_node(&self, node_id: u64) -> Result<serde_json::Value> {
        let store = self.memory_store.read().ok().and_then(|g| g.clone());
        let out = memory_query::get_node(store.as_ref(), node_id);
        Ok(memory_query::get_output_to_json(&out))
    }

    async fn get_stats(&self) -> Result<MemoryStats> {
        let store = self.memory_store.read().ok().and_then(|g| g.clone());
        let dim = self.live_dim();
        Ok(memory_query::get_stats(store.as_ref(), dim))
    }

    async fn delete_node(&self, node_id: u64) -> Result<()> {
        let store = self.memory_store.read().ok().and_then(|g| g.clone());
        memory_query::delete_node(store.as_ref(), node_id);
        Ok(())
    }

    async fn create_node(&self, input: &CreateMemoryNodeInput) -> Result<u64> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
        memory_query::create_node(Some(&store), &input.label, &input.properties)
    }

    async fn update_node(
        &self,
        node_id: u64,
        properties: &HashMap<String, serde_json::Value>,
    ) -> Result<()> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
        memory_query::update_node(Some(&store), node_id, properties)
    }

    async fn rebuild_embeddings_with_progress(
        &self,
        endpoint: &str,
        model_id: &str,
        dimension: usize,
        scope: acowork_memory::admin::RebuildScope,
        progress: Option<Arc<dyn Fn(u64, u64) + Send + Sync>>,
    ) -> Result<RebuildReport> {
        let store = self
            .memory_store
            .read()
            .ok()
            .and_then(|g| g.clone())
            .ok_or_else(|| crate::error::RuntimeError::Memory("memory store unavailable".into()))?;
        let admin: Arc<dyn acowork_memory::admin::MemoryAdminService> = store;

        // Re-embedding is CPU/IO heavy and bridges async embed into a sync
        // closure, so run the whole migration on a blocking thread (same
        // pattern as the session-task UpdateEmbedConfig migration path).
        let endpoint = endpoint.to_string();
        let model_id = model_id.to_string();
        let stats = tokio::task::spawn_blocking(move || {
            let provider =
                crate::embedding::remote::RemoteEmbeddingProvider::try_with_config_and_timeouts(
                    &endpoint,
                    None,
                    &model_id,
                    dimension,
                    &acowork_core::Timeouts::default(),
                );
            let provider: Arc<dyn crate::embedding::EmbeddingProvider> = match provider {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    return Err(crate::error::RuntimeError::Memory(format!(
                        "failed to build embedding provider for migration: {e}"
                    )));
                }
            };

            let handle = tokio::runtime::Handle::current();
            let provider_for_fn = provider.clone();
            let embed_fn = move |text: &str| -> Option<Vec<f32>> {
                let text_owned = text.to_string();
                match handle.block_on(provider_for_fn.embed(&text_owned)) {
                    Ok(vec) => Some(vec),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Re-embedding failed during migration, skipping node"
                        );
                        None
                    }
                }
            };

            let progress_cb: Option<&dyn Fn(u64, u64)> = progress
                .as_ref()
                .map(|cb| &**cb as &dyn Fn(u64, u64));
            admin.migrate_embedding_dimension_scoped(&embed_fn, dimension, scope, progress_cb)
                .map_err(|e| crate::error::RuntimeError::Memory(e.to_string()))
        })
        .await
        .map_err(|e| crate::error::RuntimeError::Memory(format!("rebuild task panicked: {e}")))??;

        let message = format!(
            "Rebuilt {} embeddings (scanned {}, skipped {} no-embedding / {} no-content, {} errors)",
            stats.rebuilt,
            stats.total_scanned,
            stats.skipped_no_embedding,
            stats.skipped_no_content,
            stats.errors,
        );
        Ok(RebuildReport {
            total_scanned: stats.total_scanned,
            rebuilt: stats.rebuilt,
            skipped_no_embedding: stats.skipped_no_embedding,
            skipped_no_content: stats.skipped_no_content,
            errors: stats.errors,
            message,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::RwLock;

    use acowork_memory::admin::MemoryAdminService;
    use acowork_memory::types::{
        DEFAULT_EMBEDDING_DIM, Episode, KnowledgeNode, KnowledgeSubType, NodeStatus, PrivacyLevel,
    };

    use super::*;

    /// Build an adapter backed by an in-memory store seeded with one Episodic
    /// node (importance only) and one Knowledge node (both fields).
    fn seeded_adapter() -> MemoryAdminAdapter {
        let store = Arc::new(
            acowork_sqlite::SqliteStore::open_in_memory(DEFAULT_EMBEDDING_DIM)
                .expect("in-memory store"),
        );
        let now = chrono::Utc::now();
        store
            .store_episode(&Episode {
                session_id: "adapter-test".to_string(),
                turn_index: 0,
                role: "user".to_string(),
                content: "episodic event".to_string(),
                embedding: None,
                timestamp: now,
                consolidated: false,
                metadata: Default::default(),
                importance: 0.7,
                knowledge_subtype: None,
                normalized: None,
            })
            .unwrap();
        store
            .store_knowledge(&KnowledgeNode {
                subject: "Rust".to_string(),
                predicate: "is".to_string(),
                object: "a systems language".to_string(),
                sub_type: KnowledgeSubType::Fact,
                confidence: 0.7,
                source_episode_id: None,
                source_episode_ids: Vec::new(),
                promotion_metadata: None,
                embedding: None,
                status: NodeStatus::Active,
                created_at: now,
                updated_at: now,
                metadata: Default::default(),
                privacy: PrivacyLevel::Personal,
                importance: 0.5,
            })
            .unwrap();

        let admin: Arc<dyn MemoryAdminService> = store;
        let memory_store: SharedMemoryStore = Arc::new(RwLock::new(Some(admin)));
        let embed_provider: crate::embedding::SharedEmbedProvider = Arc::new(RwLock::new(None));
        MemoryAdminAdapter::new(memory_store, embed_provider)
    }

    #[tokio::test]
    async fn list_nodes_passes_importance_through_verbatim() {
        let adapter = seeded_adapter();
        let resp = adapter
            .list_nodes(&MemoryNodeQuery {
                page: 1,
                size: 20,
                node_type: String::new(),
                sub_type: String::new(),
                keyword: String::new(),
                time_range: String::new(),
            })
            .await
            .expect("list should succeed");

        let by_type: std::collections::HashMap<&str, &MemoryNode> = resp
            .nodes
            .iter()
            .map(|n| (n.node_type.as_str(), n))
            .collect();

        // Episodic: importance present, confidence absent → 0.0 is the truth.
        // `importance` is f32 on the node and f64 on the DTO, so compare with
        // the tolerance the widening implies rather than bit equality.
        let ep = by_type.get("Episodic").expect("episodic node");
        assert!(
            (ep.importance - 0.7).abs() < 1e-6,
            "Episodic importance must pass through, got {}",
            ep.importance
        );
        assert_eq!(ep.confidence, 0.0, "Episodic has no confidence property");

        // Knowledge: both fields present.
        let kn = by_type.get("Knowledge").expect("knowledge node");
        assert!((kn.confidence - 0.7).abs() < 1e-6, "got {}", kn.confidence);
        assert!((kn.importance - 0.5).abs() < 1e-6, "got {}", kn.importance);
    }

    #[tokio::test]
    async fn semantic_search_keyword_falls_back_to_text_without_embedding() {
        let adapter = seeded_adapter();
        // No embedding provider is bound in the adapter test; mode=hybrid
        // with `embedding: None` must still surface BM25 text matches.
        let resp = adapter
            .semantic_search(&SemanticMemoryQuery {
                query_text: "systems language".to_string(),
                mode: "hybrid".to_string(),
                limit: 20,
                embedding: None,
            // No analyzer on this path: raw text drives both sources.
            lexical_terms: Vec::new(),
            })
            .await
            .expect("semantic search should succeed");
        assert_eq!(resp.total, 1);
        assert_eq!(resp.nodes[0].node_type, "Knowledge");
        assert!(resp.nodes[0].content.contains("Rust"));
    }
}
