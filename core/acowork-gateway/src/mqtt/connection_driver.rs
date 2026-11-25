//! Connection-driven action scheduling for MQTT consumers.
//!
//! A consumer that publishes into the broker has two independent facts to
//! respect: *its own* reason to publish (a resource changed, a snapshot
//! transitioned) and *the session's* ability to carry the publish.
//!
//! ## What is actually lost, and what is not
//!
//! Verified against rumqttc 0.25 `EventLoop::clean()`: on a connection
//! loss the request channel is **drained into `pending`** and replayed
//! after the reconnect. So a publish issued while disconnected is *not*
//! silently dropped — wiring only the consumer's own wake source still
//! delivers it, eventually. Three real gaps remain, and they are what
//! this module addresses:
//!
//! 1. **A broker restart wipes every Retained payload.** rumqttd keeps
//!    retained messages in memory. Once a publish has been acknowledged
//!    it is no longer pending, so nothing replays it — a snapshot whose
//!    value never changes again is absent from the broker forever.
//! 2. **A soft-restart rebuilds the event loop**, discarding the old
//!    `pending` list along with it.
//! 3. **A full request channel** (`queue_capacity`) makes `publish`
//!    return `Err`, which log-and-continue callers drop.
//!
//! Re-publishing on every `Connected` edge closes all three, because it
//! re-derives the payload from current state instead of relying on
//! anything the transport remembered.
//!
//! ## The contract
//!
//! [`ConnectionDriver`] resolves on every `Connected` edge of the session,
//! including the one already in effect when the consumer starts. A
//! consumer combines it with its own wake source in a `select!` and, on
//! either branch, acts only if the session is currently connected. Work
//! that arrives while disconnected is held (coalesced) and released on the
//! next edge — never dropped, never busy-retried.
//!
//! ## Why there is no timer here
//!
//! The session state machine guarantees a bounded time to its next
//! transition (poll watchdog / backoff ceiling), so a consumer parked on
//! this driver is woken by facts within a known bound. Adding a second
//! timeout here would duplicate that guarantee and let the two disagree.
//! A consumer with its own deadline (a request/response exchange, a boot
//! budget) sets that at its own level, where the business consequence of
//! expiry is known.
//!
//! ## Why the driver holds no mutable state
//!
//! `select!` drops the losing branch's future, so any "have I delivered the
//! current episode yet?" flag mutated before an await would be a
//! lost-wakeup bug: the flag would say *delivered* while the value was
//! thrown away. Instead the cursor is rewound once at construction
//! ([`SessionStateRx::mark_unseen`]) and every wait is a plain `changed()`.
//! Cancellation then drops nothing but an unadvanced receiver clone.

use acowork_mqtt_session::{SessionState, SessionStateRx};

use crate::mqtt::client::GatewayMqttClient;

/// Outcome of waiting for the next usable connection.
///
/// The two ending cases stay distinct: a session that published a terminal
/// state shut down in an orderly way, while a vanished state source means
/// the owner died without doing so. Collapsing them would hide the second.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Drive {
    /// The session is connected; the consumer may publish now.
    Connected,
    /// The session reached a terminal state and will not connect again.
    Terminal(SessionState),
    /// Every state sender was dropped without a terminal state.
    SourceGone,
}

/// Resolves on each `Connected` edge of one session.
pub struct ConnectionDriver {
    rx: SessionStateRx,
}

impl ConnectionDriver {
    /// Subscribe **before** performing the first action.
    ///
    /// Constructing this after an initial publish leaves a window where a
    /// reconnect during that publish is already marked seen and therefore
    /// never delivered, so the consumer skips the replay that reconnect
    /// owed it.
    pub fn new(client: &GatewayMqttClient) -> Self {
        let rx = client.state_rx();
        rx.mark_unseen();
        Self { rx }
    }

    /// Whether a publish issued right now would be carried.
    pub fn is_connected(&self) -> bool {
        self.rx.current().is_connected()
    }

