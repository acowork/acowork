//! acowork-doc service configuration loading.
//!
//! Config precedence (highest first):
//!
//! 1. Environment variable (`ACOWORK_DOC_DATA_DIR`)
//! 2. TOML config file (`--config <path>`, default `./acowork-doc.toml`)
//! 3. [`DocConfig::default`]
//!
//! The data directory resolves to `$HOME/.acowork/acowork-doc/` (peer of
//! `acowork-pm/`, `acowork-gateway/`, `acowork-node/`) — see plan decision
//! D-2. The Gateway only manages the process lifecycle (spawn / port / MCP
//! injection); service tuning parameters live here, parsed independently.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// acowork-doc service runtime config.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DocConfig {
    /// Data root (document library tree location).
    ///
    /// Resolution: env `ACOWORK_DOC_DATA_DIR` → TOML `data_dir` →
    /// [`default_data_dir`]. Default `$HOME/.acowork/acowork-doc/`.
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

/// Default data directory: `$ACOWORK_DOC_DATA_DIR`, else
/// `$HOME/.acowork/acowork-doc/`.
///
/// The env override is what makes the process testable and what lets the
/// Gateway relocate the library without a TOML file. It used to be
/// documented in [`DocConfig::data_dir`] without being read here — a knob
/// that silently did nothing, which meant a "throwaway" instance wrote
/// straight into the user's real library.
pub fn default_data_dir() -> PathBuf {
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

    #[test]
    fn defaults_are_sane() {
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
}
