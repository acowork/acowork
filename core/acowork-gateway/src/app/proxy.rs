//! The single cross-boundary HTTP channel for hosted apps (ADR-116).
//!
//! `/api/apps/{id}/…` is the only way the Desktop or an app page reaches an
//! app process. Three sub-channels, and nothing else:
//!
//! | Public path | Upstream | Auth |
//! |---|---|---|
//! | `/api/apps/{id}/ui/*` | `/ui/*` (app's own bundle) | public static bytes |
//! | `/api/apps/{id}/api/*` | `/*` (app's own REST) | app session ticket |
//! | `/api/apps/{id}/session` | — | one-time ticket → cookie exchange |
//!
//! MCP is deliberately **not** on this channel: agents reach an app's MCP
//! server through its declared `mcp.gateway_path` (`/api/pm/mcp`,
//! `/api/doc/mcp`), which keeps the machine-token and `X-MCP-Actor`
//! validation path exactly as ADR-064/ADR-101 defined it.
//!
//! Every forwarded request has client-asserted identity headers removed and
//! trusted ones injected, so an app never has to guess who is calling:
//! `X-App-Id`, `X-App-User`, `X-Actor`, `x-actor-admin`. The Gateway never
//! reads an app's data directory and never interprets its tool semantics.

use axum::{
    Router,
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};

use crate::app::registry::resolve_app_port;
use crate::app::ticket::{
    SESSION_HEADER, SESSION_TTL_SECS, read_session_cookie, session_cookie,
    verify_session_ticket,
};
use crate::http::proxy::{is_hop_by_hop_header, runtime_http_client};
use crate::http::routes::AppState;
use crate::http::workspace_preview::now_unix;

pub fn app_proxy_routes() -> Router<AppState> {
    Router::new().route("/api/apps/{id}/{*rest}", any(app_proxy_handler))
}

/// Headers a client must never be allowed to assert into an app process.
const FORBIDDEN_CLIENT_HEADERS: &[&str] = &[
    "x-actor",
    "x-actor-admin",
    "x-mcp-actor",
    "x-app-id",
    "x-app-user",
    "x-app-session",
    "x-app-platform-url",
    "authorization",
    "cookie",
    "x-acowork-node-token",
];

async fn app_proxy_handler(
    State(state): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
    method: Method,
    body: Bytes,
) -> Response {
    let manifest = {
        let gw = state.gateway_state.read().await;
        let manifest = gw.app_registry.read().ok().and_then(|r| r.get(&id).cloned());
        match manifest {
            Some(m) if gw.app_registry.read().map(|r| r.is_enabled(&id)).unwrap_or(false) => m,
            Some(_) => return service_unavailable("app is disabled"),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    axum::Json(serde_json::json!({ "error": "app not found" })),
                )
                    .into_response();
            }
        }
    };

    // One-time ticket → scoped cookie exchange. Kept for apps that prefer
    // cookie-authenticated sub-resource loads; the JavaScript SDK uses the
    // header path instead (see SESSION_HEADER).
    if rest == "session" && method == Method::POST {
        return exchange_session(&state, &id, &headers, &body);
    }

    // Control-plane routes are registered as exact routes in `app::api`;
    // never let them fall through to the upstream app.
    if matches!(rest.as_str(), "session" | "enable" | "disable" | "session-ticket" | "mcp") {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "not an app channel" })),
        )
            .into_response();
    }

    let (channel, upstream_path) = if rest == "ui" || rest.starts_with("ui/") {
        ("ui", normalize_ui_path(&rest, &manifest.ui.path))
    } else if let Some(tail) = rest.strip_prefix("api/") {
        ("api", tail.to_string())
    } else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({ "error": "unknown app channel" })),
        )
            .into_response();
    };

    // Static bundle bytes are public (no user data). App REST calls must
    // present a verified session ticket bound to this app.
    let user_id = if channel == "ui" {
        None
    } else {
        match authenticated_user(&state, &id, &headers) {
            Ok(user) => Some(user),
            Err(response) => return response,
        }
    };

    let (port, platform_url) = {
        let gw = state.gateway_state.read().await;
        (
            resolve_app_port(&gw, &manifest),
            format!("http://127.0.0.1:{}", gw.http_port),
        )
    };
    let Some(port) = port else {
        return service_unavailable("app service not ready");
    };

    // `normalize_ui_path` returns an absolute in-service path (`/ui/x`),
    // while the `api` channel returns a bare tail (`projects`). Normalise to
    // one form before building the URL — otherwise the UI channel becomes
    // `http://host//ui/` and the upstream answers 404.
    let path = upstream_path.trim_start_matches('/');
    let target_url = match query {
        Some(q) => format!("http://127.0.0.1:{port}/{path}?{q}"),
        None => format!("http://127.0.0.1:{port}/{path}"),
    };

    let forwarded = build_forwarded_headers(&headers, &id, user_id.as_deref(), &platform_url);
    let mut request = runtime_http_client().request(method, &target_url);
    for (name, value) in forwarded.iter() {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(upstream) => {
            let status = StatusCode::from_u16(upstream.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            let mut builder = Response::builder().status(status);
            for (name, value) in upstream.headers().iter() {
                if is_hop_by_hop_header(name) || name == header::SET_COOKIE {
                    continue;
                }
                builder = builder.header(name, value);
            }
            let bytes = upstream.bytes().await.unwrap_or_default();
            builder
                .body(axum::body::Body::from(bytes))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(app = %id, error = %e, url = %target_url, "failed to proxy to app");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "failed to connect to app service",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// `/ui/` → the app's declared ui path; `/ui/foo.css` keeps its tail.
fn normalize_ui_path(rest: &str, declared: &str) -> String {
    let tail = rest.strip_prefix("ui").unwrap_or("");
    let tail = tail.trim_start_matches('/');
    let base = declared.trim_end_matches('/');
    if tail.is_empty() {
        format!("{base}/")
    } else {
        format!("{base}/{tail}")
    }
}

fn authenticated_user(
    state: &AppState,
    app_id: &str,
    headers: &HeaderMap,
) -> Result<String, Response> {
    let ticket = headers
        .get(SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| read_session_cookie(headers));
    let Some(ticket) = ticket else {
        return Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "missing app session" })),
        )
            .into_response());
    };
    match verify_session_ticket(
        &state.app_session_secret,
        &ticket,
        app_id,
        now_unix(),
    ) {
        Ok((_, user)) => Ok(user),
        Err(e) => Err((
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({
                "error": "invalid app session",
                "reason": format!("{e:?}"),
            })),
        )
            .into_response()),
    }
}

