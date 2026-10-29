//! Provider configuration HTTP API handlers
//!
//! Full provider lifecycle management: API key (encrypted Vault) +
//! configuration (provider_list.json: base_url, models, capabilities, compact_model).
//!
//! - GET    /api/providers          — list providers (masked keys) + config
//! - POST   /api/providers          — add a provider (key + config)
//! - DELETE /api/providers/:provider — remove a provider
//! - PUT    /api/providers/:provider — update a provider (key and/or config)
//! - GET    /api/providers/:id/logo — provider brand mark, cacheable
//! - POST   /api/providers/:id/probe — ADR-103 D10 protocol probe (L1/L2/L3)
//!
//! ## ADR-103 write path
//!
//! Provider writes carry three optional protocol-intent fields (`protocol`,
//! `model_protocols`, `model_base_urls`). They record *intent*; the D1 gate in
//! [`models_api::resolve_protocol`] decides whether `responses` actually takes
//! effect, and that gate needs a passing probe verdict. After the write is
//! durable, D12 probes the changed `responses` rows and returns the verdicts
//! in the save response.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::{Deserialize, Serialize};

use crate::http::models_api;
use crate::http::routes::{ApiError, AppState, OperationAck};
use crate::resource_cache;
use acowork_core::operation::{OperationRecord, OperationState};
use acowork_core::protocol::{ModelCapabilitiesInfo, ProtocolType};
use std::collections::HashMap;
use std::path::PathBuf;

/// Build the provider configuration router
pub fn provider_routes() -> Router<AppState> {
    Router::new()
        .route("/api/providers", get(list_providers).post(add_provider))
        .route(
            "/api/providers/{provider}",
            delete(remove_provider).put(update_provider),
        )
        .route(
            "/api/providers/{provider}/keys/{account_id}",
            delete(remove_provider_account).patch(update_provider_account),
        )
        .route("/api/providers/{provider}/logo", get(provider_logo))
        // ADR-103 D10.1: explicit protocol probe (L1/L2, plus L3 on request).
        .route("/api/providers/{provider}/probe", post(probe_provider))
}

// ── Response types ────────────────────────────────────────────────────

/// Masked key entry with provider config (first 3 + last 3 chars visible).
///
/// Config fields (base_url, models, compact_model) are read from
/// provider_list.json, NOT from Vault.
#[derive(Serialize)]
pub struct ProviderEntryResponse {
    pub provider: String,
    /// Stable account UUID — empty for legacy single-key rows.
    /// Use this in subsequent add/update/remove calls.
    pub account_id: String,
    /// User-facing label for this account. Empty for legacy rows.
    pub alias: String,
    pub key_preview: String,
    /// Configured base URL (if any)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Configured default model (models[0])
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Selected models list (may be empty)
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_model: Option<String>,
    /// Whether this is a local (self-hosted) provider (no API key required)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub local: bool,
    /// Whether this is a user-defined custom provider (not listed in models.dev)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub custom: bool,
    /// Per-model capabilities map (model ID → capabilities), including user-configured overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_capabilities: Option<HashMap<String, ModelCapabilitiesInfo>>,
}

/// Default max output tokens when gateway config doesn't specify a limit.
const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 16_384;

/// One API key entry inside an `AddProviderRequest.keys` array.
#[derive(Deserialize)]
pub struct AddProviderKey {
    /// User-facing label; optional. Empty/missing → `"{provider}-default"`.
    #[serde(default)]
    pub alias: Option<String>,
    /// API key value. Empty allowed for local or key-less custom providers.
    #[serde(default)]
    pub key: String,
}

/// Add provider request.
///
/// Backwards compatibility: `key` (single, no alias) is still accepted
/// and is treated as a one-element `keys` list. New callers should use
/// `keys` to attach multiple accounts (e.g. work + personal) to the
/// same provider.
///
/// `base_url`, `models`, `compact_model` → stored in provider_list.json.
/// `model_capabilities` → user-configured overrides merged into offline data.
#[derive(Deserialize)]
pub struct AddProviderRequest {
    pub provider: String,
    /// Legacy single-key field. Ignored when `keys` is non-empty.
    #[serde(default)]
    pub key: String,
    /// Multi-account entry: each entry creates a separate vault account.
    #[serde(default)]
    pub keys: Vec<AddProviderKey>,
    /// Optional base URL override (e.g. "https://api.deepseek.com/v1")
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional default model (fallback if `models` is empty)
    #[serde(default)]
    pub default_model: Option<String>,
    /// Selected models for this provider (from models.dev).
    /// models[0] is the default/active model.
    #[serde(default)]
    pub models: Vec<String>,
    /// Compact model for LLM summarization (ADR-010). None = use current model.
    #[serde(default)]
    pub compact_model: Option<String>,
    /// Whether this is a custom (user-defined) provider not listed in models.dev.
    /// Custom providers always use OpenAI-compatible protocol.
    #[serde(default)]
    pub custom: Option<bool>,
    /// Per-model capabilities overrides (model ID → capabilities).
    /// User-configured fields (e.g. `default_reasoning_effort`) are merged
    /// into the offline models.dev data so the Runtime sees user preferences.
    #[serde(default)]
    pub model_capabilities: Option<HashMap<String, ModelCapabilitiesInfo>>,
    /// ADR-103 D1: provider-level protocol intent. When present it becomes the
    /// default protocol of every model in this write; models inherit it unless
    /// `model_protocols` overrides them.
    #[serde(default)]
    pub protocol: Option<ProtocolType>,
    /// ADR-103 D1: per-model protocol intent (`model id -> protocol`). This is
    /// the only field that can make `responses` effective, and only after the
    /// probe verdict gates it in (D1) — writing it here records *intent*, not
    /// an effective change.
    ///
    /// A `null` value means "clear the selection for this model" (the UI's
    /// "follow the data" option), which restores the npm-derived default and
    /// drops the now-meaningless probe verdict. An absent key leaves the
    /// stored value untouched.
    #[serde(default)]
    pub model_protocols: Option<HashMap<String, Option<ProtocolType>>>,
    /// ADR-103 D10.3: per-model base URL override (`model id -> url`), for
    /// providers whose Responses endpoint differs from the provider default.
    #[serde(default)]
    pub model_base_urls: Option<HashMap<String, String>>,
    /// ADR-059 §7.3: the BootstrapState `version` the client read
    /// before writing (optimistic concurrency — stale clients are
    /// rejected with `resource_version_conflict`). Absent → no
    /// precondition.
    #[serde(default)]
    pub expected_version: Option<u64>,
}

