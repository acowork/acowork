//! Intent mailbox — the minimal delivery buffer (tech-debt plan D3).
//!
//! A delivery buffer, **not** a workflow engine (plan §8 decision 3): it
//! records the facts the sender and the chat model need —
//! `message_id / conversation_id / from / to / body / created_at /
//! delivered_at / result_ref` — and the per-`conversation_id` request
//! count. Nothing else lives here; no state machine, no retries, no
//! orchestration.
//!
//! Persistence is a JSON file in the Gateway data dir (same shape as
//! [`crate::cron::store::CronStore`], atomic rewrite on every mutation).
//! Two facts make persistence mandatory rather than optional:
//!
//! - `intent_send` must report a truthful delivery state, and
//! - design doc 27 §6-13: the `conversation_id` request count must
//!   survive a Gateway restart, otherwise the conversation budget
//!   (§6.5 R4) can be bypassed by restarting the Gateway.
//!
//! ponytail: the entry list is capped (FIFO drop of the oldest entries);
//! counts are never dropped. If a deployment needs the full message
//! history, that belongs in the `acowork-user` chat store (design doc
//! 27 Phase 1), not here.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Maximum number of mailbox entries retained (delivery facts, not
/// history). Counts live in a separate map and are never evicted.
const MAX_ENTRIES: usize = 10_000;

/// One delivered (or attempted) Intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailboxEntry {
    /// Stable message identity (design doc 27 §4.5 `message_id`).
    pub message_id: String,
    /// Conversation this request belongs to (§6.5). Empty string for
    /// pre-conversation senders (cron triggers).
    #[serde(default)]
    pub conversation_id: String,
    /// Sender instance id, Gateway-stamped (§D4). Cron uses `cron:<id>`.
    pub from_instance: String,
    /// Sender execution session id when the sender is an Agent
    /// (`source_agent_session_id`); empty for user/cron originators.
    #[serde(default)]
    pub from_session_id: String,
    /// Target instance id.
    pub to_instance: String,
    /// The message body handed to the target (action + params JSON).
    pub body: String,
    /// RFC 3339 creation timestamp.
    pub created_at: String,
    /// RFC 3339 timestamp of the successful MQTT publish; empty until
    /// delivered. An entry with `delivered_at` set is the "已投递" fact.
    #[serde(default)]
    pub delivered_at: String,
    /// Response association (design doc 27 §4.5 `in_reply_to`): for a
    /// response entry, the request `message_id` it answers. Empty on
    /// requests until their response is routed back.
    #[serde(default)]
    pub result_ref: String,
    /// "request" (default) | "response" (§6.5 R2).
    #[serde(default)]
    pub kind: String,
    /// For a response: §7.1 outcome (completed/failed/aborted/timeout).
    #[serde(default)]
    pub status: String,
    /// Source Chat session this message belongs to (§4
    /// `source_chat_session_id`); the reply address. Empty for
    /// chat-less sends (cron, plain agent-to-agent before pairing).
    #[serde(default)]
    pub source_chat_session_id: String,
    /// RFC 3339 stamp set when this **request** got its answer — either a
    /// real `response` write-back or the §7.1 crash-fallback receipt.
    /// Empty = still open; the sweeper (§13-12) acts on open requests
    /// past the response deadline.
    #[serde(default)]
    pub answered_at: String,
    /// RFC 3339 stamp of the last time the Gateway saw this request's
    /// responder *do something* on the Chat surface — a routing pointer, a
    /// decision card, any Runtime-owned row it appended to the request's
    /// source Chat session. Empty until the first such event.
    ///
    /// This is the clock the response deadline is measured against (see
    /// [`MailboxEntry::deadline_clock`]). `delivered_at` alone cannot
    /// distinguish a crashed responder from a healthy one mid-turn: a
    /// write-back is only produced at turn end, so any legitimately long
    /// turn — one blocked on approval cards, one running a long tool chain —
    /// crosses a wall-clock deadline while its responder is demonstrably
    /// alive. Design 27 §5.5.1 scopes the fallback receipt to
    /// "crashed / no event at all", and "no event" is exactly what this
    /// stamp measures.
    #[serde(default)]
    pub last_activity_at: String,
    /// Design 27 §5.5.4: whether the source Chat session has a human who
    /// can answer a card — carried onto the response push so the
    /// initiator's turn sees the same answerability fact.
    #[serde(default)]
    pub has_human: Option<bool>,
}

