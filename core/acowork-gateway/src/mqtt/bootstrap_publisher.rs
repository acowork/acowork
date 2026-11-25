//! ADR-059: Bootstrap snapshot MQTT publisher.
//!
//! Owns the retained `acowork/global/bootstrap` topic. Subscribes to the
//! [`BootstrapOrchestrator`]'s change signal (a `watch` channel) and
//! republishes the latest snapshot on every change, so consumers
//! (Desktop, Runtime, Node, remote tooling) always see the Gateway's
//! current aggregated phase when they (re)connect to the broker.
//!
//! OCP: the publisher emits ONLY the wire-level `BootstrapState` payload
//! defined in `mqtt_payload.proto`. It does NOT expose per-subsystem
//! detail (see ADR-059 §5.4.4). Adding or renaming an internal subsystem
//! never changes this publisher's output.

use std::sync::Arc;

use tokio::task::JoinHandle;

use acowork_core::mqtt_proto::{
    self, data_envelope, BootstrapState as BootstrapStateProto, DataEnvelope,
};

use crate::bootstrap::{BootstrapOrchestrator, BootstrapSnapshot};
use crate::mqtt::client::{GatewayMqttClient, MqttQoS};
use crate::mqtt::connection_driver::{ConnectionDriver, Drive};

/// MQTT topic for the bootstrap snapshot (ADR-059 §5.1).
///
/// Retained, QoS 1. The Gateway is the sole publisher on this topic.
pub const TOPIC_BOOTSTRAP: &str = "acowork/global/bootstrap";

/// Handle returned by [`BootstrapPublisher::start`]. Dropping it stops
/// the publisher loop.
///
/// The handle is intentionally `Clone`-incompatible: there is exactly
/// one retained publisher per Gateway and dropping the handle is the
/// only way to stop the loop. (Borrowing [`MqttPublisherHandle`] from
/// the existing [`crate::mqtt::MqttGlobalResourcesPublisher`] is fine
/// for trigger / ready coordination, but the bootstrap publisher owns
/// its own background task.)
pub struct BootstrapPublisherHandle {
    _task: JoinHandle<()>,
    /// Held so that the orchestrator (and therefore the change signal)
    /// stays alive for the lifetime of the publisher loop.
    _orchestrator: Arc<BootstrapOrchestrator>,
}

/// Configurable knobs for [`BootstrapPublisher::start`].
pub struct BootstrapPublisherOptions<'a> {
    /// MQTT client to publish with. Usually the same
    /// `GatewayMqttClient` used by [`crate::mqtt::MqttGlobalResourcesPublisher`].
    pub client: &'a GatewayMqttClient,
    /// Orchestrator providing the live snapshot.
    pub orchestrator: Arc<BootstrapOrchestrator>,
}

/// Bootstrap snapshot publisher.
///
/// Construct via [`BootstrapPublisher::start`], which spawns the
/// background loop. The loop:
///
/// 1. Publishes on the first `Connected` edge, which may be the one
///    already in effect at startup. rumqttc queues a publish issued
///    before the ConnAck and replays it, so this is about ordering and
///    freshness rather than about the seed being dropped.
///
/// 2. Republishes on every orchestrator transition AND on every
///    subsequent `Connected` edge. No periodic polling — both sources
///    are `watch`-driven, and their level-triggered semantics guarantee
///    a transition that happens while the loop is publishing is never
///    lost (ADR-059 §7.2 re-delivery consistency). The reconnect edge
///    is NOT redundant with the transition source: rumqttd keeps
///    retained messages in memory, so a broker restart wipes the
///    snapshot while the phase stays exactly where it was — no
///    transition would ever republish it, and every later subscriber
///    would find the topic empty.
///
/// 3. Logs every publish (DEBUG) and any error (WARN). A failure to
///    publish does NOT abort the loop: the next wake retries.
pub struct BootstrapPublisher;

