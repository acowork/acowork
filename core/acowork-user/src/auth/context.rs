//! Trusted identity for the user service (ADR-084 §2 / §决策 7).
//!
//! After the split the Gateway is the **single authentication point**: its
//! `auth_middleware` verifies the access token once and `user_proxy` injects
//! the resulting identity as `X-Auth-*` headers. This service never verifies
//! a token itself — it would otherwise need the account/token machinery in
//! the request path and add a hop (ADR-084 §决策 1, rejected alternative).
//!
//! Two things make that safe:
//! - the Gateway strips any client-supplied `X-Auth-*` before injecting its
//!   own (the proxy is the only writer), and
//! - this process **only binds loopback** (enforced in `server.rs`), so no
//!   other host can reach the header surface directly.
//!
//! `local` mode has no account system: no header is injected, no identity is
//! required, and the profile routes run on the single OS user.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use acowork_core::account::Role;

use crate::error::ApiError;
use crate::state::AppState;

/// The Gateway-injected identity headers (ADR-084 §决策 7).
///
/// Re-exported from `acowork-core` rather than re-declared: the Gateway is a
/// separate binary, so two copies of these names could drift into a silent
/// wire break.
pub use acowork_core::auth::{AUTH_AS_USER_HEADER, AUTH_ROLE_HEADER, AUTH_USER_HEADER};

/// Who this request is acting as.
///
/// Carried over verbatim from the Gateway so handler authorization logic
/// (`require_admin`, `require_self_or_admin`, the avatar ownership checks)
/// is unchanged by the extraction.
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
    /// A plain user is always themselves — `as_user` is ignored unless it was
    /// granted as an admin. The Gateway enforces that at injection time and
    /// we re-check here, so a bug on either side cannot escalate.
    pub fn effective_user_id(&self) -> &str {
        match (&self.as_user, self.is_admin()) {
            (Some(u), true) => u,
            _ => &self.user_id,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self.role, Role::Admin)
    }

    /// Build from the injected headers. `None` when no identity was injected.
    ///
    /// A malformed role is treated as the least-privileged `user` rather than
    /// rejected: the Gateway always writes one of two known values, so an
    /// unknown value means something is wrong upstream, and failing closed to
    /// `user` is strictly safer than failing open.
    pub fn from_headers(headers: &axum::http::HeaderMap) -> Option<Self> {
        let user_id = header_str(headers, AUTH_USER_HEADER)?;
        let role = match header_str(headers, AUTH_ROLE_HEADER) {
            Some("admin") => Role::Admin,
            _ => Role::User,
        };
        Some(Self {
            user_id: user_id.to_string(),
            role,
            as_user: header_str(headers, AUTH_AS_USER_HEADER).map(str::to_string),
        })
    }
}

fn header_str<'a>(headers: &'a axum::http::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Paths reachable without an identity (ADR-084 §6.5).
///
/// The user service's slice of the Gateway's allowlist: what a client needs
/// *before* it can log in, plus `/health` for the supervisor probe. Logout is
/// included because a client holding an expired access token must still be
/// able to drop a dead session.
///
/// `/internal/user-profiles` is here for the same reason `/health` is: the
/// caller is the Gateway's supervisor *on this host*, not a client, and it
/// asks by **pulling** (ADR-084 §决策 4b) — so it has no identity to present
/// and no token it could have minted. Nothing is exposed that a client could
/// not already read through `GET /api/users`  (which the Gateway does
/// authenticate), and the service binds loopback only, so the reachable set is
/// the Gateway and any process already at the OS user's privilege level.
pub(crate) fn is_public_path(path: &str) -> bool {
    // The `/internal/chat-sessions/*` system surface (design 27 §4.6) joins
    // `/internal/user-profiles` for the same reason: the caller is the
    // Gateway on this host with no user identity to present. Loopback-only
    // binding is the trust boundary; `message_id` idempotency bounds replay.
    path.starts_with("/internal/chat-sessions/")
        || path == "/internal/chat-sessions"
        || matches!(
            path,
            "/health"
                | "/internal/user-profiles"
                | "/api/auth/login"
                | "/api/auth/refresh"
                | "/api/auth/logout"
                | "/api/auth/first-login"
                | "/api/auth/pair/redeem"
        )
}

/// Turn the Gateway-injected `X-Auth-*` headers into an [`AuthContext`].
///
/// Runs ahead of every route, so handlers can keep the `Extension<AuthContext>`
/// extractor they had in the Gateway.
pub async fn inject_identity(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    // `local`: the account system is off, so no identity may be asserted and
    // none is required — the profile routes address the single OS user.
    if !state.is_multi_user() {
        return next.run(req).await;
    }

    if is_public_path(req.uri().path()) {
        return next.run(req).await;
    }

    match AuthContext::from_headers(req.headers()) {
        Some(ctx) => {
            req.extensions_mut().insert(ctx);
            next.run(req).await
        }
        // The Gateway rejects unauthenticated requests before proxying, so
        // reaching here means the proxy and this service disagree. Answer 401
        // rather than letting the `Extension` extractor produce a 500.
        None => ApiError::unauthorized("missing authenticated identity").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn builds_context_from_injected_headers() {
        let ctx = AuthContext::from_headers(&headers(&[
            (AUTH_USER_HEADER, "u-1"),
            (AUTH_ROLE_HEADER, "admin"),
        ]))
        .unwrap();
        assert_eq!(ctx.user_id, "u-1");
        assert!(ctx.is_admin());
        assert_eq!(ctx.effective_user_id(), "u-1");
    }

    #[test]
    fn missing_identity_is_none() {
        assert!(AuthContext::from_headers(&headers(&[])).is_none());
        // A role without a user is still no identity.
        assert!(AuthContext::from_headers(&headers(&[(AUTH_ROLE_HEADER, "admin")])).is_none());
    }

    /// A role the Gateway would never write must not grant admin.
    #[test]
    fn unknown_role_fails_closed_to_user() {
        let ctx = AuthContext::from_headers(&headers(&[
            (AUTH_USER_HEADER, "u-1"),
            (AUTH_ROLE_HEADER, "root"),
        ]))
        .unwrap();
        assert!(!ctx.is_admin());
        assert_eq!(ctx.role, Role::User);
    }

    #[test]
    fn as_user_applies_only_to_admins() {
        let admin = AuthContext::from_headers(&headers(&[
            (AUTH_USER_HEADER, "u-1"),
            (AUTH_ROLE_HEADER, "admin"),
            (AUTH_AS_USER_HEADER, "u-2"),
        ]))
        .unwrap();
        assert_eq!(admin.effective_user_id(), "u-2");

        // A non-admin presenting `as_user` stays themselves.
        let user = AuthContext::from_headers(&headers(&[
            (AUTH_USER_HEADER, "u-1"),
            (AUTH_ROLE_HEADER, "user"),
            (AUTH_AS_USER_HEADER, "u-2"),
        ]))
        .unwrap();
        assert_eq!(user.effective_user_id(), "u-1");
    }

    #[test]
    fn public_paths_are_the_pre_login_surface() {
        for p in [
            "/health",
            "/internal/user-profiles",
            "/api/auth/login",
            "/api/auth/refresh",
            "/api/auth/logout",
            "/api/auth/first-login",
        ] {
            assert!(is_public_path(p), "{p} must be reachable pre-login");
        }
        for p in ["/api/users", "/api/auth/me", "/api/auth/change-password"] {
            assert!(!is_public_path(p), "{p} must require identity");
        }
    }
}