/// Update provider request (supports partial updates — config and
/// incremental key additions are optional).
///
/// Two key-bearing fields coexist for backwards compatibility:
/// - `key` (single, no alias): replaces the first account's key when
///   present. Pre-multi-account clients keep working.
/// - `keys` (multi): each entry becomes a brand-new account on the
///   Vault side, leaving existing accounts untouched. Used by the
///   edit dialog to add new accounts without disturbing the
///   already-configured ones.
///
/// `base_url`, `models`, `compact_model`, `model_capabilities` cover
/// provider-level config; they're shared across all accounts.
#[derive(Deserialize)]
pub struct UpdateProviderRequest {
    /// Legacy single-key field. If `None` or empty, the existing first
    /// account's key is preserved. Mutually compatible with `keys`
    /// (which adds accounts instead of replacing).
    #[serde(default)]
    pub key: Option<String>,
    /// Multi-account entries to append. Each entry creates a fresh
    /// vault account; existing accounts are not modified.
    #[serde(default)]
    pub keys: Vec<AddProviderKey>,
    /// Optional base URL override
    #[serde(default)]
    pub base_url: Option<String>,
    /// Optional default model (fallback if `models` is empty)
    #[serde(default)]
    pub default_model: Option<String>,
    /// Selected models for this provider (from models.dev).
    #[serde(default)]
    pub models: Vec<String>,
    /// Compact model for LLM summarization (ADR-010).
    #[serde(default)]
    pub compact_model: Option<String>,
    /// Per-model capabilities overrides (model ID → capabilities).
    /// User-configured fields (e.g. `default_reasoning_effort`) are merged
    /// into the offline models.dev data so the Runtime sees user preferences.
    #[serde(default)]
    pub model_capabilities: Option<HashMap<String, ModelCapabilitiesInfo>>,
    /// ADR-103 D1: provider-level protocol intent (see `AddProviderRequest`).
    #[serde(default)]
    pub protocol: Option<ProtocolType>,
    /// ADR-103 D1: per-model protocol intent (`model id -> protocol`);
    /// `null` clears the selection for that model (see `AddProviderRequest`).
    #[serde(default)]
    pub model_protocols: Option<HashMap<String, Option<ProtocolType>>>,
    /// ADR-103 D10.3: per-model base URL override (`model id -> url`).
    #[serde(default)]
    pub model_base_urls: Option<HashMap<String, String>>,
}

/// Generic message response
#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

/// Response for `POST /api/providers` — the ADR-059 ack plus the ADR-103 probe
/// verdicts this save produced (D12.1).
///
/// `probes` is omitted when nothing was probed, so the wire shape for every
/// non-`Responses` save stays byte-identical to the pre-ADR-103 ack.
#[derive(Serialize)]
pub struct ProviderSaveResponse {
    #[serde(flatten)]
    pub ack: OperationAck,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<crate::probe::ProbeReport>,
}

/// Response for `PUT /api/providers/{provider}` (see [`ProviderSaveResponse`]).
#[derive(Serialize)]
pub struct ProviderUpdateResponse {
    pub message: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub probes: Vec<crate::probe::ProbeReport>,
}

/// Request body for `POST /api/providers/{provider}/probe` (ADR-103 D10.1).
#[derive(Deserialize)]
pub struct ProbeProviderRequest {
    /// Model to probe — must already be configured on the provider.
    pub model: String,
    /// Protocol to verify. Only `responses` has an implementation; any other
    /// value is rejected with 400 rather than silently probing something else.
    #[serde(default)]
    pub protocol: Option<String>,
    /// Tier selector. `deep` (L1+L2+L3) is the only tier that can produce a
    /// positive verdict, so an explicit probe request is always upgraded to it
    /// (D10.2); the field is accepted for forward compatibility and echoed back
    /// in the response.
    #[serde(default)]
    pub tier: Option<crate::probe::ProbeTier>,
}

/// Query string for the probe endpoint.
#[derive(Deserialize)]
pub struct ProbeQuery {
    /// Bypass the 24 h verdict cache (D10.5).
    #[serde(default)]
    pub force: Option<bool>,
}

/// Response for `POST /api/providers/{provider}/probe`.
#[derive(Debug, Serialize)]
pub struct ProbeResponse {
    pub provider: String,
    pub model: String,
    pub protocol: String,
    pub tier: crate::probe::ProbeTier,
    /// D10.6 verdict: `verified` / `degraded` / `failed`.
    pub state: acowork_core::protocol::ProbeState,
    #[serde(flatten)]
    pub outcome: crate::probe::ProbeOutcome,
    /// Fields the probe had to strip to get a 2xx (D8 `UnsupportedField` trim).
    ///
    /// Always empty in this slice: the trim itself is S7. It is part of the
    /// D10.1 response contract from the start so the UI can render "degraded:
    /// reasoning omitted" without a response-shape change later.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stripped_fields: Vec<String>,
}

// ── Handlers ──────────────────────────────────────────────────────────

/// Which `(account_id, alias, key_preview)` rows to emit for one configured
/// provider.
///
/// Local providers have no key by design. A non-local provider whose vault
/// accounts are all gone (last key removed, or a leftover id from a rename)
/// must still surface one empty-account row: without it the provider is
/// invisible in the UI while `provider_list.json` and every Runtime still
/// carry it, so the user has no way to delete it.
fn account_rows_for_provider(
    is_local: bool,
    accounts: Option<Vec<(String, String, String)>>,
) -> Vec<(String, String, String)> {
    if is_local {
        return vec![(String::new(), String::new(), "(local)".to_string())];
    }
    accounts.unwrap_or_else(|| vec![(String::new(), String::new(), "(no key)".to_string())])
}

