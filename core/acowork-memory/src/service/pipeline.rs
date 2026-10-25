//! Distiller + forgetting background pipeline (ADR-101 decision 7).
//!
//! This is the memory service's own scheduler: what the Runtime ran per Agent
//! process (`consolidation_bg.rs` + `AgentCore::run_episodic_distill_once`) now
//! runs here, once per **instance namespace** instead of once per process.
//!
//! Three things changed with the move, and each has a reason:
//!
//! 1. **N timers, one loop.** The Runtime had a single `ConsolidationTimer`
//!    because it served a single Agent. One memory process serves every Agent,
//!    so timers live in a per-instance map, created on first touch and polled
//!    by one background loop. Trigger semantics (interval gate, then backlog
//!    accumulation OR idle-with-backlog) are ported unchanged — the panel reads
//!    the same numbers it used to.
//! 2. **Credentials come from the Gateway.** The distiller's model chain and the
//!    embedding endpoint are no longer the agent manifest's business: they are
//!    the Gateway's global-resources snapshot ([`crate::service::resources`]),
//!    pulled over HTTP and refreshed in the background, with
//!    `[distiller].models` / `[embedding]` in `config.toml` as the local layer.
//! 3. **A missing embedding stops the distiller.** The Runtime's manual trigger
//!    already refused to run without an embedding provider (`409`), because
//!    [`crate::consolidation::EpisodicDistiller`] with `embedding_fn = None`
//!    recalls nothing and therefore projects *every* episode as a brand-new
//!    Knowledge node — mass duplication, silently. The background loop now
//!    applies the same invariant instead of discovering it per request.
//!
//! Per-instance policy (the panel's distiller / forgetting cards) is persisted
//! next to the store as `{instance_dir}/consolidation.json`, so a config.toml
//! edit or a Gateway restart cannot silently switch a user's opt-in back off.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

use acowork_core::EmbeddingProvider;
use acowork_core::protocol::CompactModelRef;

use crate::EpisodicDecayConfig;
use crate::MemoryProvider;
use crate::consolidation::{
    DefaultEpisodicDistiller, DistillerConfig, DistillerResult, EmbeddingFn, EpisodicDistiller,
    SchedulerConfig,
};
use crate::service::config::MemoryServiceConfig;
use crate::service::embedding::HttpEmbeddingClient;
use crate::service::error::{Result, ServiceError};
use crate::service::llm::{ChainLlm, resolve_candidates};
use crate::service::resources::{
    GlobalResources, PULL_MAX_DURATION, REFRESH_INTERVAL, ResourceClient,
};
use crate::service::state::InstanceRegistry;

/// Poll cadence of the consolidation loop (the Runtime's default).
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// Per-instance policy file inside the namespace directory.
pub const SETTINGS_FILE_NAME: &str = "consolidation.json";
/// One distiller chat call may take a while — the merge reply is one JSON object
/// per episode over a whole batch. Providers with tighter limits answer 4xx and
/// the chain moves to the next candidate.
pub const LLM_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a consolidation run was triggered. Ported verbatim from the Runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerReason {
    /// Episodic forgetting scan interval point reached (opt-in).
    ForgettingInterval,
    /// Interval point reached and the unconsolidated-episode backlog is
    /// at/above the accumulation threshold (ADR-071 D1).
    DistillerAccumulation,
    /// Interval point reached, the backlog is non-empty and the agent has been
    /// idle long enough (ADR-071 D1).
    DistillerIdle,
}

/// Summary of the most recent EpisodicDistiller run (ADR-071 D2), kept per
/// instance so `GET /memory/consolidation/status` can answer "上次运行" without
/// re-scanning the store.
#[derive(Debug, Clone, Default)]
pub struct DistillRunRecord {
    pub at: chrono::DateTime<chrono::Utc>,
    pub episodes_scanned: usize,
    pub facts_promoted: usize,
    pub preferences_promoted: usize,
    pub relations_promoted: usize,
    pub procedures_promoted: usize,
    pub autobio_promoted: usize,
    pub episodes_marked_consolidated: usize,
    /// Stage-by-stage counters for the run (where the episodes went).
    pub funnel: crate::consolidation::ConsolidationFunnel,
    /// Set when the run returned `Err`, so "ran and produced nothing" and
    /// "crashed" are distinguishable from the UI without reading logs.
    pub error: Option<String>,
}

impl DistillRunRecord {
    /// Build a record from a distilled-run result.
    pub fn from_result(at: chrono::DateTime<chrono::Utc>, result: &DistillerResult) -> Self {
        Self {
            at,
            episodes_scanned: result.episodes_scanned,
            facts_promoted: result.facts_promoted,
            preferences_promoted: result.preferences_promoted,
            relations_promoted: result.relations_promoted,
            procedures_promoted: result.procedures_promoted,
            autobio_promoted: result.autobio_promoted,
            episodes_marked_consolidated: result.episodes_marked_consolidated,
            funnel: result.funnel.clone(),
            error: None,
        }
    }

    /// Record a run that failed before producing a result. The funnel is kept
    /// (partial, up to the point of failure) because that is the useful half.
    pub fn from_error(
        at: chrono::DateTime<chrono::Utc>,
        error: &str,
        funnel: crate::consolidation::ConsolidationFunnel,
    ) -> Self {
        Self {
            at,
            funnel,
            error: Some(error.to_string()),
            ..Default::default()
        }
    }

    /// Total promoted nodes across all categories.
    pub fn total_promoted(&self) -> usize {
        self.facts_promoted
            + self.preferences_promoted
            + self.relations_promoted
            + self.procedures_promoted
            + self.autobio_promoted
    }
}

/// Lightweight scheduling policy for one instance's consolidation runs.
///
/// Same state machine as the Runtime's `ConsolidationTimer`: interval gates for
/// the distiller and the forgetting scan, an idle stamp reset by agent activity,
/// and the last run summary. The config is behind an `RwLock` so a panel
/// `PUT` takes effect on the next poll without tearing the loop down
/// (ADR-071 D6).
pub struct ConsolidationTimer {
    config: RwLock<SchedulerConfig>,
    state: AsyncMutex<TimerState>,
}

struct TimerState {
    last_active_at: chrono::DateTime<chrono::Utc>,
    /// Unconsolidated-episode backlog (distiller input, ADR-071 D1).
    episode_count: usize,
    last_distill_at: chrono::DateTime<chrono::Utc>,
    last_forgetting_at: chrono::DateTime<chrono::Utc>,
    last_distill: Option<DistillRunRecord>,
}

