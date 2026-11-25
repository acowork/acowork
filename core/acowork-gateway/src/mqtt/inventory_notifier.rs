//! Inventory-change signal publisher.
//!
//! Owns the `acowork/desktop/inventory` MQTT topic. Whenever the Gateway
//! mutates its aggregated `installed_agents` table (a Node reports a
//! freshly-installed package, a Node replays its retained inventory, or
//! the Gateway itself drops an entry on `DELETE /api/agents/{id}`), it
//! publishes a small signal on this topic.
//!
//! The signal carries no inventory data: the Desktop App (the only
//! subscriber today) fetches the authoritative list via
//! `GET /api/agents`, which combines `installed_agents` with the MQTT
//! AgentRegistry's liveness verdict. The signal just collapses the
//! previous 30 s `setInterval` fallback into a real-time push. Its
//! payload is a millisecond timestamp, purely so a human tailing the
//! broker can see *when* the change landed.
//!
//! ## Why a separate signal topic (not piggyback on `bootstrap`)
//!
//! `acowork/global/bootstrap` (ADR-059) is owned by the subsystem
//! orchestrator. Its `version` counter bumps only on subsystem readiness
//! transitions; inventory changes do NOT bump it. Smuggling inventory
//! signals through bootstrap would either (a) violate ADR-059 semantics
//! or (b) require an awkward `inventory_hint` field on `BootstrapState`
//! that the orchestrator has no business computing. A dedicated signal
//! topic keeps both contracts clean.
//!
//! ## Why `acowork/desktop/` and not `acowork/global/`
//!
//! Every Runtime subscribes to `acowork/global/#` and decodes each
//! payload there as a protobuf `DataEnvelope` (see `acowork-runtime`'s
//! `mqtt::available_cache::update_from_mqtt`). Anything on that prefix
//! that is not a `DataEnvelope` makes every Runtime log "Failed to
//! decode DataEnvelope from global resource topic" — once per inventory
//! change, plus once per Runtime boot. This signal is addressed to the
//! Desktop, not to the agents, so it lives outside the contract that
//! prefix carries.
//!
//! ## Why `retain = false`
//!
//! This is a change *event*, not state. Retaining it would hand every
//! later subscriber a hit it cannot distinguish from a fresh change, for
//! no gain: the catch-up is owned by the subscriber, which refetches
//! `GET /api/agents` on every MQTT (re)connect edge. That also covers the
//! case retention cannot — a change that happened before the broker
//! restarted.
//!
//! The payload is a timestamp rather than empty simply so the signal is
//! legible when tailing the broker; subscribers ignore its content.

use std::sync::Arc;

use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::mqtt::client::{GatewayMqttClient, MqttQoS};
use crate::mqtt::connection_driver::{ConnectionDriver, Drive};

/// Build the signal payload: current wall-clock time in milliseconds,
/// as ASCII. Non-empty by construction (see the module doc — a
/// zero-length payload is never delivered).
fn inventory_signal_payload() -> Vec<u8> {
    let ts_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    ts_ms.to_string().into_bytes()
}

/// MQTT topic for the inventory-change signal.
pub const TOPIC_INVENTORY: &str = "acowork/desktop/inventory";

/// Owns the publish loop that emits the `acowork/desktop/inventory`
/// signal on every notify.
pub struct MqttInventoryNotifier {
    client: GatewayMqttClient,
    notify: Arc<Notify>,
}

impl MqttInventoryNotifier {
    pub fn new(client: GatewayMqttClient) -> Self {
        Self {
            client,
            notify: Arc::new(Notify::new()),
        }
    }

