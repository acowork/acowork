//! acowork-doc service configuration loading.
//!
//! Config precedence (highest first):
//!
//! 1. `ACOWORK_APP_DATA_DIR` (hosted-app supervisor, ADR-116)
//! 2. `ACOWORK_DOC_DATA_DIR` (legacy / standalone override)
//! 3. TOML config file (`--config <path>`, default `./acowork-doc.toml`)
//! 4. [`DocConfig::default`] → `$HOME/.acowork/acowork-doc/`
//!
//! Built-in DOC keeps the home-relative default (plan D-2). Cloned installs
//! must use `ACOWORK_APP_DATA_DIR` so they do not share that tree. The Gateway
//! only manages process lifecycle (spawn / port / MCP injection); service
//! tuning parameters live here, parsed independently.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// acowork-doc service runtime config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocConfig {
    /// Data root (document library tree location).
    ///
    /// Resolution: `ACOWORK_APP_DATA_DIR` → `ACOWORK_DOC_DATA_DIR` → TOML
    /// `data_dir` → [`default_data_dir`]. Built-in default is
    /// `$HOME/.acowork/acowork-doc/`; hosted-app installs use
    /// `{gateway_data}/app-data/{id}`.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,

    /// Standalone-process HTTP listen port.
    ///
    /// Default `18081`. On conflict the port auto-increments (max +20); the
    /// actual bound port is reported via `--port-file` to the Gateway.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Whether the doc service is enabled.
    ///
    /// Default `true`. Gateway spawns the doc subprocess accordingly; in
    /// standalone mode `false` makes the process exit immediately.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Default TTL (hours) for pending update requests before they are
    /// marked `expired` (design §5.4). Default `72`.
    #[serde(default = "default_request_ttl_hours")]
    pub request_ttl_hours: u32,

    /// Retention (days) for items in `.trash/` before auto-cleanup.
    ///
    /// Default `30` (design OD-4, D-6).
    #[serde(default = "default_trash_retention_days")]
    pub trash_retention_days: u32,

    /// Max request body size for document content (bytes).
    ///
    /// Default `2 MiB`. Exceeding returns 413 `payload_too_large`.
    #[serde(default = "default_max_doc_size")]
    pub max_doc_size_bytes: usize,

    /// Gateway HTTP base URL (plan §7 G5), injected by the supervisor via
    /// `--gateway-url`. `None` (standalone runs) disables the review
    /// receipt push — the agent-side `doc_check_request` poll still
    /// converges.
    #[serde(default)]
    pub gateway_url: Option<String>,

    /// Internal-service Bearer token for Gateway HTTP calls, from env
    /// `ACOWORK_DOC_GATEWAY_TOKEN` (never argv — same channel as PM's
    /// `ACOWORK_PM_GATEWAY_TOKEN`, ADR-064 Phase 3).
    #[serde(default)]
    pub gateway_token: Option<String>,
}

/// Default data directory resolution (highest first):
///
/// 1. `ACOWORK_APP_DATA_DIR` — hosted-app framework (ADR-116): each installed
///    app instance gets its own private tree under `{data_dir}/app-data/{id}`.
/// 2. `ACOWORK_DOC_DATA_DIR` — legacy / standalone override.
/// 3. `$HOME/.acowork/acowork-doc/` — built-in DOC default.
///
/// Without (1), a cloned install of the same binary would share the built-in
/// library and corrupt it.
pub fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("ACOWORK_APP_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("ACOWORK_DOC_DATA_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".acowork").join("acowork-doc"))
        .unwrap_or_else(|_| PathBuf::from("./data/acowork-doc"))
}

fn default_port() -> u16 {
    18081
}

fn default_true() -> bool {
    true
}

fn default_request_ttl_hours() -> u32 {
    72
}

fn default_trash_retention_days() -> u32 {
    30
}

fn default_max_doc_size() -> usize {
    2 * 1024 * 1024
}