impl ConsolidationTimer {
    /// A timer whose gates start "just ran", so nothing fires on startup.
    pub fn new(config: SchedulerConfig) -> Self {
        let now = chrono::Utc::now();
        Self {
            config: RwLock::new(config),
            state: AsyncMutex::new(TimerState {
                last_active_at: now,
                episode_count: 0,
                last_distill_at: now,
                last_forgetting_at: now,
                last_distill: None,
            }),
        }
    }

    /// Notify the timer that the agent is active. Resets the idle timer.
    pub async fn notify_active(&self) {
        let mut state = self.state.lock().await;
        state.last_active_at = chrono::Utc::now();
    }

    /// Update the unconsolidated-episode backlog (distiller input, ADR-071 D1).
    pub async fn update_episode_count(&self, count: usize) {
        let mut state = self.state.lock().await;
        state.episode_count = count;
    }

    /// Store a run summary and reset the interval gate (ADR-071 D2).
    pub async fn record_distill_result(&self, result: &DistillRunRecord) {
        let mut state = self.state.lock().await;
        state.last_distill_at = result.at;
        state.last_distill = Some(result.clone());
    }

    /// The most recent distiller-run summary, if any (ADR-071 D2).
    pub async fn last_distill_result(&self) -> Option<DistillRunRecord> {
        self.state.lock().await.last_distill.clone()
    }

    /// Whether the EpisodicDistiller should run (ADR-071 D1): the interval
    /// gate first, then backlog accumulation or idle-with-backlog.
    pub async fn should_run_distill(&self) -> Option<TriggerReason> {
        let state = self.state.lock().await;
        let cfg = self.config();
        if !cfg.distiller_enabled {
            return None;
        }
        let now = chrono::Utc::now();

        // Interval gate (outer): at most one run per configured period.
        if (now - state.last_distill_at).num_seconds() < cfg.distiller_interval_secs as i64 {
            return None;
        }

        // Inner gate: backlog accumulation or idle with a non-empty backlog.
        if state.episode_count >= cfg.distiller_accumulation {
            return Some(TriggerReason::DistillerAccumulation);
        }
        let idle_secs = (now - state.last_active_at).num_seconds();
        if idle_secs >= cfg.distiller_idle_secs as i64 && state.episode_count > 0 {
            return Some(TriggerReason::DistillerIdle);
        }
        None
    }

    /// Whether the episodic forgetting scan should run (opt-in + interval gate,
    /// so an enabled scan never hits the full Episodic table on every tick).
    pub async fn should_run_forgetting(&self) -> Option<TriggerReason> {
        let state = self.state.lock().await;
        let cfg = self.config();
        if !cfg.forgetting_enabled {
            return None;
        }
        if (chrono::Utc::now() - state.last_forgetting_at).num_seconds()
            < cfg.forgetting_interval_secs as i64
        {
            return None;
        }
        Some(TriggerReason::ForgettingInterval)
    }

    /// Record that the forgetting scan just ran (interval gate).
    pub async fn mark_forgetting_run(&self) {
        let mut state = self.state.lock().await;
        state.last_forgetting_at = chrono::Utc::now();
    }

    /// Snapshot the current scheduler policy.
    pub fn config(&self) -> SchedulerConfig {
        self.config
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Swap the policy at runtime (ADR-071 D6). Idle / backlog / last-run state
    /// is deliberately preserved across the swap.
    pub fn update_config(&self, new_config: SchedulerConfig) {
        tracing::info!(
            distiller_enabled = new_config.distiller_enabled,
            distiller_interval_secs = new_config.distiller_interval_secs,
            distiller_accumulation = new_config.distiller_accumulation,
            distiller_idle_secs = new_config.distiller_idle_secs,
            forgetting_enabled = new_config.forgetting_enabled,
            "ConsolidationTimer: scheduler config updated (ADR-071 D6)"
        );
        *self
            .config
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_config;
    }

    /// Seconds since the last agent activity.
    pub async fn idle_secs(&self) -> i64 {
        let state = self.state.lock().await;
        (chrono::Utc::now() - state.last_active_at).num_seconds()
    }

    /// Current unconsolidated-episode backlog (ADR-071 D1).
    pub async fn episode_count(&self) -> usize {
        self.state.lock().await.episode_count
    }

    /// Seconds since the last distiller run.
    pub async fn secs_since_distill(&self) -> i64 {
        let state = self.state.lock().await;
        (chrono::Utc::now() - state.last_distill_at).num_seconds()
    }
}

/// `[distiller]` policy as persisted for one instance. Field names mirror the
/// `GET /memory/consolidation/status` `distiller` object, so the panel reads and
/// writes the same shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DistillerSettings {
    pub enabled: bool,
    pub interval_secs: u64,
    pub accumulation_threshold: usize,
    pub idle_secs: u64,
    pub batch_size: usize,
    pub min_pending_age_hours: u64,
    /// Layer 1 of the model candidate chain (empty = use `[distiller].models`,
    /// then the Gateway's global compact chain, then each provider's own).
    pub models: Vec<CompactModelRef>,
}

impl Default for DistillerSettings {
    fn default() -> Self {
        Self::from_scheduler(&SchedulerConfig::default(), Vec::new())
    }
}

/// `[forgetting]` policy as persisted for one instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForgettingSettings {
    pub enabled: bool,
    pub half_life_days: u64,
    pub dormant_threshold: f32,
    pub archive_days: u64,
    pub interval_secs: u64,
}

impl Default for ForgettingSettings {
    fn default() -> Self {
        let cfg = SchedulerConfig::default();
        Self {
            enabled: cfg.forgetting_enabled,
            half_life_days: cfg.forgetting_half_life_days,
            dormant_threshold: cfg.forgetting_dormant_threshold,
            archive_days: cfg.forgetting_archive_days,
            interval_secs: cfg.forgetting_interval_secs,
        }
    }
}

/// The effective per-instance pipeline policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct InstanceSettings {
    pub distiller: DistillerSettings,
    pub forgetting: ForgettingSettings,
}

