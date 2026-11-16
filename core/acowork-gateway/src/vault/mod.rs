//! Vault integration — facade for API key distribution
//!
//! Wraps acowork-vault crate and adds Gateway-specific key distribution logic.
//! All API keys are stored encrypted on disk via acowork_vault::Vault.
//!
//! Vault ONLY stores encrypted API keys. All non-secret provider configuration
//! (base_url, models, capabilities, compact_model) is stored in
//! provider_list.json via the resource_cache module.
//!
//! ## Account dimension (multi-key per provider)
//!
//! Each provider can hold multiple accounts (e.g. work + personal key for the
//! same vendor). Vault entries use `<provider>__<account_id>.enc` as the
//! on-disk name; `account_id` is the stable addressing key (UUID), `alias`
//! is the mutable user-facing label.
//!
//! Storage formats (encrypted payload inside each `.enc`):
//!   Legacy plain: raw API key string
//!   Legacy JSON:  `{"api_key": "..."}`
//!   Current:      `{"provider_id":"…","account_id":"…","alias":"…","api_key":"…"}`
//!
//! `unlock` performs a one-shot migration of legacy entries into the
//! `<provider>__legacy.enc` namespace. After that all reads see the
//! current shape and `get_provider`/`list_keys`/`remove_key` keep
//! their pre-multi-account signatures by collapsing to "first account".

use crate::error::GatewayError;
use crate::util::preview_key;
use acowork_core::providers::vault_key_candidates;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Separator inside `<provider>__<account>.enc` entry names.
/// Picked because provider IDs never contain `__` (validated upstream).
const ACCOUNT_SEP: &str = "__";
/// Stable account ID assigned to entries that pre-date the multi-account
/// refactor. Collapses to "first/legacy account" for back-compat reads.
const LEGACY_ACCOUNT_ID: &str = "legacy";

/// Build the on-disk entry name for a (provider, account) pair.
fn account_entry_name(provider: &str, account_id: &str) -> String {
    format!("{provider}{ACCOUNT_SEP}{account_id}")
}

/// Parse `<provider>__<account>.enc` back into its components. Returns
/// `None` for legacy names without the separator.
fn parse_account_entry_name(name: &str) -> Option<(String, String)> {
    name.split_once(ACCOUNT_SEP)
        .map(|(p, a)| (p.to_string(), a.to_string()))
}

/// Vault namespaces that are NOT LLM provider accounts. Embedding
/// credentials share the same encrypted directory (see `EMBEDDING_PREFIX`)
/// but must never be indexed as providers nor pulled through the
/// legacy-name migration.
const RESERVED_VAULT_PREFIXES: [&str; 1] = ["_embedding_"];

fn is_reserved_vault_key(name: &str) -> bool {
    RESERVED_VAULT_PREFIXES.iter().any(|p| name.starts_with(p))
}

/// In-memory index key for one account. The bare `account_id` is NOT
/// unique across providers - every migrated legacy entry shares the same
/// `LEGACY_ACCOUNT_ID` - so `accounts` must be keyed by the pair, or one
/// provider's legacy entry silently overwrites another's.
fn account_key(provider_id: &str, account_id: &str) -> String {
    account_entry_name(provider_id, account_id)
}

/// Pre-multi-account JSON shape used to recognise legacy vault payloads
/// during the one-shot migration in [`VaultFacade::rebuild_index`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct LegacyProviderEntry {
    api_key: String,
}

/// Provider entry stored in Vault — the API key plus account metadata.
///
/// All non-secret provider configuration (base_url, models, capabilities,
/// compact_model) is stored in provider_list.json, NOT in the Vault.
/// See `resource_cache.rs` for provider configuration management.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderEntry {
    /// Provider identifier (e.g. "alibaba-cn"). Redundant with the on-disk
    /// entry name, but kept inside the JSON for self-describing recovery.
    pub provider_id: String,
    /// Stable account UUID — the runtime addressing key. Never reused.
    pub account_id: String,
    /// User-facing label, mutable. Not unique; display-only.
    pub alias: String,
    /// API key for this account.
    pub api_key: String,
}

/// Key entry for HTTP API listing (masked preview).
///
/// `list_keys` now expands one entry per account, so callers iterating
/// this list may see multiple rows for the same `provider`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VaultKeyEntry {
    /// Provider name
    pub provider: String,
    /// Stable account UUID — use this for any subsequent add/update/remove.
    pub account_id: String,
    /// User-facing label.
    pub alias: String,
    /// Masked key preview (first 3 + last 3 chars)
    pub key_preview: String,
}

/// Vault facade for Gateway
///
/// Delegates to acowork_vault::Vault for encrypted storage. The facade
/// keeps a small in-memory index of accounts so multi-key lookups don't
/// have to scan the encrypted directory on every call.
pub struct VaultFacade {
    /// Inner vault (encrypted on-disk storage)
    vault: acowork_vault::Vault,
    /// Decrypted account entries keyed by `provider_id__account_id`
    /// (see [`account_key`]). The bare account id is only unique within
    /// a single provider, so it cannot be the map key.
    accounts: HashMap<String, ProviderEntry>,
    /// Reverse index: `provider_id` → list of `account_id` (insertion order).
    /// Replaces the old `provider_names` cache.
    by_provider: HashMap<String, Vec<String>>,
    /// Directory path where the vault is stored
    vault_dir: String,
}

