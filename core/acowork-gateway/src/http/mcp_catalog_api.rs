//! MCP Catalog HTTP API handlers
//!
//! Manages the global MCP server catalog — a shared registry of server
//! definitions (including credentials/API keys) that all agents can
//! selectively activate. Analogous to the Vault for LLM providers.
//!
//! - GET    /api/mcp-catalog         — list all catalog entries (env values masked)
//! - PUT    /api/mcp-catalog         — replace the entire catalog
//! - POST   /api/mcp-catalog         — add a single server entry
//! - POST   /api/mcp-catalog/from-declaration — materialize a package
//!   `[[mcp_servers]]` declaration (or a user-picked preset) into the
//!   catalog, ADR-104
//! - DELETE /api/mcp-catalog/{name}   — remove a server entry
//!
//! ADR-090: install execution moved to the Agent (Runtime) process —
//! the Gateway is registry + key vault only. The former
//! `/api/mcp-catalog/install*` endpoints are removed. Runtime health
//! checks (probing) are likewise removed: a stdio probe spawns a process
//! on whatever machine the Gateway runs on, which is meaningless once
//! Runtimes live on arbitrary Nodes. Server usability is verified by the
//! Agent at first enable (see `acowork-runtime/src/mcp_ensure.rs`).

use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::http::routes::{ApiError, AppState, OperationAck};
use crate::resource_cache;
use acowork_core::operation::{OperationRecord, OperationState};
use acowork_core::protocol::McpServerConfigDef;

/// Build the MCP catalog router
pub fn mcp_catalog_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/mcp-catalog",
            get(list_catalog)
                .put(replace_catalog)
                .post(add_catalog_entry),
        )
        .route(
            "/api/mcp-catalog/from-declaration",
            post(materialize_declaration),
        )
        .route(
            "/api/mcp-catalog/{name}",
            delete(remove_catalog_entry).put(update_catalog_entry),
        )
}

// ── Persistence helpers ──────────────────────────────────────────────

/// Build the path to the MCP catalog file.
fn catalog_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("mcp_catalog.json")
}