/// `GET /api/providers` — list stored providers (masked keys) with config.
///
/// For providers with multiple accounts the response contains one entry
/// per `(provider, account_id)` pair; provider-level config
/// (`base_url`, `models`, `compact_model`, `model_capabilities`) is
/// duplicated across each row for the same provider. Local providers
/// (no API key) keep emitting a single row with empty `account_id`.
pub async fn list_providers(
    State(state): State<AppState>,
) -> Result<Json<Vec<ProviderEntryResponse>>, ApiError> {
    let gw = state.gateway_state.read().await;

    // Group vault accounts by provider so we can join against the
    // provider_list.json cache below. `vault.list_keys()` already
    // expands one row per account, so this is a 1:N fan-out.
    let mut accounts_by_provider: std::collections::HashMap<
        String,
        Vec<(String, String, String)>,
    > = std::collections::HashMap::new();
    if let Ok(entries) = gw.vault.list_keys() {
        for e in entries {
            accounts_by_provider
                .entry(e.provider)
                .or_default()
                .push((e.account_id, e.alias, e.key_preview));
        }
    }

    // Iterate resource_cache as source of truth for which providers exist.
    let mut response: Vec<ProviderEntryResponse> = Vec::new();
    for cfg in &gw.resource_cache.provider_list.providers {
        let is_local = models_api::is_local_provider(&cfg.id);
        let accounts = accounts_by_provider.remove(&cfg.id);
        let accounts_for_provider = account_rows_for_provider(is_local, accounts);

        for (account_id, alias, key_preview) in accounts_for_provider {
            response.push(ProviderEntryResponse {
                provider: cfg.id.clone(),
                account_id,
                alias,
                key_preview: key_preview.clone(),
                base_url: if cfg.base_url.is_empty() {
                    None
                } else {
                    Some(cfg.base_url.clone())
                },
                default_model: cfg.models.first().map(|m| m.id.clone()),
                models: cfg.models.iter().map(|m| m.id.clone()).collect(),
                compact_model: cfg.compact_model.clone(),
                local: is_local,
                custom: cfg.custom,
                model_capabilities: {
                    let caps: HashMap<String, ModelCapabilitiesInfo> = cfg
                        .models
                        .iter()
                        .map(|m| (m.id.clone(), m.capabilities.clone()))
                        .collect();
                    if caps.is_empty() { None } else { Some(caps) }
                },
            });
        }
    }

    // Account rows for which there's no provider_list.json entry
    // (e.g. a key was added but the provider config hasn't been
    // populated yet). Surface them too so the user can see and edit
    // orphaned accounts.
    for (provider, accounts) in accounts_by_provider {
        for (account_id, alias, key_preview) in accounts {
            response.push(ProviderEntryResponse {
                provider: provider.clone(),
                account_id,
                key_preview,
                alias,
                base_url: None,
                default_model: None,
                models: Vec::new(),
                compact_model: None,
                local: false,
                custom: false,
                model_capabilities: None,
            });
        }
    }

    Ok(Json(response))
}

/// `GET /api/providers/{id}/logo` — proxy a provider's brand mark, with a
/// cache header the upstreams refuse to send.
///
/// The Desktop renders provider marks in the Harness picker and the chat
/// input's model menu. Pointed straight at the upstreams those are up to ~180
/// separate requests per full-list open, and both models.dev and Simple Icons
/// answer `Cache-Control: max-age=0, must-revalidate` — so every one of them
/// costs a round-trip on *every* open, and the list visibly fills in row by
/// row.
///
/// Relaying through the Gateway is what makes the second open free: the
/// response is served under our own origin, so the browser applies
/// `max-age=31536000, immutable` and never asks again. `immutable` is safe
/// because the URL is keyed by provider id — a changed logo is a new id, not
/// a stale cache entry.
///
/// `?src=si` selects the Simple Icons upstream (brand-tinted); the default is
/// models.dev (`currentColor`, i.e. black). The Desktop applies a dark-mode
/// invert only to the latter, and an `<img>` cannot read a response header, so
/// the distinction has to ride in the URL.
///
/// ponytail: no disk cache and no prefetch. The browser's HTTP cache is the
/// cache; a local store would duplicate it and add an invalidation problem
/// that never existed. First open is still slow by design — the bar is
/// "second open is not slow", and this is the smallest thing that meets it.
pub async fn provider_logo(
    Path(provider): Path<String>,
    Query(src): Query<LogoSource>,
) -> Result<Response, ApiError> {
    let url = logo_upstream_url(&provider, src.src.as_deref())?;

    let resp = logo_client()
        .get(url)
        .send()
        .await
        .map_err(|e| ApiError::internal(&format!("fetch provider logo failed: {e}")))?;

    if !resp.status().is_success() {
        // A short cache, NOT `immutable`: a miss here usually means only that
        // this upstream lacks the mark (the other one may still have it), and
        // pinning that negative for a year would freeze the miss in place.
        return Ok((
            StatusCode::NOT_FOUND,
            [(axum::http::header::CACHE_CONTROL, "public, max-age=3600")],
            "",
        )
            .into_response());
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::internal(&format!("read provider logo failed: {e}")))?;

    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                "image/svg+xml; charset=utf-8",
            ),
            (
                axum::http::header::CACHE_CONTROL,
                "public, max-age=31536000, immutable",
            ),
        ],
        bytes,
    )
        .into_response())
}

/// `?src=` selector for [`provider_logo`]. Absent or unrecognised falls back
/// to models.dev, the tier that actually carries marks for most ids.
///
/// The field is named `src` because that is what the Desktop sends. It
/// previously read `source`, which `#[serde(default)]` silently swallowed on
/// a `?src=si` request — so every tier-1 logo quietly answered from
/// models.dev (a monochrome `currentColor` mark, invisible on the Desktop's
/// dark surface) and never reached the `onError` that applies `dark:invert`.
#[derive(Deserialize)]
pub struct LogoSource {
    #[serde(default)]
    pub src: Option<String>,
}

