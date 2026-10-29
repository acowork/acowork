//! Shared builders for the 5 global-resource snapshots.
//!
//! Both the MQTT retained publisher (`global_resources_publisher`) and the
//! HTTP projection (`http::global_resources_api`) consume the same
//! `build_available_*` functions. Centralising the builders here keeps the
//! two channels in lockstep: any new field added to the protobuf payload
//! automatically appears in the HTTP JSON projection.
//!
//! Authority split:
//! - **Vault decryption** lives here, not in the publisher. HTTP and MQTT
//!   both ship the decrypted `api_key` (loopback-only transport, see
//!   `mqtt.md §3.1.1`); doing the decrypt once avoids two divergent code
//!   paths that can drift in key-handling.
//! - **Provider configuration** (`base_url`, `models`, capabilities) comes
//!   from `resource_cache.provider_list`, which is the source of truth for
//!   *which providers exist* (Vault only carries the key).

use acowork_core::mqtt_proto::{
    AvailableEmbeddingModels, AvailableMcps, AvailableProviders, AvailableUsers, CompactModelRef,
    EmbeddingModelRef, McpRef, ProviderModelRef, ProviderRef, UserProfileRef,
};
use acowork_core::protocol::{McpTransportDef, ProtocolType};

use crate::gateway::state::GatewayState;
use crate::util::preview_key;

/// Whether a provider with **no** vault account is still published to
/// runtimes.
///
/// Only local providers (ollama, lmstudio…) are keyless by design: the
/// runtime treats an empty `api_key` as "no auth", which those endpoints
/// accept. A keyless non-local provider cannot be called, and publishing it
/// plants a phantom entry in every Runtime's `agent_provider.json` that can
/// even become the session's active provider.
fn publish_keyless_provider(id: &str) -> bool {
    crate::http::models_api::is_local_provider(id)
}

/// Build the `McpRef` for an MCP server whose endpoint is **reverse-proxied
/// by this Gateway** (pm / doc — ADR-064 / ADR-070).
///
/// Two identity headers travel with every such entry, and both are
/// templates the Runtime resolves at connect time — the Gateway never holds
/// a per-agent value to substitute:
///
/// - `X-MCP-Actor: {instance_id}` — **who the agent is** to PM / Doc
///   (ADR-073 §1.3 invariant 1: every identity key is the instance UUID,
///   never the package id). PM/Doc use it for member checks and `X-Actor`
///   -style attribution.
/// - `X-ACowork-Node-Token: {node_token}` — **that the caller is a trusted
///   machine** (ADR-076). Without it the whole request dies at
///   `auth_middleware` with 401 the moment `AUTH_MODE=multi_user`, and the
///   failure is *silent at the UI layer*: `tools/list` never returns, so
///   `agent_mcp_tools.json` reconciles to zero entries and the Desktop
///   Tools panel renders no expandable row for the server at all. The
///   Gateway cannot substitute a value here because the token is
///   node-scoped and minted at enrollment; the Runtime resolves the
///   template from the credential the Node injected at spawn.
///
/// Kept as one helper because pm and doc are byte-identical in shape — the
/// two copies had already drifted once (doc lacked nothing, but nothing
/// enforced that).
fn gateway_hosted_mcp_ref(name: &str, url: String) -> McpRef {
    let mut headers = std::collections::HashMap::new();
    headers.insert("X-MCP-Actor".to_string(), "{instance_id}".to_string());
    headers.insert(
        acowork_core::auth::NODE_TOKEN_HEADER.to_string(),
        acowork_core::auth::NODE_TOKEN_TEMPLATE.to_string(),
    );
    McpRef {
        id: name.to_string(),
        name: name.to_string(),
        transport: map_mcp_transport(&McpTransportDef::Http).into(),
        url,
        command: String::new(),
        args: Vec::new(),
        env: std::collections::HashMap::new(),
        headers,
        tool_timeout_secs: 60,
        auth_token: String::new(),
        // pm/doc are Gateway-hosted HTTP servers — nothing to install (ADR-090).
        install_spec_json: String::new(),
    }
}

