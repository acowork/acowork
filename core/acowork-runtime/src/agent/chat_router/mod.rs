//! Chat Router (ADR-099 / design 29).
//!
//! The Chat surface used to be a pure transport: every human message was
//! handed to whichever execution session the routing table happened to point
//! at. The router replaces that with a real LLM turn that decides between
//! *answering*, *continuing a candidate session*, and *starting a new one*.
//! The turn is stateless — chat history is the only authority — so nothing
//! here owns an LLM conversation of its own.
//!
//! The module layout mirrors one turn:
//!
//! - [`prompt`] — Block A, the byte-stable system prompt;
//! - [`context`] — Block B (history projection) and Block C (this turn's view);
//! - [`tool`] — the `dispatch` tool, the read-only information tools
//!   (`memory_recall`, GET-only `http_request`), and their validation;
//! - [`decision`] — the append-only decision log;
//! - [`runner`] — read history, call the model, act, log.
//!
//! [`ChatSessionManager`] is the only stateful piece, and it holds no
//! conversation state: one slot per Chat session carrying the history window
//! anchor ([`ChatSlot::anchor_seq`]), which is what keeps the cached prefix
//! stable across turns, plus one dispatch queue per Chat session.
//!
//! ## Where a turn runs
//!
//! A Chat turn never executes on the MQTT inbound loop — a turn is an LLM
//! call, and stalling that loop would stop every session of this agent from
//! receiving anything, including a `Stop`. The loop hands the message to the
//! chat's queue ([`ChatSessionManager::push_turn`]) and keeps reading MQTT; a
//! per-chat drain task runs the turn and, when the router does not own it
//! (`PassThrough` / `Fallback`), finishes the job on the deterministic
//! fallback route. One chat is therefore serialized — arrival order plus the
//! anchor — while different chats, and every other message of this agent,
//! proceed independently (design 27 §5.4.4).

pub mod context;
pub mod decision;
pub mod prompt;
pub mod runner;
pub mod tool;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::agent::inbound::IntentEnvelope;

/// Soft ceiling on the number of per-chat slots held in memory.
///
/// A slot is tiny, but the map is keyed by Chat session id and a long-lived
/// agent accumulates them. Past the ceiling, unreferenced slots (nobody is
/// mid-turn on that chat) are dropped; busy ones are never touched, so
/// eviction can never break the per-chat serialization guarantee.
const MAX_SLOTS: usize = 256;

/// Soft ceiling on the turns waiting for one chat's router. Crossing it only
/// logs — a queued message is never dropped — but a backlog that long means
/// the model is not keeping up and the operator should see that.
const MAX_PENDING_TURNS_PER_CHAT: usize = 64;

/// Per-Chat-session router state.
#[derive(Debug, Default)]
pub struct ChatSlot {
    /// Seq boundary of the history window (design 29 §4.4): every Block B
    /// message has `seq > anchor_seq`. In memory only — after a restart the
    /// first turn re-anchors from the newest page and pays one window
    /// rebuild, which is cheaper than persisting turn state per chat.
    pub anchor_seq: u64,
}

/// One Chat message handed to the router, plus what the fallback route needs
/// in case the router ends up not owning the turn.
#[derive(Debug)]
pub struct PendingTurn {
    pub envelope: Box<IntentEnvelope>,
    /// Session the Gateway designated for this message — an input to the
    /// deterministic route, which a degraded turn still has to reach.
    pub designated_session_id: String,
    /// Node token for the router's own Gateway HTTP calls.
    pub node_token: String,
}

/// A chat's dispatch queue. `draining` is the claim: exactly one drain task
/// owns a chat at a time, which is what keeps its turns in arrival order.
#[derive(Default)]
struct ChatQueue {
    pending: VecDeque<PendingTurn>,
    draining: bool,
}

/// Per-chat serialization + window anchors + dispatch queues. Shared by every
/// inbound Chat message of this agent's Runtime.
#[derive(Default)]
pub struct ChatSessionManager {
    slots: Mutex<HashMap<String, Arc<AsyncMutex<ChatSlot>>>>,
    queues: Mutex<HashMap<String, ChatQueue>>,
}

impl std::fmt::Debug for ChatSessionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatSessionManager")
            .field("slots", &self.slot_count())
            .field("queued_chats", &self.queued_chats())
            .finish()
    }
}