impl InstanceSettings {
    /// Policy derived from the service `config.toml` — what an instance without
    /// its own `consolidation.json` runs on.
    pub fn from_config(config: &MemoryServiceConfig) -> Self {
        let sched = config.scheduler_config();
        Self {
            distiller: DistillerSettings::from_scheduler(&sched, config.distiller.models.clone()),
            forgetting: ForgettingSettings {
                enabled: sched.forgetting_enabled,
                half_life_days: sched.forgetting_half_life_days,
                dormant_threshold: sched.forgetting_dormant_threshold,
                archive_days: sched.forgetting_archive_days,
                interval_secs: sched.forgetting_interval_secs,
            },
        }
    }

    /// Project back onto the scheduler policy the timer consumes.
    pub fn scheduler_config(&self) -> SchedulerConfig {
        let d = &self.distiller;
        let f = &self.forgetting;
        SchedulerConfig {
            accumulation_threshold: d.accumulation_threshold,
            batch_size: d.batch_size,
            min_pending_age_hours: d.min_pending_age_hours,
            distiller_enabled: d.enabled,
            // The distiller reads its parameters from this sub-config; the
            // panel's batch size has to land here or it does nothing.
            distiller_config: Some(DistillerConfig {
                batch_size: d.batch_size,
                ..DistillerConfig::default()
            }),
            distiller_interval_secs: d.interval_secs,
            distiller_accumulation: d.accumulation_threshold,
            distiller_idle_secs: d.idle_secs,
            forgetting_enabled: f.enabled,
            forgetting_half_life_days: f.half_life_days,
            forgetting_dormant_threshold: f.dormant_threshold,
            forgetting_archive_days: f.archive_days,
            forgetting_interval_secs: f.interval_secs,
            ..SchedulerConfig::default()
        }
    }
}

impl DistillerSettings {
    fn from_scheduler(sched: &SchedulerConfig, models: Vec<CompactModelRef>) -> Self {
        Self {
            enabled: sched.distiller_enabled,
            interval_secs: sched.distiller_interval_secs,
            accumulation_threshold: sched.distiller_accumulation,
            idle_secs: sched.distiller_idle_secs,
            // The distiller's own sub-config wins when present, so the service
            // reads back the number the run actually uses.
            batch_size: sched
                .distiller_config
                .as_ref()
                .map(|c| c.batch_size)
                .unwrap_or(sched.batch_size),
            min_pending_age_hours: sched.min_pending_age_hours,
            models,
        }
    }

    /// Serialize for the status endpoint: the candidate chain the panel's model
    /// picker reads back and writes through `PUT` (additive to the Runtime's
    /// shape, which had no model field — the old UI ignored unknown keys).
    pub fn models_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.models).unwrap_or_else(|_| serde_json::Value::Array(vec![]))
    }
}

/// The pipeline: credential holder + per-instance scheduler + background loops.
pub struct Pipeline {
    config: Arc<MemoryServiceConfig>,
    instances: Arc<InstanceRegistry>,
    /// Installed embedding backend. Read by the retrieval paths (through
    /// [`crate::service::state::MemoryState::embedding`]) and by the distiller.
    embedding: Arc<RwLock<Option<Arc<dyn EmbeddingProvider>>>>,
    /// Latest Gateway snapshot, `None` until the first pull succeeds.
    resources: Arc<RwLock<Option<Arc<GlobalResources>>>>,
    /// `instance_id` → scheduling state.
    timers: RwLock<HashMap<String, Arc<ConsolidationTimer>>>,
    /// `instance_id` → run lock, so a manual trigger and the background loop
    /// cannot distill the same namespace twice at once.
    runs: RwLock<HashMap<String, Arc<AsyncMutex<()>>>>,
    client: RwLock<Option<ResourceClient>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    running: Arc<AtomicBool>,
    poll_interval: Duration,
}

impl Pipeline {
    /// Build the pipeline. Nothing is spawned and no credential is installed
    /// yet — [`Self::install_embedding_from_config`] and [`Self::start`] do that,
    /// so a test can construct one without touching the network.
    pub fn new(config: Arc<MemoryServiceConfig>, instances: Arc<InstanceRegistry>) -> Self {
        Self {
            config,
            instances,
            embedding: Arc::new(RwLock::new(None)),
            resources: Arc::new(RwLock::new(None)),
            timers: RwLock::new(HashMap::new()),
            runs: RwLock::new(HashMap::new()),
            client: RwLock::new(None),
            tasks: Mutex::new(Vec::new()),
            running: Arc::new(AtomicBool::new(false)),
            poll_interval: POLL_INTERVAL,
        }
    }

