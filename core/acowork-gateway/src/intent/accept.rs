//! Runtime acceptance receipts for delivered Intents.
//!
//! Publishing to MQTT proves the broker took the bytes — nothing more. The
//! receiver then has to *route* the message (resolve the peer, pick or create
//! a session, queue a turn), and that step can fail for reasons the Gateway
//! cannot see. The incident this module exists for: a Runtime whose
//! session-meta store lacked the `intent_key` column answered every inbound
//! Intent with `no such column`, logged a `WARN` and dropped it — while the
//! Gateway recorded `delivered` and the sender, with no error to retry on,
//! stranded the task forever.
//!
//! So the receiving Runtime reports its decision back through
//! `POST /api/intents/{id}/accept` and the sending side can wait for that
//! fact:
//!
//! ```text
//! PM dispatch ──register(waiter)──> publish ──> Runtime routes
//!                                              │
//!                                    accepted ──┴── rejected(reason)
//!                                        │              │
//!                                   waiter wakes   waiter wakes
//! ```
//!
//! Two properties matter:
//!
//! * **No polling.** The waiter is a [`tokio::sync::oneshot`] registered
//!   *before* the publish, so the receipt can never beat the registration,
//!   and the waiting side is woken by the event itself.
//! * **Receipts are advisory.** A Runtime built before this protocol says
//!   nothing, so [`AcceptanceWaiter::wait`] is bounded and reports `None`;
//!   callers must then fall back to the publish fact instead of failing.
//!
//! The registry is process-local on purpose: the receipt arrives on the same
//! Gateway instance that published (the Runtime posts to its own Gateway),
//! so there is nothing to share across processes.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use tokio::sync::oneshot;

/// What the target Runtime decided about an Intent it received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    /// `true` = routed and handed to a session; `false` = refused, see `error`.
    pub accepted: bool,
    /// The refusal reason, verbatim from the receiving Runtime (empty when
    /// accepted). Kept verbatim because it is what the operator needs to
    /// debug: `no such column: intent_key` names the breakage, a coarse
    /// `routing_error` would not.
    pub error: String,
}

impl Acceptance {
    pub fn accepted() -> Self {
        Self { accepted: true, error: String::new() }
    }

    pub fn rejected(error: impl Into<String>) -> Self {
        Self { accepted: false, error: error.into() }
    }

    /// The `accept_status` spelling used on the wire and in the mailbox.
    pub fn status(&self) -> &'static str {
        if self.accepted {
            "accepted"
        } else {
            "rejected"
        }
    }
}

/// Waiters by message id. A `Vec` rather than a single sender because the
/// same Intent can be dispatched again (a retry re-uses the task's message
/// id only if the caller passes it; a fresh attempt gets a fresh id, but a
/// duplicate POST of the same id is legal) — every waiter gets the answer.
type Waiters = HashMap<String, Vec<oneshot::Sender<Acceptance>>>;

static WAITERS: LazyLock<Mutex<Waiters>> = LazyLock::new(Default::default);

/// A registration for one message id, valid from [`register`] until
/// [`wait`] returns or the value is dropped.
///
/// [`register`]: AcceptanceWaiter::register
/// [`wait`]: AcceptanceWaiter::wait
pub struct AcceptanceWaiter {
    message_id: String,
    rx: Option<oneshot::Receiver<Acceptance>>,
}

impl AcceptanceWaiter {
    /// Register interest in the acceptance of `message_id`.
    ///
    /// Call this **before** publishing: the receiving Runtime may answer
    /// within microseconds on a warm process, and a waiter registered after
    /// the publish would miss the receipt and degrade a good answer into a
    /// timeout.
    pub fn register(message_id: &str) -> Self {
        let (tx, rx) = oneshot::channel();
        WAITERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(message_id.to_string())
            .or_default()
            .push(tx);
        Self { message_id: message_id.to_string(), rx: Some(rx) }
    }

    /// Wait for the receipt, at most `timeout`.
    ///
    /// `None` means *no answer*: either the bound elapsed (a wedged or
    /// pre-receipt Runtime) or the sender side was dropped. Callers must
    /// treat that as "unknown", not as a failure.
    pub async fn wait(mut self, timeout: Duration) -> Option<Acceptance> {
        let rx = self.rx.take()?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(acceptance)) => Some(acceptance),
            // Sender dropped without answering: nothing will arrive.
            Ok(Err(_)) => None,
            // Nobody answered in time.
            Err(_) => None,
        }
    }
}

