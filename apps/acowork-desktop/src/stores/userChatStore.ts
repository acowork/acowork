/**
 * User↔user inbox state (ADR-076 §决策 8).
 *
 * Deliberately **not** folded into [chatStore](chatStore.ts): that store is
 * the agent-session chat (MQTT event streams, virtual list, tabs, tool
 * blocks, attachments) and shares no shape with a two-person inbox — an
 * agent conversation has a session id and a provider, a human one has a
 * `min__max` pair and an unread count. Merging them would branch ~3900
 * lines on "is this an agent or a person".
 *
 * Freshness is push-first: `acowork/users/{me}/chat/changed` (published by
 * `acowork-user` on every send and every read, design doc 25 §8) reaches this
 * store through the Desktop's Rust MQTT client → the `user-chat-changed` Tauri
 * event. The signal is **content-free**, so the reaction is always an HTTP
 * re-read — the HTTP response remains the only copy of the truth — and
 * `planPage` is the rule that keeps that re-read from moving the viewport.
 *
 * The poll is the degraded path and is owned by **this module**, not by a
 * component: it runs while a `multi_user` session is signed in AND the MQTT
 * bridge is down, driven by an [authStore](authStore.ts) subscription plus
 * [`setChatPushChannel`](#setChatPushChannel). That is a deliberate correction,
 * not a convenience. The loop
 * used to be mounted by whichever view rendered the inbox (`NavBar`, then a
 * full-screen `MessagesView`, then `InboxPanel`); each refactor moved the
 * `useEffect` along with the JSX and nobody noticed that the host's lifetime
 * had quietly changed from "while signed in" to "while this pane happens to
 * be the rendered one". `InboxPanel` is gated on `currentView === "chat"` *and*
 * `selectedAgentId === null`, so opening an agent session stopped the poll
 * entirely — a message sent from another device then never arrived. A data
 * loop must not live in the render tree, because the render tree is exactly
 * what refactors move.
 *
 * Read marking is the opposite case and stays with the renderer: only the
 * component that paints the messages knows the user saw them *and* knows
 * whether they are still looking at the bottom. See `markThreadRead` and
 * `lib/stickToBottom.ts`. If a future refactor ever orphans *that*, the damage is a
 * lagging badge, never silent data loss — which is the whole point of the split.
 */

import { create } from "zustand";

import {
  listChats,
  listMessages,
  markChatRead,
  sendChatMessage,
  uploadChatAttachment,
} from "../lib/user-chat-api";
import type {
  ChatAttachment,
  UserChatMessage,
  UserChatSummary,
} from "../lib/types";
import { useAuthStore } from "./authStore";
import { useProjectGroupStore } from "./projectGroupStore";

interface UserChatState {
  /** Conversations of the signed-in user, most recently active first. */
  chats: UserChatSummary[];
  /** The peer whose thread is open, if any. */
  activePeerId: string | null;
  /**
   * Display label for `activePeerId`, when the caller already knows it (the
   * sidebar UserList does). Without it a brand-new conversation — one with
   * no `GET /chats` row yet — would title itself with a raw UUID.
   */
  activePeerLabel: string | null;
  messages: UserChatMessage[];
  /**
   * Message count the loaded window reflects, taken from the server's `total`.
   * Since ADR-092 the reload rule is a `seq` suffix-diff and does not consult
   * this for correctness — it is kept only to drive the deprecated `total`
   * -delta fallback for a Gateway that predates `seq` (see `planPage`). `null`
   * = never told (older server, or the thread was not loaded yet).
   */
  messagesTotal: number | null;
  /** Arrivals appended while the reader was scrolled up — the pill count. */
  unseenCount: number;
  /**
   * Uploaded but not yet sent, in upload order. Cleared only once a message
   * that carries them is accepted — an upload is not a delivery.
   */
  pendingAttachments: ChatAttachment[];
  /** File names currently uploading, for the composer's progress hint. */
  uploading: string[];
  loadingChats: boolean;
  loadingMessages: boolean;
  sending: boolean;
  error: string | null;

