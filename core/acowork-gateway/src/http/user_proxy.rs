//! acowork-user service HTTP reverse proxy (ADR-084 §决策 1/2/7).
//!
//! The Gateway reverse-proxies the whole user domain to the standalone
//! `acowork-user` process (`127.0.0.1:{user_port}/*`), reusing
//! [`crate::http::proxy`]'s transparent proxy mode (ADR-033).
//!
//! ## Path mapping
//!
//! | Gateway public | user service internal |
//! |----------------|-----------------------|
//! | `/api/auth/login` | `/api/auth/login` |
//! | `/api/users/{id}` | `/api/users/{id}` |
//! | `/api/user/avatar-config` | `/api/user/avatar-config` |
//!
//! Unlike the doc proxy, **the prefix is not stripped**: the user service's
//! internal routes are byte-identical to the Gateway's public ones, so the
//! Desktop sees the same URLs it always did and the handlers needed no path
//! changes (ADR-084 §3 goal 4). The handler therefore forwards the request
//! URI verbatim rather than reconstructing it from a captured path segment.
//!
//! ## Not-ready semantics
//!
//! When the service is not started / restarting (`user_process` is `None`)
//! the proxy returns **503** with `Retry-After: 2` — the Desktop
//! `with503Retry` backs off per that header (same contract as `/api/pm/*`
//! and `/api/doc/*`).
//!
//! ## Identity (ADR-084 §决策 7)
//!
//! The Gateway is the **single authentication point**: `auth_middleware`
//! verifies the access token with the service's Ed25519 public key and this
//! proxy injects the resulting identity as `X-Auth-*`. The service trusts
//! those headers and binds loopback only.
//!
//! Two headers are therefore handled specially, and both matter:
//!
//! - every client-supplied `X-Auth-*` is **dropped** — the proxy is the only
//!   trusted writer, and this is what stops a peer from claiming another
//!   user's account (the same reasoning as `x-user-id` at the Runtime);
//! - `Authorization` is **dropped** as well. The service never reads it: it
//!   authenticates from the injected headers. Forwarding it would hand a
//!   valid access token to a process with no use for it, so it does not
//!   travel. Credentials the *client* must supply (login, refresh) are in
//!   the request body, not that header.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;

use crate::http::auth_middleware::AuthContext;
use crate::http::proxy::{is_hop_by_hop_header, runtime_http_client};
use crate::http::routes::AppState;

/// The user service's public surface, all of it proxy-owned.
///
/// One pattern per prefix *and* one for the bare prefix: in axum
/// `/api/users/{*rest}` does not match `/api/users`, which is the list
/// endpoint the Desktop polls most.
pub fn user_proxy_routes() -> Router<AppState> {
    let mut router = Router::new();
    for prefix in [
        "/api/auth",
        "/api/users",
        "/api/user",
        // Design 27: the Chat session store lives in the user service;
        // the Gateway only forwards (the `/internal/*` system surface is
        // deliberately NOT proxied — it is Gateway-to-user-service only).
        "/api/chat-sessions",
    ] {
        router = router
            .route(prefix, any(user_proxy_handler))
            .route(&format!("{prefix}/{{*rest}}"), any(user_proxy_handler));
    }
    router
}

/// Reverse-proxy any user-domain request to the standalone service.
///
/// Transparent proxy (RFC 7230 §2.3): forwards method, path, query, body and
/// all non-hop-by-hop headers (minus the credential + identity headers noted
/// in the module docs), injecting the verified identity. Returns 503 when the
/// service process is not ready.
async fn user_proxy_handler(
    State(state): State<AppState>,
    uri: Uri,
    headers: HeaderMap,
    method: Method,
    auth: Option<Extension<AuthContext>>,
    body: Bytes,
) -> Response {
    let (port, trusted_headers) = {
        let gw = state.gateway_state.read().await;
        let port = gw.user_process.as_ref().map(|p| p.port);
        (port, build_trusted_headers(&headers, auth.as_ref().map(|Extension(c)| c)))
    };

    let Some(port) = port else {
        let mut response = (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "error": "user service not ready",
                "message": "The user service process has not started yet (or is restarting). \
                            Retry shortly.",
            })),
        )
            .into_response();
        response
            .headers_mut()
            .insert("Retry-After", HeaderValue::from_static("2"));
        return response;
    };

    // The prefix is preserved, so the public URI *is* the target path.
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    let target_url = format!("http://127.0.0.1:{port}{path_and_query}");

    tracing::debug!(port, target_url = %target_url, "Reverse-proxying to user service");

    let client = runtime_http_client();
    let mut request = client.request(method, &target_url);

    // Forward the injected trusted headers (RFC 7230 §6.1 strips hop-by-hop).
    for (name, value) in trusted_headers.iter() {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body.to_vec());
    }

    match request.send().await {
        Ok(response) => {
            let status = StatusCode::from_u16(response.status().as_u16())
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            // Strip hop-by-hop headers (RFC 7230 §6.1) exactly as the
            // request path above does. The response side was previously a
            // blind copy of every upstream header, which forwarded
            // `transfer-encoding: chunked` onto a body that `bytes()` has
            // already de-chunked. A streamed attachment download (every
            // `/files/...` response uses `Body::from_stream`, so it has no
            // content-length) then reached the browser with a chunked
            // encoding header that no longer described the body, and fetch
            // rejected it outright as `TypeError: Failed to fetch` — a 200
            // in the Gateway log and an empty tile in the UI.
            // `content-length` is dropped for the same reason: reqwest
            // recomputes it from the buffered body.
            let resp_headers = forwardable_response_headers(response.headers());
            let body = response.bytes().await.unwrap_or_default();
            let mut response_builder = Response::builder().status(status);
            *response_builder.headers_mut().unwrap() = resp_headers;
            response_builder
                .body(axum::body::Body::from(body))
                .unwrap_or_else(|_| {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to build proxy response",
                    )
                        .into_response()
                })
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %target_url, "Failed to proxy to user service");
            (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({
                    "error": "Failed to connect to user service",
                    "detail": e.to_string(),
                })),
            )
                .into_response()
        }
    }
}