impl MailboxEntry {
    pub fn delivered(&self) -> bool {
        !self.delivered_at.is_empty()
    }

    /// A request is closed once an answer (real or fallback) was emitted.
    pub fn answered(&self) -> bool {
        !self.answered_at.is_empty()
    }

    /// The timestamp the response deadline is measured against: the later
    /// of delivery and the last responder activity the Gateway observed.
    ///
    /// RFC 3339 stamps from one UTC clock sort lexicographically, so `max`
    /// is a plain string compare — the same trick `stale_unanswered` uses.
    /// An entry with no recorded activity falls back to `delivered_at`, so
    /// a responder that never touched the Chat surface keeps the pre-
    /// activity-clock behaviour.
    pub fn deadline_clock(&self) -> &str {
        if self.last_activity_at.as_str() > self.delivered_at.as_str() {
            &self.last_activity_at
        } else {
            &self.delivered_at
        }
    }
}

/// Persisted mailbox state.
#[derive(Debug, Default, Serialize, Deserialize)]
struct MailboxData {
    entries: Vec<MailboxEntry>,
    /// `conversation_id` → accepted request count. Survives restart by
    /// construction (§13-13); never evicted with the entries.
    conversation_requests: HashMap<String, u32>,
}

/// Thread-safe persisted Intent mailbox.
#[derive(Debug)]
pub struct IntentMailbox {
    inner: Mutex<MailboxData>,
    path: Option<PathBuf>,
}

/// Error type mirroring the cron store's shape.
#[derive(Debug, thiserror::Error)]
pub enum MailboxError {
    #[error("io: {0}")]
    Io(String),
    #[error("serialization: {0}")]
    Serialize(String),
}

