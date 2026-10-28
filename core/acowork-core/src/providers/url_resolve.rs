//! ADR-103 D7 — base URL resolution for the Responses wire format.
//!
//! Resolution order (highest priority first):
//!
//! 1. an explicit override supplied by the caller (user-level setting),
//! 2. the model-level `base_url` override (`ProviderModelEntry::base_url`),
//! 3. the provider-level `api`,
//! 4. the provider's built-in default (returned as `None`; whoever builds the
//!    client owns that default).
//!
//! Two realities the ADR calls out explicitly:
//!
//! * **`/v1` is not uniform.** `api.deepseek.com` + `/responses` is right while
//!   MiniMax needs `api.minimax.cn/v1`, so a model-level override that owns the
//!   whole base is mandatory — never "provider base + `/responses`" on faith.
//! * **`${VAR}` placeholders are not expanded here.** Gateway-side catalogs
//!   contain values like `${NEON_AI_GATEWAY_BASE_URL}/v1`. Expanding them would
//!   turn provider configuration into an implicit dependency on the Gateway
//!   process environment, which is out of bounds (ADR-103 Q6), so an
//!   unexpanded placeholder disqualifies that layer: fall through to the next
//!   one and warn.

use tracing::warn;

/// Which layer produced the resolved base URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlLayer {
    /// Caller-supplied explicit override (user setting).
    Explicit,
    /// Model-level override from the provider catalog.
    Model,
    /// Provider-level `api`.
    Provider,
}

impl BaseUrlLayer {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::Model => "model",
            Self::Provider => "provider",
        }
    }
}

/// Result of base URL resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedBaseUrl {
    /// The winning base URL, or `None` when every layer was absent/unusable —
    /// the caller then applies the provider's built-in default.
    pub url: Option<String>,
    /// Which layer won; `None` iff `url` is `None`.
    pub layer: Option<BaseUrlLayer>,
}

/// A candidate is usable when it is non-empty and fully expanded.
///
/// Trailing slashes are left untouched: the provider constructors already
/// normalise `{base_url}/responses` themselves, and rewriting here would make
/// the resolved value differ from what the catalog stores.
fn usable(candidate: &str, layer: BaseUrlLayer) -> bool {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("${") {
        // Not expanded on purpose (ADR-103 D7.2 / Q6) — this layer cannot be
        // used, so the next one applies. Silence here would be the failure
        // mode the ADR forbids.
        warn!(
            layer = layer.as_str(),
            base_url = %trimmed,
            "base URL contains an unexpanded ${{VAR}} placeholder — layer ignored (ADR-103 D7)"
        );
        return false;
    }
    true
}

/// Resolve the effective base URL following ADR-103 D7.
pub fn resolve_base_url(
    explicit: Option<&str>,
    model: Option<&str>,
    provider: Option<&str>,
) -> ResolvedBaseUrl {
    for (candidate, layer) in [
        (explicit, BaseUrlLayer::Explicit),
        (model, BaseUrlLayer::Model),
        (provider, BaseUrlLayer::Provider),
    ] {
        if let Some(candidate) = candidate
            && usable(candidate, layer)
        {
            return ResolvedBaseUrl {
                url: Some(candidate.trim().to_string()),
                layer: Some(layer),
            };
        }
    }
    ResolvedBaseUrl {
        url: None,
        layer: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_wins_over_every_other_layer() {
        let resolved = resolve_base_url(
            Some("https://user.example/v1"),
            Some("https://model.example/v1"),
            Some("https://provider.example"),
        );
        assert_eq!(resolved.url.as_deref(), Some("https://user.example/v1"));
        assert_eq!(resolved.layer, Some(BaseUrlLayer::Explicit));
    }

    #[test]
    fn model_override_beats_the_provider_level() {
        // MiniMax Responses needs a different base than its provider `api`.
        let resolved = resolve_base_url(
            None,
            Some("https://api.minimax.cn/v1"),
            Some("https://api.minimax.cn/anthropic/v1"),
        );
        assert_eq!(resolved.url.as_deref(), Some("https://api.minimax.cn/v1"));
        assert_eq!(resolved.layer, Some(BaseUrlLayer::Model));
    }

    #[test]
    fn provider_level_applies_when_no_override_exists() {
        let resolved = resolve_base_url(None, None, Some("https://api.deepseek.com"));
        assert_eq!(resolved.url.as_deref(), Some("https://api.deepseek.com"));
        assert_eq!(resolved.layer, Some(BaseUrlLayer::Provider));
    }

    #[test]
    fn unexpanded_placeholder_disqualifies_the_layer_and_falls_through() {
        let resolved = resolve_base_url(
            Some("${CUSTOM_GATEWAY_BASE_URL}/v1"),
            Some("https://model.example/v1"),
            Some("https://provider.example"),
        );
        assert_eq!(
            resolved.url.as_deref(),
            Some("https://model.example/v1"),
            "an unexpanded placeholder must not win over the next layer"
        );
        assert_eq!(resolved.layer, Some(BaseUrlLayer::Model));
    }

    #[test]
    fn all_placeholder_layers_leave_the_default_to_the_caller() {
        let resolved = resolve_base_url(
            Some("${A}/v1"),
            Some("${AWS_REGION}.bedrock.example"),
            Some("${B}"),
        );
        assert!(resolved.url.is_none());
        assert!(resolved.layer.is_none());
    }

    #[test]
    fn empty_candidates_are_skipped_not_used() {
        let resolved = resolve_base_url(Some(""), Some("   "), Some("https://provider.example"));
        assert_eq!(resolved.url.as_deref(), Some("https://provider.example"));
        assert_eq!(resolved.layer, Some(BaseUrlLayer::Provider));
    }

    #[test]
    fn nothing_configured_means_no_url() {
        let resolved = resolve_base_url(None, None, None);
        assert!(resolved.url.is_none());
    }
}