/// Build the upstream URL for a logo request, or reject the provider id.
///
/// `provider` is caller-supplied and gets spliced into a remote URL, so this
/// is a trust boundary: without the character allowlist, `..`, a `/`, or a
/// whole `https://evil.example` value would aim the request somewhere other
/// than the two upstreams below. `source` is *matched*, never interpolated,
/// so an unrecognised value lands on the default rather than the URL builder.
fn logo_upstream_url(provider: &str, source: Option<&str>) -> Result<String, ApiError> {
    if provider.is_empty()
        || !provider
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
    {
        return Err(ApiError::bad_request(
            "provider id must be alphanumeric with - _ .",
        ));
    }
    // NOTE the asymmetric extensions: models.dev serves `<id>.svg`, but
    // Simple Icons' CDN path is the bare slug — `cdn.simpleicons.org/amd`
    // is 200 and `cdn.simpleicons.org/amd.svg` is 404. They look alike and
    // are not.
    Ok(match source {
        Some("si") => format!("https://cdn.simpleicons.org/{provider}"),
        _ => format!("https://models.dev/logos/{provider}.svg"),
    })
}

/// Long-lived client for logo fetches. Shared across requests so the TLS
/// handshake and connection pool are paid once, not per row in the list.
fn logo_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build provider logo HTTP client")
    })
}

/// `POST /api/providers` — add a provider (key + config).
///
/// API key → stored in encrypted Vault.
/// Config (base_url, models, compact_model, model_capabilities) → built from
/// request + offline capabilities, stored in provider_list.json via resource_cache.
pub async fn add_provider(
    State(state): State<AppState>,
    Json(body): Json<AddProviderRequest>,
) -> Result<(StatusCode, Json<ProviderSaveResponse>), ApiError> {
    // ADR-059 §7.3: reject stale writers before touching the vault.
    crate::http::routes::check_expected_version(&state, body.expected_version).await?;

    // Validate base_url format if provided
    if let Some(ref url) = body.base_url
        && !url.is_empty()
        && !url.starts_with("http://")
        && !url.starts_with("https://")
    {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://",
        ));
    }
    if body.provider.is_empty() {
        return Err(ApiError::bad_request("provider must not be empty"));
    }
    let is_local = models_api::is_local_provider(&body.provider);
    let is_custom = body.custom.unwrap_or(false);

    // Normalise `key` + `keys[]` into a single `[(alias, key)]` list.
    // Legacy single-key callers keep working; new callers pass `keys`
    // for multi-account setups.
    let key_entries: Vec<(Option<String>, String)> = if !body.keys.is_empty() {
        body.keys
            .iter()
            .map(|k| (k.alias.clone(), k.key.clone()))
            .collect()
    } else if !body.key.is_empty() || is_local || is_custom {
        vec![(None, body.key.clone())]
    } else {
        Vec::new()
    };
    if !is_local && !is_custom && key_entries.is_empty() {
        return Err(ApiError::bad_request("key must not be empty"));
    }

    let mut gw = state.gateway_state.write().await;

    // 1. Store each API key as a separate Vault account. Local / key-less
    //    custom providers still get a single default account so the
    //    Runtime can resolve a (provider, account) pair.
    for (alias, raw_key) in &key_entries {
        let effective_key = if is_local {
            "local".to_string()
        } else if is_custom && raw_key.is_empty() {
            "custom".to_string()
        } else {
            raw_key.clone()
        };
        gw.vault
            .add_account(&body.provider, alias.as_deref(), &effective_key)
            .map_err(|e| ApiError::internal(&format!("Failed to store key: {}", e)))?;
    }

    // 2. Resolve models list.
    let resolved_models: Vec<String> = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        vec![]
    };

    // 3. Build ProviderListItem (capabilities from offline_providers.json).
    let max_output_tokens = gw
        .config
        .as_ref()
        .map(|c| c.max_output_tokens_limit)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let mut item = resource_cache::build_provider_list_item(
        &body.provider,
        body.base_url.as_deref(),
        &resolved_models,
        body.compact_model.as_deref(),
        max_output_tokens,
        is_custom,
    );

    // 3b. Merge user-provided model_capabilities overrides (e.g. default_reasoning_effort).
    if let Some(ref user_caps) = body.model_capabilities {
        merge_user_capabilities(&mut item, user_caps);
    }

    // 3b2. ADR-103: re-adding an existing provider id keeps the stored
    //      protocol intent/verdicts of the models that are still configured.
    if let Some(prev) = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == body.provider)
        .cloned()
    {
        carry_over_protocol_state(&mut item, &prev);
    }

    // 3c. ADR-103 D1: record protocol intent. Effective protocol is still
    //     decided by the D1 gate, which needs a passing probe verdict.
    apply_protocol_intent(
        &mut item,
        body.protocol.clone(),
        body.model_protocols.as_ref(),
        body.model_base_urls.as_ref(),
    );

    // 4. Add to in-memory provider list (replace if already exists).
    resource_cache::remove_provider_from_memory(&mut gw, &body.provider);
    gw.resource_cache
        .provider_list
        .providers
        .push(item);

    // 5. Persist to disk and bump version.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::persist_provider_cache(&mut gw, &data_dir);
    let resource_version = gw.resource_cache.provider_list.version;
    drop(gw);

    // 5b. ADR-103 D12.1/D12.2: probe the `responses` rows this write
    //     introduced, and persist the verdicts. Runs after the save is
    //     durable (D12.5 — a probe can never fail the save) and outside the
    //     state lock, because probing is network I/O.
    let probes = crate::probe::run_save_time_probes(&state, &body.provider, None, false).await;

    // 6. Hot-push to running agents — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    // 7. ADR-059 §6: open a committed operation record so the client
    // can correlate this mutation by `operation_id` and observe the
    // resulting `resource_version`. The side effect (vault + disk)
    // already completed synchronously, so the record starts terminal-
    // ready in `Committed` and is swept after its deadline.
    let mut record = OperationRecord::new(body.expected_version.unwrap_or(0));
    record.state = OperationState::Committed;
    record.resource_version = Some(resource_version);
    let ack = OperationAck::from_record(&record);
    if let Some(store) = state.operation_store.as_ref() {
        store.insert(record);
    }

    Ok((
        StatusCode::CREATED,
        Json(ProviderSaveResponse { ack, probes }),
    ))
}