/// Build the forwarding headers: strip what the service must not see, inject
/// the verified identity.
///
/// `auth` is `None` on a public path (login / refresh / logout / first-login)
/// — the middleware injects no identity there, so nothing is forwarded and
/// the service answers on its own public-path rules.
fn build_trusted_headers(headers: &HeaderMap, auth: Option<&AuthContext>) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if is_hop_by_hop_header(name) {
            continue;
        }
        // The proxy is the only trusted writer of the identity headers, and
        // the access token has no consumer downstream (see module docs).
        if name.as_str().starts_with("x-auth-") || name == axum::http::header::AUTHORIZATION {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }

    if let Some(ctx) = auth {
        if let Ok(v) = HeaderValue::from_str(&ctx.user_id) {
            out.insert(crate::http::auth_middleware::AUTH_USER_HEADER, v);
        }
        let role = if ctx.is_admin() { "admin" } else { "user" };
        out.insert(
            crate::http::auth_middleware::AUTH_ROLE_HEADER,
            HeaderValue::from_static(role),
        );
        // ADR-076 §决策 4: validated (admin-only, read-only) in the
        // middleware; passed through so the service can re-check.
        if let Some(as_user) = &ctx.as_user
            && let Ok(v) = HeaderValue::from_str(as_user)
        {
            out.insert(crate::http::auth_middleware::AUTH_AS_USER_HEADER, v);
        }
    }

    out
}