    /// Wait for the next `Connected` edge, skipping intermediate states.
    ///
    /// Cancel-safe: no state is mutated across the await, so a `select!`
    /// that drops this future loses nothing.
    pub async fn next_connected(&self) -> Drive {
        loop {
            match self.rx.changed().await {
                Ok(state) if state.is_connected() => return Drive::Connected,
                Ok(state) if state.is_terminal() => return Drive::Terminal(state),
                Ok(_) => continue,
                Err(_) => return Drive::SourceGone,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acowork_mqtt_session::SessionStateTx;
    use std::time::Duration;

    /// The driver reads the state broadcast, not a live socket, so the
    /// tests drive a `SessionStateTx` directly through the same receiver
    /// type the client hands out.
    fn driver_from(tx: &SessionStateTx) -> ConnectionDriver {
        let rx = tx.subscribe();
        rx.mark_unseen();
        ConnectionDriver { rx }
    }

    #[tokio::test]
    async fn already_connected_session_delivers_immediately() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connected);
        let driver = driver_from(&tx);
        // Without `mark_unseen` this parks forever: `subscribe` marks the
        // starting value as seen.
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), driver.next_connected())
                .await
                .expect("an already-connected session must not require a new edge"),
            Drive::Connected
        );
    }

    #[tokio::test]
    async fn connecting_session_delivers_on_the_first_connack() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connecting);
        let driver = driver_from(&tx);
        let wait = tokio::spawn(async move { driver.next_connected().await });
        tokio::task::yield_now().await;
        tx.set(SessionState::Connected);
        assert_eq!(wait.await.unwrap(), Drive::Connected);
    }

    #[tokio::test]
    async fn retrying_states_are_skipped_not_delivered() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connecting);
        let driver = driver_from(&tx);
        let wait = tokio::spawn(async move { driver.next_connected().await });
        tokio::task::yield_now().await;
        for attempt in 1..=3 {
            tx.set(SessionState::Retrying {
                reason: "E1".into(),
                attempt,
                retry_in: Duration::from_millis(10),
            });
        }
        assert!(!wait.is_finished(), "a retry is not a usable connection");
        tx.set(SessionState::Connected);
        assert_eq!(wait.await.unwrap(), Drive::Connected);
    }

    #[tokio::test]
    async fn every_reconnect_edge_is_delivered() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connected);
        let driver = driver_from(&tx);
        assert_eq!(driver.next_connected().await, Drive::Connected);
        for _ in 0..3 {
            // The consumer is busy publishing across this whole episode;
            // the watch cursor must still owe it the new edge.
            tx.set(SessionState::Retrying {
                reason: "E1".into(),
                attempt: 1,
                retry_in: Duration::from_millis(10),
            });
            tx.set(SessionState::Connected);
            assert_eq!(driver.next_connected().await, Drive::Connected);
        }
    }

    #[tokio::test]
    async fn cancelled_wait_loses_no_edge() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connecting);
        let driver = driver_from(&tx);
        // The realistic `select!` shape: this wait is polled and parks,
        // then the consumer's own wake source fires and the parked future
        // is dropped. A driver that tracked "episode already delivered"
        // in a field mutated before the await would corrupt it here.
        tokio::select! {
            biased;
            _ = driver.next_connected() => panic!("must not resolve while Connecting"),
            () = tokio::task::yield_now() => {}
        }
        tx.set(SessionState::Connected);
        assert_eq!(driver.next_connected().await, Drive::Connected);
    }

    #[tokio::test]
    async fn terminal_and_vanished_source_are_distinct() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connecting);
        let driver = driver_from(&tx);
        let wait = tokio::spawn(async move { driver.next_connected().await });
        tokio::task::yield_now().await;
        tx.set(SessionState::Closed { reason: "shutdown".into() });
        assert!(matches!(wait.await.unwrap(), Drive::Terminal(_)));

        let (tx2, _rx2) = SessionStateTx::new(SessionState::Connecting);
        let driver2 = driver_from(&tx2);
        let wait2 = tokio::spawn(async move { driver2.next_connected().await });
        tokio::task::yield_now().await;
        drop(tx2);
        assert_eq!(wait2.await.unwrap(), Drive::SourceGone);
    }
}