impl Drop for AcceptanceWaiter {
    fn drop(&mut self) {
        // Release our own receiver *before* pruning: field drops run only
        // after this function returns, so while `rx` is still alive our own
        // sender reads as open and `retain` would keep it — leaking one
        // registration per early-exit dispatch (budget check, publish
        // failure) in a keep-alive process.
        drop(self.rx.take());
        let mut waiters = WAITERS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(list) = waiters.get_mut(&self.message_id) else {
            return;
        };
        // A waiter that already received (or abandoned) its receipt leaves a
        // closed sender behind; prune those so a long-lived Gateway does not
        // accumulate registrations for ids nobody will ever ask about again.
        list.retain(|tx| !tx.is_closed());
        if list.is_empty() {
            waiters.remove(&self.message_id);
        }
    }
}

/// Deliver an acceptance to whoever is waiting on `acceptance_id`.
///
/// Returns `true` when at least one waiter consumed it. `false` is normal:
/// receipts arrive for every Intent (the mailbox records them all), and the
/// waiters only exist for dispatches that asked for one.
pub fn notify(message_id: &str, acceptance: Acceptance) -> bool {
    let senders = {
        let mut waiters = WAITERS.lock().unwrap_or_else(|e| e.into_inner());
        match waiters.remove(message_id) {
            Some(list) => list,
            None => return false,
        }
    };
    let mut delivered = false;
    for tx in senders {
        // `send` fails only when that waiter already went away (timeout).
        delivered |= tx.send(acceptance.clone()).is_ok();
    }
    delivered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn waiter_is_woken_by_notify() {
        let waiter = AcceptanceWaiter::register("msg-woken");
        assert!(notify("msg-woken", Acceptance::rejected("no such column: intent_key")));
        let acceptance = waiter.wait(Duration::from_secs(5)).await.expect("receipt");
        assert!(!acceptance.accepted);
        assert_eq!(acceptance.error, "no such column: intent_key");
    }

    #[tokio::test]
    async fn wait_is_bounded_and_reports_no_answer() {
        let waiter = AcceptanceWaiter::register("msg-silent");
        let started = std::time::Instant::now();
        assert_eq!(waiter.wait(Duration::from_millis(50)).await, None);
        assert!(started.elapsed() < Duration::from_secs(2), "wait must be bounded");
    }

    #[tokio::test]
    async fn notify_without_waiter_is_harmless() {
        assert!(!notify("msg-nobody", Acceptance::accepted()));
    }

    #[tokio::test]
    async fn a_settled_waiter_leaves_no_registration_behind() {
        let waiter = AcceptanceWaiter::register("msg-pruned");
        assert!(notify("msg-pruned", Acceptance::accepted()));
        assert_eq!(waiter.wait(Duration::from_secs(5)).await, Some(Acceptance::accepted()));
        // Only our own id: other tests hold registrations in this process-
        // wide static concurrently under the default parallel harness.
        assert!(
            !WAITERS.lock().unwrap().contains_key("msg-pruned"),
            "a settled id must not stay registered"
        );
    }

    #[tokio::test]
    async fn a_waiter_dropped_without_waiting_is_pruned_too() {
        // The early-exit paths in `task-dispatch` (budget check, publish
        // failure) drop the waiter before ever calling `wait` — the
        // registration must still not survive a keep-alive Gateway.
        let waiter = AcceptanceWaiter::register("msg-abandoned");
        drop(waiter);
        assert!(!WAITERS.lock().unwrap().contains_key("msg-abandoned"));
        // And a late receipt for the abandoned id must stay a harmless no-op.
        assert!(!notify("msg-abandoned", Acceptance::accepted()));
    }

    #[tokio::test]
    async fn duplicate_waiters_all_get_the_same_answer() {
        let first = AcceptanceWaiter::register("msg-dup");
        let second = AcceptanceWaiter::register("msg-dup");
        assert!(notify("msg-dup", Acceptance::accepted()));
        assert_eq!(first.wait(Duration::from_secs(5)).await, Some(Acceptance::accepted()));
        assert_eq!(second.wait(Duration::from_secs(5)).await, Some(Acceptance::accepted()));
    }

    #[test]
    fn status_spellings_are_stable() {
        assert_eq!(Acceptance::accepted().status(), "accepted");
        assert_eq!(Acceptance::rejected("x").status(), "rejected");
    }
}