fn exchange_session(
    state: &AppState,
    app_id: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let payload: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(serde_json::json!({ "error": "expected {ticket: string}" })),
            )
                .into_response();
        }
    };
    let ticket = payload
        .get("ticket")
        .and_then(|v| v.as_str())
        .or_else(|| headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()));
    let Some(ticket) = ticket else {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({ "error": "missing ticket" })),
        )
            .into_response();
    };
    let (expires_at, user) = match verify_session_ticket(
        &state.app_session_secret,
        ticket,
        app_id,
        now_unix(),
    ) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({
                    "error": "invalid app session",
                    "reason": format!("{e:?}"),
                })),
            )
                .into_response();
        }
    };
    let max_age = expires_at.saturating_sub(now_unix()).min(SESSION_TTL_SECS);
    let cookie = session_cookie(app_id, ticket, max_age);
    let Ok(cookie_value) = HeaderValue::from_str(&cookie) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({ "error": "failed to encode session cookie" })),
        )
            .into_response();
    };
    let mut response = axum::Json(serde_json::json!({ "app_id": app_id, "user": user }))
        .into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, cookie_value);
    response
}

fn build_forwarded_headers(
    headers: &HeaderMap,
    app_id: &str,
    user: Option<&str>,
    platform_url: &str,
) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        let lower = name.as_str().to_ascii_lowercase();
        if FORBIDDEN_CLIENT_HEADERS.contains(&lower.as_str()) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    if let Ok(v) = HeaderValue::from_str(app_id) {
        out.insert("x-app-id", v);
    }
    if let Some(user) = user {
        if let Ok(v) = HeaderValue::from_str(user) {
            out.insert("x-app-user", v.clone());
            // PM / Doc already read `X-Actor` as the trusted REST actor.
            out.insert("x-actor", v);
        }
    }
    out.insert("x-actor-admin", HeaderValue::from_static("false"));
    out.insert("x-app-session-verified", HeaderValue::from_static("true"));
    // The Gateway tells the app where the App → Platform API lives. The app
    // combines this with the internal service token its supervisor injected;
    // the webview itself never holds a platform credential.
    if let Ok(v) = HeaderValue::from_str(platform_url) {
        out.insert("x-app-platform-url", v);
    }
    // Belt and braces: the framework never forwards the platform bearer.
    out.remove(header::AUTHORIZATION);
    out
}

fn service_unavailable(message: &str) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": "app unavailable",
            "message": message,
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("Retry-After", HeaderValue::from_static("2"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_paths_map_to_the_declared_bundle_root() {
        assert_eq!(normalize_ui_path("ui/", "/ui/"), "/ui/");
        assert_eq!(normalize_ui_path("ui", "/ui/"), "/ui/");
        assert_eq!(normalize_ui_path("ui/app.css", "/ui/"), "/ui/app.css");
        assert_eq!(normalize_ui_path("ui/index.html", "/panel/"), "/panel/index.html");
    }

    #[test]
    fn client_identity_headers_are_stripped_and_trusted_ones_injected() {
        let mut headers = HeaderMap::new();
        headers.insert("x-actor", HeaderValue::from_static("agent:evil"));
        headers.insert("x-app-id", HeaderValue::from_static("doc"));
        headers.insert("x-actor-admin", HeaderValue::from_static("true"));
        headers.insert("authorization", HeaderValue::from_static("Bearer leaked"));
        headers.insert("x-app-session", HeaderValue::from_static("ticket"));
        let out = build_forwarded_headers(
            &headers,
            "pm",
            Some("u-alice"),
            "http://127.0.0.1:19876",
        );
        assert_eq!(out.get("x-actor").unwrap(), "u-alice");
        assert_eq!(out.get("x-app-id").unwrap(), "pm");
        assert_eq!(out.get("x-actor-admin").unwrap(), "false");
        assert!(out.get("x-app-user").is_some());
        assert!(out.get("authorization").is_none());
        assert!(out.get("cookie").is_none());
        assert!(out.get("x-app-session").is_none());
        assert_eq!(
            out.get("x-app-platform-url").unwrap(),
            "http://127.0.0.1:19876"
        );
    }
}
