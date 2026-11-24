//! Session state machine (ADR-039 §5.1, ADR-065).
//!
//! Provides a `tokio::sync::watch`-based broadcast so that external
//! consumers (Tauri events, DevMode, health ledger, the Gateway's
//! bootstrap readiness projection) can observe the MQTT connection
//! lifecycle in real time.
//!
//! # State completeness contract
//!
//! Every non-terminal state MUST have a bounded exit: either an
//! event-driven transition or a time-driven one. A timeout is not a
//! mechanism bolted on next to the state machine — it is one of its
//! transitions, and its value comes from this module's own machinery
//! (keepalive, poll watchdog, reconnect backoff).
//!
//! ```mermaid
//! graph LR
//!     Connecting --> Connected
//!     Connecting --> Retrying
//!     Connected --> Retrying
//!     Retrying --> Connecting
//!     Connecting --> Closed
//!     Connected --> Closed
//!     Retrying --> Closed
//! ```
//!
//! | state | event exit | time exit | bound |
//! |---|---|---|---|
//! | `Connecting` | ConnAck / error | poll watchdog | `POLL_WATCHDOG_TIMEOUT` |
//! | `Connected` | error / DISCONNECT | poll watchdog + keepalive | `POLL_WATCHDOG_TIMEOUT` |
//! | `Retrying` | force-restart | backoff elapses | `ReconnectPolicy.max` / `FATAL_BACKOFF` |
//! | `Closed` | terminal — always emitted | — | — |
//!
//! Three consequences a consumer can rely on:
//!
//! 1. **The state value changes within a bounded time even during a
//!    total outage.** `Retrying` carries `attempt` and `retry_in`, so
//!    every retry is a *distinct* value, not the same string republished.
//!    A consumer that renders state therefore always makes progress and
//!    needs no compensating timeout of its own.
//! 2. **A terminal state is always published before the sender goes
//!    away.** A closed watch channel is an implementation bug, not a
//!    normal branch (see [`StateWait::SourceGone`]).
//! 3. **The declared state set equals the reachable state set.** Every
//!    variant here has a production producer in `client.rs`. A variant
//!    nothing can enter is not documentation — it is a trap that makes
//!    consumers write branches that never run while the verdict they
//!    actually need is never produced.
//!
//! # Why there is no `Failed` / `Disabled` / `Idle`
//!
//! Removed deliberately (they were declared, consumed by two renderers,
//! and produced by nothing):
//!
//! - **No `Failed`.** A session is a supervised resource: every failure
//!   it can observe is retryable given time or fresh credentials, so it
//!   never gives up and has no "abandoned" state. Even `AuthRefused` is
//!   transient — the remote CONNECT verifier refuses every login until
//!   the user service finishes loading. A bounded fatal budget would
//!   therefore kill sessions that were about to recover. The *permanent*
//!   verdict belongs one layer up: the readiness projection marks the
//!   subsystem `Failed` when the session cannot be constructed at all.
//! - **No `Disabled`.** A disabled connection is never constructed, so
//!   no session exists to report it. "Switched off" is owned by the
//!   readiness layer (`Skipped`), which knows the configuration.
//! - **No `Idle`.** The channel is created with `Connecting` because a
//!   session is only ever created in order to connect.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

