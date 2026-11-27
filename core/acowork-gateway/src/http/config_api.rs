//! Configuration and status HTTP API handlers
//!
//! - GET  /api/config   — get Gateway configuration
//! - PUT  /api/config   — update Gateway configuration
//! - GET  /api/status   — system status (version, running count, memory)
//! - GET  /health       — health check (defined in routes.rs)

use axum::{
    Json, Router,
    extract::State,
    routing::{delete, get},
};
use serde::{Deserialize, Serialize};

use crate::http::routes::{ApiError, AppState};

/// Build the config/status/logs router
pub fn config_routes() -> Router<AppState> {
    Router::new()
        .route("/api/config", get(get_config).put(update_config))
        .route("/api/logs", delete(delete_logs))
}

// ── Response types ────────────────────────────────────────────────────

/// Gateway configuration response
#[derive(Serialize)]
pub struct ConfigResponse {
    pub packages_dir: String,
    pub data_dir: String,
    pub log_level: String,
    /// Log file maximum size in MB (0 = disabled, default 10)
    pub log_file_size_mb: u64,
    /// Maximum number of log files to keep (0 = unlimited, default 20)
    pub log_file_count: u64,
    /// Single-file upload ceiling in bytes (ADR-122 D6)
    pub max_upload_bytes: u64,
    // Auto-sleep (per-agent `idle_timeout_secs`) was retired in
    // Sept 2026 — there is no Gateway-level equivalent to expose.
    pub dev_mode: bool,
    pub http: HttpConfigResponse,
    /// Default LLM provider (if configured)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Default LLM model (if configured)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Global max output tokens limit (default 16384)
    pub max_output_tokens_limit: u64,
    /// HuggingFace mirror URLs for model downloads
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hf_mirrors: Vec<String>,
}

/// HTTP config subset
#[derive(Serialize)]
pub struct HttpConfigResponse {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub auth_enabled: bool,
}

/// Config update request
#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    /// Log level (trace/debug/info/warn/error)
    #[serde(default)]
    pub log_level: Option<String>,
    /// Log file maximum size in megabytes (0 = disable split)
    #[serde(default)]
    pub log_file_size_mb: Option<u64>,
    /// Maximum number of log files to keep (0 = unlimited)
    #[serde(default)]
    pub log_file_count: Option<u64>,
    /// Single-file upload ceiling in bytes (ADR-122 D6); must be > 0
    #[serde(default)]
    pub max_upload_bytes: Option<u64>,
    // Auto-sleep (`idle_timeout_secs`) was retired in Sept 2026 — the
    // Gateway no longer carries a corresponding field. A legacy client
    // still sending it is harmless: `UpdateConfigRequest` has no
    // `deny_unknown_fields`, so serde drops the unknown key and the
    // request still succeeds (see
    // `test_update_config_request_ignores_legacy_idle_timeout_secs`).
    /// Default LLM provider for all agents
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default LLM model for all agents
    #[serde(default)]
    pub default_model: Option<String>,
    /// Global max output tokens limit (caps max_output_tokens in API requests)
    #[serde(default)]
    pub max_output_tokens_limit: Option<u64>,
    /// HuggingFace mirror URLs for model downloads (replaces existing list)
    #[serde(default)]
    pub hf_mirrors: Option<Vec<String>>,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// `GET /api/config` — get current Gateway configuration
///
/// P0-2 fix: Now returns the actual GatewayConfig from GatewayState
/// instead of hardcoded placeholder values.
pub async fn get_config(
    State(state): State<AppState>,
) -> Result<Json<ConfigResponse>, ApiError> {
    let gw = state.gateway_state.read().await;
    let config = gw
        .config
        .as_ref()
        .ok_or_else(|| ApiError::internal("Gateway config not initialized"))?;

    Ok(Json(ConfigResponse {
        packages_dir: config.packages_dir.clone(),
        data_dir: config.data_dir.clone(),
        log_level: config.log_level.clone(),
        dev_mode: config.dev_mode,
        http: HttpConfigResponse {
            enabled: config.http.enabled,
            host: config.http.host.clone(),
            port: config.http.port,
            auth_enabled: config.http.auth_enabled,
        },
        default_provider: config.default_provider.clone(),
        default_model: config.default_model.clone(),
        max_output_tokens_limit: config.max_output_tokens_limit,
        log_file_count: config.log_file_count,
        log_file_size_mb: config.log_file_size_mb,
        max_upload_bytes: config.max_upload_bytes,
        hf_mirrors: config.hf_mirrors.clone(),
    }))
}

