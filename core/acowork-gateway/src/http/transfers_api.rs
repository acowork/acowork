//! Agent-facing transfer routes (ADR-122 §4.3, D12/D13).
//!
//! An `intent` attachment travels agent → store → agent, and both ends reach
//! the user service's transfer area **through here**, never directly: a
//! Runtime only knows the Gateway, and the Gateway is the one component that
//! can attribute an HTTP call to an agent instance (node token + the online
//! instance's host, the same check intent delivery uses).
//!
//! What this module does *not* do is store bytes (D12). It verifies who is
//! calling, adds exactly one trusted header naming that verified instance
//! (`x-acowork-creator-instance` on the way in, `x-acowork-reader-instance` on
//! the way out), and streams the user service's answer back — a 1 GiB pull
//! must never be buffered in the Gateway's heap.
//!
//! Authorization for a read is *not* decided here: the transfer record's
//! `granted_to` list is, and the intent path is what writes to it (see
//! [`super::intent_api`]). This module only proves the caller is the instance
//! whose name it passes down.

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, put};
use axum::Router;
use serde::Deserialize;

use crate::http::intent_api::verify_sender_identity;
use crate::http::proxy::runtime_http_client;
use crate::http::user_proxy::forwardable_response_headers;
use crate::http::routes::{ApiError, AppState};

/// Header the user service reads to learn the verified uploader.
pub(crate) const CREATOR_HEADER: &str = "x-acowork-creator-instance";
/// Header the user service reads to learn the verified reader.
pub(crate) const READER_HEADER: &str = "x-acowork-reader-instance";

#[derive(Debug, Deserialize)]
struct SenderQuery {
    /// The calling agent instance. Declared, then verified — never trusted.
    sender: String,
}

/// Mount the transfer surface the Runtimes talk to.
pub fn transfer_routes() -> Router<AppState> {
    Router::new()
        .route("/api/transfers/uploads", post(create_upload))
        .route(
            "/api/transfers/uploads/{upload_id}",
            any(upload_id_verb),
        )
        .route(
            "/api/transfers/uploads/{upload_id}/complete",
            post(complete_upload),
        )
        .route("/api/transfers/uploads/{upload_id}/{offset}", put(put_chunk))
        .route("/api/transfers/{transfer_id}/{what}", get(read_transfer))
}

/// Forward to the user service with one trusted identity header.
///
/// `stream` decides how the answer's body is relayed: chunk uploads and small
/// metadata reads are buffered (they are 2 MiB / a few hundred bytes), the
/// content pull is not.
async fn forward(
    state: &AppState,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    identity: (&str, String),
    body: Option<Bytes>,
    stream: bool,
) -> Response {
    let port = {
        let gw = state.gateway_state.read().await;
        gw.user_process.as_ref().map(|p| p.port)
    };
    let Some(port) = port else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({ "error": "user service not ready" })),
        )
            .into_response();
    };

    let mut request = runtime_http_client().request(
        method.clone(),
        format!("http://127.0.0.1:{port}/internal/transfers{path_and_query}"),
    );
    // Only the headers that carry meaning for these routes travel: forwarding
    // everything would let a caller smuggle one the user service trusts.
    if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
        request = request.header(axum::http::header::CONTENT_TYPE, ct.clone());
    }
    request = request.header(identity.0, HeaderValue::from_str(&identity.1).unwrap_or_else(|_| HeaderValue::from_static("")));
    if let Some(body) = body
        && !body.is_empty()
    {
        request = request.body(body.to_vec());
    }

    let upstream = match request.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "transfer proxy: user service unreachable");
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({ "error": "user service unreachable" })),
            )
                .into_response();
        }
    };

    let status = StatusCode::from_u16(upstream.status().as_u16())
        .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let out_headers = forwardable_response_headers(upstream.headers());
    if stream {
        // `Body::from_stream` relays chunk by chunk — the pull is the only
        // path here whose size is unbounded (the ADR caps it at the upload
        // ceiling, which is 1 GiB by default).
        let mut response = Response::new(Body::from_stream(upstream.bytes_stream()));
        *response.status_mut() = status;
        *response.headers_mut() = out_headers;
        return response;
    }
    let bytes = upstream.bytes().await.unwrap_or_default();
    if method == Method::HEAD {
        // A HEAD answer must carry no body; hyper would otherwise complain
        // about a length mismatch.
        let mut response = Response::new(Body::empty());
        *response.status_mut() = status;
        *response.headers_mut() = out_headers;
        return response;
    }
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = status;
    *response.headers_mut() = out_headers;
    response
}

