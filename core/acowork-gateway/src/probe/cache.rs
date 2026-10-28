//! ADR-103 D10.5 — in-memory probe verdict cache.
//!
//! Probe verdicts are cached for 24 h per `(provider, model, protocol)` so that
//! a save-only edit (rename, capability tweak) does not re-bill a real API
//! call, while an explicit re-probe can always bypass the cache with
//! `?force=true`.
//!
//! The cache holds *transport outcomes*, not persisted verdicts: what lands in
//! `provider_list.json` is the [`ProtocolProbe`] derived from them. Keeping
//! the two separate means the persisted file stays durable/portable (RFC 3339
//! timestamp) while the cache can use a monotonic [`Instant`] and cannot go
//! wrong after a clock jump.
//!
//! [`ProtocolProbe`]: acowork_core::protocol::ProtocolProbe

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::probe::runner::ProbeOutcome;

/// D10.5: verdict lifetime.
pub const PROBE_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// One cached verdict plus the monotonic time it was produced.
#[derive(Debug, Clone)]
struct CachedProbe {
    outcome: ProbeOutcome,
    stored_at: Instant,
}

/// Process-wide probe verdict cache (owned by `GatewayState`).
#[derive(Debug, Default)]
pub struct ProbeCache {
    entries: RwLock<HashMap<String, CachedProbe>>,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cache key for one model's verdict (D7.3-style qualified key: the same
    /// model id under two providers is two different verdicts).
    pub fn key(provider: &str, model: &str, protocol: &str) -> String {
        format!("{provider}::{model}::{protocol}")
    }

    /// Look up a still-fresh verdict.
    ///
    /// `force` short-circuits the cache entirely (the endpoint's
    /// `?force=true`). A stale entry is dropped on the way out so the map does
    /// not accumulate dead rows for providers nobody probes again.
    pub fn get(&self, key: &str, force: bool) -> Option<ProbeOutcome> {
        if force {
            return None;
        }
        let mut guard = self.entries.write().ok()?;
        match guard.get(key) {
            Some(entry) if entry.stored_at.elapsed() < PROBE_CACHE_TTL => Some(entry.outcome.clone()),
            Some(_) => {
                guard.remove(key);
                None
            }
            None => None,
        }
    }

    /// Store a verdict.
    pub fn insert(&self, key: &str, outcome: ProbeOutcome) {
        self.insert_at(key, outcome, Instant::now());
    }

    /// Store a verdict with an explicit timestamp — the seam TTL tests need
    /// (no fake clock, no `sleep`).
    pub fn insert_at(&self, key: &str, outcome: ProbeOutcome, stored_at: Instant) {
        if let Ok(mut guard) = self.entries.write() {
            guard.insert(
                key.to_string(),
                CachedProbe {
                    outcome,
                    stored_at,
                },
            );
        }
    }

    /// Drop every cached verdict of one provider (D12.4: config or key change
    /// makes the verdict stale, so a later probe must actually re-probe).
    pub fn invalidate_provider(&self, provider: &str) -> usize {
        let prefix = format!("{provider}::");
        let Ok(mut guard) = self.entries.write() else {
            return 0;
        };
        let before = guard.len();
        guard.retain(|key, _| !key.starts_with(&prefix));
        before - guard.len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.read().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::runner::ProbeFailure;
    use crate::probe::wire::ProbeUsage;

    fn outcome(ok: bool) -> ProbeOutcome {
        ProbeOutcome {
            success: ok,
            tier_passed: if ok { vec!["L1".into(), "L2".into()] } else { vec![] },
            latency_ms: 12,
            observed_base_url: None,
            usage: ok.then_some(ProbeUsage {
                input_tokens: 4,
                output_tokens: 1,
            }),
            error: if ok {
                None
            } else {
                Some(ProbeFailure {
                    class: "EndpointUnavailable".into(),
                    message: "404".into(),
                    http_status: Some(404),
                })
            },
        }
    }

    #[test]
    fn key_is_provider_and_protocol_qualified() {
        assert_eq!(
            ProbeCache::key("openai", "gpt-5", "responses"),
            "openai::gpt-5::responses"
        );
        assert_ne!(
            ProbeCache::key("a", "m", "responses"),
            ProbeCache::key("b", "m", "responses")
        );
    }

    #[test]
    fn fresh_verdict_is_returned_and_stale_is_dropped() {
        let cache = ProbeCache::new();
        let key = ProbeCache::key("openai", "gpt-5", "responses");
        assert!(cache.get(&key, false).is_none());

        cache.insert(&key, outcome(true));
        assert_eq!(cache.get(&key, false), Some(outcome(true)));

        // Age the entry past the TTL: it must miss AND be evicted.
        let stale_key = ProbeCache::key("openai", "gpt-5-mini", "responses");
        cache.insert_at(&stale_key, outcome(true), Instant::now() - PROBE_CACHE_TTL);
        assert!(cache.get(&stale_key, false).is_none());
        assert_eq!(cache.len(), 1, "stale row must be evicted");
    }

    #[test]
    fn force_bypasses_the_cache_without_dropping_it() {
        let cache = ProbeCache::new();
        let key = ProbeCache::key("openai", "gpt-5", "responses");
        cache.insert(&key, outcome(false));
        assert!(cache.get(&key, true).is_none());
        // The verdict is still there for the next non-forced read.
        assert_eq!(cache.get(&key, false), Some(outcome(false)));
    }

    #[test]
    fn invalidate_provider_only_drops_that_provider() {
        let cache = ProbeCache::new();
        cache.insert(
            &ProbeCache::key("openai", "gpt-5", "responses"),
            outcome(true),
        );
        cache.insert(
            &ProbeCache::key("openai", "gpt-5-mini", "responses"),
            outcome(true),
        );
        cache.insert(
            &ProbeCache::key("openrouter", "gpt-5", "responses"),
            outcome(true),
        );

        assert_eq!(cache.invalidate_provider("openai"), 2);
        assert_eq!(cache.len(), 1);
        assert!(
            cache
                .get(&ProbeCache::key("openrouter", "gpt-5", "responses"), false)
                .is_some()
        );
    }
}