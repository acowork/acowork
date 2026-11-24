//! Readiness projection: MQTT session state -> bootstrap subsystem state.
//!
//! ADR-059: `mqtt` is a *required* bootstrap subsystem, so the aggregated
//! phase cannot reach `READY` until it is satisfied. The projection is
//! the only writer of that record.
//!
//! # Why a projection and not a `mark_ready` call
//!
//! The previous wiring called `mqtt_handle.mark_ready()` once, on the
//! branch where the client constructor returned `Ok`. That made
//! readiness a *construction-time* fact: a broker that was not
//! accepting connections yet (the embedded broker thread had only just
//! been spawned) produced a constructor error, the client was dropped,
//! its poll task was aborted, and the record stayed `Booting` for the
//! lifetime of the process. Nothing could ever move it again — the
//! Gateway was permanently `BOOTING` and every consumer waiting on
//! `READY` timed out.
//!
//! Readiness is not a construction fact, it is a *connection* fact, and
//! the connection changes over time. So the record is a pure function
//! of the session state, re-evaluated on every transition. The session
//! state machine (ADR-065, `acowork-mqtt-session`) guarantees a bounded
//! time to the next state change, which is what makes this projection
//! total: it never has to invent a timeout of its own.
//!
//! # Purity
//!
//! [`project`] is a pure, exhaustive, side-effect-free function so the
//! whole readiness contract is testable without a broker, a socket or a
//! clock. [`spawn_projection`] is the only part that touches the
//! registry, and it does nothing but forward [`project`]'s verdict.

#![deny(clippy::wildcard_enum_match_arm)]

use acowork_mqtt_session::{SessionState, SessionStateRx};

use crate::bootstrap::{SubsystemHandle, SubsystemState};

/// The readiness verdict for one session state.
///
/// `detail` is always present: a required subsystem that is not ready
/// must be able to say *why*, because that string is what an operator
/// (and the Desktop boot error) sees while the phase is stuck.
pub type Projection = (SubsystemState, String);

/// Map one session state onto a bootstrap subsystem state.
///
/// Total and pure. Every variant is matched explicitly — adding a
/// session state is a compile error here, not a silently ignored
/// transition (`clippy::wildcard_enum_match_arm` is denied above).
///
/// Transient states map to `Booting`, never to `Failed`: a connection
/// that is being retried is still on its way to `Ready`, and demoting
/// the whole Gateway to `FAILED` for a blip would be a lie. Only
/// `Closed` — the owner stopped the session for good — maps to `Failed`.
///
/// `Skipped` is deliberately absent: a disabled connection is never
/// constructed, so no session exists to project. The configuration path
/// in `gateway/mod.rs` marks the record `Skipped` itself.
pub fn project(state: &SessionState) -> Projection {
    match state {
        SessionState::Connecting => (
            SubsystemState::Booting,
            "mqtt connecting to broker".to_string(),
        ),
        SessionState::Connected => (SubsystemState::Ready, "mqtt connected".to_string()),
        SessionState::Retrying {
            reason,
            attempt,
            retry_in,
        } => (
            SubsystemState::Booting,
            format!(
                "mqtt retrying (attempt {attempt}, next in {}ms): {reason}",
                retry_in.as_millis()
            ),
        ),
        SessionState::Closed { reason } => (
            SubsystemState::Failed,
            format!("mqtt session closed: {reason}"),
        ),
    }
}

/// Drive a subsystem record from a session-state stream.
///
/// Applies the current state immediately (level-triggered: a consumer
/// that subscribes late must not miss a transition that already
/// happened), then forwards every subsequent change.
///
/// The task ends only when the state source is gone. That is a bug
/// path, not a shutdown path — a correct owner publishes `Closed`
/// before dropping the sender — and it is recorded as a failure rather
/// than left as a stale `Booting`.
pub fn spawn_projection(state_rx: SessionStateRx, handle: SubsystemHandle) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (state, detail) = project(&state_rx.current());
        apply(&handle, state, detail);
        loop {
            match state_rx.changed().await {
                Ok(next) => {
                    let (state, detail) = project(&next);
                    apply(&handle, state, detail);
                }
                Err(_) => {
                    handle.mark_failed(Some(
                        "mqtt session state source dropped without a terminal state".to_string(),
                    ));
                    return;
                }
            }
        }
    })
}