impl BootstrapPublisher {
    /// Start the bootstrap publisher loop.
    ///
    /// The returned handle owns the background task; dropping it stops
    /// the loop. The orchestrator is held alive for the same lifetime
    /// so the change-notify subscription never dangles.
    pub fn start(opts: BootstrapPublisherOptions<'_>) -> BootstrapPublisherHandle {
        let mut changes = opts.orchestrator.subscribe_changes();
        let orchestrator = opts.orchestrator.clone();
        let client = opts.client.clone();
        // Seed the retained payload BEFORE returning so the caller can
        // rely on `acowork/global/bootstrap` being available immediately.
        let initial_snapshot = orchestrator.snapshot();
        let initial_topic = TOPIC_BOOTSTRAP.to_string();
        let initial_client = client.clone();
        // Use a oneshot-friendly publish: the broker may not be ready
        // at construction time, so we spawn a background loop that
        // first publishes the initial snapshot and then waits for
        // change notifications.
        let orchestrator_for_loop = orchestrator.clone();
        let task = tokio::spawn(async move {
            tracing::info!(
                topic = initial_topic,
                instance_id = %initial_snapshot.instance_id,
                phase = ?initial_snapshot.phase,
                version = initial_snapshot.version,
                "Bootstrap publisher: starting loop"
            );

            // Subscribe to the connection BEFORE the first publish, so a
            // reconnect that happens during that publish is still owed.
            let driver = ConnectionDriver::new(&initial_client);

            // Two wake sources, no polling:
            //  * every `Connected` edge — rumqttd holds retained
            //    messages in memory, so a broker restart wipes the
            //    snapshot and a phase that does not change would never
            //    republish it;
            //  * every orchestrator transition.
            //
            // Both branches read the LATEST snapshot rather than a value
            // captured when the wake was queued, so a burst of
            // transitions collapsed into one publish still converges.
            loop {
                tokio::select! {
                    drive = driver.next_connected() => {
                        match drive {
                            Drive::Connected => {
                                tracing::debug!(
                                    "Bootstrap publisher: connected, publishing retained snapshot"
                                );
                            }
                            Drive::Terminal(state) => {
                                tracing::warn!(%state, "Bootstrap publisher: session terminal; loop exiting");
                                break;
                            }
                            Drive::SourceGone => {
                                tracing::warn!("Bootstrap publisher: state source gone; loop exiting");
                                break;
                            }
                        }
                    }
                    changed = changes.changed() => {
                        if changed.is_err() {
                            // The orchestrator was dropped — nothing left
                            // to publish.
                            break;
                        }
                        if !driver.is_connected() {
                            // Hold the transition: the next `Connected`
                            // edge republishes the latest snapshot, so
                            // nothing is lost by skipping this one.
                            continue;
                        }
                    }
                }
                let snapshot = orchestrator_for_loop.snapshot();
                publish_snapshot(&initial_client, &snapshot).await;
            }
        });
        BootstrapPublisherHandle {
            _task: task,
            _orchestrator: orchestrator,
        }
    }
}

/// Publish a single snapshot. Errors are logged but never propagated —
/// the publisher loop keeps running and the next change will retry.
async fn publish_snapshot(client: &GatewayMqttClient, snapshot: &BootstrapSnapshot) {
    let envelope = envelope_from_snapshot(snapshot);
    match client
        .publish_envelope(TOPIC_BOOTSTRAP, &envelope, MqttQoS::AtLeastOnce, true)
        .await
    {
        Ok(()) => {
            tracing::debug!(
                topic = TOPIC_BOOTSTRAP,
                instance_id = %snapshot.instance_id,
                phase = ?snapshot.phase,
                version = snapshot.version,
                "Bootstrap snapshot published (Retained)"
            );
        }
        Err(e) => {
            tracing::warn!(
                topic = TOPIC_BOOTSTRAP,
                instance_id = %snapshot.instance_id,
                phase = ?snapshot.phase,
                version = snapshot.version,
                error = %e,
                "Bootstrap snapshot publish failed; will retry on next change"
            );
        }
    }
}

