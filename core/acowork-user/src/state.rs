//! Shared HTTP state for the user service.
//!
//! Mirrors the slice of the Gateway's `AppState` / `GatewayState` that the
//! user-domain handlers actually touch, so the ported handlers needed only
//! their accessor calls rewritten — not their logic.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use acowork_core::protocol::UserProfileListFile;

use crate::auth::AuthService;
use crate::chat_alerts::AlertStore;
use crate::config::UserServiceConfig;

/// Cached derived views. One field today; kept as a struct so the growth
/// path (add a cached view, not a loose field) is obvious.
#[derive(Debug, Clone)]
pub struct ResourceCache {
    /// `user_profiles.json` — the derived public view of the account list.
    pub user_profile_list: UserProfileListFile,
}

/// State shared behind one lock.
#[derive(Debug, Clone)]
pub struct SharedState {
    pub resource_cache: ResourceCache,
    pub data_dir: PathBuf,
    /// `[multi_user].registration_open` — whether non-admins may self-register.
    pub registration_open: bool,
}

impl SharedState {
    pub fn new(data_dir: PathBuf, registration_open: bool) -> Self {
        Self {
            resource_cache: ResourceCache {
                user_profile_list: crate::profiles::load_user_profile_list(&data_dir),
            },
            data_dir,
            registration_open,
        }
    }
}

/// Application state available to all HTTP handlers.
#[derive(Clone)]
pub struct AppState {
    /// The account system. Always present (ADR-114) — there is no
    /// deployment in which this service serves profiles but no accounts.
    pub auth_service: Arc<AuthService>,
    /// Lazily-shared mutable state (profile cache + tuning flags).
    pub shared: Arc<RwLock<SharedState>>,
    /// Live delivery failures, by Chat (`crate::chat_alerts`). In memory on
    /// purpose: a failed send is news while it is news, and nothing that restarts
    /// this service should resurrect it as history.
    pub alerts: Arc<AlertStore>,
}

impl AppState {
    pub fn new(config: &UserServiceConfig, auth_service: Arc<AuthService>) -> Self {
        let shared = SharedState::new(config.data_dir.clone(), config.registration_open);
        Self {
            auth_service,
            shared: Arc::new(RwLock::new(shared)),
            alerts: Arc::new(AlertStore::new()),
        }
    }

    /// The account system's current first-boot state (ADR-114).
    ///
    /// Read straight off the store this process owns — the Gateway learns
    /// it from `/health`, so there is exactly one reader and the two can
    /// never disagree.
    pub async fn account_system_state(&self) -> acowork_core::account::AccountSystemState {
        let dir = self.shared.read().await.data_dir.clone();
        crate::health::account_system_state(&dir)
    }
}

/// ADR-059 §7.3 — validate a mutation's `expected_version` against the
/// live profile-list version.
///
/// Carried over from the Gateway, where the compared version came from the
/// global orchestrator snapshot. In the user domain the resource that a
/// `POST /api/users` precondition is about *is* `user_profiles.json`, so the
/// profile version is the correct comparison — and it is the version the
/// handler then bumps and returns in the ack.
pub async fn check_expected_version(
    state: &AppState,
    expected_version: Option<u64>,
) -> Result<(), crate::error::ApiError> {
    let Some(expected) = expected_version else {
        // No precondition — the mutation proceeds optimistically.
        return Ok(());
    };
    let current = state
        .shared
        .read()
        .await
        .resource_cache
        .user_profile_list
        .version;
    if current != expected {
        return Err(crate::error::ApiError::conflict_structured(
            acowork_core::error_codes::StructuredErrorBody::resource_version_conflict(
                current, expected,
            ),
        ));
    }
    Ok(())
}
