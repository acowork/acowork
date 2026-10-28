//! Gateway main module
//!
//! The Gateway struct is the top-level orchestrator that ties together
//! gRPC server, lifecycle manager, package manager, and vault.

pub mod node_manager;
pub mod ownership;
pub mod state;

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::GatewayConfig;
use crate::cron::CronStore;
use crate::error::GatewayError;
use crate::gateway::state::GatewayState;
use crate::handlers::server::SharedState;
use crate::interaction_store::InteractionStore;

/// Gateway — the top-level orchestrator
///
/// Owns all sub-systems and drives the event loop.
///
/// `state` is a [`SharedState`] (`Arc<RwLock<GatewayState>>`) from the
/// very first moment after construction. Wrapping the state in a shared
/// handle at construction time (rather than after a `std::mem::take` in
/// `run()`) eliminates the entire family of "self.state is a stand-in,
/// writes go nowhere" bugs that arise when an owned field is converted
/// to a shared handle partway through initialisation.
pub struct Gateway {
    config: GatewayConfig,
    state: SharedState,
}

impl Gateway {
    /// Create a new Gateway instance with the given configuration
    ///
    /// Construction runs entirely on a single thread before any async
    /// runtime exists, so we first build an owned `GatewayState` and run
    /// all synchronous initialisation (`interaction_store` setup,
    /// `restore_installed_agents`) directly against it. We then wrap the
    /// fully-initialised state in a `SharedState`. From this point on,
    /// every code path sees the same shared handle — there is no
    /// `std::mem::take`/`mem::swap` dance and no "two views of the state".
    pub fn new(config: GatewayConfig) -> Result<Self, GatewayError> {
        config.validate()?;

        let vault_dir = config.vault_dir.clone();
        let data_dir = config.data_dir.clone();

        // Ensure data directory exists before opening the database
        std::fs::create_dir_all(&data_dir).map_err(|e| {
            GatewayError::Config(format!(
                "Failed to create data directory '{}': {}",
                data_dir, e
            ))
        })?;

        // Wire up the per-agent interaction store. Keys are agent_id, so the
        // timestamps survive agent stop/restart. Loaded eagerly so the
        // /api/agents sort order is correct from the first request.
        let interaction_store = InteractionStore::new(std::path::Path::new(&data_dir));
        let mut state = GatewayState::new(&vault_dir);
        state.interaction_store = Some(interaction_store.clone());
        state.last_interactions = interaction_store.load();

        // ADR-059 §5.1: assign a fresh instance id at every Gateway
        // process start. This is the single value consumers use to
        // distinguish the current Gateway from a retained snapshot
        // emitted by a previous one. NOT persisted across restarts:
        // "重启后换发" is the entire point.
        state.set_instance_id(uuid::Uuid::new_v4().to_string());

        // ADR-055 §6.5: installed agents are no longer restored by an
        // on-disk packages scan (L2-9). The local node publishes its
        // installed-package inventory (retained InstalledAgentInfo) after
        // it starts; the Gateway aggregates those into `installed_agents`
        // via the MQTT dispatch path.

        // ADR-076 §决策 12: resolve the deployment auth mode. Under
        // `multi_user` the account system is built here. A missing
        // `bootstrap_admin` on an empty account store is *not* fatal any
        // more (v2): a passwordless `admin` is seeded and the Gateway boots
        // into restricted mode, so the Desktop can see `requires_setup`
        // instead of a dead port.
        let auth_mode = config.effective_auth_mode();
        // ADR-076 §决策 12: an explicit mode that disagrees with the bind
        // inference is allowed (explicit always wins) but suspicious — warn
        // so a `--auth-mode local` behind a `0.0.0.0` bind is never silent.
        if config.auth_mode.is_some() {
            let inferred = crate::auth::resolve_auth_mode(None, None, &config.http.host);
            if inferred != auth_mode {
                tracing::warn!(
                    explicit = %auth_mode,
                    inferred = %inferred,
                    bind_host = %config.http.host,
                    "explicit auth_mode overrides the bind-address inference (ADR-076 §决策 12)"
                );
            }
        }
        // ADR-084: the account store is *not* opened here any more. It
        // belongs to the `acowork-user` process, which the supervisor
        // starts below and whose Ed25519 public key the Gateway uses to
        // verify access tokens. Opening it here as well would make the
        // Gateway a second writer of `accounts.json` — the exact coupling
        // the ADR removes.
        if auth_mode.is_multi_user() {
            tracing::info!("AUTH_MODE=multi_user: account system delegated to acowork-user");
        } else {
            tracing::info!("AUTH_MODE=local: single-user mode (account system disabled)");
        }

        let gateway = Self {
            config,
            state: Arc::new(RwLock::new(state)),
        };

        Ok(gateway)
    }

    /// Whether the account system is in first-boot restricted mode: a
    /// passwordless admin account exists. ADR-076 §决策 12 v2.
    ///
    /// ADR-084: the Gateway no longer inspects the account store. It asks
    /// the owner — `acowork-user admin-setup --check` — which reports the
    /// *same* predicate its `/health` publishes as `requires_setup`, so the
    /// boot-time prompt cannot disagree with the restricted-mode gate
    /// applied minutes later. Returns `false` in `local` mode.
    pub fn is_first_boot_restricted(&self) -> bool {
        if !self.config.effective_auth_mode().is_multi_user() {
            return false;
        }
        crate::cli::user_setup_required(self.config.user.data_dir.as_deref()).unwrap_or(false)
    }

    /// Apply the first-boot admin password — used by the daemon's TTY
    /// prompt path (ADR-076 §决策 12 v2). Errors if the account system is
    /// not running (`local` mode) or the admin already has a password.
    pub fn set_admin_password(&self, password: &str) -> Result<(), String> {
        if !self.config.effective_auth_mode().is_multi_user() {
            return Err("admin-setup is unavailable in local mode".to_string());
        }
        crate::cli::user_admin_setup(self.config.user.data_dir.as_deref(), password)
    }

    fn cleanup_orphaned_runtimes(&self) -> usize {
        // ADR-033: gRPC endpoint no longer passed to Runtime. Use MQTT port
        // as the unique cmdline marker tying a runtime to this Gateway.
        let mqtt_marker = self
            .config
            .mqtt
            .enabled
            .then(|| format!("--mqtt-port {}", self.config.mqtt.port));

        // Find all acowork-runtime processes (full argv via `ps` — the
        // `pgrep -af` variant prints bare PIDs on macOS and silently
        // defeats every marker match).
        let procs = crate::gateway::node_manager::find_procs_by_cmdline("acowork-runtime");
        let my_pid = std::process::id();

        // Filter PIDs whose command line matches our MQTT marker (if any).
        let pids_to_kill: Vec<(u32, String)> = procs
            .into_iter()
            .filter_map(|(pid, cmdline)| {
                if pid == my_pid {
                    return None; // don't kill self
                }
                match &mqtt_marker {
                    Some(marker) if cmdline.contains(marker.as_str()) => Some((pid, cmdline)),
                    _ => None,
                }
            })
            .collect();

        if pids_to_kill.is_empty() {
            return 0;
        }

        tracing::info!(
            count = pids_to_kill.len(),
            "Found {} orphaned runtime process(es) for this Gateway, cleaning up",
            pids_to_kill.len()
        );

        for (pid, _cmdline) in &pids_to_kill {
            // Try graceful kill first (SIGTERM)
            match std::process::Command::new("kill")
                .args(["-15", &pid.to_string()])
                .output()
            {
                Ok(_) => tracing::info!("Sent SIGTERM to orphaned runtime (PID {})", pid),
                Err(e) => tracing::warn!("Failed to kill orphaned runtime (PID {}): {}", pid, e),
            }
        }

        pids_to_kill.len()
    }