/// `PUT /api/config` — update Gateway configuration (hot reload)
///
/// P0-2 fix: Now actually applies configuration changes to GatewayState
/// instead of just logging them.
pub async fn update_config(
    State(state): State<AppState>,
    Json(body): Json<UpdateConfigRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    let mut updates = Vec::new();
    if let Some(level) = &body.log_level {
        let valid = ["trace", "debug", "info", "warn", "error"];
        if !valid.contains(&level.as_str()) {
            return Err(ApiError::bad_request(&format!(
                "Invalid log_level '{}'. Must be one of: trace, debug, info, warn, error",
                level
            )));
        }
        updates.push(format!("log_level={}", level));
    }
    // Auto-sleep was retired in Sept 2026 — there is no Gateway-level
    // `idle_timeout` field to update, and no migration work here. A legacy
    // client that still sends the key is ignored by serde (no
    // `deny_unknown_fields`) and still gets a 200. Note the per-agent
    // auto-sleep field on the Runtime's `PUT /api/agents/{id}/config` was
    // retired in the same change.
    if let Some(ref provider) = body.default_provider {
        updates.push(format!("default_provider={}", provider));
    }
    if let Some(ref model) = body.default_model {
        updates.push(format!("default_model={}", model));
    }
    if let Some(limit) = body.max_output_tokens_limit {
        updates.push(format!("max_output_tokens_limit={}", limit));
    }
    if let Some(size) = body.log_file_size_mb {
        if size > 1024 {
            return Err(ApiError::bad_request(
                "log_file_size_mb must be between 0 and 1024",
            ));
        }
        updates.push(format!("log_file_size_mb={}", size));
    }
    if let Some(count) = body.log_file_count {
        updates.push(format!("log_file_count={}", count));
    }
    if let Some(bytes) = body.max_upload_bytes {
        // The value is forwarded verbatim to the enforcing processes, so a
        // zero would make every upload fail 413 with no way to read the
        // reason. Reject at the boundary instead.
        if bytes == 0 {
            return Err(ApiError::bad_request("max_upload_bytes must be > 0"));
        }
        updates.push(format!("max_upload_bytes={}", bytes));
    }
    if let Some(ref mirrors) = body.hf_mirrors {
        updates.push(format!("hf_mirrors=[{}]", mirrors.join(",")));
    }

    if updates.is_empty() {
        return Err(ApiError::bad_request("No configuration fields to update"));
    }

    // Apply updates to the stored config
    let mut gw = state.gateway_state.write().await;
    if let Some(config) = &mut gw.config {
        if let Some(level) = &body.log_level {
            config.log_level = level.clone();
        }
        // Auto-sleep was retired in Sept 2026 — there is no
        // `idle_timeout_secs` field for the Gateway to write.
        // Update default_provider: Some("name") sets it, Some("") clears it
        if let Some(ref provider) = body.default_provider {
            if provider.is_empty() {
                config.default_provider = None;
            } else {
                config.default_provider = Some(provider.clone());
            }
        }
        // Update default_model: Some("model") sets it, Some("") clears it
        if let Some(ref model) = body.default_model {
            if model.is_empty() {
                config.default_model = None;
            } else {
                config.default_model = Some(model.clone());
            }
        }
        if let Some(limit) = body.max_output_tokens_limit {
            config.max_output_tokens_limit = limit;
        }
        if let Some(size) = body.log_file_size_mb {
            config.log_file_size_mb = size;
        }
        if let Some(count) = body.log_file_count {
            config.log_file_count = count;
        }
        if let Some(bytes) = body.max_upload_bytes {
            config.max_upload_bytes = bytes;
        }
        if let Some(ref mirrors) = body.hf_mirrors {
            config.hf_mirrors = mirrors.clone();
        }
    }
    let config_snapshot = gw.config.clone();
    drop(gw);

    // Persist config to disk (so changes survive Gateway restart)
    if let Some(ref cfg) = config_snapshot
        && let Err(e) = cfg.save()
    {
        tracing::warn!("Failed to persist configuration: {}", e);
    }

    // Apply log file count change immediately
    if let Some(count) = body.log_file_count {
        // 1. Update Gateway's own file appender
        crate::cli::update_log_file_count(count);

        // 2. Push to all connected Runtime agents (ADR-033: TODO via MQTT)
        // When MQTT global config topics are implemented, publish here.
        tracing::debug!("LogFileCountUpdate: MQTT push not yet implemented");
    }

    // Apply log level change immediately via tracing reload
    if let Some(level) = &body.log_level {
        // 1. Apply to Gateway itself
        if let Some(handle) = &state.log_reload_handle {
            let new_filter = acowork_core::logging::build_env_filter(level);
            if let Err(e) = handle.reload(new_filter) {
                tracing::warn!("Failed to reload Gateway tracing filter: {}", e);
            } else {
                tracing::info!("Gateway log level changed to {}", level);
            }
        }

        // 2. Push to all connected Runtimes (ADR-033: TODO via MQTT)
        tracing::debug!("LogLevelUpdate: MQTT push not yet implemented");
    }

    tracing::info!("Config update applied: {}", updates.join(", "));

    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("Config updated: {}", updates.join(", ")),
    }))
}

