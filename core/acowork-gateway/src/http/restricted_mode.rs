//! First-boot restricted-mode middleware (ADR-076 §决策 12 v2, ADR-114).
//!
//! The account system is **always present** (ADR-114). What this gate
//! enforces is the *first-boot* state: until an administrator account can
//! actually be logged into, the Gateway refuses to serve the regular API.
//!
//! Three account states, and they mean different things (see
//! [`acowork_core::account::AccountSystemState`]):
//!
//! | State | Meaning | What the Gateway serves |
//! |---|---|---|
//! | `uninitialized` | no account store / empty store | setup surface only |
//! | `needs_setup` | accounts exist, none can be logged into | setup surface only |
//! | `ready` | at least one administrator can log in | everything |
//!
//! ## The setup surface
//!
//! The old design had **no HTTP path** to the first password — the operator
//! had to SSH into the host. ADR-114 replaces that with
//! `POST /api/auth/setup`, which the Desktop's onboarding flow calls.
//!
//! That endpoint is the one place where "who is calling?" genuinely decides
//! anything, so it is gated on the **actual peer address** of the request,
//! not on the Gateway's bind host:
//!
//! - A request from a **loopback peer** may create the first administrator.
//!   This is the Desktop on the same machine, or an operator's `curl` on the
//!   host.
//! - A request from a **non-loopback peer** may not. The bind address is
//!   irrelevant here: a Gateway bound to `0.0.0.0` still receives
//!   loopback connections from its own host, and a Gateway bound to
//!   `127.0.0.1` is unreachable from the network anyway. Judging the peer
//!   is the only formulation that is correct in both cases.
//!
//! ## Placement
//!
//! Inside `CORS` and *outside* `auth_middleware` — axum executes layers
//! bottom-to-top, so this one is `.layer()`-ed after the auth gate
//! (`http/routes.rs::build_router`). We want the 403 to still carry CORS
//! headers (so the Desktop WebView can read the body) and to short-circuit
//! *before* the token check: an unauthenticated restricted-mode request must
//! be a 403 `setup_required`, never the 401 `missing bearer token` the auth
//! gate would answer first.
//!
//! ADR-084: the state no longer comes from a per-request read of
//! `accounts.json` — that store belongs to the `acowork-user` process. It
//! comes from the snapshot the user supervisor refreshes off the service's
//! `/health`, so this is O(1) and I/O-free on every path.

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Json, Response},
};
use serde_json::json;

use crate::http::routes::AppState;

/// Paths reachable in every first-boot state — the Desktop's liveness probe
/// and the status endpoint it reads the state from.
///
/// `/api/bootstrap` (2026-11-14): the Desktop's boot pipeline waits on this
/// endpoint reaching `READY`/`DEGRADED` with a 30 s budget and no token can
/// exist before the first administrator does. Gating it on setup meant a
/// fresh install answered `403 setup_required` forever, the Desktop burned
/// its whole budget, and the first-boot wizard never appeared — a fresh
/// install could not complete its very first boot. In restricted mode the
/// subsystems still come up (vault / mqtt / publisher / node), so the
/// snapshot legitimately reaches `READY` while every other route stays
/// gated; the Desktop then reads `/api/status` → `needs_setup` and shows
/// the setup view. Information exposed equals `/api/status`'s (instance
/// id, phase, versions), so no new surface.
const ALWAYS_PUBLIC: &[&str] = &["/health", "/api/status", "/api/bootstrap"];

/// The first-boot setup endpoint: create the first administrator.
///
/// Public in the auth middleware (no token can exist yet) but only
/// reachable from a loopback peer — see the module docs.
pub const SETUP_PATH: &str = "/api/auth/setup";

/// Middleware function — installs with `from_fn_with_state` (needs the
/// `AppState` to read the account state).
///
/// Returns 403 with `{"error":"setup_required"}` if the account system is
/// not yet usable and the request is not part of the setup surface.
/// Otherwise passes through.
pub async fn restricted_mode_middleware(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();

    // ADR-114 security invariant, evaluated FIRST: the first-boot setup
    // endpoint is local-only **unconditionally**, whatever the account
    // state says.
    //
    // It must not sit behind the `Ready` early-out below. The state comes
    // from a polled `/health` snapshot that can be stale in either
    // direction, and `/api/auth/setup` is a *public* path (no token can
    // exist before the first administrator does). If a stale or malformed
    // snapshot claimed `Ready` while the store was still `needs_setup`,
    // short-circuiting here would let a non-loopback peer reach the
    // service-side setup handler and choose the first admin's password.
    // The peer is the only fact that cannot be stale.
    if path == SETUP_PATH && !peer_is_loopback(&req) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({
                "error": "setup_local_only",
                "message": "First-boot administrator creation is only allowed from the \
                            Gateway host itself. Run `acowork-gateway admin-setup` on \
                            the host, or use a client on the same machine."
            })),
        )
            .into_response();
    }

    // O(1) early-out, and genuinely O(1): the state comes from the snapshot
    // the user supervisor refreshes on its `/health` poll, so this no longer
    // re-reads `accounts.json` per request (ADR-084 §决策 4b).
    //
    // `None` = the service has not reported yet. Treated as "not restricted"
    // deliberately: during that window `auth_middleware` already answers 503
    // for every non-public path and `user_proxy` has no upstream to forward
    // to, so there is nothing to additionally gate — and claiming
    // `setup_required` here would send the Desktop into the first-boot
    // wizard on a machine that is merely still starting. The setup path is
    // still covered by the unconditional peer check above.
    let account_state = {
        let gw = state.gateway_state.read().await;
        gw.user_snapshot
            .as_ref()
            .map(|s| s.account_state)
            .unwrap_or(acowork_core::account::AccountSystemState::Ready)
    };
    if !account_state.requires_setup() {
        return next.run(req).await;
    }

    // Liveness + status stay reachable so the Desktop can probe and
    // discover the state instead of seeing a dead Gateway.
    if ALWAYS_PUBLIC
        .iter()
        .any(|p| path == *p || path.starts_with(&format!("{p}/")))
    {
        return next.run(req).await;
    }

    // The setup endpoint: the peer was already checked above.
    if path == SETUP_PATH {
        return next.run(req).await;
    }

    // Everything else is refused until an administrator exists.
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "error": "setup_required",
            "message": "The account system has not been initialised yet. \
                        Create the first administrator (from this machine) to continue.",
            "account_state": account_state.as_str(),
        })),
    )
        .into_response()
}