/// `DELETE /api/providers/:provider` — remove a provider (key + config).
pub async fn remove_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<MessageResponse>, ApiError> {
    let mut gw = state.gateway_state.write().await;

    // 1. Remove API keys from Vault. A provider can legitimately have no
    //    account left (every key removed, or a leftover id from a rename)
    //    and is still configured in `provider_list.json` — such a provider
    //    must stay deletable, so a missing key is not an error here. Only
    //    skip the vault call when we can *prove* there is nothing to
    //    remove; an enumeration failure keeps the strict path.
    let has_keys = gw
        .vault
        .list_keys()
        .map(|entries| entries.iter().any(|e| e.provider == provider))
        .unwrap_or(true);
    if has_keys {
        gw.vault.remove_key(&provider).map_err(|e| {
            ApiError::not_found(&format!("Key not found for provider '{}': {}", provider, e))
        })?;
    }

    // 2. Remove from in-memory provider list.
    resource_cache::remove_provider_from_memory(&mut gw, &provider);

    // 3. Persist to disk.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::persist_provider_cache(&mut gw, &data_dir);
    drop(gw);

    // 4. Hot-push — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("Key removed for provider: {}", provider),
    }))
}

/// `DELETE /api/providers/:provider/keys/:account_id` — remove ONE account
/// (API key) of a provider.
///
/// The provider's other accounts and its provider-level config (`base_url`,
/// `models`, `compact_model`) stay untouched. Removing the last account
/// leaves the provider listed but keyless — `list_providers` iterates the
/// account rows, so it drops out of the configured set and the provider
/// returns to the "not yet configured" list.
pub async fn remove_provider_account(
    State(state): State<AppState>,
    Path((provider, account_id)): Path<(String, String)>,
) -> Result<Json<MessageResponse>, ApiError> {
    let mut gw = state.gateway_state.write().await;

    // Vault::delete removes the on-disk entry as well, so no extra
    // persistence step here (unlike provider_list.json below).
    gw.vault
        .remove_account(&provider, &account_id)
        .map_err(|e| {
            ApiError::not_found(&format!(
                "Account '{}' not found for provider '{}': {}",
                account_id, provider, e
            ))
        })?;
    drop(gw);

    // Hot-push: running agents must stop receiving the removed account.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("Account {} removed for provider: {}", account_id, provider),
    }))
}

/// `PATCH /api/providers/:provider/keys/:account_id` — edit an existing
/// account's `alias` and/or `api_key`. At least one field must be
/// provided. Used by the harness edit dialog so users can rename or
/// rotate keys on already-configured accounts without losing the
/// account's UUID.
pub async fn update_provider_account(
    State(state): State<AppState>,
    Path((provider, account_id)): Path<(String, String)>,
    Json(body): Json<UpdateProviderAccountRequest>,
) -> Result<Json<MessageResponse>, ApiError> {
    if body.alias.is_none() && body.key.is_none() {
        return Err(ApiError::bad_request(
            "at least one of `alias` or `key` must be provided",
        ));
    }

    let mut gw = state.gateway_state.write().await;
    if let Some(ref new_alias) = body.alias {
        gw.vault
            .update_alias(&provider, &account_id, new_alias)
            .map_err(|e| {
                ApiError::not_found(&format!(
                    "Failed to update alias for {}/{}: {}",
                    provider, account_id, e
                ))
            })?;
    }
    if let Some(ref new_key) = body.key {
        gw.vault
            .update_account_key(&provider, &account_id, new_key)
            .map_err(|e| {
                ApiError::not_found(&format!(
                    "Failed to update key for {}/{}: {}",
                    provider, account_id, e
                ))
            })?;
    }
    drop(gw);

    // Hot-push: the account's alias is used by the model menu breadcrumb
    // when drilling into multi-key providers, so we must republish.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(MessageResponse {
        message: format!("Account {} updated for provider: {}", account_id, provider),
    }))
}

/// Request body for `PATCH /api/providers/:provider/keys/:account_id`.
///
/// Both fields are optional but at least one must be present — enforced
/// in the handler so we reject no-op calls early.
#[derive(Deserialize)]
pub struct UpdateProviderAccountRequest {
    /// New alias for the account. Trimmed server-side; must be non-empty.
    #[serde(default)]
    pub alias: Option<String>,
    /// New API key for the account. Trimmed server-side; must be non-empty.
    #[serde(default)]
    pub key: Option<String>,
}

