//! Authentication middleware (ADR-076 §决策 3).
//!
//! Under `AUTH_MODE=multi_user`, every `/api/*` request must carry
//! `Authorization: Bearer <access_token>`; the verified identity is
//! injected as [`AuthContext`] for downstream handlers. Under
//! `AUTH_MODE=local` there is no account system and this middleware is a
//! pass-through — the legacy bearer token keeps doing whatever it did
//! (ADR-076 §决策 12: local is a no-op for the account system).
//!
//! It is layered *inside* CORS (so a 401 still carries
//! `Access-Control-Allow-Origin` and the browser can read the error) and
//! *outside* every route (so a new route cannot forget to opt in).

use axum::{
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Json, Response},
};

use acowork_core::account::Role;
use acowork_core::auth::TokenKind;

use serde_json::json;

use crate::auth::token::now_unix;
use crate::http::routes::AppState;

/// The authenticated identity of a request (ADR-076 §决策 3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthContext {
    pub user_id: String,
    pub role: Role,
    /// ADR-076 §决策 4: admin-only "view as user X" (read-only).
    pub as_user: Option<String>,
}

impl AuthContext {
    /// Whose data this request may touch.
    ///
    /// A plain user is always themselves — `as_user` is ignored unless it
    /// was granted as an admin, which is enforced at construction.
    pub fn effective_user_id(&self) -> &str {
        match (&self.as_user, self.is_admin()) {
            (Some(u), true) => u,
            _ => &self.user_id,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self.role, Role::Admin)
    }
}

/// Trusted-identity headers injected for the user service (ADR-084 §决策 7).
///
/// The wire contract lives in `acowork-core` so this side and the service
/// cannot drift apart.
pub use acowork_core::auth::{AUTH_AS_USER_HEADER, AUTH_ROLE_HEADER, AUTH_USER_HEADER};

/// The Gateway-owned session-scope header consumed by the Runtime
/// (ADR-076 §决策 4).
///
/// **The Gateway is its only trusted writer.** The reverse proxy forwards
/// every inbound header verbatim (`proxy_to_runtime_with_method`), so a
/// client-supplied `x-user-id` would otherwise reach the Runtime first and
/// claim someone else's sessions. The middleware therefore *removes* it on
/// every request and re-inserts the token-derived value only after the
/// caller has been authenticated.
pub const USER_SCOPE_HEADER: &str = "x-user-id";

/// Scope value meaning "no filtering" — the administrator view.
pub const SCOPE_ALL: &str = "*";

/// ADR-076 §决策 10: the trusted REST actor forwarded to PM / Doc.
///
/// Under `multi_user` this is the authenticated identity
/// ([`AuthContext::effective_user_id`]); under `local` the middleware is a
/// no-op and inserts no [`AuthContext`], so the caller falls back to the
/// legacy `human` constant (§决策 12 — the whole account system is off).
pub const LOCAL_HUMAN_ACTOR: &str = "human";

/// Resolve the REST actor to inject on the PM / Doc reverse-proxy paths.
pub fn trusted_rest_actor(auth: Option<&AuthContext>) -> String {
    auth.map(|c| c.effective_user_id().to_string())
        .unwrap_or_else(|| LOCAL_HUMAN_ACTOR.to_string())
}

/// Derive the Runtime-side session scope from an authenticated identity.
///
/// A plain user is always themselves; `as_user` applies only to an
/// administrator (already enforced at `AuthContext` construction).
fn scope_value(ctx: &AuthContext) -> String {
    match (ctx.as_user.as_deref(), ctx.is_admin()) {
        (Some(u), true) => u.to_string(),
        (_, true) => SCOPE_ALL.to_string(),
        _ => ctx.user_id.clone(),
    }
}

