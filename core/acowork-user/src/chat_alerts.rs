//! Live (never-persisted) delivery alerts for Chat rows (ADR-105 D5, revision
//! 2026-11-06).
//!
//! A send that failed is a fact about **now**, not about the conversation. The
//! human needs to see "this did not go out, retry or @ the agent" while it still
//! matters; nobody needs to read it again next week, and a transcript that keeps
//! it is a transcript that narrates a transient channel state as if it were
//! history. So it is not a row:
//!
//! - it lives in this process's memory, keyed by the Chat and the human row it
//!   concerns;
//! - it is served as a decoration of that row (`chat_view::DeliveryView::alert`),
//!   so the read path the surfaces already use is the only reader;
//! - it expires on its own ([`ALERT_TTL_SECS`]), and a restart of this service
//!   takes every one of them with it.
//!
//! What it replaces: a persisted `system_notice` row with
//! `notice_type = "delivery_failed"`, written by the Gateway at three delivery
//! call sites and by the sweeper's timeout pass. Those rows were never retracted
//! — a failure that later succeeded still sat in the log forever — and they were
//! also *load bearing* (`chat_kinds::is_answer_row`), so the row settled the
//! sender's wait line. Both jobs move here: the alert settles the line through
//! `delivery.alert`, and it disappears when it stops being true.
//!
//! Deliberately not in this store: what went wrong in prose. `error_code` is the
//! machine token; the wording belongs to the surfaces (they already own the
//! copy), so this stays a fact, not a sentence.

use std::collections::HashMap;
use std::sync::Mutex;

/// How long an alert stays readable. Long enough for a human who sent the
/// message, switched tabs, and looked back; short enough that a stale failure
/// can never be mistaken for the current state of the chat. The surfaces also
/// time it out locally from `expires_in`, so an idle window drops it without a
/// re-read.
pub const ALERT_TTL_SECS: u64 = 600;

/// One failed delivery, live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatAlert {
    /// The Chat this failure belongs to. Part of the key, never of the payload
    /// alone: the same human row id may be addressed from one chat only, but the
    /// map is per chat so a read can take one chat's slice without a scan.
    pub session_id: String,
    /// The human message row the failure concerns — `delivery.alert` is attached
    /// to that row.
    pub in_reply_to: String,
    /// Who did not receive it. Kept apart from the row so a fan-out that fails
    /// for one target and lands for another can say which.
    pub target: String,
    /// Machine token, mirroring the delivery result's `error_code`.
    pub error_code: String,
    /// Unix seconds at which [`ALERT_TTL_SECS`] began.
    pub created: u64,
}

impl ChatAlert {
    fn expired(&self, now: u64) -> bool {
        now.saturating_sub(self.created) >= ALERT_TTL_SECS
    }

    /// What a surface needs to render this and know when to stop: the machine
    /// token, who it concerns, and the remaining life in seconds.
    pub fn view(&self, now: u64) -> AlertView {
        AlertView {
            target: self.target.clone(),
            error_code: self.error_code.clone(),
            expires_in: ALERT_TTL_SECS.saturating_sub(now.saturating_sub(self.created)),
        }
    }
}

/// The wire half of an alert (`chat_view::DeliveryView::alert`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AlertView {
    pub target: String,
    pub error_code: String,
    /// Seconds until this alert stops being served. A client schedules its own
    /// removal from this instead of comparing clocks with the service.
    pub expires_in: u64,
}

/// In-memory alert table. One mutex, no awaking, no persistence: every operation
/// is a map touch, and losing the lot on restart is the point.
#[derive(Debug, Default)]
pub struct AlertStore {
    inner: Mutex<HashMap<String, Vec<ChatAlert>>>,
}