    /// Run the Gateway daemon (async, multi-connection)
    ///
    /// This starts the HTTP server, MQTT broker, embed/LSP supervisors and
    /// the main event loop. Blocks until shutdown signal is received.
    ///
    /// `self.state` is already a [`SharedState`] at construction time, so
    /// there is no `std::mem::take`/`mem::swap` dance here — every
    /// mutation goes through the shared lock and every reader sees the
    /// same data.
    pub async fn run(
        &mut self,
        log_reload_handle: Option<crate::LogReloadHandle>,
    ) -> Result<(), GatewayError> {
        tracing::info!("Gateway starting");
        tracing::info!("  Vault dir: {}", self.config.vault_dir);
        tracing::info!("  Packages dir: {}", self.config.packages_dir);

        // Ensure directories exist
        self.ensure_dirs()?;

        // ADR-055 §6.5: installed agents are no longer restored by an
        // on-disk packages scan (L2-9). The local node publishes its
        // retained installed inventory after it starts; the Gateway
        // aggregates those via the MQTT dispatch path.

        // Clean up orphaned runtime processes from a previous Gateway run.
        let orphan_count = self.cleanup_orphaned_runtimes();
        if orphan_count > 0 {
            tracing::info!(count = orphan_count, "Cleaned up orphan runtime processes");
        }

        // ADR-009 §V-O: bundled agents are no longer copied to disk here.
        // The Gateway cannot create files in the node's packages dir, and at
        // this point the local node is not even up yet — the bundled install
        // is dispatched from the System-Agent bootstrap task below, once the
        // node's control plane is ready.

        // `self.state` is already a SharedState. Clone the handle so the
        // rest of `run()` can move it into long-lived tasks (reapers,
        // supervisors, HTTP server) while `&mut self` continues to be
        // available for the occasional self-state mutation.
        let shared_state: SharedState = self.state.clone();

        // ADR-059 Phase 1.2: build the bootstrap readiness picture
        // before anything else can observe Gateway state. Subsystems
        // register themselves against this registry as they come
        // online; the orchestrator aggregates their state into a
        // BootstrapState snapshot that the MQTT publisher (Phase 1.1)
        // and the HTTP `/api/bootstrap` handler (Phase 1.3) expose.
        //
        // Subsystem IDs are intentionally stable strings (not enum
        // variants) so the registry accepts arbitrary subsystems
        // without changing its API — see ADR-059 §5.4 OCP boundary.
        let bootstrap_registry = crate::bootstrap::SubsystemReadinessRegistry::new_shared();
        // Required subsystems registered statically here:
        //   * vault   - unlocked by the HTTP vault handler / dev-mode
        //               auto-unlock via unlock_vault_and_mark_ready
        //   * mqtt    - marked ready once the Gateway MQTT client
        //               connects (below)
        //   * publisher - marked ready when the global-resources
        //               publisher task is started (below)
        // Optional subsystems registered dynamically by mqtt/dispatch.rs:
        //   * node.NODE_ID - one Optional entry per connected node,
        //               raised by the NodeReady topic handler (covers
        //               both the local node and any remote nodes). A
        //               node (local or remote) going offline demotes the
        //               entry but must NOT block the aggregated READY
        //               phase — remote nodes are pluggable resources,
        //               not platform-readiness blockers (see dispatch.rs
        //               and the per-node `is_ready` control gate).
        // Optional:
        //   * embedding - fall back to a remote embedder if missing
        let vault_handle =
            bootstrap_registry.register("vault", crate::bootstrap::ReadinessKind::Required);
        let mqtt_handle =
            bootstrap_registry.register("mqtt", crate::bootstrap::ReadinessKind::Required);
        let publisher_handle =
            bootstrap_registry.register("publisher", crate::bootstrap::ReadinessKind::Required);
        let embed_handle =
            bootstrap_registry.register("embedding", crate::bootstrap::ReadinessKind::Optional);

        // Construct the orchestrator with the same instance_id as the
        // Gateway so the snapshot's `instance_id` field stays consistent
        // across the Gateway's lifetime.
        let instance_id = shared_state.read().await.instance_id.clone();
        let bootstrap_orchestrator =
            crate::bootstrap::BootstrapOrchestrator::new(instance_id, bootstrap_registry.clone());
        shared_state.write().await.bootstrap.orchestrator = Some(bootstrap_orchestrator.clone());
        // ADR-059 Phase 5.4: stash the vault handle on the shared
        // state so the HTTP vault lock/unlock handlers can demote
        // (mark_booting) and restore (mark_ready) vault readiness
        // without holding a registry reference.
        shared_state.write().await.bootstrap.vault_readiness_handle = Some(vault_handle.clone());

        // System Agent auto-start now happens AFTER the local node is up
        // and its installed inventory has been aggregated (see below).

        // ADR-055 D3: resolve the advertise host once, before the embed
        // sidecar is spawned — the embed bind decision (below) and the
        // GatewayState cache (further down) both consume this single
        // value (config > auto-detected non-loopback IP > 127.0.0.1).
        let resolved_advertise_host = crate::config::resolve_advertise_host(&self.config);

        // Try to spawn the local embedding service (acowork-embed).
        // This is optional — if the binary is not found, embedding will
        // fall back to remote providers (Ollama / OpenAI-compatible API).
        // The embed process state is stored in GatewayState for the
        // HTTP embedding API to reference.
        let mut embed_child = None;
        let mut embed_supervisor_cfg: Option<
            crate::lifecycle::embed_supervisor::EmbedSupervisorConfig,
        > = None;
        {
            let data_dir = std::path::PathBuf::from(&self.config.data_dir);
            let models_dir = data_dir.join("models");
            let embed_port = 18080; // Default port for embedding service
            let hf_mirrors = self.config.hf_mirrors.clone();
            let embedding_model = self.config.embedding_model.clone();
            // "fp32" must be a key present in BOTH `onnx_variants` and
            // `external_data_files` of the model registry: the variant
            // drives the ONNX file path AND the external-data lookup
            // (`onnx/model.onnx` + `onnx/model.onnx_data`). The previous
            // value "onnx" matched neither map, so downloads silently
            // skipped `model.onnx_data` and every model load failed.
            let onnx_variant = "fp32";
            // ADR-055 D3: bind the embed listener at the advertised address
            // so Runtimes (local or remote) can dial the published endpoint
            // — see `bind_host_for_advertise` for the loopback rule.
            let embed_bind_host =
                crate::lifecycle::embed::bind_host_for_advertise(&resolved_advertise_host);
            let existing_health = crate::lifecycle::embed::check_embed_health(embed_port).await;
            if existing_health.is_some() {
                let embed_state = crate::lifecycle::embed::attach_existing_embed_process(
                    embed_port,
                    existing_health,
                );
                tracing::info!(
                    port = embed_state.port,
                    ready = embed_state.ready,
                    "Reusing existing embedding service"
                );
                self.state.write().await.embed_process = Some(embed_state);
                // ADR-059 Phase 1.2: a healthy pre-existing embed counts
                // as ready for the bootstrap aggregation (optional
                // subsystem — it never blocks READY, only demotes to
                // DEGRADED if it later fails).
                embed_handle.mark_ready(Some("existing embed process attached".to_string()));
                embed_supervisor_cfg =
                    Some(crate::lifecycle::embed_supervisor::EmbedSupervisorConfig {
                        data_dir,
                        models_dir,
                        port: embed_port,
                        bind_host: embed_bind_host.to_string(),
                        hf_mirrors,
                        onnx_variant: onnx_variant.to_string(),
                        model_id: embedding_model.clone(),
                    });
            } else {
                match crate::lifecycle::embed::spawn_embed_process(
                    &data_dir,
                    &models_dir,
                    embed_port,
                    embed_bind_host,
                    &hf_mirrors,
                    onnx_variant,
                    embedding_model.as_deref(),
                )
                .await
                {
                    Ok((embed_state, child)) => {
                        tracing::info!(
                            pid = embed_state.pid,
                            port = embed_state.port,
                            "Embedding service process spawned"
                        );
                        self.state.write().await.embed_process = Some(embed_state);
                        embed_child = Some(child);
                        // ADR-059 Phase 1.2: the embed process is up
                        // (process-level readiness; model loading is
                        // tracked by the supervisor, not the bootstrap
                        // registry).
                        embed_handle.mark_ready(Some("embed process spawned".to_string()));
                        embed_supervisor_cfg =
                            Some(crate::lifecycle::embed_supervisor::EmbedSupervisorConfig {
                                data_dir,
                                models_dir,
                                port: embed_port,
                                bind_host: embed_bind_host.to_string(),
                                hf_mirrors,
                                onnx_variant: onnx_variant.to_string(),
                                model_id: embedding_model.clone(),
                            });
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "Failed to spawn embedding service (local ONNX embedding unavailable, will use remote fallback)"
                        );
                        // ADR-059 Phase 1.2: embed is optional and its
                        // absence is a designed-in state (remote
                        // fallback). Mark it skipped so the aggregated
                        // phase can reach READY without treating the
                        // missing binary as a failure.
                        embed_handle.mark_skipped(Some(
                            "local embed unavailable; remote fallback".to_string(),
                        ));
                    }
                }
            }
        }

        // Load resource cache (provider_list.json + mcp_list.json) into memory.
        // These files are rebuilt by HTTP handlers when resources change.
        // Writes go through the SharedState write lock so the very next
        // HTTP/MQTT request sees the loaded data — no more "loaded but
        // invisible" race where `self.state` is a stand-in.
        let cache_dir = std::path::PathBuf::from(&self.config.data_dir);
        let loaded_cache = crate::resource_cache::load_resource_cache(&cache_dir);
        shared_state.write().await.resource_cache = loaded_cache;

        // `shared_state` was constructed *earlier* in this function
        // (see `let shared_state: SharedState = self.state.clone();`)
        // so the System Agent's reaper can be wired up.

        // Spawn embed process reaper — clears state when the child exits.
        // This is the single source of truth for embed process lifecycle:
        // when the child process exits (normally or by crash), the shared
        // state is updated atomically. HTTP handlers see embed_process=None
        // on the very next request, with no defensive PID polling needed.
        //
        // The reaper is PID-aware: if the supervisor has already replaced
        // this child with a new embed (different PID), we leave the new
        // state alone.
        if let Some(mut child) = embed_child {
            // Capture the PID before moving child into the async block.
            // `child.id()` returns Option<u32>; if the child has already
            // been reaped (None) we skip the reaper entirely.
            let child_pid = child.id();
            let state_for_reaper = shared_state.clone();
            tokio::spawn(async move {
                let Some(target_pid) = child_pid else {
                    return;
                };
                let exit_status = child.wait().await;
                tracing::warn!(
                    pid = target_pid,
                    exit_status = ?exit_status,
                    "Embedding service process exited"
                );
                let mut gw = state_for_reaper.write().await;
                let still_ours = gw
                    .embed_process
                    .as_ref()
                    .map(|eps| eps.pid == target_pid)
                    .unwrap_or(false);
                if still_ours {
                    gw.embed_process = None;
                } else {
                    tracing::debug!(
                        old_pid = target_pid,
                        current_pid = ?gw.embed_process.as_ref().map(|e| e.pid),
                        "Embed reaper: state already replaced by supervisor; leaving alone"
                    );
                }
            });
        }

