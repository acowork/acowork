//! Test-only scaffolding.
//!
//! The handlers in [`crate::http`] were moved over from the Gateway together
//! with their tests. Those tests drive the real router with a real
//! `Authorization: Bearer <access_token>` and assert on the HTTP response —
//! which is still exactly how the deployed request arrives, except that the
//! Gateway now performs the token check and hands the identity down as
//! `X-Auth-*` headers (ADR-084 §决策 7).
//!
//! [`gateway_identity_shim`] reproduces that Gateway step *in front of* the
//! service's own [`crate::auth::context::inject_identity`], so the ported
//! tests kept their bodies byte-for-byte and now cover the real two-stage
//! path: verify token → inject headers → build identity.
//!
//! It is deliberately a re-implementation rather than a shared helper: from
//! M3 the Gateway's own `auth_middleware` does this for real, and a shared
//! abstraction would only have to be undone again. What this file must stay
//! honest about is the *contract* (which headers, which status codes on
//! failure) — those are what the Gateway has to keep matching.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{Method, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use acowork_core::account::Role;
use acowork_core::auth::now_unix;

use crate::auth::context::{
    AUTH_AS_USER_HEADER, AUTH_ROLE_HEADER, AUTH_USER_HEADER, is_public_path,
};
use crate::auth::{AuthService, BootstrapAdmin};
use crate::config::UserServiceConfig;
use crate::error::ApiError;
use crate::state::AppState;

/// A fresh, empty directory unique to this process + call.
///
/// `prefix` keeps the four call sites' temp trees distinguishable while
/// debugging a failure.
pub fn temp_dir(prefix: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "acowork-test-{prefix}-{}-{unique}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Config for a test service rooted at `dir`.
pub fn test_config(dir: &Path) -> UserServiceConfig {
    UserServiceConfig {
        data_dir: dir.to_path_buf(),
        // Gateway default: only an admin creates accounts (ADR-076 §决策 6).
        // Tests that exercise self-signup flip it on explicitly.
        registration_open: false,
        ..Default::default()
    }
}

/// A service whose store holds one admin (`root` / [`PWD`]).
pub fn admin_state(dir: &Path) -> AppState {
    let svc = AuthService::new(
        dir,
        Default::default(),
        Some(BootstrapAdmin {
            username: "root".into(),
            password: PWD.into(),
            display_name: None,
        }),
    )
    .unwrap();
    svc.ensure_bootstrap_admin().unwrap();
    state(dir, svc)
}

/// The password every seeded test account uses.
pub const PWD: &str = "s3cret123";

/// A service with an empty account store (no bootstrap admin) — the
/// first-boot state (ADR-114).
pub fn multi_user_state(dir: &Path) -> AppState {
    state(
        dir,
        AuthService::new(dir, Default::default(), None).unwrap(),
    )
}

fn state(dir: &Path, auth_service: AuthService) -> AppState {
    AppState::new(&test_config(dir), std::sync::Arc::new(auth_service))
}

/// The full user-service router with the Gateway's token→header step in
/// front, i.e. what a real request traverses.
pub fn build_router(state: AppState) -> Router {
    crate::http::build_router(&state)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            gateway_identity_shim,
        ))
        .with_state(state)
}

/// Stands in for the Gateway's `auth_middleware` (ADR-076 §决策 3 /
/// ADR-084 §决策 7): verify the bearer token once, then inject the identity
/// as `X-Auth-*` headers.
async fn gateway_identity_shim(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let svc = state.auth_service.clone();

    // The Gateway is the **only** trusted writer of these headers: a
    // client-supplied copy must never survive (proxy forwards headers
    // verbatim, so this is what stops a peer from claiming another user).
    for name in [AUTH_USER_HEADER, AUTH_ROLE_HEADER, AUTH_AS_USER_HEADER] {
        req.headers_mut().remove(name);
    }

    if is_public_path(req.uri().path()) {
        return next.run(req).await;
    }

    let Some(token) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return ApiError::unauthorized("missing bearer token").into_response();
    };

    let principal = match svc.verify_access(token, now_unix()) {
        Ok(principal) => principal,
        Err(e) => return ApiError::unauthorized(&format!("invalid token: {e}")).into_response(),
    };

    // ADR-076 §决策 4: `as_user` is admin-only and read-only.
    let as_user = query_param(req.uri().query(), "as_user");
    if let Some(id) = &as_user {
        if !matches!(req.method(), &Method::GET | &Method::HEAD) {
            return ApiError::forbidden(
                "as_user is a read-only view and cannot be used on a write request",
            )
            .into_response();
        }
        if principal.role != Role::Admin || !is_valid_scope_id(id) {
            return ApiError::forbidden(
                "as_user requires an administrator token and a well-formed user id",
            )
            .into_response();
        }
    }

    let headers = req.headers_mut();
    headers.insert(AUTH_USER_HEADER, principal.user_id.parse().unwrap());
    headers.insert(
        AUTH_ROLE_HEADER,
        match principal.role {
            Role::Admin => "admin",
            Role::User => "user",
        }
        .parse()
        .unwrap(),
    );
    if let Some(id) = as_user {
        headers.insert(AUTH_AS_USER_HEADER, id.parse().unwrap());
    }

    next.run(req).await
}

/// Whether an `as_user` value is safe to put in an HTTP header (no control
/// characters, no unbounded length).
fn is_valid_scope_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// First value of `key` in a raw query string. No percent-decoding: test
/// ids are `[A-Za-z0-9_-]`.
fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    query?.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}
