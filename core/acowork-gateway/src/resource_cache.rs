//! Resource cache — versioned provider, MCP, user profile, and embedding
//! model lists for AgentHello diff sync.
//!
//! Gateway maintains four versioned resource lists on disk:
//! - `provider_list.json`: `{ version: N, providers: [ProviderListItem, ...] }`
//! - `mcp_list.json`:    `{ version: N, servers: [McpListItem, ...] }`
//!
//! These are loaded into memory at startup. HTTP handlers rebuild them
//! (version+1) when the user modifies providers or MCP catalog entries.
//! The AgentHello handler reads the in-memory cache
//! and delivers changed lists to Runtime via version-driven diff sync.
//!
//! ## Key vaults (provider_key_vault / mcp_key_vault)
//!
//! Key vaults are NOT versioned — they are always delivered in full on
//! every AgentHello. They are built on-the-fly from Vault + MCP catalog
//! (reading decrypted values) rather than cached on disk.

#[cfg(test)]
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use acowork_core::protocol::{
    CompactModelRef, EmbeddingModelsFile, McpKeyEntry, McpListItem, ProviderListItem,
    ProviderModelEntry, UserProfileListFile,
};

/// In-memory resource cache loaded at Gateway startup.
///
/// Provider, MCP, User Profile, and Embedding Model lists are versioned;
/// keys are always delivered in full and are NOT stored here (built on-the-fly from Vault).
#[derive(Debug, Clone)]
pub struct ResourceCache {
    pub provider_list: ProviderListFile,
    pub mcp_list: McpListFile,
    pub user_profile_list: UserProfileListFile,
    pub embedding_models: EmbeddingModelsFile,
}

/// Versioned provider list persisted to disk.
///
/// ADR-056: Also carries `default_compact_models` — the user's ordered
/// global picks for cross-provider distillation. Live at the top level (not
/// inside any `providers[]` entry) so they can refer to any provider by id.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[derive(Default)]
pub struct ProviderListFile {
    pub version: u64,
    pub providers: Vec<ProviderListItem>,
    /// ADR-056: Ordered global compact-model candidates. Empty = no global
    /// override (runtime degrades to provider.compact_model → chat model).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_compact_models: Vec<CompactModelRef>,
    /// Legacy single-value form (pre-list `provider_list.json`).
    /// Read-on-migrate only — see `migrate_legacy_defaults`.
    #[serde(default, skip_serializing, alias = "default_compact_model")]
    pub default_compact_model_legacy: Option<CompactModelRef>,
}

impl ProviderListFile {
    /// Fold the legacy single `default_compact_model` into the ordered list.
    /// Idempotent; called once after loading from disk.
    pub fn migrate_legacy_defaults(&mut self) {
        if let Some(legacy) = self.default_compact_model_legacy.take()
            && !self
                .default_compact_models
                .iter()
                .any(|r| r.provider_id == legacy.provider_id && r.model_id == legacy.model_id)
        {
            self.default_compact_models.insert(0, legacy);
        }
    }
}

/// Versioned MCP server list persisted to disk.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[derive(Default)]
pub struct McpListFile {
    pub version: u64,
    pub servers: Vec<McpListItem>,
}

// ── File paths ─────────────────────────────────────────────────────────

fn provider_list_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("provider_list.json")
}

fn mcp_list_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("mcp_list.json")
}


fn embedding_models_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("embedding_models.json")
}

// ── Loading ────────────────────────────────────────────────────────────