/// Build `AvailableProviders` from the GatewayState resource cache.
///
/// "Available" = all providers in the cache. Phase 2+ will filter to only
/// ready providers (the health-check loop is the gate). Vault decryption is
/// performed here so both MQTT and HTTP channels see the same key.
pub(crate) fn build_available_providers(gw: &GatewayState) -> AvailableProviders {
    let cache = &gw.resource_cache.provider_list;
    let mut providers: Vec<ProviderRef> = Vec::new();
    for p in &cache.providers {
        // Decrypt every account key for this provider; one ProviderRef
        // per `(provider, account)` pair. `vault.list_keys()` returns
        // one row per account, so this is a 1:N fan-out.
        //
        // `None` = enumeration failed, so we cannot tell whether keys
        // exist. Fall back to the historical "publish without key"
        // behaviour rather than dropping a possibly-usable provider.
        let accounts = match gw.vault.list_keys() {
            Ok(entries) => Some(
                entries
                    .into_iter()
                    .filter(|e| e.provider == p.id)
                    .collect::<Vec<_>>(),
            ),
            Err(e) => {
                tracing::warn!(
                    provider_id = %p.id,
                    error = %e,
                    "global_resources: failed to enumerate vault accounts; emitting single ProviderRef without key"
                );
                None
            }
        };

        // A non-local provider with no vault account cannot be called by any
        // Runtime (the wire carries an empty `api_key` = "no auth"), and
        // publishing it plants a keyless phantom in every Runtime's
        // `agent_provider.json` — where it can even become the session's
        // active provider while being invisible (and undeletable) in the
        // Desktop. Skip it. Local providers (ollama, lmstudio…) are keyless
        // by design and must still be published.
        if accounts.as_ref().is_some_and(|a| a.is_empty()) && !publish_keyless_provider(&p.id) {
            tracing::debug!(
                provider_id = %p.id,
                "global_resources: skipping non-local provider with no vault account"
            );
            continue;
        }

        // Decide which `(account_id, api_key)` tuples to emit. Local
        // providers have no real key but still want a row, so we emit
        // one empty entry for them.
        let accounts = accounts.unwrap_or_default();
        let emit: Vec<(String, String)> = if accounts.is_empty() {
            vec![(String::new(), String::new())]
        } else {
            accounts
                .into_iter()
                .map(|a| (a.account_id, a.key_preview))
                .collect()
        };

        // We need the *decrypted* keys, not previews. Re-resolve from
        // the vault by (provider, account_id) so the wire payload
        // matches the historical "decrypt before publish" contract.
        // `vault.get_provider` returns the first account when no
        // account_id is given; for multi-account providers we look up
        // each account explicitly.
        let decrypted_keys: Vec<(String, String)> = if p.id.is_empty() {
            Vec::new()
        } else {
            // Build (account_id, decrypted_key) pairs. For the local
            // case (no accounts), emit one ("", "") row.
            if emit.len() == 1 && emit[0].0.is_empty() {
                vec![(String::new(), String::new())]
            } else {
                emit.iter()
                    .map(|(account_id, _preview)| {
                        let api_key = match gw.vault.get_account(&p.id, account_id) {
                            Ok(entry) => entry.api_key,
                            Err(e) => {
                                tracing::warn!(
                                    provider_id = %p.id,
                                    account_id = %account_id,
                                    error = %e,
                                    "global_resources: failed to decrypt account key; emitting empty"
                                );
                                String::new()
                            }
                        };
                        (account_id.clone(), api_key)
                    })
                    .collect()
            }
        };

        for (account_id, api_key) in decrypted_keys {
            // DIAG: log the byte range about to be published to MQTT
            // so we can correlate this preview with the runtime side.
            tracing::info!(
                provider_id = %p.id,
                account_id = %account_id,
                api_key_len = api_key.len(),
                api_key_prefix = %preview_key(&api_key),
                "global_resources: building ProviderRef for AvailableProviders snapshot"
            );
            providers.push(ProviderRef {
                id: p.id.clone(),
                base_url: p.base_url.clone(),
                protocol_type: map_protocol_type(&p.protocol_type).into(),
                models: p
                    .models
                    .iter()
                    .map(|m| {
                        let (input_modalities, output_modalities) = m
                            .capabilities
                            .modalities
                            .as_ref()
                            .map(|moda| (moda.input.clone(), moda.output.clone()))
                            .unwrap_or_default();
                        // ADR-103 D1/D2: the Gateway is the single decision
                        // point. Publish the per-model effective protocol
                        // ONLY when it differs from the provider-level one —
                        // absent field = inherit (proto3 optional presence).
                        // In S1 nothing persists `responses` yet, so this
                        // always equals the provider level and the wire is
                        // byte-identical to the status quo (zero traffic
                        // change). Probe verdicts are NOT delivered (§3).
                        let (effective, base_url_override) =
                            crate::http::models_api::resolve_protocol(p, &m.id);
                        let proto_protocol = (effective != p.protocol_type)
                            .then(|| i32::from(map_protocol_type(&effective)));
                        ProviderModelRef {
                            id: m.id.clone(),
                            capabilities: Some(acowork_core::mqtt_proto::ModelCapabilities {
                                context_window: m.capabilities.context_window,
                                max_output_tokens: m.capabilities.max_output_tokens,
                                input_modalities,
                                output_modalities,
                                supports_reasoning: m.capabilities.supports_reasoning,
                                default_reasoning_effort: m.capabilities.default_reasoning_effort.clone(),
                            }),
                            max_output_tokens_limit: m.max_output_tokens_limit,
                            protocol_type: proto_protocol,
                            base_url: base_url_override,
                        }
                    })
                    .collect(),
                compact_model: p.compact_model.clone().unwrap_or_default(),
                custom: p.custom,
                api_key,
                account_id,
            });
        }
    }

    AvailableProviders {
        version: cache.version,
        providers,
        // ADR-056: forward the global compact-model candidate list so
        // Runtime can resolve the distillation fallback chain without an
        // extra round-trip. Empty = "no global override" — Runtime falls
        // back to provider.compact_model and chat. Field 3 mirrors the
        // head of the list for old Runtimes that predate the list form.
        #[allow(deprecated)]
        default_compact_model: cache
            .default_compact_models
            .first()
            .cloned()
            .map(|r| CompactModelRef {
                provider_id: r.provider_id,
                model_id: r.model_id,
            }),
        default_compact_models: cache
            .default_compact_models
            .iter()
            .map(|r| CompactModelRef {
                provider_id: r.provider_id.clone(),
                model_id: r.model_id.clone(),
            })
            .collect(),
    }
}