  refreshChats: () => Promise<void>;
  openChat: (peerId: string, peerLabel?: string) => Promise<void>;
  closeChat: () => void;
  /** Reload the open thread's window under the `planPage` rule. */
  reloadMessages: () => Promise<void>;
  /**
   * One wake-up signal (or one fallback tick): re-read everything it can mean.
   * The signal never says WHICH thread changed, so both reads always run — a
   * first message in a brand-new conversation has to be discovered through the
   * chat list, and a badge cleared on another device the same way.
   */
  wake: () => Promise<void>;
  /** Upload one file; returns false when the Gateway refused it. */
  attach: (file: File) => Promise<boolean>;
  removePendingAttachment: (id: string) => void;
  /**
   * Send the draft. Returns true when the Gateway accepted the message.
   * `attachmentIds` defaults to the pending set, which is then cleared.
   */
  send: (body: string, attachmentIds?: string[]) => Promise<boolean>;
  /**
   * Ack the open thread as read. Called by whoever *renders* it, on every
   * change to the visible message list — see the module doc for why this is
   * the renderer's job while the poll is not. Idempotent per message: a
   * thread already acked through its newest message costs no request, so a
   * 5 s poll does not become a 5 s POST storm.
   */
  markThreadRead: () => Promise<void>;
  reset: () => void;
}

/** Canonical pair id — must match the Gateway's `min__max` (byte order). */
export function chatIdOf(a: string, b: string): string {
  return [a, b].sort().join("__");
}

/**
 * Fallback cadence while no push channel is up. It is NOT the normal path any
 * more — the wake-up signal replaced it (design doc 25 §8) — and the timer
 * stops entirely as soon as the MQTT bridge reports connected, the same
 * exclusivity rule the agent-session watcher applies (§8.0). It stays because
 * a Desktop whose MQTT connect never came up must still converge.
 */
export const POLL_MS = 5000;

let pollTimer: ReturnType<typeof setInterval> | null = null;

/** Whether the MQTT bridge is currently delivering `user-chat-changed`. */
let pushChannelUp = false;

/**
 * Newest acked cursor per peer — the message `seq` on a ADR-092 Gateway, or the
 * `ts` on a legacy one (see `markThreadRead`). Guards `markThreadRead` against
 * re-POSTing for a thread that has not changed since the last look, and seeds
 * the `ackedSeq` that `planPage`'s hole branch counts unseen from.
 */
const ackedThrough = new Map<string, number>();

/**
 * Start, stop or leave the poll loop so that it matches the session AND the
 * push channel.
 *
 * Idempotent and cheap: it only touches the timer when the "should the poll
 * run" predicate actually flips. Exported for tests, which drive it by moving
 * `authStore` through login/logout rather than by mounting anything.
 */
export function syncPollLoop(): void {
  const shouldRun = selfId() != null && !pushChannelUp;
  if (shouldRun === (pollTimer != null)) return;

  if (shouldRun) {
    pollTimer = setInterval(() => {
      void useUserChatStore.getState().wake();
    }, POLL_MS);
    // A sign-in should show the inbox immediately; making the user wait out
    // the first full interval is how "it only updates when I poke it" feels.
    void useUserChatStore.getState().refreshChats();
  } else {
    if (pollTimer != null) clearInterval(pollTimer);
    pollTimer = null;
    // Not here: leaving the loop is also what a healthy MQTT connection does,
    // so tearing state down on this branch would empty the inbox every time
    // the bridge came up. Session changes own that — see `onSessionChange`.
  }
}

/**
 * Report the MQTT bridge's liveness so the fallback timer can stand down.
 *
 * Called from the one place that knows the bridge state — the `mqtt-status`
 * fanout in [chatStore](chatStore.ts) — never from a view. Coming up stops the
 * poll (signals replaced it); coming back down re-arms it, which is the point:
 * the degraded path must still converge, just slower.
 *
 * ADR-092 decision 6 folds the edge invariant in here: the `down → up` edge is
 * *also* the one proactive re-read (the doorbells that fired while we were
 * unsubscribed are non-retained and gone for good), so no caller has to
 * remember to `wake()` after raising the channel. `up → up` is a no-op and must
 * not re-read; `up → down` only re-arms the timer.
 */
export function setChatPushChannel(up: boolean): void {
  if (pushChannelUp === up) return;
  pushChannelUp = up;
  syncPollLoop();
  if (up) void useUserChatStore.getState().wake();
}

/** The signed-in user's id, or null when the account system is off. */
function selfId(): string | null {
  const { mode, account } = useAuthStore.getState();
  return mode === "multi_user" ? (account?.user_id ?? null) : null;
}