/// Whether an `as_user` value is safe to put in an HTTP header.
///
/// Header values reject control characters, and a value that fails to
/// build must never degrade into "no filtering" — so an out-of-shape
/// `as_user` is rejected at the boundary instead of being coerced.
fn is_valid_scope_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
/// Paths reachable without a login token (ADR-076 §决策 3).
///
/// Kept deliberately tiny: what a client needs *before* it can log in —
/// liveness/readiness, the bootstrap probe, the login and refresh calls,
/// the first-login call (a freshly-created account has no password yet),
/// the pairing-redeem call (a scanned phone holds only a one-time QR token,
/// not a pair — see 方案③),
/// and logout (a client with an expired access token must still be able to
/// drop a dead session). The legacy Gateway token file is never exposed.
fn is_public_path(path: &str) -> bool {
    if matches!(
        path,
        "/health"
            | "/api/status"
            | "/api/bootstrap"
            | "/api/auth/login"
            | "/api/auth/refresh"
            | "/api/auth/logout"
            | "/api/auth/first-login"
            | "/api/auth/pair/redeem"
    ) {
        return true;
    }
    // `/api/providers/{id}/logo` — a public brand mark (a models.dev SVG), no
    // user data in it. It has to be reachable without a token because the
    // Desktop renders it via `<img src>`, and an `<img>` cannot carry an
    // `Authorization` header. Without this the model menu and the Harness
    // provider list get a 401 per row under `AUTH_MODE=multi_user`.
    //
    // Exactly two segments after the prefix, and the second must be `logo`:
    // a looser suffix test would also match
    // `/api/providers/{id}/keys/{account_id}` for an account literally named
    // "logo", publishing that provider's masked keys.
    let Some(rest) = path.strip_prefix("/api/providers/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let (Some(id), Some("logo"), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !id.is_empty() && !id.contains('?')
}

/// Bearer-token gate for `AUTH_MODE=multi_user` (ADR-076 §决策 3).
pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // Local mode: no account system, nothing to enforce.
    if !state.auth_mode.is_multi_user() {
        // The account system is off, so no identity may be asserted. Drop
        // any client-supplied scope header rather than letting it reach
        // the Runtime unvetted (ADR-076 §决策 4).
        req.headers_mut().remove(USER_SCOPE_HEADER);
        return next.run(req).await;
    }

    // Never forward a client-asserted identity: the header is re-derived
    // from the verified token below, and only for authenticated callers.
    req.headers_mut().remove(USER_SCOPE_HEADER);

    // CORS preflight must never be gated — the browser sends no
    // `Authorization` on `OPTIONS`.
    if req.method() == Method::OPTIONS || is_public_path(req.uri().path()) {
        return next.run(req).await;
    }

    // ADR-055 Phase 5a machine identity: a Node presenting a valid
    // `X-ACowork-Node-Token` is a trusted internal actor. The middleware
    // accepts it here instead of forcing the Node to obtain a
    // user-level Bearer token (which it has no use for), and the
    // per-route `node_tokens.any_token_matches` check still applies as
    // a defense-in-depth gate at the route entry. The store is consulted
    // regardless of `mqtt.auth_enabled`: enrollment mints a node token in
    // both modes (`decide_enroll` only makes the *enrollment* token
    // mandatory under auth), so an enrolled Node authenticates over HTTP
    // even when the broker itself is permissive. With no enrollment ever
    // performed the store is empty and the header simply falls through.
    if let Some(node_token) = req
        .headers()
        .get("X-ACowork-Node-Token")
        .and_then(|v| v.to_str().ok())
    {
        let broker_auth = state.gateway_state.read().await.mqtt_broker_auth.clone();
        let authorized = broker_auth
            .as_ref()
            .map(|auth| {
                auth.node_tokens
                    .lock()
                    .map(|store| store.any_token_matches(node_token))
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if authorized {
            return next.run(req).await;
        }
        // Header was supplied but the token is unknown — fall through to
        // the user-token path so a misconfigured Node still gets a
        // meaningful 401 rather than a silent bypass.
    }

    // Internal services the Gateway itself spawned (`acowork-pm` today) call
    // back into Gateway HTTP over loopback. They have no user account and no
    // use for a user-level Bearer token, but under `multi_user` every
    // `/api/*` route is gated below — so without this branch they can never
    // reach the API at all. `acowork-pm` reads it from
    // `ACOWORK_PM_GATEWAY_TOKEN`, which the supervisor injects at spawn.
    //
    // Shape and intent mirror the `X-ACowork-Node-Token` branch above: the
    // middleware proves *who* the caller is, and deliberately does **not**
    // decide *what* it may read. Authorization stays in each route (and in
    // the calling service), so a new Gateway endpoint PM needs later needs no
    // change here — keeping a path allowlist in this middleware would freeze
    // today's needs into the auth layer and make every new requirement an
    // auth-layer edit.
    if let Some(token) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        let expected = state
            .gateway_state
            .read()
            .await
            .internal_service_token
            .clone();
        if let Some(expected) = expected
            && crate::mqtt::enrollment::constant_time_eq(
                expected.as_bytes(),
                token.as_bytes(),
            )
        {
            return next.run(req).await;
        }
        // Not the service token — fall through to the user-token path so the
        // caller gets a meaningful 401 instead of a silent bypass.
    }

    // ADR-084 §决策 2: the account system lives in the user service now, so
    // the Gateway verifies with the Ed25519 public key that service publishes
    // and never touches the account store. The supervisor loads it once the
    // service is ready; until then nothing can be verified, and the only safe
    // answer is "not ready" — answering 401 would tell a client with a
    // perfectly good token that it is bad, and letting the request through
    // would authenticate nobody.
    let verifier = state.gateway_state.read().await.user_verifier.clone();
    let Some(verifier) = verifier else {
        return service_unavailable(
            "the user service is not ready yet; authentication is unavailable",
        );
    };

    let header_token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty());
    // ADR-089 D2: browser `WebSocket` cannot set headers, so the LSP
    // relay WS bridge authenticates with `?access_token=` — accepted
    // ONLY for upgrade requests (a plain HTTP caller must use the
    // header, keeping the token out of URLs, caches and logs for
    // everything else). `log_request_origin` redacts it regardless.
    let is_ws_upgrade = req
        .headers()
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_ascii_lowercase().contains("upgrade"))
        && req
            .headers()
            .get(header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let query_token: Option<String> = if header_token.is_none() && is_ws_upgrade {
        query_param(req.uri().query(), "access_token")
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    } else {
        None
    };
    let token = header_token.or(query_token.as_deref());

    let Some(token) = token else {
        return unauthorized("missing bearer token");
    };

    // Same claims -> identity mapping `AuthService::verify_access` performed
    // in the Gateway before the extraction (ADR-084 §决策 2): an unknown role
    // claim is the least-privileged `user`.
    let (user_id, role) = match verifier.verify_kind(token, TokenKind::Access, now_unix()) {
        Ok(claims) => (
            claims.sub,
            Role::from_claim(claims.role.as_deref().unwrap_or("user")),
        ),
        Err(e) => return unauthorized(&e.to_string()),
    };

    // ADR-076 §决策 4: `as_user` is an admin-only read view. A non-admin
    // presenting it anywhere is a client bug or an attack — reject rather
    // than silently ignore, so it can never look like it worked.
    //
    // The read-only half is enforced here, not downstream: writes are
    // authorized against the injected scope, and `as_user` *becomes* that
    // scope — so without this guard an admin could DELETE a session while
    // the Runtime recorded the owner as the impersonated user. Admins can
    // reach the same session anyway; the point is that the audit trail must
    // not name someone who never made the request (§9 开放问题 5).
    let as_user = query_param(req.uri().query(), "as_user");
    if let Some(u) = &as_user {
        if !matches!(req.method(), &Method::GET | &Method::HEAD) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "forbidden",
                    "code": "not_authorized",
                    "message": "as_user is a read-only view and cannot be used on a write request",
                })),
            )
                .into_response();
        }
        if !matches!(role, Role::Admin) || !is_valid_scope_id(u) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "error": "forbidden",
                    "code": "not_authorized",
                    "message": "as_user requires an administrator token and a well-formed user id",
                })),
            )
                .into_response();
        }
    }

    let ctx = AuthContext {
        user_id,
        role,
        as_user,
    };

    // ADR-076 §决策 4: hand the Runtime the scope the Gateway derived
    // from the token — the only value it will ever see, because the same
    // header was removed from the inbound request above.
    let scope = scope_value(&ctx);
    let Ok(scope_header) = HeaderValue::from_str(&scope) else {
        // Unreachable for a validated id; fail closed regardless.
        return unauthorized("invalid session scope");
    };
    req.headers_mut().insert(USER_SCOPE_HEADER, scope_header);

    req.extensions_mut().insert(ctx);
    next.run(req).await
}

