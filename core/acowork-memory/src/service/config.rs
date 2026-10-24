//! acowork-memory service configuration (ADR-100).
//!
//! Config precedence (highest first):
//!
//! 1. CLI flag / its env fallback (`ACOWORK_MEMORY_DATA_DIR`,
//!    `ACOWORK_MEMORY_CONFIG`, `ACOWORK_MQTT_PASSWORD` — resolved by clap)
//! 2. TOML config file (`--config <path>`, default `{data_dir}/config.toml`)
//! 3. [`MemoryServiceConfig::default`]
//!
//! The file layer is mandatory for this service (unlike the doc / PM peers,
//! which take every tunable from the Gateway): the distiller and forgetting
//! pipelines are scheduled by the memory process itself, so their model
//! candidate chain and thresholds must survive a Gateway restart unchanged.
//! The Gateway only manages the process lifecycle (spawn / port / MCP
//! injection) and therefore passes plumbing flags alone.
//!
//! A file that exists but fails to parse is a hard startup error — a silently
//! ignored typo would fall back to `enabled = false` and the pipeline would
//! simply never run. A missing file is fine (pure defaults).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::EpisodicDecayConfig;
use crate::consolidation::{DistillerConfig, SchedulerConfig};
use crate::types::DEFAULT_EMBEDDING_DIM;

/// acowork-memory service runtime config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MemoryServiceConfig {
    /// Whether the memory service is enabled.
    ///
    /// Default `true`. The Gateway supervisor spawns the process accordingly;
    /// in standalone mode `false` makes the process exit immediately.
    pub enabled: bool,

    /// Data root holding one SQLite per Agent instance:
    /// `{data_dir}/instances/{instance_id}/memory.db`.
    ///
    /// Default `$HOME/.acowork/acowork-memory/` (peer of `acowork-doc/`,
    /// `acowork-pm/`, `acowork-user/`).
    pub data_dir: PathBuf,

    /// Standalone-process HTTP listen port.
    ///
    /// Default `18084` (18080 embed / 18081 doc / 18082 pm / 18083 user are
    /// taken). On conflict the port auto-increments (max +20); the actual
    /// bound port is reported via `--port-file` to the Gateway.
    pub port: u16,

    /// Embedding vector dimension written into a brand-new instance store.
    ///
    /// Default 384 ([`DEFAULT_EMBEDDING_DIM`], the bge-small-zh output). An
    /// existing `memory.db` keeps the dimension recorded in its own `meta`
    /// table — this value only seeds new files (see
    /// [`crate::sqlite::SqliteStore::open`]), so changing it is a migration,
    /// not a switch: the rebuild-index endpoint re-embeds every node.
    pub embedding_dim: usize,

    /// Embedding backend (acowork-embed sidecar, or any OpenAI-compatible API).
    pub embedding: EmbeddingSection,

    /// Episodic distiller pipeline (ADR-068 / ADR-071).
    pub distiller: DistillerSection,

    /// Episodic forgetting / time-decay pipeline.
    pub forgetting: ForgettingSection,
}

/// `[embedding]` — how the service turns text into vectors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbeddingSection {
    /// Master switch. When `false` the service still stores nodes, but never
    /// embeds them, so only the FTS/text retrieval path works (vector search
    /// degrades instead of failing). Default `true`.
    pub enabled: bool,

    /// OpenAI-compatible base URL **without** the `/embeddings` suffix, e.g.
    /// `http://127.0.0.1:18080/v1` (the local acowork-embed sidecar).
    pub base_url: String,

    /// Model id sent in the request body. `None` = whatever the endpoint has
    /// loaded (the embed sidecar serves its single loaded model).
    pub model: Option<String>,

    /// Bearer token for a cloud embedding endpoint. `None` = no `Authorization`
    /// header, which is what the unauthenticated local sidecar expects.
    pub api_key: Option<String>,

    /// Per-request timeout in milliseconds. Default 10s.
    pub timeout_ms: u64,

    /// Retries on transport failure / 5xx before giving up on a vector.
    /// Default 2.
    pub max_retries: u32,
}

