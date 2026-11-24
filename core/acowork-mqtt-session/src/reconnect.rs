//! Reconnect policy (ADR-039 §8.2).
//!
//! Exponential backoff with jitter. Replaces the old "sleep 1s on
//! every error" approach with a structured policy keyed by
//! [`ErrClass`].

use std::time::Duration;

use crate::ErrClass;

/// Configuration for the reconnect backoff.
#[derive(Debug, Clone)]
pub struct ReconnectPolicy {
    /// Initial backoff for retryable errors (E1, E5).
    pub initial: Duration,
    /// Maximum backoff cap.
    pub max: Duration,
    /// Multiplier applied after each consecutive failure.
    pub multiplier: f64,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        }
    }
}

impl ReconnectPolicy {
    /// Compute the sleep duration for the `n`-th consecutive failure
    /// (0-indexed) of a retryable error class.
    ///
    /// Uses exponential growth: `initial * multiplier^n`, capped at
    /// `max`. A small deterministic jitter (±20%) is applied to
    /// avoid thundering-herd reconnects when multiple agents lose
    /// connectivity simultaneously.
    ///
    /// For fatal errors (E2/E3/E4/E6) this returns `None`, signalling
    /// the caller should not retry.
    pub fn backoff(&self, err_class: ErrClass, consecutive_failures: u32) -> Option<Backoff> {
        if err_class.is_fatal() {
            return None;
        }
        Some(Backoff {
            duration: self.delay(consecutive_failures),
            attempt: consecutive_failures,
            err_class,
        })
    }

    /// The retry delay for the `n`-th consecutive failure, independent
    /// of any error classification.
    ///
    /// Split out of [`Self::backoff`] so resources that are not MQTT
    /// clients (the embedded broker's bind supervisor) share the exact
    /// same curve instead of re-deriving it — and without having to
    /// invent an [`ErrClass`] that does not describe them.
    pub fn delay(&self, consecutive_failures: u32) -> Duration {
        // Exponential growth: initial * multiplier^n
        let exp = consecutive_failures.min(20) as f64; // guard overflow
        let raw_ms = self.initial.as_millis() as f64;
        let growth = self.multiplier.powf(exp);
        let mut delay_ms = (raw_ms * growth).min(self.max.as_millis() as f64);

        // Deterministic jitter: ±20%, derived from failure count to
        // avoid RNG dependency. This is intentionally NOT
        // cryptographically random – it only needs to spread
        // reconnects apart.
        let jitter_seed = (consecutive_failures.wrapping_mul(2654435761)) as f64;
        let jitter_frac = ((jitter_seed % 1000.0) / 1000.0) * 0.4 - 0.2; // -0.2..+0.2
        delay_ms *= 1.0 + jitter_frac;
        delay_ms = delay_ms.max(100.0); // floor at 100ms

        Duration::from_millis(delay_ms as u64)
    }
}

/// A computed backoff decision.
#[derive(Debug, Clone)]
pub struct Backoff {
    /// How long to sleep before the next reconnect attempt.
    pub duration: Duration,
    /// 0-indexed consecutive failure count.
    pub attempt: u32,
    /// The error class that triggered this backoff.
    pub err_class: ErrClass,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fatal_errors_return_none() {
        let policy = ReconnectPolicy::default();
        assert!(policy.backoff(ErrClass::AuthRefused, 0).is_none());
        assert!(policy.backoff(ErrClass::ProtocolRefused, 0).is_none());
        assert!(policy.backoff(ErrClass::ConfigError, 0).is_none());
        assert!(policy.backoff(ErrClass::InternalBug, 0).is_none());
    }

    #[test]
    fn retryable_errors_return_some() {
        let policy = ReconnectPolicy::default();
        assert!(policy.backoff(ErrClass::Transient, 0).is_some());
        assert!(policy.backoff(ErrClass::BrokerUnavailable, 0).is_some());
    }

    #[test]
    fn backoff_grows_exponentially() {
        let policy = ReconnectPolicy::default();
        let b0 = policy.backoff(ErrClass::Transient, 0).unwrap().duration;
        let b1 = policy.backoff(ErrClass::Transient, 1).unwrap().duration;
        let b2 = policy.backoff(ErrClass::Transient, 2).unwrap().duration;
        // Without jitter the growth is 500ms, 1000ms, 2000ms.
        // With ±20% jitter the trend should still be increasing.
        // Check that b1 > b0 AND b2 > b1 in the vast majority of cases.
        // (Jitter could cause edge cases but the gap is 4x end-to-end and 2x
        // per step so the monotonic trend holds.)
        assert!(
            b1 > b0,
            "backoff should grow between attempts 0 and 1: b0={b0:?} b1={b1:?}"
        );
        assert!(
            b2 > b1,
            "backoff should grow between attempts 1 and 2: b1={b1:?} b2={b2:?}"
        );
    }

    #[test]
    fn backoff_caps_at_max() {
        let policy = ReconnectPolicy {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
            multiplier: 2.0,
        };
        // After 10+ failures the raw delay would be 500ms * 2^10 = 512s,
        // well above the 30s cap. With jitter it should still be <= 36s.
        let b = policy.backoff(ErrClass::Transient, 15).unwrap();
        assert!(
            b.duration <= Duration::from_secs(36),
            "backoff should be capped: {:?}",
            b.duration
        );
    }

    #[test]
    fn backoff_floor_is_100ms() {
        let policy = ReconnectPolicy::default();
        let b = policy.backoff(ErrClass::Transient, 0).unwrap();
        assert!(
            b.duration >= Duration::from_millis(100),
            "backoff should be >= 100ms: {:?}",
            b.duration
        );
    }

    #[test]
    fn backoff_records_attempt_and_class() {
        let policy = ReconnectPolicy::default();
        let b = policy.backoff(ErrClass::BrokerUnavailable, 3).unwrap();
        assert_eq!(b.attempt, 3);
        assert_eq!(b.err_class, ErrClass::BrokerUnavailable);
    }
}