/// The lifecycle state of an MQTT session.
///
/// Match exhaustively: a wildcard arm silently swallows states added
/// later, which is exactly how a consumer ends up stuck in an
/// intermediate state it does not know about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SessionState {
    /// `AsyncClient` created, waiting for `ConnAck`. Bounded by the
    /// poll watchdog: a black-holed TCP connect cannot park here
    /// forever.
    Connecting,

    /// `ConnAck` received, bootstrap completed. Ready for
    /// publish/subscribe.
    Connected,

    /// An attempt failed (or the connection dropped) and the next
    /// attempt is scheduled. This is the *only* failure state a
    /// healthy-but-unreachable broker produces — it is not terminal.
    ///
    /// `attempt` and `retry_in` are what make an outage observable:
    /// without them the value would be identical on every retry and a
    /// state-driven consumer would see no progress at all.
    Retrying {
        /// Human-readable cause of the failed attempt.
        reason: String,
        /// 1-based count of consecutive failed attempts.
        attempt: u32,
        /// How long until the next attempt starts. `Duration::ZERO`
        /// means the retry is immediate (soft-restart path).
        retry_in: Duration,
    },

    /// Stopped because the owner asked for it (shutdown, client
    /// dropped, gateway URL switched). Terminal.
    Closed {
        /// Human-readable reason.
        reason: String,
    },
}

impl SessionState {
    /// Returns `true` if the session is in a connected state and
    /// ready for publish/subscribe.
    pub fn is_connected(&self) -> bool {
        matches!(self, SessionState::Connected)
    }

    /// Returns `true` if the session is in a transient (non-terminal)
    /// state that may progress to `Connected`.
    pub fn is_transient(&self) -> bool {
        match self {
            SessionState::Connecting | SessionState::Retrying { .. } => true,
            SessionState::Connected | SessionState::Closed { .. } => false,
        }
    }

    /// Returns `true` if the session has stopped for good.
    pub fn is_terminal(&self) -> bool {
        match self {
            SessionState::Closed { .. } => true,
            SessionState::Connecting
            | SessionState::Connected
            | SessionState::Retrying { .. } => false,
        }
    }

    /// Consecutive failed attempts, when the state is `Retrying`.
    pub fn attempt(&self) -> Option<u32> {
        match self {
            SessionState::Retrying { attempt, .. } => Some(*attempt),
            SessionState::Connecting
            | SessionState::Connected
            | SessionState::Closed { .. } => None,
        }
    }

    /// The cause carried by this state, if any.
    pub fn reason(&self) -> Option<&str> {
        match self {
            SessionState::Retrying { reason, .. } | SessionState::Closed { reason } => Some(reason),
            SessionState::Connecting | SessionState::Connected => None,
        }
    }
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Connecting => write!(f, "connecting"),
            SessionState::Connected => write!(f, "connected"),
            SessionState::Retrying {
                reason,
                attempt,
                retry_in,
            } => write!(
                f,
                "retrying #{attempt} in {}ms ({reason})",
                retry_in.as_millis()
            ),
            SessionState::Closed { reason } => write!(f, "closed ({reason})"),
        }
    }
}

// ── watch channel wrappers ───────────────────────────────────

/// Sender side of the session-state broadcast.
///
/// Held by the event loop. Cloning is cheap (Arc inside).
#[derive(Clone)]
pub struct SessionStateTx {
    tx: watch::Sender<SessionState>,
}

impl SessionStateTx {
    /// Create a new broadcast channel with the given initial state.
    pub fn new(initial: SessionState) -> (Self, SessionStateRx) {
        let (tx, rx) = watch::channel(initial);
        (
            SessionStateTx { tx },
            SessionStateRx {
                rx: Arc::new(Mutex::new(rx)),
            },
        )
    }

    /// Update the state. Logs the transition at INFO level.
    ///
    /// Uses `send_modify` instead of `send` because `send` returns
    /// `Err` when there are no receivers – and in this architecture the
    /// receiver is usually not held (the `SessionStateRx` from `new()`
    /// is dropped).  `send_modify` unconditionally updates the stored
    /// value and provides a reference to the previous value, which is
    /// exactly what we need: `current()` reads via `borrow()` which
    /// always reflects the latest stored value regardless of receiver
    /// count.
    pub fn set(&self, state: SessionState) {
        let mut prev_str = String::new();
        self.tx.send_modify(|current| {
            if *current != state {
                prev_str = format!("{}", *current);
                tracing::info!(from = %prev_str, to = %state, "MQTT session state transition");
            }
            *current = state;
        });
    }