impl Default for DocConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            port: default_port(),
            enabled: default_true(),
            request_ttl_hours: default_request_ttl_hours(),
            trash_retention_days: default_trash_retention_days(),
            max_doc_size_bytes: default_max_doc_size(),
            gateway_url: None,
            gateway_token: None,
        }
    }
}

impl DocConfig {
    /// Validate config invariants; returns the first offending field name.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.port == 0 {
            return Err("port must be non-zero".into());
        }
        if self.request_ttl_hours == 0 {
            return Err("request_ttl_hours must be >= 1".into());
        }
        if self.trash_retention_days == 0 {
            return Err("trash_retention_days must be >= 1".into());
        }
        if self.max_doc_size_bytes == 0 {
            return Err("max_doc_size_bytes must be >= 1".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `default_data_dir()` reads process-global env vars, and
    /// `app_data_dir_env_wins_over_legacy` mutates them. Cargo runs tests in
    /// parallel threads, so without a lock that test's `set_var` window lands
    /// inside this test's `DocConfig::default()` and the data-dir assertion
    /// flakes. One lock around every test that reads or writes those vars.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn defaults_are_sane() {
        let _g = env_guard();
        let cfg = DocConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, 18081);
        assert_eq!(cfg.request_ttl_hours, 72);
        assert_eq!(cfg.trash_retention_days, 30);
        assert_eq!(cfg.max_doc_size_bytes, 2 * 1024 * 1024);
        assert!(cfg.data_dir.to_string_lossy().contains("acowork-doc"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn serde_roundtrip_full() {
        let cfg = DocConfig {
            data_dir: PathBuf::from("D:/tmp/doc"),
            port: 18100,
            enabled: false,
            request_ttl_hours: 24,
            trash_retention_days: 7,
            max_doc_size_bytes: 1024,
            gateway_url: Some("http://127.0.0.1:19876".to_string()),
            gateway_token: Some("svc".to_string()),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: DocConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.data_dir, PathBuf::from("D:/tmp/doc"));
        assert_eq!(back.port, 18100);
        assert!(!back.enabled);
        assert_eq!(back.request_ttl_hours, 24);
        assert_eq!(back.trash_retention_days, 7);
        assert_eq!(back.max_doc_size_bytes, 1024);
        assert_eq!(back.gateway_url.as_deref(), Some("http://127.0.0.1:19876"));
        assert_eq!(back.gateway_token.as_deref(), Some("svc"));
    }

    #[test]
    fn validate_rejects_zero_port() {
        let cfg = DocConfig {
            port: 0,
            ..DocConfig::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn app_data_dir_env_wins_over_legacy() {
        let _g = env_guard();
        // Hosted-app clones must not share ~/.acowork/acowork-doc with the
        // built-in instance. ACOWORK_APP_DATA_DIR is the supervisor contract.
        let prev_app = std::env::var_os("ACOWORK_APP_DATA_DIR");
        let prev_doc = std::env::var_os("ACOWORK_DOC_DATA_DIR");
        unsafe {
            std::env::set_var("ACOWORK_APP_DATA_DIR", "D:/tmp/app-data/doc-clone");
            std::env::set_var("ACOWORK_DOC_DATA_DIR", "D:/tmp/legacy-doc");
        }
        let dir = default_data_dir();
        match prev_app {
            Some(v) => unsafe { std::env::set_var("ACOWORK_APP_DATA_DIR", v) },
            None => unsafe { std::env::remove_var("ACOWORK_APP_DATA_DIR") },
        }
        match prev_doc {
            Some(v) => unsafe { std::env::set_var("ACOWORK_DOC_DATA_DIR", v) },
            None => unsafe { std::env::remove_var("ACOWORK_DOC_DATA_DIR") },
        }
        assert_eq!(dir, PathBuf::from("D:/tmp/app-data/doc-clone"));
    }
}