        // S3.2: Open CronStore and load persisted cron entries
        {
            let cron_db_path = std::path::Path::new(&self.config.data_dir).join("cron_entries.db");
            match CronStore::open(&cron_db_path) {
                Ok(store) => {
                    let mut gw = shared_state.write().await;
                    if let Err(e) = gw.cron_scheduler.load_from_store(&store) {
                        tracing::warn!("Failed to load cron entries: {}", e);
                    }
                    gw.cron_store = Some(std::sync::Arc::new(store));
                }
                Err(e) => {
                    tracing::warn!("Failed to open cron store: {}", e);
                }
            }
        }

        // D3 (tech-debt plan): open the Intent mailbox. Persistence is
        // required, not optional — the conversation request counts must
        // survive a Gateway restart (design doc 27 §6-13), and every
        // delivery must leave an observable fact.
        {
            let mailbox_path =
                std::path::Path::new(&self.config.data_dir).join("intent_mailbox.json");
            let mut gw = shared_state.write().await;
            gw.intent_mailbox = Some(std::sync::Arc::new(
                crate::intent::mailbox::IntentMailbox::open(&mailbox_path),
            ));
        }

        // P0-2 fix: Store config snapshot in GatewayState for Config API
        {
            let mut gw = shared_state.write().await;
            gw.config = Some(self.config.clone());
            // ADR-055 D3: cache the advertise host resolved at startup
            // (above) so every endpoint constructor (embed / LSP /
            // AgentHello) reads a single source of truth.
            let advertise = resolved_advertise_host;
            tracing::info!(advertise_host = %advertise, "Resolved advertise host (ADR-055 D3)");
            gw.advertise_host = advertise;
        }

        // Auto-sleep was retired in Sept 2026 — the Gateway no longer
        // owns any per-agent idle watcher; the only on/off transition
        // is Runtime start / stop driven through the MQTT status topic.

        tracing::info!("Gateway entering gRPC event loop (async multi-connection)");

        // Clone HTTP config before moving into the task
        let http_config = self.config.http.clone();
        let data_dir_path = std::path::PathBuf::from(&self.config.data_dir);

        // Internal-service HTTP credential (ADR-064 Phase 3). Generated before
        // any supervisor so `auth_middleware` can compare against it from the
        // first request, and stored in shared state so the token and the
        // comparison site can never drift apart. See
        // `GatewayState::internal_service_token` for why this is NOT the MQTT
        // `publisher_token`.
        let internal_service_token = crate::mqtt::enrollment::generate_token();
        {
            let mut gw = shared_state.write().await;
            gw.internal_service_token = Some(internal_service_token.clone());
        }