    /// Shorter poll interval for tests (production keeps [`POLL_INTERVAL`]).
    pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
        self.poll_interval = poll_interval;
        self
    }

    // ── Credentials ─────────────────────────────────────────────────────

    /// Install the Gateway client (from `main.rs`, which owns the CLI plumbing:
    /// Gateway host/port and the node token = the broker password, ADR-076).
    /// Without a client the pipeline still runs — on `config.toml` alone.
    pub fn set_resource_client(&self, client: ResourceClient) {
        *self
            .client
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(client);
    }

    /// The installed client, if any.
    pub fn resource_client(&self) -> Option<ResourceClient> {
        self.client
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The latest Gateway snapshot (`None` before the first successful pull).
    pub fn snapshot(&self) -> Option<Arc<GlobalResources>> {
        self.resources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Apply a snapshot: remember it, then re-derive the embedding backend from
    /// it. Called by the refresh loop; `pub` so tests (and P2's status endpoint)
    /// can drive it directly.
    pub fn apply_snapshot(&self, snapshot: GlobalResources) {
        let embedding = Self::embedding_from(&snapshot, &self.config);
        let dim = embedding.as_ref().map(|p| p.dimension()).filter(|d| *d > 0);
        tracing::info!(
            providers = snapshot.providers.len(),
            embedding = embedding.is_some(),
            dimension = ?dim,
            gateway_instance = %snapshot.instance_id,
            "global-resources applied to the memory pipeline"
        );
        *self
            .resources
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Arc::new(snapshot));
        self.install_embedding(embedding, dim);
    }

    /// Install the embedding backend described by `[embedding]` in config.toml.
    /// Done at startup so retrieval works before the Gateway answers.
    pub fn install_embedding_from_config(&self) {
        if !self.config.embedding.enabled {
            tracing::info!("embedding disabled via [embedding].enabled — retrieval stays lexical");
            return;
        }
        let client =
            HttpEmbeddingClient::from_config(&self.config.embedding, self.config.embedding_dim)
                .map(|c| -> Arc<dyn EmbeddingProvider> { Arc::new(c) });
        match client {
            Ok(provider) => {
                let dim = provider.dimension();
                self.install_embedding(Some(provider), Some(dim));
            }
            Err(e) => tracing::error!(
                error = %e,
                base_url = %self.config.embedding.base_url,
                "could not build the embedding client — retrieval stays lexical"
            ),
        }
    }

    fn install_embedding(&self, provider: Option<Arc<dyn EmbeddingProvider>>, dim: Option<usize>) {
        if let Some(provider) = provider.as_ref() {
            tracing::info!(
                name = %provider.name(),
                dimension = provider.dimension(),
                "memory embedding backend installed"
            );
        }
        // A brand-new instance store must be created at the width the live
        // provider emits, otherwise the first write fails the dimension check
        // against a model the operator just switched to. Existing stores keep
        // their own width (that is a rebuild, not a switch).
        if let Some(dim) = dim.filter(|d| *d > 0) {
            self.instances.set_embedding_dim(dim);
        }
        *self
            .embedding
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = provider;
    }

    /// The installed embedding backend.
    pub fn embedding(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embedding
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Replace the embedding backend (tests, and any later P2-driven override).
    pub fn set_embedding(&self, provider: Option<Arc<dyn EmbeddingProvider>>) {
        *self
            .embedding
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = provider;
    }

    /// Resolve the embedding backend from a snapshot: cloud selection first,
    /// then the local ONNX endpoint, then the `config.toml` section — which is
    /// what keeps a Gateway that publishes no embedding topic from disabling
    /// retrieval on a node that runs acowork-embed. Only `[embedding].enabled =
    /// false` can take the backend away.
    fn embedding_from(
        snapshot: &GlobalResources,
        config: &MemoryServiceConfig,
    ) -> Option<Arc<dyn EmbeddingProvider>> {
        if !config.embedding.enabled {
            return None;
        }
        let Some(base_url) = snapshot.embedding.as_ref().and_then(|s| s.base_url.clone()) else {
            // Nothing published: the node's own answer stays in force.
            return HttpEmbeddingClient::from_config(&config.embedding, config.embedding_dim)
                .map(|c| -> Arc<dyn EmbeddingProvider> { Arc::new(c) })
                .map_err(|e| {
                    tracing::error!(
                        error = %e,
                        base_url = %config.embedding.base_url,
                        "could not build the config embedding client either"
                    );
                })
                .ok();
        };
        let snap = snapshot.embedding.as_ref()?;
        let dimension = if snap.dimension > 0 {
            snap.dimension
        } else {
            config.embedding_dim
        };
        let client = HttpEmbeddingClient::new(
            &base_url,
            snap.api_key.as_deref(),
            snap.model.as_deref().unwrap_or(""),
            dimension,
            Duration::from_millis(config.embedding.timeout_ms.max(1)),
        )
        .map_err(|e| {
            tracing::error!(error = %e, base_url = %base_url, "snapshot embedding endpoint unusable");
        })
        .ok()?;
        Some(Arc::new(client))
    }

    // ── Per-instance policy ─────────────────────────────────────────────

    /// `{instance_dir}/consolidation.json`.
    pub fn settings_path(&self, instance_id: &str) -> Result<PathBuf> {
        Ok(self
            .instances
            .instance_dir(instance_id)?
            .join(SETTINGS_FILE_NAME))
    }

    /// Read the instance policy, falling back to `config.toml`.
    ///
    /// A missing file is normal (never touched the panel). An unparseable one is
    /// logged and ignored: the panel must keep working, and the file is written
    /// only by this service, so a hand-edit breaking the pipeline is the worse
    /// outcome.
    pub fn settings(&self, instance_id: &str) -> InstanceSettings {
        let path = match self.settings_path(instance_id) {
            Ok(p) => p,
            Err(_) => return InstanceSettings::from_config(&self.config),
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<InstanceSettings>(&text) {
                Ok(settings) => settings,
                Err(e) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "instance policy unreadable, falling back to config.toml"
                    );
                    InstanceSettings::from_config(&self.config)
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                InstanceSettings::from_config(&self.config)
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "could not read instance policy");
                InstanceSettings::from_config(&self.config)
            }
        }
    }

    /// Persist the instance policy and push it into the live timer, so the next
    /// poll (at most `poll_interval` away) acts on it without a restart.
    pub fn save_settings(&self, instance_id: &str, settings: &InstanceSettings) -> Result<()> {
        let path = self.settings_path(instance_id)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(settings)
            .map_err(|e| ServiceError::Internal(format!("serialize policy: {e}")))?;
        std::fs::write(&path, text)?;
        tracing::info!(
            instance_id,
            path = %path.display(),
            distiller_enabled = settings.distiller.enabled,
            forgetting_enabled = settings.forgetting.enabled,
            "instance pipeline policy saved"
        );
        // Update an already-live timer; a timer created later reads the file.
        if let Some(timer) = self
            .timers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(instance_id)
        {
            timer.update_config(settings.scheduler_config());
        }
        Ok(())
    }

    /// The timer for one instance, created on first touch from the persisted
    /// policy.
    pub fn timer(&self, instance_id: &str) -> Result<Arc<ConsolidationTimer>> {
        crate::service::state::validate_instance_id(instance_id)?;
        if let Some(hit) = self
            .timers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(instance_id)
        {
            return Ok(hit.clone());
        }
        let timer = Arc::new(ConsolidationTimer::new(
            self.settings(instance_id).scheduler_config(),
        ));
        // Re-check under the write lock: two first touches of one instance must
        // not end up with two timers (only one would ever be polled).
        let mut map = self
            .timers
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = map.get(instance_id) {
            return Ok(existing.clone());
        }
        map.insert(instance_id.to_string(), timer.clone());
        Ok(timer)
    }

    /// Record agent activity for one instance (the service's substitute for the
    /// Runtime's per-turn `notify_active`: `memory_store` is the signal that this
    /// Agent is working). Only touches an existing timer — a recall against a
    /// namespace that has never distilled must not allocate scheduling state.
    pub fn note_activity(&self, instance_id: &str) {
        if let Some(timer) = self
            .timers
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(instance_id)
            .cloned()
        {
            let timer = timer.clone();
            tokio::spawn(async move { timer.notify_active().await });
        }
    }

    fn run_lock(&self, instance_id: &str) -> Arc<AsyncMutex<()>> {
        let mut map = self
            .runs
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.entry(instance_id.to_string())
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }

    // ── Distiller execution ─────────────────────────────────────────────

    /// Build the candidate chain for one instance: panel override first, then
    /// `[distiller].models`, then the Gateway global compact chain, then each
    /// provider's own compact model.
    pub fn llm_for(&self, settings: &InstanceSettings) -> ChainLlm {
        let candidates = resolve_candidates(
            self.snapshot().as_deref(),
            &settings.distiller.models,
            &self.config.distiller.models,
        );
        if candidates.is_empty() {
            tracing::warn!(
                "distiller has no usable candidate — check the Gateway provider list \
                 and [distiller].models (only OpenAI-compatible providers are callable)"
            );
        }
        ChainLlm::new(candidates, LLM_TIMEOUT)
    }

    /// One EpisodicDistiller pass, identical execution path for the loop and the
    /// manual trigger. `Some` after a run, `None` when it failed.
    async fn distill_step(
        &self,
        store: &Arc<crate::sqlite::SqliteStore>,
        llm: &ChainLlm,
        embedding_fn: Option<&EmbeddingFn>,
        settings: &InstanceSettings,
    ) -> Option<DistillerResult> {
        let distiller = DefaultEpisodicDistiller;
        let config = settings.scheduler_config();
        let distiller_config = config.distiller_config.clone().unwrap_or_default();
        let result = match distiller
            .run(store.as_ref(), Some(llm), embedding_fn, &distiller_config)
            .await
        {
            Ok(result) => {
                tracing::info!(
                    episodes_scanned = result.episodes_scanned,
                    facts_promoted = result.facts_promoted,
                    preferences_promoted = result.preferences_promoted,
                    relations_promoted = result.relations_promoted,
                    procedures_promoted = result.procedures_promoted,
                    autobio_promoted = result.autobio_promoted,
                    episodes_marked_consolidated = result.episodes_marked_consolidated,
                    evaluations = result.promotion_evaluations.len(),
                    funnel = %result.funnel.summary(),
                    "EpisodicDistiller run complete (ADR-068)"
                );
                for eval in &result.promotion_evaluations {
                    tracing::debug!(
                        kind = ?eval.promoted_kind,
                        decision = ?eval.decision,
                        confidence = eval.llm_confidence,
                        reasoning = %eval.llm_reasoning,
                        episodes = ?eval.source_episode_ids,
                        "distiller evaluation"
                    );
                }
                Some(result)
            }
            Err(e) => {
                tracing::warn!(error = %e, "EpisodicDistiller run failed (ADR-068)");
                None
            }
        };

        // ADR-068 M8: 30-day Relationship promotion. Rule-based, idempotent,
        // and the only producer of the category.
        match distiller.promote_autobio_relationship(store.as_ref()).await {
            Ok(Some(eval)) => tracing::info!(
                kind = ?eval.promoted_kind,
                node_id = eval.promoted_node_id,
                rule = %eval.llm_reasoning,
                "Relationship node promoted (ADR-068 M8)"
            ),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "Relationship promotion failed (ADR-068 M8)");
            }
        }
        result
    }

    /// Bridge an async embedding provider into the distiller's synchronous
    /// clustering call.
    ///
    /// `Handle::block_on` inside a tokio worker panics ("Cannot start a runtime
    /// from within a runtime"), so on the multi-thread runtime the call is
    /// handed to a blocking worker via `block_in_place`. `None` (current-thread
    /// runtime, or no runtime at all — a plain unit test) means the caller runs
    /// without embeddings, which the loop treats as "do not distill".
    fn bridge_of(provider: Arc<dyn EmbeddingProvider>) -> Option<EmbeddingFn> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            tracing::debug!(
                flavor = ?handle.runtime_flavor(),
                "embedding bridge unavailable on a non-multi-thread runtime — \
                 distiller would degrade to exact-key clustering"
            );
            return None;
        }
        Some(Arc::new(move |text: &str| -> Vec<f32> {
            let text = text.to_string();
            let provider = provider.clone();
            match tokio::task::block_in_place(|| handle.block_on(provider.embed(&text))) {
                Ok(vec) => vec,
                Err(e) => {
                    tracing::warn!(error = %e, "embedding failed during consolidation, using zero vector");
                    vec![]
                }
            }
        }))
    }

    /// The bridge for the currently installed backend, if one can be built.
    fn embedding_bridge(&self) -> Option<EmbeddingFn> {
        self.embedding().and_then(Self::bridge_of)
    }

    /// Manual trigger (`POST /memory/distill`), force semantics: bypasses the
    /// periodic gate but still honours the `enabled` switch — a distiller that
    /// is explicitly off must not produce semantic-layer nodes.
    ///
    /// `Ok(Some(..))` ran, `Ok(None)` means the run died (the loop's wording is
    /// recorded either way), `Err` reproduces the Runtime's error strings so the
    /// handler can keep classifying `409` vs `500`.
    pub async fn distill_once(
        &self,
        instance_id: &str,
    ) -> std::result::Result<Option<DistillerResult>, String> {
        let settings = self.settings(instance_id);
        if !settings.distiller.enabled {
            return Err(
                "distiller is disabled (config.toml [distiller].enabled = false, or the panel \
                 override for this instance)"
                    .to_string(),
            );
        }
        let store = self
            .instances
            .get(instance_id)
            .map_err(|e| format!("memory provider not initialized: {e}"))?;
        // Same invariant as the background loop: without a callable embedding
        // path the distiller recalls nothing and projects every episode as new,
        // so refuse rather than duplicate the semantic layer. A provider that
        // exists but cannot be bridged (single-thread runtime) is the same
        // failure for the caller.
        let embedding = self
            .embedding()
            .ok_or_else(|| "embedding provider not initialized".to_string())?;
        let embedding_fn = Self::bridge_of(embedding).ok_or_else(|| {
            "embedding provider not initialized (no synchronous embedding bridge on this runtime)"
                .to_string()
        })?;
        let timer = self
            .timer(instance_id)
            .map_err(|e| format!("memory provider not initialized: {e}"))?;
        let run_lock = self.run_lock(instance_id);

        // Held for the whole pass: two overlapping distills of one namespace
        // would double-promote the same episodes.
        let _guard = run_lock.lock().await;
        let llm = self.llm_for(&settings);
        let result = self
            .distill_step(&store, &llm, Some(&embedding_fn), &settings)
            .await;
        match &result {
            Some(result) => {
                timer
                    .record_distill_result(&DistillRunRecord::from_result(
                        chrono::Utc::now(),
                        result,
                    ))
                    .await;
            }
            None => {
                timer
                    .record_distill_result(&DistillRunRecord::from_error(
                        chrono::Utc::now(),
                        "distiller run failed (see logs)",
                        Default::default(),
                    ))
                    .await;
            }
        }
        Ok(result)
    }

    /// `GET /memory/consolidation/status` for one instance — the Runtime's
    /// response shape, field names and all.
    pub async fn status(&self, instance_id: &str) -> Result<serde_json::Value> {
        let timer = self.timer(instance_id)?;
        let settings = self.settings(instance_id);
        let idle_secs = timer.idle_secs().await;
        let episode_backlog = timer.episode_count().await;
        let secs_since_distill = timer.secs_since_distill().await;
        let last_run = timer.last_distill_result().await;

        let last_run_json = last_run.as_ref().map(|r| {
            serde_json::json!({
                "at": r.at.to_rfc3339(),
                "episodes_scanned": r.episodes_scanned,
                "total_promoted": r.total_promoted(),
                "facts_promoted": r.facts_promoted,
                "preferences_promoted": r.preferences_promoted,
                "relations_promoted": r.relations_promoted,
                "procedures_promoted": r.procedures_promoted,
                "autobio_promoted": r.autobio_promoted,
                "episodes_marked_consolidated": r.episodes_marked_consolidated,
                "error": r.error,
                "funnel": {
                    "scanned": r.funnel.scanned,
                    "backlog_remaining": r.funnel.backlog_remaining,
                    "llm_calls": r.funnel.llm_calls,
                    "llm_calls_failed": r.funnel.llm_calls_failed,
                    "llm_calls_truncated": r.funnel.llm_calls_truncated,
                    "below_importance": r.funnel.below_importance,
                    "candidates_recalled": r.funnel.candidates_recalled,
                    "projected": r.funnel.projected,
                    "verdict_no_merge": r.funnel.verdict_no_merge,
                    "verdict_merged": r.funnel.verdict_merged,
                    "verdict_superseded": r.funnel.verdict_superseded,
                    "episodes_consolidated": r.funnel.episodes_consolidated,
                    "episodes_deferred": r.funnel.episodes_deferred,
                    "errors": r.funnel.errors,
                },
            })
        });

        Ok(serde_json::json!({
            "idle_secs": idle_secs,
            "bg_task_running": self.is_running(),
            "distiller": {
                "enabled": settings.distiller.enabled,
                "episode_backlog": episode_backlog,
                "secs_since_distill": secs_since_distill,
                "interval_secs": settings.distiller.interval_secs,
                "accumulation_threshold": settings.distiller.accumulation_threshold,
                "idle_secs": settings.distiller.idle_secs,
                "last_run": last_run_json,
                // Additive to the Runtime shape: the chain the panel's model
                // picker writes back through PUT.
                "models": settings.distiller.models_json(),
            },
            "forgetting": {
                "enabled": settings.forgetting.enabled,
                "half_life_days": settings.forgetting.half_life_days,
                "dormant_threshold": settings.forgetting.dormant_threshold,
                "archive_days": settings.forgetting.archive_days,
                "interval_secs": settings.forgetting.interval_secs,
            },
        }))
    }

    // ── Background loops ────────────────────────────────────────────────

    /// Whether the background loops are live (what `bg_task_running` reports).
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Spawn the credential-refresh and consolidation loops. Idempotent: a
    /// second call is a no-op, so a restart-supervision path cannot double-tick.
    ///
    /// Takes the `Arc` because the loops need to own a reference to the pipeline
    /// (`MemoryState` holds the only strong one).
    pub fn start(self: &Arc<Self>) {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.is_running() {
            tracing::debug!("memory pipeline already running");
            return;
        }
        self.running.store(true, Ordering::SeqCst);
        tasks.push(tokio::spawn(resource_loop(self.clone())));
        tasks.push(tokio::spawn(consolidation_loop(self.clone())));
        tracing::info!(
            poll_interval_secs = self.poll_interval.as_secs(),
            "memory consolidation pipeline started (distiller + forgetting, per instance)"
        );
    }

    /// Stop the loops, called by the service's graceful shutdown.
    pub fn shutdown(&self) {
        self.running.store(false, Ordering::SeqCst);
        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        for task in tasks {
            task.abort();
        }
    }
}

