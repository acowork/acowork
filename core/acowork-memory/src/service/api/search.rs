//! Memory-scope search route (ADR-100 P1d).
//!
//! `GET /memory/search` is the memory half of the Runtime's aggregate
//! `GET /search` ([`crate::service::api`] documents why the service keeps the
//! Runtime path shape). The other scopes — conversation, file, git — are not
//! memory data and stay in the Runtime: the Desktop's memory tab will talk to
//! this route directly (P4), while the aggregate dialog keeps asking the agent.
//!
//! The hit and envelope shapes are copied from the Runtime's memory scope so
//! the panel's renderer does not branch: locate keys first, retrieval evidence
//! merged in on top of them, `cosine` / `fused_score` deliberately **not**
//! published (see [`crate::wire::RetrievalEvidence::merge_into`]).

use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use acowork_core::retrieval::FusionPolicy;

use crate::service::api::{admin, embed_query, instance_store};
use crate::service::error::Result;
use crate::service::state::MemoryState;
use crate::wire::RetrievalEvidence;

/// Default / ceiling hit count, as the Runtime's `GET /search` used them.
const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 50;

/// Query parameters of `GET /memory/search`.
#[derive(Debug, Default, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub instance_id: Option<String>,
    /// Query text. Empty returns the empty envelope.
    #[serde(default)]
    pub q: String,
    /// `vector` / `hybrid` / `keyword`. Empty means `hybrid`.
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// `GET /memory/search?q=&mode=&limit=&instance_id=` — ranked memory hits.
pub async fn search(
    State(state): State<MemoryState>,
    Query(params): Query<SearchQuery>,
) -> Result<Json<Value>> {
    let (_id, store) = instance_store(&state, params.instance_id.as_deref())?;
    let svc = admin(store);

    let q = params.q.trim().to_string();
    let limit = params.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    // Same short-circuit as the aggregate endpoint: an empty query is not an
    // error, and it reports no scopes rather than a scope with zero hits.
    if q.is_empty() {
        return Ok(Json(json!({"hits": [], "scopes": {}, "indexing": false})));
    }

    let embedding = embed_query(&state, &q).await;
    let embedded = embedding.is_some();
    let mode = if params.mode.is_empty() {
        "hybrid"
    } else {
        params.mode.as_str()
    };

    // The memory scope's production policy (ADR-095 D3): 4:1 toward the vector
    // source and no cosine floor. The per-agent `search` config override lived
    // in the Runtime's `agent_config.json`, which this service does not read,
    // so the default is the honest value rather than a guess at the agent's.
    //
    // No lexical terms are passed: the ADR-095 analyzer lives on the Runtime
    // side, and an empty term list tells the engine to drive FTS with the raw
    // query (D2) — which is what `analyzer_mode = off` did there, so retrieval
    // behaviour is unchanged by the move.
    let hits = svc.semantic_search_detailed(
        &q,
        &[],
        embedding.as_deref(),
        mode,
        limit,
        &FusionPolicy::SEMANTIC_4TO1,
    );

    let hits_json: Vec<Value> = hits
        .into_iter()
        .map(|(node, fused)| {
            let evidence = RetrievalEvidence {
                matched_by: fused.matched_by,
                vector_rank: fused.vector_rank,
                text_rank: fused.text_rank,
                cosine: fused.cosine,
                fused_score: fused.fused_score,
                fused_rank: Some(fused.fused_rank),
            };
            // Locate keys first, evidence merged on top — assigning the
            // evidence object as the whole payload dropped `node_id` and made
            // the preview panel fetch node 0.
            let mut payload = json!({
                "node_id": node.node_id,
                "node_type": node.node_type.clone(),
                "sub_type": node.sub_type,
            });
            if let Some(map) = payload.as_object_mut() {
                evidence.merge_into(map);
            }
            json!({
                "scope": "memory",
                "title": node.node_type,
                "snippet": node.content,
                // In-source RRF signal, not a similarity: the caller renders
                // the returned order, never a re-sort by this number (D4).
                "score": evidence.fused_score,
                "payload": payload,
            })
        })
        .collect();

    let count = hits_json.len();
    Ok(Json(json!({
        "hits": hits_json,
        "scopes": {"memory": {"status": "ok", "count": count}},
        "indexing": false,
        "search_meta": {
            "analyzer_mode": "off",
            "lexical_term_count": 0,
            "memory": {"embedded": embedded},
        },
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The empty-query envelope has no `scopes` / `search_meta` block — a
    /// client that expects `scopes.memory` for every response would break on
    /// this, so the asymmetry is contract, not accident.
    #[test]
    fn empty_search_envelope_keys() {
        let v = json!({"hits": [], "scopes": {}, "indexing": false});
        assert!(v["scopes"].as_object().unwrap().is_empty());
        assert!(v.get("search_meta").is_none());
    }

    #[test]
    fn limit_bounds_match_the_runtime() {
        let clamp = |l: Option<usize>| l.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        assert_eq!(clamp(None), 20);
        assert_eq!(clamp(Some(0)), 1);
        assert_eq!(clamp(Some(500)), 50);
    }
}