/// Copy upstream response headers, dropping the ones that describe *this*
/// hop rather than the message.
///
/// A reverse proxy must strip hop-by-hop headers in **both** directions
/// (RFC 7230 §6.1). The request direction has always done so; the response
/// direction used to copy everything, which forwarded
/// `transfer-encoding: chunked` from a streamed upstream onto a body that
/// `bytes()` had already de-chunked. Browsers reject that response outright
/// (`TypeError: Failed to fetch`) even though it logged as a 200 — the
/// failure that showed up as a missing attachment thumbnail.
///
/// `content-length` is dropped for the same reason
/// [`is_hop_by_hop_header`] excludes it: reqwest recomputes it from the
/// buffered body.
fn forwardable_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in upstream.iter() {
        if !is_hop_by_hop_header(name) {
            out.insert(name.clone(), value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::Router;
    use axum::body::{Body, to_bytes};
    use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
    use axum::response::Response;
    use axum::routing::get;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    use acowork_core::account::Role;

    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use super::{build_trusted_headers, forwardable_response_headers, AuthContext};
    use crate::http::auth_middleware::{AUTH_AS_USER_HEADER, AUTH_ROLE_HEADER, AUTH_USER_HEADER};
    use crate::http::routes::{AppState, build_router};
    use crate::lifecycle::user_supervisor::UserProcessState;

    fn temp_dir() -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-userproxy-{}-{}",
            std::process::id(),
            unique
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state(dir: &std::path::Path, port: Option<u16>) -> AppState {
        let mut gw = GatewayState::new(&dir.to_string_lossy());
        gw.config = Some(crate::config::GatewayConfig {
            data_dir: dir.to_string_lossy().to_string(),
            ..Default::default()
        });
        gw.user_process = port.map(|p| UserProcessState {
            pid: 0,
            port: p,
            ready: true,
        });
        AppState::new(Arc::new(RwLock::new(gw)), Arc::new(HttpAuth::new(false)))
    }

    /// A stand-in for `acowork-user`: echoes the identity headers it received
    /// so a test can assert on what the proxy injected.
    async fn fake_user_service() -> (u16, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route(
                "/api/users",
                get(|headers: HeaderMap| async move {
                    let get =
                        |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("");
                    axum::Json(serde_json::json!({
                        "user": get(AUTH_USER_HEADER),
                        "role": get(AUTH_ROLE_HEADER),
                        "as_user": get(AUTH_AS_USER_HEADER),
                        "authorization": headers.contains_key(axum::http::header::AUTHORIZATION),
                        "echo": headers
                            .get("x-echo")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or(""),
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, handle)
    }

    async fn body_json(resp: Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn not_ready_returns_503_with_retry_after() {
        let dir = temp_dir();
        let router = build_router(state(&dir, None));

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(resp.headers().get("retry-after").unwrap(), "2");
        let body = body_json(resp).await;
        assert_eq!(body["error"], "user service not ready");
    }

    /// The proxy forwards the public URI verbatim (no `/api/user` stripping,
    /// unlike the doc proxy) and drops the access token on the way.
    #[tokio::test]
    async fn forwards_path_verbatim_and_strips_the_access_token() {
        let dir = temp_dir();
        let (port, _svc) = fake_user_service().await;
        let router = build_router(state(&dir, Some(port)));

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header("authorization", "Bearer some.access.token")
                    .header("x-echo", "passthrough")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body["authorization"], false,
            "the access token must not reach a process that cannot use it"
        );
        assert_eq!(
            body["echo"], "passthrough",
            "unrelated headers still pass through"
        );
    }

    /// ADR-084 §决策 7: the proxy is the only trusted writer of `X-Auth-*`.
    /// A client-supplied copy must never survive, authenticated or not.
    #[tokio::test]
    async fn client_supplied_identity_headers_are_discarded() {
        let dir = temp_dir();
        let (port, _svc) = fake_user_service().await;
        let router = build_router(state(&dir, Some(port)));

        // No token → public path → no identity injected at all, and the
        // forged headers are gone.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .header("x-auth-user", "attacker-chosen-id")
                    .header("x-auth-role", "admin")
                    .header("x-auth-as-user", "victim")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(body["user"], "");
        assert_eq!(body["role"], "");
        assert_eq!(body["as_user"], "");
    }

    /// `build_trusted_headers` is the whole identity contract, so assert it
    /// directly rather than only through a fake service.
    #[test]
    fn trusted_headers_carry_admin_and_view_as() {
        let mut headers = HeaderMap::new();
        headers.insert("x-auth-user", HeaderValue::from_static("forged"));
        headers.insert("x-auth-role", HeaderValue::from_static("admin"));
        headers.insert("authorization", HeaderValue::from_static("Bearer t"));
        headers.insert("x-echo", HeaderValue::from_static("keep"));

        let ctx = AuthContext {
            user_id: "u-real".into(),
            role: Role::Admin,
            as_user: Some("u-viewed".into()),
        };
        let out = build_trusted_headers(&headers, Some(&ctx));

        assert_eq!(out.get(AUTH_USER_HEADER).unwrap(), "u-real");
        assert_eq!(out.get(AUTH_ROLE_HEADER).unwrap(), "admin");
        assert_eq!(out.get(AUTH_AS_USER_HEADER).unwrap(), "u-viewed");
        assert!(!out.contains_key("authorization"));
        assert_eq!(out.get("x-echo").unwrap(), "keep");

        // A plain user never gets `as_user` forwarded, even if the context
        // somehow carries one (the middleware rejects that case, and the
        // service re-checks — but the proxy must not be the weak link).
        let ctx = AuthContext {
            user_id: "u-real".into(),
            role: Role::User,
            as_user: None,
        };
        let out = build_trusted_headers(&headers, Some(&ctx));
        assert_eq!(out.get(AUTH_ROLE_HEADER).unwrap(), "user");
        assert!(!out.contains_key(AUTH_AS_USER_HEADER));
    }

    #[test]
    fn public_paths_get_no_identity() {
        let headers = HeaderMap::new();
        let out = build_trusted_headers(&headers, None);
        assert!(!out.contains_key(AUTH_USER_HEADER));
        assert!(!out.contains_key(AUTH_ROLE_HEADER));
    }

    #[test]
    fn a_chunked_upstream_response_does_not_forward_transfer_encoding() {
        // The bug this locks: a streamed attachment download (`Body::from_stream`
        // in the user service, so no content-length) came back as
        // `transfer-encoding: chunked` on an already-dechunked body. The
        // browser threw `TypeError: Failed to fetch` while the Gateway log
        // showed a 200, so the chat row silently lost its thumbnail.
        let mut upstream = HeaderMap::new();
        upstream.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        upstream.insert("content-length", HeaderValue::from_static("17"));
        upstream.insert("content-type", HeaderValue::from_static("image/jpeg"));
        upstream.insert(
            "content-disposition",
            HeaderValue::from_static("attachment; filename=\"a.jpg\""),
        );

        let out = forwardable_response_headers(&upstream);
        assert!(!out.contains_key("transfer-encoding"));
        // Recomputed by hyper from the buffered body.
        assert!(!out.contains_key("content-length"));
        // The headers that describe the message itself must survive.
        assert_eq!(out.get("content-type").unwrap(), "image/jpeg");
        assert!(out.contains_key("content-disposition"));
    }
}