    /// Read the current state without subscribing.
    pub fn current(&self) -> SessionState {
        self.tx.borrow().clone()
    }

    /// Create a new receiver that can observe future state changes.
    /// The receiver will see the current state immediately on first
    /// `changed()` call.
    pub fn subscribe(&self) -> SessionStateRx {
        SessionStateRx {
            rx: Arc::new(Mutex::new(self.tx.subscribe())),
        }
    }
}

/// Outcome of a bounded wait on the state broadcast.
///
/// The three cases are deliberately distinct types rather than a
/// `bool` / `Option`: "the event has not arrived yet" and "the event
/// source is gone" are different faults and must not collapse into
/// one branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateWait {
    /// The predicate holds; carries the state that satisfied it.
    Matched(SessionState),
    /// The budget elapsed first. **Not a failure verdict** — the
    /// resource keeps working; this only tells the caller that its own
    /// wait is over. Carries the state observed at expiry.
    Elapsed(SessionState),
    /// Every sender was dropped without publishing a terminal state.
    /// A correct implementation publishes `Closed` / `Failed` first, so
    /// reaching this means the owner died abnormally.
    SourceGone,
}

/// Receiver side of the session-state broadcast.
///
/// Held by external consumers (Tauri command handlers, DevMode,
/// health ledger). Cloning is NOT supported – create separate
/// receivers with [`SessionStateTx::subscribe`] instead.
pub struct SessionStateRx {
    rx: Arc<Mutex<watch::Receiver<SessionState>>>,
}

use std::sync::Mutex;

impl SessionStateRx {
    /// Wait for the next state change, returning the new state.
    ///
    /// Returns `Err` if all senders have been dropped.
    pub async fn changed(&self) -> Result<SessionState, watch::error::SendError<SessionState>> {
        // Clone the receiver out of the mutex so the lock is not held
        // across the `.await` (clippy: await_holding_lock), then write
        // the advanced clone back. Without the write-back the stored
        // receiver keeps its old "seen" version, so every subsequent
        // call would observe the same already-consumed change and
        // return immediately — a busy loop for any consumer driving
        // itself off `changed()`.
        //
        // Cancellation needs no special handling: `watch::Receiver::changed`
        // advances the seen version only when it *returns* (tokio's
        // `maybe_changed`), and there is no await point between that
        // return and the write-back below. A cancelled wait therefore
        // drops an unadvanced clone — identical to the stored one — so
        // the cursor can never rewind. Pinned by
        // `cancelled_wait_does_not_rewind_the_cursor`.
        let mut rx = self.rx.lock().unwrap().clone();
        let result = rx.changed().await;
        let state = rx.borrow().clone();
        *self.rx.lock().unwrap() = rx;
        result.map_err(|_| watch::error::SendError(SessionState::Closed {
            reason: "sender dropped".into(),
        }))?;
        Ok(state)
    }

    /// Read the current state without waiting.
    pub fn current(&self) -> SessionState {
        self.rx.lock().unwrap().borrow().clone()
    }

