//! ADR-103 D8/S7 — the Responses trim profile and its evidence cache.
//!
//! `POST /responses` has exactly one field v1 is allowed to drop when a
//! provider rejects it: `reasoning` (MiniMax M3.1-Flash answers 400 for
//! `reasoning.effort: none`). Everything else — `tool_choice`, `text.format`,
//! `temperature`, `top_p`, `tools` — changes model behaviour when silently
//! dropped, so those surface to the user instead (D8 "v1 明确不裁").
//!
//! Two rules shape this module:
//!
//! 1. **Never reuse [`StripProfile`](super::compat::StripProfile).** That
//!    struct is completions-shaped and was the subject of the 2026-09-05
//!    incident; the Responses profile is a separate type so a completions
//!    learning can never leak into the new wire shape (or the reverse).
//! 2. **The evidence policy is not re-implemented here.** Two lucky 400s must
//!    not harden into a durable trim any more than they may harden into
//!    `strip_tools`, so this module only supplies the profile; the
//!    confirmation gate / TTL lease / plain-success reset live in
//!    [`super::compat::CompatCache`], shared with the completions cache.
//!
//! Storage is a separate file (`provider_compat_responses.json`): keys are
//! already shape-qualified, and keeping the payloads apart means rolling
//! ADR-103 back cannot make `provider_compat.json` unreadable.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::compat::{CompatCache, SHAPE_RESPONSES, TrimAction, cache_key};

/// File name (inside `{work_dir}/config/`) for the Responses trim cache.
pub const RESPONSES_COMPAT_FILE: &str = "provider_compat_responses.json";

/// What the Runtime omits from a Responses request body for one
/// `(provider, model)` pair.
///
/// A single field today, deliberately: `omit_reasoning` mirrors exactly one
/// entry of [`acowork_core::providers::responses_errors::TRIMMABLE_FIELDS`],
/// and widening it requires an ADR change (every extra entry is a silent
/// behaviour change for the user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ResponsesTrim {
    /// Drop the `reasoning: { effort }` block entirely.
    pub omit_reasoning: bool,
    /// Unix timestamp of the last successful use (diagnostics only; the TTL is
    /// anchored on the promotion time inside the cache).
    pub last_success_unix_ts: u64,
}

impl ResponsesTrim {
    /// The one trim v1 knows how to apply.
    pub fn omit_reasoning() -> Self {
        Self {
            omit_reasoning: true,
            last_success_unix_ts: 0,
        }
    }

    /// No trim — used to compare against "did we send it as-is?".
    pub fn none() -> Self {
        Self::default()
    }

    /// Fields this profile drops, named as the provider names them (so the
    /// Gateway's `stripped_fields` / the debug panel can echo them verbatim).
    pub fn stripped_fields(&self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        if self.omit_reasoning {
            fields.push("reasoning");
        }
        fields
    }
}

impl TrimAction for ResponsesTrim {
    fn same_action(&self, other: &Self) -> bool {
        self.omit_reasoning == other.omit_reasoning
    }

    fn describe(&self) -> String {
        if self.omit_reasoning {
            "omit[reasoning]".to_string()
        } else {
            "omit[]".to_string()
        }
    }

    fn touch_last_success(&mut self, now_unix_ts: u64) {
        self.last_success_unix_ts = now_unix_ts;
    }
}

/// The Responses-shaped compat cache (`CompatCache` specialised to
/// [`ResponsesTrim`]).
pub type ResponsesCompatCache = CompatCache<ResponsesTrim>;

/// Load `{work_dir}/config/provider_compat_responses.json`.
pub fn load(work_dir: &Path) -> Arc<ResponsesCompatCache> {
    CompatCache::<ResponsesTrim>::open(work_dir.join("config").join(RESPONSES_COMPAT_FILE))
}

/// Cache key for one `(provider, model)` pair on the `/responses` shape.
///
/// Shape-qualified through the shared [`cache_key`] so the same vendor+model
/// reached over `/chat/completions` keeps its own entry (ADR-103 D7.3).
pub fn trim_key(provider_id: &str, model: &str) -> String {
    cache_key(provider_id, SHAPE_RESPONSES, model)
}

