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
 * Transport is plain HTTP + a poll (there is no MQTT topic for user chat —
 * ADR-076 §决策 8 puts the whole feature on the Gateway's HTTP surface).
 *
 * The poll is owned by **this module**, not by a component: it runs exactly
 * while a `multi_user` session is signed in, driven by an [authStore](authStore.ts)
 * subscription. That is a deliberate correction, not a convenience. The loop
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
 * component that paints the messages knows the user saw them. See
 * `markThreadRead`. If a future refactor ever orphans *that*, the damage is a
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
  reloadMessages: () => Promise<void>;
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

/** Poll cadence. Exactly one loop exists, keyed on the session, not on a view. */
export const POLL_MS = 5000;

let pollTimer: ReturnType<typeof setInterval> | null = null;

/**
 * Newest message `ts` already acked per peer. Guards `markThreadRead` against
 * re-POSTing for a thread that has not changed since the last look.
 */
const ackedThrough = new Map<string, number>();

/**
 * Start, stop or leave the poll loop so that it matches the session.
 *
 * Idempotent and cheap: it only touches the timer when the "should the poll
 * run" predicate actually flips. Exported for tests, which drive it by moving
 * `authStore` through login/logout rather than by mounting anything.
 */
export function syncPollLoop(): void {
  const shouldRun = selfId() != null;
  if (shouldRun === (pollTimer != null)) return;

  if (shouldRun) {
    pollTimer = setInterval(() => {
      const current = useUserChatStore.getState();
      void current.refreshChats();
      if (current.activePeerId) void current.reloadMessages();
    }, POLL_MS);
    // A sign-in should show the inbox immediately; making the user wait out
    // the first full interval is how "it only updates when I poke it" feels.
    void useUserChatStore.getState().refreshChats();
  } else {
    if (pollTimer != null) clearInterval(pollTimer);
    pollTimer = null;
    // `reset` also drops the per-peer ack memory: a new session must not
    // inherit "already read" for a thread it has never shown.
    useUserChatStore.getState().reset();
  }
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
  pendingAttachments: [] as ChatAttachment[],
  uploading: [] as string[],
  loadingChats: false,
  loadingMessages: false,
  sending: false,
  error: null as string | null,
};

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
    const self = selfId();
    if (!self) return;
    set({
      activePeerId: peerId,
      activePeerLabel: peerLabel ?? null,
      messages: [],
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
      pendingAttachments: [],
    }),

  reloadMessages: async () => {
    const self = selfId();
    const peerId = get().activePeerId;
    if (!self || !peerId) return;
    set({ loadingMessages: true });
    try {
      const page = await listMessages(self, chatIdOf(peerId, self));
      // Drop a stale response if the user switched threads meanwhile.
      if (get().activePeerId === peerId) set({ messages: page.messages, error: null });
    } catch (err) {
      if (get().activePeerId === peerId) set({ error: describe(err) });
    } finally {
      set({ loadingMessages: false });
    }
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

    // Ack once per message. The poll re-sets `messages` every tick even when
    // nothing arrived, so without this the visible thread would POST every
    // 5 s for the rest of the session.
    const newest = messages.reduce((n, m) => (m.ts > n ? m.ts : n), 0);
    if (ackedThrough.get(peerId) === newest) return;

    // Clear the badge with the paint that made the messages visible, then
    // tell the Gateway. A failed receipt leaves the optimistic zero in place
    // until the next `refreshChats` re-reads the server's count — the badge
    // may briefly come back, which beats a dot that never clears.
    set((s) => ({
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
useAuthStore.subscribe(syncPollLoop);
syncPollLoop();