/// The `/api/*` contract for "the user service is not up yet": 503 with
/// `Retry-After`, matching what `user_proxy` returns so the Desktop's
/// `with503Retry` handles both the same way.
fn service_unavailable(message: &str) -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": "user service not ready", "message": message })),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("2"));
    response
}

fn unauthorized(message: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        Json(json!({ "error": "unauthorized", "message": message })),
    )
        .into_response()
}

/// Percent-decoded-enough lookup of one query parameter.
///
/// Only used for `as_user`, which is a UUID: no percent-encoding is ever
/// expected, and a malformed value simply fails to match an account later.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn ctx(user: &str, role: Role, as_user: Option<&str>) -> AuthContext {
        AuthContext {
            user_id: user.into(),
            role,
            as_user: as_user.map(str::to_string),
        }
    }

    #[test]
    fn plain_user_is_always_self() {
        let c = ctx("u-1", Role::User, None);
        assert_eq!(c.effective_user_id(), "u-1");
        // as_user is rejected before construction for non-admins; even if
        // one slipped through, it must not take effect.
        let c = ctx("u-1", Role::User, Some("u-2"));
        assert_eq!(c.effective_user_id(), "u-1");
    }

    #[test]
    fn admin_as_user_switches_the_effective_identity() {
        let c = ctx("admin-1", Role::Admin, Some("u-2"));
        assert_eq!(c.effective_user_id(), "u-2");
        assert!(c.is_admin());
    }

    #[test]
    fn public_paths_are_the_pre_login_surface() {
        for p in [
            "/health",
            "/api/status",
            "/api/bootstrap",
            "/api/auth/login",
            "/api/auth/refresh",
            "/api/auth/logout",
            "/api/auth/first-login",
        ] {
            assert!(is_public_path(p), "{p} should be public");
        }
        for p in [
            "/api/auth/me",
            "/api/auth/change-password",
            "/api/agents",
            "/api/users",
            "/api/agents/foo/sessions",
        ] {
            assert!(!is_public_path(p), "{p} must require auth");
        }
    }

    #[test]
    fn provider_logo_is_public_but_the_rest_of_the_provider_api_is_not() {
        // The Desktop renders provider marks with `<img src>`, which cannot
        // carry an Authorization header — so this one route has to be
        // reachable unauthenticated or every row 401s under multi_user mode.
        assert!(is_public_path("/api/providers/anthropic/logo"));
        assert!(is_public_path("/api/providers/custom-my-proxy/logo"));

        // The suffix match must not leak the rest of the provider API. These
        // are the routes that return masked keys, base URLs and account ids.
        for p in [
            "/api/providers",
            "/api/providers/anthropic",
            "/api/providers/anthropic/keys/abc-123",
            // A key id that merely ends in "logo" is not a logo request.
            "/api/providers/anthropic/keys/logo",
        ] {
            assert!(!is_public_path(p), "{p} must require auth");
        }
    }

    #[test]
    fn scope_value_maps_identity_to_runtime_scope() {
        // Plain user → themselves, even if as_user somehow survives.
        assert_eq!(scope_value(&ctx("u-1", Role::User, Some("u-2"))), "u-1");
        // Admin without as_user → the explicit "no filtering" sentinel.
        assert_eq!(scope_value(&ctx("a-1", Role::Admin, None)), SCOPE_ALL);
        // Admin with as_user → that user.
        assert_eq!(scope_value(&ctx("a-1", Role::Admin, Some("u-2"))), "u-2");
    }

    #[test]
    fn scope_ids_are_validated_before_becoming_headers() {
        assert!(is_valid_scope_id("4f8a1c2e-0000-4000-8000-000000000001"));
        assert!(is_valid_scope_id("u_alice-1"));
        // Rejected: empty, oversized, control chars, header-injection
        // shaped, non-ascii.
        assert!(!is_valid_scope_id(""));
        assert!(!is_valid_scope_id(&"a".repeat(65)));
        assert!(!is_valid_scope_id("u-1\r\nX-Admin: 1"));
        assert!(!is_valid_scope_id("u 1"));
        assert!(!is_valid_scope_id("用户一"));
    }

    /// ADR-064 Phase 3 regression: `acowork-pm` is spawned by the Gateway and
    /// calls back over loopback to read the agent directory. Under `multi_user`
    /// every `/api/*` route is gated, so without the internal-service token
    /// branch PM's `agent_exists` 401s and every *add-member* is rejected with
    /// a misleading 400 "agent not found". The branch must open for the
    /// service token, stay shut for everyone else, and — because it proves
    /// identity only — must not be a path allowlist.
    #[tokio::test]
    async fn internal_service_token_authenticates_but_only_that_token_does() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::{Router, routing::get};
        use tower::ServiceExt;

        let dir = std::env::temp_dir().join(format!("acowork-mw-svc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        const SERVICE_TOKEN: &str = "svc-token-abc";
        let app = |token: Option<&'static str>| {
            let mut gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
            gw.user_verifier = Some(crate::http::test_support::verifier());
            gw.internal_service_token = token.map(str::to_string);
            let mut st = crate::http::routes::AppState::new(
                Arc::new(tokio::sync::RwLock::new(gw)),
                Arc::new(crate::http::auth::HttpAuth::new(false)),
            );
            st.auth_mode = crate::auth::AuthMode::MultiUser;
            Router::new()
                .route("/api/agents", get(|| async { "ok" }))
                .layer(axum::middleware::from_fn_with_state(st, auth_middleware))
        };
        let call = |bearer: &str| {
            Request::builder()
                .uri("/api/agents")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap()
        };

        // The service token authenticates: PM can read the agent directory.
        let resp = app(Some(SERVICE_TOKEN))
            .oneshot(call(SERVICE_TOKEN))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Identity only, no path allowlist: a *new* endpoint PM needs later
        // must not require an auth-layer edit. `GET /api/agents` and whatever
        // else the caller reaches are decided by the route, not here.
        let any_path = |bearer: &str| {
            Request::builder()
                .uri("/api/some/future/endpoint")
                .header("authorization", format!("Bearer {bearer}"))
                .body(Body::empty())
                .unwrap()
        };
        let resp = app(Some(SERVICE_TOKEN))
            .oneshot(any_path(SERVICE_TOKEN))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "the gate must authenticate the service and then let the router answer, \
             not 404 it as an unknown path (which would mean a path allowlist exists)"
        );

        // A near-miss token is NOT the service: it must fall through to the
        // user-token path and get a real 401, not a silent bypass.
        let resp = app(Some(SERVICE_TOKEN))
            .oneshot(call("svc-token-abd"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // No service spawned (`internal_service_token = None`) → fail closed,
        // even for the token value that would otherwise have matched.
        let resp = app(None).oneshot(call(SERVICE_TOKEN)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// The whole point of ADR-076 §决策 4's transport: a client-asserted
    /// `x-user-id` must never survive to the Runtime, and an authenticated
    /// caller must get exactly one, derived from the token.
    #[tokio::test]
    async fn middleware_strips_client_scope_and_injects_the_token_scope() {
        use axum::body::Body;
        use axum::http::Request;
        use axum::{Router, routing::get};
        use tower::ServiceExt;

        let dir = std::env::temp_dir().join(format!("acowork-mw-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // ADR-084: no account store in the Gateway — a token minted with
        // the fixture key is what the gate now verifies.
        let admin_token = crate::http::test_support::access_token("root", "admin");

        let state = |multi: bool| {
            let mut gw = crate::gateway::state::GatewayState::new(&dir.to_string_lossy());
            if multi {
                gw.user_verifier = Some(crate::http::test_support::verifier());
            }
            let mut st = crate::http::routes::AppState::new(
                Arc::new(tokio::sync::RwLock::new(gw)),
                Arc::new(crate::http::auth::HttpAuth::new(false)),
            );
            if multi {
                st.auth_mode = crate::auth::AuthMode::MultiUser;
            }
            st
        };

        // Terminal handler: echo the scope header the Runtime would see.
        async fn echo_scope(req: Request<Body>) -> String {
            req.headers()
                .get(USER_SCOPE_HEADER)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("<none>")
                .to_string()
        }

        let app = |multi: bool| {
            Router::new()
                .route("/api/agents", get(echo_scope))
                // Write endpoint, for the read-only guard on `as_user`.
                .route("/api/agents/{sid}", axum::routing::delete(echo_scope))
                .layer(axum::middleware::from_fn_with_state(
                    state(multi),
                    auth_middleware,
                ))
        };
        let call = |uri: &str, token: Option<&str>| {
            let mut b = Request::builder()
                .uri(uri)
                .header(USER_SCOPE_HEADER, "u-victim");
            if let Some(t) = token {
                b = b.header("authorization", format!("Bearer {t}"));
            }
            b.body(Body::empty()).unwrap()
        };
        let body_of = |resp: Response| async move {
            axum::body::to_bytes(resp.into_body(), 1 << 20)
                .await
                .unwrap()
        };

        // Local mode: forged header is dropped, nothing injected.
        let resp = app(false).oneshot(call("/api/agents", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(&body_of(resp).await[..], b"<none>");

        // multi_user, no token: 401 (nothing downstream).
        let resp = app(true).oneshot(call("/api/agents", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // multi_user, valid admin token: the forged "u-victim" is gone and
        // the token-derived scope ("*" = no filtering) is what the Runtime
        // sees.
        let resp = app(true)
            .oneshot(call("/api/agents", Some(&admin_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(&body_of(resp).await[..], SCOPE_ALL.as_bytes());

        // as_user narrows an admin to one user's scope...
        let resp = app(true)
            .oneshot(call("/api/agents?as_user=u-alice", Some(&admin_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(&body_of(resp).await[..], b"u-alice");

        // ...but a scope value that could not be a header fails closed.
        let resp = app(true)
            .oneshot(call("/api/agents?as_user=bad%20value", Some(&admin_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // §9 开放问题 5: `as_user` is read-only. On a write it must be
        // rejected outright — forwarding it would make the Runtime
        // authorize (and later attribute) the write to the impersonated
        // user, so the audit trail would name someone who never asked.
        let write = |uri: &str, token: Option<&str>| {
            let mut b = Request::builder()
                .method("DELETE")
                .uri(uri)
                .header(USER_SCOPE_HEADER, "u-victim");
            if let Some(t) = token {
                b = b.header("authorization", format!("Bearer {t}"));
            }
            b.body(Body::empty()).unwrap()
        };
        let resp = app(true)
            .oneshot(write(
                "/api/agents/sess-1?as_user=u-alice",
                Some(&admin_token),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // The same write without `as_user` still works as the admin.
        let resp = app(true)
            .oneshot(write("/api/agents/sess-1", Some(&admin_token)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(&body_of(resp).await[..], SCOPE_ALL.as_bytes());
    }
}
