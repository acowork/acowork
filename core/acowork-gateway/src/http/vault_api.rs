//! Vault lock/unlock HTTP API (ADR-059 Phase 5.4)
//!
//! - `POST /api/vault/lock` — lock the vault (zeroize the derived
//!   master key), demote the `vault` subsystem to `Booting` so the
//!   aggregated BootstrapState phase drops and dependent Runtimes
//!   re-evaluate provider-key usage.
//! - `POST /api/vault/unlock` — unlock with a password (slow Argon2id
//!   KDF on the blocking pool), then mark the `vault` subsystem Ready
//!   again and trigger a global-resources republish so Runtimes
//!   receive the decrypted keys.
//!
//! Both paths share [`crate::vault::unlock_vault_and_mark_ready`] with
//! the dev-mode cold-start auto-unlock — the relock recovery path is
//! the same code, not a duplicate implementation (ADR-059 Phase 5.4).

use axum::{
    Json, Router,
    extract::State,
    routing::post,
};
#[cfg(test)]
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the vault router.
pub fn vault_routes() -> Router<AppState> {
    Router::new()
        .route("/api/vault/lock", post(lock_vault))
        .route("/api/vault/unlock", post(unlock_vault))
}

/// Request body for `POST /api/vault/unlock`.
#[derive(Deserialize)]
pub struct UnlockVaultRequest {
    /// Vault password (derived into the master key via Argon2id).
    pub password: String,
}

/// Vault lock state projection for both endpoints.
#[derive(Debug, Serialize)]
pub struct VaultStatusResponse {
    /// Whether the vault currently holds a derived master key.
    pub unlocked: bool,
}

/// `POST /api/vault/lock` — lock the vault and demote its readiness.
///
/// The vault is a required subsystem; demoting it to `Booting` (NOT
/// `Failed` — the transition must stay reversible, a later unlock
/// honours `mark_ready`) drops the aggregated BootstrapState phase so
/// Runtimes that depend on provider credentials re-evaluate. The
/// orchestrator republishes the bootstrap snapshot automatically on
/// this transition; no explicit trigger is needed here because the
/// vault data itself did not change.
pub async fn lock_vault(
    State(state): State<AppState>,
) -> Result<Json<VaultStatusResponse>, ApiError> {
    let mut gw = state.gateway_state.write().await;
    if !gw.vault.is_unlocked() {
        return Err(ApiError::bad_request("Vault is already locked"));
    }
    gw.vault.lock();
    if let Some(ref h) = gw.bootstrap.vault_readiness_handle {
        h.mark_booting(Some("vault locked by user".to_string()));
    } else {
        tracing::warn!("Vault readiness handle not registered; lock without readiness demotion");
    }
    tracing::info!("Vault locked by user request");
    Ok(Json(VaultStatusResponse { unlocked: false }))
}

/// `POST /api/vault/unlock` — unlock the vault and restore readiness.
///
/// Runs the slow Argon2id KDF on the blocking pool (never inside an
/// async context), then reuses the exact cold-start unlock sequence
/// (mark vault ready → republish global resources → raise publisher
/// barrier if still pending) via
/// [`crate::vault::unlock_vault_and_mark_ready`].
pub async fn unlock_vault(
    State(state): State<AppState>,
    Json(body): Json<UnlockVaultRequest>,
) -> Result<Json<VaultStatusResponse>, ApiError> {
    if body.password.is_empty() {
        return Err(ApiError::bad_request("password must not be empty"));
    }
    // Idempotent fast path: an already-unlocked vault has nothing to
    // re-derive; skip the ~1 s KDF.
    let gateway_state = state.gateway_state.clone();
    {
        let gw = gateway_state.read().await;
        if gw.vault.is_unlocked() {
            return Ok(Json(VaultStatusResponse { unlocked: true }));
        }
    }
    let handle = {
        let gw = gateway_state.read().await;
        gw.bootstrap.vault_readiness_handle.clone()
    };
    let Some(handle) = handle else {
        return Err(ApiError::internal(
            "vault readiness handle not registered — cannot restore readiness",
        ));
    };
    let trigger = state.mqtt_publisher_trigger.clone();
    crate::vault::unlock_vault_and_mark_ready(
        gateway_state,
        handle,
        trigger,
        body.password,
        "vault unlocked via HTTP".to_string(),
    )
    .await
    .map_err(|e| ApiError::bad_request(&e))?;
    Ok(Json(VaultStatusResponse { unlocked: true }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::SubsystemReadinessRegistry;
    use crate::gateway::state::GatewayState;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Build an AppState with a fresh temp-dir vault and a live
    /// readiness registry so handlers can be exercised end to end.
    fn test_app_state(dir: &str) -> AppState {
        let gateway_state = Arc::new(RwLock::new(GatewayState::new(dir)));
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("vault", crate::bootstrap::ReadinessKind::Required);
        // Runtime registration happens in `Gateway::run`; mirror it here.
        gateway_state.blocking_write().bootstrap.vault_readiness_handle = Some(handle);
        AppState::new(gateway_state, Arc::new(crate::http::auth::HttpAuth::new(false)))
    }

    #[test]
    fn lock_and_unlock_roundtrip_demotes_and_restores_readiness() {
        let dir = crate::test_support::unique_temp_dir("acowork-test-vaultapi-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.to_string_lossy().to_string();

        let state = test_app_state(&dir);
        // Seed: unlock + store a key so the locked state is observable.
        {
            let mut gw = state.gateway_state.blocking_write();
            gw.vault.unlock("password123").unwrap();
            gw.vault.store_key("openai", "sk-vault-api").unwrap();
        }

        let rt = tokio::runtime::Runtime::new().unwrap();

        // Lock: vault zeroized + subsystem demoted to Booting.
        let resp = rt.block_on(lock_vault(State(state.clone())));
        let body = resp.unwrap();
        assert!(!body.0.unlocked);
        assert!(!state.gateway_state.blocking_read().vault.is_unlocked());

        // Unlock with the same password: readiness restored.
        let req = UnlockVaultRequest {
            password: "password123".to_string(),
        };
        let resp = rt.block_on(unlock_vault(State(state.clone()), Json(req)));
        let body = resp.unwrap();
        assert!(body.0.unlocked);
        let gw = state.gateway_state.blocking_read();
        assert!(gw.vault.is_unlocked());
        assert_eq!(gw.vault.get_key("openai").unwrap(), "sk-vault-api");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lock_on_locked_vault_returns_bad_request() {
        let dir = crate::test_support::unique_temp_dir("acowork-test-vaultapi-already-locked");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.to_string_lossy().to_string();

        let state = test_app_state(&dir);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resp = rt.block_on(lock_vault(State(state.clone())));
        assert!(resp.is_err());
        assert_eq!(resp.unwrap_err().code, StatusCode::BAD_REQUEST.as_u16());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unlock_with_empty_password_rejected() {
        let dir = crate::test_support::unique_temp_dir("acowork-test-vaultapi-empty-pw");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.to_string_lossy().to_string();

        let state = test_app_state(&dir);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let req = UnlockVaultRequest {
            password: String::new(),
        };
        let resp = rt.block_on(unlock_vault(State(state.clone()), Json(req)));
        assert!(resp.is_err());
        assert_eq!(resp.unwrap_err().code, StatusCode::BAD_REQUEST.as_u16());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
