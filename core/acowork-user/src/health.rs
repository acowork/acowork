//! acowork-user health-check endpoint (supervisor liveness contract).
//!
//! Reuses [`acowork_core::health::HealthResponse`]: the Gateway supervisor
//! probes `GET /health` to decide whether this process is ready / alive
//! (same contract as PM / doc / embed / LSP relay).
//!
//! The `details` payload is also the Gateway's snapshot source for the
//! account system's first-boot state and `registration_open` (ADR-084
//! §决策 4b): it rides the poll the supervisor already performs, so the
//! restricted-mode gate and `/api/status` need no extra round trip.

use axum::Json;
use axum::extract::State;
use serde_json::json;

use acowork_core::account::AccountSystemState;

use crate::state::AppState;

/// Build the `/health` route.
pub fn health_route() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/health",
        axum::routing::get(move |State(state): State<AppState>| {
            let state = state.clone();
            async move {
                // Copy the small values out under the read guard, then
                // RELEASE it before the synchronous disk read below. The
                // Gateway polls this endpoint every 2s; holding the guard
                // across `load_accounts()` would stall every writer of the
                // shared profile cache for the duration of that I/O.
                let (data_dir, registration_open, profile_version) = {
                    let shared = state.shared.read().await;
                    (
                        shared.data_dir.clone(),
                        shared.registration_open,
                        shared.resource_cache.user_profile_list.version,
                    )
                };
                let account_state = crate::health::account_system_state(&data_dir);
                Json(acowork_core::health::HealthResponse {
                    status: "ok".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                    process: "acowork-user".to_string(),
                    details: Some(json!({
                        "data_dir": data_dir.display().to_string(),
                        // ADR-114: the account system's first-boot state.
                        // The Gateway reads this to decide whether to keep
                        // the first-boot gate closed.
                        "account_state": account_state.as_str(),
                        "registration_open": registration_open,
                        // Lets the Gateway notice a profile change on the poll
                        // it already performs. The MQTT signal
                        // (`acowork/user/profiles/changed`) is the fast path;
                        // this is the one that survives a broker that refuses
                        // our client (see `mqtt_publisher`).
                        "user_profile_version": profile_version,
                    })),
                })
            }
        }),
    )
}

/// The account system's current first-boot state.
///
/// Read straight off the account store — the store is this process's own,
/// so there is no second reader to disagree with.
pub fn account_system_state(data_dir: &std::path::Path) -> AccountSystemState {
    match crate::account::store::load_accounts(data_dir) {
        Ok(list) => AccountSystemState::from_accounts(&list),
        // An unreadable store is not "ready" — the operator has to look at
        // it before anyone can log in.
        Err(_) => AccountSystemState::NeedsSetup,
    }
}