    /// Start the background publish loop.
    ///
    /// The loop wakes on a trigger or on a session `Connected` edge and
    /// publishes the signal payload on [`TOPIC_INVENTORY`] when a change
    /// is pending and the session can carry it.
    ///
    /// A trigger raised while disconnected is held until the next edge
    /// rather than dropped: the subscriber's fetch-on-(re)connect covers
    /// its own reconnect, not the Gateway's. Publish errors keep the
    /// change pending so the next wake retries.
    pub fn start(&self) -> InventoryNotifierHandle {
        let notify = self.notify.clone();
        let client = self.client.clone();
        let task: JoinHandle<()> = tokio::spawn(async move {
            tracing::info!("MQTT Inventory Notifier loop started");
            let driver = ConnectionDriver::new(&client);
            // A change that lands while the session cannot carry it is
            // held here rather than left to the transport's pending
            // list, which a soft-restart discards. The Desktop's own
            // fetch-on-reconnect only covers *its* reconnect — the
            // Gateway session can drop and recover while the Desktop
            // stays connected, and then nothing else would signal the
            // change at all.
            //
            // Coalescing is correct here and not an optimisation: the
            // payload carries no inventory data, so N pending changes
            // and one pending change ask the subscriber for the same
            // thing — refetch the authoritative list.
            let mut pending = false;
            loop {
                tokio::select! {
                    () = notify.notified() => {
                        tracing::debug!("MQTT inventory notifier: triggered publish");
                        pending = true;
                    }
                    drive = driver.next_connected() => {
                        match drive {
                            // Re-evaluate `pending`; do NOT signal
                            // unconditionally. A reconnect with no
                            // pending change is not an inventory change.
                            Drive::Connected => {}
                            Drive::Terminal(state) => {
                                tracing::warn!(%state, "MQTT inventory notifier: session terminal; loop exiting");
                                return;
                            }
                            Drive::SourceGone => {
                                tracing::warn!("MQTT inventory notifier: state source gone; loop exiting");
                                return;
                            }
                        }
                    }
                }
                if !pending || !driver.is_connected() {
                    continue;
                }
                match client
                    .publish_raw(
                        TOPIC_INVENTORY,
                        inventory_signal_payload(),
                        MqttQoS::AtLeastOnce,
                        false,
                    )
                    .await
                {
                    Ok(()) => pending = false,
                    Err(e) => {
                        // Keep `pending`: the next edge or trigger retries.
                        tracing::warn!(
                            error = %e,
                            topic = TOPIC_INVENTORY,
                            "MQTT inventory notifier: publish failed; holding signal for next wake"
                        );
                    }
                }
            }
        });
        InventoryNotifierHandle { _task: task }
    }

    /// Clonable trigger that callers use to wake the publish loop.
    pub fn create_trigger(&self) -> InventoryNotifierTrigger {
        InventoryNotifierTrigger {
            notify: self.notify.clone(),
        }
    }
}

/// Handle returned by `start()`.
///
/// `tokio` detaches (does not abort) a `JoinHandle` on drop, so dropping
/// this handle leaves the publish loop running. It is kept by the
/// Gateway purely so the loop's lifetime is tied to the Gateway's rather
/// than floating: bind it for as long as the loop should live.
pub struct InventoryNotifierHandle {
    _task: JoinHandle<()>,
}

/// Clonable trigger stored in `AppState` / `DispatchContext` so any
/// inventory-mutating code path can wake the publisher without holding
/// the full handle.
#[derive(Clone)]
pub struct InventoryNotifierTrigger {
    pub(crate) notify: Arc<Notify>,
}

impl InventoryNotifierTrigger {
    /// Signal that the aggregated `installed_agents` table just changed.
    /// Multiple `notify()` calls during a burst coalesce — the publish
    /// loop publishes at most once per wakeup, and the signal is
    /// idempotent for the subscriber.
    pub fn notify(&self) {
        self.notify.notify_one();
    }

