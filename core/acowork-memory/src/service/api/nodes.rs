//! Node CRUD, stats and graph routes (ADR-100 P1d).
//!
//! Ports of `get_memory_nodes` / `create_memory_node` / `get_memory_node` /
//! `update_memory_node` / `delete_memory_node` / `get_memory_stats` /
//! `get_memory_graph` from `acowork-runtime::http::server`, through the
//! `MemoryAdminService` trait the Runtime's `MemoryAdminAdapter` also drove.
//! The business logic (pagination, clamping, the unfiltered-scan guard, stats
//! aggregation) stays in the store implementation, so a handler here is a
//! protocol converter and nothing else — moving it to the service must not
//! become a second place where those rules live.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::admin::{AdminListNodesParams, AdminNodeDetail, AdminNodeRecord, RebuildScope};
use crate::service::api::{
    InstanceQuery, JsonErrorResult, admin, embed_query, err_json, instance_store, model_dim,
};
use crate::service::error::{Result, ServiceError};
use crate::service::state::MemoryState;
use crate::wire::{MemoryNode, MemoryNodeListResponse, MemoryStats};

/// Page-size ceiling the store's semantic path applies, mirrored here only so
/// the response reports the size that was actually served.
const MAX_SEMANTIC_PAGE_SIZE: usize = 100;

/// Query parameters of `GET /memory/nodes`.
///
/// Mirrors the Runtime's `ListNodesQuery` (contract:
/// `docs/protocols/http.md` §7.7) plus `instance_id`. `type` is renamed to
/// `node_type` exactly as the Runtime did — the panel sends `?type=Knowledge`.
#[derive(Debug, Default, Deserialize)]
pub struct ListNodesQuery {
    #[serde(default)]
    pub instance_id: Option<String>,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub size: Option<u32>,
    #[serde(default, rename = "type")]
    pub node_type: Option<String>,
    #[serde(default)]
    pub sub_type: Option<String>,
    #[serde(default)]
    pub keyword: Option<String>,
    #[serde(default)]
    pub time_range: Option<String>,
    /// Semantic search text; only used when `mode` is `vector` / `hybrid`.
    #[serde(default)]
    pub q: Option<String>,
    /// `vector` / `hybrid` / `keyword`. Empty or `keyword` keeps the substring
    /// path (ADR-081 P1-1).
    #[serde(default)]
    pub mode: Option<String>,
}

/// `GET /memory/nodes` — list nodes (paginated, filtered, searched).
pub async fn list_nodes(
    State(state): State<MemoryState>,
    Query(params): Query<ListNodesQuery>,
) -> Result<Json<Value>> {
    let (_id, store) = instance_store(&state, params.instance_id.as_deref())?;
    let svc = admin(store);
    let dim = model_dim(&state);

    // `?q=&mode=vector|hybrid` switches to the semantic path; `mode=keyword` or
    // an empty mode keeps the legacy substring filter below.
    let mode = params.mode.as_deref().unwrap_or("").trim();
    if mode != "keyword" && !mode.is_empty() {
        // The Runtime short-circuits an empty query to this literal instead of
        // calling the engine: `page`/`size` here are the *search* defaults, not
        // the caller's, so a zero-hit answer must not echo `?page=7`.
        let Some(q) = params.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(Json(
                json!({"nodes": [], "total": 0, "page": 1, "size": 0, "model_dim": 0}),
            ));
        };
        let embedding = embed_query(&state, q).await;
        let limit = params.size.unwrap_or(20) as usize;
        let records = svc.semantic_search(q, embedding.as_deref(), mode, limit);
        let total = records.len() as u64;
        let resp = MemoryNodeListResponse {
            nodes: records.into_iter().map(to_wire_node).collect(),
            total,
            page: 1,
            // The engine clamps its own page size (1..=100); the response has
            // to report the number of rows it actually allowed, not the one
            // asked for. Same ceiling the Runtime handler applied.
            size: limit.clamp(1, MAX_SEMANTIC_PAGE_SIZE) as u32,
            model_dim: dim,
        };
        return Ok(Json(serde_json::to_value(resp)?));
    }

    let out = svc.list_nodes(&AdminListNodesParams {
        page: params.page.unwrap_or(1),
        size: params.size.unwrap_or(20),
        node_type: params.node_type.unwrap_or_default(),
        sub_type: params.sub_type.unwrap_or_default(),
        keyword: params.keyword.unwrap_or_default(),
        time_range: params.time_range.unwrap_or_default(),
    });
    let resp = MemoryNodeListResponse {
        nodes: out.nodes.into_iter().map(to_wire_node).collect(),
        total: out.total,
        page: out.page,
        size: out.size,
        model_dim: dim,
    };
    Ok(Json(serde_json::to_value(resp)?))
}