/// Load the MCP catalog from disk.
/// Returns an empty Vec if the file does not exist.
pub fn load_mcp_catalog(data_dir: &std::path::Path) -> Result<Vec<McpServerConfigDef>, String> {
    let path = catalog_path(data_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw =
        std::fs::read_to_string(&path).map_err(|e| format!("Failed to read MCP catalog: {}", e))?;
    serde_json::from_str(&raw).map_err(|e| format!("Failed to parse MCP catalog: {}", e))
}

/// Save the MCP catalog to disk.
pub fn save_mcp_catalog(
    data_dir: &std::path::Path,
    catalog: &[McpServerConfigDef],
) -> Result<(), String> {
    let json = serde_json::to_string_pretty(catalog)
        .map_err(|e| format!("Failed to serialize MCP catalog: {}", e))?;
    std::fs::write(catalog_path(data_dir), json)
        .map_err(|e| format!("Failed to write MCP catalog: {}", e))?;
    tracing::info!(count = catalog.len(), "MCP catalog saved");
    Ok(())
}

// ── Masking helper ───────────────────────────────────────────────────

/// A field key (env var name or HTTP header name) whose value is a secret and
/// must be masked in API responses. `authorization` is included so a Bearer
/// token header is treated as sensitive the same way an `*_API_KEY` env is.
fn is_sensitive_key(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        "key",
        "token",
        "secret",
        "password",
        "authorization",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
}

/// Replace sensitive values in a key/value map with "••••" (see is_sensitive_key).
fn mask_sensitive_map(
    map: &std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    map.iter()
        .map(|(k, v)| {
            (
                k.clone(),
                if is_sensitive_key(k) {
                    "••••".to_string()
                } else {
                    v.clone()
                },
            )
        })
        .collect()
}

/// Whether a config holds any secret (a sensitive env key or header key) that
/// GET masks and PUT preserves — drives the "has API key" badge in the UI.
fn has_sensitive_fields(config: &McpServerConfigDef) -> bool {
    config.env.keys().any(|k| is_sensitive_key(k))
        || config.headers.keys().any(|k| is_sensitive_key(k))
}

/// Mask sensitive env values for API responses.
/// Returns a copy of the config with env values containing "key", "token",
/// "secret", or "password" in their key name replaced with "•••". Sensitive
/// HTTP header values are masked the same way.
fn mask_sensitive_env(config: &McpServerConfigDef) -> McpServerConfigDef {
    let masked_env = mask_sensitive_map(&config.env);
    let masked_headers = mask_sensitive_map(&config.headers);

    McpServerConfigDef {
        name: config.name.clone(),
        display_name: config.display_name.clone(),
        transport: config.transport.clone(),
        url: config.url.clone(),
        command: config.command.clone(),
        args: config.args.clone(),
        env: masked_env,
        headers: masked_headers,
        tool_timeout_secs: config.tool_timeout_secs,
        install: config.install.clone(),
        required_secrets: config.required_secrets.clone(),
        // Masking hides secret *values*; provenance is not a secret and
        // dropping it would make the masked read-back look hand-added.
        origin: config.origin.clone(),
    }
}

// ── Response types ───────────────────────────────────────────────────

/// Catalog entry response (env values with sensitive fields masked)
#[derive(Serialize)]
pub struct McpCatalogEntryResponse {
    #[serde(flatten)]
    pub config: McpServerConfigDef,
    /// Whether this entry has sensitive env vars that are masked
    pub has_secrets: bool,
}

/// Full catalog response
#[derive(Serialize)]
pub struct McpCatalogResponse {
    pub servers: Vec<McpCatalogEntryResponse>,
}

/// Request to add a single MCP server entry
#[derive(Deserialize)]
pub struct AddCatalogEntryRequest {
    #[serde(flatten)]
    pub config: McpServerConfigDef,
    /// ADR-059 §7.3: the BootstrapState `version` the client read
    /// before writing (optimistic concurrency — stale clients are
    /// rejected with `resource_version_conflict`). Absent → no
    /// precondition.
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// Body of `POST /api/mcp-catalog/from-declaration` (ADR-104).
///
/// Mirrors exactly what a `[[mcp_servers]]` declaration can express — no more,
/// so a client cannot smuggle in a field the manifest cannot state. Exactly one
/// of `preset_id` / `inline` must be present, mirroring the manifest's
/// three-form mutual exclusion (`hosted` declarations need no materialization:
/// the platform already publishes those servers into every agent's catalog).
#[derive(Debug, Clone, Deserialize)]
pub struct MaterializeDeclarationRequest {
    /// Catalog key — `McpServerDecl::key()`, the join key between the
    /// manifest, `agent_mcp.json`, and the catalog.
    pub key: String,
    /// Preset form: `[[mcp_servers]] preset = "<id>"`.
    #[serde(default)]
    pub preset_id: Option<String>,
    /// Inline form: `[[mcp_servers]] transport = "http"` + `url`.
    #[serde(default)]
    pub inline: Option<InlineDeclaration>,
    /// Package that recommended this server. Selects the `origin` value so the
    /// catalog can answer "why is this entry here?".
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Credentials typed by the user **before** the entry existed (the preset
    /// picker asks for API keys up front). Only honored on the creating call:
    /// once the entry exists this field is ignored, because overwriting a
    /// stored secret on a re-run would be data loss.
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// ADR-059 §7.3 optimistic-concurrency precondition.
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// The inline (self-contained remote) half of a declaration.
#[derive(Debug, Clone, Deserialize)]
pub struct InlineDeclaration {
    pub transport: acowork_core::protocol::McpTransportDef,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub required_secrets: Vec<String>,
}

impl MaterializeDeclarationRequest {
    /// Resolve the declaration into the catalog entry it denotes.
    fn to_catalog_entry(&self) -> Result<McpServerConfigDef, String> {
        if self.key.is_empty() {
            return Err("declaration key must not be empty".to_string());
        }
        let origin = match self.agent_id.as_deref() {
            Some(agent_id) if !agent_id.is_empty() => {
                crate::http::mcp_presets::package_origin(agent_id, &self.key)
            }
            _ => match self.preset_id.as_deref() {
                Some(id) => crate::http::mcp_presets::preset_origin(id),
                None => "user".to_string(),
            },
        };

        match (&self.preset_id, &self.inline) {
            (Some(_), Some(_)) => {
                Err("a declaration is either a preset or inline, not both".to_string())
            }
            (None, None) => Err(
                "a declaration must name a preset or carry an inline remote definition".to_string(),
            ),
            (Some(id), None) => {
                if id != &self.key {
                    return Err(format!(
                        "declaration key '{}' must equal the preset id '{id}' — the key is how                          the manifest, agent_mcp.json and the catalog join",
                        self.key
                    ));
                }
                let preset = crate::http::mcp_presets::preset(id)
                    .ok_or_else(|| format!("unknown MCP preset '{id}'"))?;
                let mut entry = preset.to_catalog_entry(&origin);
                entry.env = self.env.clone();
                Ok(entry)
            }
            (None, Some(inline)) => {
                let url = inline
                    .url
                    .clone()
                    .filter(|u| !u.is_empty())
                    .ok_or_else(|| "an inline declaration needs a url".to_string())?;
                Ok(McpServerConfigDef {
                    name: self.key.clone(),
                    display_name: None,
                    transport: inline.transport.clone(),
                    url: Some(url),
                    command: String::new(),
                    args: Vec::new(),
                    env: Default::default(),
                    headers: inline.headers.clone(),
                    tool_timeout_secs: None,
                    install: None,
                    required_secrets: inline.required_secrets.clone(),
                    origin: Some(origin),
                })
            }
        }
    }
}

/// Request to update a single MCP server entry
#[derive(Deserialize)]
pub struct UpdateCatalogEntryRequest {
    #[serde(flatten)]
    pub config: McpServerConfigDef,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/mcp-catalog` — list all MCP server definitions (env values masked)
pub async fn list_catalog(
    State(state): State<AppState>,
) -> Result<Json<McpCatalogResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let servers: Vec<McpCatalogEntryResponse> = catalog
        .iter()
        .map(|c| {
            let masked = mask_sensitive_env(c);
            let has_secrets = has_sensitive_fields(c);
            McpCatalogEntryResponse {
                config: masked,
                has_secrets,
            }
        })
        .collect();

    Ok(Json(McpCatalogResponse { servers }))
}

/// `PUT /api/mcp-catalog` — replace the entire catalog
pub async fn replace_catalog(
    State(state): State<AppState>,
    Json(new_catalog): Json<Vec<McpServerConfigDef>>,
) -> Result<Json<McpCatalogResponse>, ApiError> {
    // Validate: no duplicate names
    let mut seen = std::collections::HashSet::new();
    for entry in &new_catalog {
        if !seen.insert(entry.name.clone()) {
            return Err(ApiError::bad_request(&format!(
                "Duplicate MCP server name: '{}'",
                entry.name
            )));
        }
        if entry.name.is_empty() {
            return Err(ApiError::bad_request("MCP server name must not be empty"));
        }
    }

    let data_dir = get_data_dir(&state).await?;
    save_mcp_catalog(&data_dir, &new_catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &new_catalog);
    }

    // Hot-push MCP config — handled by MQTT publisher trigger.
    // (The old ResourcePusher stub has been removed.)

    // Return masked response
    let servers: Vec<McpCatalogEntryResponse> = new_catalog
        .iter()
        .map(|c| {
            let masked = mask_sensitive_env(c);
            let has_secrets = has_sensitive_fields(c);
            McpCatalogEntryResponse {
                config: masked,
                has_secrets,
            }
        })
        .collect();

    Ok(Json(McpCatalogResponse { servers }))
}

/// `POST /api/mcp-catalog` — add a single server entry
pub async fn add_catalog_entry(
    State(state): State<AppState>,
    Json(body): Json<AddCatalogEntryRequest>,
) -> Result<(StatusCode, Json<OperationAck>), ApiError> {
    // ADR-059 §7.3: reject stale writers before touching the catalog.
    crate::http::routes::check_expected_version(&state, body.expected_version).await?;

    if body.config.name.is_empty() {
        return Err(ApiError::bad_request("MCP server name must not be empty"));
    }

    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    // Check for duplicate name
    if catalog.iter().any(|c| c.name == body.config.name) {
        return Err(ApiError::bad_request(&format!(
            "MCP server '{}' already exists in catalog",
            body.config.name
        )));
    }

    catalog.push(body.config);
    let ack = commit_catalog_push(&state, &data_dir, &catalog, body.expected_version).await?;

    Ok((StatusCode::CREATED, Json(ack)))
}

/// Persist the already-mutated catalog and run the mutation fan-out.
///
/// Factored out of [`add_catalog_entry`] so the preset-recommendation path
/// (ADR-104) performs *exactly* the same side effects: save to disk, rebuild
/// the `mcp_list` cache the AgentHello diff sync reads, trigger the MQTT
/// global-resource republish, and open the ADR-059 operation record. A second
/// hand-rolled copy would drift the first time one of those steps changes.
async fn commit_catalog_push(
    state: &AppState,
    data_dir: &std::path::Path,
    catalog: &[McpServerConfigDef],
    expected_version: Option<u64>,
) -> Result<OperationAck, ApiError> {
    save_mcp_catalog(data_dir, catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, data_dir, catalog);
    }

    // Hot-push MCP config to all running agents — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    // ADR-059 §6: open a committed operation record so the client can
    // correlate this mutation by `operation_id` and observe the
    // resulting `resource_version`. The side effect (catalog + disk +
    // mcp_list cache) already completed synchronously.
    let resource_version = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.mcp_list.version
    };
    let mut record = OperationRecord::new(expected_version.unwrap_or(0));
    record.state = OperationState::Committed;
    record.resource_version = Some(resource_version);
    let ack = OperationAck::from_record(&record);
    if let Some(store) = state.operation_store.as_ref() {
        store.insert(record);
    }
    Ok(ack)
}

/// `POST /api/mcp-catalog/from-declaration` — turn a package's
/// `[[mcp_servers]]` declaration (or a user-picked preset) into a catalog
/// entry (ADR-104).
///
/// ## Why the Gateway materializes instead of the client
///
/// Two things the client must not own:
///
/// 1. **The preset recipe.** `preset = "brave-search"` is an identifier whose
///    meaning lives in the server-side registry ([`super::mcp_presets`]). A
///    Desktop-side materializer would have to restate the spawn command and
///    args, and a second client (mobile) would restate them again.
/// 2. **The `origin` string.** "Where did this catalog entry come from?" is a
///    catalog fact; if each client formatted it, they would diverge and the
///    diagnosis would eventually be untrustworthy.
///
/// ## Idempotency and the one thing that is never overwritten
///
/// Re-materializing a name that already exists returns `200` and leaves the
/// entry **untouched** — in particular its `env`. The user's API key is the
/// one field a package update must not reset, and a package's recommendation
/// is re-projected on every start, so this path runs often.
pub async fn materialize_declaration(
    State(state): State<AppState>,
    Json(body): Json<MaterializeDeclarationRequest>,
) -> Result<(StatusCode, Json<OperationAck>), ApiError> {
    crate::http::routes::check_expected_version(&state, body.expected_version).await?;

    let entry = body
        .to_catalog_entry()
        .map_err(|e| ApiError::bad_request(&e))?;
    let name = entry.name.clone();

    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    if catalog.iter().any(|c| c.name == name) {
        // Already present — report success without touching the entry.
        return Ok((
            StatusCode::OK,
            Json(committed_ack(&state, body.expected_version).await),
        ));
    }

    catalog.push(entry);
    let ack = commit_catalog_push(&state, &data_dir, &catalog, body.expected_version).await?;
    Ok((StatusCode::CREATED, Json(ack)))
}

/// Build a committed (no-op) ACK without mutating the catalog.
async fn committed_ack(state: &AppState, expected_version: Option<u64>) -> OperationAck {
    let resource_version = {
        let gw = state.gateway_state.read().await;
        gw.resource_cache.mcp_list.version
    };
    let mut record = OperationRecord::new(expected_version.unwrap_or(0));
    record.state = OperationState::Committed;
    record.resource_version = Some(resource_version);
    let ack = OperationAck::from_record(&record);
    if let Some(store) = state.operation_store.as_ref() {
        store.insert(record);
    }
    ack
}

/// `PUT /api/mcp-catalog/{name}` — update a single server entry
pub async fn update_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<UpdateCatalogEntryRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    // If the name is being changed, check for conflicts first
    if body.config.name != name && catalog.iter().any(|c| c.name == body.config.name) {
        return Err(ApiError::bad_request(&format!(
            "MCP server '{}' already exists in catalog",
            body.config.name
        )));
    }

    // Find the existing entry index
    let idx = catalog.iter().position(|c| c.name == name).ok_or_else(|| {
        ApiError::not_found(&format!("MCP server '{}' not found in catalog", name))
    })?;

    // ADR-104: provenance is server-owned — captured before the entry is
    // rebuilt so an edit cannot launder "added by package X" into "manual".
    let old_origin = catalog[idx].origin.clone();

    // Preserve sensitive env values that were sent as "••••" (masked)
    // If the user didn't change a secret field, keep the old value
    let old_env = catalog[idx].env.clone();
    let merged_env: std::collections::HashMap<String, String> = body
        .config
        .env
        .into_iter()
        .map(|(k, v)| {
            if v == "••••" {
                // Keep the old value for this key
                let old_val = old_env.get(&k).cloned().unwrap_or_default();
                (k, old_val)
            } else {
                (k, v)
            }
        })
        .collect();

    // Same masked-value round-trip for HTTP headers (e.g. an Authorization
    // Bearer token): a value returned masked and sent back unchanged keeps
    // the stored secret rather than overwriting it with "••••".
    let old_headers = catalog[idx].headers.clone();
    let merged_headers: std::collections::HashMap<String, String> = body
        .config
        .headers
        .into_iter()
        .map(|(k, v)| {
            if v == "••••" {
                let old_val = old_headers.get(&k).cloned().unwrap_or_default();
                (k, old_val)
            } else {
                (k, v)
            }
        })
        .collect();

    let new_name = body.config.name.clone();
    catalog[idx] = McpServerConfigDef {
        name: body.config.name,
        display_name: body.config.display_name,
        transport: body.config.transport,
        url: body.config.url,
        command: body.config.command,
        args: body.config.args,
        env: merged_env,
        headers: merged_headers,
        tool_timeout_secs: body.config.tool_timeout_secs,
        install: body.config.install,
        required_secrets: body.config.required_secrets,
        // Provenance is server-owned (ADR-104): an update keeps whatever the
        // entry was created with, so "added by package X" cannot be laundered
        // into "added by hand" by a later edit.
        origin: old_origin,
    };

    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("MCP server '{}' updated in catalog", new_name),
    }))
}

/// `DELETE /api/mcp-catalog/{name}` — remove a server entry
pub async fn remove_catalog_entry(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<MessageResponse>, ApiError> {
    let data_dir = get_data_dir(&state).await?;
    let mut catalog = load_mcp_catalog(&data_dir).map_err(|e| ApiError::internal(&e))?;

    let original_len = catalog.len();
    catalog.retain(|c| c.name != name);
    if catalog.len() == original_len {
        return Err(ApiError::not_found(&format!(
            "MCP server '{}' not found in catalog",
            name
        )));
    }

    save_mcp_catalog(&data_dir, &catalog).map_err(|e| ApiError::internal(&e))?;

    // Rebuild mcp_list cache for AgentHello diff sync.
    {
        let mut gw = state.gateway_state.write().await;
        resource_cache::rebuild_and_save_mcp_cache(&mut gw, &data_dir, &catalog);
    }

    // Hot-push MCP config — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("MCP server '{}' removed from catalog", name),
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Get the data_dir from Gateway state
async fn get_data_dir(state: &AppState) -> Result<PathBuf, ApiError> {
    let gw = state.gateway_state.read().await;
    Ok(gw
        .config
        .as_ref()
        .map(|c| PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| PathBuf::from("./data")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_mask_sensitive_env() {
        let config = McpServerConfigDef {
            name: "github".to_string(),
            display_name: None,
            transport: acowork_core::protocol::McpTransportDef::Stdio,
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            env: std::collections::HashMap::from([
                (
                    "GITHUB_PERSONAL_ACCESS_TOKEN".to_string(),
                    "ghp_abc123".to_string(),
                ),
                ("SOME_OTHER_VAR".to_string(), "visible_value".to_string()),
            ]),
            ..Default::default()
        };

        let masked = mask_sensitive_env(&config);
        assert_eq!(
            masked.env.get("GITHUB_PERSONAL_ACCESS_TOKEN"),
            Some(&"••••".to_string())
        );
        assert_eq!(
            masked.env.get("SOME_OTHER_VAR"),
            Some(&"visible_value".to_string())
        );
    }

    #[test]
    fn test_catalog_save_load_roundtrip() {
        let dir =
            std::env::temp_dir().join(format!("acowork-test-mcp-catalog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let catalog = vec![McpServerConfigDef {
            name: "filesystem".to_string(),
            display_name: None,
            transport: acowork_core::protocol::McpTransportDef::Stdio,
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
            ],
            ..Default::default()
        }];

        save_mcp_catalog(&dir, &catalog).unwrap();
        let loaded = load_mcp_catalog(&dir).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "filesystem");

        let _ = std::fs::remove_dir_all(&dir);
    }

// ADR-104: materializing a declaration into the catalog.

    fn state_with_data_dir(dir: &std::path::Path) -> AppState {
        let gw = Arc::new(tokio::sync::RwLock::new(
            crate::gateway::state::GatewayState::new(&dir.join("vault").to_string_lossy()),
        ));
        {
            let mut g = gw.try_write().expect("fresh state");
            g.config = Some(crate::config::GatewayConfig {
                data_dir: dir.to_string_lossy().to_string(),
                ..Default::default()
            });
        }
        AppState::new(gw, Arc::new(crate::http::auth::HttpAuth::new(false)))
    }

    fn preset_decl(key: &str, agent_id: Option<&str>) -> MaterializeDeclarationRequest {
        MaterializeDeclarationRequest {
            key: key.to_string(),
            preset_id: Some(key.to_string()),
            inline: None,
            agent_id: agent_id.map(str::to_string),
            env: Default::default(),
            expected_version: None,
        }
    }

    #[tokio::test]
    async fn declaration_materializes_preset_with_package_origin() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let (status, _ack) = materialize_declaration(
            State(state.clone()),
            Json(preset_decl("brave-search", Some("com.acme.researcher"))),
        )
        .await
        .expect("preset should materialize");
        assert_eq!(status, StatusCode::CREATED);

        let catalog = load_mcp_catalog(dir.path()).unwrap();
        assert_eq!(catalog.len(), 1);
        let entry = &catalog[0];
        assert_eq!(entry.name, "brave-search");
        assert_eq!(
            entry.origin.as_deref(),
            Some("agent-package:com.acme.researcher#brave-search"),
            "the catalog must be able to say which package put this entry here"
        );
        assert_eq!(entry.required_secrets, vec!["BRAVE_API_KEY".to_string()]);
        assert!(entry.env.is_empty(), "a preset never pre-fills a secret");
        assert!(entry.install.is_some());
    }

    #[tokio::test]
    async fn declaration_without_agent_uses_preset_origin() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        // The status code is asserted by the sibling test; here only the
        // persisted `origin` matters, so the response tuple is discarded.
        let _ = materialize_declaration(State(state), Json(preset_decl("figma", None)))
            .await
            .unwrap();

        let catalog = load_mcp_catalog(dir.path()).unwrap();
        assert_eq!(catalog[0].origin.as_deref(), Some("preset:figma"));
    }

    #[tokio::test]
    async fn declaration_twice_keeps_the_first_entry_and_its_env() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let _ = materialize_declaration(State(state.clone()), Json(preset_decl("brave-search", None)))
            .await
            .unwrap();

        // Simulate the user typing their key afterwards.
        let mut catalog = load_mcp_catalog(dir.path()).unwrap();
        catalog[0]
            .env
            .insert("BRAVE_API_KEY".to_string(), "sk-user-typed".to_string());
        save_mcp_catalog(dir.path(), &catalog).unwrap();

        // Re-materializing (a package update re-runs its recommendation on
        // every start) must not reset the user's credential.
        let (status, _ack) = materialize_declaration(
            State(state),
            Json(preset_decl("brave-search", Some("com.acme.researcher"))),
        )
        .await
        .unwrap();
        assert_eq!(status, StatusCode::OK, "idempotent, not an error");

        let catalog = load_mcp_catalog(dir.path()).unwrap();
        assert_eq!(catalog.len(), 1, "no duplicate entry");
        assert_eq!(
            catalog[0].env.get("BRAVE_API_KEY").map(String::as_str),
            Some("sk-user-typed"),
            "the user's credential survives a re-install"
        );
    }

    #[tokio::test]
    async fn declaration_accepts_a_secret_typed_before_the_entry_existed() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let mut body = preset_decl("brave-search", None);
        body.env
            .insert("BRAVE_API_KEY".to_string(), "sk-typed-up-front".to_string());
        let _ = materialize_declaration(State(state), Json(body)).await.unwrap();

        let catalog = load_mcp_catalog(dir.path()).unwrap();
        assert_eq!(
            catalog[0].env.get("BRAVE_API_KEY").map(String::as_str),
            Some("sk-typed-up-front"),
            "the preset picker collects the key before the entry exists"
        );
    }

    #[tokio::test]
    async fn declaration_rejects_unknown_preset() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let err = materialize_declaration(State(state), Json(preset_decl("no-such-preset", None)))
            .await
            .expect_err("unknown preset must be refused");
        assert!(format!("{err:?}").contains("no-such-preset"));
        assert!(load_mcp_catalog(dir.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn declaration_rejects_preset_key_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let mut body = preset_decl("brave-search", None);
        body.key = "renamed-in-manifest".to_string();
        let err = materialize_declaration(State(state), Json(body))
            .await
            .expect_err("key/preset mismatch must be refused");
        assert!(format!("{err:?}").contains("must equal the preset id"));
        assert!(load_mcp_catalog(dir.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn declaration_materializes_an_inline_remote() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let mut headers = std::collections::HashMap::new();
        headers.insert("X-Tenant".to_string(), "acme".to_string());
        let (status, _ack) = materialize_declaration(
            State(state),
            Json(MaterializeDeclarationRequest {
                key: "acme-internal".to_string(),
                preset_id: None,
                inline: Some(InlineDeclaration {
                    transport: acowork_core::protocol::McpTransportDef::Http,
                    url: Some("https://mcp.acme.internal/rpc".to_string()),
                    headers,
                    required_secrets: vec!["ACME_MCP_TOKEN".to_string()],
                }),
                agent_id: Some("com.acme.researcher".to_string()),
                env: Default::default(),
                expected_version: None,
            }),
        )
        .await
        .expect("inline declaration should materialize");
        assert_eq!(status, StatusCode::CREATED);

        let catalog = load_mcp_catalog(dir.path()).unwrap();
        assert_eq!(catalog[0].name, "acme-internal");
        assert_eq!(
            catalog[0].url.as_deref(),
            Some("https://mcp.acme.internal/rpc")
        );
        assert_eq!(
            catalog[0].origin.as_deref(),
            Some("agent-package:com.acme.researcher#acme-internal")
        );
        assert!(
            catalog[0].command.is_empty(),
            "an http entry has no command"
        );
    }

    #[tokio::test]
    async fn declaration_rejects_both_forms_and_neither_form() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_with_data_dir(dir.path());

        let both = MaterializeDeclarationRequest {
            key: "x".to_string(),
            preset_id: Some("brave-search".to_string()),
            inline: Some(InlineDeclaration {
                transport: acowork_core::protocol::McpTransportDef::Http,
                url: Some("https://example.test/mcp".to_string()),
                headers: Default::default(),
                required_secrets: vec![],
            }),
            agent_id: None,
            env: Default::default(),
            expected_version: None,
        };
        let err = materialize_declaration(State(state.clone()), Json(both))
            .await
            .expect_err("both forms must be refused");
        assert!(format!("{err:?}").contains("not both"));

        let neither = MaterializeDeclarationRequest {
            key: "x".to_string(),
            preset_id: None,
            inline: None,
            agent_id: None,
            env: Default::default(),
            expected_version: None,
        };
        let err = materialize_declaration(State(state), Json(neither))
            .await
            .expect_err("neither form must be refused");
        assert!(format!("{err:?}").contains("must name a preset"));

        assert!(load_mcp_catalog(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn test_load_nonexistent_catalog() {
        let dir = std::env::temp_dir().join(format!(
            "acowork-test-mcp-catalog-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let loaded = load_mcp_catalog(&dir).unwrap();
        assert!(loaded.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
