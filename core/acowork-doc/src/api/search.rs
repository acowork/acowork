//! Search REST handler (design §4: `GET /api/search?keyword=`).

use axum::extract::Query;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;

use crate::api::{actor_from, ApiError, ApiResult, ApiState};
use crate::service::search::SearchService;
use crate::types::SearchHit;

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub keyword: String,
    #[serde(default = "default_limit")]
    pub limit: usize,
}

fn default_limit() -> usize {
    20
}

/// `GET /search?keyword=...&limit=...` — cross-directory keyword search.
pub async fn search(
    state: ApiState,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<Vec<SearchHit>>> {
    let actor = actor_from(&headers);
    let hits = state
        .search
        .search(&q.keyword, q.limit)
        .await
        .map_err(ApiError::from)?;
    // A hit leaks title + snippet, so it must be `view`-visible (design §9).
    let mut visible = Vec::with_capacity(hits.len());
    for h in hits {
        if state.acl.can_view_doc(&actor, &h.doc_id).await {
            visible.push(h);
        }
    }
    Ok(Json(visible))
}