/// Request body of `POST /memory/nodes`.
#[derive(Debug, Deserialize)]
pub struct CreateNodeBody {
    /// Node label (`"Knowledge"`, `"Episodic"`, …).
    pub label: String,
    /// Flat key→JSON property map.
    #[serde(default)]
    pub properties: HashMap<String, Value>,
}

/// `POST /memory/nodes` — create a node.
pub async fn create_node(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    Json(body): Json<CreateNodeBody>,
) -> Result<Json<Value>> {
    let (_id, store) = instance_store(&state, query.as_deref())?;
    let node_id = admin(store).create_node(&body.label, &body.properties)?;
    Ok(Json(json!({"node_id": node_id, "label": body.label})))
}

/// `GET /memory/nodes/{nid}` — one node in full.
pub async fn get_node(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    Path(nid): Path<String>,
) -> Result<Json<Value>> {
    let node_id = parse_node_id(&nid)?;
    let (_id, store) = instance_store(&state, query.as_deref())?;
    let detail = admin(store).get_node(node_id);
    Ok(Json(node_detail_json(&detail)))
}

/// Request body of `PUT /memory/nodes/{nid}`.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateNodeBody {
    #[serde(default)]
    pub properties: HashMap<String, Value>,
}

/// `PUT /memory/nodes/{nid}` — merge properties into a node.
///
/// One of the two routes that answers with a JSON error body, because the
/// panel reads `error` to tell "someone else deleted this row" apart from
/// "the request was malformed". The Runtime distinguished
/// `RuntimeError::Memory("… not found")` (404) from everything else (500) and
/// this keeps that split, matching on the same `not found` marker the SQLite
/// `update_node` emits.
pub async fn update_node(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    Path(nid): Path<String>,
    Json(body): Json<UpdateNodeBody>,
) -> JsonErrorResult<Json<Value>> {
    let node_id: u64 = nid.parse().map_err(|_| {
        err_json(
            axum::http::StatusCode::BAD_REQUEST,
            "node_id must be a non-negative integer",
        )
    })?;

    let (_id, store) = instance_store(&state, query.as_deref()).map_err(|e| json_error_body(&e))?;

    match admin(store).update_node(node_id, &body.properties) {
        Ok(()) => Ok(Json(json!({"updated": true, "node_id": node_id}))),
        Err(e) if e.to_string().contains("not found") => Err((
            axum::http::StatusCode::NOT_FOUND,
            Json(json!({"error": e.to_string(), "node_id": node_id})),
        )),
        Err(e) => {
            tracing::warn!(
                node_id,
                error = %e,
                "memory service: update_node failed"
            );
            Err(err_json(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "failed to update memory node",
            ))
        }
    }
}

/// `DELETE /memory/nodes/{nid}` — delete a node.
///
/// Reports `deleted: true` whatever the store says, because the Runtime
/// handler discarded the trait's `bool` and answered with a literal. Whether
/// the row existed is not part of this contract yet (the panel treats a 200 as
/// success and re-fetches the list), so keeping the behaviour is what preserves
/// the wire format; fixing it would be a contract change, not a port.
pub async fn delete_node(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
    Path(nid): Path<String>,
) -> Result<Json<Value>> {
    let node_id = parse_node_id(&nid)?;
    let (_id, store) = instance_store(&state, query.as_deref())?;
    admin(store).delete_node(node_id);
    Ok(Json(json!({"deleted": true, "node_id": node_id})))
}

/// `GET /memory/stats` — statistics plus vector-index diagnostics.
pub async fn stats(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
) -> Result<Json<Value>> {
    let (_id, store) = instance_store(&state, query.as_deref())?;
    let svc = admin(store);
    let s = svc.get_stats();
    let model = model_dim(&state);
    // The repair count is measured against the width a rebuild would target:
    // the active model's dimension, or the store's own declared dimension when
    // no model is bound (P1e installs the provider; until then `model` is 0 and
    // this falls back, exactly as the Runtime did with an unbound provider).
    let rebuild_dim = if model > 0 { model } else { s.stored_dim } as usize;
    let out = MemoryStats {
        total_nodes: s.total_nodes,
        storage_bytes: s.storage_bytes,
        by_type: s.by_type,
        by_status: s.by_status,
        index_health: s.index_health,
        stored_dim: s.stored_dim,
        nodes_with_embedding: s.nodes_with_embedding,
        vectors_of_other_dim: s.vectors_of_other_dim,
        rebuild_rows_all: svc.rebuild_scope_count(RebuildScope::All, rebuild_dim),
        rebuild_rows_repair: svc.rebuild_scope_count(RebuildScope::Repair, rebuild_dim),
        model_dim: model,
        schema_version: s.schema_version,
    };
    Ok(Json(serde_json::to_value(out)?))
}

