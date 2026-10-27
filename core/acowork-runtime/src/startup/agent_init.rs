//! Phase A: per-agent initialization.
//!
//! Covers Steps 0-8 of the original `async_main`:
//!   - Load .agent package
//!   - Connect to Gateway gRPC + AgentHello handshake
//!   - Build system prompt & SkillRegistry
//!   - Resolve LLM provider
//!   - Build FallbackEmbeddingProvider (3-tier chain)
//!   - Build ToolRegistry + activate tools
//!   - Build tool definitions
//!   - Build ContextBuilder
//!   - Create Budget + chunk_tx/chunk_rx channel

use std::sync::Arc;

use acowork_core::protocol::ProtocolType;

use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::AgentBootContext;
use crate::startup::pull_global_resources_from_gateway;

/// Phase A: initialize all per-agent (cross-session) resources.
///
/// This function is the first phase of the agent startup sequence.
/// It produces an `AgentBootContext` that is consumed by subsequent phases.
pub(crate) async fn phase_a_init_agent(config: &RuntimeConfig) -> Result<AgentBootContext> {
    let _span = tracing::info_span!("startup_phase_a").entered();

    use crate::agent::context::ContextBuilder;
    use crate::agent_config::load_agent_provider_config;
    use crate::embedding::EmbeddingProvider;
    use crate::embedding::remote::RemoteEmbeddingProvider;
    use crate::package::loader::load_package;
    use crate::package::prompt_builder::build_system_prompt_with_mode;
    use crate::tools::builtin;
    use crate::tools::registry::ToolRegistry;

    // ── Step 1: Load .agent package ─────────────────────────────────
    tracing::info!(path = %config.package_path, "Loading .agent package");
    let loaded = load_package(std::path::Path::new(&config.package_path))?;
    tracing::info!(
        agent_id = %loaded.manifest.agent_id,
        name = %loaded.manifest.name,
        "Package loaded successfully"
    );

    // ── ADR-040: gRPC path removed. Only MQTT transport remains. ─────

    // ── ADR-033: Start MQTT client + HTTP server ─────────────────────
    let mut mqtt_client: Option<crate::mqtt::RuntimeMqttClient> = None;
    let mut available_cache: Option<crate::mqtt::SharedAvailableCache> = None;
    let mut control_rx: Option<tokio::sync::mpsc::UnboundedReceiver<(String, Vec<u8>)>> = None;
    // ADR-042: receiver for `acowork/global/user_profile` retained updates,
    // wired through `gateway_loop` to SessionManager.
    let mut identity_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<acowork_core::protocol::UserProfile>,
    > = None;
    let mut provider_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::ProviderUpdate>,
    > = None;
    // ADR-033: receiver for `acowork/global/embedding_models` retained
    // updates, wired through `gateway_loop` to SessionManager so sessions
    // rebuild their embedding provider when the embed sidecar becomes
    // ready or the active model switches.
    let mut embedding_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::EmbeddingUpdate>,
    > = None;
    // ADR-055 §6.7 (Phase 4): receiver for node LSP relay state changes,
    // wired through `gateway_loop` to SessionManager.
    let mut lsps_update_rx: Option<
        tokio::sync::mpsc::UnboundedReceiver<crate::mqtt::client::LspRelayUpdate>,
    > = None;
    let mut runtime_http_port: Option<u16> = None;

    // Shared session snapshot map, created here so it can be passed to both
    // the HTTP server (Phase A) and SessionManager (Phase B). The same Arc
    // is shared, so session state writes are immediately visible to HTTP reads.
    let session_snapshots: crate::agent::session_state::SharedSessionSnapshots =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    // Shared latest session Arc, written by SessionManager on every session
    // creation and startup scan, read by the HTTP /sessions/latest endpoint.
    let latest_session: crate::agent::session_state::SharedLatestSession =
        Arc::new(std::sync::RwLock::new(None));

    // ADR-047: Shared session config map for SessionConfigService.
    let session_configs: crate::usecases::SharedSessionConfigs =
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new()));

    // ADR-033: Dispatch channel for Runtime HTTP → agent loop write operations.
    // HTTP handlers send (session_id, InboundMessage) tuples; the gateway loop
    // forwards them to the right session's AgentLoop via send_inbound().
    let (http_dispatch_tx, http_dispatch_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, crate::agent::inbound::InboundMessage)>();
    let http_dispatch_shared: crate::http::SharedDispatchSender =
        Arc::new(tokio::sync::Mutex::new(Some(http_dispatch_tx)));

    // Shared degradation reasons. Created empty here and populated by
    // Phase B if session persistence fails. The same Arc is passed to
    // the HTTP server so `/health` can surface non-fatal startup errors.
    let degraded_reasons: crate::http::SharedDegradation =
        Arc::new(std::sync::RwLock::new(Vec::new()));

    // ADR-028: Late-bind slot for the AgentCore. Created empty in Phase A
    // and populated by Phase B once `AgentCore::new` completes. The
    // `list_sessions` HTTP handler uses it to merge disk-scanned token
    // totals and report `agent_total_input_tokens` / `agent_total_output_tokens`.
    let agent_core_shared: crate::http::SharedAgentCore = Arc::new(std::sync::RwLock::new(None));

    // ADR-040: Late-bind slot for session metadata service. Populated
    // by Phase B. The `list_sessions` handler falls back to direct
    // implementation when this is still None.
    let session_metadata_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionMetadataService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // ADR-081 §4.2: Late-bind slot for the conversation vector index.
    // Populated in Phase B (session_init.rs) once the Runtime SQLite store is
    // open; the indexer task tails the JSONL conversation logs from there.
    let conversation_index_slot: crate::http::SharedConversationIndex =
        Arc::new(std::sync::RwLock::new(None));

    // ADR-040: Late-bind slots for workspace query + mutation services.
    // Workspace services are populated immediately after the runtime
    // boots (no async dependency like memory) — see session_init.rs.
    let workspace_query_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceQueryService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));
    let workspace_mutation_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::WorkspaceMutationService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // ADR-078: Late-bind slot for the git query service (read-only
    // `/git/*` endpoints). Same sync-work_dir pattern — populated
    // alongside the workspace services in session_init.rs.
    let git_query_slot: Arc<tokio::sync::Mutex<Option<Arc<dyn crate::usecases::GitQueryService>>>> =
        Arc::new(tokio::sync::Mutex::new(None));

    // ADR-040 follow-up: Late-bind slot for the Tools-panel persistence
    // service (the `/agents/{id}/mcp-servers` HTTP handlers). The service holds
    // only the work_dir (sync, no async dependency), so we wire it
    // immediately after the workspace services in session_init.rs.
    let agent_tools_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentToolsService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // ADR-040 follow-up: Late-bind slot for the per-agent runtime
    // config service (`PUT /agents/{id}/config`). Shares the same
    // sync-work_dir state as `agent_tools_slot`, so the same
    // session_init Phase B pattern applies.
    let agent_config_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AgentConfigService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // ADR-046: attachment blob store (`POST /sessions/{sid}/files` +
    // `GET /files/{doc_id}`). Same sync-work_dir pattern as the other
    // services — populates in Phase B.
    let attachment_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::AttachmentService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // ADR-047: session config service for GET/PUT /sessions/{sid}/config.
    let session_config_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::SessionConfigService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));
    let rag_provider_slot: crate::http::server::SharedRagProvider =
        Arc::new(std::sync::RwLock::new(None));
    // ADR-048: late-bind slot for the Debug service. Empty in Phase A;
    // populated in Phase B once SessionManager has built per-session
    // debug controllers (only when DevMode is active — outside DevMode
    // the slot stays None and `/api/debug/*` returns 503).
    let debug_service_slot: Arc<
        tokio::sync::Mutex<Option<Arc<dyn crate::usecases::DebugService>>>,
    > = Arc::new(tokio::sync::Mutex::new(None));

    // Late-bind slot for `SessionManager`. Empty in Phase A; populated
    // by Phase B once SessionManager is constructed. Cloned into the
    // Runtime HTTP server so the runtime `POST /api/debug/enable`
    // route can flip DevMode on for an agent that started without it.
    let session_manager_slot: crate::http::server::SharedSessionManagerSlot =
        Arc::new(tokio::sync::RwLock::new(None));

    // ADR-038-style late-bind slot for the MQTT client. The Runtime HTTP
    // server starts here in Phase A before `mqtt_client` is connected, so
    // we hand the server an `Arc<Mutex<Option<_>>>` slot and populate it
    // once the broker connection succeeds. The `PUT /agents/{id}/config`
    // handler uses it to re-PUBLISH the retained AgentConfig snapshot.
    let mqtt_client_slot: crate::http::server::SharedMqttClientSlot =
        Arc::new(tokio::sync::Mutex::new(None));

    // Shared WorkspaceResolver — created in Phase A so the HTTP server
    // can reload it after workspace mutations (create/update/delete).
    // The same `Arc` is injected into SessionManager in Phase B (see
    // `context.rs` / `session_init.rs`), so a reload triggered by a
    // mutation handler is immediately visible to `route_workspace_switch`
    // — a freshly-added workspace becomes selectable without a restart.
    let workspace_resolver: crate::tools::workspace_resolver::SharedResolver =
        Arc::new(std::sync::RwLock::new(
            crate::tools::workspace_resolver::WorkspaceResolver::new(&config.work_dir),
        ));
    // Fix-3 observability: log the on-disk path + allowed_dirs count at
    // startup so users reporting "workspace disappeared on restart" can
    // correlate the workspace_resolver state with the work_dir the
    // runtime was actually using. The number should match the entries
    // persisted on the previous run (see
    // `workspace_mutation_impl::save_config`). See
    // `desktop-onboarding-bugfix_154b7ff7.md` §Fix 3.
    {
        let guard = workspace_resolver
            .read()
            .expect("workspace_resolver lock poisoned");
        tracing::info!(
            work_dir = %config.work_dir,
            allowed_dirs = guard.allowed_dirs().len(),
            "WorkspaceResolver loaded from agent_workspaces.json"
        );
    }

    // ADR-058: workspace FS watcher set. When the HTTP server starts it
    // creates the set (shared with its state); otherwise a standalone
    // set is created below. Either way Phase C and the (future) CRUD
    // hooks reconcile the same Arc. Publishing goes through the same
    // `mqtt_client_slot` the HTTP server holds.
    let mut workspace_watcher_set: Option<crate::workspace::SharedWorkspaceWatcherSet> = None;

    // ADR-069 follow-up: shared MCP config notifier. When the HTTP
    // server starts it creates the notifier (and exposes it on the
    // handle); we clone it out above. Otherwise (no `--http-port`,
    // standalone) we create a fresh one below the `if let` — either way
    // the builtin MCP tools and the gateway loop subscribe to one Arc.
    let mut mcp_notifier: Arc<crate::mcp_notify::McpConfigNotifier> =
        Arc::new(crate::mcp_notify::McpConfigNotifier::default());

    if let Some(bind_port) = config.http_port {
        // ADR-055 §6.4: bind the Node-allocated loopback port so the
        // Node reverse proxy has a stable `{agent_id} → port` mapping.
        match crate::http::RuntimeHttpServer::start_with_bind_port(
            bind_port,
            std::path::PathBuf::from(&config.work_dir),
            // ADR-063: the agent's `.agent` package directory holds
            // `prompts/<file>.md` — distinct from `work_dir` (runtime
            // state). Wired into HttpState so the `/agents/{id}/prompts*`
            // routes can resolve canonical filenames under
            // `<package_dir>/prompts/`.
            loaded.package_dir.clone(),
            loaded.manifest.agent_id.clone(),
            // ADR-073: the instance identity (UUID) — the Runtime's HTTP
            // `/agents/{id}/*` routes accept only this id in the path, so
            // the Gateway's instance-scoped reverse-proxy reaches the right
            // process (see `HttpState::instance_matches`).
            config.instance_id().to_string(),
            session_snapshots.clone(),
            latest_session.clone(),
            http_dispatch_shared,
            degraded_reasons.clone(),
            mqtt_client_slot.clone(),
            session_metadata_slot.clone(),
            conversation_index_slot.clone(),
            workspace_query_slot.clone(),
            workspace_mutation_slot.clone(),
            git_query_slot.clone(),
            agent_tools_slot.clone(),
            agent_config_slot.clone(),
            attachment_slot.clone(),
            session_config_slot.clone(),
            rag_provider_slot.clone(),
            debug_service_slot.clone(),
            workspace_resolver.clone(),
            session_manager_slot.clone(),
            // ADR-063 §3.7.5 L2 reload: the HTTP reload handler
            // (`POST /agents/{id}/prompts/reload`) reads the live
            // `AgentCore` Arc through this slot. Phase B
            // (`session_init.rs`) populates it once `AgentCore::new`
            // completes. The slot is currently None at Phase A; the
            // reload handler returns 503 (same late-bind pattern as
            // every other shared resource) until Phase B finishes.
            agent_core_shared.clone(),
        )
        .await
        {
            Ok(server) => {
                runtime_http_port = Some(server.port);
                workspace_watcher_set = Some(server.workspace_watchers.clone());
                // ADR-069 follow-up: the HTTP server created the shared
                // MCP config notifier (same Arc on HttpState). Clone it
                // out now — the handle is `mem::forget`-ed below, and the
                // notifier must be shared with the builtin MCP tools +
                // gateway loop so `PUT /mcp-servers` / `PUT /mcp-tools`
                // (which fire it via HttpState) actually trigger a
                // reconnect.
                if let Some(n) = server.mcp_notifier.clone() {
                    mcp_notifier = n;
                }
                tracing::info!(port = server.port, "Runtime HTTP server started");
                std::mem::forget(server);
            }
            Err(e) => tracing::warn!(error = %e, "Runtime HTTP server start failed"),
        }
    }

    // Fallback for the no-HTTP-server path: create the set standalone so
    // Phase C's startup hook always has a set to reconcile into.
    let workspace_watcher_set: crate::workspace::SharedWorkspaceWatcherSet = workspace_watcher_set
        .unwrap_or_else(|| {
            Arc::new(tokio::sync::Mutex::new(
                crate::workspace::WorkspaceWatcherSet::new(
                    loaded.manifest.agent_id.clone(),
                    mqtt_client_slot.clone(),
                ),
            ))
        });

    if let Some(mqtt_port) = config.mqtt_port {
        let cache = crate::mqtt::new_shared_cache();
        let (control_tx, ctrl_rx) = tokio::sync::mpsc::unbounded_channel();
        // ADR-042: sink for `acowork/global/user_profile` retained updates.
        // The MQTT event loop decodes the payload and pushes the latest
        // `UserProfile` here; gateway_loop forwards to SessionManager so
        // all active sessions pick up the new identity_context.
        let (identity_update_tx, identity_update_chan_rx) =
            tokio::sync::mpsc::unbounded_channel::<acowork_core::protocol::UserProfile>();
        let (provider_update_tx, provider_update_chan_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::mqtt::client::ProviderUpdate>();
        // ADR-033: sink for `acowork/global/embedding_models` retained
        // updates (embed sidecar ready / model switch). gateway_loop
        // forwards to SessionManager::handle_embedding_config_update.
        let (embedding_update_tx, embedding_update_chan_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::mqtt::client::EmbeddingUpdate>();
        // ADR-055 §6.7 (Phase 4): sink for the node's LSP relay state.
        // The MQTT event loop decodes `acowork/nodes/{id}/lsps` retained
        // pushes; gateway_loop forwards to SessionManager so the
        // codebase tool is registered / unregistered in all sessions.
        let (lsps_update_tx, lsps_update_chan_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::mqtt::client::LspRelayUpdate>();
        // §6.3.3: sink for the node's reverse-proxy base URL on change
        // (LAN IP switch). The MQTT event loop decodes the retained
        // `acowork/nodes/{id}/info` push; when the base changed, we
        // re-publish this Runtime's retained `http_endpoint` so the
        // Gateway keeps routing through the node — no restart.
        let (node_proxy_update_tx, mut node_proxy_update_rx) =
            tokio::sync::mpsc::unbounded_channel::<String>();
        let config_json =
            crate::agent_config::load_agent_config(std::path::Path::new(&config.work_dir))
                .ok()
                .flatten()
                .map(|c| serde_json::to_string(&c).unwrap_or_default())
                .unwrap_or_default();
        match crate::mqtt::RuntimeMqttClient::connect(crate::mqtt::client::MqttConnectConfig {
            // ADR-055 D3: parameterise the broker host instead of
            // hard-coding 127.0.0.1 so the Runtime can connect to a
            // remote / distributed Gateway broker. Defaults to
            // 127.0.0.1 for single-machine topology (L3-5).
            host: config.gateway_host.as_deref().unwrap_or("127.0.0.1"),
            port: mqtt_port,
            agent_id: &loaded.manifest.agent_id,
            // ADR-073: instance identity (required — injected by the
            // Node at spawn time via --agent-instance-id).
            instance_id: config.instance_id(),
            agent_name: &loaded.manifest.name,
            agent_version: &loaded.manifest.version,
            config_json: &config_json,
            available_cache: cache.clone(),
            control_tx,
            identity_update_tx: Some(identity_update_tx),
            mcp_notifier: Some(mcp_notifier.clone()),
            provider_update_tx: Some(provider_update_tx),
            embedding_update_tx: Some(embedding_update_tx),
            node_id: config.node_id.as_deref(),
            lsps_update_tx: Some(lsps_update_tx),
            node_proxy_update_tx: Some(node_proxy_update_tx),
            work_dir: std::path::PathBuf::from(&config.work_dir),
            username: config.mqtt_username.as_deref(),
            password: config.mqtt_password.as_deref(),
            http_advertise_endpoint: config.http_advertise_endpoint.as_deref(),
            http_port: runtime_http_port,
        })
        .await
        {
            Ok(client) => {
                tracing::info!(agent_id=%loaded.manifest.agent_id, "Runtime MQTT client connected");
                // Publish HTTP port (Retained) so Gateway can proxy session queries.
                // Surface publish errors instead of silently swallowing them — a failed publish
                // is the most likely cause of the Gateway returning 503 with latency=0ms
                // (the Gateway subscribes to this retained topic on startup; if the publish
                // never lands, the registry stays empty and every reverse-proxy request 503s).
                if let Some(port) = runtime_http_port {
                    // ADR-055 D3: publish the full endpoint (not just the
                    // bare port) so the Gateway stores an opaque address it
                    // can reverse-proxy to without knowing whether it is
                    // loopback (Phase 1) or a Node reverse-proxy (Phase 2).
                    // Runtime HTTP stays loopback-only (D2 decision).
                    //
                    // ADR-055 §6.4: when the Node injects
                    // `--http-advertise-endpoint` (the Node reverse-proxy
                    // base URL), publish `{base}/agents/{instance_id}` so
                    // the Gateway routes through the Node. The Runtime only
                    // concatenates — node-internal topology stays private
                    // to the Node. ADR-073: the proxy path variable is the
                    // instance identity.
                    let instance_id = config.instance_id().to_string();
                    let endpoint = match &config.http_advertise_endpoint {
                        Some(base) => {
                            format!("{}/agents/{}", base.trim_end_matches('/'), instance_id)
                        }
                        None => format!("http://127.0.0.1:{}", port),
                    };
                    let topic = format!("acowork/agents/{}/http_endpoint", instance_id);
                    match client
                        .publish_raw(
                            &topic,
                            endpoint.as_bytes(),
                            crate::mqtt::client::MqttQoS::AtLeastOnce,
                            true, // Retained — so Gateway can discover on restart
                        )
                        .await
                    {
                        Ok(()) => tracing::info!(
                            agent_id=%loaded.manifest.agent_id,
                            instance_id=%instance_id,
                            %endpoint,
                            "Published retained http_endpoint for Gateway reverse-proxy discovery"
                        ),
                        Err(e) => tracing::error!(
                            agent_id=%loaded.manifest.agent_id,
                            instance_id=%instance_id,
                            %endpoint,
                            error=%e,
                            "Failed to publish retained http_endpoint — Gateway will return 503 until the Runtime restarts and re-publishes"
                        ),
                    }
                }
                // ADR-038-style: fill the late-bind slot so the HTTP
                // server's `PUT /agents/{id}/config` handler can re-PUBLISH
                // the retained AgentConfig snapshot when the Desktop flips a
                // builtin tool. `RuntimeMqttClient` is `Clone` (cheap Arc
                // handles for AsyncClient + event-loop guard — see
                // `mqtt/client.rs`), so we hand a clone to the slot and
                // keep the original for `AgentBootContext::mqtt_client`,
                // which downstream sites (`subsystems.rs` / `gateway_loop.rs`)
                // still consume by `&RuntimeMqttClient` reference.
                *mqtt_client_slot.lock().await =
                    Some(Arc::new(tokio::sync::Mutex::new(client.clone())));

                // §6.3.3: live re-publication of the retained
                // `http_endpoint`. When the node changes network (Wi-Fi
                // hotspot switch), the MQTT event loop decodes the fresh
                // retained NodeInfo and pushes the new reverse-proxy base
                // here; we re-publish `{base}/agents/{id}` retained so
                // the Gateway keeps routing through the node — no node or
                // Runtime restart, no agent restart.
                {
                    let endpoint_client = client.clone();
                    let endpoint_agent_id = loaded.manifest.agent_id.clone();
                    let endpoint_instance_id = config.instance_id().to_string();
                    tokio::spawn(async move {
                        while let Some(base) = node_proxy_update_rx.recv().await {
                            let endpoint = format!(
                                "{}/agents/{}",
                                base.trim_end_matches('/'),
                                endpoint_instance_id
                            );
                            let topic =
                                format!("acowork/agents/{}/http_endpoint", endpoint_instance_id);
                            match endpoint_client
                                .publish_raw(
                                    &topic,
                                    endpoint.as_bytes(),
                                    crate::mqtt::client::MqttQoS::AtLeastOnce,
                                    true,
                                )
                                .await
                            {
                                Ok(()) => tracing::info!(
                                    agent_id = %endpoint_agent_id,
                                    instance_id = %endpoint_instance_id,
                                    %endpoint,
                                    "Re-published retained http_endpoint after node proxy change (§6.3.3)"
                                ),
                                Err(e) => tracing::warn!(
                                    agent_id = %endpoint_agent_id,
                                    instance_id = %endpoint_instance_id,
                                    %endpoint,
                                    error = %e,
                                    "Failed to re-publish retained http_endpoint after node proxy change"
                                ),
                            }
                        }
                    });
                }

                mqtt_client = Some(client);
                available_cache = Some(cache);
                control_rx = Some(ctrl_rx);
                identity_update_rx = Some(identity_update_chan_rx);
                provider_update_rx = Some(provider_update_chan_rx);
                embedding_update_rx = Some(embedding_update_chan_rx);
                lsps_update_rx = Some(lsps_update_chan_rx);

                // Bug B fix (active pull, v3): after the MQTT client
                // has connected and the AvailableResourceCache has been
                // wired into the event loop, actively request the
                // Gateway's current global-resource snapshot via
                // `GET /api/global-resources`. This recovers from the
                // retained-delivery race where the Gateway published an
                // empty snapshot before vault unlock, and the corrected
                // snapshot is therefore never re-delivered to an
                // already-subscribed Runtime.
                //
                // The pull function BLOCKS until either (a) success,
                // (b) the Gateway returns a SHUTTING_DOWN sentinel, or
                // (c) the PULL_MAX_DURATION wall-clock deadline elapses.
                // Phase A therefore cannot publish `ready=true` until
                // the pull loop has finished — the session_task that
                // runs in Phase B will then find a fully-populated
                // `AvailableResourceCache` (or, on deadline, fall back
                // to whatever MQTT retained has delivered). See
                // `startup/global_resources_pull.rs` for the full
                // retry semantics and the "never poisons the cache"
                // guarantee on 503.
                let pull_ok = pull_global_resources_from_gateway(
                    config,
                    available_cache
                        .as_ref()
                        .expect("available_cache initialised above"),
                )
                .await;
                if pull_ok {
                    tracing::info!(
                        agent_id = %loaded.manifest.agent_id,
                        "Active pull of /api/global-resources completed; \
                         session_init will see a populated AvailableResourceCache"
                    );
                } else {
                    tracing::warn!(
                        agent_id = %loaded.manifest.agent_id,
                        "Active pull of /api/global-resources did not complete before deadline; \
                         session_init will fall back to whatever the MQTT retained path delivered"
                    );
                }

                // ADR-085 D3: publish HTTP_READY as soon as Phase A
                // completes — HTTP server is listening and `http_port`
                // is already announced, so the Gateway can proxy
                // `/workspaces`, `/workspaces/tree` and the other
                // Phase-A-ready endpoints immediately. Session
                // interfaces stay 503 until SESSIONS_READY (Phase B
                // end, see `session_init::phase_b_init_session`).
                // Previously this signal was deferred to Phase D and
                // waited on `sync_from_resolver()` → PollWatcher.watch
                // recursively walking large workspace roots (e.g. a
                // project tree with `target/`, `node_modules/`, etc.
                // could delay the ready signal by 6–8 seconds even
                // though the HTTP server was up after ~35 ms). See
                // ADR-058 §3.4 for the Desktop reconnect-driven full
                // tree re-sync that makes late watcher events safe.
                let _ = mqtt_client
                    .as_ref()
                    .expect("just initialized above")
                    .publish_lifecycle(acowork_core::mqtt_proto::AgentLifecycleState::HttpReady, "")
                    .await;
                tracing::info!(
                    agent_id=%loaded.manifest.agent_id,
                    "Phase A lifecycle (HTTP_READY) published; Phase B/C continue in background"
                );
            }
            Err(e) => tracing::warn!(error=%e, "MQTT client connect failed"),
        }
    }

    // ── Step 3: Load package-level prompt overrides ─────────────────
    //
    // ADR-094: the *system prompt* itself is NOT built here any more —
    // see Step 4.5. Its Progressive-mode skills summary is derived from
    // `skill_mode`, which is derived from
    // `agent_tools.json::load_skill.enabled`, and that file is only
    // resolved (loaded, or seeded from the manifest on first boot) in
    // Step 4. Building the prompt before Step 4 would resolve the skill
    // mode against a not-yet-seeded file, so a first boot would enable
    // the `load_skill` tool while leaving the system prompt without the
    // "Available Skills" summary — the exact inconsistent state ADR-094
    // §4 forbids. Only the tool-config-independent package prompt
    // overrides are loaded here.

    // ADR-053: agent-specific compaction prompt (prompts/summary.md, optional).
    // Loaded once here (Phase A) so BOTH Gateway mode (phase_b_init_session)
    // and Standalone mode (cli.rs AgentLoop construction) resolve the same
    // value — the file is a package-level declaration, so the package load
    // point is its single source of truth. `None` (no file) means the
    // built-in COMPACTION_SYSTEM_PROMPT fallback is used at compaction time.
    let compaction_prompt =
        crate::package::prompt_builder::load_compaction_prompt(&loaded.package_dir);
    if let Some(ref p) = compaction_prompt {
        tracing::debug!(
            prompt_len = p.len(),
            "Loaded agent-specific compaction prompt from prompts/summary.md"
        );
    }

    // ADR-063: 8 additional package-level prompt overrides. Each is loaded
    // from `prompts/<file>.md` independently — missing files yield `None`
    // and the LLM call site falls back to the built-in constant. We log at
    // debug level (not info) because the absence of an override is the
    // common case for the v1 .agent packages shipped today.
    let load_or_trace = |filename: &'static str, ctx_desc: &'static str| {
        let loaded =
            crate::package::prompt_builder::load_optional_prompt(&loaded.package_dir, filename);
        if let Some(ref p) = loaded {
            tracing::debug!(
                filename,
                ctx_desc,
                prompt_len = p.len(),
                "ADR-063: loaded package-level prompt override"
            );
        }
        loaded
    };
    let compact_template = load_or_trace("compact-template.md", "COMPACT_PROMPT");
    let title_prompt = load_or_trace("title.md", "TITLE_PROMPT");
    let abstention_prompt = load_or_trace("abstention.md", "abstention guidance (no built-in default)");

    // ADR-071 D7/D9: the distiller prompt override.
    // ADR-101: the distiller moved to the memory service, so the override moved
    // with it — the Runtime no longer loads `prompts/distiller-merge.md`.

    // ADR-099 (extended): `prompts/chat.md` is the whole chat-layer prompt,
    // not just the persona section. Seed it on first boot with the built-in
    // default so the Debug tab always opens a real, editable file; a missing
    // file keeps the built-in default in force.
    crate::package::prompt_builder::seed_chat_prompt(
        &loaded.package_dir,
        loaded.manifest.display_name.as_deref(),
        &loaded.manifest.name,
        loaded.manifest.role.as_deref(),
        &loaded.manifest.description,
    );
    let chat_persona = load_or_trace("chat.md", "chat prompt override");

    // ── Step 3.5: Load skill registry ───────────────────────────────
    // Kept alive on the boot context (not discarded) so Phase B injects it
    // into AgentCore — the runtime resolves per-turn chat `command` (skill
    // name) against it, and skills are the agent's own package content, not
    // something the frontend should supply.
    let skills_dir = loaded.package_dir.join("skills");
    let skill_registry = Arc::new(
        crate::skills::parser::SkillRegistry::load_from_dir(&skills_dir).unwrap_or_else(|e| {
            tracing::warn!(
                skills_dir = %skills_dir.display(),
                error = %e,
                "Failed to load skills registry, proceeding without skills"
            );
            crate::skills::parser::SkillRegistry::new()
        }),
    );

    // ── Step 3: Initialize LLM Provider ─────────────────────────────
    let mut gateway_current_provider_id: Option<String> = None;
    let provider_config = load_agent_provider_config(std::path::Path::new(&config.work_dir))
        .unwrap_or_else(|e| {
            tracing::warn!(
                work_dir = %config.work_dir,
                error = %e,
                "Failed to load agent_provider.json, starting without cached provider list"
            );
            None
        });
    if let Some(ref cfg) = provider_config {
        tracing::info!(
            provider_count = cfg.providers.len(),
            version = cfg.version,
            "Loaded provider config from agent_provider.json"
        );
    }

    let mut compat_cache: Option<Arc<crate::providers::compat::CompatCache>> = None;

    let (provider, resolved_model, available_models, protocol_type) = {
        // ADR-033: MQTT available_cache is the only provider source (gRPC path removed per ADR-040).
        if let Some(ref cache) = available_cache {
            // Wait up to 10s for the first acowork/global/providers to arrive.
            // The Gateway publishes every 5s (non-retained due to rumqttd 0.14).
            let mut waited = 0;
            loop {
                let cache_read = cache.read().await;
                if cache_read.providers.is_some() || waited >= 20 {
                    break;
                }
                drop(cache_read);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                waited += 1;
            }
            let cache_read = cache.read().await;
            if let Some(ref available_providers) = cache_read.providers {
                // Use MQTT provider info. API key resolution now happens
                // via the `api_key` field embedded in the
                // `acowork/global/providers` payload itself — see
                // [global_resources_publisher] and mqtt.md §3.1.1. The
                // legacy `ACOWORK_PROVIDER_<ID>_KEY` env var is no longer
                // used because the Gateway does not inject keys into the
                // Runtime process environment (bug 1 of the
                // senior-engineer startup log).
                let chosen = available_providers.providers.first();

                if let Some(prov) = chosen {
                    gateway_current_provider_id = Some(prov.id.clone());
                    let api_key = prov.api_key.clone();
                    // Empty string means "no key configured" (e.g. local
                    // Ollama). Convert to Option<&str> for the provider
                    // factory, which treats None as "no auth header".
                    let api_key_opt: Option<&str> = if api_key.is_empty() {
                        None
                    } else {
                        Some(api_key.as_str())
                    };
                    let available = prov.models.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
                    let model_id = prov
                        .models
                        .first()
                        .map(|m| m.id.clone())
                        .unwrap_or_else(|| "default".to_string());
                    let proto_str =
                        acowork_core::protocol::llm_protocol_to_protocol_type(prov.protocol_type);
                    let timeouts = Some(crate::providers::router::ProviderTimeouts::from(config));

                    // Load (or initialize) the per-agent compatibility cache.
                    // Persisted to `{work_dir}/config/provider_compat.json` so
                    // successful fallback profiles survive restarts.
                    let compat_cache_path = std::path::Path::new(&config.work_dir)
                        .join("config")
                        .join("provider_compat.json");
                    let compat_cache_arc =
                        crate::providers::compat::CompatCache::load(compat_cache_path);
                    compat_cache = Some(compat_cache_arc.clone());
                    let wiring = crate::providers::router::ProviderWiring {
                        provider_id: Some(prov.id.clone()),
                        compat_cache: Some(compat_cache_arc),
                    };

                    let provider = crate::providers::router::create_provider_with_wiring(
                        &prov.id,
                        &proto_str,
                        api_key_opt,
                        Some(&prov.base_url),
                        timeouts,
                        wiring,
                    );
                    tracing::info!(
                        provider = %prov.id,
                        model = %model_id,
                        num_models = available.len(),
                        has_api_key = api_key_opt.is_some(),
                        source = "mqtt_available_cache",
                        "Provider initialized from MQTT available cache"
                    );
                    (provider, model_id, available, proto_str)
                } else {
                    tracing::warn!("MQTT providers available but none selected, using noop");
                    noop_provider_tuple()
                }
            } else {
                tracing::warn!("MQTT connected but no providers in cache yet, using noop");
                noop_provider_tuple()
            }
        } else {
            noop_provider_tuple()
        }
    };

    // ── Step 3.5: Build Embedding Provider (single-tier, no fallback) ──
    //
    // Single source: env var ACOWORK_EMBED_ENDPOINT (Gateway embed_supervisor
    // port 18080). ADR-040: gRPC hello_config path removed.
    //
    // Failure semantics:
    //   - endpoint set + provider ok → INFO ✅
    //   - endpoint set + provider fail → ERROR ❌, emb_provider = None
    //   - endpoint not set → ERROR ❌, emb_provider = None
    //
    // emb_provider = None means memory::manager auto-falls back to
    // text_search_with_filter (manager.rs L270-280).

    let env_endpoint = std::env::var("ACOWORK_EMBED_ENDPOINT").ok();
    let env_model = std::env::var("ACOWORK_EMBED_MODEL").ok();
    let env_dim = std::env::var("ACOWORK_EMBED_DIMENSION")
        .ok()
        .and_then(|s| s.parse().ok());

    // Bug1: the Gateway never injects ACOWORK_EMBED_ENDPOINT into spawned
    // node/runtime env after the MQTT refactor, so endpoint is normally
    // missing here. Fall back to the cached global-resources snapshot
    // (populated by the synchronous active pull right after MQTT connect,
    // or by the retained `acowork/global/embedding_models` message).
    let cached_embed = match available_cache.as_ref() {
        Some(cache) => cache.read().await.embedding_config(),
        None => None,
    };
    let cache_present = available_cache.is_some();
    tracing::info!(
        target: "embedding_diag",
        env_endpoint_present = env_endpoint.is_some(),
        env_model_present = env_model.is_some(),
        env_dim_present = env_dim.is_some(),
        cache_present,
        cache_hit = cached_embed.is_some(),
        "Step 3.5: embedding config source check"
    );

    // Prefer explicit env values when present; otherwise recover the
    // endpoint/model/dimension from the MQTT-populated cache.
    let (embed_endpoint, embed_model_id, embed_dimension) = match cached_embed {
        Some((ep, mdl, d)) => (
            env_endpoint.or(Some(ep)),
            env_model.unwrap_or(mdl),
            env_dim.unwrap_or(d),
        ),
        None => (
            env_endpoint,
            env_model.unwrap_or_else(|| "bge-small-zh-v1.5".to_string()),
            env_dim.unwrap_or(512),
        ),
    };
    tracing::info!(
        target: "embedding_diag",
        resolved_endpoint = embed_endpoint.as_deref(),
        resolved_model_id = %embed_model_id,
        resolved_dimension = embed_dimension,
        resolved_endpoint_present = embed_endpoint.is_some(),
        "Step 3.5: embedding config resolved"
    );

    let emb_provider: Option<Arc<dyn EmbeddingProvider>> = match embed_endpoint.as_deref() {
        Some(endpoint) => {
            tracing::info!(
                endpoint = %endpoint,
                model = %embed_model_id,
                dim = embed_dimension,
                "🔧 Building embedding provider (Tier 1: local ONNX at {})",
                endpoint
            );
            match RemoteEmbeddingProvider::try_with_config_and_timeouts(
                endpoint,
                None,
                &embed_model_id,
                embed_dimension,
                &config.timeouts,
            ) {
                Ok(ep) => {
                    let dim = ep.dimension();
                    let name = ep.name();
                    tracing::info!(
                        endpoint = %endpoint,
                        model = %embed_model_id,
                        dim = dim,
                        provider_name = %name,
                        "✅ Embedding provider initialized successfully (Tier 1: local ONNX)"
                    );
                    // Nothing records this dimension any more, on purpose. The
                    // provider is the record: `update_embedding_provider` puts
                    // it in the shared cell and readers ask it. A separate copy
                    // of the number is what the memory panel kept showing after
                    // a model switch - stale, because the session that adopts a
                    // new provider never reaches this one.
                    Some(Arc::new(ep))
                }
                Err(e) => {
                    tracing::error!(
                        endpoint = %endpoint,
                        model = %embed_model_id,
                        dim = embed_dimension,
                        error = %e,
                        "❌ Failed to build embedding provider (Tier 1: local ONNX at {}). \
                         No fallback chain — fix the embed service (acowork-embed on {}) \
                         and restart runtime. Memory will degrade to text-only search.",
                        endpoint, endpoint
                    );
                    None
                }
            }
        }
        None => {
            tracing::error!(
                "❌ ACOWORK_EMBED_ENDPOINT not set. \
                 Gateway embed_supervisor must be running before runtime starts. \
                 Memory will degrade to text-only search."
            );
            None
        }
    };

    // ── Step 4: Build tool registry + activate by manifest ──────────

    let mut registry = ToolRegistry::new();
    // Tech-debt plan D1: intent_send now POSTs to the Gateway delivery
    // entry — it needs this agent's instance identity, the Gateway host,
    // and the machine's node token (the same credential the MQTT session
    // and the global-resources pull use).
    let intent_send_config = crate::tools::builtin::intent_send::IntentSendConfig {
        instance_id: config.agent_instance_id.clone(),
        gateway_host: config
            .gateway_host
            .clone()
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        node_token: config.mqtt_password.clone().unwrap_or_default(),
    };
    for tool in builtin::all_builtin_tools(
        &workspace_resolver,
        config.timeouts.tool_http_timeout_ms,
        Some(mcp_notifier.clone()),
        config.work_dir.clone(),
        intent_send_config,
        // ADR-094: `load_skill` needs the package dir to read
        // `<package_dir>/skills/<name>/SKILL.md`. `loaded.package_dir`
        // is the resolved `.agent` package root — distinct from the
        // session `work_dir` (which holds runtime state, not the
        // agent's read-only package files).
        loaded.package_dir.clone(),
    ) {
        registry.register(tool);
    }

    // ── ADR-051: Conditionally register rag_query tool ─────────────
    //
    // When the manifest declares a `[[tools]]` entry with `type = "rag"`,
    // construct an `HttpRagProvider` from the manifest `RagToolConfig`
    // and register the `rag_query` tool. The LLM invokes this tool
    // on-demand when it decides external knowledge is needed (tool-based
    // RAG, not automatic pre-retrieval merge — see ADR-051 §4.1.4 H).
    //
    // Auth resolution: the manifest `auth_ref` (e.g., "vault:rag_key")
    // is resolved from the `provider_key_vault` populated by the MQTT
    // available cache. If the key is not yet available at this point
    // (MQTT race), the provider is constructed with `None` auth and
    // RAG queries will return empty results until the key is available.
    // A follow-up enhancement could make the credential dynamic.
    let rag_provider: Option<Arc<dyn acowork_core::rag::RagProvider>> = if let Some((
        tool_name,
        rag_config,
    )) =
        loaded.manifest.rag_config()
    {
        tracing::info!(
            tool_name = %tool_name,
            endpoint = %rag_config.endpoint,
            "Manifest declares RAG tool - constructing HttpRagProvider"
        );

        // Best-effort auth resolution: try to find the RAG key in the
        // MQTT available cache's provider list. The `auth_ref` format
        // is "vault:<key_name>" - we look for a provider whose ID
        // matches <key_name>. If not found, the provider is
        // constructed with `None` auth; RAG queries will return
        // empty results until the key is available.
        let auth = {
            let key_value: Option<String> = available_cache.as_ref().and_then(|cache| {
                let cache_read = cache.blocking_read();
                cache_read.providers.as_ref().and_then(|p| {
                    rag_config
                        .auth_ref
                        .as_deref()
                        .and_then(|ref_str| {
                            crate::tools::rag::client::RagAuthCredential::vault_provider_name(
                                ref_str,
                            )
                        })
                        .and_then(|key_name| {
                            p.providers
                                .iter()
                                .find(|pr| pr.id == key_name)
                                .map(|pr| pr.api_key.clone())
                        })
                })
            });
            crate::tools::rag::client::RagAuthCredential::from_vault_ref(
                rag_config.auth_ref.as_deref(),
                &rag_config.auth_type,
                key_value.as_deref(),
            )
        };
        if matches!(auth, crate::tools::rag::client::RagAuthCredential::None)
            && rag_config.auth_ref.is_some()
        {
            tracing::warn!(
                auth_ref = ?rag_config.auth_ref,
                "RAG auth_ref declared but key not found in available cache - RAG queries will fail until key is available"
            );
        }

        let rag_client_config = crate::tools::rag::client::RagClientConfig::from_manifest(
            rag_config,
            tool_name.to_string(),
            auth,
        );
        let provider = Arc::new(crate::tools::rag::client::HttpRagProvider::new(
            rag_client_config,
        ));

        // Register the rag_query tool so the LLM can invoke it.
        let rag_tool = crate::tools::builtin::rag_query::RagQueryTool::new(provider.clone());
        registry.register(Arc::new(rag_tool) as Arc<dyn acowork_core::tools::traits::Tool>);
        tracing::info!("rag_query tool registered");

        Some(provider as Arc<dyn acowork_core::rag::RagProvider>)
    } else {
        None
    };

    // ── ADR-029: Resolve `agent_tools.json` enabled flags ──────────
    //
    // 1. If `{work_dir}/config/agent_tools.json` exists -> load it,
    //    merge with code registry, and **persist the merged result**
    //    so new tools from code upgrades are saved to disk.
    //    Persisted `enabled` flags are always preserved (the user's
    //    choices are the single source of truth; never overwrite).
    // 2. If absent -> generate initial config from manifest `[[tools]]`
    //    (only declared tools are enabled; if no `[[tools]]` section
    //    exists, all tools default to disabled — opt-in model).
    let work_path = std::path::Path::new(&config.work_dir);
    let code_tool_list: Vec<String> = registry
        .all()
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    let manifest_tool_names = loaded.manifest.builtin_tool_names();

    let resolved_entries: Vec<crate::agent_config::AgentToolEntry> =
        match crate::agent_config::load_agent_tools_config(work_path) {
            Ok(Some(persisted_cfg)) => {
                tracing::info!(
                    count = persisted_cfg.tools.len(),
                    "Loaded existing agent_tools.json — merging with code registry"
                );
                let merged =
                    crate::agent_config::merge_tools_config(&code_tool_list, &persisted_cfg.tools);
                // Persist the merged result so new tools from code
                // upgrades are written to agent_tools.json immediately.
                // This ensures the file is always a complete,
                // up-to-date reflection of the code registry.
                if let Err(e) = crate::agent_config::save_agent_tools_config(
                    work_path,
                    &crate::agent_config::AgentToolsConfig {
                        tools: merged.clone(),
                    },
                ) {
                    tracing::warn!(
                        error = %e,
                        "Failed to persist merged agent_tools.json after startup merge"
                    );
                }
                merged
            }
            Ok(None) => {
                // First start: seed from manifest [[tools]] declarations.
                // Only tools listed in the manifest are enabled; everything
                // else is disabled (opt-in model).  If the manifest has no
                // [[tools]] section, all tools are disabled by default.
                let initial = crate::agent_config::init_tools_config_from_manifest(
                    &code_tool_list,
                    &manifest_tool_names,
                );
                tracing::info!(
                    manifest_tools = manifest_tool_names.len(),
                    enabled = initial.iter().filter(|e| e.enabled).count(),
                    disabled = initial.iter().filter(|e| !e.enabled).count(),
                    "agent_tools.json absent — seeded from manifest [[tools]]"
                );
                // Persist the freshly generated config so future startups see it
                if let Err(e) = crate::agent_config::save_agent_tools_config(
                    work_path,
                    &crate::agent_config::AgentToolsConfig {
                        tools: initial.clone(),
                    },
                ) {
                    tracing::warn!(error = %e, "Failed to persist initial agent_tools.json");
                }
                initial
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to parse agent_tools.json — falling back to manifest-derived config"
                );
                crate::agent_config::init_tools_config_from_manifest(
                    &code_tool_list,
                    &manifest_tool_names,
                )
            }
        };

    // Activate with the merged enabled list, applying security
    // decorators (path guard + rate limiter) to each tool.
    let active_tools =
        registry.activate(&loaded.manifest, &workspace_resolver, 60, &resolved_entries);
    tracing::info!(
        total_enabled = active_tools.iter().filter(|e| e.enabled).count(),
        total_disabled = active_tools.iter().filter(|e| !e.enabled).count(),
        "agent_tools.json resolved"
    );

    // ── Step 4.5: Build system prompt ───────────────────────────────
    //
    // ADR-094: deliberately AFTER the `agent_tools.json` resolution
    // above. `resolve_skill_mode` reads `load_skill.enabled` from that
    // file (falling back to the manifest default only when the entry is
    // absent), so the file must already be seeded/merged before the
    // mode is resolved. Otherwise a first boot activates the
    // `load_skill` tool (via the freshly seeded config) while the
    // system prompt is built in Manual mode — no "Available Skills"
    // summary, i.e. the one state ADR-094 §4 exists to prevent.
    let skill_mode =
        crate::startup::super_mod::resolve_skill_mode(&loaded.manifest, &config.work_dir);
    tracing::debug!(
        skill_mode = ?skill_mode,
        "Skill mode resolved from the persisted agent_tools.json"
    );
    let prompt_parts = build_system_prompt_with_mode(&loaded.package_dir, skill_mode)?;
    tracing::debug!(
        base_len = prompt_parts.base.len(),
        has_skills = prompt_parts.skills_summary.is_some(),
        "System prompt built"
    );

    // ── Step 5: Build tool definitions ──────────────────────────────
    #[allow(unused_imports)]
    use acowork_core::tools::traits::Tool;
    // LLM-visible tool specs: enabled subset only.
    let tool_specs: Vec<(String, serde_json::Value)> = active_tools
        .iter()
        .filter(|e| e.enabled)
        .map(|e| {
            let spec = e.spec();
            let serialized = serde_json::to_value(&spec).unwrap_or_default();
            tracing::warn!(
                tool = %spec.name,
                has_parameters = serialized.get("parameters").is_some(),
                has_input_schema = serialized.get("input_schema").is_some(),
                "DEBUG: Tool spec serialized fields check"
            );
            (spec.name.clone(), serialized)
        })
        .collect();
    let tool_definitions: Vec<serde_json::Value> =
        tool_specs.iter().map(|(_, v)| v.clone()).collect();

    // Full builtin specs: every builtin regardless of enabled (used by
    // the frontend ToolsTab — `GET /api/agents/{id}/builtin-tools`).
    let full_tool_specs: Vec<(String, serde_json::Value)> = active_tools
        .iter()
        .map(|e| {
            let spec = e.spec();
            let serialized = serde_json::to_value(&spec).unwrap_or_default();
            (spec.name.clone(), serialized)
        })
        .collect();
    tracing::info!(
        active_specs = tool_specs.len(),
        full_specs = full_tool_specs.len(),
        "Tool specs: enabled vs full builtin registry"
    );
    // Note: `tool_definitions` (flat JSON Vec for the LLM) is no longer
    // propagated to `AgentBootContext`. New sessions now derive their
    // initial `ContextBuilder.tool_definitions` live from
    // `core.builtin_tools` inside `SessionTask::new`, so a pre-baked
    // snapshot here would diverge from the hot-reloaded dispatch list.

    // ── Step 6: Build context builder ───────────────────────────────
    // ADR-042: User identity is delivered via the `acowork/global/user_profile`
    // retained MQTT topic (subscribed as part of `acowork/global/#` in the
    // bootstrap). Wait briefly for the first snapshot to arrive so the
    // compact model's language hint is populated from the start; on
    // timeout (Gateway not running yet, no user created, etc.), fall
    // back to None — the compaction prompt's detection-based fallback
    // (ADR-011 v6) handles this gracefully.
    let identity_context: Option<String> = match available_cache.as_ref() {
        Some(cache) => {
            // ADR-039 bootstrap has already received the ConnAck; the
            // cached retained snapshot should be there. If not (race
            // with broker publish), poll briefly.
            //
            // Break as soon as the retained envelope has landed —
            // `active_user` being absent (no user created yet) is a
            // definitive answer, not a race. Waiting on it cost ~5s
            // per agent boot when the gateway had no active user.
            // ponytail: 200ms ceiling covers broker-subscribe race; for
            // late-arriving users, runtime.rs handles via
            // `identity_update_tx` (mqtt/client.rs).
            let start = std::time::Instant::now();
            let timeout = std::time::Duration::from_millis(200);
            let identity = loop {
                {
                    let cache_read = cache.read().await;
                    if cache_read.user_profile.is_some() {
                        break cache_read.active_user_profile().map(|profile| {
                            // Boot-time identity build runs before
                            // `apply_runtime_config_override` seeds the
                            // per-agent `session_language_override` into
                            // AgentCore, so the override is unknown here.
                            // The post-boot `update_user_identity` push
                            // (triggered by the next Gateway
                            // `UserProfileUpdate`) will re-format with
                            // the override if set. See
                            // `SessionManager::apply_runtime_config_override`
                            // + `update_user_identity`.
                            crate::agent::session::session_manager::format_user_profile_context(
                                &profile, None,
                            )
                        });
                    }
                }
                if start.elapsed() >= timeout {
                    break None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            };
            tracing::info!(
                has_identity = identity.is_some(),
                ctx_len = identity.as_ref().map(|s| s.len()).unwrap_or(0),
                waited_ms = start.elapsed().as_millis() as u64,
                "Initial identity_context built from acowork/global/user_profile"
            );
            identity
        }
        None => {
            tracing::warn!(
                "available_cache not available; identity_context is None (compaction will use detection-based fallback)"
            );
            None
        }
    };

    let mut context_builder = ContextBuilder::new(prompt_parts.base.clone())
        .with_identity(identity_context.clone())
        .with_tools(tool_definitions.clone());
    context_builder = context_builder.with_override_model(resolved_model.clone());
    // ADR-094: Progressive-mode skills summary lands on its OWN
    // ContextBuilder section (`skill_instructions`), parallel to
    // `tool_definitions`. This is what makes `compute_section_sizes`
    // emit a distinct section entry — visible in both the usage popover
    // (skills category %) and the Debug panel snapshot. Without this
    // call, the summary would be lost (it used to be embedded in the
    // base string, which collapsed everything into the `system_prompt`
    // section and hid the skills from the UI).
    if let Some(ref skills) = prompt_parts.skills_summary {
        context_builder.set_skill_summary(skills.clone());
    }

    tracing::info!(
        provider = %provider.name(),
        model = %resolved_model,
        available_count = available_models.len(),
        "Final model selection after per-agent preference resolution"
    );

    // ── Step 7: Create budget ────────────────────────────────────────
    let budget = acowork_core::Budget {
        daily_tokens: None,
        monthly_tokens: None,
        daily_cost_usd: None,
        monthly_cost_usd: None,
        exceeded_action: "warn".to_string(),
    };

    // ── Step 8: Create chunk channel ──────────────────────────────────
    // ADR-021: Single channel for control events only.
    // Data events (Delta, ReasoningDelta, ToolCall, ToolResult) are no longer
    // pushed via channel — the frontend polls them via HTTP.
    let (chunk_tx, chunk_rx) = if mqtt_client.is_some() {
        let (tx, rx) = tokio::sync::mpsc::channel::<crate::agent::loop_::SessionChunkEvent>(
            config.data_flow.chunk_capacity,
        );
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    // ── Capture reconnect params for Gateway mode ───────────────────
    let agent_id = config.agent_id.clone();

    Ok(AgentBootContext {
        loaded,
        mqtt_client,
        available_cache,
        control_rx,
        identity_update_rx,
        provider_update_rx,
        embedding_update_rx,
        lsps_update_rx,
        runtime_http_port,
        provider,
        resolved_model,
        available_models,
        protocol_type,
        gateway_current_provider_id,
        compat_cache,
        emb_provider,
        active_tools,
        full_tool_specs,
        system_prompt: prompt_parts.base.clone(),
        // ADR-094: Progressive-mode skills summary is stored on the
        // boot context so Phase B (SessionManagerConfig + per-session
        // ContextBuilder) seeds it. Without this, the summary would
        // only exist on the boot-time context_builder and be lost when
        // SessionManager re-builds the system prompt for hot-reload.
        skills_summary: prompt_parts.skills_summary.clone(),
        compaction_prompt,
        // Per-turn skill command injection: registry loaded in Step 3.5
        // above, injected into AgentCore in Phase B (session_init.rs).
        skill_registry,
        // ADR-063: 4 additional package-level overrides (Phase A loaded
        // each from `prompts/<file>.md`; see load_or_trace above). Wired
        // through `AgentBootContext` so Phase B's session_init.rs can
        // inject them into `AgentCore` as `Arc<RwLock<Option<String>>>`.
        //
        // ADR-068: 3 grafeo-specific overrides (extraction /
        // conflict-classification / generalization) removed — see the
        // matching comment in `agent_core.rs` for rationale.
        compact_template,
        title_prompt,
        abstention_prompt,
        chat_persona,
        mcp_notifier,
        workspace_resolver,
        rag_provider,
        context_builder: Some(context_builder),
        identity_context,
        chunk_tx,
        chunk_rx,
        budget,
        provider_config,
        session_snapshots,
        latest_session,
        agent_id,
        http_dispatch_rx: Some(http_dispatch_rx),
        degraded_reasons,
        // Same Arc as the local `mqtt_client_slot` above — both Phase C
        // (subsystems) and the runtime `/api/debug/enable` route read
        // through `ctx.mqtt_client_slot`.
        mqtt_client_slot: mqtt_client_slot.clone(),
        agent_core_shared,
        session_metadata_slot,
        conversation_index_slot,
        workspace_query_slot,
        workspace_mutation_slot,
        git_query_slot,
        agent_tools_slot,
        agent_config_slot,
        attachment_slot,
        session_config_slot,
        rag_provider_slot,
        debug_service_slot,
        session_manager_slot,
        // ADR-058: same Arc as the one cloned into the HTTP server —
        // Phase C reads it via `ctx.workspace_watcher_set`.
        workspace_watcher_set,
        session_configs,
    })
}

/// Return a noop provider tuple for fallback cases where no provider list
/// is available from Gateway or MQTT cache.
fn noop_provider_tuple() -> (
    std::sync::Arc<dyn acowork_core::providers::traits::Provider>,
    String,
    Vec<String>,
    ProtocolType,
) {
    (
        crate::providers::router::create_noop_provider(),
        "no-model".to_string(),
        vec![],
        ProtocolType::OpenAI,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::registry::ToolRegistry;

    /// G2: Verify that when a manifest declares a RAG tool, the
    /// `rag_config()` extraction, `RagClientConfig::from_manifest`,
    /// `HttpRagProvider::new`, `RagQueryTool::new`, and the registry
    /// registration chain work end-to-end.
    ///
    /// This tests the exact code path in `build_builtin_registry`
    /// (lines 535-612) without needing the full agent_init
    /// infrastructure (MQTT, vault, workspace, etc.).
    #[test]
    fn test_rag_tool_registration_from_manifest() {
        let toml_str = r#"
            agent_id = "com.example.sales"
            version = "1.0.0"
            name = "Sales Assistant"
            description = "Enterprise sales agent with RAG"
            author = "corp"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"

            [[tools]]
            type = "rag"
            name = "enterprise_knowledge"

            [tools.rag]
            endpoint = "https://rag.corp.example.com/v1/query"
            collection = "product_docs"
            auth_ref = "vault:rag_enterprise_key"
            auth_type = "bearer"
            max_results = 5
            score_threshold = 0.7
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
        assert!(manifest.has_rag());

        // Extract RAG config from manifest (same as agent_init.rs line 551).
        let (tool_name, rag_config) = manifest.rag_config().unwrap();
        assert_eq!(tool_name, "enterprise_knowledge");
        assert_eq!(rag_config.endpoint, "https://rag.corp.example.com/v1/query");

        // Build RagClientConfig (same as agent_init.rs lines 597-601).
        let auth = crate::tools::rag::client::RagAuthCredential::from_vault_ref(
            rag_config.auth_ref.as_deref(),
            &rag_config.auth_type,
            None, // no key resolved (simulates MQTT race)
        );
        let rag_client_config = crate::tools::rag::client::RagClientConfig::from_manifest(
            rag_config,
            tool_name.to_string(),
            auth,
        );

        // Construct HttpRagProvider (same as agent_init.rs line 602).
        let provider = Arc::new(crate::tools::rag::client::HttpRagProvider::new(
            rag_client_config,
        ));

        // Register RagQueryTool in registry (same as agent_init.rs lines 605-606).
        let mut registry = ToolRegistry::new();
        let rag_tool = crate::tools::builtin::rag_query::RagQueryTool::new(provider.clone());
        registry.register(Arc::new(rag_tool) as Arc<dyn acowork_core::tools::traits::Tool>);

        // G8: Verify the tool is discoverable in the registry by name.
        let found = registry.get("rag_query");
        assert!(
            found.is_some(),
            "rag_query must be in the registry after registration"
        );

        // Verify the tool spec has the right name and input schema.
        let tool = found.unwrap();
        assert_eq!(tool.name(), "rag_query");
        let spec = tool.spec();
        assert!(spec.input_schema["properties"]["query"].is_object());
        assert!(spec.input_schema["properties"]["top_k"].is_object());
        assert!(
            spec.input_schema["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("query"))
        );
    }

    /// G2b: Verify that a manifest WITHOUT RAG declaration produces
    /// `rag_config() == None`, and the registration path is skipped
    /// (no rag_query tool in the registry).
    #[test]
    fn test_no_rag_tool_when_manifest_has_no_rag() {
        let toml_str = r#"
            agent_id = "com.test.basic"
            version = "1.0.0"
            name = "Basic Agent"
            description = "No RAG"
            author = "test"
            runtime_version = "0.1.0"

            [llm]
            provider = "openai"
            model = "gpt-4"
        "#;
        let manifest = acowork_core::AgentManifest::from_toml(toml_str).unwrap();
        assert!(!manifest.has_rag());
        assert!(manifest.rag_config().is_none());

        // In agent_init.rs, rag_provider would be None and no tool registered.
        let registry = ToolRegistry::new();
        assert!(
            registry.get("rag_query").is_none(),
            "rag_query should not exist without RAG manifest"
        );
    }

    /// G8b: Verify that the registered RAG tool's spec name matches
    /// what the LLM would see in the tool list. The LLM invokes tools
    /// by their `name()` - if the name is wrong, the tool is invisible.
    #[test]
    fn test_rag_tool_spec_name_matches_registry_lookup() {
        let rag_config = acowork_core::RagToolConfig {
            endpoint: "https://rag.example.com/v1/query".to_string(),
            collection: Some("test_docs".to_string()),
            auth_ref: None,
            auth_type: "bearer".to_string(),
            max_results: 5,
            score_threshold: 0.7,
            timeout_secs: 10,
        };
        let client_config = crate::tools::rag::client::RagClientConfig::from_manifest(
            &rag_config,
            "enterprise_knowledge".to_string(),
            crate::tools::rag::client::RagAuthCredential::None,
        );
        let provider = Arc::new(crate::tools::rag::client::HttpRagProvider::new(
            client_config,
        ));
        let rag_tool = crate::tools::builtin::rag_query::RagQueryTool::new(provider);

        // The tool's name() must be "rag_query" - this is what the LLM sees.
        let tool: Arc<dyn acowork_core::tools::traits::Tool> = Arc::new(rag_tool);
        assert_eq!(tool.name(), "rag_query");

        // The spec name must also be "rag_query".
        assert_eq!(tool.spec().name, "rag_query");
    }
}