/// `[distiller]` — background episodic → semantic/behavioral promotion.
///
/// Field names and defaults mirror [`crate::consolidation::SchedulerConfig`]
/// so the panel's `GET/PUT /memory/consolidation/status` maps without unit
/// translation (intervals are seconds, not minutes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DistillerSection {
    /// Master switch (ADR-071 D4). Default `false` — opt-in, matching
    /// `SchedulerConfig::default()`.
    pub enabled: bool,
    /// Minimum gap between automatic runs, seconds. Default 3600.
    pub interval_secs: u64,
    /// Unconsolidated-episode backlog required to fire at an interval point.
    /// Default 50.
    pub accumulation_threshold: usize,
    /// Idle gap that lets a smaller backlog fire. Default 1800.
    pub idle_secs: u64,
    /// Batch size per run. Default 50.
    pub batch_size: usize,
    /// Minimum age of a `Pending` node before it is eligible. Default 1.
    pub min_pending_age_hours: u64,
    /// Ordered model candidate chain for distiller LLM calls (ADR-071 D5
    /// equivalent — tried in order, first success wins). Empty = fall back to
    /// the Gateway-published global default compact model.
    pub models: Vec<acowork_core::protocol::CompactModelRef>,
}

/// `[forgetting]` — episodic time decay and archive sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForgettingSection {
    /// Master switch. Default `false` — opt-in.
    pub enabled: bool,
    /// Decay half-life in days. Default 180.
    pub half_life_days: u64,
    /// Retention below which an Active node becomes Dormant. Default 0.1.
    pub dormant_threshold: f32,
    /// Days a Dormant node is kept before archiving to the purge log.
    /// Default 90.
    pub archive_days: u64,
    /// Minimum gap between decay scans, seconds (guards against a full-table
    /// scan per poll tick). Default 3600.
    pub interval_secs: u64,
}

impl Default for MemoryServiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            data_dir: default_data_dir(),
            port: default_port(),
            embedding_dim: default_embedding_dim(),
            embedding: EmbeddingSection::default(),
            distiller: DistillerSection::default(),
            forgetting: ForgettingSection::default(),
        }
    }
}

impl Default for EmbeddingSection {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: default_embed_base_url(),
            model: None,
            api_key: None,
            timeout_ms: default_timeout_ms(),
            max_retries: default_max_retries(),
        }
    }
}

impl Default for DistillerSection {
    fn default() -> Self {
        let sched = SchedulerConfig::default();
        Self {
            enabled: sched.distiller_enabled,
            interval_secs: sched.distiller_interval_secs,
            accumulation_threshold: sched.distiller_accumulation,
            idle_secs: sched.distiller_idle_secs,
            batch_size: sched.batch_size,
            min_pending_age_hours: sched.min_pending_age_hours,
            models: Vec::new(),
        }
    }
}

impl Default for ForgettingSection {
    fn default() -> Self {
        let decay = EpisodicDecayConfig::default();
        Self {
            enabled: decay.enabled,
            half_life_days: decay.half_life_days,
            dormant_threshold: decay.dormant_threshold,
            archive_days: decay.archive_days,
            interval_secs: 3600,
        }
    }
}

/// Default data directory: `$HOME/.acowork/acowork-memory/`.
pub fn default_data_dir() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(|h| PathBuf::from(h).join(".acowork").join("acowork-memory"))
        .unwrap_or_else(|_| PathBuf::from("./data/acowork-memory"))
}

fn default_port() -> u16 {
    18084
}

fn default_embedding_dim() -> usize {
    DEFAULT_EMBEDDING_DIM
}

fn default_embed_base_url() -> String {
    "http://127.0.0.1:18080/v1".to_string()
}

fn default_timeout_ms() -> u64 {
    10_000
}

fn default_max_retries() -> u32 {
    2
}

impl MemoryServiceConfig {
    /// Build the effective config from the three layers (defaults → TOML → CLI).
    ///
    /// Returns the first error as a display string; `main` exits non-zero on it.
    pub fn load(cli: &crate::service::cli::Cli) -> std::result::Result<Self, String> {
        let mut cfg = Self::file_layer(cli)?.unwrap_or_default();
        // Layer 1 (highest): CLI / clap env fallbacks.
        if let Some(dir) = &cli.data_dir {
            cfg.data_dir = dir.clone();
        }
        if let Some(port) = cli.port {
            cfg.port = port;
        }
        Ok(cfg)
    }

