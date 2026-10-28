//! ADR-103 D10/D12 — Responses-protocol probing.
//!
//! The probe answers one question per `(provider, model)`: *does this endpoint
//! actually speak the OpenAI Responses API well enough for the Runtime to use
//! it?* Its verdict is the D1 gate input — `Responses` only becomes effective
//! for a model when its last probe is `verified` or `degraded`.
//!
//! Layering (data → pure logic → I/O), so each layer is testable on its own:
//!
//! - [`wire`] — request builders and response assertions (pure).
//! - [`runner`] — HTTP transport + D8 error classification, returns
//!   [`runner::ProbeOutcome`] (no Gateway state).
//! - [`store`] — which rows to probe, and how a verdict lands on
//!   `ProviderModelEntry::protocol_probe` (pure).
//! - [`cache`] — the 24 h verdict cache (`?force=true` bypass).
//! - [`service`] — orchestration against Gateway state: vault key, lock scopes,
//!   persistence, MQTT republish (the only layer that touches `AppState`).
//!
//! Red lines honoured here:
//!
//! - Probe usage MUST NOT enter ADR-027/066 budget accounting (D10.4) — see
//!   [`runner::log_probe_audit`].
//! - A probe result MUST NOT be written into the user's `base_url`; it may only
//!   be *reported* as `observed_base_url` (D10.3).
//! - A failed probe MUST NOT block a save (D12.5) — [`service`] runs after the
//!   save has been persisted and turns every failure into a verdict.

pub mod cache;
pub mod runner;
pub mod service;
pub mod store;
pub mod wire;

pub use cache::{PROBE_CACHE_TTL, ProbeCache};
pub use runner::{PROBE_REQUEST_TIMEOUT, ProbeFailure, ProbeOutcome, ProbeRunner, ProbeTier};
pub use service::{PROBE_BATCH_BUDGET, ProbeReport, is_probeable, probe_one, run_save_time_probes};
pub use store::{ProbePlanItem, apply_probe_outcome, mark_probes_stale, plan_probes};
/// Fixtures shared by the probe module's unit tests.
///
/// `ModelCapabilitiesInfo` has no `Default` (capabilities are resolved from
/// models.dev, never invented), so tests that only need "a model row" fill in
/// the two mandatory limits explicitly.
#[cfg(test)]
pub(crate) mod test_support {
    use acowork_core::protocol::{
        ModelCapabilitiesInfo, ProviderListItem, ProviderModelEntry,
        ProtocolType,
    };

    pub fn capabilities() -> ModelCapabilitiesInfo {
        ModelCapabilitiesInfo {
            context_window: 128_000,
            max_output_tokens: 4_096,
            max_input_tokens: None,
            supports_tool_calling: true,
            supports_reasoning: None,
            supports_attachment: None,
            supports_temperature: None,
            cost: None,
            modalities: None,
            name: None,
            family: None,
            knowledge_cutoff: None,
            default_reasoning_effort: None,
            thinking_mode: None,
        }
    }

    /// A model row with an optional protocol intent.
    pub fn model(id: &str, protocol: Option<ProtocolType>) -> ProviderModelEntry {
        ProviderModelEntry {
            id: id.to_string(),
            capabilities: capabilities(),
            max_output_tokens_limit: 4096,
            protocol_type: protocol,
            base_url: None,
            protocol_probe: None,
        }
    }

    /// A provider row carrying `models`.
    pub fn provider(base_url: &str, models: Vec<ProviderModelEntry>) -> ProviderListItem {
        ProviderListItem {
            id: "acme".to_string(),
            base_url: base_url.to_string(),
            protocol_type: ProtocolType::OpenAI,
            models,
            compact_model: None,
            custom: false,
        }
    }
}