impl VaultFacade {
    /// Create a new vault facade pointing at the given directory
    ///
    /// The vault starts in a locked state. Call `unlock()` with a password
    /// to derive the master key and enable store/retrieve operations.
    pub fn new(vault_dir: &str) -> Self {
        let vault = acowork_vault::Vault::open(std::path::Path::new(vault_dir))
            .unwrap_or_else(|e| panic!("Failed to open vault directory '{}': {}", vault_dir, e));
        Self {
            vault,
            accounts: HashMap::new(),
            by_provider: HashMap::new(),
            vault_dir: vault_dir.to_string(),
        }
    }

    /// Unlock the vault with a password (delegates to acowork_vault).
    ///
    /// After the underlying vault unlocks, this method:
    /// 1. Lists every `<name>.enc` on disk.
    /// 2. Decodes each name as `<provider>__<account_id>` when possible.
    /// 3. Migrates any legacy names (no `__`) into the
    ///    `<provider>__legacy.enc` namespace — covering both plaintext
    ///    legacy blobs and the older `{ "api_key": "…" }` JSON shape.
    /// 4. Rebuilds the in-memory `accounts` / `by_provider` indices.
    pub fn unlock(&mut self, password: &str) -> Result<(), GatewayError> {
        self.vault
            .unlock(password)
            .map_err(|e| GatewayError::Vault(format!("Failed to unlock vault: {}", e)))?;
        self.rebuild_index()
    }

    /// Walk the on-disk vault and rebuild the in-memory account index,
    /// migrating any legacy entries as a side effect.
    fn rebuild_index(&mut self) -> Result<(), GatewayError> {
        self.accounts.clear();
        self.by_provider.clear();

        let names = self
            .vault
            .list()
            .map_err(|e| GatewayError::Vault(format!("Failed to list vault keys: {}", e)))?;

        // Pass 1: load everything that's already in the new shape.
        // The reserved `_embedding_*` namespace shares the
        // directory but is not an LLM account - never index it.
        for name in &names {
            let Some((provider, _account)) = parse_account_entry_name(name) else {
                continue; // legacy; handled in pass 2
            };
            if is_reserved_vault_key(&provider) {
                continue;
            }
            match self.vault.retrieve(name) {
                Ok(secret) => match serde_json::from_str::<ProviderEntry>(secret.expose_secret()) {
                    Ok(entry) => self.index_entry(entry),
                    Err(e) => {
                        tracing::warn!(
                            entry = %name,
                            error = %e,
                            "vault: skipping malformed multi-account entry"
                        );
                    }
                },
                Err(e) => tracing::warn!(
                    entry = %name,
                    error = %e,
                    "vault: failed to decrypt multi-account entry"
                ),
            }
        }

        // Pass 2: migrate legacy entries (no `__` in the name).
        let legacy: Vec<String> = names
            .into_iter()
            .filter(|n| parse_account_entry_name(n).is_none() && !is_reserved_vault_key(n))
            .collect();
        for old_name in legacy {
            self.migrate_legacy_entry(&old_name)?;
        }
        Ok(())
    }