/// Is the peer of this request on this machine?
///
/// The authoritative loopback test (ADR-114): the answer comes from the
/// connection's source address, not from the Gateway's configured bind
/// host.
///
/// `ConnectInfo` is present for every request that arrived over TCP: both
/// listeners are mounted with `into_make_service_with_connect_info`
/// (`http/server.rs`, `relay/remote_listener.rs`). A request without it has
/// no established network origin — an in-process call, a test, or a handler
/// invoked directly. That must **fail closed**: an origin we cannot prove
/// is local is not allowed to bootstrap the first administrator.
fn peer_is_loopback(req: &Request) -> bool {
    let Some(peer) = req
        .extensions()
        .get::<axum::extract::connect_info::ConnectInfo<std::net::SocketAddr>>()
    else {
        return false;
    };
    crate::auth::is_loopback_peer(peer.0.ip())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::state::GatewayState;
    use crate::http::auth::HttpAuth;
    use crate::http::routes::AppState;
    use acowork_core::account::AccountSystemState;
    use axum::{
        body::Body,
        extract::connect_info::ConnectInfo,
        http::{Request, StatusCode},
        middleware,
        routing::get,
        Router,
    };
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::RwLock;
    use tower::ServiceExt;

    fn build_state(dir: &std::path::Path) -> AppState {
        let gw_state = GatewayState::new(&dir.to_string_lossy());
        AppState::new(
            Arc::new(RwLock::new(gw_state)),
            Arc::new(HttpAuth::new(false)),
        )
    }

    async fn ok_handler() -> &'static str {
        "ok"
    }

    /// Build the router state for a first-boot Gateway.
    fn app_with_state(account_state: AccountSystemState) -> (Router, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path());
        state
            .gateway_state
            .try_write()
            .expect("fresh state")
            .user_snapshot = Some(crate::lifecycle::user_supervisor::UserSnapshot {
                account_state,
                registration_open: false,
                ..Default::default()
            });
        let router = Router::new()
            .route("/api/anything", get(ok_handler))
            .route("/health", get(ok_handler))
            .route("/api/status", get(ok_handler))
            .route("/api/bootstrap", get(ok_handler))
            .route(SETUP_PATH, axum::routing::post(ok_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                restricted_mode_middleware,
            ))
            .with_state(state);
        (router, dir)
    }

    #[tokio::test]
    async fn restricted_mode_blocks_unlisted_routes() {
        for st in [
            AccountSystemState::Uninitialized,
            AccountSystemState::NeedsSetup,
        ] {
            let (router, _dir) = app_with_state(st);
            let resp = router
                .oneshot(
                    Request::builder()
                        .uri("/api/anything")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
            let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "setup_required");
            // The state is reported so the client can tell "no account
            // system at all" from "accounts exist but cannot be used".
            assert_eq!(json["account_state"], st.as_str());
        }
    }

    #[tokio::test]
    async fn restricted_mode_allows_health_and_status() {
        let (router, _dir) = app_with_state(AccountSystemState::Uninitialized);
        // `/api/bootstrap` must be in this list: the Desktop's boot
        // pipeline (30 s, tokenless) waits on it — gating it strands a
        // fresh install before the setup wizard (2026-11-14 incident).
        for path in ["/health", "/api/status", "/api/bootstrap"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn ready_state_passes_everything_through() {
        let (router, _dir) = app_with_state(AccountSystemState::Ready);
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/anything")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The setup endpoint is the whole point of ADR-114's onboarding
    /// integration: a client on the same machine must be able to create the
    /// first administrator without an SSH session.
    #[tokio::test]
    async fn setup_endpoint_is_reachable_from_a_loopback_peer() {
        let (router, _dir) = app_with_state(AccountSystemState::Uninitialized);
        let peer: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(SETUP_PATH)
                    .extension(ConnectInfo(peer))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// …and only from a loopback peer. A remote client must not be able to
    /// take over a fresh install.
    #[tokio::test]
    async fn setup_endpoint_is_refused_from_a_remote_peer() {
        let (router, _dir) = app_with_state(AccountSystemState::Uninitialized);
        let peer: SocketAddr = "192.168.1.20:5555".parse().unwrap();
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(SETUP_PATH)
                    .extension(ConnectInfo(peer))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "setup_local_only");
    }

    /// Regression (ADR-114 review): the peer check must NOT be short-circuited
    /// by a `Ready` snapshot. The snapshot is a polled, possibly stale view of
    /// somebody else's store, while `/api/auth/setup` is a public path. If a
    /// stale `ready` ever reaches the gate while the store is still
    /// `needs_setup`, an early-out would let a remote caller choose the first
    /// administrator's password.
    #[tokio::test]
    async fn setup_stays_local_only_when_the_snapshot_says_ready() {
        let (router, _dir) = app_with_state(AccountSystemState::Ready);

        let remote: SocketAddr = "192.168.1.20:5555".parse().unwrap();
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(SETUP_PATH)
                    .extension(ConnectInfo(remote))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "setup_local_only");

        // A loopback caller still reaches the handler (which will answer 409
        // because the account system is already usable).
        let local: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(SETUP_PATH)
                    .extension(ConnectInfo(local))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// No `ConnectInfo` means no provable network origin. Fail closed rather
    /// than treating an in-process call as "local".
    #[tokio::test]
    async fn setup_without_a_provable_peer_is_refused() {
        let (router, _dir) = app_with_state(AccountSystemState::Uninitialized);
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(SETUP_PATH)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// A dual-stack listener reports a local IPv4 connection as
    /// `::ffff:127.0.0.1`; that is still this machine.
    #[tokio::test]
    async fn ipv4_mapped_loopback_peer_can_set_up() {
        let (router, _dir) = app_with_state(AccountSystemState::NeedsSetup);
        let peer: SocketAddr = "[::ffff:127.0.0.1]:5555".parse().unwrap();
        let resp = router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(SETUP_PATH)
                    .extension(ConnectInfo(peer))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The bind address must not influence the decision — only the peer.
    /// A Gateway bound to `0.0.0.0` ("remote" in the old design) still
    /// gets loopback connections from its own host, and those are exactly
    /// the ones allowed to bootstrap. A loopback-only bind behaves
    /// identically. The old code derived the account regime from this
    /// value; ADR-114 makes it irrelevant to authentication.
    #[tokio::test]
    async fn the_bind_address_does_not_gate_the_setup_endpoint() {
        for host in ["127.0.0.1", "0.0.0.0"] {
            let dir = tempfile::tempdir().unwrap();
            let state = build_state(dir.path());
            {
                let mut gw = state.gateway_state.try_write().expect("fresh state");
                gw.config = Some(crate::config::GatewayConfig {
                    data_dir: dir.path().to_string_lossy().to_string(),
                    http: crate::config::HttpConfig {
                        host: host.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                });
                gw.user_snapshot = Some(crate::lifecycle::user_supervisor::UserSnapshot {
                    account_state: AccountSystemState::Uninitialized,
                    registration_open: false,
                    ..Default::default()
                });
            }
            let router = Router::new()
                .route(SETUP_PATH, axum::routing::post(ok_handler))
                .layer(middleware::from_fn_with_state(
                    state.clone(),
                    restricted_mode_middleware,
                ))
                .with_state(state);

            let local: SocketAddr = "127.0.0.1:5555".parse().unwrap();
            let resp = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(SETUP_PATH)
                        .extension(ConnectInfo(local))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "bind {host}");

            let remote: SocketAddr = "192.168.1.20:5555".parse().unwrap();
            let resp = router
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(SETUP_PATH)
                        .extension(ConnectInfo(remote))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "bind {host}");
        }
    }

    /// The gate must be the *outer* middleware. Layer order only exists in
    /// the real router, so this asserts against `build_router`: with no
    /// `Authorization` header at all, a restricted Gateway must answer
    /// 403 `setup_required`, not the 401 `missing bearer token` that
    /// `auth_middleware` would produce if it ran first.
    #[tokio::test]
    async fn real_router_answers_403_not_401_without_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let state = build_state(dir.path());
        {
            let mut gw = state.gateway_state.try_write().expect("fresh state");
            gw.config = Some(crate::config::GatewayConfig {
                data_dir: dir.path().to_string_lossy().to_string(),
                ..Default::default()
            });
            // First-boot snapshot, as the user service reports it in
            // `/health`.
            gw.user_snapshot = Some(crate::lifecycle::user_supervisor::UserSnapshot {
                account_state: AccountSystemState::Uninitialized,
                registration_open: false,
                ..Default::default()
            });
        }
        let router = crate::http::routes::build_router(state);

        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/users")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "setup_required");

        // `/api/status` stays reachable *and* advertises the gate, so the
        // Desktop can discover the state instead of seeing a dead Gateway.
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["account_state"], "uninitialized");
    }
}