        // ADR-064: PM 已从 Gateway 解耦为独立进程 `acowork-pm`。
        //
        // Gateway 不再内嵌 PM（删除 `acowork-pm` 依赖），改为：
        //   1. `pm_supervisor` spawn 独立进程并轮询 `/health`（失败指数退避重启）；
        //   2. `/api/pm/*` 由 `http::pm_proxy` 反向代理到 `127.0.0.1:{pm_port}`。
        //
        // 失败**非致命**：PM 无法启动时 Gateway 继续运行，`/api/pm/*` 返回 503。
        // PM 数据目录独立为 `$HOME/.acowork/acowork-pm/`（与 gateway/node 平级）。
        if self.config.pm.enabled {
            let pm_bin = std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    exe.parent().map(|p| {
                        let bin_name = if cfg!(windows) {
                            "acowork-pm.exe"
                        } else {
                            "acowork-pm"
                        };
                        p.join(bin_name)
                    })
                })
                .unwrap_or_else(|| {
                    let bin_name = if cfg!(windows) {
                        "acowork-pm.exe"
                    } else {
                        "acowork-pm"
                    };
                    std::path::PathBuf::from(bin_name)
                });
            let supervisor_cfg = crate::lifecycle::pm_supervisor::PmSupervisorConfig {
                pm_bin,
                port: self.config.pm.port,
                port_file: data_dir_path.join("pm.port"),
                log_dir: data_dir_path.join("logs"),
                gateway_health_url: format!("http://127.0.0.1:{}/health", http_config.port),
                // ADR-064 Phase 3: PM 经 Gateway HTTP 查询 `/api/agents`（AgentDirectory）。
                gateway_url: Some(format!("http://127.0.0.1:{}", http_config.port)),
                gateway_token: Some(internal_service_token.clone()),
            };
            crate::lifecycle::pm_supervisor::start_pm_supervisor(
                supervisor_cfg,
                shared_state.clone(),
            );
            tracing::info!(
                port = self.config.pm.port,
                enabled = true,
                "PM supervisor started (ADR-064)"
            );
        } else {
            tracing::info!("PM supervisor disabled (pm.enabled=false)");
        }

        // pm_mcp_url: MCP 端点经 Gateway 反代暴露（Desktop 走 /api/pm/mcp）。
        {
            let mut gw = shared_state.write().await;
            gw.pm_mcp_url = if self.config.pm.auto_inject_mcp {
                Some(format!(
                    "http://{}:{}{}",
                    gw.advertise_host.clone(),
                    http_config.port,
                    self.config.pm.mcp_http_path
                ))
            } else {
                None
            };
        }

        // ADR-084 §决策 4b: generated once and shared by the three
        // internal publishers — the user/doc supervisors (forwarded to
        // the services as `--mqtt-password`, so `user:service` /
        // `doc:service` pass the broker's CONNECT auth) and the
        // Gateway's own publisher (client_id `gateway:publisher`,
        // created after the broker starts). Generating it before the
        // supervisors start keeps a single source for all of them.
        let publisher_token = crate::mqtt::enrollment::generate_token();

        // acowork-doc: standalone process `acowork-doc`, lifecycle managed by
        // the Gateway supervisor (spawn + /health poll + exponential-backoff
        // restart); `/api/doc/*` is reverse-proxied by `http::doc_proxy` to
        // `127.0.0.1:{doc_port}`.
        //
        // Failure is **non-fatal**: when doc cannot start the Gateway keeps
        // running and `/api/doc/*` returns 503. The data directory defaults
        // to `$HOME/.acowork/acowork-doc/`; `[doc].data_dir` and
        // `[doc].request_ttl_hours` are optional overrides forwarded via CLI.
        if self.config.doc.enabled {
            let doc_bin = std::env::current_exe()
                .ok()
                .and_then(|exe| {
                    exe.parent().map(|p| {
                        let bin_name = if cfg!(windows) {
                            "acowork-doc.exe"
                        } else {
                            "acowork-doc"
                        };
                        p.join(bin_name)
                    })
                })
                .unwrap_or_else(|| {
                    let bin_name = if cfg!(windows) {
                        "acowork-doc.exe"
                    } else {
                        "acowork-doc"
                    };
                    std::path::PathBuf::from(bin_name)
                });
            let supervisor_cfg = crate::lifecycle::doc_supervisor::DocSupervisorConfig {
                doc_bin,
                port: self.config.doc.port,
                port_file: data_dir_path.join("doc.port"),
                log_dir: data_dir_path.join("logs"),
                gateway_health_url: format!("http://127.0.0.1:{}/health", http_config.port),
                data_dir: self.config.doc.data_dir.clone(),
                request_ttl_hours: self.config.doc.request_ttl_hours,
                mqtt_port: self.config.mqtt.port,
                // ADR-084 §决策 4b: with broker auth on, the service
                // authenticates as `doc:service` with the shared
                // internal publisher token.
                mqtt_password: self.config.mqtt.auth_enabled.then(|| publisher_token.clone()),
            };
            crate::lifecycle::doc_supervisor::start_doc_supervisor(
                supervisor_cfg,
                shared_state.clone(),
            );
            tracing::info!(
                port = self.config.doc.port,
                enabled = true,
                "doc supervisor started"
            );
        } else {
            tracing::info!("doc supervisor disabled (doc.enabled=false)");
        }

        // acowork-user: standalone process `acowork-user`, lifecycle managed
        // by the Gateway supervisor (spawn + `/health` poll + exponential-
        // backoff restart). The whole user domain — `/api/auth/*`,
        // `/api/users/*`, `/api/user/*` — is reverse-proxied to it by
        // `http::user_proxy`, and its Ed25519 public key is what the Gateway
        // verifies access tokens with (ADR-084 §决策 1/2/5).
        //
        // Non-fatal: if the service cannot start, the Gateway keeps running
        // and those routes return 503 + `Retry-After` rather than 500.
        if self.config.user.enabled {
            let user_bin = crate::lifecycle::process::sibling_binary("acowork-user");
            let supervisor_cfg = crate::lifecycle::user_supervisor::UserSupervisorConfig {
                user_bin,
                port: self.config.user.port,
                port_file: data_dir_path.join("user.port"),
                log_dir: data_dir_path.join("logs"),
                gateway_health_url: format!("http://127.0.0.1:{}/health", http_config.port),
                data_dir: self.config.user.data_dir.clone(),
                config: self.config.user.config.clone(),
                auth_mode: self.config.effective_auth_mode().as_str(),
                mqtt_port: self.config.mqtt.port,
                // ADR-084 §决策 4b: with broker auth on, the service
                // authenticates as `user:service` with the shared
                // internal publisher token.
                mqtt_password: self.config.mqtt.auth_enabled.then(|| publisher_token.clone()),
            };
            crate::lifecycle::user_supervisor::start_user_supervisor(
                supervisor_cfg,
                shared_state.clone(),
            );
            tracing::info!(
                port = self.config.user.port,
                enabled = true,
                "user supervisor started (ADR-084)"
            );
        } else {
            tracing::info!("user supervisor disabled (user.enabled=false)");
        }

        // acowork-memory: standalone process `acowork-memory`, lifecycle managed
        // by the Gateway supervisor (spawn + `/health` poll + exponential-
        // backoff restart). Memory left the Runtime with ADR-101, so this is the
        // only place that decides whether an Agent has a memory at all:
        // `memory.enabled=false` means no process, no `memory` MCP server in any
        // catalog, and `/api/memory/*` answers 503.
        //
        // Non-fatal like the others — a memory outage must not take the Gateway
        // or the agents down with it.
        if self.config.memory.enabled {
            let memory_bin = crate::lifecycle::process::sibling_binary("acowork-memory");
            let supervisor_cfg = crate::lifecycle::memory_supervisor::MemorySupervisorConfig {
                memory_bin,
                port: self.config.memory.port,
                port_file: data_dir_path.join("memory.port"),
                log_dir: data_dir_path.join("logs"),
                gateway_health_url: format!("http://127.0.0.1:{}/health", http_config.port),
                // The service pulls its distiller / embedding credentials over
                // HTTP (ADR-101 decision 7), so it needs the loopback API
                // address, not the advertise host.
                gateway_url: format!("http://127.0.0.1:{}", http_config.port),
                gateway_token: Some(internal_service_token.clone()),
                data_dir: self.config.memory.data_dir.clone(),
                config: self.config.memory.config.clone(),
            };
            crate::lifecycle::memory_supervisor::start_memory_supervisor(
                supervisor_cfg,
                shared_state.clone(),
            );
            tracing::info!(
                port = self.config.memory.port,
                enabled = true,
                "memory supervisor started (ADR-101)"
            );
        } else {
            tracing::info!("memory supervisor disabled (memory.enabled=false)");
        }

        // doc_mcp_url: MCP endpoint exposed via the Gateway reverse proxy
        // (Desktop goes through /api/doc/mcp).
        {
            let mut gw = shared_state.write().await;
            gw.doc_mcp_url = if self.config.doc.auto_inject_mcp {
                Some(format!(
                    "http://{}:{}{}",
                    gw.advertise_host.clone(),
                    http_config.port,
                    self.config.doc.mcp_http_path
                ))
            } else {
                None
            };
        }

        // memory_mcp_url: same pattern (ADR-101). Written unconditionally so a
        // stale value from a previous branch can never survive: `Some` only when
        // the service is enabled *and* injection is on.
        {
            let mut gw = shared_state.write().await;
            gw.memory_mcp_url = if self.config.memory.enabled && self.config.memory.auto_inject_mcp {
                Some(format!(
                    "http://{}:{}{}",
                    gw.advertise_host.clone(),
                    http_config.port,
                    self.config.memory.mcp_http_path
                ))
            } else {
                None
            };
        }

        // Rebuild resource cache from MCP catalog at startup.
        // provider_list.json is loaded by load_resource_cache() above;
        // it is the source of truth for provider config. No rebuild needed.
        {
            let mut gw = shared_state.write().await;
            if let Ok(catalog) = crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir_path) {
                crate::resource_cache::rebuild_and_save_mcp_cache(
                    &mut gw,
                    &data_dir_path,
                    &catalog,
                );
            }
        }

        // S3.1: Load cron scheduler entries from store
        let cron_scheduler = Arc::new(tokio::sync::Mutex::new({
            let gw = shared_state.read().await;
            std::mem::take(&mut gw.cron_scheduler.clone())
        }));
        // Sync back loaded entries into the shared scheduler
        {
            let mut gw = shared_state.write().await;
            gw.cron_scheduler = {
                let sched = cron_scheduler.lock().await;
                sched.clone()
            };
        }

        // Start HTTP server in a separate tokio task (parallel with gRPC)
        let http_state = shared_state.clone();

        // Design doc 24 §8.2: relay tunnel client. Always constructed
        // (no tasks until enabled); the HTTP layer gets the handle for
        // /api/relay/*, and the supervisor auto-starts below when
        // [relay] enabled=true in the persisted config.
        let relay_client = crate::relay::RelayClient::new(
            crate::config::GatewayConfig::project_config_dir().join("relay_identity.json"),
            std::net::SocketAddr::from(([127, 0, 0, 1], self.config.relay.remote_http_port)),
        );
        // The HTTP-server task captures its own Arc clone; the original
        // stays with this scope for the boot auto-enable below.
        let http_relay_client = relay_client.clone();

        // Start the embed supervisor. It watches the embed's SSE event
        // stream, updates `shared_state.embed_process.{active_model_id,
        // active_dimension, ready}` from the embed's state events, and
        // restarts the embed process on heartbeat timeout or connection
        // loss (with exponential backoff and a 5-attempts/5-min cap).
        if let (Some(sup_cfg), Some(shared_arc)) =
            (embed_supervisor_cfg.take(), Some(shared_state.clone()))
        {
            crate::lifecycle::embed_supervisor::start_embed_supervisor(sup_cfg, shared_arc);
        }

        // ADR-055 §6.7 (Phase 4): the LSP relay is NO LONGER hosted
        // here — each Node hosts its own node-local relay and publishes
        // the retained `acowork/nodes/{node_id}/lsps` topic. The Gateway
        // subscribes to that topic (node_registry) and serves
        // `GET /api/agents/{id}/lsp-endpoint` from it.

        // ADR-033: Start MQTT broker + Gateway client BEFORE HTTP server
        // so it's available for chat handlers to publish control commands.
        let mqtt_config = self.config.mqtt.clone();
        // ADR-055 Phase 5a: prepare the credential stores + internal
        // tokens BEFORE the broker starts so the CONNECT auth handler
        // installs with the full state. The HTTP token doubles as the
        // Desktop MQTT password, so it is generated whenever either
        // channel enables auth (the HTTP handler layer itself stays
        // permissive in this phase — the token is only consumed by the
        // broker CONNECT handler and /api/status).
        let http_auth = Arc::new(crate::http::auth::HttpAuth::new(
            http_config.auth_enabled || mqtt_config.auth_enabled,
        ));
        if let Err(e) = http_auth.write_token_file(&data_dir_path) {
            tracing::warn!(error = %e, "Failed to write http_token file");
        }
        let enrollment_tokens = crate::mqtt::new_shared_enrollment_store(&data_dir_path);
        let node_tokens = crate::mqtt::new_shared_node_token_store(&data_dir_path);
        // ADR-087: ownership policy tables (same directory + atomic-write
        // pattern as the token stores). Shared between the HTTP layer
        // (permission middleware, install/uninstall handlers, list
        // rendering) and the MQTT dispatch (enroll binds the node owner,
        // retained-inventory clear prunes agent rows).
        let node_owners = crate::gateway::ownership::new_shared_node_owners(&data_dir_path);
        let agent_owners = crate::gateway::ownership::new_shared_agent_owners(&data_dir_path);
        // ADR-087 D7 / review M4: staged installs (memory-only ledger,
        // same Arc shared between HTTP staging and MQTT commit/drop).
        let pending_installs = crate::gateway::ownership::new_shared_pending_installs();
        // ADR-087: default owner — the earliest-created admin account,
        // resolved via the owning process (ADR-084: the Gateway never
        // opens `accounts.json` itself). `None` in local mode (ownerless
        // is the correct semantics there — the machine is the owner) and
        // on a first boot before any admin exists.
        let default_owner: Option<String> = if mqtt_config.auth_enabled {
            crate::cli::user_first_admin(self.config.user.data_dir.as_deref())
        } else {
            None
        };
        {
            // One-shot adoption, marker-gated: resources created while no
            // admin was resolvable (first-boot local node, CLI installs)
            // and pre-ADR-087 rows (the local→multi_user migration) are
            // attributed to the admin exactly once. The marker keeps a
            // deliberately released row (owner=None via PATCH) from being
            // silently re-adopted on the next restart.
            let marker = data_dir_path.join("owner_adoption.done");
            if mqtt_config.auth_enabled
                && !marker.exists()
                && let Some(admin) = default_owner.as_deref()
            {
                let adopted =
                    crate::gateway::ownership::adopt_ownerless(&node_owners, &agent_owners, admin);
                if adopted > 0 {
                    tracing::info!(
                        adopted,
                        admin_user = %admin,
                        "ADR-087: adopted ownerless node/agent rows into the admin account"
                    );
                }
                // Write the marker even for a zero-row adoption: the
                // point is that adoption ran, not that it found work.
                let _ = std::fs::write(&marker, admin.as_bytes());
            }
            // Surface anything still ownerless (adoption skipped because
            // no admin existed yet) so the operator knows the claim
            // endpoints are the manual fallback.
            if mqtt_config.auth_enabled {
                let (ownerless_nodes, ownerless_agents) =
                    crate::gateway::ownership::count_ownerless(&node_owners, &agent_owners);
                if ownerless_nodes + ownerless_agents > 0 {
                    tracing::warn!(
                        ownerless_nodes,
                        ownerless_agents,
                        "ownerless resources remain — claim via POST /api/nodes/{{id}}/claim \
                         or POST /api/agents/{{id}}/claim (admin: any resource; other users: \
                         the local node and the agents on it)"
                    );
                }
            }
        }
        // `publisher_token` was generated before the user/doc supervisors
        // started — they already forwarded it to their services
        // (ADR-084 §决策 4b).
        let broker_auth = crate::mqtt::broker::BrokerAuth {
            auth_enabled: mqtt_config.auth_enabled,
            enrollment_tokens: enrollment_tokens.clone(),
            node_tokens: node_tokens.clone(),
            publisher_token: publisher_token.clone(),
            http_token: http_auth.token().map(str::to_string),
        };

        // security backstop: peer-IP allowlist. Decided once at boot —
        // may redirect the broker to loopback and enable a TCP
        // pre-filter that enforces the list on non-loopback binds.
        let mqtt_allowlist = self.config.security.to_allowlist();
        let mqtt_filter = if mqtt_config.enabled
            && crate::mqtt::tcp_filter::needs_mqtt_tcp_filter(&mqtt_config.host, &mqtt_allowlist)
        {
            // Broker binds loopback; the pre-filter owns the configured
            // external address and gates peers by IP before splicing.
            let target_port = mqtt_config.port;
            match crate::mqtt::tcp_filter::start_mqtt_tcp_filter(
                &mqtt_config.host,
                mqtt_config.port,
                "127.0.0.1",
                target_port,
                mqtt_allowlist.clone(),
            )
            .await
            {
                Ok(handle) => Some(handle),
                Err(e) => {
                    tracing::error!(%e, "MQTT TCP pre-filter failed to start");
                    None
                }
            }
        } else {
            None
        };

        let mut mqtt_broker_handle: Option<crate::mqtt::MqttBrokerHandle> = if mqtt_config.enabled {
            // ADR-033: start_broker runs in a separate OS thread
            // because rumqttd creates its own tokio runtime internally.
            let auth = if mqtt_config.auth_enabled {
                Some(broker_auth.clone())
            } else {
                None
            };
            // Design doc 24 §7.2 (M3): the strict remote MQTT listener —
            // always hosted alongside the main server (loopback-only;
            // the only paths to it are the `/mqtt` WebSocket bridges on the
            // remote HTTP listener and on the MAIN LAN listener, which is
            // how a node-local mobile WebView reaches the broker). Its auth
            // handler re-reads the user verifier from GatewayState per
            // CONNECT, so it follows the user-service lifecycle without any
            // wiring here.
            let remote_mqtt = crate::mqtt::RemoteMqttListener {
                host: "127.0.0.1".to_string(),
                port: self.config.relay.remote_mqtt_port,
                auth: crate::mqtt::RemoteMqttAuth {
                    gateway_state: shared_state.clone(),
                },
            };
            // When the pre-filter is active the broker must bind loopback
            // (the pre-filter already owns the external address). Only the
            // pre-filter's absence lets the broker bind the configured host.
            let broker_host = if mqtt_filter.is_some() {
                "127.0.0.1"
            } else {
                &mqtt_config.host
            };
            match crate::mqtt::start_broker_with_auth(
                broker_host,
                mqtt_config.port,
                auth,
                Some(remote_mqtt),
            ) {
                Ok(h) => {
                    if mqtt_filter.is_some() {
                        tracing::info!(addr = %h.listen_addr, "MQTT broker started (loopback, behind TCP pre-filter)");
                    } else {
                        tracing::info!(addr = %h.listen_addr, "MQTT broker started");
                    }
                    Some(h)
                }
                Err(e) => {
                    tracing::error!(%e, "MQTT broker failed");
                    None
                }
            }
        } else {
            None
        };

        // ADR-XXX Debug: Share broker handle with HTTP debug endpoints so they can
        // trigger graceful shutdown for connection-recovery tests.
        if let Some(h) = mqtt_broker_handle.take() {
            {
                let gw = shared_state.write().await;
                let mut ctrl = gw.mqtt_broker_control.lock().await;
                *ctrl = Some(h);
            }
            tracing::info!("MQTT broker handle registered with debug control");
        }
        // ADR-055 Phase 5a: share the auth state so a debug-triggered
        // broker restart keeps the same credential model.
        {
            let mut gw = shared_state.write().await;
            gw.mqtt_broker_auth = Some(broker_auth);
        }

        // ADR-033: Create runtime HTTP registry and agent registry.
        let runtime_http_registry = crate::http::proxy::new_shared_registry();
        let agent_registry = crate::mqtt::agent_registry::new_shared_registry();
        // Clone for the reconcile loop. The clone is cheap (`Arc`
        // bump) and gives the loop its own handle so it can run on a
        // dedicated task without contending with the dispatch path.
        let agent_registry_for_reconcile_opt = Some(agent_registry.clone());
        // ADR-055: node registry — the Gateway's view of Node Agents
        // (LWT-driven online state + retained info snapshots).
        let node_registry = crate::mqtt::node_registry::new_shared_registry();

        // ADR-087 D7: orphan audit. Ownership rows whose resource never
        // materialized (uninstall raced the row removal, CLI-enrolled
        // node deprovisioned out of band) are WARNed once per boot —
        // kept, never auto-deleted. Delayed so the retained inventory /
        // node-info replay has landed before "not seen" is judged.
        {
            let shared_state = shared_state.clone();
            let node_registry = node_registry.clone();
            let node_owners = node_owners.clone();
            let agent_owners = agent_owners.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let max_age = chrono::Duration::days(30);
                {
                    let gw = shared_state.read().await;
                    let store = ownership::lock(&agent_owners);
                    store.audit_orphans("agent", |id| gw.installed_agents.contains_key(id), max_age);
                }
                {
                    let reg = node_registry.read().await;
                    let store = ownership::lock(&node_owners);
                    store.audit_orphans("node", |id| reg.get(id).is_some(), max_age);
                }
            });
        }

        // Track whether the broker actually started — used to gate the
        // Gateway-side publisher without needing to re-check the broker
        // handle (which has been moved into the debug control slot above).
        let mqtt_broker_started = {
            let ctrl = shared_state.read().await;
            let guard = ctrl.mqtt_broker_control.lock().await;
            guard.is_some()
        };

        // ADR-055 §6.2: node control client — created after the MQTT
        // client exists; shared between dispatch (NodeEvent correlation)
        // and the HTTP handlers (command issue) via this slot.
        let node_control_slot: std::sync::Arc<
            tokio::sync::Mutex<Option<crate::mqtt::node_control::NodeControlClient>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        // ADR-059 §6.2: the operation store is shared between the HTTP
        // handlers (open records) and the MQTT dispatch layer (transition
        // them on NodeEvent correlation). Created here so the MQTT
        // callback registered below can capture it before the HTTP
        // server starts.
        let operation_store_shared = crate::operation_store::OperationStore::new_shared();

        // Inventory trigger slot — populated by the notifier startup
        // below. The dispatcher dereferences it on every inventory
        // mutation. `None` until the notifier starts (only matters
        // during the brief window before the MQTT broker accepts
        // connections, when no inventory messages can flow anyway).
        let inventory_trigger_slot: std::sync::Arc<
            tokio::sync::Mutex<Option<crate::mqtt::InventoryNotifierTrigger>>,
        > = std::sync::Arc::new(tokio::sync::Mutex::new(None));

        let mqtt_gw_client: Option<Arc<crate::mqtt::GatewayMqttClient>> = if mqtt_broker_started {
            let reg_for_dispatch = runtime_http_registry.clone();
            let agent_reg_for_dispatch = agent_registry.clone();
            let node_reg_for_dispatch = node_registry.clone();
            // Pass the client into dispatch so the status re-publish
            // path (plain text → protobuf DataEnvelope) has a way to
            // call `publish_envelope`. Note: the client is created
            // AFTER this callback registration, so we capture an
            // `Option<Arc<…>>` that the callback re-borrows each time
            // via the closure below. Easier: re-use the *same* client
            // — we already have `mqtt_gw_client` in scope, but the
            // callback needs to own its clone. Build the callback with
            // a one-shot placeholder, then wire it after `client`
            // exists via the message_callback's set-after-connect
            // pattern. For now, capture the *eventual* client via
            // `Option<Arc<…>>` by deferring construction:
            let dispatch_client: Option<Arc<crate::mqtt::GatewayMqttClient>> = None;
            let dispatch_client_slot: std::sync::Arc<
                tokio::sync::Mutex<Option<Arc<crate::mqtt::GatewayMqttClient>>>,
            > = std::sync::Arc::new(tokio::sync::Mutex::new(dispatch_client));
            let slot_for_cb = dispatch_client_slot.clone();
            let node_control_slot_for_cb = node_control_slot.clone();
            let state_for_dispatch = shared_state.clone();
            // ADR-055 Phase 5a: enroll dispatch needs the credential
            // stores + the auth flag. Cloned BEFORE the move-closure so
            // the originals stay available (local-node token pre-issue
            // below).
            let enrollment_tokens_for_dispatch = enrollment_tokens.clone();
            let node_tokens_for_dispatch = node_tokens.clone();
            // ADR-087: ownership stores — clone before the move-closure;
            // the originals stay available for the HTTP server below.
            let node_owners_for_dispatch = node_owners.clone();
            let agent_owners_for_dispatch = agent_owners.clone();
            let pending_installs_for_dispatch = pending_installs.clone();
            let auth_enabled_for_dispatch = mqtt_config.auth_enabled;
            // ADR-087: default owner for the enroll/inventory convergence
            // points — cloned per message like the stores above.
            let default_owner_for_dispatch = default_owner.clone();
            // ADR-059 §7.2: NodeReady dispatch drives `node.{id}`
            // readiness on the bootstrap registry.
            let bootstrap_registry_for_dispatch = bootstrap_registry.clone();
            // Clone before the move-closure; the original stays
            // available for the HTTP server below.
            let operation_store_for_dispatch = operation_store_shared.clone();
            // ADR-059 §7.2 replay guard: shared across every dispatch
            // message (a per-message guard would never see the live
            // online signal that the stale replay check needs).
            let node_replay_guard = Arc::new(crate::mqtt::dispatch::NodeReplayGuard::default());
            // Clone before the move-closure: the callback is `move` and
            // would otherwise consume the original, which must stay
            // available for `set_replay_guard` after connect.
            let node_replay_guard_for_cb = node_replay_guard.clone();
            let inventory_trigger_for_cb = inventory_trigger_slot.clone();
            let callback: crate::mqtt::MqttMessageCallback = Arc::new(move |topic, payload| {
                // Plain-text dispatch (http_port, status, ready, …)
                let slot = slot_for_cb.clone();
                let node_control_slot = node_control_slot_for_cb.clone();
                let topic = topic.clone();
                let payload = payload.clone();
                let reg_for_dispatch = reg_for_dispatch.clone();
                let agent_reg_for_dispatch = agent_reg_for_dispatch.clone();
                let node_reg_for_dispatch = node_reg_for_dispatch.clone();
                let state_for_dispatch = state_for_dispatch.clone();
                let enrollment_tokens_for_cb = enrollment_tokens_for_dispatch.clone();
                let node_tokens_for_cb = node_tokens_for_dispatch.clone();
                let node_owners_for_cb = node_owners_for_dispatch.clone();
                let agent_owners_for_cb = agent_owners_for_dispatch.clone();
                let pending_installs_for_cb = pending_installs_for_dispatch.clone();
                let bootstrap_registry_for_cb = bootstrap_registry_for_dispatch.clone();
                let operation_store_for_cb = operation_store_for_dispatch.clone();
                let node_replay_guard_for_cb = node_replay_guard_for_cb.clone();
                let inventory_trigger_for_cb = inventory_trigger_for_cb.clone();
                let default_owner_for_cb = default_owner_for_dispatch.clone();
                tokio::spawn(async move {
                    let client = slot.lock().await.clone();
                    let node_control = node_control_slot.lock().await.clone();
                    let inventory_trigger = inventory_trigger_for_cb.lock().await.clone();
                    // ADR-059 follow-up: bundle every dispatch dependency
                    // into one context instead of a growing argument list.
                    // `client` / `node_control` / the token stores are moved
                    // in (single-use inside this task); registry Arcs are
                    // shallow-cloned.
                    let dispatch_ctx = crate::mqtt::dispatch::DispatchContext {
                        runtime_http_registry: reg_for_dispatch.clone(),
                        agent_registry: agent_reg_for_dispatch.clone(),
                        node_registry: node_reg_for_dispatch.clone(),
                        mqtt_client: client,
                        state: state_for_dispatch.clone(),
                        node_control,
                        enrollment_tokens: Some(enrollment_tokens_for_cb),
                        node_tokens: Some(node_tokens_for_cb),
                        node_owners: Some(node_owners_for_cb),
                        agent_owners: Some(agent_owners_for_cb),
                        pending_installs: Some(pending_installs_for_cb),
                        auth_enabled: auth_enabled_for_dispatch,
                        default_owner: default_owner_for_cb,
                        bootstrap_registry: Some(bootstrap_registry_for_cb),
                        operation_store: Some(operation_store_for_cb),
                        node_replay_guard: node_replay_guard_for_cb,
                        // Inventory trigger is cloned from the slot at
                        // dispatch time (see `inventory_trigger` above).
                        // The slot starts `None` and gets populated
                        // when the notifier starts below; a missed
                        // signal during that brief startup window is
                        // harmless — the inventory can't change before
                        // the broker is accepting messages anyway.
                        inventory_trigger,
                    };
                    crate::mqtt::dispatch::handle_message(&topic, &payload, &dispatch_ctx);
                });
            });

            // ADR-055 Phase 5a: when MQTT auth is enabled the broker
            // rejects credential-less connections — the publisher
            // presents the internal startup token (client_id as the
            // username; the CONNECT check keys on client_id + password).
            let publisher_result = if mqtt_config.auth_enabled {
                crate::mqtt::GatewayMqttClient::new_publisher_with_callback_and_credentials(
                    &mqtt_config.host,
                    mqtt_config.port,
                    callback,
                    acowork_core::defaults::GATEWAY_MQTT_PUBLISHER_CLIENT_ID,
                    &publisher_token,
                )
                .await
            } else {
                crate::mqtt::GatewayMqttClient::new_publisher_with_callback(
                    &mqtt_config.host,
                    mqtt_config.port,
                    callback,
                )
                .await
            };
            match publisher_result {
                Ok(c) => {
                    tracing::info!(
                        "MQTT Gateway client connected (persistent subscriptions handled by ConnAck handler)"
                    );
                    let client = Arc::new(c.clone());
                    // ADR-059 §7.2: attach the replay guard so the poll
                    // task stamps gateway (re)connections — dispatch uses
                    // the stamp to suppress stale retained replays after
                    // sleep/wake reconnects.
                    c.set_replay_guard(node_replay_guard.clone());
                    // Backfill the dispatch slot so the status re-publish
                    // path can call `publish_envelope`. Until now the
                    // slot was `None` and dispatch silently dropped
                    // re-publishes (still updating the in-process
                    // AgentRegistry, which is what the Gateway itself
                    // needs).
                    *dispatch_client_slot.lock().await = Some(client.clone());
                    // Backfill the node control client so dispatch can
                    // correlate NodeEvent results (ADR-055 §6.2).
                    *node_control_slot.lock().await = Some(
                        crate::mqtt::node_control::NodeControlClient::new(client.clone()),
                    );
                    // ADR-059 Phase 1.2: the MQTT broker + Gateway client
                    // are required subsystems; mark them ready as soon as
                    // the client connected. Phase transitions are
                    // idempotent, so re-entry is safe.
                    mqtt_handle.mark_ready(Some("client connected".to_string()));
                    Some(client)
                }
                Err(e) => {
                    tracing::warn!(%e, "MQTT Gateway client failed");
                    None
                }
            }
        } else {
            None
        };

        // ADR-033: Start MQTT Global Resources Publisher.
        // Publishes providers, models, MCP catalog, embedding models
        // to acowork/global/* Retained topics so Runtime can discover them.
        //
        // The publisher defers its first Retained publish until a "ready"
        // signal is raised — see `desktop-onboarding-bugfix_154b7ff7.md`
        // §Fix 1. The handle is stored on `shared_state` so that (a) the
        // dev_mode vault auto-unlock task and (b) the local-node
        // supervisor completion can independently call `handle.mark_ready()`
        // once their respective preconditions are satisfied.
        let mqtt_publisher_trigger: Option<crate::mqtt::MqttPublisherTrigger> =
            if let Some(ref client) = mqtt_gw_client {
                let publisher = crate::mqtt::MqttGlobalResourcesPublisher::new(
                    client.as_ref().clone(),
                    shared_state.clone(),
                );
                let handle = publisher.start();
                let trigger = handle.create_trigger();
                tracing::info!("MQTT Global Resources Publisher started");
                // Store the handle so the ready barrier can be raised by the
                // vault auto-unlock task and the local-node supervisor.
                shared_state.write().await.mqtt_publisher_handle = Some(handle);
                // ADR-059 Phase 1.2: the publisher is a required subsystem.
                // Marking it ready here is intentionally conservative: the
                // publisher's own deferred-publish loop only emits the FIRST
                // retained snapshot once `mark_ready(true)` is raised, so
                // the bootstrap snapshot can transition to READY before any
                // provider payload has been released.
                publisher_handle.mark_ready(Some("publisher started".to_string()));

                // ADR-080: spawn the advertise-host IP-change watchdog.
                // Subscribes to OS interface-change events (NotifyAddrChange
                // / netlink / SCDynamicStore via `if-watch`); on any change
                // it re-runs the UDP-trick IP detector and, if the LAN IP
                // drifted, refreshes `pm_mcp_url` / `doc_mcp_url` and
                // triggers a fresh retained `acowork/global/mcps` so every
                // Runtime rewrites its `agent_mcp.json`. Runs regardless of
                // whether `advertise_host` was pinned — a pin fixes only the
                // initial value, not future IP drift.
                let watchdog_cfg = crate::lifecycle::advertise_watchdog::AdvertiseWatchdogConfig {
                    http_port: http_config.port,
                    pm_mcp_path: if self.config.pm.auto_inject_mcp {
                        Some(self.config.pm.mcp_http_path.clone())
                    } else {
                        None
                    },
                    doc_mcp_path: if self.config.doc.auto_inject_mcp {
                        Some(self.config.doc.mcp_http_path.clone())
                    } else {
                        None
                    },
                    // ADR-101: same gate as the initial `memory_mcp_url` write
                    // below — a disabled supervisor must not be re-advertised
                    // by an IP-change pass.
                    memory_mcp_path: if self.config.memory.enabled
                        && self.config.memory.auto_inject_mcp
                    {
                        Some(self.config.memory.mcp_http_path.clone())
                    } else {
                        None
                    },
                };
                crate::lifecycle::advertise_watchdog::spawn_advertise_watchdog(
                    shared_state.clone(),
                    trigger.clone(),
                    watchdog_cfg,
                );

                Some(trigger)
            } else {
                None
            };

        // Keep a clonable trigger for the dev-mode vault auto-unlock
        // task (below). The handle's `mark_ready()` is invoked via the
        // shared state (see GatewayState::mqtt_publisher_handle); this
        // `unlock_republish_trigger` is kept for backward-compatible
        // explicit republish on first provider-key visibility.
        let unlock_republish_trigger = mqtt_publisher_trigger.clone();

        // Inventory-change signal notifier. Wakes on every mutation to
        // `installed_agents` (node install completion, node retained
        // replay, empty-payload clear, HTTP uninstall) and publishes an
        // non-retained `acowork/desktop/inventory` signal (see
        // `mqtt::inventory_notifier` for why both choices matter).
        // Subscribers (Desktop App) refetch `GET /api/agents` on each
        // signal — replaces the prior 30s `setInterval` fallback in the
        // AgentList sidebar.
        //
        // The trigger is also written into `inventory_trigger_slot` so
        // the dispatch layer (which started before this point) can
        // dereference it on every inventory aggregation.
        //
        // The handle is bound for the rest of `run()`: `tokio` detaches
        // a `JoinHandle` on drop rather than aborting it, so the loop
        // would survive either way, but holding it keeps the loop's
        // lifetime explicit and local to this block.
        let (inventory_trigger, _inventory_notifier_handle): (
            Option<crate::mqtt::InventoryNotifierTrigger>,
            Option<crate::mqtt::InventoryNotifierHandle>,
        ) = if let Some(ref client) = mqtt_gw_client {
            let notifier = crate::mqtt::MqttInventoryNotifier::new(client.as_ref().clone());
            let handle = notifier.start();
            let trigger = notifier.create_trigger();
            *inventory_trigger_slot.lock().await = Some(trigger.clone());
            tracing::info!("MQTT Inventory Notifier started");
            (Some(trigger), Some(handle))
        } else {
            (None, None)
        };

        // ADR-059 Phase 1.2: start the bootstrap snapshot publisher
        // once the MQTT client exists. It seeds the retained
        // `acowork/global/bootstrap` topic immediately (BOOTING) and
        // republishes on every orchestrator change (READY / DEGRADED /
        // FAILED / SHUTTING_DOWN). The handle is bound for the rest of
        // `run()`: dropping it would abort the publish loop.
        let _bootstrap_publisher_handle: Option<crate::mqtt::BootstrapPublisherHandle> =
            if let Some(ref client) = mqtt_gw_client {
                let handle = crate::mqtt::BootstrapPublisher::start(
                    crate::mqtt::BootstrapPublisherOptions {
                        client: client.as_ref(),
                        orchestrator: bootstrap_orchestrator.clone(),
                    },
                );
                tracing::info!("MQTT Bootstrap publisher started (acowork/global/bootstrap)");
                Some(handle)
            } else {
                None
            };

        // ADR-033: Start cron scheduler (uses MQTT for Intent delivery).
        // Must be started AFTER MQTT client is available.
        {
            let cron_mqtt = mqtt_gw_client.clone();
            let cron_gw_state = shared_state.clone();
            let cron_node_control = node_control_slot.lock().await.clone();
            // D2: cron delivers through the shared wake-and-deliver
            // entry, whose online fact is the retained AgentStatus —
            // it needs the same registry view the dispatch layer feeds.
            let cron_registry = agent_registry.clone();
            let _cron_handle = tokio::spawn(async move {
                crate::cron::run_cron_scheduler(
                    cron_scheduler,
                    cron_mqtt,
                    cron_gw_state,
                    cron_node_control,
                    cron_registry,
                )
                .await;
            });
        }

        // Reconcile loop: periodic safety net for the
        // `running_agents` ↔ `agent_registry` consistency contract.
        //
        // The dispatch layer's event handlers are the low-latency
        // path (they react to each `acowork/agents/+/status` transition
        // in milliseconds), but they can still drop a transition when
        // the Gateway's MQTT client itself reconnects mid-burst — the
        // broker's retained replay ordering, a stale message that races
        // a fresh one, or a process crash between LWT `offline` and the
        // live `online` re-publish. After an OS sleep/wake in
        // particular the post-2026-09-07 incident logs show the
        // sequence `offline → online` arriving within 121 ms — a stale
        // `offline` (the LWT, or its retained replay after a Gateway
        // (re)connect) can be processed after a live `online` already
        // re-installed the entry, and nothing re-installs it a second
        // time once the dispatcher has moved on. The dispatch event
        // path converges most cases; this loop is the consistency
        // backstop for the rest.
        //
        // The loop below polls every `RECONCILE_INTERVAL` (its first
        // tick fires immediately, so retained snapshots from Runtimes
        // that connected before the Gateway are picked up right away)
        // and walks the full authoritative view from `agent_registry`,
        // re-installing any missing `running_agents` entry and
        // dropping any stale one. It is **idempotent** and cheap (a
        // few `HashMap` walks), so the interval can be aggressive. A
        // Gateway MQTT (re)connect itself converges through the
        // dispatch event path — the broker replays the retained
        // statuses and each one re-aligns `running_agents` — so no
        // separate reconnect hook is needed on top of the interval.
        if let Some(agent_registry_for_reconcile) = agent_registry_for_reconcile_opt.clone() {
            let gw_for_reconcile = shared_state.clone();
            let _reconcile_handle = tokio::spawn(async move {
                crate::mqtt::dispatch::run_reconcile_loop(
                    gw_for_reconcile,
                    agent_registry_for_reconcile,
                )
                .await;
            });
            tracing::info!(
                interval_secs = crate::mqtt::dispatch::RECONCILE_INTERVAL_SECS,
                "running_agents reconcile loop started (ADR-055 / post-wake recovery)"
            );
        }

        // Start the HTTP API as early as possible: the desktop app's
        // readiness probe (10 s) must see :19876 listening before the
        // node / System Agent startup dance completes. Every dependency
        // (broker, Gateway client, registries, node-control slot) is
        // ready at this point.
        let node_control = node_control_slot.lock().await.clone();
        let http_node_registry = node_registry.clone();
        // ADR-059 §2.3: hand the readiness registry to the HTTP layer
        // so mutation handlers gate on Node control-plane readiness.
        let http_bootstrap_registry = bootstrap_registry.clone();
        // ADR-059 §6.2: the operation store is shared between the HTTP
        // handlers (open records) and the MQTT dispatch layer
        // (transition them on NodeEvent correlation).
        let http_operation_store = operation_store_shared.clone();
        // ADR-084: the HTTP layer resolves whether the account system is on
        // from the mode alone — the account service itself now lives behind
        // the user proxy, and its public key arrives via the supervisor.
        let http_auth_mode = self.config.effective_auth_mode();
        // Design doc 24 §7.2 (M3): remote-origin listener ports, read
        // up front — the spawn below takes `self` out of scope.
        let remote_listener = crate::http::server::RemoteListenerParams {
            http_port: self.config.relay.remote_http_port,
            mqtt_port: self.config.relay.remote_mqtt_port,
        };
        // ADR-087 D2: the HTTP server mints enrollment tokens bound to the
        // caller; the store stays alive on this side for the local-node
        // bootstrap token below, so hand the spawn a clone.
        let enrollment_tokens_for_http = enrollment_tokens.clone();
        let http_handle = tokio::spawn(async move {
            if let Err(e) = crate::http::server::start_http_server(
                &http_config,
                http_state,
                &data_dir_path,
                log_reload_handle,
                mqtt_gw_client,
                mqtt_publisher_trigger,
                inventory_trigger,
                Some(runtime_http_registry),
                Some(agent_registry),
                node_control,
                Some(http_node_registry),
                Some(http_bootstrap_registry),
                Some(http_operation_store),
                http_auth,
                http_auth_mode,
                Some(http_relay_client),
                Some(remote_listener),
                node_owners,
                agent_owners,
                enrollment_tokens_for_http,
                pending_installs,
            )
            .await
            {
                tracing::error!("HTTP server failed: {}", e);
            }
        });

        // Design doc 24 §8.2: auto-start the relay tunnel when the
        // persisted config says so (enabled via `POST /api/relay/enable`
        // in a previous run). Delayed a moment so boot logs stay ordered
        // and the HTTP API (which surfaces the status) is already up.
        if self.config.relay.enabled {
            match self.config.relay.url.clone() {
                Some(url) => {
                    let client = relay_client.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        if let Err(e) = client.enable(url).await {
                            tracing::error!(error = %e, "relay tunnel failed to start from persisted config");
                        }
                    });
                }
                None => tracing::warn!(
                    "relay.enabled=true but relay.url is not set; tunnel not started"
                ),
            }
        }

        // ADR-055 §6.11: ensure a local Node Agent is running and
        // supervise it (orphan cleanup → reuse window → spawn + reaper).
        // Must run AFTER the MQTT broker + Gateway client are up so the
        // retained `acowork/nodes/+/status` reuse window works.
        // ADR-075 D7: the Gateway no longer pre-issues placeholder node
        // tokens (enroll mints the long-lived credential on first
        // connect). When MQTT auth is enabled, hand the child a one-time
        // enrollment token as its boot credential — it is swapped for
        // the persisted node_token at enroll time.
        // ADR-087: the token carries the default owner (earliest-created
        // admin — the operator who can start a Gateway on this machine IS
        // the admin), so the Gateway-spawned local node never lands
        // ownerless. `None` before any admin exists: the enroll-time
        // fallback and the one-shot adoption cover that window.
        let local_node_token = if mqtt_config.auth_enabled {
            Some(
                enrollment_tokens
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .create_token(std::time::Duration::from_secs(3600), default_owner.clone()),
            )
        } else {
            None
        };
        let local_node_supervisor: Option<
            std::sync::Arc<crate::gateway::node_manager::LocalNodeSupervisor>,
        > = if mqtt_broker_started && self.config.local_node.enabled {
            match crate::gateway::node_manager::ensure_local_node(
                mqtt_config.port,
                &self.config.packages_dir,
                node_registry.clone(),
                local_node_token,
                self.config.node_proxy_port,
                self.config.node_lsp_relay_port,
                &self.config.data_dir,
            )
            .await
            {
                Ok(supervisor) => Some(supervisor),
                Err(e) => {
                    tracing::warn!(error = %e, "Local node agent supervision failed");
                    None
                }
            }
        } else if mqtt_broker_started && !self.config.local_node.enabled {
            tracing::info!(
                "Local node agent disabled by [local_node] enabled=false \
                     (or --no-spawn-local-node); relying on externally-started nodes"
            );
            None
        } else {
            None
        };

        // ADR-055 §6.11: Fix 1 follow-up — raise the publisher ready barrier
        // once the local node has enrolled, regardless of whether the
        // vault auto-unlock path fired. In non-dev_mode this is the only
        // signal that releases the publisher loop; in dev_mode it covers
        // the window between vault-unlock and node-enrollment so we
        // don't emit a snapshot before the node is ready to talk.
        if local_node_supervisor.is_some() {
            let registry_for_ready = node_registry.clone();
            let ready_state = shared_state.clone();
            tokio::spawn(async move {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
                while tokio::time::Instant::now() < deadline {
                    if crate::mqtt::node_registry::local_node_id(&registry_for_ready)
                        .await
                        .is_some()
                    {
                        let st = ready_state.read().await;
                        if let Some(ref h) = st.mqtt_publisher_handle {
                            h.mark_ready();
                        }
                        tracing::info!("Publisher ready barrier raised by local node online event");
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                tracing::warn!(
                    "Local node did not enroll within 20s; \
                     publisher ready barrier is left for the vault \
                     auto-unlock path (or, in non-dev_mode, will not fire \
                     until the first resource change)"
                );
            });
        }

        // In dev_mode, auto-unlock the vault in the background. Unlock runs
        // Argon2id (a deliberately slow KDF, ~1 s) under the GatewayState
        // write lock, so it must start AFTER the HTTP server is up — a
        // boot-time unlock would serialize all startup writes behind it
        // (the broker / node / System Agent path does not touch the vault;
        // only provider-credential HTTP handlers do, and they queue behind
        // the 1 s unlock at most). This is intentionally insecure —
        // dev_mode is for local development only.
        if self.config.dev_mode {
            let unlock_state = shared_state.clone();
            let unlock_republish = unlock_republish_trigger;
            tokio::spawn(async move {
                let vault_boot_handle = match unlock_state
                    .read()
                    .await
                    .bootstrap
                    .vault_readiness_handle
                    .clone()
                {
                    Some(h) => h,
                    None => {
                        tracing::error!(
                            "Vault readiness handle not registered; dev-mode auto-unlock skipped"
                        );
                        return;
                    }
                };
                match crate::vault::unlock_vault_and_mark_ready(
                    unlock_state,
                    vault_boot_handle.clone(),
                    unlock_republish,
                    "dev-mode-unlock".to_string(),
                    "vault unlocked (dev_mode)".to_string(),
                )
                .await
                {
                    Ok(()) => {}
                    Err(e) => {
                        tracing::warn!("Failed to auto-unlock vault in dev_mode: {}", e);
                        // A failed unlock means the vault cannot serve
                        // provider credentials — a required subsystem
                        // in Failed drops the aggregated phase to
                        // FAILED.
                        vault_boot_handle
                            .mark_failed(Some(format!("vault auto-unlock failed: {e}")));
                    }
                }
            });
        } else {
            // No background unlock in non-dev mode. The ready barrier can
            // still be raised by the local-node ready path below; if
            // neither fires, the publisher loop will wait forever, which
            // is correct — surfacing "global resources" without an
            // enrolled node is exactly the onboarding bug we are fixing.
            drop(unlock_republish_trigger);
        }

        // S5.9: Wait for either SIGTERM/SIGINT or HTTP server exit.
        // On signal, all server tasks are aborted, triggering
        // PidFileGuard::Drop which cleans up the pidfile.
        let shutdown_result = tokio::select! {
            http_result = http_handle => {
                tracing::info!("HTTP server exited");
                http_result.map_err(|e| GatewayError::Config(format!("HTTP server task error: {}", e)))
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Received shutdown signal, cleaning up...");

                // ADR-059 Phase 1.2: publish the SHUTTING_DOWN snapshot
                // BEFORE tearing down subsystems, so consumers observing
                // the retained `acowork/global/bootstrap` topic see a
                // graceful transition instead of a silent disappearance.
                bootstrap_orchestrator.mark_shutting_down();

                // ADR-055 §6.2: Runtime processes are hosted by the node —
                // the Gateway no longer kills them directly. The local node
                // is shut down below (its supervisor SIGTERMs the node
                // process group); any surviving Runtime is reaped by the
                // next Gateway startup's orphan cleanup.

                // Kill the embedding service process before exiting.
                // This prevents acowork-embed from becoming an orphan process.
                {
                    let gw = shared_state.read().await;
                    if let Some(ref embed_state) = gw.embed_process
                        && embed_state.pid != 0
                    {
                        tracing::info!(pid = embed_state.pid, "Shutting down embedding service");
                        if let Err(e) = crate::lifecycle::embed::kill_embed_process(embed_state.pid).await {
                            tracing::warn!(error = %e, "Failed to kill embedding service process");
                        }
                    }
                }

                // The other Gateway-hosted services (doc / pm / user / memory)
                // are spawned by this process, so this process owns their
                // lifetime — same reason as `acowork-embed` above. Without
                // this they outlive a restart until their ADR-018 watchdog
                // notices the Gateway is gone (5 min default): duplicate
                // services on the old ports, and on Windows a locked
                // `acowork-*.exe` that blocks the next build.
                {
                    let gw = shared_state.read().await;
                    let hosted = [
                        ("acowork-doc", gw.doc_process.as_ref().map(|p| p.pid)),
                        ("acowork-pm", gw.pm_process.as_ref().map(|p| p.pid)),
                        ("acowork-user", gw.user_process.as_ref().map(|p| p.pid)),
                        ("acowork-memory", gw.memory_process.as_ref().map(|p| p.pid)),
                    ];
                    drop(gw);
                    for (name, pid) in hosted {
                        let Some(pid) = pid.filter(|p| *p != 0) else {
                            continue;
                        };
                        tracing::info!(name, pid, "Shutting down hosted service");
                        if let Err(e) = crate::lifecycle::process::kill_agent_process(pid).await {
                            tracing::warn!(name, error = %e, "Failed to kill hosted service process");
                        }
                    }
                }

                // ADR-055 §6.11: shut down the local Node Agent.
                if let Some(supervisor) = &local_node_supervisor {
                    supervisor.shutdown().await;
                }

                Ok(())
            }
        };

        shutdown_result?;

        Ok(())
    }

    /// List installed agents.
    ///
    /// Returns a snapshot of installed agents and their current running
    /// state. Marked `async` because `self.state` is a tokio
    /// [`SharedState`] (the daemon-side handlers already use it under
    /// `read().await`).
    pub async fn list_agents(&self) -> Vec<AgentListEntry> {
        let state = self.state.read().await;
        state
            .installed_agents
            .values()
            .map(|info| AgentListEntry {
                agent_id: info.agent_id.clone(),
                name: info.name.clone(),
                version: info.version.clone(),
                running: state.is_running(&info.agent_id),
            })
            .collect()
    }

    /// Ensure the Gateway's own directories exist.
    ///
    /// `packages_dir` is deliberately absent: it points at the **node's**
    /// package directory (ADR-055) and is created by the node itself
    /// (`acowork_node::config::NodeConfig::ensure_dirs`) — see ADR-009
    /// §V-O.
    fn ensure_dirs(&self) -> Result<(), GatewayError> {
        self.config.ensure_dirs()
    }
}