    /// Test-only constructor: build a trigger around a caller-supplied
    /// `Notify` so the test can `notified().await` on the same handle
    /// to observe wake-ups. Production callers go through
    /// [`MqttInventoryNotifier::create_trigger`] which always supplies
    /// a fresh `Notify` tied to its own publish loop.
    #[cfg(test)]
    pub fn for_test(notify: Notify) -> Self {
        Self {
            notify: Arc::new(notify),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rumqttc::{AsyncClient, EventLoop, MqttOptions, QoS};
    use std::time::Duration;

    fn subscriber(port: u16, client_id: &str) -> (AsyncClient, EventLoop) {
        let mut opts = MqttOptions::new(client_id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        AsyncClient::new(opts, 10)
    }

    /// Poll the eventloop for a publish on `topic`, returning the payload
    /// if one arrives within `budget`.
    async fn next_publish_on(
        eventloop: &mut EventLoop,
        topic: &str,
        budget: Duration,
    ) -> Option<Vec<u8>> {
        let start = std::time::Instant::now();
        while start.elapsed() < budget {
            let remaining = budget.saturating_sub(start.elapsed());
            match tokio::time::timeout(
                remaining.min(Duration::from_millis(100)),
                eventloop.poll(),
            )
            .await
            {
                Ok(Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(p)))) => {
                    if p.topic == topic {
                        return Some(p.payload.to_vec());
                    }
                }
                Ok(_) => {
                    // ConnAck / SubAck / PingResp / transient error — yield
                    // instead of pinning a core while we wait.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(_) => {}
            }
        }
        None
    }

    /// Subscribe **and drive the eventloop until SUBACK**.
    ///
    /// `AsyncClient::subscribe` only queues the request — the SUBSCRIBE
    /// packet is not on the wire until `EventLoop::poll` runs. Publishing
    /// straight after `subscribe().await` therefore loses the first
    /// message, which is exactly the kind of harness bug that makes a
    /// delivery test lie in both directions.
    async fn subscribe_and_wait(
        client: &AsyncClient,
        eventloop: &mut EventLoop,
        filter: &str,
    ) {
        client.subscribe(filter, QoS::AtLeastOnce).await.unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if let Ok(Ok(rumqttc::Event::Incoming(rumqttc::Incoming::SubAck(_)))) =
                tokio::time::timeout(Duration::from_millis(100), eventloop.poll()).await
            {
                return;
            }
        }
        panic!("no SUBACK for {filter} within 5s");
    }

    async fn start_notifier(port: u16) -> (InventoryNotifierTrigger, InventoryNotifierHandle) {
        let client = GatewayMqttClient::new_publisher("127.0.0.1", port)
            .await
            .expect("client should connect");
        let notifier = MqttInventoryNotifier::new(client);
        let handle = notifier.start();
        (notifier.create_trigger(), handle)
    }

    /// An already-connected subscriber receives the signal, and the
    /// payload is NON-EMPTY. This is the regression guard for the bug
    /// that shaped the design: rumqttd never delivers a zero-length
    /// payload publish here, so an "empty signal" reaches nobody (see
    /// the module doc).
    #[tokio::test]
    async fn inventory_signal_reaches_connected_subscriber() {
        let port = 18981;
        let broker =
            crate::mqtt::broker::start_broker("127.0.0.1", port).expect("broker should start");
        let (trigger, handle) = start_notifier(port).await;

        let (sub, mut eventloop) = subscriber(port, "test:inventory-sub-a");
        subscribe_and_wait(&sub, &mut eventloop, TOPIC_INVENTORY).await;

        trigger.notify();

        let payload = next_publish_on(&mut eventloop, TOPIC_INVENTORY, Duration::from_secs(5))
            .await
            .expect("a connected subscriber must receive the inventory signal");
        assert!(
            !payload.is_empty(),
            "the signal payload must be non-empty: rumqttd drops zero-length \
             payload publishes, so an empty signal would reach no subscriber"
        );

        drop(sub);
        drop(handle);
        drop(broker);
    }

    /// The signal must NOT be retained: a subscriber that connects after
    /// the change must not receive a stale hit (it would be indistinguishable
    /// from a fresh change). This is exactly why the Desktop additionally
    /// refetches on every MQTT connect edge instead of relying on a
    /// retained replay — an empty payload with `retain = true` would only
    /// DELETE the retained message (MQTT §3.3.1.3), leaving a late
    /// subscriber with nothing anyway.
    #[tokio::test]
    async fn inventory_signal_leaves_no_retained_message_for_late_subscribers() {
        let port = 18982;
        let broker =
            crate::mqtt::broker::start_broker("127.0.0.1", port).expect("broker should start");
        let (trigger, handle) = start_notifier(port).await;

        // Publish with nobody subscribed.
        trigger.notify();
        tokio::time::sleep(Duration::from_millis(300)).await;

        // A subscriber arriving afterwards must see nothing.
        let (sub, mut eventloop) = subscriber(port, "test:inventory-sub-b");
        subscribe_and_wait(&sub, &mut eventloop, TOPIC_INVENTORY).await;

        let late = next_publish_on(&mut eventloop, TOPIC_INVENTORY, Duration::from_millis(800))
            .await;
        assert!(
            late.is_none(),
            "the inventory signal must not be retained, but a late subscriber received {:?}",
            late
        );

        drop(sub);
        drop(handle);
        drop(broker);
    }


}