function describe(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

const EMPTY = {
  chats: [] as UserChatSummary[],
  activePeerId: null as string | null,
  activePeerLabel: null as string | null,
  messages: [] as UserChatMessage[],
  messagesTotal: null as number | null,
  unseenCount: 0,
  pendingAttachments: [] as ChatAttachment[],
  uploading: [] as string[],
  loadingChats: false,
  loadingMessages: false,
  sending: false,
  error: null as string | null,
};

/* ---------------- the reload rule ---------------- */

export type PagePlan =
  | { kind: "none" }
  | { kind: "append"; tail: UserChatMessage[] }
  | { kind: "resync"; messages: UserChatMessage[]; unseen: number };

/**
 * Decide what a freshly fetched page does to the window the user is looking at.
 *
 * ADR-092 decision 2: the rule is a **seq suffix-diff**, a content judgement,
 * not a count guess. The page is always the newest `limit` messages, so a naive
 * `set({ messages })` replaces everything above the viewport too — and a
 * bubble's height is the displacement, so a reader parked halfway through a
 * thread is yanked to a different message. `heldMaxSeq` is the newest seq we
 * already render; only messages past it are new.
 *
 *  - `append.length === 0` — nothing arrived: not a single React re-render, no
 *    scroll. This is content-level, so a re-parse of the same page (a poll tick
 *    against an idle thread) is provably `none`, never a spurious resync.
 *  - no hole (`held` empty, or the page starts at or before `heldMaxSeq + 1`) —
 *    append the suffix. The head keeps its object references, so the reader
 *    stays put; unseen is exactly the appended count.
 *  - a hole — messages between `heldMaxSeq` and the page start are missing
 *    (a corrupt line, or a window that fell off), so the delta cannot be
 *    trusted: rebuild. unseen counts the page messages past what was acked.
 *
 * `seq` is absent only on a Gateway predating ADR-092 (Mobile may reach one);
 * we then fall back to the deprecated `total`-delta rule (ADR-076 §决策 8 v2
 * 第 4 条), bugs and all — identity cannot be conjured from a count.
 *
 * The mobile app carries the identical function in its own store; the two
 * clients read the same endpoint and must not drift on this rule.
 */
export function planPage(
  held: UserChatMessage[],
  page: UserChatMessage[],
  counts: { knownTotal: number | null; total: number | null },
  ackedSeq = 0,
): PagePlan {
  if (page.length > 0 && typeof page[0].seq === "number") {
    const heldMaxSeq = held.reduce((n, m) => Math.max(n, m.seq ?? 0), 0);
    const append = page.filter((m) => (m.seq ?? 0) > heldMaxSeq);
    if (append.length === 0) return { kind: "none" };
    const gapless = held.length === 0 || (page[0].seq ?? 0) <= heldMaxSeq + 1;
    if (gapless) return { kind: "append", tail: append };
    const unseen = page.reduce((n, m) => ((m.seq ?? 0) > ackedSeq ? n + 1 : n), 0);
    return { kind: "resync", messages: page, unseen };
  }
  // No seq on the newest message means a legacy Gateway. An empty page is not
  // evidence of legacy-ness, so settle it directly: nothing held → nothing to
  // show; something held → the thread reports empty, rebuild to empty.
  if (page.length === 0) {
    return held.length === 0
      ? { kind: "none" }
      : { kind: "resync", messages: page, unseen: 0 };
  }
  return planPageByCount(held, counts.knownTotal, page, counts.total);
}

/**
 * Deprecated `total`-delta fallback for a pre-ADR-092 Gateway (see `planPage`).
 * Kept byte-for-byte as the rule it replaced; the count-guess bug family it
 * carries is knowingly preserved, not fixed.
 */
function planPageByCount(
  held: UserChatMessage[],
  knownTotal: number | null,
  page: UserChatMessage[],
  total: number | null,
): PagePlan {
  if (total === null || knownTotal === null) {
    // No count to compare against: only replace when the content differs — a
    // poll tick against an idle thread is otherwise a pure re-render.
    return sameMessages(held, page)
      ? { kind: "none" }
      : {
          kind: "resync",
          messages: page,
          unseen: Math.max(0, (total ?? page.length) - (knownTotal ?? 0)),
        };
  }
  const d = total - knownTotal;
  if (d === 0) return { kind: "none" };
  if (d < 0 || d > page.length) return { kind: "resync", messages: page, unseen: Math.max(0, d) };
  const tail = page.slice(page.length - d);
  return tail.length === 0 ? { kind: "none" } : { kind: "append", tail };
}

/** Reference equality per item: a re-parse of the same page is not "the same". */
function sameMessages(a: UserChatMessage[], b: UserChatMessage[]): boolean {
  if (a.length !== b.length) return false;
  return a.every((m, i) => m === b[i]);
}

/* A burst is normal, not exceptional: one send from my other laptop emits two
 * signals and both reach this device. Each would otherwise start a chat-list
 * GET and a page GET, so the rounds are coalesced — one in flight, at most one
 * more queued behind it. Declared above the store because `wake` closes over
 * both; the timer only ever calls them after module init. */
let wakeInFlight: Promise<void> | null = null;
let wakeQueued = false;

export const useUserChatStore = create<UserChatState>((set, get) => ({
  ...EMPTY,

  refreshChats: async () => {
    const self = selfId();
    if (!self) return;
    set({ loadingChats: true });
    try {
      set({ chats: await listChats(self), error: null });
    } catch (err) {
      set({ error: describe(err) });
    } finally {
      set({ loadingChats: false });
    }
  },

  openChat: async (peerId, peerLabel) => {
    // Middle-pane exclusivity: opening an inbox peer closes any open
    // project group chat (mirror of `agentStore.selectAgent`).
    useProjectGroupStore.getState().clearSelection();
    const self = selfId();
    if (!self) return;
    set({
      activePeerId: peerId,
      activePeerLabel: peerLabel ?? null,
      messages: [],
      // The window is about to become a DIFFERENT thread's newest page: an old
      // count would read as a negative delta and force a resync.
      messagesTotal: null,
      unseenCount: 0,
      pendingAttachments: [],
      error: null,
    });
    // No read receipt here on purpose. Opening a thread does not mean the
    // user read it — the renderer that paints these messages acks them via
    // `markThreadRead`, which also covers every message that arrives later
    // while the thread stays open (the old `openChat`-only ack is why a
    // badge sat at 1 forever while the poll happily delivered the message).
    await get().reloadMessages();
  },

  closeChat: () =>
    set({
      activePeerId: null,
      activePeerLabel: null,
      messages: [],
      messagesTotal: null,
      unseenCount: 0,
      pendingAttachments: [],
    }),

  reloadMessages: async () => {
    const self = selfId();
    const peerId = get().activePeerId;
    if (!self || !peerId) return;
    // Only the first load of a thread is worth a "loading" state: a refresh
    // that ends in `none` or a tail append must not flicker the empty hint.
    if (get().messages.length === 0) set({ loadingMessages: true });
    let page;
    try {
      page = await listMessages(self, chatIdOf(peerId, self));
    } catch (err) {
      // A transient failure keeps showing what we have — the next signal or
      // tick re-reads, and blanking a thread that rendered fine is worse than
      // a thread one beat behind.
      if (get().activePeerId === peerId) set({ error: describe(err) });
      return;
    } finally {
      if (get().activePeerId === peerId) set({ loadingMessages: false });
    }
    // Drop a stale response if the user switched threads meanwhile; the count
    // is per-thread, so it must not travel either.
    if (get().activePeerId !== peerId) return;
    const ackedSeq = ackedThrough.get(peerId) ?? 0;
    const plan = planPage(
      get().messages,
      page.messages,
      { knownTotal: get().messagesTotal, total: page.total },
      ackedSeq,
    );
    if (plan.kind === "none") return;
    if (plan.kind === "append") {
      set((s) => ({
        messages: [...s.messages, ...plan.tail],
        messagesTotal: page.total,
        // The first paint of a thread lands on the newest page — that is not
        // "missed" anything. After that, arrivals while scrolled up are the
        // pill count; the panel clears it on the same tick when the reader was
        // already at the bottom (they did just see it).
        unseenCount: s.messages.length === 0 ? 0 : s.unseenCount + plan.tail.length,
        error: null,
      }));
      return;
    }
    set((s) => ({
      messages: plan.messages,
      messagesTotal: page.total,
      // A rebuilt window has no meaningful "how many did I miss" beyond what
      // `planPage` counted; the first load of a thread missed nothing at all.
      unseenCount: s.messages.length === 0 ? 0 : s.unseenCount + plan.unseen,
      error: null,
    }));
  },

  /*
   * React to one wake-up signal (also what the fallback timer calls).
   *
   * Both reads, always: the signal is empty by design, so a thread arriving in
   * a brand-new conversation has to be discovered through the chat list, and a
   * badge cleared on another device has to be re-read the same way.
   */
  wake: () => {
    if (wakeInFlight) {
      wakeQueued = true;
      return wakeInFlight;
    }
    const run = (async () => {
      do {
        wakeQueued = false;
        await get().refreshChats();
        await get().reloadMessages();
      } while (wakeQueued);
      // Cleared inside the body, not in a `finally`: the gap between the loop's
      // last test and this line has no await, so no signal can be lost in it.
      wakeInFlight = null;
    })();
    wakeInFlight = run;
    return run;
  },

  attach: async (file) => {
    const self = selfId();
    const peerId = get().activePeerId;
    if (!self || !peerId) return false;

    // Snapshot the thread: a switch while the upload is in flight must not
    // drop the result into someone else's composer.
    const chatId = chatIdOf(peerId, self);
    set((s) => ({ uploading: [...s.uploading, file.name], error: null }));
    try {
      const attachment = await uploadChatAttachment(self, chatId, file);
      if (get().activePeerId !== peerId) return false;
      set((s) => ({ pendingAttachments: [...s.pendingAttachments, attachment] }));
      return true;
    } catch (err) {
      set({ error: describe(err) });
      return false;
    } finally {
      set((s) => ({ uploading: s.uploading.filter((n) => n !== file.name) }));
    }
  },

  removePendingAttachment: (id) =>
    set((s) => ({
      pendingAttachments: s.pendingAttachments.filter((a) => a.id !== id),
    })),

  send: async (body, attachmentIds) => {
    const self = selfId();
    const peerId = get().activePeerId;
    const ids = attachmentIds ?? get().pendingAttachments.map((a) => a.id);
    const text = body.trim();
    if (!self || !peerId || (!text && ids.length === 0)) return false;

    set({ sending: true });
    try {
      const sent = await sendChatMessage(
        self,
        chatIdOf(peerId, self),
        text,
        ids,
      );
      if (get().activePeerId === peerId) {
        set((s) => ({
          messages: [...s.messages, sent],
          // Account for it in the count too, or the echo of our own signal
          // arrives looking like one new message and appends it twice.
          messagesTotal: s.messagesTotal === null ? null : s.messagesTotal + 1,
          error: null,
          // Only the set that actually went out is dropped; anything the
          // user attached during the round trip stays queued.
          pendingAttachments: s.pendingAttachments.filter(
            (a) => !ids.includes(a.id),
          ),
        }));
      }
      // The first message of a conversation also *creates* it, so refetch
      // rather than patching a row that may not exist yet.
      void get().refreshChats();
      return true;
    } catch (err) {
      set({ error: describe(err) });
      return false;
    } finally {
      set({ sending: false });
    }
  },

  reset: () => {
    ackedThrough.clear();
    set({ ...EMPTY });
  },

  markThreadRead: async () => {
    const self = selfId();
    const peerId = get().activePeerId;
    if (!self || !peerId) return;
    const messages = get().messages;
    if (messages.length === 0) return;

    // Ack once per message. A refresh that changed nothing does not re-set
    // `messages` any more (see `planPage`), but the renderer also fires on
    // thread switches and on the pinned flip, so without this guard an idle
    // visible thread would still POST on every look.
    // The cursor is the newest `seq` on an ADR-092 Gateway — same-second read
    // receipts can no longer collide (decision 2); a legacy message falls back
    // to `ts`, preserving the pre-ADR-092 behavior exactly.
    const newest = messages.reduce((n, m) => Math.max(n, m.seq ?? m.ts), 0);
    if (ackedThrough.get(peerId) === newest) return;

    // Clear the badge with the paint that made the messages visible, then
    // tell the Gateway. A failed receipt leaves the optimistic zero in place
    // until the next `refreshChats` re-reads the server's count — the badge
    // may briefly come back, which beats a dot that never clears.
    set((s) => ({
      unseenCount: 0,
      chats: s.chats.map((c) =>
        c.peer_user_id === peerId ? { ...c, unread_count: 0 } : c,
      ),
    }));
    ackedThrough.set(peerId, newest);
    try {
      await markChatRead(self, chatIdOf(peerId, self));
    } catch {
      ackedThrough.delete(peerId); // let the next look retry
    }
  },
}));

// The poll's host is the session, so the subscription lives here rather than
// in any component. The immediate call covers a session that was already
// restored before this module was first imported.

/**
 * The account the state currently belongs to.
 *
 * Tracked separately from `syncPollLoop`'s "is anyone signed in" test because
 * the two questions have different answers on a push-channel flip: signing in
 * must NOT wipe the inbox (the bridge coming up stops the poll and would
 * otherwise look like a logout), while a session actually changing hands must.
 */
let lastSelfId: string | null = null;

function onSessionChange(): void {
  const self = selfId();
  if (self !== lastSelfId) {
    lastSelfId = self;
    // Also drops the per-peer ack memory: a new session must not inherit
    // "already read" for a thread it has never shown.
    useUserChatStore.getState().reset();
  }
  syncPollLoop();
}

useAuthStore.subscribe(onSessionChange);
onSessionChange();
