//! acowork-user HTTP surface (ADR-084 §决策 6 / §决策 1).
//!
//! **Internal paths are byte-identical to the old Gateway paths** and the
//! Gateway's `user_proxy` does **not** strip the prefix (unlike `/api/doc`),
//! so the ported handlers needed no path changes and the Desktop sees the
//! same URLs it always did.
//!
//! Mode split (mirrors the Gateway's registration, ADR-076 §决策 12):
//!
//! | Mode | Registered |
//! |------|-----------|
//! | `multi_user` | `account_routes` (`/api/users` with credential semantics), `chat_routes`, `auth_routes` |
//! | `local` | `users_routes` (`/api/users` presentation-only CRUD) |
//! | both | `user_avatar_routes` |
//!
//! Never both account and presentation `/api/users` routes: axum panics on a
//! double registration of one path, and the two have different authorization
//! semantics.

pub mod account_api;
pub mod auth_api;
pub mod chat_api;
pub mod chat_sessions_api;
pub mod profile_api;

use axum::Router;

use crate::state::AppState;

/// Largest body this surface accepts.
///
/// The chat attachment upload endpoint raises the router-wide ceiling to
/// this value (ADR-076 §决策 9); the value is carried over from the
/// Gateway's `GLOBAL_BODY_LIMIT` so an attachment that was accepted there is
/// still accepted here.
pub const GLOBAL_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// Build the user-domain router **without** `/health` (merged by the server
/// together with the identity layer).
pub fn user_router(state: &AppState) -> Router<AppState> {
    let mut router = if state.is_multi_user() {
        account_api::account_routes()
            .merge(chat_api::chat_routes())
            .merge(chat_sessions_api::chat_session_routes())
    } else {
        profile_api::users_routes()
    };

    router = router.merge(profile_api::user_avatar_routes());

    if state.is_multi_user() {
        router = router.merge(auth_api::auth_routes());
    }

    router
}

/// The complete user-service surface: user domain + `/health` + the
/// Gateway-only snapshot endpoint, behind the identity layer.
///
/// One assembler so the server, the tests and (from M5) the Gateway's proxy
/// target cannot drift apart on route ordering or layer placement.
pub fn build_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .merge(user_router(state))
        .route("/internal/user-profiles", axum::routing::get(profile_snapshot))
        .merge(crate::health::health_route())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::context::inject_identity,
        ))
}

/// `GET /internal/user-profiles` — the derived view the Gateway pulls to feed
/// `last_user_profile` to Runtime (ADR-084 §决策 4b).
///
/// Read-only and outside the `/api` surface the Desktop talks to. Not secret
/// in itself (same data as `GET /api/users`), so the pull needs no token: the
/// Gateway asks by *pulling*, and the service binds loopback (ADR-084 §决策 7)
/// so only the Gateway can reach it at all.
async fn profile_snapshot(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::Json<acowork_core::protocol::UserProfileListFile> {
    axum::Json(
        state
            .shared
            .read()
            .await
            .resource_cache
            .user_profile_list
            .clone(),
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    /// The path is a cross-process contract: the Gateway's
    /// `user_profile_sync` pulls exactly this URL. A rename on one side only
    /// would leave every Runtime on a stale `last_user_profile` with no error
    /// anywhere — so pin the URL and the payload, and check it serves the
    /// *live* cache rather than a snapshot taken at construction.
    ///
    /// `multi_user` on purpose: under `local` the identity middleware is a
    /// no-op, so a `local`-mode test passes even when the route is
    /// unreachable in the deployment that actually pulls it (that is how the
    /// first version of this test missed a live 401).
    #[tokio::test]
    async fn internal_snapshot_serves_the_live_profile_cache() {
        let dir = crate::test_support::temp_dir("http-internal");
        let state = crate::test_support::multi_user_state(&dir);

        // The write path every account mutation goes through.
        {
            let mut shared = state.shared.write().await;
            crate::profiles::rebuild_and_save_user_profile_cache(&mut shared);
            assert_eq!(shared.resource_cache.user_profile_list.version, 1);
        }

        let resp = crate::test_support::build_router(state)
            .oneshot(
                Request::builder()
                    .uri("/internal/user-profiles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["version"], 1);
        assert_eq!(body["users"], serde_json::json!([]));
    }
}