    /// Wait until `predicate` holds, for at most `budget`.
    ///
    /// Level-triggered: the current state is tested first, so a
    /// consumer that subscribes after the transition still matches.
    /// Implemented as `select!` over the watch channel and a timer —
    /// never a sleep-and-poll loop.
    ///
    /// Expiry does not touch the connection: it ends *this wait*, and
    /// the caller decides what its own business logic does next.
    pub async fn wait_for(
        &self,
        predicate: impl Fn(&SessionState) -> bool,
        budget: Duration,
    ) -> StateWait {
        let current = self.current();
        if predicate(&current) {
            return StateWait::Matched(current);
        }
        let deadline = tokio::time::sleep(budget);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                changed = self.changed() => {
                    match changed {
                        Ok(state) if predicate(&state) => return StateWait::Matched(state),
                        Ok(_) => continue,
                        Err(_) => return StateWait::SourceGone,
                    }
                }
                () = &mut deadline => return StateWait::Elapsed(self.current()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn retrying(reason: &str) -> SessionState {
        SessionState::Retrying {
            reason: reason.into(),
            attempt: 1,
            retry_in: Duration::from_millis(500),
        }
    }

    #[test]
    fn state_transitions() {
        assert!(SessionState::Connecting.is_transient());
        assert!(!SessionState::Connecting.is_connected());

        assert!(SessionState::Connected.is_connected());
        assert!(!SessionState::Connected.is_transient());

        assert!(retrying("test").is_transient());
        assert!(!retrying("test").is_connected());

        let terminal = SessionState::Closed {
            reason: "test".into(),
        };
        assert!(terminal.is_terminal(), "{terminal} must be terminal");
        assert!(!terminal.is_connected());
        assert!(!terminal.is_transient());
    }

    /// Every declared variant must be classified by every predicate.
    /// With `Closed` the only terminal, a future variant that is neither
    /// transient nor terminal would slip through `is_transient` /
    /// `is_terminal` unnoticed — this pins the partition.
    #[test]
    fn every_state_is_either_transient_connected_or_terminal() {
        for state in [
            SessionState::Connecting,
            SessionState::Connected,
            retrying("test"),
            SessionState::Closed {
                reason: "test".into(),
            },
        ] {
            let classified = state.is_transient() as u8
                + state.is_connected() as u8
                + state.is_terminal() as u8;
            assert_eq!(classified, 1, "{state} must belong to exactly one class");
        }
    }

    #[test]
    fn display_formats() {
        assert_eq!(SessionState::Connecting.to_string(), "connecting");
        assert_eq!(SessionState::Connected.to_string(), "connected");
        assert_eq!(
            SessionState::Closed {
                reason: "bye".into()
            }
            .to_string(),
            "closed (bye)"
        );
    }

    /// Regression: the drop reason must survive into the snapshot the
    /// Desktop's diagnostic panel reads.
    #[test]
    fn retrying_carries_the_drop_reason() {
        let state = retrying("eventloop error: Websocket: IO error");
        assert!(state.to_string().contains("Websocket: IO error"));
        assert!(state.is_transient());
        assert!(!state.is_connected());
        assert!(!state.is_terminal());
    }

    /// The completeness contract in value terms: two consecutive
    /// retries of the *same* cause must still be different states, so
    /// a consumer rendering state sees progress during a total outage.
    #[test]
    fn consecutive_retries_are_distinct_values() {
        let first = SessionState::Retrying {
            reason: "connection refused".into(),
            attempt: 1,
            retry_in: Duration::from_millis(500),
        };
        let second = SessionState::Retrying {
            reason: "connection refused".into(),
            attempt: 2,
            retry_in: Duration::from_secs(1),
        };
        assert_ne!(first, second);
        assert_eq!(first.attempt(), Some(1));
        assert_eq!(second.attempt(), Some(2));
    }

    #[test]
    fn changed_reason_is_a_distinct_state() {
        let (tx, _rx) = SessionStateTx::new(SessionState::Connecting);
        tx.set(retrying("broker sent DISCONNECT"));
        tx.set(retrying("poll watchdog timeout"));
        assert_eq!(
            tx.current().reason(),
            Some("poll watchdog timeout"),
            "the latest reason must win"
        );
    }

    #[tokio::test]
    async fn watch_channel_delivers_changes() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        assert_eq!(rx.current(), SessionState::Connecting);

        tx.set(SessionState::Connecting);
        tokio::task::yield_now().await;

        let state = rx.changed().await.unwrap();
        assert_eq!(state, SessionState::Connecting);

        tx.set(SessionState::Connected);
        let state = rx.changed().await.unwrap();
        assert_eq!(state, SessionState::Connected);
    }

    /// Regression (I5): `changed()` used to clone the receiver, await
    /// on the clone and throw it away, so the stored receiver never
    /// advanced its seen version. The second call then returned
    /// immediately, for ever — a busy loop for any event-driven
    /// consumer.
    #[tokio::test]
    async fn changed_does_not_replay_an_already_consumed_change() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        tx.set(SessionState::Connecting);
        assert_eq!(rx.changed().await.unwrap(), SessionState::Connecting);

        // Nothing else is published: a second wait must NOT return.
        let second = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        assert!(
            second.is_err(),
            "changed() replayed an already-consumed change"
        );
    }