/// Load the resource cache from disk at Gateway startup.
///
/// Returns empty lists with version 0 if files don't exist.
///
/// `user_profile_list` is deliberately **not** in this list: since ADR-084
/// §决策 4b `user_profiles.json` belongs to `acowork-user`, so the Gateway
/// starts empty and fills the cache from a pull
/// ([`crate::lifecycle::user_profile_sync::refresh`]).
pub fn load_resource_cache(data_dir: &Path) -> ResourceCache {
    let provider_list = load_provider_list(data_dir);
    let mcp_list = load_mcp_list(data_dir);
    let embedding_models = load_embedding_models(data_dir);
    tracing::info!(
        provider_count = provider_list.providers.len(),
        provider_version = provider_list.version,
        mcp_count = mcp_list.servers.len(),
        mcp_version = mcp_list.version,
        embedding_model_count = embedding_models.models.len(),
        embedding_models_version = embedding_models.version,
        "Resource cache loaded"
    );
    ResourceCache {
        provider_list,
        mcp_list,
        user_profile_list: UserProfileListFile {
            version: 0,
            users: Vec::new(),
        },
        embedding_models,
    }
}

fn load_provider_list(data_dir: &Path) -> ProviderListFile {
    let path = provider_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str::<ProviderListFile>(&raw) {
            Ok(mut list) => {
                list.migrate_legacy_defaults();
                list
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse provider_list.json, using empty list"
                );
                ProviderListFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("provider_list.json not found, initializing empty");
            ProviderListFile::default()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read provider_list.json, using empty list"
            );
            ProviderListFile::default()
        }
    }
}

fn load_mcp_list(data_dir: &Path) -> McpListFile {
    let path = mcp_list_path(data_dir);
    match std::fs::read_to_string(&path) {
        Ok(raw) => match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse mcp_list.json, using empty list"
                );
                McpListFile::default()
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("mcp_list.json not found, initializing empty");
            McpListFile::default()
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to read mcp_list.json, using empty list"
            );
            McpListFile::default()
        }
    }
}



fn load_embedding_models(data_dir: &Path) -> EmbeddingModelsFile {
    let path = embedding_models_path(data_dir);

    // 1. data_dir is the user-editable copy (always wins when present).
    if let Ok(raw) = std::fs::read_to_string(&path) {
        return match serde_json::from_str(&raw) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to parse embedding_models.json, using empty list"
                );
                EmbeddingModelsFile {
                    version: 0,
                    models: Vec::new(),
                }
            }
        };
    }

    // 2. Bundled copy lives next to the gateway binary. Whoever
    //    distributes the binary (dev build script, package installer,
    //    Tauri bundler) is responsible for placing it there.
    let bundled = bundled_embedding_models_path();
    if let Some(ref bundled_path) = bundled
        && let Ok(raw) = std::fs::read_to_string(bundled_path) {
            match serde_json::from_str::<EmbeddingModelsFile>(&raw) {
                Ok(list) => {
                    tracing::info!(
                        source = %bundled_path.display(),
                        count = list.models.len(),
                        "Loaded embedding_models.json from bundled location"
                    );
                    // Auto-seed to data_dir so the user gets a writable copy.
                    if let Err(e) = std::fs::write(&path, &raw) {
                        tracing::warn!(
                            dest = %path.display(),
                            error = %e,
                            "Failed to seed embedding_models.json into data_dir"
                        );
                    } else {
                        tracing::info!(
                            dest = %path.display(),
                            "Auto-seeded embedding_models.json into data_dir (editable)"
                        );
                    }
                    return list;
                }
                Err(e) => {
                    tracing::warn!(
                        path = %bundled_path.display(),
                        error = %e,
                        "Failed to parse bundled embedding_models.json, using empty list"
                    );
                    return EmbeddingModelsFile {
                        version: 0,
                        models: Vec::new(),
                    };
                }
            }
        }

    tracing::error!(
        data_dir = %path.display(),
        bundled = %bundled.map(|p| p.display().to_string()).unwrap_or_else(|| "<unresolved>".to_string()),
        "embedding_models.json not found. Dev: run dev/build_core.sh after building. Release: reinstall the package."
    );
    EmbeddingModelsFile {
        version: 0,
        models: Vec::new(),
    }
}

/// Path to the bundled `embedding_models.json` next to the running gateway binary.
fn bundled_embedding_models_path() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .map(|d| d.join("embedding_models.json"))
}