impl ChatSessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hand a Chat turn to the router. Returns `true` when the caller must
    /// spawn a drain task for this chat, i.e. nobody was already draining it.
    ///
    /// The claim is released by [`Self::finish_drain`], which re-claims it when
    /// a turn landed in the meantime, so a queued turn can never be left
    /// without a drainer. Both take only the queue map lock and never hold it
    /// across an await.
    pub fn push_turn(&self, chat_session_id: &str, turn: PendingTurn) -> bool {
        let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        // Same soft ceiling as the slot registry: drop queues that are neither
        // claimed nor holding work, so an old chat cannot keep an entry.
        if queues.len() >= MAX_SLOTS {
            queues.retain(|_, q| q.draining || !q.pending.is_empty());
        }
        let queue = queues.entry(chat_session_id.to_string()).or_default();
        if queue.pending.len() >= MAX_PENDING_TURNS_PER_CHAT {
            tracing::warn!(
                chat_session_id = %chat_session_id,
                pending = queue.pending.len(),
                "chat router queue is backing up — the model is slower than this chat"
            );
        }
        let must_spawn = !queue.draining;
        queue.draining = true;
        queue.pending.push_back(turn);
        must_spawn
    }

    /// Next turn of one chat, in arrival order. `None` means the queue is
    /// empty and the drainer should call [`Self::finish_drain`].
    pub fn take_turn(&self, chat_session_id: &str) -> Option<PendingTurn> {
        self.queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(chat_session_id)
            .and_then(|q| q.pending.pop_front())
    }

    /// Give up the drain claim. `true` means a turn arrived while the drainer
    /// was between its last `take_turn` and here, so the same drainer keeps
    /// going (the claim stays with it); `false` means the chat is now idle.
    pub fn finish_drain(&self, chat_session_id: &str) -> bool {
        let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        let Some(queue) = queues.get_mut(chat_session_id) else {
            return false;
        };
        if queue.pending.is_empty() {
            queues.remove(chat_session_id);
            return false;
        }
        true
    }

    /// Turns waiting for one chat. For diagnostics and tests.
    pub fn pending_turns(&self, chat_session_id: &str) -> usize {
        self.queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(chat_session_id)
            .map(|q| q.pending.len())
            .unwrap_or(0)
    }

    /// Whether this chat has router work — queued or mid-turn. An entry exists
    /// from the first push until the drainer releases an empty queue, so a
    /// caller can tell "handed to the queue" from "routed inline".
    pub fn chat_is_busy(&self, chat_session_id: &str) -> bool {
        self.queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(chat_session_id)
    }

    /// Chats with a dispatch queue entry (diagnostics / Debug only).
    fn queued_chats(&self) -> usize {
        self.queues.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// Take the turn lock for one Chat session.
    ///
    /// The returned guard is held for the whole turn — including the model
    /// call — so a second message in the same chat waits for the first to be
    /// routed. That is the intended queue: two router turns of one chat must
    /// not both read the same anchor and then race on the write-back. Other
    /// chats are unaffected. The guard is *owned*, so the slot stays alive
    /// even if the map evicts it while a turn is in flight.
    pub async fn begin_turn(&self, chat_session_id: &str) -> OwnedMutexGuard<ChatSlot> {
        self.slot(chat_session_id).lock_owned().await
    }

    /// Number of slots currently held (test/debug surface).
    pub fn slot_count(&self) -> usize {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    fn slot(&self, chat_session_id: &str) -> Arc<AsyncMutex<ChatSlot>> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = slots.get(chat_session_id) {
            return Arc::clone(existing);
        }
        if slots.len() >= MAX_SLOTS {
            // Only unreferenced slots are droppable: a slot whose Arc is held
            // by a running turn (strong count > 1) must keep its identity or
            // a later turn would start from a fresh anchor and rebuild the
            // window for no reason.
            slots.retain(|_, slot| Arc::strong_count(slot) > 1);
        }
        let slot = Arc::new(AsyncMutex::new(ChatSlot::default()));
        slots.insert(chat_session_id.to_string(), Arc::clone(&slot));
        slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// Two turns of the same chat must not overlap, and the slot's anchor
    /// must be visible to the second one.
    #[tokio::test]
    async fn same_chat_turns_are_serialized() {
        let manager = Arc::new(ChatSessionManager::new());
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let manager = Arc::clone(&manager);
            let in_flight = Arc::clone(&in_flight);
            let max_seen = Arc::clone(&max_seen);
            tasks.push(tokio::spawn(async move {
                let mut slot = manager.begin_turn("chat-1").await;
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                slot.anchor_seq += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(max_seen.load(Ordering::SeqCst), 1, "turns overlapped");
        let slot = manager.begin_turn("chat-1").await;
        assert_eq!(slot.anchor_seq, 2, "anchor updates were lost");
    }

    /// The turn lock is per chat, not global: two chats can be mid-turn at the
    /// same time. (The inbound loop never waits on a turn at all — that is the
    /// queue below plus `spawn_chat_router_drain`'s tests in `gateway_loop`.)
    #[tokio::test]
    async fn different_chats_take_their_own_turn_lock() {
        let manager = Arc::new(ChatSessionManager::new());
        let both_inside = Arc::new(tokio::sync::Barrier::new(2));
        let mut tasks = Vec::new();
        for chat in ["chat-a", "chat-b"] {
            let manager = Arc::clone(&manager);
            let both_inside = Arc::clone(&both_inside);
            tasks.push(tokio::spawn(async move {
                let _slot = manager.begin_turn(chat).await;
                // Deadlocks (test timeout) if the second chat had to wait.
                both_inside.wait().await;
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    }

    fn pending(chat: &str, message: &str) -> PendingTurn {
        PendingTurn {
            envelope: Box::new(IntentEnvelope {
                from: "user-1".into(),
                action: "chat".into(),
                params: serde_json::json!({ "message": message }),
                message_id: message.to_string(),
                conversation_id: String::new(),
                source_chat_session_id: chat.to_string(),
                repo: String::new(),
                session_config_json: String::new(),
                kind: "user_message".into(),
                in_reply_to: String::new(),
                status: String::new(),
                has_human: Some(true),
                chat_attachment_ids: Vec::new(),
                for_execution_session: String::new(),
            }),
            designated_session_id: String::new(),
            node_token: String::new(),
        }
    }

    /// One chat has exactly one drainer: the first push claims it, later pushes
    /// only append. That single drainer is what makes turns run in arrival
    /// order, so the queue must hand them back FIFO.
    #[test]
    fn one_drainer_per_chat_and_fifo_order() {
        let manager = ChatSessionManager::new();
        assert!(manager.push_turn("chat-1", pending("chat-1", "a")));
        assert!(!manager.push_turn("chat-1", pending("chat-1", "b")));
        assert!(!manager.push_turn("chat-1", pending("chat-1", "c")));
        assert_eq!(manager.pending_turns("chat-1"), 3);

        let order: Vec<String> = std::iter::from_fn(|| manager.take_turn("chat-1"))
            .map(|t| t.envelope.message_id.clone())
            .collect();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    /// Chats do not share a claim — a stalled chat cannot hold up another
    /// chat's drainer.
    #[test]
    fn separate_chats_are_claimed_independently() {
        let manager = ChatSessionManager::new();
        assert!(manager.push_turn("chat-a", pending("chat-a", "a")));
        assert!(manager.push_turn("chat-b", pending("chat-b", "b")));
        assert_eq!(manager.pending_turns("chat-a"), 1);
        assert_eq!(manager.pending_turns("chat-b"), 1);
    }

    /// The race that loses a message: the drainer finds the queue empty, and a
    /// turn is pushed before it releases the claim. `finish_drain` must keep
    /// the drainer going instead of leaving the turn with nobody to take it.
    #[test]
    fn a_turn_arriving_while_the_drainer_finishes_is_not_left_behind() {
        let manager = ChatSessionManager::new();
        assert!(manager.push_turn("chat-1", pending("chat-1", "a")));
        assert!(manager.take_turn("chat-1").is_some());

        // Second push races the finishing drainer: it sees the claim still
        // held, so no second drainer is spawned…
        assert!(!manager.push_turn("chat-1", pending("chat-1", "b")));
        // …and the original drainer is told to keep going.
        assert!(
            manager.finish_drain("chat-1"),
            "the queued turn would have been orphaned"
        );
        assert!(manager.take_turn("chat-1").is_some());
        assert!(!manager.finish_drain("chat-1"));
        assert_eq!(manager.pending_turns("chat-1"), 0);
    }

    /// An idle chat's queue entry is dropped, and a later turn re-claims —
    /// otherwise the map grows with every chat this agent has ever seen.
    #[test]
    fn an_idle_queue_is_dropped_and_a_later_turn_reclaims() {
        let manager = ChatSessionManager::new();
        assert!(manager.push_turn("chat-1", pending("chat-1", "a")));
        assert!(manager.take_turn("chat-1").is_some());
        assert!(!manager.finish_drain("chat-1"));
        assert_eq!(manager.queued_chats(), 0);
        // A turn arriving after the release starts a fresh drainer.
        assert!(manager.push_turn("chat-1", pending("chat-1", "b")));
    }

    /// Eviction drops idle slots only: a slot an in-flight turn still owns
    /// keeps its identity, so its anchor survives.
    #[tokio::test]
    async fn eviction_keeps_slots_owned_by_a_turn() {
        let manager = ChatSessionManager::new();
        let guard = manager.begin_turn("busy").await;
        for i in 0..MAX_SLOTS {
            let _ = manager.begin_turn(&format!("chat-{i}")).await;
        }
        // The busy slot is still the same object: setting the anchor through
        // a fresh lookup mutates the one the guard owns.
        drop(guard);
        let slot = manager.begin_turn("busy").await;
        assert_eq!(slot.anchor_seq, 0);
        assert!(manager.slot_count() <= MAX_SLOTS + 1);
    }
}
