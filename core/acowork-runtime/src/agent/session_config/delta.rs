//! SessionConfigDelta and SessionConfigSnapshot (ADR-047 §3.2).
//!
//! `SessionConfigDelta` is the **single payload type** for all config
//! mutations coming from any external interface (HTTP, MQTT, CLI).
//! Each field is `None` (unchanged) or `Some(new_value)`.
//!
//! `SessionConfigSnapshot` is a read-only view of the current config,
//! used by HTTP GET, MQTT retained messages, and LLM-side effect
//! application.

use acowork_core::session_meta::ConfigField;
use serde::{Deserialize, Serialize};

/// Partial session config update.
///
/// Adding a new config parameter:
/// 1. Add field here, and to `SessionConfigSnapshot` (the two are one mirror
///    pair — every field of the delta is a field of the snapshot).
/// 2. Add handling in **both** write paths — `ConversationSession::apply_config`
///    (a session this process holds open) and
///    `usecases::session_config_meta::apply_field` (a session that lives only
///    as a meta row). The differential test
///    `session_config_meta::tests::the_two_residency_paths_agree_on_every_field`
///    compares the whole snapshot and fails when only one of them was taught
///    the new field.
/// 3. Add to `SessionConfig` proto + `build_session_config_snapshot()`
/// 4. (Optional) Add LLM-side effect in `llm_effects.rs`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfigDelta {
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Multi-account: selected account for `provider` (selects the API key).
    /// `None` = provider's first account (pre-multi-account behaviour).
    pub account_id: Option<String>,
    pub workspace_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
    /// ADR-074: per-session context window override.
    ///
    /// - `Some(0)` = clear the override (inherit the per-agent chain)
    /// - `Some(n)` where `n` is valid (`FLOOR..=CEILING`) = set
    /// - `None` = unchanged (field absent or `null` in the request body)
    ///
    /// Out-of-range values are rejected with HTTP 400 at
    /// `put_session_config` before reaching `apply_config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub title: Option<String>,
}

impl SessionConfigDelta {
    /// The [`ConfigField`]s this delta actually touches, in
    /// [`ConfigField::ALL`] order.
    ///
    /// The authorization table in `put_session_config` iterates **this**, not
    /// the handler's own field list: a field added to both structs and this
    /// method is gated automatically, and one added only to the struct fails to
    /// compile here — which is the only way a per-field gate stays exhaustive
    /// as the delta grows.
    pub fn present_fields(&self) -> Vec<ConfigField> {
        let mut out = Vec::new();
        for field in ConfigField::ALL {
            let present = match field {
                ConfigField::Model => self.model.is_some(),
                ConfigField::Provider => self.provider.is_some(),
                ConfigField::Account => self.account_id.is_some(),
                ConfigField::ReasoningEffort => self.reasoning_effort.is_some(),
                ConfigField::Temperature => self.temperature.is_some(),
                ConfigField::ContextWindow => self.context_window.is_some(),
                ConfigField::Workspace => self.workspace_id.is_some(),
                ConfigField::Title => self.title.is_some(),
                // Not carried by this delta: they have their own endpoints
                // (share / delete), which enforce the same table.
                ConfigField::ToolPolicy | ConfigField::Visibility | ConfigField::Delete => false,
            };
            if present {
                out.push(*field);
            }
        }
        out
    }
}

/// Read-only snapshot of current session config.
///
/// Used by HTTP GET, MQTT retained, and LLM-side effect application.
///
/// `PartialEq` exists for the differential test that keeps the two write
/// paths (resident conversation / non-resident meta row) honest: comparing
/// whole snapshots means a field added to this struct is compared there
/// automatically, so the "mirror, not a second definition" rule is enforced
/// by the compiler rather than by a comment. (`Eq` is out: `temperature` is
/// an `f32`.)
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SessionConfigSnapshot {
    pub model: Option<String>,
    pub provider: Option<String>,
    /// Multi-account: selected account for `provider`. `None` = first account.
    pub account_id: Option<String>,
    pub workspace_id: Option<String>,
    pub reasoning_effort: Option<String>,
    pub temperature: Option<f32>,
    /// ADR-074: raw per-session context window override. `null` = no
    /// override (session inherits the per-agent chain). The effective
    /// session window is NOT exposed here — its authoritative source is
    /// the `contextUsage.context_window` push (§11.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub title: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field the delta can carry must be classified by the authorization
    /// table, or `put_session_config` would enforce a subset and let the rest
    /// through on the caller's mere `may_turn()`.
    ///
    /// Written as one all-fields delta rather than eight cases: the assertion
    /// that fails when a field is added is "the list changed", which is exactly
    /// the moment someone has to decide its tier.
    #[test]
    fn every_delta_field_is_classified_by_the_authority_table() {
        let delta = SessionConfigDelta {
            model: Some("m".into()),
            provider: Some("p".into()),
            account_id: Some("a".into()),
            workspace_id: Some("w".into()),
            reasoning_effort: Some("low".into()),
            temperature: Some(0.5),
            context_window: Some(200_000),
            title: Some("t".into()),
        };
        assert_eq!(
            delta.present_fields(),
            vec![
                ConfigField::Model,
                ConfigField::Provider,
                ConfigField::Account,
                ConfigField::ReasoningEffort,
                ConfigField::Temperature,
                ConfigField::ContextWindow,
                ConfigField::Workspace,
                ConfigField::Title,
            ]
        );
        // An empty delta touches nothing — "unchanged" is not "allowed".
        assert!(SessionConfigDelta::default().present_fields().is_empty());
    }
}