// ── Saving ─────────────────────────────────────────────────────────────

/// Save the provider list to disk.
pub fn save_provider_list(data_dir: &Path, list: &ProviderListFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize provider list: {}", e))?;
    std::fs::write(provider_list_path(data_dir), &json)
        .map_err(|e| format!("Failed to write provider_list.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.providers.len(),
        "Provider list saved"
    );
    Ok(())
}

/// ADR-056: Validate that `(provider_id, model_id)` points to an existing
/// model inside `providers[]`. Returns `Ok(())` on success, or an `Err`
/// describing the mismatch — used by HTTP handlers to reject malformed
/// `default_compact_model` updates with HTTP 422.
pub fn validate_compact_model_ref(
    list: &ProviderListFile,
    r: &CompactModelRef,
) -> Result<(), String> {
    let Some(provider) = list.providers.iter().find(|p| p.id == r.provider_id) else {
        return Err(format!(
            "default_compact_model: provider_id '{}' not found in provider_list",
            r.provider_id
        ));
    };
    if !provider.models.iter().any(|m| m.id == r.model_id) {
        return Err(format!(
            "default_compact_model: model_id '{}' is not a model of provider '{}'",
            r.model_id, r.provider_id
        ));
    }
    Ok(())
}

/// ADR-056: Set (or clear) the ordered global compact-model candidate list
/// on the in-memory `ProviderListFile`, bumping `version` so MQTT retained
/// republish fires.
///
/// The incoming list is validated entry-by-entry and de-duplicated by
/// `(provider_id, model_id)` (first occurrence wins, order preserved).
/// `value = []` clears the global override (Runtime then degrades to the
/// legacy provider.compact_model → chat-model fallback chain).
///
/// Returns the previous list on success; `Err` is returned when any entry
/// does not point to an existing model. The in-memory state is **not**
/// mutated on error — caller decides whether to persist.
pub fn set_default_compact_models(
    list: &mut ProviderListFile,
    value: Vec<CompactModelRef>,
) -> Result<Vec<CompactModelRef>, String> {
    for r in &value {
        validate_compact_model_ref(list, r)?;
    }
    let mut seen = std::collections::HashSet::new();
    let deduped: Vec<CompactModelRef> = value
        .into_iter()
        .filter(|r| seen.insert((r.provider_id.clone(), r.model_id.clone())))
        .collect();
    let prev = std::mem::take(&mut list.default_compact_models);
    list.default_compact_models = deduped;
    list.version = list.version.saturating_add(1);
    tracing::info!(
        new = ?list.default_compact_models,
        prev = ?prev,
        version = list.version,
        "default_compact_models updated"
    );
    Ok(prev)
}

/// Save the MCP list to disk.
pub fn save_mcp_list(data_dir: &Path, list: &McpListFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize MCP list: {}", e))?;
    std::fs::write(mcp_list_path(data_dir), &json)
        .map_err(|e| format!("Failed to write mcp_list.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.servers.len(),
        "MCP list saved"
    );
    Ok(())
}


/// Save the embedding models list to disk.
pub fn save_embedding_models(data_dir: &Path, list: &EmbeddingModelsFile) -> Result<(), String> {
    let json = serde_json::to_string_pretty(list)
        .map_err(|e| format!("Failed to serialize embedding models: {}", e))?;
    std::fs::write(embedding_models_path(data_dir), &json)
        .map_err(|e| format!("Failed to write embedding_models.json: {}", e))?;
    tracing::info!(
        version = list.version,
        count = list.models.len(),
        "Embedding models saved"
    );
    Ok(())
}

// ── Provider list manipulation ────────────────────────────────────────

/// Build a `ProviderListItem` from user-provided configuration.
///
/// Capabilities are sourced from offline_providers.json (models.dev).
/// Protocol type and default base_url are also looked up from offline data.
/// User-provided `base_url` overrides the default when present.
/// When `custom` is true, the provider is user-defined: protocol defaults to
/// OpenAI-compatible and offline data lookup is skipped.
pub(crate) fn build_provider_list_item(
    name: &str,
    base_url_override: Option<&str>,
    model_ids: &[String],
    compact_model: Option<&str>,
    max_output_tokens: u64,
    custom: bool,
) -> ProviderListItem {
    use acowork_core::protocol::ProtocolType;

    let (protocol_type, api_base_url) = if custom {
        // Custom providers always use OpenAI-compatible protocol;
        // base_url must be provided by the user.
        (ProtocolType::OpenAI, None)
    } else {
        crate::http::models_api::lookup_protocol_info(name, None)
    };
    let base_url = base_url_override
        .filter(|u| !u.is_empty())
        .map(|u| u.to_string())
        .or(api_base_url)
        .unwrap_or_default();

    let models: Vec<ProviderModelEntry> = model_ids
        .iter()
        .map(|model_id| {
            let capabilities = if custom {
                // Custom providers have no offline data — use sensible defaults.
                // The /v1/models endpoint of OpenAI-compatible APIs does not
                // return modality info, so we default to text-only (the common
                // case for LLMs). User-provided capabilities overrides are
                // merged later via merge_user_capabilities in add_provider /
                // update_provider.
                acowork_core::protocol::ModelCapabilitiesInfo {
                    context_window: 128_000,
                    max_output_tokens: 16_384,
                    max_input_tokens: None,
                    supports_tool_calling: true,
                    supports_reasoning: None,
                    supports_attachment: None,
                    supports_temperature: None,
                    cost: None,
                    modalities: Some(acowork_core::protocol::ModelModalities {
                        input: vec!["text".to_string()],
                        output: vec!["text".to_string()],
                    }),
                    name: None,
                    family: None,
                    knowledge_cutoff: None,
                    default_reasoning_effort: None,
                    thinking_mode: None,
                    reasoning_efforts: None,
                }
            } else {
                crate::http::models_api::lookup_model_capabilities(name, model_id)
                    .unwrap_or(acowork_core::protocol::ModelCapabilitiesInfo {
                        context_window: 128_000,
                        max_output_tokens: 16_384,
                        max_input_tokens: None,
                        supports_tool_calling: true,
                        supports_reasoning: None,
                        supports_attachment: None,
                        supports_temperature: None,
                        cost: None,
                        modalities: None,
                        name: None,
                        family: None,
                        knowledge_cutoff: None,
                        default_reasoning_effort: None,
                        thinking_mode: None,
                        reasoning_efforts: None,
                    })
            };
            ProviderModelEntry {
                id: model_id.clone(),
                capabilities,
                max_output_tokens_limit: max_output_tokens,
                protocol_type: None,
                base_url: None,
                protocol_probe: None,
            }
        })
        .collect();

    ProviderListItem {
        id: name.to_string(),
        base_url,
        protocol_type,
        models,
        compact_model: compact_model.map(|s| s.to_string()),
        custom,
    }
}

/// Persist the current in-memory `provider_list` to disk and bump its version.
///
/// Callers MUST modify `gw.resource_cache.provider_list.providers` BEFORE
/// calling this function. This function only saves and bumps the version.
pub(crate) fn persist_provider_cache(
    gw: &mut crate::gateway::state::GatewayState,
    data_dir: &Path,
) {
    let new_version = gw.resource_cache.provider_list.version.wrapping_add(1);
    gw.resource_cache.provider_list.version = new_version;
    let list = gw.resource_cache.provider_list.clone();

    if let Err(e) = save_provider_list(data_dir, &list) {
        tracing::error!(error = %e, "Failed to save provider_list.json");
    }
}

/// Remove a provider from the in-memory provider list (caller must persist afterwards).
pub(crate) fn remove_provider_from_memory(
    gw: &mut crate::gateway::state::GatewayState,
    provider_name: &str,
) {
    gw.resource_cache
        .provider_list
        .providers
        .retain(|p| p.id != provider_name);
}

/// Rebuild mcp_list.json from MCP catalog entries and update in-memory cache.
///
/// Called by mcp_catalog_api.rs handlers after catalog add/update/delete.
pub fn rebuild_and_save_mcp_cache(
    gw: &mut crate::gateway::state::GatewayState,
    data_dir: &Path,
    catalog: &[acowork_core::protocol::McpServerConfigDef],
) {
    let servers = build_mcp_list_from_catalog(catalog);
    let new_version = gw.resource_cache.mcp_list.version.wrapping_add(1);
    let new_list = McpListFile {
        version: new_version,
        servers,
    };

    if let Err(e) = save_mcp_list(data_dir, &new_list) {
        tracing::error!(error = %e, "Failed to save mcp_list.json after catalog change");
    }
    gw.resource_cache.mcp_list = new_list;
}

/// Convert MCP catalog entries (McpServerConfigDef) to McpListItem entries.
///
/// MCP keys are built on-the-fly by extracting env vars and headers that
/// contain credentials (api_key, token, etc).
pub fn build_mcp_list_from_catalog(
    catalog: &[acowork_core::protocol::McpServerConfigDef],
) -> Vec<McpListItem> {
    catalog
        .iter()
        .map(|def| McpListItem {
            id: def.name.clone(),
            name: def.name.clone(),
            transport: def.transport.clone(),
            url: def.url.clone(),
            command: def.command.clone(),
            args: def.args.clone(),
            env: def.env.clone(),
            headers: def.headers.clone(),
            tool_timeout_secs: def.tool_timeout_secs,
        })
        .collect()
}

/// Build MCP key vault from catalog entries.
///
/// Extracts potential API keys from env vars and headers.
pub fn build_mcp_key_vault(
    catalog: &[acowork_core::protocol::McpServerConfigDef],
) -> Vec<McpKeyEntry> {
    catalog
        .iter()
        .map(|def| {
            // Extract api key from env vars or headers
            let api_key = extract_api_key_from_mcp_config(def);
            McpKeyEntry {
                mcp_id: def.name.clone(),
                api_key,
            }
        })
        .collect()
}

/// Try to extract an API key from MCP server config env vars and headers.
pub fn extract_api_key_from_mcp_config(
    config: &acowork_core::protocol::McpServerConfigDef,
) -> Option<String> {
    let key_patterns = ["api_key", "api-key", "token", "auth", "secret", "password"];

    // Check env vars
    for (k, v) in &config.env {
        let lower = k.to_lowercase();
        if key_patterns.iter().any(|p| lower.contains(p)) && !v.is_empty() {
            return Some(v.clone());
        }
    }
    // Check headers
    for (k, v) in &config.headers {
        let lower = k.to_lowercase();
        if key_patterns.iter().any(|p| lower.contains(p)) && !v.is_empty() {
            return Some(v.clone());
        }
    }
    None
}

// ── Defaults ───────────────────────────────────────────────────────────




impl Default for ResourceCache {
    fn default() -> Self {
        Self {
            provider_list: ProviderListFile::default(),
            mcp_list: McpListFile::default(),
            user_profile_list: UserProfileListFile {
                version: 0,
                users: Vec::new(),
            },
            embedding_models: EmbeddingModelsFile {
                version: 0,
                models: Vec::new(),
            },
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("acowork-test-resource-cache-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_default_provider_list() {
        let list = ProviderListFile::default();
        assert_eq!(list.version, 0);
        assert!(list.providers.is_empty());
    }

    #[test]
    fn test_save_and_load_provider_list() {
        let dir = temp_dir("save-provider");
        let list = ProviderListFile {
            version: 1,
            default_compact_models: Vec::new(), default_compact_model_legacy: None,
            providers: vec![ProviderListItem {
                id: "openai".to_string(),
                base_url: "https://api.openai.com/v1".to_string(),
                protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                compact_model: None,
                custom: false,
                models: vec![ProviderModelEntry {
                    id: "gpt-4o".to_string(),
                    capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                        context_window: 128000,
                        max_output_tokens: 16384,
                        max_input_tokens: Some(120000),
                        supports_tool_calling: true,
                        supports_reasoning: None,
                        supports_attachment: Some(true),
                        supports_temperature: None,
                        cost: None,
                        modalities: None,
                        name: Some("GPT-4o".to_string()),
                        family: Some("gpt".to_string()),
                        knowledge_cutoff: Some("2025-04".to_string()),
                        default_reasoning_effort: None,
                        thinking_mode: None,
                        reasoning_efforts: None,
                    },
                    max_output_tokens_limit: 32768,
                    protocol_type: None,
                    base_url: None,
                    protocol_probe: None,
                }],
            }],
        };

        save_provider_list(&dir, &list).unwrap();
        let loaded = load_provider_list(&dir);
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.providers.len(), 1);
        assert_eq!(loaded.providers[0].id, "openai");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_nonexistent_provider_list() {
        let dir = temp_dir("nonexistent-provider");
        let loaded = load_provider_list(&dir);
        assert_eq!(loaded.version, 0);
        assert!(loaded.providers.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_and_load_mcp_list() {
        let dir = temp_dir("save-mcp");
        let list = McpListFile {
            version: 2,
            servers: vec![McpListItem {
                id: "github".to_string(),
                name: "GitHub MCP".to_string(),
                transport: acowork_core::protocol::McpTransportDef::Stdio,
                url: None,
                command: "npx".to_string(),
                args: vec![
                    "-y".to_string(),
                    "@modelcontextprotocol/server-github".to_string(),
                ],
                env: HashMap::from([("GITHUB_TOKEN".to_string(), "ghp_xxx".to_string())]),
                headers: HashMap::new(),
                tool_timeout_secs: Some(30),
            }],
        };

        save_mcp_list(&dir, &list).unwrap();
        let loaded = load_mcp_list(&dir);
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.servers.len(), 1);
        assert_eq!(loaded.servers[0].name, "GitHub MCP");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_build_mcp_list_from_catalog() {
        let defs = vec![acowork_core::protocol::McpServerConfigDef {
            name: "test-server".to_string(),
            display_name: None,
            transport: acowork_core::protocol::McpTransportDef::Stdio,
            command: "node".to_string(),
            args: vec!["server.js".to_string()],
            ..Default::default()
        }];
        let items = build_mcp_list_from_catalog(&defs);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "test-server");
    }

    #[test]
    fn test_extract_api_key_from_mcp() {
        let config = acowork_core::protocol::McpServerConfigDef {
            name: "test".to_string(),
            display_name: None,
            env: HashMap::from([
                ("API_KEY".to_string(), "secret-123".to_string()),
                ("OTHER_VAR".to_string(), "visible".to_string()),
            ]),
            ..Default::default()
        };
        let key = extract_api_key_from_mcp_config(&config);
        assert_eq!(key, Some("secret-123".to_string()));
    }

    #[test]
    fn test_extract_api_key_from_headers() {
        let config = acowork_core::protocol::McpServerConfigDef {
            name: "test".to_string(),
            display_name: None,
            headers: HashMap::from([("Authorization".to_string(), "Bearer token-456".to_string())]),
            ..Default::default()
        };
        let key = extract_api_key_from_mcp_config(&config);
        assert_eq!(key, Some("Bearer token-456".to_string()));
    }

    #[test]
    fn test_load_resource_cache() {
        let dir = temp_dir("load-cache");
        let cache = load_resource_cache(&dir);
        assert_eq!(cache.provider_list.version, 0);
        assert_eq!(cache.mcp_list.version, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ADR-056: default_compact_model setter tests ────────────────────

    /// Build a small `ProviderListFile` fixture for setter tests.
    /// Two providers (`ollama` and `deepseek`) with distinct model lists.
    fn fixture_provider_list() -> ProviderListFile {
        ProviderListFile {
            version: 7,
            default_compact_models: Vec::new(), default_compact_model_legacy: None,
            providers: vec![
                ProviderListItem {
                    id: "ollama".to_string(),
                    base_url: "http://localhost:11434/v1".to_string(),
                    protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                    compact_model: None,
                    custom: false,
                    models: vec![
                        ProviderModelEntry {
                            id: "qwen2.5:0.5b".to_string(),
                            capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                                context_window: 32_000,
                                ..fixture_min_caps()
                            },
                            max_output_tokens_limit: 32_768,
                            protocol_type: None,
                            base_url: None,
                            protocol_probe: None,
                        },
                        ProviderModelEntry {
                            id: "llama3:8b".to_string(),
                            capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                                context_window: 8_000,
                                ..fixture_min_caps()
                            },
                            max_output_tokens_limit: 16_384,
                            protocol_type: None,
                            base_url: None,
                            protocol_probe: None,
                        },
                    ],
                },
                ProviderListItem {
                    id: "deepseek".to_string(),
                    base_url: "https://api.deepseek.com/v1".to_string(),
                    protocol_type: acowork_core::protocol::ProtocolType::OpenAI,
                    compact_model: Some("deepseek-v4-flash".to_string()),
                    custom: false,
                    models: vec![ProviderModelEntry {
                        id: "deepseek-v4-flash".to_string(),
                        capabilities: acowork_core::protocol::ModelCapabilitiesInfo {
                            context_window: 64_000,
                            ..fixture_min_caps()
                        },
                        max_output_tokens_limit: 32_768,
                        protocol_type: None,
                        base_url: None,
                        protocol_probe: None,
                    }],
                },
            ],
        }
    }

    fn fixture_min_caps() -> acowork_core::protocol::ModelCapabilitiesInfo {
        acowork_core::protocol::ModelCapabilitiesInfo {
            context_window: 0,
            max_output_tokens: 4096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
            reasoning_efforts: None,
        }
    }

    #[test]
    fn test_load_old_provider_list_without_default_compact() {
        // Legacy `provider_list.json` without any compact-model field must
        // deserialize to an empty list (no migration needed).
        let dir = temp_dir("old-provider-list");
        let raw = r#"{
            "version": 42,
            "providers": [
                {
                    "id": "legacy",
                    "base_url": "https://example.invalid/v1",
                    "protocol_type": "openai",
                    "custom": false,
                    "models": []
                }
            ]
        }"#;
        std::fs::write(provider_list_path(&dir), raw).unwrap();
        let loaded = load_provider_list(&dir);
        assert_eq!(loaded.version, 42);
        assert_eq!(loaded.providers.len(), 1);
        assert!(loaded.default_compact_models.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_legacy_single_value_migrates_to_list() {
        // Old on-disk shape: `"default_compact_model": {...}` (single object).
        // load_provider_list must fold it into `default_compact_models` as
        // the head element, and the legacy field must not survive.
        let dir = temp_dir("legacy-single-migrate");
        let raw = r#"{
            "version": 9,
            "default_compact_model": {"provider_id": "ollama", "model_id": "qwen2.5:0.5b"},
            "providers": [
                {
                    "id": "ollama",
                    "base_url": "http://localhost:11434/v1",
                    "protocol_type": "openai",
                    "custom": false,
                    "models": []
                }
            ]
        }"#;
        std::fs::write(provider_list_path(&dir), raw).unwrap();
        let loaded = load_provider_list(&dir);
        assert_eq!(loaded.default_compact_models.len(), 1);
        assert_eq!(loaded.default_compact_models[0].provider_id, "ollama");
        assert_eq!(loaded.default_compact_models[0].model_id, "qwen2.5:0.5b");
        assert!(loaded.default_compact_model_legacy.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn cm(provider: &str, model: &str) -> CompactModelRef {
        CompactModelRef {
            provider_id: provider.to_string(),
            model_id: model.to_string(),
        }
    }

    #[test]
    fn test_set_default_compact_models_happy_path() {
        let mut list = fixture_provider_list();
        let prev_version = list.version;
        let prev = set_default_compact_models(
            &mut list,
            vec![cm("ollama", "qwen2.5:0.5b"), cm("deepseek", "deepseek-v4-flash")],
        )
        .expect("both refs are valid");

        assert!(prev.is_empty());
        assert_eq!(list.default_compact_models.len(), 2);
        assert_eq!(list.default_compact_models[0].provider_id, "ollama");
        // Order preserved — the list IS the priority order.
        assert_eq!(list.default_compact_models[1].provider_id, "deepseek");
        assert!(list.version > prev_version, "version must monotonically increase");
    }

    #[test]
    fn test_set_default_compact_models_dedup_preserves_first_order() {
        let mut list = fixture_provider_list();
        set_default_compact_models(
            &mut list,
            vec![
                cm("deepseek", "deepseek-v4-flash"),
                cm("ollama", "qwen2.5:0.5b"),
                cm("deepseek", "deepseek-v4-flash"), // duplicate — dropped
            ],
        )
        .unwrap();
        assert_eq!(list.default_compact_models.len(), 2);
        assert_eq!(list.default_compact_models[0].provider_id, "deepseek");
        assert_eq!(list.default_compact_models[1].provider_id, "ollama");
    }

    #[test]
    fn test_set_default_compact_models_persists_and_round_trips() {
        let dir = temp_dir("default-compact-persist");
        let mut list = fixture_provider_list();
        set_default_compact_models(
            &mut list,
            vec![cm("deepseek", "deepseek-v4-flash"), cm("ollama", "llama3:8b")],
        )
        .unwrap();
        save_provider_list(&dir, &list).unwrap();

        let loaded = load_provider_list(&dir);
        assert_eq!(
            loaded.default_compact_models,
            vec![cm("deepseek", "deepseek-v4-flash"), cm("ollama", "llama3:8b")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_set_default_compact_models_unknown_provider_is_rejected() {
        let mut list = fixture_provider_list();
        let result = set_default_compact_models(
            &mut list,
            vec![cm("ollama", "qwen2.5:0.5b"), cm("anthropic", "claude-3-5-sonnet")],
        );
        assert!(result.is_err(), "unknown provider_id must reject the whole list");
        // State must NOT be mutated on error.
        assert!(list.default_compact_models.is_empty());
        assert_eq!(list.version, fixture_provider_list().version);
    }

    #[test]
    fn test_set_default_compact_models_model_not_in_provider_is_rejected() {
        // Cross-provider sanity: `qwen2.5:0.5b` belongs to ollama, not deepseek.
        let mut list = fixture_provider_list();
        let result = set_default_compact_models(&mut list, vec![cm("deepseek", "qwen2.5:0.5b")]);
        assert!(result.is_err(), "model from another provider must be rejected");
        assert!(list.default_compact_models.is_empty());
    }

    #[test]
    fn test_set_default_compact_models_clear_with_empty_vec() {
        let mut list = fixture_provider_list();
        set_default_compact_models(&mut list, vec![cm("ollama", "qwen2.5:0.5b")]).unwrap();
        assert_eq!(list.default_compact_models.len(), 1);

        let prev = set_default_compact_models(&mut list, Vec::new()).unwrap();
        assert_eq!(prev, vec![cm("ollama", "qwen2.5:0.5b")]);
        assert!(list.default_compact_models.is_empty());
    }

    #[test]
    fn test_set_default_compact_models_version_monotonic_under_repeat() {
        let mut list = fixture_provider_list();
        let v0 = list.version;
        let refs = vec![cm("ollama", "qwen2.5:0.5b")];
        set_default_compact_models(&mut list, refs.clone()).unwrap();
        let v1 = list.version;
        assert!(v1 > v0);
        set_default_compact_models(&mut list, refs).unwrap();
        let v2 = list.version;
        assert!(v2 > v1);
    }
}