/// Refresh loop: pull the Gateway snapshot, apply it, wait, repeat.
async fn resource_loop(this: Arc<Pipeline>) {
    let Some(client) = this.resource_client() else {
        tracing::info!(
            "no Gateway resource client configured — the distiller runs on config.toml only"
        );
        return;
    };
    loop {
        if !this.is_running() {
            return;
        }
        let mut embedding_known = true;
        match client.pull_with_retry(PULL_MAX_DURATION).await {
            Some(snapshot) => {
                embedding_known = snapshot
                    .embedding
                    .as_ref()
                    .and_then(|e| e.base_url.as_ref())
                    .is_some();
                this.apply_snapshot(snapshot);
            }
            None => tracing::warn!(
                url = %client.url(),
                "global-resources pull did not complete — keeping the previous credentials"
            ),
        }
        // A snapshot without an embedding endpoint is a startup race — the
        // embed sidecar is still loading its model while the Gateway already
        // serves — not a steady state. Until the real endpoint (and width)
        // arrives, the pipeline runs on the config fallback whose declared
        // dimension may not match the model the sidecar loads; re-pull fast so
        // that window is seconds, not one full refresh interval.
        let wait = if embedding_known || !this.config.embedding.enabled {
            REFRESH_INTERVAL
        } else {
            EMBEDDING_PENDING_INTERVAL
        };
        tokio::time::sleep(wait).await;
    }
}