/// ADR-090 D3: is secret `name` configured (non-blank) on this catalog
/// entry? Looked up across `env ∪ headers` — stdio keys live in env, custom
/// HTTP keys in headers (same convention as
/// [`crate::resource_cache::extract_api_key_from_mcp_config`]).
fn secret_configured(entry: &acowork_core::protocol::McpServerConfigDef, name: &str) -> bool {
    entry
        .env
        .get(name)
        .or_else(|| entry.headers.get(name))
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Build `AvailableMcps` from the GatewayState resource cache.
///
/// The `auth_token` is extracted from env vars and headers via
/// `extract_api_key_from_mcp_config` — same logic that builds
/// `mcp_key_vault` for gRPC AgentHello. Empty when no auth required.
pub(crate) fn build_available_mcps(gw: &GatewayState) -> AvailableMcps {
    let cache = &gw.resource_cache.mcp_list;
    // MCP catalog is the source of truth for env/headers, not the
    // resource_cache (which only stores server lists). Load it here.
    let data_dir = gw
        .config
        .as_ref()
        .map(|c| std::path::PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| std::path::PathBuf::from("./data"));
    let catalog: Vec<acowork_core::protocol::McpServerConfigDef> =
        crate::http::mcp_catalog_api::load_mcp_catalog(&data_dir)
            .ok()
            .unwrap_or_default();
    let servers: Vec<McpRef> = cache
        .servers
        .iter()
        .filter_map(|s| {
            // Look up the catalog entry: source of truth for token,
            // required_secrets and the install spec (ADR-090 D3/D4/D5).
            let entry = catalog.iter().find(|c| c.name == s.id);
            let auth_token = entry
                .and_then(crate::resource_cache::extract_api_key_from_mcp_config)
                .unwrap_or_default();
            // ADR-090 D3 push filter: skip an entry whose required_secrets is
            // non-empty while any required name is missing/empty across
            // `env ∪ headers`. Everything else (no-secret servers + fully
            // keyed ones) is pushed regardless of install state — the Agent
            // installs on first enable (D5).
            if let Some(e) = entry
                && !e.required_secrets.is_empty()
                && e.required_secrets.iter().any(|k| !secret_configured(e, k))
            {
                tracing::debug!(
                    server = %s.id,
                    "MCP catalog entry not pushed: required secret missing (ADR-090 D3)"
                );
                return None;
            }
            let install_spec_json = entry
                .and_then(|e| e.install.as_ref())
                .and_then(|i| serde_json::to_string(i).ok())
                .unwrap_or_default();
            Some(McpRef {
                id: s.id.clone(),
                name: s.name.clone(),
                transport: map_mcp_transport(&s.transport).into(),
                url: s.url.clone().unwrap_or_default(),
                command: s.command.clone(),
                args: s.args.clone(),
                env: s.env.clone(),
                headers: s.headers.clone(),
                tool_timeout_secs: s.tool_timeout_secs.unwrap_or(0),
                auth_token,
                install_spec_json,
            })
        })
        .collect();

    // T3-4: 自动注入 pm MCP（设计 §6.1 / §21）。
    //
    // `pm_mcp_url` 在 `Gateway::run` 启动时设置（`Some` ⇔ PM 服务已启动且
    // `pm.auto_inject_mcp = true`）。注入后，每个 Agent 的 catalog 都会出现
    // `name = "pm"` 的 HTTP MCP server，Agent 启动即可调用 `pm_*` 工具。
    let mut servers = servers;
    if let Some(pm_url) = &gw.pm_mcp_url {
        servers.push(gateway_hosted_mcp_ref("pm", pm_url.clone()));
    }

    // D3-4: 自动注入 doc MCP（设计 §6）。`doc_mcp_url` 在 `Gateway::run`
    // 启动时设置（`Some` ⇔ doc 服务已启动且 `doc.auto_inject_mcp = true`）。
    // 注入后 Agent catalog 出现 `name = "doc"` 的 HTTP MCP server，Agent
    // 启动即可调用 `doc_*` 工具（读写文档 + PR 式审核提交）。
    if let Some(doc_url) = &gw.doc_mcp_url {
        servers.push(gateway_hosted_mcp_ref("doc", doc_url.clone()));
    }

    // ADR-101: auto-inject the memory MCP. `memory_mcp_url` is set in
    // `Gateway::run` (`Some` ⇔ the memory supervisor is enabled and
    // `memory.auto_inject_mcp = true`) and re-derived by the advertise-host
    // watchdog on IP change, exactly like doc's. The URL points at the
    // Gateway-facing `/api/memory/mcp` path, not at the service port, so the
    // Gateway stays the single authentication/forwarding hop (ADR-009).
    if let Some(memory_url) = &gw.memory_mcp_url {
        servers.push(gateway_hosted_mcp_ref("memory", memory_url.clone()));
    }

    AvailableMcps {
        version: cache.version,
        servers,
    }
}

/// Build `AvailableEmbeddingModels` from the GatewayState resource cache
/// + embed process state + active cloud embedding selection (Vault).
pub(crate) fn build_available_embedding_models(gw: &GatewayState) -> AvailableEmbeddingModels {
    let cache = &gw.resource_cache.embedding_models;
    let models: Vec<EmbeddingModelRef> = cache
        .models
        .iter()
        .map(|m| EmbeddingModelRef {
            id: m.id.clone(),
            name: m.name.clone(),
            description: m.description.clone().unwrap_or_default(),
            dimension: m.dimension as u32,
            max_tokens: m.max_tokens as u32,
            size_mb: m.size_mb,
            languages: m.languages.clone(),
            hf_repo: m.hf_repo.clone(),
            onnx_file: m.onnx_file.clone(),
            tokenizer_file: m.tokenizer_file.clone(),
            bundled: m.bundled,
            recommended: m.recommended,
        })
        .collect();

    // Active model info from embed process state.
    let (active_model_id, active_dimension, endpoint) = match &gw.embed_process {
        Some(eps) if eps.ready => (
            eps.active_model_id.clone().unwrap_or_default(),
            eps.active_dimension.unwrap_or(0) as u32,
            // ADR-055 D3: advertise host instead of hard-coded 127.0.0.1.
            format!("http://{}:{}/v1", gw.advertise_host, eps.port),
        ),
        _ => (String::new(), 0, String::new()),
    };

    // Cloud embedding selection (S1-5b): read active selection from disk +
    // decrypt the API key from the Vault. When the snapshot is empty, the
    // proto defaults to empty strings and Runtime continues with local
    // ONNX.
    let data_dir: Option<&std::path::Path> = gw
        .config
        .as_ref()
        .map(|c| std::path::Path::new(&c.data_dir));
    let cloud = data_dir
        .map(|dir| crate::embedding_providers::resolve_active_cloud_embedding(dir, &gw.vault))
        .unwrap_or_default();

    AvailableEmbeddingModels {
        version: cache.version,
        models,
        active_model_id,
        active_dimension,
        endpoint,
        active_provider_id: cloud.active_provider_id,
        active_api_key: cloud.active_api_key,
        active_base_url: cloud.active_base_url,
    }
}

/// ADR-042: Build `AvailableUsers` from the GatewayState user profile list.
///
/// Finds the user with `is_active == true` and serialises it into
/// `UserProfileRef`. UI-only fields (avatar / builtin_avatar /
/// created_at / updated_at / is_active) are omitted — Runtime never
/// renders user profile UI. `custom` HashMap is serialised to JSON.
pub(crate) fn build_available_users(gw: &GatewayState) -> AvailableUsers {
    let list = &gw.resource_cache.user_profile_list;
    let active = list.users.iter().find(|u| u.is_active).map(|u| {
        let custom_json = serde_json::to_string(&u.custom).unwrap_or_else(|e| {
            tracing::warn!(
                user_id = %u.user_id,
                error = %e,
                "Failed to serialise UserProfile.custom to JSON; sending empty"
            );
            "{}".to_string()
        });
        UserProfileRef {
            user_id: u.user_id.clone(),
            display_name: u.display_name.clone(),
            language: u.language.clone(),
            timezone: u.timezone.clone(),
            city: u.city.clone(),
            country: u.country.clone(),
            occupation: u.occupation.clone(),
            communication_style: u.communication_style.clone(),
            custom_json,
        }
    });

    AvailableUsers {
        version: list.version,
        active_user: active,
    }
}

// ── Enum mappers ─────────────────────────────────────────────────────

pub(crate) fn map_protocol_type(pt: &ProtocolType) -> acowork_core::mqtt_proto::LlmProtocol {
    match pt {
        ProtocolType::OpenAI => acowork_core::mqtt_proto::LlmProtocol::Openai,
        ProtocolType::Anthropic => acowork_core::mqtt_proto::LlmProtocol::Anthropic,
        ProtocolType::Google => acowork_core::mqtt_proto::LlmProtocol::Google,
        ProtocolType::Ollama => acowork_core::mqtt_proto::LlmProtocol::Ollama,
        // ADR-103 D3: Responses shape over the MQTT wire.
        ProtocolType::Responses => acowork_core::mqtt_proto::LlmProtocol::Responses,
        // A read-side tolerance value never originates here (the Gateway
        // resolves `Unknown` away before publishing), but the match must be
        // total; publishing it as UNSPECIFIED would let old Runtimes silently
        // pick OpenAI, so keep it visible.
        ProtocolType::Unknown => {
            tracing::warn!("map_protocol_type: Unknown protocol published as UNSPECIFIED (should be unreachable)");
            acowork_core::mqtt_proto::LlmProtocol::Unspecified
        }
    }
}

pub(crate) fn map_mcp_transport(t: &McpTransportDef) -> acowork_core::mqtt_proto::McpTransport {
    match t {
        McpTransportDef::Stdio => acowork_core::mqtt_proto::McpTransport::Stdio,
        McpTransportDef::Http => acowork_core::mqtt_proto::McpTransport::Http,
        McpTransportDef::Sse => acowork_core::mqtt_proto::McpTransport::Sse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ADR-090 D3: secret lookup spans `env ∪ headers` (stdio keys live in
    /// env, custom HTTP keys in headers); blank/whitespace values do not
    /// count as configured.
    #[test]
    fn test_secret_configured_env_headers_and_blank() {
        use acowork_core::protocol::McpServerConfigDef;
        let mut entry = McpServerConfigDef {
            name: "brave".into(),
            display_name: None,
            env: std::collections::HashMap::from([
                ("BRAVE_API_KEY".to_string(), "k1".to_string()),
                ("BLANK".to_string(), "   ".to_string()),
            ]),
            headers: std::collections::HashMap::from([(
                "X-API-KEY".to_string(),
                "h1".to_string(),
            )]),
            ..Default::default()
        };
        assert!(secret_configured(&entry, "BRAVE_API_KEY"), "env hit");
        assert!(secret_configured(&entry, "X-API-KEY"), "headers hit");
        assert!(!secret_configured(&entry, "BLANK"), "whitespace = not set");
        assert!(!secret_configured(&entry, "MISSING"));
        // env wins when both carry the name (same convention as
        // extract_api_key_from_mcp_config — presence in either suffices).
        entry.headers
            .insert("BRAVE_API_KEY".to_string(), "h2".to_string());
        assert!(secret_configured(&entry, "BRAVE_API_KEY"));
    }

    /// Only local providers may be published without a key; a keyless
    /// non-local provider must not reach the runtimes.
    #[test]
    fn test_publish_keyless_provider_only_for_local() {
        assert!(publish_keyless_provider("ollama"));
        assert!(publish_keyless_provider("lmstudio"));
        assert!(!publish_keyless_provider("volcengine-agent-plan"));
        assert!(!publish_keyless_provider("custom-volcengine-agent-plan"));
    }

    #[test]
    fn test_map_protocol_type() {
        assert_eq!(
            map_protocol_type(&ProtocolType::OpenAI),
            acowork_core::mqtt_proto::LlmProtocol::Openai
        );
        assert_eq!(
            map_protocol_type(&ProtocolType::Anthropic),
            acowork_core::mqtt_proto::LlmProtocol::Anthropic
        );
    }

    #[test]
    fn test_map_mcp_transport() {
        assert_eq!(
            map_mcp_transport(&McpTransportDef::Stdio),
            acowork_core::mqtt_proto::McpTransport::Stdio
        );
    }

    #[test]
    fn test_build_available_providers_empty() {
        let gw = GatewayState::new("/tmp/test-vault");
        let payload = build_available_providers(&gw);
        assert_eq!(payload.version, 0);
        assert!(payload.providers.is_empty());
    }

    /// T4-1（P4 远程）：`pm_mcp_url` 存在时，`build_available_mcps` 把 pm MCP
    /// 注入全局 `acowork/global/mcps` 资源，远程 Runtime 即可拿到 advertise
    /// endpoint（`http://{advertise_host}:{gw_http_port}{mcp_http_path}`）。
    #[test]
    fn test_build_available_mcps_injects_pm_mcp_when_url_set() {
        let mut gw = GatewayState::new("/tmp/test-vault");
        // 模拟 `Gateway::run` 在 PM 启动成功且 `pm.auto_inject_mcp` 时写入的
        // advertise endpoint（ADR-055 D3：用 advertise_host 而非 127.0.0.1）。
        gw.pm_mcp_url = Some("http://192.168.1.50:19876/api/pm/mcp".to_string());

        let payload = build_available_mcps(&gw);

        let pm = payload
            .servers
            .iter()
            .find(|s| s.id == "pm")
            .expect("pm MCP should be injected into global mcps when pm_mcp_url is set");
        assert_eq!(pm.name, "pm");
        assert_eq!(pm.url, "http://192.168.1.50:19876/api/pm/mcp");
        assert_eq!(
            pm.transport,
            map_mcp_transport(&McpTransportDef::Http) as i32,
            "pm MCP must use HTTP transport"
        );
        // 身份模板：Runtime 侧替换为实际 instance_id（X-MCP-Actor header，ADR-073）。
        assert_eq!(
            pm.headers.get("X-MCP-Actor").map(|s| s.as_str()),
            Some("{instance_id}"),
            "pm MCP must carry X-MCP-Actor identity template (ADR-073 instance_id)"
        );
        assert_eq!(pm.tool_timeout_secs, 60);
    }

    /// ADR-076 regression: every Gateway-hosted MCP entry (pm / doc) must
    /// carry BOTH identity templates.
    ///
    /// Dropping `X-ACowork-Node-Token` does not fail loudly anywhere: the
    /// Runtime's `tools/list` 401s, `agent_mcp_tools.json` reconciles to
    /// zero rows, and the Desktop Tools panel simply renders no expandable
    /// row — the server looks "off" with no error surfaced. So assert the
    /// header on **every** injected entry rather than per-server, which is
    /// also what keeps a future third Gateway-hosted service covered.
    #[test]
    fn gateway_hosted_mcps_carry_both_identity_templates() {
        use acowork_core::auth::{NODE_TOKEN_HEADER, NODE_TOKEN_TEMPLATE};

        let mut gw = GatewayState::new("/tmp/test-vault");
        gw.pm_mcp_url = Some("http://192.168.1.50:19876/api/pm/mcp".to_string());
        gw.doc_mcp_url = Some("http://192.168.1.50:19876/api/doc/mcp".to_string());
        gw.memory_mcp_url = Some("http://192.168.1.50:19876/api/memory/mcp".to_string());

        let payload = build_available_mcps(&gw);
        for id in ["pm", "doc", "memory"] {
            let s = payload
                .servers
                .iter()
                .find(|s| s.id == id)
                .unwrap_or_else(|| panic!("{id} MCP should be injected"));
            assert_eq!(
                s.headers.get("X-MCP-Actor").map(String::as_str),
                Some("{instance_id}"),
                "{id}: agent-identity template (ADR-073) must survive",
            );
            assert_eq!(
                s.headers.get(NODE_TOKEN_HEADER).map(String::as_str),
                Some(NODE_TOKEN_TEMPLATE),
                "{id}: without the node-token template the Runtime 401s at \
                 auth_middleware and the Tools panel shows no expandable row \
                 (ADR-076)",
            );
        }
    }

    /// T4-1 反向：`pm_mcp_url` 为 None（PM 未启动 / auto_inject_mcp=false）时
    /// **不**注入 pm MCP，避免向 Agent 暴露不可达端点。
    #[test]
    fn test_build_available_mcps_skips_pm_when_url_none() {
        let gw = GatewayState::new("/tmp/test-vault");
        assert!(gw.pm_mcp_url.is_none());

        let payload = build_available_mcps(&gw);
        assert!(
            !payload.servers.iter().any(|s| s.id == "pm"),
            "pm MCP must NOT be injected when pm_mcp_url is None"
        );
    }

    /// D3-4: `doc_mcp_url` 存在时，`build_available_mcps` 把 doc MCP 注入
    /// 全局 mcps 资源（远程/本地 Runtime 均可经 advertise endpoint 调用）。
    #[test]
    fn test_build_available_mcps_injects_doc_mcp_when_url_set() {
        let mut gw = GatewayState::new("/tmp/test-vault");
        gw.doc_mcp_url = Some("http://192.168.1.50:19876/api/doc/mcp".to_string());

        let payload = build_available_mcps(&gw);

        let doc = payload
            .servers
            .iter()
            .find(|s| s.id == "doc")
            .expect("doc MCP should be injected when doc_mcp_url is set");
        assert_eq!(doc.name, "doc");
        assert_eq!(doc.url, "http://192.168.1.50:19876/api/doc/mcp");
        assert_eq!(
            doc.transport,
            map_mcp_transport(&McpTransportDef::Http) as i32,
            "doc MCP must use HTTP transport"
        );
        assert_eq!(
            doc.headers.get("X-MCP-Actor").map(|s| s.as_str()),
            Some("{instance_id}"),
            "doc MCP must carry X-MCP-Actor identity template (ADR-073 instance_id)"
        );
        assert_eq!(doc.tool_timeout_secs, 60);
    }

    /// D3-4 反向：`doc_mcp_url` 为 None（doc 未启用 / auto_inject_mcp=false）
    /// 时不注入 doc MCP。
    #[test]
    fn test_build_available_mcps_skips_doc_when_url_none() {
        let gw = GatewayState::new("/tmp/test-vault");
        assert!(gw.doc_mcp_url.is_none());

        let payload = build_available_mcps(&gw);
        assert!(
            !payload.servers.iter().any(|s| s.id == "doc"),
            "doc MCP must NOT be injected when doc_mcp_url is None"
        );
    }

    /// ADR-101: `memory_mcp_url` set ⇒ the memory MCP is injected like doc's,
    /// on HTTP transport with the instance-identity template.
    #[test]
    fn test_build_available_mcps_injects_memory_mcp_when_url_set() {
        let mut gw = GatewayState::new("/tmp/test-vault");
        gw.memory_mcp_url = Some("http://192.168.1.50:19876/api/memory/mcp".to_string());

        let payload = build_available_mcps(&gw);

        let memory = payload
            .servers
            .iter()
            .find(|s| s.id == "memory")
            .expect("memory MCP should be injected when memory_mcp_url is set");
        assert_eq!(memory.name, "memory");
        assert_eq!(memory.url, "http://192.168.1.50:19876/api/memory/mcp");
        assert_eq!(
            memory.transport,
            map_mcp_transport(&McpTransportDef::Http) as i32,
            "memory MCP must use HTTP transport"
        );
        assert_eq!(
            memory
                .headers
                .get("X-MCP-Actor")
                .map(|s| s.as_str()),
            Some("{instance_id}"),
            "memory MCP must carry X-MCP-Actor identity template (ADR-101 instance namespace)"
        );
        assert_eq!(memory.tool_timeout_secs, 60);
    }

    /// ADR-101 reverse: the memory MCP stays out of the catalog when
    /// `memory_mcp_url` is `None` (supervisor disabled, or the restart cap was
    /// hit) — a reachable-looking endpoint with no process behind it would
    /// turn every `memory_*` call into a tool timeout.
    #[test]
    fn test_build_available_mcps_skips_memory_when_url_none() {
        let gw = GatewayState::new("/tmp/test-vault");
        assert!(gw.memory_mcp_url.is_none());

        let payload = build_available_mcps(&gw);
        assert!(
            !payload.servers.iter().any(|s| s.id == "memory"),
            "memory MCP must NOT be injected when memory_mcp_url is None"
        );
    }
}


// ── ADR-103 S1: model-level wire emission tests ───────────────────────

#[cfg(test)]
mod adr103_builder_tests {
    use super::*;
    use acowork_core::mqtt_proto::LlmProtocol;
    use acowork_core::protocol::{
        ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry,
    };

    fn caps() -> ModelCapabilitiesInfo {
        serde_json::from_value(serde_json::json!({
            "context_window": 1000u64, "max_output_tokens": 100u64
        }))
        .unwrap()
    }

    fn gw_with(model: ProviderModelEntry) -> GatewayState {
        let mut gw = GatewayState::new("/tmp/test-vault");
        // Use a local provider id: keyless publish is allowed only for
        // local providers, and the test vault is locked.
        gw.resource_cache.provider_list.version = 1;
        gw.resource_cache.provider_list.providers = vec![ProviderListItem {
            id: "ollama".into(),
            base_url: "http://localhost:11434/v1".into(),
            protocol_type: ProtocolType::OpenAI,
            models: vec![model],
            compact_model: None,
            custom: false,
        }];
        gw.config = Some(crate::config::GatewayConfig::default());
        gw
    }

    fn model(json: serde_json::Value) -> ProviderModelEntry {
        let mut m: ProviderModelEntry = serde_json::from_value(serde_json::json!({
            "id": "gpt-5",
            "capabilities": caps(),
            "max_output_tokens_limit": 4096u64,
        }))
        .unwrap();
        if let Some(v) = json.get("protocol_type") {
            m.protocol_type = Some(serde_json::from_value(v.clone()).unwrap());
        }
        if let Some(v) = json.get("protocol_probe") {
            m.protocol_probe = Some(serde_json::from_value(v.clone()).unwrap());
        }
        if let Some(v) = json.get("base_url") {
            m.base_url = v.as_str().map(|s| s.to_string());
        }
        m
    }

    #[test]
    fn status_quo_payload_carries_no_model_level_fields() {
        // Zero-traffic-change invariant: with no model-level selection, the
        // published ProviderModelRef is byte-identical to pre-ADR-103 (all
        // new optional fields absent).
        let gw = gw_with(model(serde_json::json!({})));
        let payload = build_available_providers(&gw);
        let m = &payload.providers[0].models[0];
        assert_eq!(m.protocol_type, None);
        assert_eq!(m.base_url, None);
    }

    #[test]
    fn gated_responses_selection_emits_model_level_override() {
        let m = model(serde_json::json!({
            "protocol_type": "responses",
            "protocol_probe": { "state": "verified" },
            "base_url": "https://api.openai.com/v1",
        }));
        let gw = gw_with(m);
        let payload = build_available_providers(&gw);
        let pm = &payload.providers[0].models[0];
        // Effective Responses != provider OpenAI -> model-level field present.
        assert_eq!(pm.protocol_type, Some(LlmProtocol::Responses as i32));
        assert_eq!(pm.base_url.as_deref(), Some("https://api.openai.com/v1"));
    }

    #[test]
    fn an_unverified_responses_selection_emits_nothing_model_level() {
        // A selection without a passing verdict resolves to the provider
        // level -> model-level fields absent -> the Runtime inherits, so
        // the wire stays identical to the status quo. This is what keeps
        // "selected but not verified" from reaching live traffic.
        let m = model(serde_json::json!({
            "protocol_type": "responses",
            "protocol_probe": { "state": "failed" },
            "base_url": "https://api.openai.com/v1",
        }));
        let gw = gw_with(m);
        let payload = build_available_providers(&gw);
        let pm = &payload.providers[0].models[0];
        assert_eq!(pm.protocol_type, None);
        assert_eq!(pm.base_url, None);
    }

    #[test]
    fn map_protocol_type_new_variants() {
        assert_eq!(
            map_protocol_type(&ProtocolType::Responses),
            LlmProtocol::Responses
        );
        assert_eq!(i32::from(LlmProtocol::Responses), 5);
        // Unknown is published as UNSPECIFIED only as a last-resort total
        // match; resolve_protocol removes it before this point.
        assert_eq!(
            map_protocol_type(&ProtocolType::Unknown),
            LlmProtocol::Unspecified
        );
    }
}