impl AlertStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one failure, replacing an equal one.
    ///
    /// "Equal" is `(session, in_reply_to, target)` without the code: a retry of
    /// the same send that fails the same way must not stack a second copy, and a
    /// retry that fails *differently* (the agent got uninstalled in between) must
    /// replace what the human is being told rather than accumulate. This is also
    /// what makes the sweeper's retried tick idempotent.
    pub fn record(&self, alert: ChatAlert) {
        let mut map = match self.inner.lock() {
            Ok(m) => m,
            // A poisoned lock means another thread panicked mid-update. The
            // table is a decoration: take the data and carry on rather than
            // failing a delivery path that is already reporting a failure.
            Err(poisoned) => poisoned.into_inner(),
        };
        let slot = map.entry(alert.session_id.clone()).or_default();
        let key = |a: &ChatAlert| (a.in_reply_to.clone(), a.target.clone());
        match slot.iter_mut().find(|a| key(a) == key(&alert)) {
            Some(existing) => *existing = alert,
            None => slot.push(alert),
        }
    }

    /// The live alerts of `ids` in this chat, by `in_reply_to`, dropping anything
    /// that has expired.
    ///
    /// `ids` is the loaded window's own row ids, so this never builds a chat-wide
    /// map for a page of fifty rows. Expiry is applied here, on the read, because
    /// this is the only operation that has to be honest about age.
    ///
    /// When one row carries failures for several targets (a group `@` fan-out
    /// where two agents are both uninstalled) the map keeps the **last recorded**
    /// one, because the wire shape is a single `delivery.alert`: a row wears one
    /// short line, and the newest reason is the one worth telling. The store
    /// keeps every target (`len` counts them all), so nothing is silently lost —
    /// only the row's one-line rendering is.
    pub fn live(&self, session_id: &str, ids: &[String], now: u64) -> HashMap<String, AlertView> {
        let mut map = match self.inner.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(slot) = map.get_mut(session_id) else {
            return HashMap::new();
        };
        slot.retain(|a| !a.expired(now));
        if slot.is_empty() {
            map.remove(session_id);
            return HashMap::new();
        }
        let wanted: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        slot.iter()
            .filter(|a| wanted.contains(a.in_reply_to.as_str()))
            .map(|a| (a.in_reply_to.clone(), a.view(now)))
            .collect()
    }

    /// Live alert count of one chat — diagnostics and tests only.
    pub fn len(&self, session_id: &str, now: u64) -> usize {
        let mut map = match self.inner.lock() {
            Ok(m) => m,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Some(slot) = map.get_mut(session_id) else {
            return 0;
        };
        slot.retain(|a| !a.expired(now));
        if slot.is_empty() {
            map.remove(session_id);
            return 0;
        }
        slot.len()
    }

    pub fn is_empty(&self, session_id: &str, now: u64) -> bool {
        self.len(session_id, now) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(session: &str, row: &str, target: &str, code: &str) -> ChatAlert {
        ChatAlert {
            session_id: session.to_string(),
            in_reply_to: row.to_string(),
            target: target.to_string(),
            error_code: code.to_string(),
            created: 1_000,
        }
    }

    #[test]
    fn an_alert_is_served_on_the_row_it_concerns() {
        let store = AlertStore::new();
        store.record(alert("c1", "m1", "agent-a", "target_not_installed"));
        let live = store.live("c1", &["m1".to_string(), "m2".to_string()], 1_010);
        assert_eq!(live.len(), 1);
        let got = live.get("m1").expect("the row the send owed");
        assert_eq!(got.target, "agent-a");
        assert_eq!(got.error_code, "target_not_installed");
        assert_eq!(got.expires_in, ALERT_TTL_SECS - 10);
        // A row id outside the loaded window is never served.
        assert!(store.live("c1", &["m2".to_string()], 1_010).is_empty());
    }

    #[test]
    fn it_expires_on_its_own_and_leaves_nothing_behind() {
        let store = AlertStore::new();
        store.record(alert("c1", "m1", "agent-a", "not_authorized"));
        assert!(store.live("c1", &["m1".to_string()], 1_000 + ALERT_TTL_SECS - 1).contains_key("m1"));
        assert!(
            store.live("c1", &["m1".to_string()], 1_000 + ALERT_TTL_SECS).is_empty(),
            "an expired failure is not history, it is noise"
        );
        // The chat's slot is dropped with its last alert, not left as an empty vec.
        assert!(store.is_empty("c1", 1_000 + ALERT_TTL_SECS));
    }

    #[test]
    fn a_retried_send_replaces_its_alert_instead_of_stacking() {
        let store = AlertStore::new();
        store.record(alert("c1", "m1", "agent-a", "target_not_installed"));
        store.record(alert("c1", "m1", "agent-a", "not_authorized"));
        let live = store.live("c1", &["m1".to_string()], 1_000);
        assert_eq!(live.len(), 1);
        assert_eq!(live["m1"].error_code, "not_authorized", "the newest reason");
        assert_eq!(store.len("c1", 1_000), 1);

        // A fan-out that fails for two targets says both: they are two facts.
        store.record(alert("c1", "m1", "agent-b", "target_not_installed"));
        assert_eq!(store.len("c1", 1_000), 2);
    }

    #[test]
    fn chats_do_not_see_each_others_alerts() {
        let store = AlertStore::new();
        store.record(alert("c1", "m1", "agent-a", "not_authorized"));
        assert!(store
            .live("c2", &["m1".to_string()], 1_000)
            .is_empty());
    }
}