/// Retry interval while the Gateway snapshot carries no embedding endpoint
/// (see [`resource_loop`]).
const EMBEDDING_PENDING_INTERVAL: Duration = Duration::from_secs(15);

/// Consolidation loop: one tick every `poll_interval`, walking every known
/// instance. The Runtime loop's body, lifted to per-namespace.
async fn consolidation_loop(this: Arc<Pipeline>) {
    let mut interval = tokio::time::interval(this.poll_interval);
    // First tick fires immediately — skip it so nothing runs on startup.
    interval.tick().await;
    loop {
        interval.tick().await;
        if !this.is_running() {
            return;
        }
        for instance_id in this.instances.list() {
            if let Err(e) = this.tick_instance(&instance_id).await {
                tracing::warn!(instance_id = %instance_id, error = %e, "consolidation tick failed");
            }
        }
    }
}

impl Pipeline {
    /// One poll for one instance: refresh the backlog gauge, then run whichever
    /// pipeline its gates opened for.
    async fn tick_instance(&self, instance_id: &str) -> Result<()> {
        let timer = self.timer(instance_id)?;
        let settings = self.settings(instance_id);
        let config = settings.scheduler_config();

        if config.distiller_enabled {
            let store = self.instances.get(instance_id)?;
            match store.count_unconsolidated_episodes() {
                Ok(count) => timer.update_episode_count(count).await,
                Err(e) => {
                    tracing::warn!(
                        instance_id,
                        error = %e,
                        "failed to count unconsolidated episodes"
                    );
                    return Ok(());
                }
            }
        }

        let forgetting_trigger = timer.should_run_forgetting().await;
        let distill_trigger = if config.distiller_enabled {
            timer.should_run_distill().await
        } else {
            None
        };
        if forgetting_trigger.is_none() && distill_trigger.is_none() {
            return Ok(());
        }

        if distill_trigger.is_some() {
            let backlog = timer.episode_count().await;
            tracing::info!(
                instance_id,
                reason = ?distill_trigger,
                backlog,
                "EpisodicDistiller triggered"
            );
            let store = self.instances.get(instance_id)?;
            let run_lock = self.run_lock(instance_id);
            let _guard = run_lock.lock().await;
            let embedding_fn = self.embedding_bridge();
            if embedding_fn.is_none() {
                // No embedding ⇒ no candidate recall ⇒ the distiller would
                // promote every scanned episode as a brand-new node. Refuse, and
                // say so in the record the panel reads, rather than quietly
                // duplicating the semantic layer.
                tracing::warn!(
                    instance_id,
                    "distiller run skipped: no embedding backend installed"
                );
                timer
                    .record_distill_result(&DistillRunRecord::from_error(
                        chrono::Utc::now(),
                        "embedding provider not initialized — distiller run skipped",
                        Default::default(),
                    ))
                    .await;
            } else {
                let llm = self.llm_for(&settings);
                let result = self
                    .distill_step(&store, &llm, embedding_fn.as_ref(), &settings)
                    .await;
                let record = match &result {
                    Some(result) => DistillRunRecord::from_result(chrono::Utc::now(), result),
                    None => DistillRunRecord::from_error(
                        chrono::Utc::now(),
                        "distiller run failed (see the Step 2a / promotion warning above)",
                        Default::default(),
                    ),
                };
                timer.record_distill_result(&record).await;
            }
        }

        if forgetting_trigger.is_some() {
            tracing::info!(instance_id, "Episodic forgetting scan triggered");
            let store = self.instances.get(instance_id)?;
            let decay = EpisodicDecayConfig {
                enabled: true,
                half_life_days: config.forgetting_half_life_days,
                dormant_threshold: config.forgetting_dormant_threshold,
                archive_days: config.forgetting_archive_days,
            };
            match store.run_episodic_decay_scan(&decay) {
                Ok(result) => tracing::info!(
                    instance_id,
                    to_dormant = result.to_dormant,
                    purged = result.purged,
                    "Episodic forgetting scan complete"
                ),
                Err(e) => {
                    tracing::warn!(instance_id, error = %e, "Episodic forgetting scan failed")
                }
            }
            timer.mark_forgetting_run().await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::config::EmbeddingSection;
    use crate::service::resources::EmbeddingSnapshot;
    use crate::service::state::MemoryState;
    use serde_json::json;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    /// Config with no embedding endpoint: a test installs a backend explicitly
    /// instead of inheriting one that would dial a sidecar nobody started.
    fn base_config(dir: &std::path::Path) -> MemoryServiceConfig {
        MemoryServiceConfig {
            data_dir: dir.to_path_buf(),
            embedding: EmbeddingSection {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn state_in(dir: &std::path::Path) -> MemoryState {
        MemoryState::new(base_config(dir)).expect("state")
    }

    /// Open the gate fully (`interval = 0`) so a test decides the outcome with
    /// the backlog / idle input only.
    fn open_distill(accumulation: usize, idle_secs: u64) -> SchedulerConfig {
        SchedulerConfig {
            distiller_enabled: true,
            distiller_interval_secs: 0,
            distiller_accumulation: accumulation,
            distiller_idle_secs: idle_secs,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn distill_gate_needs_interval_then_backlog_or_idle() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let timer = state.pipeline().timer(ID).unwrap();

        // Opt-in: the default policy never fires, however full the backlog.
        timer.update_episode_count(10).await;
        assert_eq!(timer.should_run_distill().await, None);

        timer.update_config(open_distill(5, 1800));
        assert_eq!(
            timer.should_run_distill().await,
            Some(TriggerReason::DistillerAccumulation)
        );

        // Below the accumulation threshold an idle Agent is still consolidated.
        timer.update_config(open_distill(100, 1800));
        assert_eq!(timer.should_run_distill().await, None);
        {
            let mut s = timer.state.lock().await;
            s.last_active_at = chrono::Utc::now() - chrono::TimeDelta::minutes(40);
        }
        assert_eq!(
            timer.should_run_distill().await,
            Some(TriggerReason::DistillerIdle)
        );

        // Idle plus an empty episodic layer is nothing to do.
        timer.update_episode_count(0).await;
        assert_eq!(timer.should_run_distill().await, None);

        // Forgetting has its own switch and its own interval gate.
        assert_eq!(timer.should_run_forgetting().await, None);
        timer.update_config(SchedulerConfig {
            forgetting_enabled: true,
            forgetting_interval_secs: 0,
            ..open_distill(5, 1800)
        });
        assert_eq!(
            timer.should_run_forgetting().await,
            Some(TriggerReason::ForgettingInterval)
        );
    }

    #[tokio::test]
    async fn panel_policy_persists_and_hot_updates_the_live_timer() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let pipeline = state.pipeline();

        // No policy file yet: the instance runs on config.toml.
        let fresh = pipeline.settings(ID);
        assert!(!fresh.distiller.enabled, "distiller is opt-in");
        assert_eq!(fresh, InstanceSettings::from_config(&pipeline.config));

        let timer = pipeline.timer(ID).unwrap();
        let mut edited = fresh.clone();
        edited.distiller.enabled = true;
        edited.distiller.interval_secs = 900;
        edited.forgetting.enabled = true;
        pipeline.save_settings(ID, &edited).unwrap();

        assert!(
            pipeline.settings_path(ID).unwrap().exists(),
            "written to the namespace"
        );
        assert_eq!(pipeline.settings(ID), edited, "read back what was written");
        // The already-live timer takes the new policy without a restart.
        assert_eq!(timer.config().distiller_interval_secs, 900);
        assert!(timer.config().distiller_enabled);
        assert!(timer.config().forgetting_enabled);

        // A second instance in the same process keeps the config default.
        let other = "22222222-2222-4222-8222-222222222222";
        assert_eq!(pipeline.settings(other), fresh);
    }

    #[test]
    fn snapshot_embedding_installs_and_retargets_new_stores() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(dir.path());
        config.embedding = EmbeddingSection {
            enabled: true,
            ..Default::default()
        };
        let state = MemoryState::new(config.clone()).unwrap();
        let pipeline = state.pipeline();
        let local_dim = config.embedding_dim;
        assert_eq!(pipeline.embedding().unwrap().dimension(), local_dim);

        pipeline.apply_snapshot(GlobalResources {
            embedding: Some(EmbeddingSnapshot {
                base_url: Some("https://api.example.com/v1".into()),
                model: Some("bge-m3".into()),
                api_key: Some("sk-test".into()),
                dimension: 1024,
                cloud: true,
            }),
            ..Default::default()
        });

        let installed = pipeline.embedding().expect("snapshot backend installed");
        assert_eq!(
            installed.dimension(),
            1024,
            "the cloud selection replaces the local"
        );
        // Brand-new instance files are created at the live width; the registry
        // value is what `InstanceRegistry::get` passes to `SqliteStore::open`.
        assert_eq!(state.instances().embedding_dim(), 1024);
    }

    #[test]
    fn a_snapshot_without_embedding_keeps_the_local_backend() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = base_config(dir.path());
        config.embedding = EmbeddingSection {
            enabled: true,
            ..Default::default()
        };
        let state = MemoryState::new(config.clone()).unwrap();
        let pipeline = state.pipeline();
        // `MemoryState::new` installs the local endpoint before any Gateway call.
        let local = pipeline
            .embedding()
            .expect("local backend installed at startup");
        assert_eq!(local.dimension(), config.embedding_dim);

        // A Gateway that publishes no embedding topic must not disable it.
        pipeline.apply_snapshot(GlobalResources {
            providers: vec![],
            ..Default::default()
        });
        assert!(
            pipeline.embedding().is_some(),
            "local backend survives the pull"
        );
        assert!(
            pipeline.snapshot().is_some(),
            "the snapshot itself is still kept"
        );
    }

    #[test]
    fn embedding_disabled_vetoes_the_snapshot_credential() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        state.pipeline().apply_snapshot(GlobalResources {
            embedding: Some(EmbeddingSnapshot {
                base_url: Some("https://api.example.com/v1".into()),
                dimension: 1024,
                cloud: true,
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(
            state.pipeline().embedding().is_none(),
            "the operator switch outranks the Gateway"
        );
    }

    #[tokio::test]
    async fn manual_distill_refuses_without_an_embedding_path() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let pipeline = state.pipeline();

        // Disabled by default.
        let err = pipeline.distill_once(ID).await.unwrap_err();
        assert!(err.contains("distiller is disabled"), "got {err}");

        // Enabled, but no embedding path — running anyway would project every
        // episode as a brand-new node instead of merging it.
        let mut on = pipeline.settings(ID);
        on.distiller.enabled = true;
        pipeline.save_settings(ID, &on).unwrap();
        let err = pipeline.distill_once(ID).await.unwrap_err();
        assert!(
            err.contains("embedding provider not initialized"),
            "got {err}"
        );
    }

    #[tokio::test]
    async fn status_body_keeps_the_runtime_shape_plus_models() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let body = state.pipeline().status(ID).await.unwrap();

        for key in ["idle_secs", "bg_task_running", "distiller", "forgetting"] {
            assert!(body.get(key).is_some(), "missing top-level {key}: {body}");
        }
        for key in [
            "enabled",
            "episode_backlog",
            "secs_since_distill",
            "interval_secs",
            "accumulation_threshold",
            "idle_secs",
            "last_run",
        ] {
            assert!(
                body["distiller"].get(key).is_some(),
                "missing distiller.{key}"
            );
        }
        for key in [
            "enabled",
            "half_life_days",
            "dormant_threshold",
            "archive_days",
            "interval_secs",
        ] {
            assert!(
                body["forgetting"].get(key).is_some(),
                "missing forgetting.{key}"
            );
        }
        assert!(body["distiller"]["last_run"].is_null(), "no run yet");
        assert_eq!(body["bg_task_running"], json!(false), "loops not started");
        // The additive field the panel's model card reads.
        assert_eq!(body["distiller"]["models"], json!([]));

        // An unusable namespace is an error, not an empty status.
        assert!(state.pipeline().status("../escape").await.is_err());
    }

    #[tokio::test]
    async fn note_activity_restarts_the_idle_gate_of_a_live_timer_only() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let pipeline = state.pipeline();

        pipeline.note_activity(ID);
        assert!(
            !pipeline.timers.read().unwrap().contains_key(ID),
            "a store nobody asked about gets no scheduling state"
        );

        let timer = pipeline.timer(ID).unwrap();
        {
            let mut s = timer.state.lock().await;
            s.last_active_at = chrono::Utc::now() - chrono::TimeDelta::hours(2);
        }
        assert!(timer.idle_secs().await > 3600);
        pipeline.note_activity(ID);
        // The stamp is taken on a spawned task (a store call must not wait on it).
        for _ in 0..50 {
            if timer.idle_secs().await < 60 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            timer.idle_secs().await < 60,
            "memory_store resets the idle gate"
        );
    }

    #[tokio::test]
    async fn loops_start_once_and_shutdown_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let state = state_in(dir.path());
        let pipeline = state.pipeline().clone();

        assert!(!pipeline.is_running());
        pipeline.start();
        assert_eq!(
            pipeline.tasks.lock().unwrap().len(),
            2,
            "resource loop + consolidation loop"
        );
        // A second start must not double-tick the scheduler.
        pipeline.start();
        assert_eq!(pipeline.tasks.lock().unwrap().len(), 2);

        pipeline.shutdown();
        assert!(!pipeline.is_running());
        assert!(pipeline.tasks.lock().unwrap().is_empty());
        pipeline.shutdown();
    }
}

