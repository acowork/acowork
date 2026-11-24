//! The single answer to "what can this model do?" — resolved per provider.
//!
//! ## Why this module exists
//!
//! `ProviderListItem` is a list of *providers*, each carrying its own model
//! entries, so one model id can appear under several providers with different
//! capabilities: a user-configured custom provider, an aggregator that proxies
//! an id another provider also serves, the same vendor reachable through two
//! base URLs. Before this catalog every consumer walked the list itself and
//! matched the model id alone, which returned whichever provider happened to
//! come first — so a hand-configured `image` capability on one provider was
//! silently shadowed by a text-only row of the same id elsewhere (2026-11-23:
//! `image_read` refused a vision-capable custom provider because a same-named
//! text-only row in another provider was indexed earlier).
//!
//! Two rules, and the whole point is that they are decided HERE and nowhere
//! else, so the request-side filter, the tool gate, the context budget and the
//! UI push cannot disagree:
//!
//! 1. **A lookup that knows its provider gets that provider's entry.** This is
//!    the only way user-configured capabilities on a custom provider can win:
//!    the Gateway gives custom providers a text-only default and merges the
//!    user's overrides on top (`merge_user_capabilities`), so the entry in the
//!    list already *is* the user's truth. The catalog indexes it as-is and
//!    never second-guesses it with catalog defaults.
//! 2. **A lookup without a provider uses the one provider that serves the id.**
//!    When several do, it takes the first in `provider_list` order —
//!    deliberately the order the user sees in Provider Settings — and
//!    [`ModelCatalog::ambiguous_models`] reports the collision so the owner can
//!    log it once per provider-list update instead of once per turn.
//!
//! Unknown ids answer `None` from [`ModelCatalog::caps`] — the caller decides
//! whether "no capability metadata" means "go ahead" (image/PDF parts, see
//! `attachment_to_image::model_accepts_image`) or "no limit known" (the output
//! token cap falls back to [`DEFAULT_MAX_OUTPUT_TOKENS_LIMIT`]).

use std::collections::HashMap;

use crate::protocol::{ModelCapabilitiesInfo, ProviderListItem};

/// Output-token cap used when the model (or its provider) is unknown.
///
/// Matches the pre-catalog default of the old per-id walk in `AgentCore`,
/// which returned this same value for an unregistered model.
pub const DEFAULT_MAX_OUTPUT_TOKENS_LIMIT: u64 = 16_384;

/// Provider-aware, append-only index over a `provider_list` snapshot.
///
/// Build it once per provider-list update (`SessionManager::
/// update_global_provider_list`) and share it read-only — it never mutates
/// after [`ModelCatalog::build`], which is what lets every session hold the
/// same `Arc` and never see a half-built index.
#[derive(Debug, Default)]
pub struct ModelCatalog {
    entries: Vec<Entry>,
    /// `provider_id -> model_id -> entry index`. Two-level map on purpose:
    /// the hot path (`caps(model, Some(provider))`) allocates nothing, while a
    /// `(String, String)` key would allocate two strings per lookup.
    by_provider: HashMap<String, HashMap<String, usize>>,
    /// `model_id -> entry indices`, in `provider_list` order.
    by_model: HashMap<String, Vec<usize>>,
}

#[derive(Debug)]
struct Entry {
    provider_id: String,
    model_id: String,
    capabilities: ModelCapabilitiesInfo,
    max_output_tokens_limit: u64,
}

impl ModelCatalog {
    /// Index every `(provider, model)` pair in `providers`, preserving list
    /// order for the no-provider tie-break.
    ///
    /// A repeated `(provider, model)` pair keeps the first entry: providers
    /// are supposed to be unique by id, so a duplicate is a configuration bug,
    /// and taking the first keeps the result stable across rebuilds.
    pub fn build(providers: &[ProviderListItem]) -> Self {
        let mut catalog = Self::default();
        for provider in providers {
            for model in &provider.models {
                let provider_models = catalog.by_provider.entry(provider.id.clone()).or_default();
                if provider_models.contains_key(&model.id) {
                    continue;
                }
                let index = catalog.entries.len();
                catalog.entries.push(Entry {
                    provider_id: provider.id.clone(),
                    model_id: model.id.clone(),
                    capabilities: model.capabilities.clone(),
                    max_output_tokens_limit: model.max_output_tokens_limit,
                });
                provider_models.insert(model.id.clone(), index);
                catalog
                    .by_model
                    .entry(model.id.clone())
                    .or_default()
                    .push(index);
            }
        }
        catalog
    }