impl IntentMailbox {
    /// Open (or create) the mailbox at `{data_dir}/intent_mailbox.json`.
    pub fn open(path: &Path) -> Self {
        let data = if path.exists() {
            std::fs::read_to_string(path)
                .map(|s| serde_json::from_str(&s).unwrap_or_default())
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "intent mailbox unreadable — starting fresh");
                    MailboxData::default()
                })
        } else {
            MailboxData::default()
        };
        Self {
            inner: Mutex::new(data),
            path: Some(path.to_path_buf()),
        }
    }

    /// In-memory mailbox (tests, or no data dir configured).
    pub fn in_memory() -> Self {
        Self {
            inner: Mutex::new(MailboxData::default()),
            path: None,
        }
    }

    /// Record a new Intent (status: not yet delivered) and count it
    /// against its conversation. Returns the entry snapshot.
    ///
    /// §6.5 R4: only `request` messages count against the conversation
    /// limit — a response closes a round, it does not extend it. An
    /// empty `kind` reads as request (cron / pre-Phase-2 producers).
    ///
    /// Idempotent on `message_id`: a retry of the same dispatch (the PM
    /// scanner persists its dispatch identity and reuses it across
    /// attempts) refreshes the existing entry instead of adding a second
    /// one, and the conversation budget counts *generations*, not
    /// attempts — otherwise a delivery-failure retry loop would starve
    /// its own conversation (§6.5 R4) and the chat log would fill with
    /// duplicate rows. Delivery facts are monotonic: a recorded
    /// `delivered_at` / `answered_at` is never cleared by a later record
    /// of the same id.
    pub fn record(&self, entry: MailboxEntry) -> Result<MailboxEntry, MailboxError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner
            .entries
            .iter_mut()
            .find(|e| e.message_id == entry.message_id)
        {
            let mut merged = entry.clone();
            if merged.delivered_at.is_empty() {
                merged.delivered_at = existing.delivered_at.clone();
            }
            if merged.answered_at.is_empty() {
                merged.answered_at = existing.answered_at.clone();
            }
            // Activity is monotonic — and deliberately not the "fill in when
            // empty" shape used above: a re-record of the same dispatch (the
            // PM scanner reuses its identity across attempts) must never
            // *rewind* the deadline clock, which would re-arm a request whose
            // responder has since gone quiet. Keep the later of the two.
            if existing.last_activity_at.as_str() > merged.last_activity_at.as_str() {
                merged.last_activity_at = existing.last_activity_at.clone();
            }
            *existing = merged;
            self.save_locked(&inner)?;
            return Ok(entry);
        }
        if !entry.conversation_id.is_empty() && entry.kind != "response" {
            *inner
                .conversation_requests
                .entry(entry.conversation_id.clone())
                .or_insert(0) += 1;
        }
        inner.entries.push(entry.clone());
        if inner.entries.len() > MAX_ENTRIES {
            // Known ceiling (plan §10-6): an evicted request entry makes a
            // very late response unmatchable ("no recorded request"). The
            // budget map above is never evicted, so the §6.5 R4 cap holds
            // regardless. Upgrade path: per-conversation pruning.
            let excess = inner.entries.len() - MAX_ENTRIES;
            inner.entries.drain(..excess);
        }
        self.save_locked(&inner)?;
        Ok(entry)
    }

    /// Stamp the delivery fact after a successful publish.
    pub fn mark_delivered(&self, message_id: &str) -> Result<(), MailboxError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(e) = inner.entries.iter_mut().find(|e| e.message_id == message_id) {
            e.delivered_at = chrono::Utc::now().to_rfc3339();
            self.save_locked(&inner)?;
        }
        Ok(())
    }

    /// Close a request: its answer (real response or §7.1 fallback receipt)
    /// has been emitted. Idempotent — a second mark is a no-op refresh.
    pub fn mark_answered(&self, message_id: &str) -> Result<(), MailboxError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(e) = inner.entries.iter_mut().find(|e| e.message_id == message_id) {
            e.answered_at = chrono::Utc::now().to_rfc3339();
            self.save_locked(&inner)?;
        }
        Ok(())
    }

    /// Requests whose deadline clock is older than `older_than` (RFC3339)
    /// and that still have no answer — the §7.1 crash-fallback candidates.
    /// Response entries and undelivered attempts are never returned: an
    /// undelivered request failed synchronously and its sender already has
    /// the error.
    ///
    /// The clock is `MailboxEntry::deadline_clock` — delivery refreshed by
    /// observed responder activity — so a long turn that is still producing
    /// Chat-surface events is not mistaken for a crashed one.
    ///
    /// The sweeper passes the more permissive of its two windows (response
    /// deadline, responder-liveness grace) and applies the exact cutoff per
    /// entry, so the parameter is deliberately not named after either
    /// meaning.
    pub fn stale_unanswered(&self, older_than: &str) -> Vec<MailboxEntry> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .iter()
            .filter(|e| {
                e.kind != "response"
                    && e.delivered()
                    && !e.answered()
                    && !e.delivered_at.is_empty()
                    && e.deadline_clock() < older_than
            })
            .cloned()
            .collect()
    }

    /// Stamp "the responder is alive and working on this Chat session" and
    /// return how many open requests it re-armed.
    ///
    /// Called on every Runtime-owned row the Gateway relays into a Chat
    /// session: a routing pointer, a decision card, an echo. Each is proof
    /// the responder's process is up and its turn has not been lost, which
    /// is the fact the sweeper needs before it declares a crash (§7.1).
    ///
    /// Matched on `(responder, chat session)` rather than on the row's
    /// `in_reply_to`, because only some row kinds carry the coordinate —
    /// a decision card is frequently unthreaded, and a turn waiting on that
    /// card is precisely the case this must not misfire on. A responder with
    /// several queued requests in one Chat session re-arms all of them: the
    /// Gateway cannot tell which one the current turn is serving, and
    /// guessing would re-introduce the false receipt this exists to remove.
    pub fn touch_activity(&self, responder: &str, chat_session_id: &str) -> usize {
        if chat_session_id.is_empty() {
            return 0;
        }
        let mut inner = self.inner.lock().unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut touched = 0usize;
        for e in inner.entries.iter_mut() {
            if e.kind == "response" || !e.delivered() || e.answered() {
                continue;
            }
            if e.to_instance != responder || e.source_chat_session_id != chat_session_id {
                continue;
            }
            // Only ever move forward — a stale relay retry must not rewind.
            if e.last_activity_at.as_str() < now.as_str() {
                e.last_activity_at = now.clone();
                touched += 1;
            }
        }
        if touched > 0
            && let Err(err) = self.save_locked(&inner)
        {
            tracing::warn!(error = %err, "mailbox activity stamp not persisted");
        }
        touched
    }

    /// Look up one delivery fact by `message_id`.
    pub fn get(&self, message_id: &str) -> Option<MailboxEntry> {
        let inner = self.inner.lock().unwrap();
        inner.entries.iter().find(|e| e.message_id == message_id).cloned()
    }

    /// Requests already accepted for a conversation (§6.5 R4 gate).
    /// A never-seen conversation returns 0.
    pub fn conversation_request_count(&self, conversation_id: &str) -> u32 {
        let inner = self.inner.lock().unwrap();
        inner
            .conversation_requests
            .get(conversation_id)
            .copied()
            .unwrap_or(0)
    }

    fn save_locked(&self, data: &MailboxData) -> Result<(), MailboxError> {
        let Some(path) = &self.path else { return Ok(()); };
        let json = serde_json::to_string(data).map_err(|e| MailboxError::Serialize(e.to_string()))?;
        // Atomic rewrite: temp file + rename, same contract as the cron
        // store — a crash mid-write never truncates the mailbox.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| MailboxError::Io(e.to_string()))?;
        std::fs::rename(&tmp, path).map_err(|e| MailboxError::Io(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn entry(id: &str, conv: &str) -> MailboxEntry {
        MailboxEntry {
            message_id: id.to_string(),
            conversation_id: conv.to_string(),
            from_instance: "a".into(),
            from_session_id: String::new(),
            to_instance: "b".into(),
            body: "{}".into(),
            created_at: chrono::Utc::now().to_rfc3339(),
            delivered_at: String::new(),
            result_ref: String::new(),
            kind: "request".into(),
            status: String::new(),
            source_chat_session_id: String::new(),
            answered_at: String::new(),
            last_activity_at: String::new(),
            has_human: None,
        }
    }

    #[test]
    fn record_then_mark_delivered() {
        let mb = IntentMailbox::in_memory();
        mb.record(entry("m1", "c1")).unwrap();
        assert!(!mb.get("m1").unwrap().delivered());
        mb.mark_delivered("m1").unwrap();
        assert!(mb.get("m1").unwrap().delivered());
    }

    #[test]
    fn stale_unanswered_selects_only_open_delivered_requests() {
        let mb = IntentMailbox::in_memory();
        // Same UTC offset + fixed date-time prefix ⇒ RFC3339 strings
        // compare lexicographically in chronological order.
        let cutoff = chrono::Utc::now().to_rfc3339();
        let past = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let mut old = entry("m-old", "c1");
        old.delivered_at = past.clone();
        let mut fresh = entry("m-fresh", "c1");
        fresh.delivered_at = "2999-01-01T00:00:00+00:00".into();
        let undelivered = entry("m-undel", "c1");
        let mut resp = entry("m-resp", "c1");
        resp.kind = "response".into();
        resp.delivered_at = past.clone();
        // Plan 2a.4: a human 1:1 message rides the mailbox as
        // `kind = "user_message"` — it owes an answer too, so the
        // sweeper must consider it (design 27 §5.5.1: no silence).
        let mut human = entry("m-human", "");
        human.kind = "user_message".into();
        human.delivered_at = past.clone();
        human.source_chat_session_id = "cs-1".into();
        for e in [old, fresh, undelivered, resp, human] {
            mb.record(e).unwrap();
        }
        let stale = mb.stale_unanswered(&cutoff);
        assert_eq!(stale.len(), 2, "the old request and the old human message");
        assert!(stale.iter().any(|e| e.message_id == "m-old"));
        assert!(stale.iter().any(|e| e.message_id == "m-human"));
        // Closing them removes them from the candidate set.
        mb.mark_answered("m-old").unwrap();
        mb.mark_answered("m-human").unwrap();
        assert!(mb.stale_unanswered(&cutoff).is_empty());
    }

    /// Design 27 §5.5.1 scopes the fallback receipt to "Runtime 崩溃 /
    /// 无任何事件" — crashed, or **no event at all**. Measuring the deadline
    /// from `delivered_at` alone broke that scope: a write-back is only
    /// produced at turn *end*, so any legitimately long turn — one blocked
    /// on approval cards, one running a long tool chain — crossed the
    /// deadline while its responder was alive and working, and the Chat
    /// surface got a red `failed` receipt beside a session that was still
    /// running and had never errored.
    #[test]
    fn activity_rearm_keeps_a_live_long_turn_out_of_the_fallback_window() {
        let mb = IntentMailbox::in_memory();
        let deadline_cutoff = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();

        let mut req = entry("m-long", "");
        req.kind = "user_message".into();
        req.source_chat_session_id = "cs-1".into();
        // Delivered two hours ago — well past the response deadline...
        req.delivered_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        // ...but the responder wrote a row through the chat-append relay a
        // minute ago (a routing pointer, an approval card). That is an
        // event, so "no event at all" is false and the request is not a
        // fallback candidate.
        req.last_activity_at = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        mb.record(req).unwrap();

        assert!(
            mb.stale_unanswered(&deadline_cutoff).is_empty(),
            "a turn that is still producing events must not be declared crashed"
        );
    }

    /// The other half of the fix: re-arming must not turn into the
    /// permanent silence §3.3 forbids. A responder that produced nothing
    /// after delivery is still swept on the original clock.
    #[test]
    fn a_request_with_no_activity_is_still_swept() {
        let mb = IntentMailbox::in_memory();
        let deadline_cutoff = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();

        let mut req = entry("m-silent", "");
        req.kind = "user_message".into();
        req.source_chat_session_id = "cs-1".into();
        req.delivered_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        mb.record(req).unwrap();

        let stale = mb.stale_unanswered(&deadline_cutoff);
        assert_eq!(
            stale.len(),
            1,
            "no activity recorded must fall back to the delivery clock"
        );
        assert_eq!(stale[0].message_id, "m-silent");
    }

    /// `touch_activity` is scoped to (responder, chat session) over open
    /// requests only. Re-arming someone else's request, or a request in
    /// another conversation, would let any chatty agent mask a real crash.
    #[test]
    fn touch_activity_rearms_only_that_responder_s_open_requests_in_that_chat() {
        let mb = IntentMailbox::in_memory();
        let past = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();

        let mut mine = entry("m-mine", "");
        mine.to_instance = "b".into();
        mine.source_chat_session_id = "cs-1".into();
        mine.delivered_at = past.clone();

        // Same responder, different Chat session.
        let mut other_chat = entry("m-other-chat", "");
        other_chat.to_instance = "b".into();
        other_chat.source_chat_session_id = "cs-2".into();
        other_chat.delivered_at = past.clone();

        // Same Chat session, different responder.
        let mut other_responder = entry("m-other-responder", "");
        other_responder.to_instance = "z".into();
        other_responder.source_chat_session_id = "cs-1".into();
        other_responder.delivered_at = past.clone();

        // Already answered — nothing to re-arm.
        let mut closed = entry("m-closed", "");
        closed.to_instance = "b".into();
        closed.source_chat_session_id = "cs-1".into();
        closed.delivered_at = past.clone();
        closed.answered_at = past.clone();

        for e in [mine, other_chat, other_responder, closed] {
            mb.record(e).unwrap();
        }

        let touched = mb.touch_activity("b", "cs-1");
        assert_eq!(touched, 1, "only this responder's open request here");
        assert!(
            !mb.get("m-mine").unwrap().last_activity_at.is_empty(),
            "the matched request is stamped"
        );
        for id in ["m-other-chat", "m-other-responder", "m-closed"] {
            assert!(
                mb.get(id).unwrap().last_activity_at.is_empty(),
                "{id} must not be re-armed"
            );
        }
        // A chat-less send (cron, plain agent-to-agent) has no surface to
        // observe activity on, so it must never be swept up by a stamp.
        assert_eq!(mb.touch_activity("b", ""), 0);
    }

    /// A relay retry must not rewind the clock, and a re-`record` of the
    /// same dispatch (the PM scanner reuses its identity across attempts)
    /// must not clear it — otherwise a retry loop could reset the deadline
    /// it is supposed to be measured against.
    #[test]
    fn activity_clock_never_rewinds_on_retry_or_re_record() {
        let mb = IntentMailbox::in_memory();
        let mut req = entry("m-retry", "");
        req.kind = "user_message".into();
        req.source_chat_session_id = "cs-1".into();
        req.delivered_at = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        mb.record(req.clone()).unwrap();

        mb.touch_activity("b", "cs-1");
        let stamped = mb.get("m-retry").unwrap().last_activity_at.clone();
        assert!(!stamped.is_empty(), "the touch stamped the clock");

        // A re-record carrying an *older* activity stamp keeps the newer one.
        let mut stale_retry = req;
        stale_retry.delivered_at = stamped.clone();
        stale_retry.last_activity_at =
            (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        assert!(stale_retry.last_activity_at < stamped, "test premise: older");
        mb.record(stale_retry).unwrap();
        assert_eq!(
            mb.get("m-retry").unwrap().last_activity_at,
            stamped,
            "a retry with a stale stamp must not rewind the deadline clock"
        );

        // A re-record carrying no stamp keeps it too.
        let mut bare = entry("m-retry", "");
        bare.kind = "user_message".into();
        bare.source_chat_session_id = "cs-1".into();
        bare.delivered_at = stamped.clone();
        mb.record(bare).unwrap();
        assert_eq!(mb.get("m-retry").unwrap().last_activity_at, stamped);
    }

    /// Replay of the incident this fix was made for, on the recorded
    /// timestamps (chat session `44114713`, request `ab6ff18e`, responder
    /// = the PM agent, `response_timeout_secs` = the 3600 s default).
    ///
    /// The human's message was delivered at 12:03:29Z. The turn was still
    /// running — it raised an approval card at 12:56:17Z, that card timed
    /// out at 13:01:17Z, and the session went on to finish at 13:30:07Z
    /// without ever reporting an error. The sweeper ticked at 13:04:07Z,
    /// 38 s past the wall-clock deadline, and wrote a `failed` receipt
    /// claiming the execution "may have crashed or been lost" beside a
    /// live, working session.
    #[test]
    fn replay_incident_ab6ff18e_is_not_declared_crashed() {
        let mb = IntentMailbox::in_memory();
        let zone = chrono::FixedOffset::east_opt(0).unwrap();
        let stamp = |h: u32, m: u32, s: u32| -> String {
            zone.with_ymd_and_hms(2026, 10, 31, h, m, s)
                .unwrap()
                .to_rfc3339()
        };
        let cutoff = |at: &str, secs: i64| {
            chrono::DateTime::parse_from_rfc3339(at)
                .unwrap()
                .with_timezone(&zone)
                - chrono::Duration::seconds(secs)
        };

        let mut req = entry("ab6ff18e", "");
        req.kind = "user_message".into();
        req.source_chat_session_id = "44114713".into();
        req.delivered_at = stamp(12, 3, 29);
        mb.record(req).unwrap();

        // The Gateway relayed the responder's rows: the routing pointer at
        // 12:03:31, the approval card at 12:56:17, its timeout at 13:01:17.
        // Each is a `touch_activity` on (responder, chat session); replay
        // them as direct stamps so the assertion runs on the recorded
        // clock rather than on the test runner's wall clock.
        for at in [stamp(12, 3, 31), stamp(12, 56, 17), stamp(13, 1, 17)] {
            let mut armed = mb.get("ab6ff18e").unwrap();
            armed.last_activity_at = at;
            mb.record(armed).unwrap();
        }
        // The clock now reads 13:01:17Z, not 12:03:29Z.
        assert_eq!(
            mb.get("ab6ff18e").unwrap().deadline_clock(),
            stamp(13, 1, 17).as_str()
        );

        // The tick that fired in production: 13:04:07Z, deadline 3600 s —
        // i.e. the cutoff the sweeper compared `delivered_at` against.
        let deadline_cutoff = cutoff(&stamp(13, 4, 7), 3600).to_rfc3339();
        assert!(
            mb.stale_unanswered(&deadline_cutoff).is_empty(),
            "the live turn must not have been swept at 13:04:07Z"
        );
        // It WAS swept before this fix: the old clock was `delivered_at`
        // (12:03:29Z), which is older than this cutoff.
        assert!(
            stamp(12, 3, 29).as_str() < deadline_cutoff.as_str(),
            "test premise: the delivery-time clock would have fired"
        );

        // And re-arming must not become permanent silence (§3.3): once 3600 s
        // of genuine silence pass after the last event at 13:01:17Z, the
        // request is a candidate again.
        let after_silence = cutoff(&stamp(13, 1, 17), -3601).to_rfc3339();
        assert_eq!(
            mb.stale_unanswered(&after_silence).len(),
            1,
            "silence still ends the request"
        );
    }

    #[test]
    fn conversation_count_survives_reload() {
        let dir = std::env::temp_dir().join(format!("acowork-mailbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("intent_mailbox.json");
        {
            let mb = IntentMailbox::open(&path);
            mb.record(entry("m1", "c1")).unwrap();
            mb.record(entry("m2", "c1")).unwrap();
            mb.record(entry("m3", "c2")).unwrap();
        }
        let mb = IntentMailbox::open(&path);
        assert_eq!(mb.conversation_request_count("c1"), 2, "§13-13: count must survive restart");
        assert_eq!(mb.conversation_request_count("c2"), 1);
        assert_eq!(mb.conversation_request_count("nope"), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_is_idempotent_on_message_id() {
        // The PM scanner persists its dispatch identity and retries with
        // the same message_id. Without dedup every failed attempt would
        // burn a §6.5 R4 budget slot (the task starves permanently once
        // the conversation hits the cap) and pile duplicate rows into the
        // chat log. One generation = one entry = one slot, across reloads.
        let dir = std::env::temp_dir().join(format!("acowork-mailbox-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("intent_mailbox.json");
        {
            let mb = IntentMailbox::open(&path);
            mb.record(entry("m1", "c1")).unwrap();
            mb.mark_delivered("m1").unwrap();
            // Retry of the same generation: refreshes, never re-counts.
            mb.record(entry("m1", "c1")).unwrap();
            assert_eq!(mb.conversation_request_count("c1"), 1, "a retry must not re-count");
            assert!(
                mb.get("m1").unwrap().delivered(),
                "a re-record must not undo the delivered fact (monotonic)"
            );
            // A new generation on the same conversation still counts.
            mb.record(entry("m2", "c1")).unwrap();
            assert_eq!(mb.conversation_request_count("c1"), 2);
        }
        let mb = IntentMailbox::open(&path);
        assert_eq!(mb.conversation_request_count("c1"), 2, "dedup count survives reload");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
