//! LLM Provider router and factory
//!
//! Creates the appropriate Provider based on protocol type.
//! Supports OpenAI-compatible, Anthropic, Google Gemini, and Ollama protocols.
//!
//! DESIGN: Runtime always runs under Gateway. base_url and api_key are
//! delivered via ProviderListUpdate (and the bundled AgentHelloConfig)
//! from Gateway, which has full models.dev offline data. Protocol
//! selection is data-driven via ProtocolType — no hardcoded provider
//! name matching.
//!
//! If Gateway does not deliver a usable provider/model, Runtime refuses
//! service with a clear error — no silent fallbacks.

use std::sync::Arc;

use acowork_core::ProtocolType;
use std::time::Duration;

use acowork_core::providers::traits::Provider;

use crate::providers::anthropic::AnthropicProvider;
use crate::providers::ollama::OllamaProvider;
use crate::providers::openai::OpenAIProvider;

/// Bundle of timeout values passed from RuntimeConfig to provider constructors.
#[derive(Debug, Clone, Copy)]
pub struct ProviderTimeouts {
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    pub stream_read_timeout: Duration,
}

impl From<&crate::config::RuntimeConfig> for ProviderTimeouts {
    fn from(config: &crate::config::RuntimeConfig) -> Self {
        Self {
            request_timeout: Duration::from_millis(config.timeouts.provider_request_timeout_ms),
            connect_timeout: Duration::from_millis(config.timeouts.provider_connect_timeout_ms),
            stream_read_timeout: Duration::from_millis(
                config.timeouts.provider_stream_read_timeout_ms,
            ),
        }
    }
}

impl ProviderTimeouts {
    /// Default timeouts (10min request, 10s connect, 45s stream read).
    pub fn defaults() -> Self {
        Self {
            request_timeout: Duration::from_secs(600),
            connect_timeout: Duration::from_secs(10),
            stream_read_timeout: Duration::from_secs(45),
        }
    }
}

/// Bundle of optional wiring passed to [`create_provider_with_wiring`].
///
/// `provider_id` is the stable cache key prefix used by `CompatCache`
/// (typically the same as `provider_name`).  When `Some`, it is installed
/// on the resulting `OpenAIProvider` so cache lookups namespace correctly.
///
/// `compat_cache` is the optional compatibility cache.  When `Some`, it
/// is installed on the resulting `OpenAIProvider` so fallback
/// successes are persisted across processes.
///
/// `responses_compat_cache` is the same idea for the `/responses` shape
/// (ADR-103 D8/S7). It is a **separate** cache holding a separate payload
/// type: a completions `StripProfile` and a Responses `ResponsesTrim` are not
/// interchangeable, and silently reading one as the other is exactly the kind
/// of cross-shape leak D7.3 forbids.
#[derive(Default, Clone)]
pub struct ProviderWiring {
    pub provider_id: Option<String>,
    pub compat_cache: Option<Arc<crate::providers::compat::CompatCache>>,
    pub responses_compat_cache:
        Option<Arc<crate::providers::responses_compat::ResponsesCompatCache>>,
}

/// Create a provider based on protocol type.
///
/// Protocol selection is data-driven: Gateway determines the correct
/// ProtocolType from offline provider metadata and delivers it via
/// ProviderListUpdate. Runtime simply instantiates the matching provider.
///
/// base_url is always supplied by the Gateway-delivered provider list.
/// If missing, the provider will likely fail - this is expected since
/// Runtime cannot function without Gateway.
///
/// Without wiring, the provider behaves exactly as before the
/// compatibility-cache work — used by unit tests and standalone mode.
pub fn create_provider(
    provider_name: &str,
    protocol_type: &ProtocolType,
    api_key: Option<&str>,
    base_url: Option<&str>,
    timeouts: Option<ProviderTimeouts>,
) -> Arc<dyn Provider> {
    create_provider_with_wiring(
        provider_name,
        protocol_type,
        api_key,
        base_url,
        timeouts,
        ProviderWiring::default(),
    )
}