    /// Parse the TOML layer, if a file is present.
    ///
    /// An explicit `--config` path that does not exist is an error (the
    /// operator asked for it by name); the implicit `{data_dir}/config.toml`
    /// is allowed to be missing.
    fn file_layer(cli: &crate::service::cli::Cli) -> std::result::Result<Option<Self>, String> {
        let path = match &cli.config {
            Some(p) => p.clone(),
            None => cli
                .data_dir
                .clone()
                .unwrap_or_else(default_data_dir)
                .join("config.toml"),
        };
        if !path.exists() {
            if cli.config.is_some() {
                return Err(format!("config file not found: {}", path.display()));
            }
            return Ok(None);
        }
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let parsed: Self =
            toml::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))?;
        tracing::info!(path = %path.display(), "Loaded acowork-memory config file");
        Ok(Some(parsed))
    }

    /// Validate config invariants; returns the first offending field.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.port == 0 {
            return Err("port must be non-zero".into());
        }
        if self.embedding_dim == 0 {
            return Err("embedding_dim must be >= 1".into());
        }
        if self.embedding.enabled && self.embedding.base_url.trim().is_empty() {
            return Err("embedding.base_url must be set when embedding.enabled".into());
        }
        if self.embedding.timeout_ms == 0 {
            return Err("embedding.timeout_ms must be >= 1".into());
        }
        if !(0.0 < self.forgetting.dormant_threshold && self.forgetting.dormant_threshold <= 1.0) {
            return Err("forgetting.dormant_threshold must be in (0.0, 1.0]".into());
        }
        Ok(())
    }

    /// Directory holding the per-instance stores: `{data_dir}/instances`.
    pub fn instances_dir(&self) -> PathBuf {
        self.data_dir
            .join(crate::service::state::INSTANCES_DIR_NAME)
    }

    /// Project this config onto the consolidation scheduler policy.
    ///
    /// The service owns the pipeline (ADR-100), so the file is the only layer;
    /// the per-agent runtime override layer that the Runtime used to hold is
    /// now served by `PUT /memory/consolidation/status` against a live
    /// instance (P1d/P1e).
    pub fn scheduler_config(&self) -> SchedulerConfig {
        SchedulerConfig {
            accumulation_threshold: self.distiller.accumulation_threshold,
            batch_size: self.distiller.batch_size,
            min_pending_age_hours: self.distiller.min_pending_age_hours,
            distiller_enabled: self.distiller.enabled,
            // The distiller reads its own parameters from this sub-config, so
            // `[distiller].batch_size` has to be projected here as well —
            // `SchedulerConfig::batch_size` alone is the legacy offline path.
            distiller_config: Some(DistillerConfig {
                batch_size: self.distiller.batch_size,
                ..DistillerConfig::default()
            }),
            distiller_interval_secs: self.distiller.interval_secs,
            distiller_accumulation: self.distiller.accumulation_threshold,
            distiller_idle_secs: self.distiller.idle_secs,
            forgetting_enabled: self.forgetting.enabled,
            forgetting_half_life_days: self.forgetting.half_life_days,
            forgetting_dormant_threshold: self.forgetting.dormant_threshold,
            forgetting_archive_days: self.forgetting.archive_days,
            forgetting_interval_secs: self.forgetting.interval_secs,
            ..SchedulerConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> crate::service::cli::Cli {
        crate::service::cli::Cli::parse_from(
            std::iter::once("acowork-memory").chain(args.iter().copied()),
        )
    }

    #[test]
    fn defaults_are_sane() {
        let cfg = MemoryServiceConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.port, 18084);
        assert_eq!(cfg.embedding_dim, DEFAULT_EMBEDDING_DIM);
        assert_eq!(cfg.embedding.base_url, "http://127.0.0.1:18080/v1");
        assert!(cfg.embedding.enabled);
        // Pipelines are opt-in, mirroring SchedulerConfig::default().
        assert!(!cfg.distiller.enabled);
        assert_eq!(cfg.distiller.interval_secs, 3600);
        assert_eq!(cfg.distiller.accumulation_threshold, 50);
        assert_eq!(cfg.distiller.idle_secs, 1800);
        assert!(!cfg.forgetting.enabled);
        assert_eq!(cfg.forgetting.half_life_days, 180);
        assert_eq!(cfg.forgetting.dormant_threshold, 0.1);
        assert_eq!(cfg.forgetting.archive_days, 90);
        assert!(cfg.data_dir.to_string_lossy().contains("acowork-memory"));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn defaults_track_scheduler_config() {
        let sched = SchedulerConfig::default();
        let d = DistillerSection::default();
        assert_eq!(d.enabled, sched.distiller_enabled);
        assert_eq!(d.interval_secs, sched.distiller_interval_secs);
        assert_eq!(d.accumulation_threshold, sched.distiller_accumulation);
        assert_eq!(d.idle_secs, sched.distiller_idle_secs);
        assert_eq!(d.batch_size, sched.batch_size);
        assert_eq!(d.min_pending_age_hours, sched.min_pending_age_hours);
    }

    #[test]
    fn scheduler_config_bridge_maps_service_sections() {
        let cfg = MemoryServiceConfig {
            distiller: DistillerSection {
                enabled: true,
                interval_secs: 60,
                ..Default::default()
            },
            forgetting: ForgettingSection {
                enabled: true,
                interval_secs: 120,
                ..Default::default()
            },
            ..Default::default()
        };
        let sched = cfg.scheduler_config();
        assert!(sched.distiller_enabled);
        assert_eq!(sched.distiller_interval_secs, 60);
        assert_eq!(sched.distiller_accumulation, 50);
        assert!(sched.forgetting_enabled);
        assert_eq!(sched.forgetting_interval_secs, 120);
        assert_eq!(
            sched.idle_timeout_secs,
            SchedulerConfig::default().idle_timeout_secs
        );
    }

    #[test]
    fn partial_toml_layer_keeps_unset_fields_defaulted() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(
            &file,
            r#"
port = 19000

[distiller]
enabled = true
models = [{ provider_id = "volc", model_id = "deepseek-v3" }]
"#,
        )
        .unwrap();
        let cfg = MemoryServiceConfig::load(&cli(&[
            "--config",
            file.to_str().unwrap(),
            "--data-dir",
            dir.path().to_str().unwrap(),
        ]))
        .unwrap();
        assert_eq!(cfg.port, 19000);
        assert!(cfg.distiller.enabled);
        assert_eq!(cfg.distiller.interval_secs, 3600);
        assert_eq!(cfg.distiller.models.len(), 1);
        assert_eq!(cfg.distiller.models[0].provider_id, "volc");
        // Untouched sections fall back to defaults.
        assert_eq!(cfg.embedding.base_url, "http://127.0.0.1:18080/v1");
        assert!(!cfg.forgetting.enabled);
        assert_eq!(cfg.data_dir, dir.path());
    }

    #[test]
    fn cli_overrides_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "port = 19000\n").unwrap();
        let cfg = MemoryServiceConfig::load(&cli(&[
            "--config",
            file.to_str().unwrap(),
            "--port",
            "19500",
        ]))
        .unwrap();
        assert_eq!(cfg.port, 19500);
    }

    #[test]
    fn unknown_toml_key_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, "enabled_false = true\n").unwrap();
        let err =
            MemoryServiceConfig::load(&cli(&["--config", file.to_str().unwrap()])).unwrap_err();
        assert!(err.contains("parse"), "unexpected error: {err}");
    }

    #[test]
    fn missing_explicit_config_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        let err =
            MemoryServiceConfig::load(&cli(&["--config", missing.to_str().unwrap()])).unwrap_err();
        assert!(err.contains("not found"), "unexpected error: {err}");
    }

    #[test]
    fn missing_implicit_config_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            MemoryServiceConfig::load(&cli(&["--data-dir", dir.path().to_str().unwrap()])).unwrap();
        // Only the explicitly overridden field changes; every section that the
        // absent file would have supplied stays at its default.
        assert_eq!(cfg.data_dir, dir.path());
        let expected = MemoryServiceConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        };
        assert_eq!(cfg, expected);
    }

    #[test]
    fn validate_rejects_bad_values() {
        for mutate in [
            (|c: &mut MemoryServiceConfig| c.port = 0) as fn(&mut MemoryServiceConfig),
            |c| c.embedding_dim = 0,
            |c| c.embedding.timeout_ms = 0,
            |c| c.embedding.base_url = "  ".into(),
            |c| c.forgetting.dormant_threshold = 0.0,
            |c| c.forgetting.dormant_threshold = 1.5,
        ] {
            let mut cfg = MemoryServiceConfig::default();
            mutate(&mut cfg);
            assert!(cfg.validate().is_err(), "expected reject: {cfg:?}");
        }
    }

    #[test]
    fn instances_dir_is_under_data_dir() {
        let cfg = MemoryServiceConfig::default();
        assert_eq!(cfg.instances_dir(), cfg.data_dir.join("instances"));
    }
}