/// `PUT /api/providers/:provider` — update a provider (key and/or config).
///
/// If `key` is None/empty, the existing Vault key is preserved.
/// If `models` is empty and `default_model` is None, existing models are
/// preserved from provider_list.json.
pub async fn update_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(body): Json<UpdateProviderRequest>,
) -> Result<Json<ProviderUpdateResponse>, ApiError> {
    if let Some(ref url) = body.base_url
        && !url.is_empty()
        && !url.starts_with("http://")
        && !url.starts_with("https://")
    {
        return Err(ApiError::bad_request(
            "base_url must start with http:// or https://",
        ));
    }

    let mut gw = state.gateway_state.write().await;

    // 0. ADR-103 D12.2/D12.4: the row as stored *before* this write. It decides
    //    which models count as "changed" (only those get probed) and whether the
    //    config changed enough to invalidate earlier probe verdicts.
    let previous_entry = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider)
        .cloned();

    // 1a. Legacy single-key update path — kept for backwards
    //     compatibility with pre-multi-account clients. When the caller
    //     provides `keys`, the per-account update still wins: it adds
    //     accounts instead of mutating the first one.
    let has_legacy_key_change = body.key.as_ref().is_some_and(|k| !k.is_empty());
    if has_legacy_key_change && body.keys.is_empty() {
        let api_key = body.key.as_ref().expect("checked above").clone();
        gw.vault
            .store_key(&provider, &api_key)
            .map_err(|e| ApiError::internal(&format!("Failed to update key: {}", e)))?;
    }

    // 1b. Multi-account path — append each entry as a fresh account.
    //     Empty keys are silently dropped so the dialog can keep
    //     stub rows around without contaminating the vault.
    for entry in &body.keys {
        let alias = entry.alias.as_deref();
        let api_key = entry.key.trim();
        if api_key.is_empty() {
            continue;
        }
        gw.vault
            .add_account(&provider, alias, api_key)
            .map_err(|e| ApiError::internal(&format!("Failed to add account: {}", e)))?;
    }

    // 2. Resolve models: provided > default_model > existing from cache.
    let resolved_models: Vec<String> = if !body.models.is_empty() {
        body.models.clone()
    } else if let Some(ref m) = body.default_model {
        vec![m.clone()]
    } else {
        // Preserve existing models from provider_list.json cache.
        gw.resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider)
            .map(|p| p.models.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default()
    };

    // 3. Resolve base_url: provided > existing from cache.
    let resolved_base_url = if body.base_url.is_some() {
        body.base_url.clone()
    } else {
        gw.resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|p| {
                if p.base_url.is_empty() {
                    None
                } else {
                    Some(p.base_url.clone())
                }
            })
    };

    // 4. Resolve compact_model: provided > existing from cache.
    let resolved_compact_model = if body.compact_model.is_some() {
        body.compact_model.clone()
    } else {
        gw.resource_cache
            .provider_list
            .providers
            .iter()
            .find(|p| p.id == provider)
            .and_then(|p| p.compact_model.clone())
    };

    // 5. Rebuild ProviderListItem (capabilities from offline_providers.json).
    let max_output_tokens = gw
        .config
        .as_ref()
        .map(|c| c.max_output_tokens_limit)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    // Preserve the existing custom flag from the stored provider entry.
    let is_custom = gw
        .resource_cache
        .provider_list
        .providers
        .iter()
        .find(|p| p.id == provider)
        .map(|p| p.custom)
        .unwrap_or(false);
    let mut item = resource_cache::build_provider_list_item(
        &provider,
        resolved_base_url.as_deref(),
        &resolved_models,
        resolved_compact_model.as_deref(),
        max_output_tokens,
        is_custom,
    );

    // 5b. Merge user-provided model_capabilities overrides (e.g. default_reasoning_effort).
    if let Some(ref user_caps) = body.model_capabilities {
        merge_user_capabilities(&mut item, user_caps);
    }

    // 5b2. ADR-103: keep the stored protocol intent + verdicts for models that
    //      survive the rebuild, so a partial update does not silently forget a
    //      `Responses` selection (or lose the verdict that gates it).
    if let Some(ref prev) = previous_entry {
        carry_over_protocol_state(&mut item, prev);
    }

    // 5c. ADR-103 D1: record protocol intent (see `apply_protocol_intent`).
    apply_protocol_intent(
        &mut item,
        body.protocol.clone(),
        body.model_protocols.as_ref(),
        body.model_base_urls.as_ref(),
    );

    // 5d. ADR-103 D12.4: does this write invalidate earlier probe verdicts?
    //     Only a moved probe target does — the base URL (here) or the key (a
    //     verdict describes the credentials it was made with). Adding or
    //     reordering models does not: it changes nothing about what the
    //     endpoint answers for the rows that stayed put.
    let key_changed = has_legacy_key_change || body.keys.iter().any(|k| !k.key.trim().is_empty());
    let probe_inputs_changed = previous_entry
        .as_ref()
        .is_some_and(|prev| crate::probe::service::probe_inputs_changed(prev, &item, key_changed));

    // 6. Replace in in-memory list.
    resource_cache::remove_provider_from_memory(&mut gw, &provider);
    gw.resource_cache
        .provider_list
        .providers
        .push(item);

    // 7. Persist to disk.
    let data_dir = get_data_dir_from_gw(&gw);
    resource_cache::persist_provider_cache(&mut gw, &data_dir);
    drop(gw);

    // 7b. ADR-103 D12: probe only the changed `responses` rows, then persist
    //     the verdicts. Already durable at this point, so a probe failure can
    //     never fail the save (D12.5).
    let probes = crate::probe::run_save_time_probes(
        &state,
        &provider,
        previous_entry,
        probe_inputs_changed,
    )
    .await;

    // 8. Hot-push — handled by MQTT publisher trigger below.
    // ADR-033: Trigger MQTT global resource republish after resource change.
    if let Some(ref trigger) = state.mqtt_publisher_trigger {
        trigger.trigger();
    }

    Ok(Json(ProviderUpdateResponse {
        message: format!("Key updated for provider: {}", provider),
        probes,
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────

/// Carry the stored protocol intent + probe verdicts over to a rebuilt row.
///
/// `build_provider_list_item` reconstructs every model from offline data, which
/// drops the ADR-103 fields (`protocol_type`, `base_url`, `protocol_probe`).
/// A partial update (rename, capability tweak) must not silently forget that
/// the user selected `Responses` for a model, nor lose the verdict that gates
/// it — so the previous state is copied back for every model that survives the
/// rebuild. Request-supplied intent is applied *after* this and wins.
fn carry_over_protocol_state(
    item: &mut acowork_core::protocol::ProviderListItem,
    previous: &acowork_core::protocol::ProviderListItem,
) {
    item.protocol_type = previous.protocol_type.clone();

    for model in &mut item.models {
        let Some(prev) = previous.models.iter().find(|m| m.id == model.id) else {
            continue;
        };
        model.protocol_type = prev.protocol_type.clone();
        model.base_url = prev.base_url.clone();
        model.protocol_probe = prev.protocol_probe.clone();
    }
}

/// Apply ADR-103 D1 protocol intent onto a freshly built provider item.
///
/// This records **intent** only. Whether `Responses` becomes the effective
/// protocol is decided exclusively by the D1 gate
/// (`models_api::resolve_protocol`), which additionally requires a passing
/// probe verdict — so writing intent here can never change live traffic on its
/// own (ADR-103 D2 two-stage rollout).
///
/// An id in `model_protocols` / `model_base_urls` that the resolved model list
/// does not contain is dropped with a warning rather than resurrected: the
/// model list is the source of truth for which models exist.
fn apply_protocol_intent(
    item: &mut acowork_core::protocol::ProviderListItem,
    protocol: Option<ProtocolType>,
    model_protocols: Option<&HashMap<String, Option<ProtocolType>>>,
    model_base_urls: Option<&HashMap<String, String>>,
) {
    if let Some(protocol) = protocol {
        item.protocol_type = protocol;
    }

    for model in &mut item.models {
        if let Some(protocol) = model_protocols.and_then(|m| m.get(&model.id)) {
            match protocol {
                Some(protocol) => model.protocol_type = Some(protocol.clone()),
                // "Follow the data": the user withdrew the selection, so the
                // probe verdict that gated it is meaningless — drop it rather
                // than leaving a stale yellow badge behind (D11).
                None => {
                    model.protocol_type = None;
                    model.protocol_probe = None;
                }
            }
        }
        if let Some(url) = model_base_urls.and_then(|m| m.get(&model.id)) {
            let trimmed = url.trim();
            model.base_url = (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
    }

    let known: std::collections::HashSet<&str> =
        item.models.iter().map(|m| m.id.as_str()).collect();
    let declared_ids = model_protocols
        .map(|m| m.keys())
        .into_iter()
        .flatten()
        .chain(model_base_urls.map(|m| m.keys()).into_iter().flatten());
    for per_model in declared_ids {
        if !known.contains(per_model.as_str()) {
            tracing::warn!(
                provider_id = %item.id,
                model_id = %per_model,
                "protocol intent for an unconfigured model ignored"
            );
        }
    }
}

/// Merge user-provided model capabilities overrides into a ProviderListItem.
///
/// For each model ID present in `user_caps`:
/// - If the model already exists in `item.models`, user-set fields override
///   the offline data (only non-None user fields are applied).
/// - If the model is not in `item.models`, the override is silently ignored
///   (only configured models can have overrides).
fn merge_user_capabilities(
    item: &mut acowork_core::protocol::ProviderListItem,
    user_caps: &HashMap<String, ModelCapabilitiesInfo>,
) {
    for model_entry in &mut item.models {
        if let Some(user_cap) = user_caps.get(&model_entry.id) {
            // Core limits — always override (frontend always sends these)
            model_entry.capabilities.context_window = user_cap.context_window;
            model_entry.capabilities.max_output_tokens = user_cap.max_output_tokens;
            model_entry.capabilities.supports_tool_calling = user_cap.supports_tool_calling;

            // Optional fields — override only when the user explicitly set them
            if user_cap.modalities.is_some() {
                model_entry.capabilities.modalities = user_cap.modalities.clone();
            }
            if user_cap.supports_attachment.is_some() {
                model_entry.capabilities.supports_attachment = user_cap.supports_attachment;
            }
            if user_cap.default_reasoning_effort.is_some() {
                model_entry.capabilities.default_reasoning_effort =
                    user_cap.default_reasoning_effort.clone();
            }
            if user_cap.thinking_mode.is_some() {
                model_entry.capabilities.thinking_mode = user_cap.thinking_mode.clone();
            }
            if user_cap.supports_reasoning.is_some() {
                model_entry.capabilities.supports_reasoning = user_cap.supports_reasoning;
            }
            if user_cap.supports_temperature.is_some() {
                model_entry.capabilities.supports_temperature = user_cap.supports_temperature;
            }
        }
    }
}

/// `POST /api/providers/:provider/probe` — ADR-103 D10.1 explicit probe.
///
/// Probes one configured model and writes the verdict back, so the D1 gate and
/// the D11 badge both reflect it. Deliberately independent of the platform gate
/// `llm.responses_api_enabled`: a user must be able to find out whether a
/// provider supports Responses *before* the platform enables the feature (see
/// [`crate::probe::service`]).
///
/// Probing a failing endpoint is a **200 with `success: false`**, not an HTTP
/// error: "this endpoint does not speak Responses" is the answer the caller
/// asked for. Transport/config problems that prevent the probe from running at
/// all (unknown provider, unknown model, no base URL, unsupported protocol) are
/// 4xx.
pub async fn probe_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<ProbeQuery>,
    Json(body): Json<ProbeProviderRequest>,
) -> Result<Json<ProbeResponse>, ApiError> {
    let protocol = body
        .protocol
        .clone()
        .unwrap_or_else(|| ProtocolType::Responses.to_string());
    // L3 is the minimum for a verdict (D10.2), so a user-initiated probe never
    // runs the cheap tier: the whole point of clicking "verify" is the positive
    // verdict that unlocks the `R` badge.
    let mut tier = body.tier.unwrap_or(crate::probe::ProbeTier::Deep);
    if tier != crate::probe::ProbeTier::Deep {
        tracing::debug!(
            provider_id = %provider,
            model = %body.model,
            "explicit probe upgraded to the deep tier: L3 is required for a verdict"
        );
        tier = crate::probe::ProbeTier::Deep;
    }

    let outcome = crate::probe::probe_one(
        &state,
        &provider,
        &body.model,
        &protocol,
        tier,
        query.force.unwrap_or(false),
    )
    .await?;

    Ok(Json(ProbeResponse {
        provider,
        model: body.model,
        protocol,
        tier,
        state: outcome.state(),
        outcome,
        stripped_fields: Vec::new(),
    }))
}

/// Get data_dir from GatewayState config.
pub(crate) fn get_data_dir_from_gw(gw: &crate::gateway::state::GatewayState) -> PathBuf {
    gw.config
        .as_ref()
        .map(|c| PathBuf::from(&c.data_dir))
        .unwrap_or_else(|| PathBuf::from("./data"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A keyless provider must stay visible in the UI (one empty-account
    /// row) so it can be deleted — otherwise it lingers in
    /// `provider_list.json` and in every Runtime's provider list.
    #[test]
    fn account_rows_surface_keyless_non_local_provider() {
        // Local provider: placeholder row, as before.
        assert_eq!(
            account_rows_for_provider(true, None),
            vec![(String::new(), String::new(), "(local)".to_string())]
        );
        // Non-local provider with no vault account: still one row.
        assert_eq!(
            account_rows_for_provider(false, None),
            vec![(String::new(), String::new(), "(no key)".to_string())]
        );
        // Accounts present: handed through untouched.
        let accounts = vec![("acc-1".to_string(), "alias".to_string(), "sk-...123".to_string())];
        assert_eq!(
            account_rows_for_provider(false, Some(accounts.clone())),
            accounts
        );
    }

    #[test]
    fn test_add_provider_request_deserialization() {
        let json = r#"{"provider": "openai", "key": "sk-12345"}"#;
        let req: AddProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.provider, "openai");
        assert_eq!(req.key, "sk-12345");
        assert!(req.base_url.is_none());
        assert!(req.default_model.is_none());
    }

    #[test]
    fn test_add_provider_request_with_full_config() {
        let json = r#"{"provider": "deepseek", "key": "sk-abc", "base_url": "https://api.deepseek.com/v1", "default_model": "deepseek-chat"}"#;
        let req: AddProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.provider, "deepseek");
        assert_eq!(req.key, "sk-abc");
        assert_eq!(
            req.base_url,
            Some("https://api.deepseek.com/v1".to_string())
        );
        assert_eq!(req.default_model, Some("deepseek-chat".to_string()));
    }

    #[test]
    fn test_update_provider_request_deserialization() {
        let json = r#"{"key": "sk-new-key"}"#;
        let req: UpdateProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, Some("sk-new-key".to_string()));
        assert!(req.base_url.is_none());
        assert!(req.default_model.is_none());
    }

    #[test]
    fn test_update_provider_request_with_full_config() {
        let json = r#"{"key": "sk-new", "base_url": "https://api.custom.com/v1", "default_model": "custom-model"}"#;
        let req: UpdateProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.key, Some("sk-new".to_string()));
        assert_eq!(req.base_url, Some("https://api.custom.com/v1".to_string()));
        assert_eq!(req.default_model, Some("custom-model".to_string()));
    }

    #[test]
    fn test_update_provider_request_accepts_keys_array() {
        // The edit dialog sends `keys` to append accounts in one round-trip.
        let json = r#"{
            "keys": [
                {"alias": "work", "key": "sk-work"},
                {"alias": "personal", "key": "sk-personal"}
            ],
            "base_url": "https://api.example.com/v1"
        }"#;
        let req: UpdateProviderRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.keys.len(), 2);
        assert_eq!(req.keys[0].alias.as_deref(), Some("work"));
        assert_eq!(req.keys[0].key, "sk-work");
        assert_eq!(req.keys[1].alias.as_deref(), Some("personal"));
        assert_eq!(req.keys[1].key, "sk-personal");
        assert_eq!(req.base_url.as_deref(), Some("https://api.example.com/v1"));
        // `key` defaults to None when only `keys` is sent.
        assert!(req.key.is_none());
    }

    #[test]
    fn test_update_provider_account_request_alias_only() {
        let json = r#"{"alias": "prod"}"#;
        let req: UpdateProviderAccountRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.alias.as_deref(), Some("prod"));
        assert!(req.key.is_none());
    }

    #[test]
    fn test_update_provider_account_request_key_only() {
        let json = r#"{"key": "sk-rotated"}"#;
        let req: UpdateProviderAccountRequest = serde_json::from_str(json).unwrap();
        assert!(req.alias.is_none());
        assert_eq!(req.key.as_deref(), Some("sk-rotated"));
    }

    #[test]
    fn test_update_provider_account_request_both_fields() {
        let json = r#"{"alias": "prod", "key": "sk-rotated"}"#;
        let req: UpdateProviderAccountRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.alias.as_deref(), Some("prod"));
        assert_eq!(req.key.as_deref(), Some("sk-rotated"));
    }


    #[test]
    fn logo_source_accepts_the_query_name_the_desktop_actually_sends() {
        // Regression: the struct field was named `source` but the Desktop
        // requests `?src=si`. `#[serde(default)]` swallowed the unknown key
        // instead of erroring, so every tier-1 logo silently came from
        // models.dev — a black `currentColor` mark, invisible on dark
        // surfaces, and never reaching the Desktop's `onError` that applies
        // `dark:invert`. The `logo_upstream_url` unit tests could not catch
        // this: they call it directly, bypassing the query round trip.
        //
        // Parsed through the same `axum::extract::Query` wrapper the handler
        // uses, so this stays a test — not a new dependency.
        use axum::extract::Query;
        use axum::http::Uri;

        let parse = |q: &str| -> LogoSource {
            let uri: Uri = format!("http://t/logo?{q}").parse().unwrap();
            Query::<LogoSource>::try_from_uri(&uri)
                .map(|Query(v)| v)
                .unwrap_or_else(|e| panic!("{q:?} must deserialize into LogoSource: {e}"))
        };

        // `amd` is the probe: Simple Icons ships it in brand red (#ED1C24)
        // while models.dev serves the same shape in `currentColor`, so the
        // chosen upstream is observable in the returned URL.
        let si = parse("src=si");
        assert_eq!(si.src.as_deref(), Some("si"));
        assert_eq!(
            logo_upstream_url("amd", si.src.as_deref()).unwrap(),
            "https://cdn.simpleicons.org/amd"
        );

        // Absent still means the models.dev default, not an error.
        let none = parse("");
        assert!(none.src.is_none());
        assert_eq!(
            logo_upstream_url("anthropic", none.src.as_deref()).unwrap(),
            "https://models.dev/logos/anthropic.svg"
        );

        // An unrecognised value is not interpolated; it lands on the default.
        let bogus = parse("src=https://evil.example");
        assert_eq!(
            logo_upstream_url("anthropic", bogus.src.as_deref()).unwrap(),
            "https://models.dev/logos/anthropic.svg"
        );
    }

    #[test]
    fn logo_upstream_url_only_ever_targets_the_two_known_hosts() {
        // The provider id is spliced into a remote URL, so anything that can
        // steer that URL is a request-forgery hole: '..' walks the path, a
        // '/' adds a segment, and a scheme makes it a whole different origin.
        for bad in [
            "",
            "../secret",
            "a/b",
            "https://evil.example/x",
            "anthropic?x=1",
            "anthropic#frag",
            "an thropic",
        ] {
            assert!(
                logo_upstream_url(bad, None).is_err(),
                "provider id {bad:?} must be rejected"
            );
        }

        // Real ids resolve, and the two tiers hit their own host. The
        // extensions differ per upstream and are NOT interchangeable:
        // models.dev wants `<id>.svg`, Simple Icons' CDN wants the bare slug.
        assert_eq!(
            logo_upstream_url("anthropic", None).unwrap(),
            "https://models.dev/logos/anthropic.svg"
        );
        assert_eq!(
            logo_upstream_url("amd", Some("si")).unwrap(),
            "https://cdn.simpleicons.org/amd"
        );
        // An unknown `src` is not interpolated into the URL — it falls
        // back to the default upstream.
        assert_eq!(
            logo_upstream_url("anthropic", Some("https://evil.example")).unwrap(),
            "https://models.dev/logos/anthropic.svg"
        );
    }
}