/// Same as [`create_provider`] but additionally installs `provider_id`
/// and `compat_cache` on the constructed provider before returning.
pub fn create_provider_with_wiring(
    provider_name: &str,
    protocol_type: &ProtocolType,
    api_key: Option<&str>,
    base_url: Option<&str>,
    timeouts: Option<ProviderTimeouts>,
    wiring: ProviderWiring,
) -> Arc<dyn Provider> {
    let t = timeouts.unwrap_or_else(ProviderTimeouts::defaults);
    match protocol_type {
        ProtocolType::Anthropic => {
            tracing::info!(
                provider = provider_name,
                "Using Anthropic protocol provider"
            );
            let provider = if let Some(url) = base_url {
                AnthropicProvider::with_base_url_and_timeouts(
                    Some(url),
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                )
            } else {
                AnthropicProvider::with_base_url_and_timeouts(
                    None,
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                )
            };
            Arc::new(provider)
        }

        ProtocolType::Google => {
            // Google Gemini uses OpenAI-compatible protocol via OpenAIProvider
            // until a native GoogleProvider is implemented.
            tracing::info!(
                provider = provider_name,
                "Using Google Gemini (OpenAI-compatible) provider"
            );
            let mut provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url_and_timeouts(
                    Some(url),
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                )
            } else {
                OpenAIProvider::with_base_url_and_timeouts(
                    None,
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                )
            };
            install_wiring(&mut provider, &wiring);
            Arc::new(provider)
        }

        ProtocolType::Ollama => {
            let provider = if let Some(url) = base_url {
                OllamaProvider::with_base_url_and_timeouts(
                    Some(url),
                    t.request_timeout,
                    t.connect_timeout,
                )
            } else {
                OllamaProvider::with_base_url_and_timeouts(
                    None,
                    t.request_timeout,
                    t.connect_timeout,
                )
            };
            Arc::new(provider)
        }

        ProtocolType::OpenAI | ProtocolType::Unknown => {
            // ADR-103 D3: `Unknown` is the read-side tolerance value for an
            // unrecognized wire protocol. It must never silently masquerade
            // as the status quo — make the fallback visible, then behave as
            // OpenAI-compatible (the historical default).
            if matches!(protocol_type, ProtocolType::Unknown) {
                tracing::warn!(
                    provider = provider_name,
                    "unrecognized protocol_type delivered; falling back to OpenAI-compatible provider (ADR-103 D3)"
                );
            }
            tracing::info!(provider = provider_name, "Using OpenAI-compatible provider");
            let mut provider = if let Some(url) = base_url {
                OpenAIProvider::with_base_url_and_timeouts(
                    Some(url),
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                )
            } else {
                OpenAIProvider::with_base_url_and_timeouts(
                    None,
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                )
            };
            install_wiring(&mut provider, &wiring);
            Arc::new(provider)
        }

        ProtocolType::Responses => {
            // ADR-103 D4/D8: standalone Responses provider. The
            // completions-shaped `CompatCache` is NOT installed (its profile
            // type and fallback chain do not apply here); the Responses cache
            // carries the single-field D8 trim instead (S7).
            tracing::info!(
                provider = provider_name,
                "Using OpenAI Responses protocol provider"
            );
            let mut provider =
                crate::providers::responses::ResponsesProvider::with_base_url_and_timeouts(
                    base_url,
                    api_key,
                    t.request_timeout,
                    t.connect_timeout,
                    t.stream_read_timeout,
                );
            install_responses_wiring(&mut provider, &wiring);
            Arc::new(provider)
        }
    }
}