/// `GET /memory/graph` — every node, for the panel's graph view.
///
/// `size` is asked for as `100_000` and the store clamps it to its page-size
/// ceiling; that clamp is part of the observable behaviour (the graph shows at
/// most one page), so the number here is kept as-is rather than "corrected".
///
/// The Runtime echoed its own `agent_id`; the service has no agent identity, so
/// the field carries the instance namespace key. The panel does not read this
/// field, and the Gateway proxy that does keep agent addressing is P2.
pub async fn graph(
    State(state): State<MemoryState>,
    Query(query): Query<InstanceQuery>,
) -> Result<Json<Value>> {
    let (id, store) = instance_store(&state, query.as_deref())?;
    let out = admin(store).list_nodes(&AdminListNodesParams {
        page: 1,
        size: 100_000,
        node_type: String::new(),
        sub_type: String::new(),
        keyword: String::new(),
        time_range: "all".to_string(),
    });
    let nodes: Vec<Value> = out
        .nodes
        .iter()
        .map(|n| {
            json!({
                "node_id": n.node_id,
                "node_type": n.node_type,
                "content": n.content,
                "confidence": n.confidence,
                "decay_score": n.decay_score,
                "created_at": n.created_at,
                "last_accessed_at": n.last_accessed_at,
                "access_count": n.access_count,
                "status": n.status,
            })
        })
        .collect();
    Ok(Json(json!({
        "agent_id": id,
        "node_count": nodes.len(),
        "nodes": nodes,
        "edges": [],
    })))
}

// ── Shared shaping ────────────────────────────────────────────────────────

/// Admin record → the panel's `MemoryNode`.
fn to_wire_node(r: AdminNodeRecord) -> MemoryNode {
    MemoryNode {
        node_id: r.node_id,
        node_type: r.node_type,
        sub_type: r.sub_type,
        content: r.content,
        confidence: r.confidence,
        importance: r.importance,
        decay_score: r.decay_score,
        created_at: r.created_at,
        last_accessed_at: r.last_accessed_at,
        access_count: r.access_count,
        status: r.status,
    }
}

/// `GET /memory/nodes/{nid}` body, field-for-field as `get_output_to_json`
/// built it: `sub_type` is **absent** (not null) when the node has no
/// sub-classification, and `found: false` is a 200 — the panel renders
/// "not found" from the flag, never from a status code.
fn node_detail_json(d: &AdminNodeDetail) -> Value {
    let mut obj = json!({
        "node_id": d.node_id,
        "found": d.found,
        "node_type": d.node_type,
        "content": d.content,
        "confidence": d.confidence,
        "importance": d.importance,
        "decay_score": d.decay_score,
        "created_at": d.created_at,
        "last_accessed_at": d.last_accessed_at,
        "access_count": d.access_count,
        "status": d.status,
        "properties": d.properties,
        "message": d.message,
    });
    if let Some(ref st) = d.sub_type {
        obj["sub_type"] = Value::String(st.clone());
    }
    obj
}

/// Path parameter → node id, as a 400 (bare, like the Runtime's `StatusCode`).
fn parse_node_id(nid: &str) -> Result<u64> {
    nid.parse::<u64>()
        .map_err(|_| ServiceError::BadRequest("node_id must be a non-negative integer".into()))
}

/// A [`ServiceError`] rendered as the JSON-body error shape this route uses.
fn json_error_body(e: &ServiceError) -> (axum::http::StatusCode, Json<Value>) {
    err_json(e.http_status(), e.code())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detail(sub_type: Option<&str>) -> AdminNodeDetail {
        AdminNodeDetail {
            node_id: 7,
            found: true,
            node_type: "Knowledge".to_string(),
            sub_type: sub_type.map(str::to_string),
            content: "likes dark mode".to_string(),
            confidence: 0.9,
            importance: 0.5,
            decay_score: 1.0,
            created_at: 100,
            last_accessed_at: 101,
            access_count: 2,
            status: "Active".to_string(),
            properties: HashMap::new(),
            message: String::new(),
        }
    }

    #[test]
    fn node_detail_json_omits_absent_sub_type() {
        let v = node_detail_json(&detail(None));
        assert!(v.get("sub_type").is_none(), "must be absent, not null");
        assert_eq!(v["found"], true);
        assert_eq!(v["importance"], 0.5);
        assert!(v.get("properties").is_some());
        assert!(v.get("message").is_some());
    }

    #[test]
    fn node_detail_json_carries_present_sub_type() {
        let v = node_detail_json(&detail(Some("Preference")));
        assert_eq!(v["sub_type"], "Preference");
    }

    #[test]
    fn a_non_numeric_node_id_is_a_bad_request() {
        let e = parse_node_id("abc").unwrap_err();
        assert_eq!(e.http_status(), axum::http::StatusCode::BAD_REQUEST);
        assert_eq!(parse_node_id("42").unwrap(), 42);
    }
}