    /// Convert a legacy `<provider>.enc` into `<provider>__legacy.enc`
    /// using the current `ProviderEntry` shape, then delete the old file.
    fn migrate_legacy_entry(&mut self, old_name: &str) -> Result<(), GatewayError> {
        let secret = match self.vault.retrieve(old_name) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    entry = %old_name,
                    error = %e,
                    "vault: cannot decrypt legacy entry during migration; leaving in place"
                );
                return Ok(());
            }
        };
        let raw = secret.expose_secret();

        // Try the older JSON shape first (`{"api_key":"…"}`); fall back
        // to treating the raw bytes as a plain-text key (the original
        // pre-JSON format).
        let api_key = match serde_json::from_str::<LegacyProviderEntry>(raw) {
            Ok(old) => old.api_key,
            Err(_) => raw.to_string(),
        };
        let entry = ProviderEntry {
            provider_id: old_name.to_string(),
            account_id: LEGACY_ACCOUNT_ID.to_string(),
            alias: format!("{old_name}-default"),
            api_key,
        };
        let json = serde_json::to_string(&entry)
            .map_err(|e| GatewayError::Vault(format!("Migration serialise: {}", e)))?;

        let new_name = account_entry_name(old_name, LEGACY_ACCOUNT_ID);
        // Write the new namespace entry first; only delete the old one
        // once the new write succeeds, so a mid-migration crash leaves
        // the legacy file recoverable.
        self.vault
            .store(&new_name, &json)
            .map_err(|e| GatewayError::Vault(format!("Migration write: {}", e)))?;
        self.vault.delete(old_name).map_err(|e| {
            // Best-effort rollback: if delete fails, the old entry is
            // still readable via the legacy path on next unlock.
            tracing::warn!(
                old = %old_name,
                new = %new_name,
                error = %e,
                "vault: failed to remove legacy entry after migration"
            );
            GatewayError::Vault(format!("Failed to remove legacy entry: {}", e))
        })?;
        self.index_entry(entry);
        tracing::info!(
            provider = %old_name,
            new_entry = %new_name,
            "vault: migrated legacy entry to multi-account namespace"
        );
        Ok(())
    }

    /// Insert an entry into the in-memory indices, preserving insertion
    /// order within a provider's account list.
    fn index_entry(&mut self, entry: ProviderEntry) {
        let provider = entry.provider_id.clone();
        let account = entry.account_id.clone();
        self.by_provider
            .entry(provider.clone())
            .or_default()
            .push(account.clone());
        self.accounts
            .insert(account_key(&provider, &account), entry);
    }

    /// Check if vault is unlocked
    pub fn is_unlocked(&self) -> bool {
        self.vault.is_unlocked()
    }

    /// Lock the vault: zeroize the derived master key and drop the
    /// in-memory account index.
    ///
    /// On-disk encrypted blobs are untouched — a later `unlock()` with
    /// the same password restores access to everything previously
    /// stored. Idempotent: locking an already-locked vault is a no-op.
    pub fn lock(&mut self) {
        self.vault.lock();
        self.accounts.clear();
        self.by_provider.clear();
    }

    /// Get the vault directory path
    pub fn dir(&self) -> &std::path::Path {
        std::path::Path::new(&self.vault_dir)
    }

    /// Store a provider API key (encrypted on disk).
    ///
    /// Stores only the API key. Provider configuration (base_url, models, etc.)
    /// is managed separately via provider_list.json.
    /// Legacy back-compat entry point: stores a single account under the
    /// default alias `"{provider}-default"`. New callers should use
    /// [`add_account`] directly so they can pick the alias.
    pub fn store_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        let _ = self.add_account(provider, None, api_key)?;
        Ok(())
    }

    /// Add a new account for a provider. The `account_id` is generated
    /// server-side (UUIDv4); the alias defaults to `"{provider}-default"`
    /// when `None` or empty.
    ///
    /// Returns the new `account_id` on success.
    pub fn add_account(
        &mut self,
        provider: &str,
        alias: Option<&str>,
        api_key: &str,
    ) -> Result<String, GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        // DIAG: keep the historical store-time preview log so upstream
        // bugs surface in the same place they did before this change.
        tracing::info!(
            provider = %provider,
            api_key_len = api_key.len(),
            api_key_prefix = %preview_key(api_key),
            "vault.add_account: serialising ProviderEntry before encryption"
        );
        let account_id = uuid::Uuid::new_v4().to_string();
        let alias = alias.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("");
        let alias = if alias.is_empty() {
            format!("{provider}-default")
        } else {
            alias.to_string()
        };
        let entry = ProviderEntry {
            provider_id: provider.to_string(),
            account_id: account_id.clone(),
            alias,
            api_key: api_key.to_string(),
        };
        let json = serde_json::to_string(&entry)
            .map_err(|e| GatewayError::Vault(format!("Failed to serialize provider entry: {}", e)))?;
        let name = account_entry_name(provider, &account_id);
        self.vault
            .store(&name, &json)
            .map_err(|e| GatewayError::Vault(format!("Failed to store key: {}", e)))?;
        self.index_entry(entry);
        Ok(account_id)
    }

    /// Look up one account by `(provider_id, account_id)`.
    pub fn get_account(
        &self,
        provider_id: &str,
        account_id: &str,
    ) -> Result<ProviderEntry, GatewayError> {
        self.accounts
            .get(&account_key(provider_id, account_id))
            .cloned()
            .ok_or_else(|| {
                GatewayError::Vault(format!(
                    "No account {account_id} for provider {provider_id}"
                ))
            })
    }

    /// Rename the alias of an existing account. The provider_id and
    /// account_id are unchanged; only the `alias` field is rewritten
    /// in-place.
    pub fn update_alias(
        &mut self,
        provider_id: &str,
        account_id: &str,
        new_alias: &str,
    ) -> Result<(), GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let mut entry = self
            .accounts
            .get(&account_key(provider_id, account_id))
            .cloned()
            .ok_or_else(|| {
                GatewayError::Vault(format!(
                    "No account {account_id} for provider {provider_id}"
                ))
            })?;
        let trimmed = new_alias.trim();
        if trimmed.is_empty() {
            return Err(GatewayError::Vault("alias must not be empty".into()));
        }
        entry.alias = trimmed.to_string();
        let json = serde_json::to_string(&entry)
            .map_err(|e| GatewayError::Vault(format!("Failed to serialize provider entry: {}", e)))?;
        let name = account_entry_name(provider_id, account_id);
        self.vault
            .store(&name, &json)
            .map_err(|e| GatewayError::Vault(format!("Failed to update alias: {}", e)))?;
        self.accounts
            .insert(account_key(provider_id, account_id), entry);
        Ok(())
    }

    /// Replace the API key of an existing account.
    ///
    /// Same shape as `update_alias`: `account_id` is immutable, only
    /// `api_key` is rewritten in place and re-persisted to the vault.
    /// Used by the harness edit dialog so users can rotate a key
    /// without losing the account's UUID (which is the runtime
    /// addressing key for sessions).
    pub fn update_account_key(
        &mut self,
        provider_id: &str,
        account_id: &str,
        new_api_key: &str,
    ) -> Result<(), GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let mut entry = self
            .accounts
            .get(&account_key(provider_id, account_id))
            .cloned()
            .ok_or_else(|| {
                GatewayError::Vault(format!(
                    "No account {account_id} for provider {provider_id}"
                ))
            })?;
        let trimmed = new_api_key.trim();
        if trimmed.is_empty() {
            return Err(GatewayError::Vault("api_key must not be empty".into()));
        }
        entry.api_key = trimmed.to_string();
        let json = serde_json::to_string(&entry)
            .map_err(|e| GatewayError::Vault(format!("Failed to serialize provider entry: {}", e)))?;
        let name = account_entry_name(provider_id, account_id);
        self.vault
            .store(&name, &json)
            .map_err(|e| GatewayError::Vault(format!("Failed to update key: {}", e)))?;
        self.accounts
            .insert(account_key(provider_id, account_id), entry);
        Ok(())
    }


    /// Look up the first account for a provider (legacy single-key API).
    ///
    /// Used by callers that don't yet know about the account dimension —
    /// they just want "any key" for this provider. Returns the first
    /// account's `ProviderEntry` with both `account_id` and `alias`
    /// populated.
    ///
    /// Lookup order:
    /// 1. Direct: any account indexed under this provider id.
    /// 2. Inner-field match: an entry whose `provider_id` matches but
    ///    whose on-disk namespace is an alias (preserves the historical
    ///    `zhipuai` → `glm/zhipu` semantics after the namespace migration).
    /// 3. Raw alias fallback: try un-migrated `<alias>.enc` files via
    ///    `vault_key_candidates` (best-effort only; the migration runs
    ///    on every `unlock` so this branch is normally dead).
    pub fn get_provider(&self, provider: &str) -> Result<ProviderEntry, GatewayError> {
        // Build the set of "names we accept" — the query plus any alias
        // candidates the historical `vault_key_candidates` recognises.
        // This preserves the pre-multi-account semantics where `glm` and
        // `zhipuai` were interchangeable for the same canonical provider.
        let accepted: std::collections::HashSet<String> = vault_key_candidates(provider)
            .into_iter()
            .map(String::from)
            .collect();

        // 1. Direct lookup: accounts indexed under this exact id.
        if let Some(accounts) = self.by_provider.get(provider)
            && let Some(first) = accounts.first()
            && let Some(entry) = self.accounts.get(&account_key(provider, first))
        {
            return Ok(entry.clone());
        }
        // 2. Inner-field match: any account whose `provider_id` matches
        //    one of the accepted names. This is what makes `store_key(
        //    "zhipuai", …)` discoverable via `get_provider("glm")` after
        //    the namespace migration — the inner field is canonical.
        for entry in self.accounts.values() {
            if accepted.contains(&entry.provider_id) {
                return Ok(entry.clone());
            }
        }
        // 3. Best-effort fallback to legacy alias candidates on disk
        //    (only matters if `unlock` failed to migrate for some reason).
        for candidate in vault_key_candidates(provider) {
            if let Ok(secret) = self.vault.retrieve(candidate) {
                let raw = secret.expose_secret();
                if let Ok(entry) = serde_json::from_str::<ProviderEntry>(raw) {
                    return Ok(entry);
                }
                return Ok(ProviderEntry {
                    provider_id: provider.to_string(),
                    account_id: LEGACY_ACCOUNT_ID.to_string(),
                    alias: format!("{provider}-default"),
                    api_key: raw.to_string(),
                });
            }
        }
        Err(GatewayError::Vault(format!(
            "No key found for provider '{provider}'"
        )))
    }

    /// Get just the API key for a provider (one-time distribution, decrypted)
    /// Backward-compatible: works with both JSON and legacy format.
    /// Also tries alias names if the canonical ID is not found in Vault.
    pub fn get_key(&self, provider: &str) -> Result<String, GatewayError> {
        let entry = self.get_provider(provider)?;
        Ok(entry.api_key)
    }

    /// List all providers with stored keys (no values returned).
    pub fn list_providers(&self) -> Vec<String> {
        let mut providers: Vec<String> = self.by_provider.keys().cloned().collect();
        providers.sort();
        providers
    }

    /// List all accounts across all providers with masked previews.
    ///
    /// Returns one `VaultKeyEntry` per account; callers iterating this
    /// list may see multiple rows for the same `provider`. When the
    /// vault is locked every preview is masked to `"***"`.
    pub fn list_keys(&self) -> Result<Vec<VaultKeyEntry>, GatewayError> {
        let mut entries = Vec::new();
        // Iterate providers in a stable order so the API response is
        // deterministic for snapshot diffs and tests.
        let mut providers: Vec<&String> = self.by_provider.keys().collect();
        providers.sort();
        for provider in providers {
            let account_ids = match self.by_provider.get(provider) {
                Some(v) => v,
                None => continue,
            };
            for account_id in account_ids {
                let entry = match self.accounts.get(&account_key(provider, account_id)) {
                    Some(e) => e,
                    None => continue,
                };
                let preview = if self.vault.is_unlocked() {
                    let key = &entry.api_key;
                    if key.len() > 6 {
                        format!("{}...{}", &key[..3], &key[key.len() - 3..])
                    } else {
                        "***".to_string()
                    }
                } else {
                    "***".to_string()
                };
                entries.push(VaultKeyEntry {
                    provider: provider.clone(),
                    account_id: account_id.clone(),
                    alias: entry.alias.clone(),
                    key_preview: preview,
                });
            }
        }
        Ok(entries)
    }

    /// Remove **all** accounts for a provider (legacy single-key API).
    ///
    /// Use [`remove_account`] for per-account deletion in the new flow.
    /// Also cleans up any legacy alias-namespace files that happen to
    /// share the same `provider_id` field, preserving the old
    /// "zhipuai → glm/zhipu" cleanup semantics.
    pub fn remove_key(&mut self, provider: &str) -> Result<(), GatewayError> {
        let mut removed_any = false;
        let accepted: std::collections::HashSet<String> = vault_key_candidates(provider)
            .into_iter()
            .map(String::from)
            .collect();

        // 1. Drop accounts indexed under this exact provider id.
        if let Some(account_ids) = self.by_provider.remove(provider) {
            for account_id in account_ids {
                if let Some(entry) = self.accounts.remove(&account_key(provider, &account_id)) {
                    let name = account_entry_name(&entry.provider_id, &account_id);
                    if self.vault.exists(&name) {
                        self.vault.delete(&name).map_err(|e| {
                            GatewayError::Vault(format!(
                                "Failed to remove key for '{}': {}",
                                provider, e
                            ))
                        })?;
                    }
                    removed_any = true;
                }
            }
        }

        // 2. Catch accounts whose inner `provider_id` matches an alias
        //    candidate — preserves the historical "remove `glm` cleans up
        //    the `zhipuai` entry" behaviour.
        let alias_account_ids: Vec<String> = self
            .accounts
            .iter()
            .filter(|(_, e)| accepted.contains(&e.provider_id))
            .map(|(id, _)| id.clone())
            .collect();
        for account_id in alias_account_ids {
            if let Some(entry) = self.accounts.remove(&account_id) {
                let name = account_entry_name(&entry.provider_id, &account_id);
                if self.vault.exists(&name) {
                    let _ = self.vault.delete(&name);
                }
                removed_any = true;
            }
        }

        if !removed_any {
            return Err(GatewayError::Vault(format!(
                "No key found for provider '{provider}'"
            )));
        }
        Ok(())
    }

    /// Remove a single account by `(provider_id, account_id)`.
    pub fn remove_account(
        &mut self,
        provider_id: &str,
        account_id: &str,
    ) -> Result<(), GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        // Drop the in-memory entry; the on-disk file is deleted below.
        let _entry = self
            .accounts
            .remove(&account_key(provider_id, account_id))
            .ok_or_else(|| GatewayError::Vault(format!("No account {account_id}")))?;
        if let Some(list) = self.by_provider.get_mut(provider_id) {
            list.retain(|a| a != account_id);
            if list.is_empty() {
                self.by_provider.remove(provider_id);
            }
        }
        let name = account_entry_name(provider_id, account_id);
        if self.vault.exists(&name) {
            self.vault.delete(&name).map_err(|e| {
                GatewayError::Vault(format!("Failed to remove account: {e}"))
            })?;
        }
        Ok(())
    }

    // ── Embedding provider key CRUD (stored under "_embedding_" prefix) ──
    //
    // Separate namespace from the default provider namespace so embedding
    // provider keys cannot collide with or be confused with chat keys.

    const EMBEDDING_PREFIX: &str = "_embedding_";

    /// Store a cloud embedding provider API key (encrypted on disk).
    pub fn store_embedding_key(&mut self, provider: &str, api_key: &str) -> Result<(), GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let key_name = format!("{}{provider}", Self::EMBEDDING_PREFIX);
        self.vault
            .store(&key_name, api_key)
            .map_err(|e| GatewayError::Vault(format!("Failed to store embedding key: {e}")))?;
        Ok(())
    }

    /// Get a cloud embedding provider API key (decrypted).
    ///
    /// Returns `Err(Vault(...))` when no key is configured for this
    /// provider — callers should distinguish this from a locked vault.
    pub fn get_embedding_key(&self, provider: &str) -> Result<String, GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let key_name = format!("{}{provider}", Self::EMBEDDING_PREFIX);
        let secret = self
            .vault
            .retrieve(&key_name)
            .map_err(|e| GatewayError::Vault(format!("No embedding key for '{provider}': {e}")))?;
        Ok(secret.expose_secret().to_string())
    }

    /// Check whether an embedding provider has a key configured.
    /// Does NOT require the vault to be unlocked.
    pub fn has_embedding_key(&self, provider: &str) -> bool {
        let key_name = format!("{}{provider}", Self::EMBEDDING_PREFIX);
        self.vault.exists(&key_name)
    }

    /// List all configured embedding providers with masked key previews.
    /// Returns entries with provider name and masked API key (first 3 + last 3 chars).
    pub fn list_embedding_keys(&self) -> Result<Vec<VaultKeyEntry>, GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let all_keys = self
            .vault
            .list()
            .map_err(|e| GatewayError::Vault(format!("Failed to list vault keys: {e}")))?;
        let mut entries = Vec::new();
        for key_name in &all_keys {
            if let Some(provider) = key_name.strip_prefix(Self::EMBEDDING_PREFIX) {
                let preview = match self.vault.retrieve(key_name) {
                    Ok(secret) => {
                        let key = secret.expose_secret();
                        if key.len() > 6 {
                            format!("{}...{}", &key[..3], &key[key.len() - 3..])
                        } else {
                            "***".to_string()
                        }
                    }
                    Err(_) => "***".to_string(),
                };
                entries.push(VaultKeyEntry {
                    // Embedding providers have no account dimension — each
                    // provider holds at most one key. Fill the new
                    // multi-account fields with placeholders so the struct
                    // shape stays unified for the HTTP layer.
                    provider: provider.to_string(),
                    account_id: format!("embedding:{provider}"),
                    alias: String::new(),
                    key_preview: preview,
                });
            }
        }
        Ok(entries)
    }

    /// Remove a cloud embedding provider API key.
    /// Idempotent: returns Ok even if no key exists.
    pub fn remove_embedding_key(&mut self, provider: &str) -> Result<(), GatewayError> {
        if !self.vault.is_unlocked() {
            return Err(GatewayError::Vault("Vault is locked".into()));
        }
        let key_name = format!("{}{provider}", Self::EMBEDDING_PREFIX);
        if !self.vault.exists(&key_name) {
            // Idempotent — no error if key doesn't exist
            return Ok(());
        }
        self.vault
            .delete(&key_name)
            .map_err(|e| GatewayError::Vault(format!("Failed to remove embedding key: {e}")))?;
        Ok(())
    }
}