/// ADR-103 D2 — model-level two-stage input for provider construction.
///
/// Selects the construction branch via `model.protocol_type.or(provider
/// .protocol_type)` and the per-model base URL override. The value delivered
/// at model level is the Gateway-resolved *effective* protocol (§3 red line
/// 1) — this function only consumes it, it never re-derives or gates.
///
/// When `model_id` is given but no matching entry exists, falls back to the
/// provider level with a `warn!` (D2 MUST: never guess).
pub fn effective_protocol_and_base(
    provider: &acowork_core::protocol::ProviderListItem,
    model_id: Option<&str>,
) -> (ProtocolType, String) {
    let model = model_id.and_then(|mid| provider.models.iter().find(|m| m.id == mid));

    if model.is_none()
        && let Some(mid) = model_id
    {
        tracing::warn!(
            provider_id = %provider.id,
            model_id = %mid,
            "model entry not found in provider list; falling back to provider-level protocol/base_url"
        );
    }

    let protocol = model
        .and_then(|m| m.protocol_type.clone())
        .unwrap_or_else(|| provider.protocol_type.clone());

    // ADR-103 D7: one resolution order for every layer — explicit user
    // override > model-level override > provider-level `api` > built-in
    // default (owned by the provider constructor). An unexpanded `${VAR}`
    // disqualifies its layer instead of being expanded (Q6).
    let resolved = acowork_core::providers::url_resolve::resolve_base_url(
        None,
        model.and_then(|m| m.base_url.as_deref()),
        Some(provider.base_url.as_str()),
    );
    if let Some(layer) = resolved.layer {
        tracing::debug!(
            provider_id = %provider.id,
            model_id = model_id.unwrap_or(""),
            layer = layer.as_str(),
            base_url = resolved.url.as_deref().unwrap_or(""),
            "base URL resolved (ADR-103 D7)"
        );
    }

    (protocol, resolved.url.unwrap_or_default())
}

/// Install the D8 trim cache on the Responses provider.
///
/// Both halves are required: without a provider id the cache key would have no
/// namespace, so the cache stays off rather than being shared across providers.
fn install_responses_wiring(
    provider: &mut crate::providers::responses::ResponsesProvider,
    wiring: &ProviderWiring,
) {
    if let (Some(id), Some(cache)) = (
        wiring.provider_id.clone(),
        wiring.responses_compat_cache.clone(),
    ) {
        provider.set_compat_wiring(id, cache);
    }
}

fn install_wiring(provider: &mut OpenAIProvider, wiring: &ProviderWiring) {
    if let Some(id) = wiring.provider_id.clone() {
        provider.set_provider_id(id);
    }
    if let Some(cache) = wiring.compat_cache.clone() {
        provider.set_compat_cache(cache);
    }
}

/// Infer protocol type from provider name for standalone mode.
///
/// In standalone mode (no Gateway), there is no offline metadata to
/// determine protocol type. This function provides a fallback inference
/// based on the provider name. Gateway mode should always use the
/// protocol_type from the Gateway-delivered provider list instead.
pub fn infer_protocol_type(provider_name: &str) -> ProtocolType {
    match provider_name {
        "anthropic" | "claude" => ProtocolType::Anthropic,
        "ollama" => ProtocolType::Ollama,
        "google" | "gemini" => ProtocolType::Google,
        _ => ProtocolType::OpenAI,
    }
}

/// Create a no-op provider that always returns an error.
/// Used when no LLM config is available (Gateway mode without API key).
pub fn create_noop_provider() -> Arc<dyn Provider> {
    Arc::new(NoopProvider)
}

/// A provider that always returns an error, used when no LLM config is available.
struct NoopProvider;