    #[tokio::test]
    async fn watch_channel_current_is_instant() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connected);
        assert_eq!(tx.current(), SessionState::Connected);
        assert_eq!(rx.current(), SessionState::Connected);
    }

    /// Regression: `set` must update the value even when no receiver
    /// is held (`watch::Sender::send` refuses to write with zero
    /// receivers; `send_modify` does not).
    #[test]
    fn set_works_without_receiver() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        drop(rx);

        tx.set(SessionState::Connected);
        assert_eq!(tx.current(), SessionState::Connected);

        tx.set(retrying("test"));
        assert_eq!(tx.current().attempt(), Some(1));

        tx.set(SessionState::Closed {
            reason: "test".into(),
        });
        assert!(tx.current().is_terminal());
    }

    #[test]
    fn clone_shares_state() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        let tx_clone = tx.clone();
        drop(rx);

        tx_clone.set(SessionState::Connected);
        assert_eq!(tx.current(), SessionState::Connected);

        tx.set(retrying("test"));
        assert_eq!(tx_clone.current().attempt(), Some(1));
    }

    /// A bounded wait is level-triggered: a late subscriber still
    /// matches the state that is already there.
    #[tokio::test]
    async fn wait_for_matches_the_current_state_immediately() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connected);
        let outcome = rx
            .wait_for(SessionState::is_connected, Duration::from_secs(30))
            .await;
        assert_eq!(outcome, StateWait::Matched(SessionState::Connected));
        drop(tx);
    }

    /// Expiry ends the wait, not the resource: it reports the state
    /// observed at expiry so the caller can render progress.
    #[tokio::test]
    async fn wait_for_reports_the_state_on_expiry() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        tx.set(retrying("connection refused"));
        let outcome = rx
            .wait_for(SessionState::is_connected, Duration::from_millis(20))
            .await;
        match outcome {
            StateWait::Elapsed(state) => assert_eq!(state.attempt(), Some(1)),
            other => panic!("expected Elapsed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_for_wakes_on_the_matching_transition() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        let waiter = tokio::spawn(async move {
            rx.wait_for(SessionState::is_connected, Duration::from_secs(5))
                .await
        });
        tokio::task::yield_now().await;
        tx.set(SessionState::Connected);
        assert_eq!(
            waiter.await.unwrap(),
            StateWait::Matched(SessionState::Connected)
        );
    }

    /// A dead event source is its own outcome, never an expiry: the
    /// caller must fail fast instead of waiting out a budget.
    #[tokio::test]
    async fn wait_for_reports_a_dead_source() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        drop(tx);
        let outcome = rx
            .wait_for(SessionState::is_connected, Duration::from_secs(30))
            .await;
        assert_eq!(outcome, StateWait::SourceGone);
    }

    #[tokio::test]
    async fn cancelled_wait_does_not_rewind_the_cursor() {
        let (tx, rx) = SessionStateTx::new(SessionState::Connecting);
        tx.set(retrying("a"));
        let out = rx
            .wait_for(|s| matches!(s, SessionState::Connected), Duration::from_millis(5))
            .await;
        assert!(matches!(out, StateWait::Elapsed(_)));
        let blocked = tokio::time::timeout(Duration::from_millis(50), rx.changed()).await;
        assert!(blocked.is_err(), "cursor rewound: replayed a consumed change");
    }

}