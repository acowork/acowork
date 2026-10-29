//! Vault key management commands

use std::collections::HashMap;
use serde::Deserialize;
use tauri::State;

use crate::gateway_client::{
    GenericMessageResponse, ModelCapabilities, ModelProtocolIntent, ProviderAddResult,
    ProviderUpdateResult, VaultKeyEntry,
};
use crate::state::AppState;

/// One API key entry inside an `add_key` invoke call.
#[derive(Debug, Clone, Deserialize)]
pub struct AddProviderKey {
    #[serde(default)]
    pub alias: Option<String>,
    pub key: String,
}

/// List all stored API keys (masked)
#[tauri::command]
pub async fn list_keys(state: State<'_, AppState>) -> Result<Vec<VaultKeyEntry>, String> {
    let client = state.gateway.read().await;
    client.list_keys().await.map_err(|e| e.to_string())
}

/// Add one or more API keys for a provider.
///
/// `keys` carries the actual credentials — pass one entry per account
/// (e.g. `[{alias:"work",key:"sk-..."}, {alias:"personal",key:"sk-..."}]`).
/// Provider-level config (base_url, models, compact_model, capabilities)
/// is shared across all accounts for the same provider.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn add_key(
    state: State<'_, AppState>,
    provider: String,
    keys: Vec<AddProviderKey>,
    base_url: Option<String>,
    default_model: Option<String>,
    models: Option<Vec<String>>,
    model_capabilities: Option<HashMap<String, ModelCapabilities>>,
    compact_model: Option<String>,
    custom: Option<bool>,
    model_protocols: Option<ModelProtocolIntent>,
    model_base_urls: Option<HashMap<String, String>>,
) -> Result<ProviderAddResult, String> {
    let client = state.gateway.read().await;
    let caps = model_capabilities.unwrap_or_default();
    client
        .add_key(
            &provider,
            &keys,
            base_url.as_deref(),
            default_model.as_deref(),
            models.as_deref(),
            &caps,
            compact_model.as_deref(),
            custom.unwrap_or(false),
            &model_protocols.unwrap_or_default(),
            &model_base_urls.unwrap_or_default(),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Remove an API key for a provider.
///
/// When `account_id` is `None` the entire provider (all accounts) is
/// removed. When `Some(account_id)` only that single account is
/// removed; the provider's base_url/models config stays in place.
#[tauri::command]
pub async fn remove_key(
    state: State<'_, AppState>,
    provider: String,
    account_id: Option<String>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .remove_key(&provider, account_id.as_deref())
        .await
        .map_err(|e| e.to_string())
}

/// Update an API key (supports partial updates — key is optional)
#[tauri::command]
/// Update an existing provider's config and / or append new accounts.
///
/// `key` (legacy) replaces the first account's API key when present.
/// `keys` (multi-account) appends fresh accounts — existing accounts
/// are left untouched. Provider-level config (`base_url`, `models`,
/// `compact_model`, `model_capabilities`) is updated in the same call.
#[allow(clippy::too_many_arguments)]
pub async fn update_key(
    state: State<'_, AppState>,
    provider: String,
    keys: Vec<AddProviderKey>,
    base_url: Option<String>,
    default_model: Option<String>,
    models: Option<Vec<String>>,
    model_capabilities: Option<HashMap<String, ModelCapabilities>>,
    compact_model: Option<String>,
    model_protocols: Option<ModelProtocolIntent>,
    model_base_urls: Option<HashMap<String, String>>,
) -> Result<ProviderUpdateResult, String> {
    let client = state.gateway.read().await;
    let caps = model_capabilities.unwrap_or_default();
    client
        .update_key(
            &provider,
            &keys,
            base_url.as_deref(),
            default_model.as_deref(),
            models.as_deref(),
            &caps,
            compact_model.as_deref(),
            &model_protocols.unwrap_or_default(),
            &model_base_urls.unwrap_or_default(),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Edit one existing account's alias and/or key. Used by the harness
/// edit dialog so users can rename or rotate a key on an account
/// they've already configured — no need to delete + re-add.
#[tauri::command]
pub async fn update_account_key(
    state: State<'_, AppState>,
    provider: String,
    account_id: String,
    alias: Option<String>,
    key: Option<String>,
) -> Result<GenericMessageResponse, String> {
    let client = state.gateway.read().await;
    client
        .update_account_key(
            &provider,
            &account_id,
            alias.as_deref(),
            key.as_deref(),
        )
        .await
        .map_err(|e| e.to_string())
}
