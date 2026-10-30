//! Phase B: per-session initialization (Gateway mode).
//!
//! Covers Steps 2.5 + session-related parts of Step 9:
//!   - Initialize conversation session (resume latest or create new)
//!   - Validate provider/model against cached provider list
//!   - Build AgentCore + inject provider list, key vault, memory
//!   - Create SessionManager + set resolver + set default workspace
//!   - Create initial session with resumed/created conversation
//!   - SessionState is assembled with the persisted provider; no extra
//!     ModelSwitch message is needed at startup
//!   - Apply workspace context and runtime overrides

use std::sync::Arc;

use crate::agent::agent_core::AgentCore;
use crate::agent::session::session_manager::RuntimeConfigOverrides;
use crate::agent::session::{SessionManager, SessionManagerConfig};
use crate::config::RuntimeConfig;
use crate::error::Result;
use crate::startup::context::{AgentBootContext, SessionBootContext, build_session_manager_config};
use acowork_core::timeout_config::constants;

/// Phase B: assemble per-session state on the main thread (Gateway mode).
///
/// This must complete synchronously (no spawn) before Phase C so that
/// the full `SessionState` is ready before `SessionTask` starts.
pub(crate) async fn phase_b_init_session(
    ctx: &mut AgentBootContext,
    config: &RuntimeConfig,
) -> Result<SessionBootContext> {
    let _span = tracing::info_span!("startup_phase_b").entered();

    let work_dir_path = std::path::Path::new(&config.work_dir);

    // ── Step 2.5: Initialize conversation session ───────────────────
    let conversations_dir = work_dir_path.join("conversations");
    std::fs::create_dir_all(&conversations_dir)?;

    // ADR-022: Shared counter updated by writer thread after each disk write.
    let committed_lines = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // ADR-024: load agent_config.json early so max_sessions override is available.
    let mut agent_cfg = crate::agent_config::load_agent_config(work_dir_path)
        .unwrap_or_default()
        .unwrap_or_default();

    // ADR-033 + Bug B fix v4: resolve the initial model/provider from
    // `available_cache` BEFORE creating a new conversation. Without this
    // a fresh session persisted `model = None, provider = None`, and the
    // session_task would later bind a noop provider for the entire
    // session lifetime (Bug B: first chat after onboarding failed with
    // "unexpected error").
    //
    // The pull loop in `agent_init::phase_a` blocks until either the
    // Gateway returns 200 + a non-empty `AvailableProviders` snapshot,
    // or the pull deadline (PULL_MAX_DURATION) elapses. By the time we
    // reach here the cache should normally contain a usable provider;
    // the `None` fallback below is only hit on Gateway failure / on the
    // cold-start race where the Gateway is still booting when the
    // Runtime spawns (the active pull then keeps retrying and the
    // next session creation picks up the cache automatically).
    let cache_initial_provider_model: Option<(String, String)> =
        if let Some(ref cache) = ctx.available_cache {
            // Try once with a short grace period — the pull in phase_a
            // usually already populated the cache, but in pathological
            // startup races (Runtime spawned before phase_a even
            // started the pull) we still want to give it a chance.
            // Bounded so a misbehaving Gateway cannot block Phase B.
            let cache_grace = std::time::Duration::from_secs(2);
            let grace_start = std::time::Instant::now();
            loop {
                {
                    let cache_read = cache.read().await;
                    if let Some(ref available) = cache_read.providers
                        && let Some(found) = available.providers.iter().find_map(|p| {
                            if p.api_key.is_empty() {
                                None
                            } else {
                                p.models.first().map(|m| (p.id.clone(), m.id.clone()))
                            }
                        })
                    {
                        break Some(found);
                    }
                }
                if grace_start.elapsed() >= cache_grace {
                    break None;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        } else {
            None
        };
    if let Some((ref pid, ref mid)) = cache_initial_provider_model {
        tracing::info!(
            provider_id = %pid,
            model_id = %mid,
            "Resolved initial session model/provider from available_cache"
        );
    } else {
        tracing::warn!(
            "available_cache has no usable provider when session_init runs; \
             new session will persist model=None, provider=None (legacy fallback)"
        );
    }

    let conversation_session = if let Some(latest_id) =
        crate::conversation::find_latest_session(&conversations_dir)
    {
        tracing::info!(session_id = %latest_id, "Resuming latest conversation session");
        match crate::conversation::ConversationSession::resume(
            work_dir_path,
            &latest_id,
            committed_lines.clone(),
        ) {
            Ok((conv, config_rx, state_rx)) => {
                // ADR-043: Spawn config + state change relays so
                // persisted-session updates flow through MQTT to the
                // Desktop.
                if let Some(chunk_tx) = ctx.chunk_tx.clone() {
                    crate::startup::subsystems::spawn_config_change_relay(
                        config_rx,
                        chunk_tx.clone(),
                        conv.clone(),
                        latest_id.clone(),
                        // agent_core_shared is populated later in
                        // Phase B (after AgentCore construction +
                        // injection). The relay reads it lazily on
                        // each config change event, so by the time
                        // a user action triggers a push the slot is
                        // already filled.
                        ctx.agent_core_shared.clone(),
                    );
                    crate::startup::subsystems::spawn_state_change_relay(
                        state_rx,
                        chunk_tx,
                        conv.clone(),
                        latest_id.clone(),
                    );
                }
                Some(conv)
            }
            Err(e) => {
                let msg = format!(
                    "session_persistence_unavailable: cannot resume session \"{}\" in {:?}: {}",
                    latest_id, conversations_dir, e
                );
                eprintln!(
                    "⚠️ {}; runtime will run with an in-memory session (history will be lost on restart)",
                    msg
                );
                if let Ok(mut reasons) = ctx.degraded_reasons.write() {
                    reasons.push(msg);
                }
                None
            }
        }
    } else {
        let new_id = crate::conversation::generate_session_id();
        tracing::info!(session_id = %new_id, "Creating new conversation session");
        // Use the cache-resolved values when present (Bug B fix v4);
        // fall back to None so the legacy validation / noop path
        // takes over on Gateway failure.
        let (initial_model, initial_provider) = match cache_initial_provider_model {
            Some((pid, mid)) => (Some(mid), Some(pid)),
            None => (None, None),
        };
        let (conv, config_rx, state_rx) = crate::conversation::ConversationSession::new(
            work_dir_path,
            &new_id,
            crate::conversation::SessionConfig {
                agent_id: config.agent_id.clone(),
                workspace_id: None,
                model: initial_model,
                provider: initial_provider,
            },
            agent_cfg.max_sessions.unwrap_or(config.max_sessions),
            committed_lines.clone(),
        )?;
        // Drop the meta-change receiver — new-session creation writes
        // the initial meta file, but no subscriber cares about it yet
        // (the session_task spawns its own relay after this returns to
        // the SessionManager path). Leaving the receiver unhandled
        // here would silently buffer `UnboundedSender`s and prevent
        // Drop-time notifications from being observed. Dropping is
        // the explicit "I don't care" signal.
        drop(config_rx);
        drop(state_rx);
        Some(conv)
    };

    // ADR-033 (MQTT path): pre-compute the set of provider IDs that have a
    // decrypted API key in the cached `AvailableProviders` payload. This is
    // the MQTT-path counterpart of the legacy gRPC `provider_key_vault`.
    let available_cache_provider_ids: std::collections::HashSet<String> =
        if let Some(ref cache) = ctx.available_cache {
            let cache_read = cache.read().await;
            cache_read
                .providers
                .as_ref()
                .map(|available| {
                    available
                        .providers
                        .iter()
                        .filter(|p| !p.api_key.is_empty())
                        .map(|p| p.id.clone())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            std::collections::HashSet::new()
        };

    // Validate the resumed session's model/provider against the cached provider list.
    if let Some(ref conv) = conversation_session {
        let session_model = conv.model();
        let session_provider = conv.provider();

        let is_valid = match (&session_model, &session_provider) {
            (Some(model), Some(provider_id)) => {
                let in_cache = ctx
                    .provider_config
                    .as_ref()
                    .map(|c| &c.providers)
                    .is_none_or(|providers| {
                        providers.iter().any(|p| {
                            p.id == *provider_id && p.models.iter().any(|m| m.id == *model)
                        })
                    });
                if !in_cache {
                    false
                } else {
                    // Two independent sources can authorize a provider_id:
                    //   1. Phase A default (`gateway_current_provider_id`) —
                    //      the first provider with a key in the MQTT cache.
                    //   2. MQTT `available_cache` (ADR-033) — keys
                    //      are shipped inline per `mqtt.md` §3.1.1.
                    ctx.gateway_current_provider_id.as_deref() == Some(provider_id.as_str())
                        || available_cache_provider_ids.contains(provider_id)
                }
            }
            _ => true,
        };

        if !is_valid {
            let fallback_model = ctx
                .provider_config
                .as_ref()
                .map(|c| &c.providers)
                .and_then(|p| p.first())
                .and_then(|p| p.models.first())
                .map(|m| m.id.clone());

            if let Some(ref fallback) = fallback_model {
                tracing::warn!(
                    session_id = %conv.session_id(),
                    invalid_model = ?session_model,
                    invalid_provider = ?session_provider,
                    fallback = %fallback,
                    "Session model/provider invalid, falling back"
                );
                conv.update_model_provider(fallback, None);
            }
        }
    }

    // Spawn background session scan.
    // ADR-085: the result (latest session by `last_active_at`) is seeded
    // straight into the shared `latest_session` Arc the HTTP server and
    // SessionManager already read. Previously it landed in a separate
    // slot that Phase B consumed exactly once — a scan finishing after
    // that read was silently dropped and `/latest-session` answered
    // 404 forever while sessions existed on disk (the orphan-session
    // seed, paired with the D7 removal).
    let latest_session_scan = ctx.latest_session.clone();
    let conversations_dir_clone = conversations_dir.clone();
    let _session_scan_handle = tokio::spawn(async move {
        let handle = crate::conversation::scan_sessions_async(
            conversations_dir_clone,
            None,
            None,
            crate::conversation::SessionScope::Unfiltered,
        );
        let (sessions, _, _agent_totals) = handle.await.unwrap_or((Vec::new(), 0, (0, 0, 0, 0)));
        if let Some(s) = sessions.first() {
            let pair = (s.session_id.clone(), s.title.clone());
            // Don't clobber a session the resume path already seeded (same
            // lookup, but ordering between the two writers is not fixed).
            let mut g = latest_session_scan.write().unwrap();
            if g.is_none() {
                *g = Some(pair);
            }
            drop(g);
        }
        tracing::info!(count = sessions.len(), "Background session scan complete");
    });

    // ── Step 9 (Gateway mode): Build AgentCore ───────────────────────
    let provider = ctx.provider.clone();
    let active_tools = ctx.active_tools.clone();

    let mut core = Arc::new(AgentCore::new(
        config.clone(),
        ctx.loaded.manifest.clone(),
        provider,
        active_tools,
    ));

    // ADR-078 follow-up: bind the git-refresh nudge handles. Both Phase-A
    // resources exist by now (the MQTT client slot is passed into the HTTP
    // server in Phase A, and `workspace_resolver` is built before it), so
    // this is the first point where a shell tool call can actually
    // publish a `git-changed` event.
    Arc::get_mut(&mut core)
        .expect("AgentCore must be unshared at construction")
        .set_git_nudge(crate::agent::git_nudge::GitNudgeSlots {
            mqtt_slot: ctx.mqtt_client_slot.clone(),
            resolver: ctx.workspace_resolver.clone(),
        });

    // ADR-061 §13.4: warn (not reject) when the boot model's effective
    // input budget cannot run the 8-level compression loop. Warning only —
    // a hard rejection here would block agent startup entirely (e.g.
    // onboarding with a small-window model); `model_switch` stays a hard
    // rejection so the user gets a clear error when actively picking one.
    {
        let boot_model = conversation_session.as_ref().and_then(|c| c.model());
        if let Some(model) = boot_model
            && let Some(caps) = core.get_model_capabilities(&model)
            && let Err(e) = crate::agent::compression_constants::validate_model_budget(&caps)
        {
            tracing::warn!(
                model = %model,
                error = %e,
                "ADR-061: boot model below MIN_BUDGET_FOR_AGENT (compression loop will not run efficiently)"
            );
        }
    }

    // ADR-046: build the attachment blob store up front so we can both
    // inject it into `AgentCore` (so SessionTask's image-derivation
    // pipeline can read image bytes — see `attachment_to_image.rs`) and
    // publish it to the HTTP server's `attachment_slot` (so
    // `POST /sessions/{sid}/files` and `GET /files/{document_id}` work).
    // `RuntimeAttachmentService` only needs the boot-time `work_dir`;
    // no async resource dependency, so the construction is sync.
    let attach_svc: Arc<dyn crate::usecases::AttachmentService> = Arc::new(
        crate::usecases::RuntimeAttachmentService::new(work_dir_path.to_path_buf()),
    );

    // ADR-104: project the package's `[[mcp_servers]]` into
    // `agent_mcp.json`'s `recommended` list before anything reads that file.
    // The list is derived state (rewritten every boot), and it is what the
    // Tools panel renders as "this agent suggests" plus what seeds
    // `agent_mcp_tools.json`'s per-server defaults. A failure here must not
    // block startup: the agent runs fine without its recommendations.
    if let Err(e) = crate::agent_config::project_recommended_mcp_servers(
        work_dir_path,
        &ctx.loaded.manifest,
    ) {
        tracing::warn!(error = %e, "Failed to project manifest [[mcp_servers]] into agent_mcp.json");
    }

    // ADR-101: the agent's MCP catalog (pushed by the Gateway) decides whether
    // the official memory service is wired for this agent. Read once here; the
    // manifest hand-off below keys on it and nothing else.
    let catalog_mcps = crate::agent_config::load_agent_mcp_config(work_dir_path)
        .ok()
        .flatten()
        .map(|cfg| cfg.catalog)
        .unwrap_or_default();

    // Inject global provider list, key vault, and memory into AgentCore.
    if let Some(c) = Arc::get_mut(&mut core) {
        // Inject the per-agent compatibility cache so `build_provider_for`
        // can wire it into rebuilt providers (429-retry / session-resume).
        c.compat_cache = ctx.compat_cache.take();
        // ADR-103 D8/S7: the Responses trim cache travels with it.
        c.responses_compat_cache = ctx.responses_compat_cache.take();

        // Agent-specific compaction prompt (prompts/summary.md, optional).
        // Loaded once in Phase A (see `AgentBootContext::compaction_prompt`)
        // so Gateway and Standalone modes share the same resolution; `None`
        // (no file) means the built-in COMPACTION_SYSTEM_PROMPT fallback is
        // used at compaction time.
        //
        // ADR-063 §3.7.5: wrap in `Arc<RwLock<Option<String>>>` so the
        // L2 reload (`DebugService::reload_prompts`) can write through
        // any Arc<AgentCore> clone held by a running session. Phase B
        // is the single injection point for Gateway mode — see
        // `cli.rs` (Standalone mode) for the symmetric Standalone path.
        *c.compaction_prompt.write().unwrap() = ctx.compaction_prompt.clone();

        // ADR-063: 7 additional package-level prompt overrides. Phase A
        // loaded each `prompts/<file>.md` into the matching field on
        // `AgentBootContext`; here we mirror the Phase B injection
        // pattern (RwLock write — Phase B is the only writer before
        // sessions are spawned, so no contention with the LLM call
        // sites' read guards). `None` is the normal "no override" state.
        *c.compact_template.write().unwrap() = ctx.compact_template.clone();
        *c.title_prompt.write().unwrap() = ctx.title_prompt.clone();
        *c.abstention_prompt.write().unwrap() = ctx.abstention_prompt.clone();

        // ADR-099: chat router persona (prompts/chat.md). Read only by the
        // chat-layer prompt builder; never folded into the execute prompt.
        *c.chat_persona.write().unwrap() = ctx.chat_persona.clone();

        // Provider list is loaded from agent_provider.json (persisted by the
        // MQTT handler on receiving acowork/global/providers).
        let providers_for_init = ctx.provider_config.as_ref().map(|c| &c.providers);

        if let Some(providers) = providers_for_init {
            {
                let mut pcm = c.provider_compact_models.write().unwrap();
                for p in providers {
                    pcm.insert(p.id.clone(), p.compact_model.clone());
                }
            }
            {
                let mut list = c.global_provider_list.write().unwrap();
                *list = providers.clone();
            }
            tracing::info!(
                provider_count = providers.len(),
                compact_count = c.provider_compact_models.read().unwrap().len(),
                "Populated AgentCore.global_provider_list from resource cache"
            );
        }

        // ADR-056: Surface the global compact-model candidate list from
        // `AgentProviderConfig` (mirrors `AvailableProviders.default_compact_models`)
        // into `AgentCore.default_compact_models`. The MQTT path is the
        // authoritative source for runtime sessions; the on-disk field is
        // the cold-start cache populated by the MQTT handler.
        if let Some(cfg) = ctx.provider_config.as_ref()
            && !cfg.default_compact_models.is_empty()
        {
            *c.default_compact_models.write().unwrap() = cfg
                .default_compact_models
                .iter()
                .map(|cm| (cm.provider_id.clone(), cm.model_id.clone()))
                .collect();
            tracing::info!(
                count = c.default_compact_models.read().unwrap().len(),
                "Populated AgentCore.default_compact_models from resource cache"
            );
        }

        // ADR-033: MQTT available_cache is the only source of provider API keys
        // (gRPC hello_config path removed per ADR-040). The available cache
        // is updated by the Runtime MQTT event loop whenever the Gateway
        // publishes `acowork/global/providers` (retained). Each `ProviderRef`
        // carries the decrypted API key inline (see mqtt.md §3.1.1) so the
        // model_switch path can look up the key by provider_id.
        if let Some(ref cache) = ctx.available_cache {
            let cache_read = cache.read().await;
            if let Some(available) = cache_read.providers.as_ref() {
                let mut vault = c.provider_key_vault.write().unwrap();
                vault.clear();
                for p in &available.providers {
                    if !p.api_key.is_empty() {
                        vault.entry(p.id.clone()).or_default().push(
                            acowork_core::protocol::ProviderKeyEntry {
                                provider_id: p.id.clone(),
                                account_id: p.account_id.clone(),
                                api_key: p.api_key.clone(),
                            },
                        );
                    }
                }
                tracing::info!(
                    version = available.version,
                    provider_count = available.providers.len(),
                    key_count = vault.len(),
                    "Populated AgentCore provider_key_vault from MQTT available cache"
                );
            }
        }

        c.rag_provider = ctx.rag_provider.take();
        // Through the setter, not by assignment: assignment updates this clone
        // only, and the cell is what the conversation indexer and the HTTP
        // layer read. A provider bound here but not there leaves the two halves
        // of the runtime embedding with different models.
        if let Some(provider) = ctx.emb_provider.clone() {
            c.update_embedding_provider(provider);
        }
        // Per-turn skill command injection: carry the Phase A SkillRegistry
        // (skills/*/SKILL.md) on AgentCore so `dispatch_inbound` can resolve
        // a chat `command` (skill name) into instructions — the runtime is
        // the authoritative source, the frontend only sends names.
        c.skill_registry = Arc::new(std::sync::RwLock::new(ctx.skill_registry.clone()));
        // ADR-101: open the Runtime-local SQLite store. It backs only session
        // metadata + the conversation vector index now — memory nodes belong
        // to the memory service.
        c.init_runtime_sqlite(work_dir_path);

        // ADR-101 decision 12: manifest hand-off to the memory service.
        // Keyed on the MCP catalog only — no official `memory` server means
        // no coupling at all. Fire-and-forget: agent startup must not block
        // on (or fail with) the service, which may still be booting.
        if let Some(base) = crate::agent::bootstrap_autobio::service_base(&catalog_mcps) {
            let client = reqwest::Client::new();
            let instance_id = config.agent_instance_id.clone();
            let node_token = config.mqtt_password.clone().unwrap_or_default();
            let manifest = ctx.loaded.manifest.clone();
            tokio::spawn(async move {
                crate::agent::bootstrap_autobio::import_manifest(
                    &client,
                    &base,
                    &instance_id,
                    &node_token,
                    &manifest,
                )
                .await;
            });
        }

        // ADR-081 §4.2 (P1-2): open the conversation vector index and
        // publish it to the late-bind slot, then spawn the tailer. The
        // tailer reads the live embedding provider from `agent_core_shared`
        // each sweep, so it picks up the provider whenever it binds. The
        // index is rebuildable from the JSONL history — delete + restart
        // rebuilds it.
        {
            // ADR-082 §4 step 3: the index lives in the memory backend's
            // SQLite store, so there is no separate `conversation_index.sqlite`
            // to open and no dimension to pass — the store remembers its own.
            // No store means memory init failed (already logged there):
            // conversation search stays off rather than silently writing to a
            // second, orphan file.
            let opened = match c.sqlite_store() {
                Some(store) => {
                    crate::conversation_index::ConversationIndex::from_store(store, work_dir_path)
                        .map(Some)
                }
                None => Ok(None),
            };
            match opened {
                Ok(Some(index)) => {
                    let index = Arc::new(index);
                    if let Ok(mut slot) = ctx.conversation_index_slot.write() {
                        *slot = Some(index.clone());
                    } else {
                        tracing::warn!(
                            "Failed to publish conversation_index_slot — /search conversation scope unavailable"
                        );
                    }
                    let indexer = Arc::new(crate::conversation_index::ConversationIndexer::new(
                        index,
                        work_dir_path,
                        ctx.agent_core_shared.clone(),
                    ));
                    tokio::spawn(async move { indexer.run().await });
                    // No store path here: it is already reported — with the
                    // real path — by `ConversationIndex::from_store` just
                    // above.
                    tracing::info!("conversation index: opened, tailer spawned");
                }
                Ok(None) => {
                    tracing::error!(
                        "conversation index: no memory store — conversation search disabled"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "conversation index: failed to open store — conversation search will report not_indexed"
                    );
                }
            }
        }

        // ADR-040: Publish workspace query + mutation services. Workspace
        // services only need the agent's `work_dir` (resolved at boot);
        // they don't depend on async resources like memory_store, so we
        // can wire them immediately. Both services must be published
        // BEFORE the workspace HTTP handlers can serve a real response.
        {
            let query_svc: Arc<dyn crate::usecases::WorkspaceQueryService> = Arc::new(
                crate::usecases::RuntimeWorkspaceQueryService::new_with_package_dir(
                    work_dir_path.to_path_buf(),
                    ctx.agent_id.clone(),
                    ctx.loaded.package_dir.clone(),
                ),
            );
            let mut slot = ctx.workspace_query_slot.lock().await;
            *slot = Some(query_svc);
        }
        {
            let mutation_svc: Arc<dyn crate::usecases::WorkspaceMutationService> = Arc::new(
                crate::usecases::RuntimeWorkspaceMutationService::new_with_package_dir(
                    work_dir_path.to_path_buf(),
                    ctx.loaded.package_dir.clone(),
                ),
            );
            let mut slot = ctx.workspace_mutation_slot.lock().await;
            *slot = Some(mutation_svc);
        }
        {
            // ADR-078: git query service — same sync-work_dir pattern as
            // the workspace services above. Executes read-only git CLI
            // commands inside the Runtime (workspace owner); the Gateway
            // only reverse-proxies `/git/*`. Must be published before the
            // git HTTP handlers can serve a real response.
            let git_svc: Arc<dyn crate::usecases::GitQueryService> =
                Arc::new(crate::usecases::RuntimeGitQueryService::new(
                    work_dir_path.to_path_buf(),
                    ctx.agent_id.clone(),
                ));
            let mut slot = ctx.git_query_slot.lock().await;
            *slot = Some(git_svc);
        }

        // ADR-040 follow-up: Publish Tools-panel persistence service.
        // The `/agents/{id}/mcp-servers` HTTP handlers route through this
        // trait. The service holds only the agent's `work_dir` (sync, no
        // async dependency), so it can be wired immediately alongside the
        // workspace services.
        {
            let tools_svc: Arc<dyn crate::usecases::AgentToolsService> = Arc::new(
                crate::usecases::RuntimeAgentToolsService::new(work_dir_path.to_path_buf()),
            );
            let mut slot = ctx.agent_tools_slot.lock().await;
            *slot = Some(tools_svc);
        }

        // ADR-040 follow-up: Publish per-agent runtime config service.
        // The `PUT /agents/{id}/config` handler routes through this trait.
        // Same sync-work_dir pattern as `agent_tools_slot` above.
        {
            let config_svc: Arc<dyn crate::usecases::AgentConfigService> = Arc::new(
                crate::usecases::RuntimeAgentConfigService::new(work_dir_path.to_path_buf()),
            );
            let mut slot = ctx.agent_config_slot.lock().await;
            *slot = Some(config_svc);
        }

        // ADR-046: Inject attachment blob store into AgentCore so that
        // `SessionTask` can derive multimodal image parts from
        // `AttachedItem::ImageUpload` (see
        // `crate::agent::attachment_to_image::derive_image_parts`).
        // Without this injection, the slot stays `None` and every
        // image-attached chat turn degrades to plain text. The HTTP
        // server sees the same Arc via `ctx.attachment_slot`, published
        // outside this `if let` block below.
        c.set_attachment_service(attach_svc.clone());

        // ── Resolve & persist agent_config.json defaults ─────────────
        //
        // agent_config.json is the single source of truth for the
        // frontend setup panel.  Resolve any field still None through
        // its fallback chain and persist the concrete value so the
        // frontend always sees a value instead of "empty = use default".
        //
        // Fields intentionally NOT auto-resolved:
        //   system_prompt_override — None = "use compiled manifest prompt"
        //   max_output_tokens      — None = "use each model's native limit"
        //   avatar / display name  — NOT part of this file at all; they are
        //     user preferences in `AgentOverrides` (ADR-009 §5), resolved by
        //     `http::avatar` as overrides > manifest.
        {
            let mut updated = agent_cfg.clone();
            let mut dirty = false;
            // ── manifest layer: one projection, never an overwrite ──
            // ADR-104: every manifest-authored knob reaches `agent_config.json`
            // through `config_defaults()`, and only into fields this file
            // leaves `None`. That is what makes the file the single source of
            // truth at request time: the manifest is read once, at seed time,
            // instead of being re-consulted by individual consumers
            // (temperature / context_window used to be seeded here while
            // `[llm].max_tokens` was read straight off the manifest inside the
            // request builder — two landing paths for one declaration).
            if updated.seed_from_manifest(&ctx.loaded.manifest.config_defaults()) {
                dirty = true;
            }

            // ── context_window: → 200K ──
            // Note: `Some(0)` (old "no limit") is invalid under ADR-074 §6 —
            // the resolution chain skips it and falls to
            // DEFAULT_CONTEXT_WINDOW. We still persist whatever the manifest
            // says; the chain does the filtering at resolve time.
            if updated.context_window.is_none() {
                updated.context_window = Some(crate::config::DEFAULT_CONTEXT_WINDOW);
                dirty = true;
            }
            c.context_window_override = updated.context_window;
            // ADR-061: compression ratio threshold — None = built-in default
            // (0.90). The manifest may seed it (above); there is no
            // config-level fallback, so no dirty-write here.
            c.compression_ratio_threshold = updated.compression_ratio_threshold;
            // Per-agent session language override — None = fall through to
            // the global `UserProfile.language` (no fallback chain, the
            // empty setting IS the default behaviour). Live reload fires
            // via `apply_runtime_config_override` + the next
            // `update_user_identity` push.
            c.session_language_override = updated.session_language.clone();

            // ── temperature: → 0.3 ──
            if updated.temperature.is_none() {
                updated.temperature = Some(crate::config::DEFAULT_TEMPERATURE);
                dirty = true;
            }
            c.temperature_override = updated.temperature;

            // ── max_output_tokens: no fallback (None = model native) ──
            // Seeded from the manifest above; the request builder treats it as
            // a *ceiling* over the Gateway's per-model capability, not as the
            // old manifest-level request hard override.
            c.max_output_tokens_override =
                updated.max_output_tokens.map(|v| v.min(u32::MAX as u64) as u32);
            c.temperature_override = updated.temperature;

            // ── max_iterations: RuntimeConfig.max_iterations (200) ──
            if updated.max_iterations.is_none() {
                updated.max_iterations = Some(config.max_iterations);
                dirty = true;
            }

            // ── max_sessions: RuntimeConfig.max_sessions (1000) ─────
            if updated.max_sessions.is_none() {
                updated.max_sessions = Some(config.max_sessions);
                dirty = true;
            }

            // ── shell_approval_threshold: config default ("medium") ─
            if updated.shell_approval_threshold.is_none() {
                updated.shell_approval_threshold = Some(config.shell_approval_threshold.clone());
                dirty = true;
            }

            // ── approval_timeout_secs: core constant (300) ──────────
            if updated.approval_timeout_secs.is_none() {
                updated.approval_timeout_secs = Some(constants::APPROVAL.as_secs());
                dirty = true;
            }
            c.approval_timeout_secs = updated.approval_timeout_secs;

            // ── ADR-101: distiller / forgetting knobs are service-owned ──
            // The old first-boot seed wrote manifest `[memory.distiller]`
            // defaults into `agent_config.json` so the Runtime's scheduler
            // could read them. Both the scheduler and the seed moved to the
            // memory service, which receives the manifest policy once via
            // `POST /admin/bootstrap` and then owns it.

            if dirty {
                if let Err(e) = crate::agent_config::save_agent_config(work_dir_path, &updated) {
                    tracing::warn!(
                        error = %e,
                        "Failed to persist resolved defaults to agent_config.json",
                    );
                } else {
                    tracing::info!("Resolved and persisted agent_config.json defaults",);
                }
            }
        }

        // ADR-046: Publish attachment blob store to the HTTP server's
        // late-bind slot. The same `Arc` instance is shared with
        // `AgentCore::attachment_service` (set above), so any upload
        // posted to `POST /sessions/{sid}/files` becomes visible to
        // `derive_image_parts` in the same session turn. Must run
        // AFTER the `if let Some(c)` block because the slot write
        // borrows `ctx` for `.lock().await`, which conflicts with the
        // mutable `Arc::get_mut` borrow.
        let mut slot = ctx.attachment_slot.lock().await;
        *slot = Some(attach_svc);
    }

    // Reload agent_cfg from disk to pick up resolved defaults that were
    // persisted inside the AgentCore init block above. The outer variable
    // must be current so the has_overrides / apply_runtime_config_override
    // section below sees every resolved field.
    agent_cfg = crate::agent_config::load_agent_config(work_dir_path)
        .unwrap_or_default()
        .unwrap_or_default();

    // ── Step 9: Create SessionManager ───────────────────────────────
    let session_manager_config: SessionManagerConfig = build_session_manager_config(ctx, config);

    // ADR-040: Construct usecase services before `core` is moved
    // into SessionManager.
    //
    // ADR-074 §11.1: hoisted so Phase B (after SessionManager is built)
    // can inject the idle-session `usage_recompute` callback into the
    // concrete service.
    // Assigned unconditionally inside the block below (never read as None
    // on the production path) — declared without an initializer so the
    // compiler can prove it is set before the Phase B use at the bottom.
    let session_config_impl: Option<Arc<crate::usecases::RuntimeSessionConfigService>>;
    {
        let core_clone = Arc::clone(&core);
        if let Ok(mut slot) = ctx.agent_core_shared.write() {
            *slot = Some(core_clone.clone());
        } else {
            tracing::warn!("Failed to publish AgentCore to shared slot");
        }

        // Build the AgentTokenService (thin wrapper around AgentCore).
        let agent_token: Arc<dyn crate::usecases::AgentTokenService> =
            Arc::new(crate::usecases::RuntimeAgentTokenService::new(core_clone));

        // Build the SessionMetadataService.
        let session_metadata: Arc<dyn crate::usecases::SessionMetadataService> =
            Arc::new(crate::usecases::RuntimeSessionMetadataService::new(
                work_dir_path.to_path_buf(),
                agent_token,
                ctx.session_snapshots.clone(),
                ctx.latest_session.clone(),
            ));

        // Publish to the HTTP server's late-bind slot.
        {
            let mut slot = ctx.session_metadata_slot.lock().await;
            *slot = Some(session_metadata);
        }

        // ADR-047: Build the SessionConfigService.
        //
        // Pass the `agent_core_shared` slot so `get_config` can apply
        // the shared `resolve_effective_reasoning_effort` chain before
        // returning the snapshot. Without this, HTTP
        // `GET /sessions/{sid}/config` would surface raw persisted
        // `reasoning_effort` and could report `null` for any session
        // whose `meta.json` was written before reasoning was ever
        // supported — see `usecases::session_config_impl` for the full
        // rationale.
        //
        // The slot is populated by Phase B at the bottom of this
        // function (after AgentCore construction + injection is
        // complete), so by the time the first HTTP request lands the
        // slot is already filled. `get_config` falls back to the raw
        // persisted value if it isn't — safe during boot races.
        //
        // ADR-074 §11.1: keep the concrete Arc so Phase B can inject the
        // idle-session `usage_recompute` callback once SessionManager is
        // constructed (it owns the session snapshots + chunk channel).
        let impl_arc = Arc::new(crate::usecases::RuntimeSessionConfigService::new(
            ctx.session_configs.clone(),
            Some(ctx.workspace_resolver.clone()),
            ctx.agent_core_shared.clone(),
        ));
        session_config_impl = Some(impl_arc.clone());
        let session_config: Arc<dyn crate::usecases::SessionConfigService> = impl_arc;
        {
            let mut slot = ctx.session_config_slot.lock().await;
            *slot = Some(session_config);
        }

        // Publish the RAG provider to the HTTP server's late-bind slot. It was
        // set on the core above; the memory consolidation timer that used to
        // share this block is gone with ADR-101 (the service owns it).
        {
            // core_clone was moved into RuntimeAgentTokenService above; use
            // agent_core_shared (which holds a clone) to access the fields.
            let shared = ctx.agent_core_shared.read();
            if let Ok(ref guard) = shared
                && let Some(core) = guard.as_ref()
                && let Some(ref rag) = core.rag_provider
                && let Ok(mut slot) = ctx.rag_provider_slot.write()
            {
                *slot = Some(rag.clone());
            }
        }
    }

    let mut session_manager = SessionManager::new(core, session_manager_config);

    session_manager.set_resolver(ctx.workspace_resolver.clone());

    // Seed the per-agent default workspace for NEW sessions. Sessions are
    // the source of truth (their meta persists the user's last selection),
    // so the resumed latest session's workspace_id wins; the disk
    // `last_active` flag is kept only as a fallback for legacy data written
    // by the pre-ADR-040 Gateway. Nothing else: the default is intentionally
    // NOT persisted at agent level — it is inherited from the session meta
    // at startup and updated from `route_workspace_switch` at runtime.
    let inherited_ws = conversation_session.as_ref().and_then(|c| c.workspace_id());
    let fallback_ws = ctx
        .workspace_resolver
        .read()
        .unwrap()
        .last_active_workspace_id()
        .map(|s| s.to_owned());
    if let Some(ws_id) = inherited_ws.clone().or(fallback_ws) {
        session_manager.set_default_workspace_id(&ws_id);
        tracing::info!(
            default_workspace_id = %ws_id,
            inherited_from_session = inherited_ws.is_some(),
            "SessionManager: initialized default workspace from latest session meta (or last_active fallback)"
        );
    }

    // (The background scan seeds `latest_session` directly — see Step 9
    // above; no post-construction copy is needed anymore.)

    // ── Step 9 (cont.): Resume latest session ────────────────────────
    // ADR-085 D7: the `else` branch that pre-created an ownerless
    // `initial session` when zero sessions were found on disk is
    // removed. Zero sessions is a legitimate state — the "ensure a
    // session exists" contract now has exactly one writer: the Desktop,
    // which POSTs /sessions after observing SESSIONS_READY and a
    // confirmed-empty list. The old dual-writer setup meant every
    // mistaken 404-read ("no session" while the scan was still
    // running, or while the slot was unfilled) left behind an orphan
    // session the user never sees.
    if let Some(conv) = conversation_session {
        // ADR-028 / ADR-066: merge the resumed session's persisted token
        // totals into the AgentCore counters so the live context_usage
        // WebSocket push doesn't report agent_total < session_total
        // after a process restart.
        if let Some(t) = conv.tokens() {
            session_manager.core().merge_token_totals((
                Some(t.total_input),
                Some(t.total_output),
                Some(t.total_cache_read),
                Some(t.total_cache_write),
            ));
        }
        let sid = conv.session_id().to_string();
        session_manager
            .create_session_with_id_and_conversation(
                sid.clone(),
                Some(conv),
                Some(committed_lines.clone()),
            )
            .await?;
        tracing::info!(initial_session_id = %sid, "Resumed latest session at startup");
    }

    // Workspace context and prompt file are applied inside
    // create_session_with_id_and_conversation (single source of truth from
    // the shared WorkspaceResolver). No follow-up step required here.

    // ADR-040: gRPC hello_config path removed. Runtime config overrides
    // from agent_config.json are always applied (all paths are Gateway mode).
    // agent_config.json defaults are resolved & persisted above
    // (Step 9 — AgentCore init). Apply any remaining overrides
    // from the loaded config to the session manager so new
    // sessions pick them up via runtime_overrides.

    let has_overrides = agent_cfg.max_output_tokens.is_some()
        || agent_cfg.max_iterations.is_some()
        || agent_cfg.temperature.is_some()
        || agent_cfg.context_window.is_some()
        || agent_cfg.system_prompt_override.is_some()
        || agent_cfg.shell_approval_threshold.is_some()
        || agent_cfg.approval_timeout_secs.is_some();
    if has_overrides {
        tracing::info!(
            max_output_tokens = ?agent_cfg.max_output_tokens,
            max_iterations = ?agent_cfg.max_iterations,
            temperature = ?agent_cfg.temperature,
            "Applying runtime config overrides from workspace agent_config.json"
        );
        session_manager.apply_runtime_config_override(&RuntimeConfigOverrides::from(&agent_cfg));
    }

    // Wrap SessionManager in an Arc<tokio::sync::Mutex<>> so the late-bind
    // slot below and any future background observers can read session
    // state without taking `&mut session_manager` (which is already
    // borrowed by the `runtime_overrides` setup above). The lock is
    // only held for the duration of the read call, so contention with
    // the rest of the runtime is negligible.
    let session_manager_arc: Arc<tokio::sync::Mutex<SessionManager>> =
        Arc::new(tokio::sync::Mutex::new(session_manager));

    // Late-bind the SessionManager into the slot the HTTP server is
    // already holding (it was given a clone of `ctx.session_manager_slot`
    // in Phase A). The runtime `POST /api/debug/enable` route reads
    // this slot to call `enable_debug_mode` without a restart; the
    // Phase C `debug_mode` startup wiring also goes through the same
    // slot via `enable_debug_mode_and_fill_slot`. Filling it here
    // (rather than later in Phase C) means both consumers can rely on
    // the slot being populated as soon as Phase B returns.
    *ctx.session_manager_slot.write().await = Some(session_manager_arc.clone());

    // ADR-074 §11.1: inject the idle-session usage recompute callback into
    // the SessionConfigService. Fire-and-forget — the callback spawns its
    // own task so `apply_config` never blocks on the SessionManager lock.
    // A running loop is skipped inside `usage_recompute`; the next per-turn
    // usage push takes over.
    {
        let sm = session_manager_arc.clone();
        if let Some(svc) = session_config_impl.as_ref() {
            svc.set_usage_recompute(Arc::new(move |session_id: String| {
                let sm = sm.clone();
                tokio::spawn(async move {
                    let guard = sm.lock().await;
                    guard.usage_recompute(&session_id);
                });
            }));
        }
    }

    // ADR-085 D3: SESSIONS_READY — the authoritative "chat can begin"
    // signal, published at the end of Phase B once BOTH late-bind slots
    // (session_metadata / session_config, filled above) and the
    // SessionManager slot are populated. It additionally waits on the
    // background session scan: until the scan lands, `/latest-session`
    // cannot honestly distinguish "zero sessions" from "sessions exist
    // but are still being walked" — a premature SESSIONS_READY would
    // re-open the orphan-session window D7 just closed (D4 table,
    // "扫描未完成" row). Detached so Phase C/D never block on a large
    // conversations tree (ADR-058 §3.4 keeps the same rationale).
    // Monotonicity + reconnect re-stamp live in `publish_lifecycle` /
    // `run_bootstrap` (D3b); a failed publish self-heals via the 5 s
    // heartbeat, which re-stamps `current_lifecycle`.
    {
        let slot = ctx.mqtt_client_slot.clone();
        tokio::spawn(async move {
            let _ = _session_scan_handle.await;
            let Some(client) = slot.lock().await.as_ref().cloned() else {
                // Unreachable in Gateway mode (Phase A fills the slot
                // before Phase B runs); logged because a silent skip
                // would leave the agent stuck at HTTP_READY with
                // nothing on record to explain why.
                tracing::warn!(
                    "SESSIONS_READY skipped: MQTT client slot is empty \
                     (standalone mode or Phase A connect failure)"
                );
                return;
            };
            if let Err(e) = client
                .lock()
                .await
                .publish_lifecycle(
                    acowork_core::mqtt_proto::AgentLifecycleState::SessionsReady,
                    "",
                )
                .await
            {
                tracing::warn!(
                    error = %e,
                    "SESSIONS_READY publish failed; heartbeat will retry"
                );
            }
        });
    }

    Ok(SessionBootContext {
        session_manager: session_manager_arc,
        committed_lines,
    })
}

