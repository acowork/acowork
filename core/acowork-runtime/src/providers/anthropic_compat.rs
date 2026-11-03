//! Anthropic-shaped compat cache.
//!
//! The Messages API rejects the *same* request fields the ChatGPT-shaped
//! endpoints do, only with different wording — and newer Claude models reject
//! two of them outright:
//!
//! * `` "`temperature` is deprecated for this model." ``
//! * `"thinking.type.enabled" is not supported for this model. Use
//!   "thinking.type.adaptive" and "output_config.effort"`
//!
//! Configured through a custom provider, a model carries no capability data
//! (`supports_temperature` / `thinking_mode` are unknown), so the 400 body is
//! the only evidence available. This cache remembers what worked, per
//! `(provider, model)`, using the same confirmation-gated policy as the
//! completions cache ([`super::compat`]) — a single lucky success is a
//! candidate, not a fact.
//!
//! A separate payload type and a separate file on purpose: a completions
//! `StripProfile` and an `AnthropicTrim` share no fields, and reading one as
//! the other is exactly the cross-shape leak ADR-103 D7.3 forbids.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::compat::{CompatCache, TrimAction, cache_key};

/// File name (inside `{work_dir}/config/`) for the Anthropic trim cache.
pub const ANTHROPIC_COMPAT_FILE: &str = "provider_compat_anthropic.json";

/// Wire shape of the Anthropic Messages API.
pub const SHAPE_ANTHROPIC: &str = "anthropic";

/// What the Runtime changes in a Messages request for one `(provider, model)`
/// pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AnthropicTrim {
    /// Drop the `temperature` field entirely (newer Claude models deprecated
    /// it; sending it at all is a 400).
    #[serde(default)]
    pub omit_temperature: bool,
    /// Send `thinking: { type: "adaptive" }` + `output_config.effort` instead
    /// of the extended `{ type: "enabled", budget_tokens }` shape.
    #[serde(default)]
    pub adaptive_thinking: bool,
    /// Unix timestamp of the last successful use (diagnostics only; the TTL is
    /// anchored on the promotion time inside the cache).
    #[serde(default)]
    pub last_success_unix_ts: u64,
}

impl AnthropicTrim {
    /// Nothing changed — the request goes out exactly as asked.
    pub fn is_empty(&self) -> bool {
        !self.omit_temperature && !self.adaptive_thinking
    }

    /// The changed fields, named as the API names them (for logs).
    pub fn changed_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.omit_temperature {
            fields.push("temperature");
        }
        if self.adaptive_thinking {
            fields.push("thinking=adaptive");
        }
        fields
    }
}

impl TrimAction for AnthropicTrim {
    fn same_action(&self, other: &Self) -> bool {
        self.omit_temperature == other.omit_temperature
            && self.adaptive_thinking == other.adaptive_thinking
    }

    fn describe(&self) -> String {
        let changed = self.changed_fields();
        if changed.is_empty() {
            "adjust[]".to_string()
        } else {
            format!("adjust[{}]", changed.join(","))
        }
    }

    fn touch_last_success(&mut self, now_unix_ts: u64) {
        self.last_success_unix_ts = now_unix_ts;
    }
}

/// The Anthropic-shaped compat cache.
pub type AnthropicCompatCache = CompatCache<AnthropicTrim>;

/// Load `{work_dir}/config/provider_compat_anthropic.json`.
pub fn load(work_dir: &Path) -> Arc<AnthropicCompatCache> {
    CompatCache::<AnthropicTrim>::open(work_dir.join("config").join(ANTHROPIC_COMPAT_FILE))
}

/// Cache key for one `(provider, model)` pair on the Messages API shape.
pub fn trim_key(provider_id: &str, model: &str) -> String {
    cache_key(provider_id, SHAPE_ANTHROPIC, model)
}

/// Path helper kept public for callers that only have a work dir string.
pub fn cache_path(work_dir: &str) -> PathBuf {
    Path::new(work_dir)
        .join("config")
        .join(ANTHROPIC_COMPAT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_comparison_ignores_bookkeeping() {
        let a = AnthropicTrim {
            omit_temperature: true,
            ..Default::default()
        };
        let b = AnthropicTrim {
            omit_temperature: true,
            last_success_unix_ts: 12345,
            ..Default::default()
        };
        assert!(a.same_action(&b), "timestamps must not split the evidence");

        let c = AnthropicTrim {
            adaptive_thinking: true,
            ..Default::default()
        };
        assert!(!a.same_action(&c), "different adjustments are different actions");
    }

    #[test]
    fn keys_are_shape_qualified() {
        let key = trim_key("custom-loostone", "claude-haiku-5-5");
        assert_eq!(key, "custom-loostone::anthropic::claude-haiku-5-5");
        assert_ne!(
            key,
            super::super::compat::cache_key(
                "custom-loostone",
                super::super::compat::SHAPE_COMPLETIONS,
                "claude-haiku-5-5"
            ),
            "a learned Messages trim must never be read by the completions chain"
        );
    }

    #[test]
    fn empty_trim_reports_nothing_changed() {
        let trim = AnthropicTrim::default();
        assert!(trim.is_empty());
        assert!(trim.changed_fields().is_empty());
        assert_eq!(trim.describe(), "adjust[]");
    }
}