async fn create_upload(
    State(state): State<AppState>,
    Query(q): Query<SenderQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    verify_sender_identity(&state, &headers, &q.sender).await?;
    Ok(forward(
        &state,
        Method::POST,
        "/uploads",
        &headers,
        (CREATOR_HEADER, q.sender),
        Some(body),
        false,
    )
    .await)
}

async fn put_chunk(
    State(state): State<AppState>,
    Query(q): Query<SenderQuery>,
    Path((upload_id, offset)): Path<(String, u64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    verify_sender_identity(&state, &headers, &q.sender).await?;
    Ok(forward(
        &state,
        Method::PUT,
        &format!("/uploads/{upload_id}/{offset}"),
        &headers,
        (CREATOR_HEADER, q.sender),
        Some(body),
        false,
    )
    .await)
}

/// `HEAD` (resume offset) and `DELETE` (abort) on one path — the id is only
/// addressable by someone the store already knows nothing about, and both
/// verbs act on in-flight state under a server-generated uuid.
async fn upload_id_verb(
    State(state): State<AppState>,
    method: Method,
    Query(q): Query<SenderQuery>,
    Path(upload_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if method != Method::HEAD && method != Method::DELETE {
        return Err(ApiError::bad_request("transfer upload verb must be HEAD or DELETE"));
    }
    verify_sender_identity(&state, &headers, &q.sender).await?;
    Ok(forward(
        &state,
        method.clone(),
        &format!("/uploads/{upload_id}"),
        &headers,
        (CREATOR_HEADER, q.sender),
        None,
        false,
    )
    .await)
}

async fn complete_upload(
    State(state): State<AppState>,
    Query(q): Query<SenderQuery>,
    Path(upload_id): Path<String>,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    verify_sender_identity(&state, &headers, &q.sender).await?;
    // The query is forwarded verbatim: `?mime=` is what types the landed
    // transfer, and the caller's own `sender` riding along does no harm — the
    // trusted header, never a query parameter, is what the store believes.
    let query = raw.map(|q| format!("?{q}")).unwrap_or_default();
    Ok(forward(
        &state,
        Method::POST,
        &format!("/uploads/{upload_id}/complete{query}"),
        &headers,
        (CREATOR_HEADER, q.sender),
        Some(body),
        false,
    )
    .await)
}

/// `GET /api/transfers/{id}/content` and `/meta` — the receiver's side of a
/// transfer, streamed for the content read.
async fn read_transfer(
    State(state): State<AppState>,
    Query(q): Query<SenderQuery>,
    Path((transfer_id, what)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if what != "content" && what != "meta" {
        return Err(ApiError::not_found("unknown transfer resource"));
    }
    verify_sender_identity(&state, &headers, &q.sender).await?;
    Ok(forward(
        &state,
        Method::GET,
        &format!("/{transfer_id}/{what}"),
        &headers,
        (READER_HEADER, q.sender.clone()),
        None,
        what == "content",
    )
    .await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body as TestBody;
    use tower::ServiceExt;

    /// The surface exists on the router and refuses a caller it cannot
    /// attribute — no identity, no forwarding. (The happy path needs a live
    /// user service and is covered by the e2e suite.)
    fn test_state() -> AppState {
        use crate::gateway::state::GatewayState;
        use crate::http::auth::HttpAuth;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let dir = std::env::temp_dir().join(format!("acowork-transfers-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let mut gw = GatewayState::new(&dir.to_string_lossy());
        let cfg = crate::config::GatewayConfig {
            data_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        };
        gw.config = Some(cfg);
        AppState::new(Arc::new(RwLock::new(gw)), Arc::new(HttpAuth::new(false)))
    }

    /// An agent instance nothing can be attributed to (no node token, not in
    /// the registry) is refused before any forwarding happens.
    #[tokio::test]
    async fn transfer_routes_reject_an_unattributable_caller() {
        let app = transfer_routes().with_state(test_state());
        let resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("GET")
                    .uri("/api/transfers/00000000-0000-4000-8000-000000000000/content?sender=inst-x")
                    .body(TestBody::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "unattributable caller answered {}",
            resp.status()
        );
    }
}