    /// Capabilities for `model`, from `provider` when it serves that id.
    ///
    /// `provider` is the session's provider (or the explicit provider a
    /// candidate was resolved from). When it does not serve the id — a model
    /// switch mid-flight, a fallback that moved the turn to another provider —
    /// the lookup falls back to the no-provider rule rather than failing: the
    /// pixels a text-only model answers are worse than a missing capability.
    pub fn caps(&self, model: &str, provider: Option<&str>) -> Option<&ModelCapabilitiesInfo> {
        self.resolve(model, provider)
            .map(|entry| &entry.capabilities)
    }

    /// Gateway-side output-token cap for `model` under `provider`.
    ///
    /// Same resolution as [`Self::caps`], with
    /// [`DEFAULT_MAX_OUTPUT_TOKENS_LIMIT`] for an unregistered model.
    pub fn max_output_tokens_limit(&self, model: &str, provider: Option<&str>) -> u64 {
        self.resolve(model, provider)
            .map(|entry| entry.max_output_tokens_limit)
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS_LIMIT)
    }

    /// The provider whose entry answered — the *same* resolution [`Self::caps`]
    /// and [`Self::max_output_tokens_limit`] use.
    ///
    /// A caller that needs the provider id (seeding a new session's provider
    /// from a model-only default) must ask here rather than re-derive "first in
    /// list order" from [`Self::serving_providers`]: two implementations of the
    /// tie-break drift apart the moment rule 2 changes, and then the session's
    /// provider and the capabilities resolved for it name different rows.
    ///
    /// `None` only when no provider serves the id. Note this is NOT a membership
    /// test for `provider`: an explicit provider that does not serve the id
    /// falls back to rule 2, exactly as [`Self::caps`] does.
    pub fn resolved_provider(&self, model: &str, provider: Option<&str>) -> Option<&str> {
        self.resolve(model, provider)
            .map(|entry| entry.provider_id.as_str())
    }

    /// Every provider that serves `model`, in `provider_list` order.
    ///
    /// The set-shaped question (`/model` routing, fallback candidates) as
    /// opposed to the single-value question [`Self::caps`] answers.
    pub fn serving_providers(&self, model: &str) -> Vec<&str> {
        self.by_model
            .get(model)
            .map(|indices| {
                indices
                    .iter()
                    .map(|index| self.entries[*index].provider_id.as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Model ids served by more than one provider, with their providers.
    ///
    /// A lookup without a provider resolves such an id to the first provider in
    /// list order — correct by rule 2, but a configuration the operator should
    /// see, because the *other* rows' capabilities are unreachable without a
    /// provider. Reported instead of logged here: this crate carries no logging
    /// dependency and no runtime opinion about log levels.
    pub fn ambiguous_models(&self) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = self
            .by_model
            .iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(model, indices)| {
                (
                    model.clone(),
                    indices
                        .iter()
                        .map(|index| self.entries[*index].provider_id.clone())
                        .collect(),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// True when the index holds no `(provider, model)` pair — a Runtime that
    /// has not received a provider list yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every indexed model id, for the "not found" diagnostic.
    ///
    /// Duplicates are expected and meaningful: they are the ambiguous ids.
    pub fn model_ids(&self) -> Vec<&str> {
        self.entries
            .iter()
            .map(|entry| entry.model_id.as_str())
            .collect()
    }

    fn resolve(&self, model: &str, provider: Option<&str>) -> Option<&Entry> {
        if let Some(provider) = provider
            && let Some(index) = self
                .by_provider
                .get(provider)
                .and_then(|models| models.get(model))
        {
            return Some(&self.entries[*index]);
        }
        // No provider given, or the given provider does not serve this id.
        // Rule 2: the first provider in list order wins.
        self.by_model
            .get(model)
            .and_then(|indices| indices.first())
            .map(|index| &self.entries[*index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ModelModalities, ProtocolType, ProviderModelEntry};

    fn caps_with(input: &[&str], context_window: u64) -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window,
            max_output_tokens: 8_192,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: Some(ModelModalities {
                input: input.iter().map(|s| s.to_string()).collect(),
                output: vec!["text".to_string()],
            }),
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
            reasoning_efforts: None,
        }
    }

    fn model(id: &str, caps: ModelCapabilitiesInfo, limit: u64) -> ProviderModelEntry {
        ProviderModelEntry {
            id: id.to_string(),
            capabilities: caps,
            max_output_tokens_limit: limit,
            protocol_type: None,
            base_url: None,
            protocol_probe: None,
        }
    }

    fn provider(id: &str, models: Vec<ProviderModelEntry>, custom: bool) -> ProviderListItem {
        ProviderListItem {
            id: id.to_string(),
            base_url: "https://example.invalid/v1".to_string(),
            protocol_type: ProtocolType::OpenAI,
            models,
            compact_model: None,
            custom,
        }
    }

    /// The 2026-11-23 regression, in test form: the same model id under a
    /// text-only provider (indexed first) and a custom provider whose user
    /// configured image input. A provider-aware lookup must reach the custom
    /// row's pixels; the old id-only walk always answered with the first row.
    #[test]
    fn provider_lookup_reaches_the_custom_providers_capabilities() {
        let list = vec![
            provider(
                "custom-relay",
                vec![model(
                    "deepseek-v4.1-flash-1017",
                    caps_with(&["text"], 1_000_000),
                    4_096,
                )],
                true,
            ),
            provider(
                "custom-direct",
                vec![model(
                    "deepseek-v4.1-flash-1017",
                    caps_with(&["text", "image"], 1_000_000),
                    65_536,
                )],
                true,
            ),
        ];
        let catalog = ModelCatalog::build(&list);

        let direct = catalog
            .caps("deepseek-v4.1-flash-1017", Some("custom-direct"))
            .expect("custom-direct serves the model");
        assert!(
            direct
                .modalities
                .as_ref()
                .is_some_and(|m| m.input.iter().any(|s| s == "image")),
            "the user-configured image capability must survive indexing"
        );

        let relay = catalog
            .caps("deepseek-v4.1-flash-1017", Some("custom-relay"))
            .expect("custom-relay serves the model");
        assert_eq!(
            relay.modalities.as_ref().map(|m| m.input.clone()),
            Some(vec!["text".to_string()]),
            "each provider answers with its own row"
        );

        assert_eq!(
            catalog.max_output_tokens_limit("deepseek-v4.1-flash-1017", Some("custom-direct")),
            65_536
        );
        assert_eq!(
            catalog.max_output_tokens_limit("deepseek-v4.1-flash-1017", Some("custom-relay")),
            4_096
        );
    }

    /// Without a provider, rule 2 applies: the first provider in list order,
    /// and the collision is reported so the owner can warn once.
    #[test]
    fn providerless_lookup_takes_list_order_and_reports_the_collision() {
        let list = vec![
            provider(
                "alpha",
                vec![model("shared", caps_with(&["text"], 100), 1)],
                false,
            ),
            provider(
                "beta",
                vec![model("shared", caps_with(&["text", "image"], 200), 2)],
                true,
            ),
            provider(
                "gamma",
                vec![model("solo", caps_with(&["text"], 300), 3)],
                false,
            ),
        ];
        let catalog = ModelCatalog::build(&list);

        assert_eq!(
            catalog
                .caps("shared", None)
                .map(|c| c.context_window)
                .expect("shared is indexed"),
            100,
            "no provider means the first row in list order"
        );
        assert_eq!(catalog.max_output_tokens_limit("shared", None), 1);
        assert_eq!(
            catalog.serving_providers("shared"),
            vec!["alpha", "beta"],
            "the set-shaped question sees every provider"
        );
        assert_eq!(catalog.serving_providers("solo"), vec!["gamma"]);
        assert!(catalog.serving_providers("absent").is_empty());
        assert_eq!(
            catalog.ambiguous_models(),
            vec![(
                "shared".to_string(),
                vec!["alpha".to_string(), "beta".to_string()]
            )]
        );
    }

    /// A provider that does not serve the id falls back to rule 2 instead of
    /// answering `None`: a mid-flight model switch or a fallback to another
    /// provider must still find the capabilities.
    #[test]
    fn unknown_provider_falls_back_to_the_providerless_rule() {
        let list = vec![provider(
            "only",
            vec![model("m", caps_with(&["text", "image"], 42), 7)],
            false,
        )];
        let catalog = ModelCatalog::build(&list);

        assert!(
            catalog
                .caps("m", Some("a-provider-that-does-not-serve-it"))
                .is_some(),
            "an unknown provider must not hide a model that exists"
        );
        assert!(
            catalog.caps("m", Some("")).is_some(),
            "an empty provider is no provider"
        );
        assert_eq!(catalog.max_output_tokens_limit("m", Some("nope")), 7);
    }

    #[test]
    fn empty_catalog_answers_nothing_and_defaults_the_limit() {
        let catalog = ModelCatalog::build(&[]);
        assert!(catalog.is_empty());
        assert!(catalog.caps("anything", None).is_none());
        assert!(catalog.caps("anything", Some("p")).is_none());
        assert_eq!(
            catalog.max_output_tokens_limit("anything", None),
            DEFAULT_MAX_OUTPUT_TOKENS_LIMIT
        );
        assert!(catalog.model_ids().is_empty());
    }

    /// A duplicate `(provider, model)` pair keeps the first row, so rebuilds
    /// of the same list are stable.
    #[test]
    fn duplicate_pair_keeps_the_first_entry() {
        let list = vec![provider(
            "dup",
            vec![
                model("m", caps_with(&["text"], 111), 1),
                model("m", caps_with(&["text", "image"], 222), 2),
            ],
            true,
        )];
        let catalog = ModelCatalog::build(&list);
        assert_eq!(
            catalog.caps("m", Some("dup")).map(|c| c.context_window),
            Some(111)
        );
        assert_eq!(catalog.model_ids(), vec!["m"]);
    }

    /// `resolved_provider` and `caps` must name the same row. That is the whole
    /// reason a caller asks the index for the provider id instead of re-deriving
    /// "first in list order" from `serving_providers().next()`: the session's
    /// provider and the capabilities resolved for it have to agree.
    #[test]
    fn resolved_provider_and_caps_name_the_same_row() {
        let list = vec![
            provider(
                "first",
                vec![model("shared", caps_with(&["text"], 111), 1)],
                true,
            ),
            provider(
                "second",
                vec![model("shared", caps_with(&["text", "image"], 222), 2)],
                true,
            ),
        ];
        let catalog = ModelCatalog::build(&list);

        // No provider: rule 2 takes the first, and both answers agree.
        assert_eq!(catalog.resolved_provider("shared", None), Some("first"));
        assert_eq!(
            catalog.caps("shared", None).map(|c| c.context_window),
            Some(111)
        );

        // Explicit provider: both follow it to the second row.
        assert_eq!(
            catalog.resolved_provider("shared", Some("second")),
            Some("second")
        );
        assert_eq!(
            catalog
                .caps("shared", Some("second"))
                .map(|c| c.context_window),
            Some(222)
        );

        // A provider that does not serve the id falls back to rule 2 for BOTH
        // answers — `resolved_provider` is deliberately not a membership test.
        assert_eq!(
            catalog.resolved_provider("shared", Some("absent")),
            Some("first")
        );
        assert_eq!(
            catalog
                .caps("shared", Some("absent"))
                .map(|c| c.context_window),
            Some(111)
        );

        // Unknown id: neither answer invents a provider.
        assert_eq!(catalog.resolved_provider("nope", None), None);
    }
}