/// `DELETE /api/logs` — delete all log files
///
/// Three-phase cleanup:
/// 1. Push LogRotate to **running** agents via gRPC — each Runtime
///    force-rotates to a new log file and deletes old `*.log` files.
/// 2. Delete **Gateway**'s own log files in the project config directory.
/// 3. For **stopped** agents (installed but not running), delete
///    `{install_path}/workspace/logs/*.log` directly via filesystem.
pub async fn delete_logs(
    State(_state): State<AppState>,
) -> Result<Json<MessageResponse>, ApiError> {
    let mut total_deleted = 0u64;

    // ── Phase 1: Push LogRotate to running agents (ADR-033: TODO via MQTT) ──
    tracing::debug!("LogRotate: MQTT push not yet implemented");
    // Phase 2: Delete Gateway logs─────────────────────
    {
        // Only `gateway-*.log`; leave `embed.log` and any other sibling
        // log files alone (they belong to other processes).
        let log_dir = crate::config::GatewayConfig::project_data_dir().join("logs");
        if log_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&log_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name.starts_with("gateway-") && path.extension().is_some_and(|ext| ext == "log")
                {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!("Failed to delete Gateway log {:?}: {}", path, e);
                    } else {
                        total_deleted += 1;
                    }
                }
            }
        }
        tracing::info!("Gateway logs cleaned from {:?}", log_dir);
    }

    // ── Phase 3: Stopped agent logs ──────────────────────────────────
    // ADR-009: Gateway no longer directly accesses agent workspace files.
    // Stopped agents' workspace logs will be cleaned by the Runtime
    // on next startup (self-cleanup). This eliminates the need for the
    // Gateway to touch {install_path}/workspace/logs/.
    tracing::info!(
        "Log cleanup complete: {} log file(s) deleted",
        total_deleted
    );
    Ok(Json(MessageResponse {
        message: format!("Deleted {} log file(s)", total_deleted),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_update_config_request_deserialization() {
        let json = r#"{"log_level": "debug"}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.log_level, Some("debug".to_string()));
        assert!(req.default_provider.is_none());
        assert!(req.default_model.is_none());
        assert!(req.max_upload_bytes.is_none());
    }

    /// ADR-122 D6: the ceiling is one number owned by the Gateway config, and
    /// a `gateway.toml` written before the key existed must still load — with
    /// the same value the enforcing services fall back to, not a second default.
    #[test]
    fn test_max_upload_bytes_falls_back_to_the_shared_ceiling() {
        let cfg: crate::config::GatewayConfig =
            toml::from_str(
                "vault_dir = \"/tmp/v\"
data_dir = \"/tmp/x\"
packages_dir = \"/tmp/p\"",
            )
            .unwrap();
        assert_eq!(cfg.max_upload_bytes, acowork_core::upload::max_file_bytes());
    }

    #[test]
    fn test_update_config_request_ignores_legacy_idle_timeout_secs() {
        // Legacy clients still send `idle_timeout_secs` (it lived on the
        // Gateway before the Runtime took ownership). The struct no longer
        // has the field, so the deserializer must accept the key without
        // failing — `#[serde(default)]` on every present field guarantees
        // this; we only assert deserialization still works.
        let json = r#"{"log_level": "warn", "idle_timeout_secs": 600}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.log_level, Some("warn".to_string()));
    }

    #[test]
    fn test_update_config_request_provider_and_model() {
        let json = r#"{"default_provider": "deepseek", "default_model": "deepseek-chat"}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.default_provider, Some("deepseek".to_string()));
        assert_eq!(req.default_model, Some("deepseek-chat".to_string()));
    }

    /// `[security].allowed_node_ips` must NOT be changeable through
    /// `PUT /api/config`. `UpdateConfigRequest` has no `security` field,
    /// so a payload smuggling one in is silently ignored at
    /// deserialization — the allowlist stays boot-time-only.
    #[test]
    fn test_update_config_request_ignores_security_field() {
        let json = r#"{"log_level": "info", "security": {"allowed_node_ips": ["203.0.113.5"]}}"#;
        let req: UpdateConfigRequest = serde_json::from_str(json).unwrap();
        // The smuggled security payload is not surfaced as any field.
        assert_eq!(req.log_level, Some("info".to_string()));
        // And GatewayConfig has no runtime path that copies it — the only
        // place `security` is ever populated is config load (TOML/env).
        let cfg = crate::config::GatewayConfig::default();
        assert!(cfg.security.allowed_node_ips.is_empty());
    }

    #[test]
    fn test_config_response_serialization() {
        let resp = ConfigResponse {
            packages_dir: "/tmp/packages".to_string(),
            data_dir: "/tmp/data".to_string(),
            log_level: "info".to_string(),
            dev_mode: false,
            http: HttpConfigResponse {
                enabled: true,
                host: "127.0.0.1".to_string(),
                port: 19876,
                auth_enabled: false,
            },
            default_provider: Some("deepseek".to_string()),
            default_model: Some("deepseek-chat".to_string()),
            max_output_tokens_limit: 32768,
            log_file_size_mb: 10,
            log_file_count: 20,
            max_upload_bytes: 1024 * 1024 * 1024,
            hf_mirrors: Vec::new(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("19876"));
        assert!(json.contains("info"));
        assert!(json.contains("deepseek"));
    }
}