#[async_trait::async_trait]
impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "noop"
    }

    async fn chat(
        &self,
        _request: acowork_core::providers::traits::ChatRequest,
    ) -> acowork_core::error::Result<acowork_core::providers::traits::ChatResponse> {
        Err(acowork_core::error::AcoworkError::Provider(
            acowork_core::providers::traits::ProviderError::unknown(
                "No LLM provider configured. Please add an API key in Desktop App Settings."
                    .to_string(),
            ),
        ))
    }

    async fn chat_stream(
        &self,
        _request: acowork_core::providers::traits::ChatRequest,
    ) -> acowork_core::error::Result<
        Box<dyn futures_core::Stream<Item = acowork_core::providers::traits::StreamEvent> + Send>,
    > {
        Err(acowork_core::error::AcoworkError::Provider(
            acowork_core::providers::traits::ProviderError::unknown(
                "No LLM provider configured. Please add an API key in Desktop App Settings."
                    .to_string(),
            ),
        ))
    }

    async fn chat_token_count(
        &self,
        _messages: &[acowork_core::providers::traits::ChatMessage],
    ) -> acowork_core::error::Result<u64> {
        Err(acowork_core::error::AcoworkError::Provider(
            acowork_core::providers::traits::ProviderError::unknown(
                "No LLM provider configured.".to_string(),
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_openai_provider() {
        let provider =
            create_provider("openai", &ProtocolType::OpenAI, Some("sk-test"), None, None);
        assert_eq!(provider.name(), "openai");
    }

    #[test]
    fn test_create_anthropic_provider() {
        let provider = create_provider(
            "anthropic",
            &ProtocolType::Anthropic,
            Some("sk-ant-test"),
            None,
            None,
        );
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn test_create_ollama_provider() {
        let provider = create_provider("ollama", &ProtocolType::Ollama, None, None, None);
        assert_eq!(provider.name(), "ollama");
    }

    #[test]
    fn test_create_deepseek_provider() {
        let provider = create_provider(
            "deepseek",
            &ProtocolType::OpenAI,
            Some("sk-test"),
            None,
            None,
        );
        assert_eq!(provider.name(), "openai"); // Uses OpenAI-compatible protocol
    }

    #[test]
    fn test_infer_protocol_type() {
        assert_eq!(infer_protocol_type("anthropic"), ProtocolType::Anthropic);
        assert_eq!(infer_protocol_type("claude"), ProtocolType::Anthropic);
        assert_eq!(infer_protocol_type("minimax"), ProtocolType::OpenAI);
        assert_eq!(infer_protocol_type("minimax-cn"), ProtocolType::OpenAI);
        assert_eq!(infer_protocol_type("ollama"), ProtocolType::Ollama);
        assert_eq!(infer_protocol_type("openai"), ProtocolType::OpenAI);
        assert_eq!(infer_protocol_type("deepseek"), ProtocolType::OpenAI);
        assert_eq!(
            infer_protocol_type("unknown-provider"),
            ProtocolType::OpenAI
        );
    }
}

// ── ADR-103 S1: router construction-branch tests ──────────────────────

#[cfg(test)]
mod adr103_router_tests {
    use super::*;
    use acowork_core::protocol::{ProbeState, ProtocolProbe, ProviderModelEntry};

    fn model(
        id: &str,
        proto: Option<ProtocolType>,
        base: Option<&str>,
    ) -> ProviderModelEntry {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "capabilities": { "context_window": 1000u64, "max_output_tokens": 100u64 },
            "max_output_tokens_limit": 4096u64,
            "protocol_type": proto,
            "base_url": base,
        }))
        .unwrap()
    }

    fn provider(proto: ProtocolType, models: Vec<ProviderModelEntry>) -> acowork_core::protocol::ProviderListItem {
        acowork_core::protocol::ProviderListItem {
            id: "p".into(),
            base_url: "https://provider/v1".into(),
            protocol_type: proto,
            models,
            compact_model: None,
            custom: false,
        }
    }

    #[test]
    fn responses_branch_reaches_the_responses_provider_without_compat_wiring() {
        // Wiring carries a provider id; compat_cache is deliberately None
        // here because the Responses branch MUST NOT install it (D4) — the
        // branch's non-use is pinned by construction, not by a live cache.
        let wiring = ProviderWiring {
            provider_id: Some("p".into()),
            compat_cache: None,
            responses_compat_cache: None,
        };
        let p = create_provider_with_wiring(
            "openai",
            &ProtocolType::Responses,
            Some("k"),
            Some("https://api.openai.com/v1"),
            None,
            wiring,
        );
        assert_eq!(p.name(), "openai-responses");
    }

    #[test]
    fn unknown_branch_falls_back_to_openai_compatible() {
        let p = create_provider_with_wiring(
            "mystery",
            &ProtocolType::Unknown,
            Some("k"),
            None,
            None,
            ProviderWiring::default(),
        );
        assert_eq!(p.name(), "openai");
    }

    #[test]
    fn effective_protocol_inherits_and_overrides() {
        // No model-level fields -> provider level, provider base_url.
        let p = provider(ProtocolType::Anthropic, vec![model("m1", None, None)]);
        let (proto, base) = effective_protocol_and_base(&p, Some("m1"));
        assert_eq!(proto, ProtocolType::Anthropic);
        assert_eq!(base, "https://provider/v1");
        // Model-level Responses (Gateway-resolved effective value) + base override.
        let p = provider(
            ProtocolType::OpenAI,
            vec![model("m1", Some(ProtocolType::Responses), Some("https://mm/v1"))],
        );
        let (proto, base) = effective_protocol_and_base(&p, Some("m1"));
        assert_eq!(proto, ProtocolType::Responses);
        assert_eq!(base, "https://mm/v1");
        // Unknown model id -> provider level + warn (never guess).
        let (proto, base) = effective_protocol_and_base(&p, Some("nope"));
        assert_eq!(proto, ProtocolType::OpenAI);
        assert_eq!(base, "https://provider/v1");
        // No model id at all -> provider level, silent.
        let (proto, _) = effective_protocol_and_base(&p, None);
        assert_eq!(proto, ProtocolType::OpenAI);
    }

    #[test]
    fn unexpanded_placeholder_base_url_falls_back_to_the_provider_level() {
        // ADR-103 D7.2: `${VAR}` is never expanded at runtime; the layer is
        // dropped and the next one applies. Silent inheritance would hide a
        // mis-configured catalog entry.
        let p = provider(
            ProtocolType::OpenAI,
            vec![model(
                "m1",
                Some(ProtocolType::Responses),
                Some("${NEON_AI_GATEWAY_BASE_URL}/v1"),
            )],
        );
        let (proto, base) = effective_protocol_and_base(&p, Some("m1"));
        assert_eq!(proto, ProtocolType::Responses);
        assert_eq!(base, "https://provider/v1");
    }

    #[test]
    fn empty_model_base_url_is_skipped_not_used() {
        let p = provider(ProtocolType::OpenAI, vec![model("m1", None, Some(""))]);
        let (_, base) = effective_protocol_and_base(&p, Some("m1"));
        assert_eq!(base, "https://provider/v1");
    }

    #[test]
    fn runtime_never_uses_probe_even_if_present() {
        // Defense-in-depth: even if a probe somehow rode along in the domain
        // entry, construction consumes only protocol_type/base_url — there is
        // no probe input to effective_protocol_and_base at all. The type
        // signature is the guarantee; this test pins the field presence.
        let mut m = model("m1", Some(ProtocolType::Responses), None);
        m.protocol_probe = Some(ProtocolProbe {
            state: ProbeState::Failed,
            tier_passed: vec![],
            reason: None,
            observed_base_url: None,
            probed_at: None,
            stripped_fields: vec![],
        });
        let p = provider(ProtocolType::OpenAI, vec![m]);
        let (proto, _) = effective_protocol_and_base(&p, Some("m1"));
        // Runtime consumes the Gateway-resolved value verbatim (red line 1):
        // re-gating here would be a second decision point.
        assert_eq!(proto, ProtocolType::Responses);
    }
}