/// Agent list entry for display
#[derive(Debug, Clone)]
pub struct AgentListEntry {
    pub agent_id: String,
    pub name: String,
    pub version: String,
    pub running: bool,
}

impl std::fmt::Display for AgentListEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let status = if self.running { "running" } else { "stopped" };
        write!(
            f,
            "{} ({}) v{} [{}]",
            self.name, self.agent_id, self.version, status
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GatewayConfig;

    fn test_config() -> GatewayConfig {
        GatewayConfig {
            config_source_path: None,
            vault_dir: std::env::temp_dir()
                .join("acowork-test-vault")
                .to_string_lossy()
                .to_string(),
            packages_dir: std::env::temp_dir()
                .join("acowork-test-packages")
                .to_string_lossy()
                .to_string(),
            data_dir: std::env::temp_dir()
                .join("acowork-test-data")
                .to_string_lossy()
                .to_string(),
            log_level: "info".to_string(),
            log_file_size_mb: 10,
            log_file_count: 20,
            timeouts: acowork_core::Timeouts::default(),
            max_iterations: 20,
            dev_mode: true,
            http: crate::config::HttpConfig::default(),
            llm: crate::config::LlmConfig::default(),
            intent: crate::config::IntentConfig::default(),
            default_provider: None,
            default_model: None,
            max_output_tokens_limit: 32768,
            embedding_model: None,
            hf_mirrors: Vec::new(),
            data_flow: crate::config::DataFlowConfig::default(),
            mqtt: crate::config::MqttConfig::default(),
            advertise_host: None,
            node_proxy_port: None,
            node_lsp_relay_port: None,
            local_node: crate::config::LocalNodeConfig::default(),
            pm: crate::config::PmConfig::default(),
            doc: crate::config::DocConfig::default(),
            user: crate::config::UserConfig::default(),
            memory: crate::config::MemoryConfig::default(),
            security: crate::config::SecurityConfig::default(),
            auth_mode: None,
            relay: crate::config::RelayClientConfig::default(),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_gateway_new() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        assert!(gateway.list_agents().await.is_empty());
    }

    #[test]
    fn test_ensure_dirs() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        assert!(gateway.ensure_dirs().is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_list_agents_empty() {
        let config = test_config();
        let gateway = Gateway::new(config).unwrap();
        let list = gateway.list_agents().await;
        assert!(list.is_empty());
    }

    #[test]
    fn test_agent_list_entry_display() {
        let entry = AgentListEntry {
            agent_id: "com.example.weather".to_string(),
            name: "Weather Agent".to_string(),
            version: "1.0.0".to_string(),
            running: true,
        };
        let display = format!("{}", entry);
        assert!(display.contains("Weather Agent"));
        assert!(display.contains("running"));
    }
}