/// Path helper kept public for callers that only have a work dir string.
pub fn cache_path(work_dir: &str) -> PathBuf {
    Path::new(work_dir)
        .join("config")
        .join(RESPONSES_COMPAT_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::compat::ErrorClass;

    fn tmp_work_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "acowork_resp_compat_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }

    #[test]
    fn key_is_shape_qualified() {
        // A completions entry for the same pair must never be read here.
        assert_eq!(trim_key("minimax", "m3.1-flash"), "minimax::responses::m3.1-flash");
        assert_ne!(
            trim_key("minimax", "m3.1-flash"),
            crate::providers::compat::cache_key(
                "minimax",
                crate::providers::compat::SHAPE_COMPLETIONS,
                "m3.1-flash"
            )
        );
    }

    #[test]
    fn stripped_fields_name_the_wire_field() {
        assert_eq!(ResponsesTrim::omit_reasoning().stripped_fields(), vec!["reasoning"]);
        assert!(ResponsesTrim::none().stripped_fields().is_empty());
    }

    #[test]
    fn file_lives_under_config_and_is_not_the_completions_file() {
        let path = cache_path("/tmp/work");
        assert!(path.ends_with(RESPONSES_COMPAT_FILE));
        assert_ne!(
            path.file_name().unwrap(),
            crate::providers::compat::COMPLETIONS_COMPAT_FILE
        );
    }

    #[tokio::test]
    async fn one_lucky_trim_is_not_durable() {
        // The 2026-09-05 rule, applied to the new shape: a single success with
        // the field dropped must not become policy.
        let work = tmp_work_dir("single");
        let cache = load(&work);
        let key = trim_key("minimax", "m3.1-flash");

        cache.record_fallback_success(key.as_str(), ResponsesTrim::omit_reasoning(), ErrorClass::RequestSchema);
        assert!(
            cache.get(&key).is_none(),
            "a single trim success must stay a candidate"
        );
    }

    #[tokio::test]
    async fn repeated_trim_success_becomes_durable_and_survives_reload() {
        let work = tmp_work_dir("confirm");
        let cache = load(&work);
        let key = trim_key("minimax", "m3.1-flash");
        for _ in 0..crate::providers::compat::COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(
                key.as_str(),
                ResponsesTrim::omit_reasoning(),
                ErrorClass::RequestSchema,
            );
        }
        assert_eq!(cache.get(&key), Some(ResponsesTrim::omit_reasoning()));
        // The promotion write is spawned, so give it a moment to flush.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;

        // Durable means "survives a restart" — otherwise every Runtime rebuild
        // (or agent respawn) forgets and re-learns the same 400.
        let reloaded = load(&work);
        assert_eq!(
            reloaded.get(&key),
            Some(ResponsesTrim::omit_reasoning()),
            "promotion must be persisted to {}",
            RESPONSES_COMPAT_FILE
        );
    }

    #[tokio::test]
    async fn plain_success_resets_pending_evidence() {
        // An unmodified request that works is contradicting evidence: the
        // provider accepted `reasoning`, so the accident window is over.
        let work = tmp_work_dir("reset");
        let cache = load(&work);
        let key = trim_key("minimax", "m3.1-flash");
        cache.record_fallback_success(key.as_str(), ResponsesTrim::omit_reasoning(), ErrorClass::RequestSchema);
        cache.record_plain_success(&key);
        for _ in 0..crate::providers::compat::COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(
                key.as_str(),
                ResponsesTrim::omit_reasoning(),
                ErrorClass::RequestSchema,
            );
        }
        // Confirmations restart from zero after the reset, so a durable entry
        // only appears once the full quorum is re-observed.
        assert!(cache.get(&key).is_some(), "quorum re-reached after reset");
    }

    #[tokio::test]
    async fn content_integrity_never_becomes_a_trim() {
        // `reasoning_content must be passed back` (DeepSeek) is a *content*
        // error, not a schema one — masking it by dropping `reasoning` would
        // hide a real defect (ADR-103 D8, 2026-09-05 lesson).
        let work = tmp_work_dir("content");
        let cache = load(&work);
        let key = trim_key("deepseek", "deepseek-reasoner");
        for _ in 0..crate::providers::compat::COMPAT_CONFIRM_REQUIRED {
            cache.record_fallback_success(
                key.as_str(),
                ResponsesTrim::omit_reasoning(),
                ErrorClass::ContentIntegrity,
            );
        }
        assert!(cache.get(&key).is_none(), "non-learnable class must not harden");
    }
}