/// Unlock the vault and raise every readiness signal that depends on it.
///
/// ADR-059 Phase 5.4: this is the ONE implementation of the
/// "unlock → mark vault ready → republish" sequence. The dev-mode
/// auto-unlock task (cold start) and the HTTP `POST /api/vault/unlock`
/// handler (post-relock recovery) both call it — the plan requires the
/// relock path to reuse the cold-start code rather than duplicate it.
///
/// The Argon2id KDF is deliberately slow (~1 s), so the unlock itself
/// runs on the blocking pool via `blocking_write`; the readiness
/// transitions happen on the async side afterwards. On success the
/// vault subsystem is marked Ready, a global-resources republish is
/// triggered so Runtimes receive the now-decrypted keys, and the
/// publisher ready barrier (cold-start only) is raised if still
/// pending. On failure an `Err` is returned — callers decide whether
/// to mark the subsystem Failed (dev-mode auto-unlock) or surface an
/// HTTP error (lock/unlock API).
pub async fn unlock_vault_and_mark_ready(
    state: std::sync::Arc<tokio::sync::RwLock<crate::gateway::state::GatewayState>>,
    handle: crate::bootstrap::SubsystemHandle,
    trigger: Option<crate::mqtt::MqttPublisherTrigger>,
    password: String,
    detail: String,
) -> Result<(), String> {
    let state_for_kdf = state.clone();
    let password_for_kdf = password.clone();
    let result = tokio::task::spawn_blocking(move || {
        state_for_kdf
            .blocking_write()
            .vault
            .unlock(&password_for_kdf)
    })
    .await;
    match result {
        Ok(Ok(())) => {
            tracing::info!("Vault unlocked");
            handle.mark_ready(Some(detail));
            if let Some(ref t) = trigger {
                t.trigger();
            }
            // Cold-start only: the publisher loop may still be waiting
            // on its ready barrier; raising it now releases the first
            // retained publish with populated `api_key` fields. On a
            // later unlock (relock cycle) the handle is already ready
            // and this is a no-op.
            let gw = state.read().await;
            if let Some(ref h) = gw.mqtt_publisher_handle {
                h.mark_ready();
            }
            Ok(())
        }
        Ok(Err(e)) => Err(format!("Failed to unlock vault: {e}")),
        Err(e) => Err(format!("Vault unlock task panicked: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault_dir(name: &str) -> String {
        let dir = crate::test_support::unique_temp_dir(format!("vaultfacade-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    #[test]
    fn test_vault_locked_by_default() {
        let dir = temp_vault_dir("locked");
        let vault = VaultFacade::new(&dir);
        assert!(!vault.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_unlock() {
        let dir = temp_vault_dir("unlock");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        assert!(vault.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_lock_clears_access_but_keeps_data() {
        let dir = temp_vault_dir("lock_clear");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-lock-key").unwrap();
        assert!(vault.is_unlocked());

        // Lock: master key zeroized, provider-name cache dropped.
        vault.lock();
        assert!(!vault.is_unlocked());
        assert!(vault.get_key("openai").is_err());
        assert!(vault.list_providers().is_empty());

        // The same password restores access to the stored data.
        vault.unlock("password123").unwrap();
        assert!(vault.is_unlocked());
        assert_eq!(vault.get_key("openai").unwrap(), "sk-lock-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_lock_is_idempotent() {
        let dir = temp_vault_dir("lock_idem");
        let mut vault = VaultFacade::new(&dir);
        // Locking an already-locked (freshly constructed) vault is a
        // no-op — the relock path must never panic on double lock.
        vault.lock();
        assert!(!vault.is_unlocked());
        vault.lock();
        assert!(!vault.is_unlocked());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_and_get() {
        let dir = temp_vault_dir("store_get");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-test-key").unwrap();
        let key = vault.get_key("openai").unwrap();
        assert_eq!(key, "sk-test-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_get_locked_fails() {
        let dir = temp_vault_dir("get_locked");
        let vault = VaultFacade::new(&dir);
        let result = vault.get_key("openai");
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_locked_fails() {
        let dir = temp_vault_dir("store_locked");
        let mut vault = VaultFacade::new(&dir);
        let result = vault.store_key("openai", "sk-test-key");
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_get_missing_provider() {
        let dir = temp_vault_dir("missing");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        let result = vault.get_key("anthropic");
        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_list_providers() {
        let dir = temp_vault_dir("list");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("openai", "sk-key1").unwrap();
        vault.store_key("ollama", "").unwrap();
        let providers = vault.list_providers();
        assert_eq!(providers.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_and_get_full() {
        let dir = temp_vault_dir("store_get_full");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("deepseek", "sk-abc").unwrap();
        let key = vault.get_key("deepseek").unwrap();
        assert_eq!(key, "sk-abc");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_store_and_get_empty_key() {
        let dir = temp_vault_dir("store_get_empty");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.store_key("ollama", "").unwrap();
        let key = vault.get_key("ollama").unwrap();
        assert_eq!(key, "");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_legacy_format_compatibility() {
        let dir = temp_vault_dir("legacy");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store using the API (JSON format)
        vault.store_key("openai", "sk-legacy-key").unwrap();
        // Retrieve — should work
        let entry = vault.get_provider("openai").unwrap();
        assert_eq!(entry.api_key, "sk-legacy-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_get_provider_glm_to_zhipuai() {
        let dir = temp_vault_dir("alias_glm");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under old alias "glm"
        vault.store_key("glm", "sk-glm-key").unwrap();
        // Retrieve using canonical "zhipuai" — should find "glm" via alias
        let entry = vault.get_provider("zhipuai").unwrap();
        assert_eq!(entry.api_key, "sk-glm-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_get_provider_qwen_to_alibaba() {
        let dir = temp_vault_dir("alias_qwen");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under old alias "qwen"
        vault.store_key("qwen", "sk-qwen-key").unwrap();
        // Retrieve using canonical "alibaba" — should find "qwen" via alias
        let entry = vault.get_provider("alibaba").unwrap();
        assert_eq!(entry.api_key, "sk-qwen-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_get_provider_moonshot_to_moonshotai() {
        let dir = temp_vault_dir("alias_moonshot");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under old alias "moonshot"
        vault.store_key("moonshot", "sk-moonshot-key").unwrap();
        // Retrieve using canonical "moonshotai" — should find "moonshot" via alias
        let entry = vault.get_provider("moonshotai").unwrap();
        assert_eq!(entry.api_key, "sk-moonshot-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_canonical_takes_priority() {
        let dir = temp_vault_dir("alias_priority");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under both canonical and alias
        vault.store_key("zhipuai", "sk-canonical-key").unwrap();
        vault.store_key("glm", "sk-alias-key").unwrap();
        // Canonical should take priority
        let entry = vault.get_provider("zhipuai").unwrap();
        assert_eq!(entry.api_key, "sk-canonical-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_remove_cleans_all() {
        let dir = temp_vault_dir("alias_remove");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under both canonical and alias
        vault.store_key("zhipuai", "sk-canonical-key").unwrap();
        vault.store_key("glm", "sk-alias-key").unwrap();
        // Remove using canonical name — should clean up both
        vault.remove_key("zhipuai").unwrap();
        // Both should be gone
        assert!(vault.get_provider("zhipuai").is_err());
        assert!(vault.get_provider("glm").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_alias_reverse_lookup() {
        let dir = temp_vault_dir("alias_reverse");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        // Store under canonical "zhipuai"
        vault.store_key("zhipuai", "sk-zhipuai-key").unwrap();
        // Retrieve using old alias "glm" — should still find "zhipuai"
        let entry = vault.get_provider("glm").unwrap();
        assert_eq!(entry.api_key, "sk-zhipuai-key");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Multi-account tests ─────────────────────────────────────────

    #[test]
    fn test_vault_multi_account_per_provider() {
        let dir = temp_vault_dir("multi_account");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();

        let a1 = vault
            .add_account("openai", Some("work"), "sk-work")
            .unwrap();
        let a2 = vault
            .add_account("openai", Some("personal"), "sk-personal")
            .unwrap();
        assert_ne!(a1, a2, "each add_account must mint a fresh UUID");

        let keys = vault.list_keys().unwrap();
        let openai_keys: Vec<&VaultKeyEntry> =
            keys.iter().filter(|k| k.provider == "openai").collect();
        assert_eq!(openai_keys.len(), 2);
        let aliases: std::collections::HashSet<&str> =
            openai_keys.iter().map(|k| k.alias.as_str()).collect();
        assert!(aliases.contains("work"));
        assert!(aliases.contains("personal"));

        let work_entry = vault.get_account("openai", &a1).unwrap();
        assert_eq!(work_entry.api_key, "sk-work");
        assert_eq!(work_entry.alias, "work");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_default_alias_when_empty() {
        let dir = temp_vault_dir("default_alias");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        vault.add_account("alibaba", None, "sk-a").unwrap();
        let entry = vault.get_provider("alibaba").unwrap();
        assert_eq!(entry.alias, "alibaba-default");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_update_alias_renames_in_place() {
        let dir = temp_vault_dir("update_alias");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        let account = vault.add_account("anthropic", Some("dev"), "sk-dev").unwrap();
        vault.update_alias("anthropic", &account, "prod").unwrap();
        // account_id must not change; only the alias does.
        let entry = vault.get_account("anthropic", &account).unwrap();
        assert_eq!(entry.account_id, account);
        assert_eq!(entry.alias, "prod");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_update_account_key_rewrites_only_the_key() {
        let dir = temp_vault_dir("update_account_key");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        let account = vault
            .add_account("anthropic", Some("dev"), "sk-old")
            .unwrap();
        vault
            .update_account_key("anthropic", &account, "sk-new")
            .unwrap();
        let entry = vault.get_account("anthropic", &account).unwrap();
        // alias + account_id unchanged, key replaced.
        assert_eq!(entry.account_id, account);
        assert_eq!(entry.alias, "dev");
        assert_eq!(entry.api_key, "sk-new");
        // Empty key is rejected — caller must send a non-empty value.
        assert!(vault
            .update_account_key("anthropic", &account, "  ")
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_remove_account_only_drops_target() {
        let dir = temp_vault_dir("remove_account");
        let mut vault = VaultFacade::new(&dir);
        vault.unlock("password123").unwrap();
        let a1 = vault
            .add_account("deepseek", Some("primary"), "sk-1")
            .unwrap();
        let _a2 = vault
            .add_account("deepseek", Some("backup"), "sk-2")
            .unwrap();
        vault.remove_account("deepseek", &a1).unwrap();
        let keys = vault.list_keys().unwrap();
        let deepseek_keys: Vec<&VaultKeyEntry> =
            keys.iter().filter(|k| k.provider == "deepseek").collect();
        assert_eq!(deepseek_keys.len(), 1);
        assert_eq!(deepseek_keys[0].alias, "backup");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_migrate_legacy_plaintext_entry() {
        // Pre-refactor vault files store the bare API key string.
        // `unlock` must migrate them into `<provider>__legacy.enc` and
        // the legacy file must be removed afterwards.
        let dir = temp_vault_dir("migrate_plain");
        // Manually create a legacy entry bypassing VaultFacade.
        {
            let mut v = acowork_vault::Vault::open(std::path::Path::new(&dir)).unwrap();
            v.unlock("password123").unwrap();
            v.store("openai", "sk-legacy-plain").unwrap();
        }
        let mut facade = VaultFacade::new(&dir);
        facade.unlock("password123").unwrap();

        // After migration the legacy file is gone and the new namespace
        // file exists, with the default alias.
        let legacy_path =
            std::path::Path::new(&dir).join(format!("openai{ACCOUNT_SEP}{LEGACY_ACCOUNT_ID}.enc"));
        assert!(legacy_path.exists(), "migrated entry must exist at new namespace path");
        assert!(
            !std::path::Path::new(&dir).join("openai.enc").exists(),
            "legacy entry must be removed after migration"
        );

        let entry = facade.get_provider("openai").unwrap();
        assert_eq!(entry.api_key, "sk-legacy-plain");
        assert_eq!(entry.alias, "openai-default");
        assert_eq!(entry.account_id, LEGACY_ACCOUNT_ID);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_migrate_legacy_json_entry() {
        // Older JSON shape `{"api_key":"…"}` must also migrate cleanly.
        let dir = temp_vault_dir("migrate_json");
        {
            let mut v = acowork_vault::Vault::open(std::path::Path::new(&dir)).unwrap();
            v.unlock("password123").unwrap();
            v.store("anthropic", r#"{"api_key":"sk-legacy-json"}"#).unwrap();
        }
        let mut facade = VaultFacade::new(&dir);
        facade.unlock("password123").unwrap();
        let entry = facade.get_provider("anthropic").unwrap();
        assert_eq!(entry.api_key, "sk-legacy-json");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_legacy_accounts_are_isolated_per_provider() {
        // Two legacy entries both use the account id `"legacy"`. The
        // in-memory index must keep them apart (they used to collapse to
        // whichever was written last), and removing one must not delete
        // the other's vault file.
        let dir = temp_vault_dir("legacy_isolation");
        {
            let mut v = acowork_vault::Vault::open(std::path::Path::new(&dir)).unwrap();
            v.unlock("password123").unwrap();
            v.store("openai", "sk-openai").unwrap();
            v.store("deepseek", "sk-deepseek").unwrap();
        }
        let mut facade = VaultFacade::new(&dir);
        facade.unlock("password123").unwrap();

        let keys = facade.list_keys().unwrap();
        for (provider, alias, key) in [
            ("openai", "openai-default", "sk-openai"),
            ("deepseek", "deepseek-default", "sk-deepseek"),
        ] {
            let row = keys
                .iter()
                .find(|k| k.provider == provider)
                .unwrap_or_else(|| panic!("{provider} missing from list_keys"));
            assert_eq!(row.alias, alias, "{provider} alias must be its own");
            assert_eq!(facade.get_provider(provider).unwrap().api_key, key);
        }

        facade.remove_key("openai").unwrap();
        let remaining: Vec<String> = facade
            .list_keys()
            .unwrap()
            .iter()
            .map(|k| k.provider.clone())
            .collect();
        assert_eq!(remaining, vec!["deepseek".to_string()]);
        assert!(
            std::path::Path::new(&dir)
                .join(format!("deepseek{ACCOUNT_SEP}{LEGACY_ACCOUNT_ID}.enc"))
                .exists(),
            "removing openai must not delete deepseek's legacy account"
        );
        assert_eq!(
            facade.get_provider("deepseek").unwrap().api_key,
            "sk-deepseek"
        );

        // Simulate a Gateway restart: a fresh facade re-reads the vault
        // from disk. The removed provider must NOT come back — under the
        // old keying every delete hit another provider's file, so the
        // target was never removed and reappeared on the next unlock.
        let mut reopened = VaultFacade::new(&dir);
        reopened.unlock("password123").unwrap();
        let providers: Vec<String> = reopened
            .list_keys()
            .unwrap()
            .iter()
            .map(|k| k.provider.clone())
            .collect();
        assert_eq!(providers, vec!["deepseek".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_vault_reserved_namespaces_are_not_providers() {
        // Embedding keys share the vault directory but must not
        // be indexed as LLM providers, nor migrated into the `__legacy`
        // shape (that is how a stray `_embedding_volcengine` used to show
        // up in the configured-providers list).
        let dir = temp_vault_dir("reserved_prefix");
        {
            let mut v = acowork_vault::Vault::open(std::path::Path::new(&dir)).unwrap();
            v.unlock("password123").unwrap();
            v.store("_embedding_volcengine", "sk-embed").unwrap();
        }
        let mut facade = VaultFacade::new(&dir);
        facade.unlock("password123").unwrap();

        assert!(
            facade.list_keys().unwrap().is_empty(),
            "reserved keys must not surface as configured providers"
        );
        // Left untouched on disk: neither migrated nor deleted.
        assert!(
            std::path::Path::new(&dir)
                .join("_embedding_volcengine.enc")
                .exists()
        );

        // Still invisible after a reopen — reserved namespaces never
        // surface as providers, so there is nothing to delete here.
        let mut reopened = VaultFacade::new(&dir);
        reopened.unlock("password123").unwrap();
        assert!(reopened.list_keys().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