/// Convert an internal [`BootstrapSnapshot`] into the wire-level
/// `DataEnvelope` carrying `BootstrapState`.
fn envelope_from_snapshot(snapshot: &BootstrapSnapshot) -> DataEnvelope {
    let proto = BootstrapStateProto {
        protocol_version: snapshot.protocol_version,
        instance_id: snapshot.instance_id.clone(),
        version: snapshot.version,
        phase: snapshot.phase.as_proto_i32(),
        phase_detail: snapshot.phase_detail.clone(),
        issued_at_ms: snapshot.issued_at_ms,
    };
    DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::BootstrapState(proto)),
    }
}

// Suppress dead_code lint for the unused import in tests — prost types
// are referenced by name inside test payloads below.
#[allow(dead_code)]
fn _ensure_proto_import_kept() -> mqtt_proto::BootstrapState {
    mqtt_proto::BootstrapState::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_carries_bootstrap_state_payload() {
        let snap = BootstrapSnapshot::booting("instance-A".to_string());
        let env = envelope_from_snapshot(&snap);
        match env.payload {
            Some(data_envelope::Payload::BootstrapState(bs)) => {
                assert_eq!(bs.instance_id, "instance-A");
                assert_eq!(bs.protocol_version, 1);
                assert_eq!(bs.phase, 1); // Booting
            }
            other => panic!("expected BootstrapState payload, got {:?}", other),
        }
    }

    #[test]
    fn envelope_propagates_phase_transitions() {
        use crate::bootstrap::orchestrator::{BootstrapPhase, BootstrapSnapshot};

        let snap = BootstrapSnapshot {
            protocol_version: 1,
            instance_id: "7b8c9d0e-1f2a-4023-8243-f35d6e7f8a9b".to_string(),
            version: 7,
            phase: BootstrapPhase::Ready,
            phase_detail: "1/1 required ready".to_string(),
            issued_at_ms: 12345,
        };
        let env = envelope_from_snapshot(&snap);
        match env.payload {
            Some(data_envelope::Payload::BootstrapState(bs)) => {
                assert_eq!(bs.phase, 2); // Ready
                assert_eq!(bs.version, 7);
                assert_eq!(bs.phase_detail, "1/1 required ready");
                assert_eq!(bs.issued_at_ms, 12345);
            }
            other => panic!("expected BootstrapState payload, got {:?}", other),
        }
    }

    /// Regression: a reconnect must republish the retained snapshot even
    /// when the phase never changes.
    ///
    /// rumqttd keeps retained messages in memory, so a broker restart
    /// wipes `acowork/global/bootstrap`. An already-acknowledged publish
    /// is no longer in rumqttc's pending list, so the transport does not
    /// replay it either. The previous wiring woke only on orchestrator
    /// transitions — with the phase settled at READY, nothing ever
    /// republished, and every later subscriber found the topic empty and
    /// timed out its boot.
    ///
    /// A forced soft-reconnect stands in for the broker restart: it
    /// produces the same `Connected` edge, and the orchestrator is
    /// deliberately never touched, so any second publish can only come
    /// from the reconnect hook.
    #[tokio::test]
    async fn reconnect_republishes_without_a_phase_change() {
        use crate::bootstrap::registry::SubsystemReadinessRegistry;
        use rumqttc::{AsyncClient, MqttOptions, QoS};

        let port = crate::test_support::free_port();
        let _broker = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start on the free port");
        let registry = SubsystemReadinessRegistry::new_shared();
        let orchestrator = BootstrapOrchestrator::new("reconnect-instance".into(), registry);

        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("publisher session should connect");
        let _handle = BootstrapPublisher::start(BootstrapPublisherOptions {
            client: &client,
            orchestrator: orchestrator.clone(),
        });

        let mut opts = MqttOptions::new("test:bootstrap-reconnect-sub", "127.0.0.1", port);
        opts.set_keep_alive(std::time::Duration::from_secs(5));
        let (sub, mut events) = AsyncClient::new(opts, 10);
        sub.subscribe(TOPIC_BOOTSTRAP, QoS::AtLeastOnce)
            .await
            .unwrap();

        // Count bootstrap publishes seen on a live subscription.
        async fn count_publishes(
            events: &mut rumqttc::EventLoop,
            want: usize,
            budget: std::time::Duration,
        ) -> usize {
            let start = std::time::Instant::now();
            let mut seen = 0;
            while start.elapsed() < budget && seen < want {
                let remaining = budget.saturating_sub(start.elapsed());
                match tokio::time::timeout(
                    remaining.min(std::time::Duration::from_millis(100)),
                    events.poll(),
                )
                .await
                {
                    Ok(Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p)))) => {
                        if p.topic == TOPIC_BOOTSTRAP && !p.payload.is_empty() {
                            seen += 1;
                        }
                    }
                    Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                    Err(_) => {}
                }
            }
            seen
        }

        let first = count_publishes(&mut events, 1, std::time::Duration::from_secs(10)).await;
        assert_eq!(first, 1, "seed snapshot must reach the subscriber");

        // The phase is untouched from here on.
        let version_before = orchestrator.snapshot().version;
        client.force_reconnect();

        let second = count_publishes(&mut events, 1, std::time::Duration::from_secs(15)).await;
        assert_eq!(
            orchestrator.snapshot().version,
            version_before,
            "precondition: no orchestrator transition may drive the republish"
        );
        assert_eq!(
            second, 1,
            "a reconnect must republish the retained snapshot with no phase change"
        );
    }

    /// Characterization: the seed snapshot reaches a subscriber that
    /// attaches after the broker comes up.
    ///
    /// This passes with and without the reconnect hook — rumqttc queues
    /// the early publish and replays it — so it is NOT a regression test
    /// for the hook. It pins the late-broker startup path, which is the
    /// normal case now that the session is constructed without waiting
    /// for a ConnAck.
    #[tokio::test]
    async fn seed_publish_is_carried_when_the_broker_starts_late() {
        use crate::bootstrap::registry::SubsystemReadinessRegistry;
        use rumqttc::{AsyncClient, MqttOptions, QoS};

        let port = crate::test_support::free_port();
        let registry = SubsystemReadinessRegistry::new_shared();
        let orchestrator = BootstrapOrchestrator::new("late-broker-instance".into(), registry);

        // No broker on `port` yet: the session can only be Connecting /
        // Retrying while the publisher starts.
        let client = GatewayMqttClient::spawn("127.0.0.1", port, "test:bootstrap-late", None, None)
            .await
            .expect("session construction must not require a broker");
        assert!(
            !client.current_state().is_connected(),
            "precondition: the session must start disconnected"
        );

        let _handle = BootstrapPublisher::start(BootstrapPublisherOptions {
            client: &client,
            orchestrator: orchestrator.clone(),
        });

        // Give the loop a chance to run its (previously fatal) seed
        // publish into the dead session before the broker exists.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let _broker = crate::mqtt::broker::start_broker("127.0.0.1", port)
            .expect("broker should start on the free port");

        // A fresh subscriber: whatever it receives on the bootstrap topic
        // must have been (re)published after the broker came up.
        let mut opts = MqttOptions::new("test:bootstrap-sub", "127.0.0.1", port);
        opts.set_keep_alive(std::time::Duration::from_secs(5));
        let (sub, mut events) = AsyncClient::new(opts, 10);
        sub.subscribe(TOPIC_BOOTSTRAP, QoS::AtLeastOnce)
            .await
            .unwrap();

        let budget = std::time::Duration::from_secs(15);
        let start = std::time::Instant::now();
        let mut got = false;
        while start.elapsed() < budget && !got {
            let remaining = budget.saturating_sub(start.elapsed());
            match tokio::time::timeout(
                remaining.min(std::time::Duration::from_millis(100)),
                events.poll(),
            )
            .await
            {
                Ok(Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p)))) => {
                    if p.topic == TOPIC_BOOTSTRAP && !p.payload.is_empty() {
                        got = true;
                    }
                }
                Ok(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
                Err(_) => {}
            }
        }
        assert!(
            got,
            "retained bootstrap snapshot must be published once the session connects, \
             with no orchestrator change to drive it"
        );
    }
}