/// Total over the registry's vocabulary. `Skipped` is not produced by
/// [`project`] (see its docs) but is a real registry state, so the
/// forwarder stays exhaustive rather than growing a wildcard arm.
fn apply(handle: &SubsystemHandle, state: SubsystemState, detail: String) {
    match state {
        SubsystemState::Ready => handle.mark_ready(Some(detail)),
        SubsystemState::Booting => handle.mark_booting(Some(detail)),
        SubsystemState::Failed => handle.mark_failed(Some(detail)),
        SubsystemState::Skipped => handle.mark_skipped(Some(detail)),
        SubsystemState::Deregistered => {
            // Never produced by `project`; deregistration is an owner
            // action, not a readiness verdict.
            unreachable!("projection never yields Deregistered")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::{ReadinessKind, SubsystemReadinessRegistry};
    use std::time::Duration;

    /// Every session state must have a verdict, and the verdict must
    /// satisfy the readiness contract: only `Connected` satisfies a
    /// required subsystem, and only a non-recoverable state may fail.
    #[test]
    fn projection_is_total_and_only_connected_is_ready() {
        let cases = vec![
            SessionState::Connecting,
            SessionState::Connected,
            SessionState::Retrying {
                reason: "connection refused".into(),
                attempt: 3,
                retry_in: Duration::from_secs(4),
            },
            SessionState::Closed {
                reason: "client dropped".into(),
            },
        ];
        for state in cases {
            let (verdict, detail) = project(&state);
            assert!(!detail.is_empty(), "{state:?} must explain itself");
            match state {
                SessionState::Connected => assert_eq!(verdict, SubsystemState::Ready),
                SessionState::Closed { .. } => assert_eq!(verdict, SubsystemState::Failed),
                SessionState::Connecting | SessionState::Retrying { .. } => {
                    assert_eq!(verdict, SubsystemState::Booting)
                }
            }
        }
    }

    /// A retry must be *visible*: two consecutive retries with different
    /// attempt counters must produce different details, otherwise a
    /// consumer rendering `phase_detail` sees a frozen string for the
    /// whole outage.
    #[test]
    fn retry_progress_changes_the_detail() {
        let a = project(&SessionState::Retrying {
            reason: "connection refused".into(),
            attempt: 1,
            retry_in: Duration::from_secs(1),
        });
        let b = project(&SessionState::Retrying {
            reason: "connection refused".into(),
            attempt: 2,
            retry_in: Duration::from_secs(2),
        });
        assert_eq!(a.0, b.0);
        assert_ne!(a.1, b.1, "retry progress must reach the operator");
    }

    /// The projection must follow the stream, not latch on the first
    /// verdict: Booting -> Ready -> Booting -> Ready.
    #[tokio::test]
    async fn projection_follows_every_transition() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("mqtt", ReadinessKind::Required);
        let (tx, rx) = acowork_mqtt_session::SessionStateTx::new(SessionState::Connecting);
        let task = spawn_projection(rx, handle);

        assert_eq!(registry.state(&"mqtt".into()), Some(SubsystemState::Booting));

        tx.set(SessionState::Connected);
        wait_state(&registry, SubsystemState::Ready, Duration::from_secs(2)).await;

        tx.set(SessionState::Retrying {
            reason: "broker went away".into(),
            attempt: 1,
            retry_in: Duration::from_secs(1),
        });
        wait_state(&registry, SubsystemState::Booting, Duration::from_secs(2)).await;

        tx.set(SessionState::Connected);
        wait_state(&registry, SubsystemState::Ready, Duration::from_secs(2)).await;

        task.abort();
    }

    /// Losing the state source is a failure, never a silent `Booting`.
    #[tokio::test]
    async fn source_gone_is_recorded_as_failed() {
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("mqtt", ReadinessKind::Required);
        let (tx, rx) = acowork_mqtt_session::SessionStateTx::new(SessionState::Connecting);
        let task = spawn_projection(rx, handle);
        drop(tx);
        task.await.expect("projection task must end when the source is gone");
        assert_eq!(registry.state(&"mqtt".into()), Some(SubsystemState::Failed));
    }

    /// Regression for the boot race that stranded the Gateway in
    /// `BOOTING`: the embedded broker thread is spawned moments before
    /// the client, so the first dial can hit a port nobody is listening
    /// on yet.
    ///
    /// The session must survive that, keep retrying, and reach `Ready`
    /// on its own when the broker appears — with **no** reconstruction
    /// by the caller. The old wiring waited 2s, returned `Err`, dropped
    /// the client (aborting its poll task) and left the required record
    /// in `Booting` for the lifetime of the process.
    #[tokio::test]
    async fn readiness_recovers_when_the_broker_appears_late() {
        let port = crate::test_support::free_port();
        let registry = SubsystemReadinessRegistry::new_shared();
        let handle = registry.register("mqtt", ReadinessKind::Required);

        // Nothing is listening on `port` yet.
        let client = crate::mqtt::GatewayMqttClient::spawn(
            "127.0.0.1",
            port,
            "gateway:test-late-broker",
            None,
            None,
        )
        .await
        .expect("constructing a session must not fail because the broker is not up yet");
        let task = spawn_projection(client.state_rx(), handle);
        assert_eq!(registry.state(&"mqtt".into()), Some(SubsystemState::Booting));

        // The outage must be *visible* while it lasts: the retry counter
        // reaches the record, so a stuck boot names its blocker.
        wait_detail_contains(&registry, "attempt", Duration::from_secs(10)).await;

        // The broker shows up. Nobody re-creates the client.
        let _broker = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start on the free port");

        wait_state(&registry, SubsystemState::Ready, Duration::from_secs(30)).await;
        task.abort();
    }

    async fn wait_state(
        registry: &SubsystemReadinessRegistry,
        want: SubsystemState,
        budget: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + budget;
        while registry.state(&"mqtt".into()) != Some(want) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "never reached {want:?} (detail: {:?})",
                detail_of(registry)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_detail_contains(
        registry: &SubsystemReadinessRegistry,
        needle: &str,
        budget: Duration,
    ) {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            if let Some(d) = detail_of(registry)
                && d.contains(needle)
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "detail never mentioned {needle:?} (detail: {:?})",
                detail_of(registry)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn detail_of(registry: &SubsystemReadinessRegistry) -> Option<String> {
        registry
            .snapshot()
            .into_iter()
            .find(|(id, _, _, _)| id == &"mqtt".into())
            .and_then(|(_, _, _, detail)| detail)
    }
}