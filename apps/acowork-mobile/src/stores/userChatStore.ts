/**
 * User-to-user 1:1 chat (ADR-076 §决策 8, push path per design doc 25 §8).
 *
 * Deliberately a SEPARATE store from `chatStore`, not a special case inside
 * it. An agent conversation has sessions, `can_write`, a live status and an
 * MQTT channel; a human thread has none of those — it is a flat message log
 * with a server-side read cursor. Forcing both through one state machine
 * would mean a `null` sprinkled through every agent-only branch.
 *
 * The chat id is derived (`userChatId(me, peer)`), never looked up: a thread
 * exists as soon as its first message is sent, so there is no "create chat"
 * step and no way to open a thread that the server has not seen.
 *
 * Freshness is push-first: `acowork/users/{me}/chat/changed` (published by
 * `acowork-user` on every send and every read) is a **content-free** nudge,
 * so the reaction is always an HTTP re-read — the HTTP response is the only
 * copy of the truth. The signal never says WHICH thread changed, which is
 * why the reload is cheap to get right and expensive to get wrong: see
 * `planPage` for the rule that keeps a reload from moving the viewport.
 */

import { create } from 'zustand'
import { fetchUserChats, fetchUserMessages, markUserChatRead, sendUserMessage, userChatId } from '../lib/api'
import { useAuthStore } from './authStore'
import type { UserChatMessage, UserChatSummary } from '../lib/types'

/**
 * Fallback cadence while no push channel is up. It is NOT the normal path any
 * more — the wake-up signal replaced it (design doc 25 §8) — and the timer
 * stops entirely as soon as the channel subscribes successfully, the same
 * exclusivity rule `lib/realtime.ts` applies to agent sessions. It stays
 * because a WebView that cannot reach the `/mqtt` bridge must still converge.
 */
export const USER_CHAT_POLL_MS = 5000

/** The logged-in account id, or null until `/auth/me` has resolved. */
function meId(): string | null {
  return useAuthStore.getState().me?.user_id ?? null
}

interface UserChatStore {
  /** Threads this account has, most recent first — the inbox previews. */
  chats: UserChatSummary[]
  chatsLoaded: boolean
  /** Peer currently on screen; null = the inbox. */
  activePeerId: string | null
  activePeerName: string | null
  /** Oldest-first, exactly as the server returns them. */
  messages: UserChatMessage[]
  /**
   * Message count the loaded window reflects, taken from the server's `total`.
   * Since ADR-092 the reload rule is a `seq` suffix-diff and does not consult
   * this for correctness — it is kept only to drive the deprecated `total`
   * -delta fallback for a Gateway that predates `seq` (Mobile may reach one;
   * see `planPage`). `null` = never told (older server, or not loaded yet).
   */
  messagesTotal: number | null
  /** Arrivals appended while the user was reading higher up — the pill count. */
  unseenCount: number
  loading: boolean
  sending: boolean
  error: string | null

  refreshChats(): Promise<void>
  /** Open the thread with `peerId`; label falls back to the chat summary. */
  openChat(peerId: string, peerName?: string): Promise<void>
  closeChat(): void
  send(text: string): Promise<boolean>
  /** Reload the open thread's window under the `planPage` rule. */
  refreshMessages(): Promise<void>
  /** One wake-up signal (or one fallback tick): re-read what it can mean. */
  wake(): Promise<void>
  /** The user has seen everything on screen: clear the badge everywhere. */
  markSeen(): Promise<void>
}

/* ---------------- the reload rule ---------------- */

export type PagePlan =
  | { kind: 'none' }
  | { kind: 'append'; tail: UserChatMessage[] }
  | { kind: 'resync'; messages: UserChatMessage[]; unseen: number }

/**
 * Decide what a freshly fetched page does to the window the user is looking at.
 *
 * ADR-092 decision 2: a **seq suffix-diff**, a content judgement, not a count
 * guess. The page is always the newest `limit` messages, so a naive
 * `set({ messages })` replaces everything above the viewport too — and a
 * bubble's height is the displacement, so a reader parked halfway through a
 * thread is yanked to a different message. `heldMaxSeq` is the newest seq we
 * already render; only messages past it are new.
 *
 *  - `append.length === 0` — nothing arrived: not a single re-render, no scroll.
 *    Content-level, so a re-parse of the same page is provably `none`.
 *  - no hole (`held` empty, or the page starts at or before `heldMaxSeq + 1`) —
 *    append the suffix; unseen is exactly the appended count.
 *  - a hole — messages between `heldMaxSeq` and the page start are missing, so
 *    rebuild; unseen counts the page messages past what was acked.
 *
 * `seq` is absent only on a Gateway predating ADR-092; we then fall back to the
 * deprecated `total`-delta rule (ADR-076 §决策 8 v2 第 4 条), bugs and all.
 *
 * The Desktop carries the identical function; the two clients read the same
 * endpoint and must not drift on this rule.
 */
export function planPage(
  held: UserChatMessage[],
  page: UserChatMessage[],
  counts: { knownTotal: number | null; total: number | null },
  ackedSeq = 0,
): PagePlan {
  const first = page[0]
  if (first && typeof first.seq === 'number') {
    const heldMaxSeq = held.reduce((n, m) => Math.max(n, m.seq ?? 0), 0)
    const append = page.filter((m) => (m.seq ?? 0) > heldMaxSeq)
    if (append.length === 0) return { kind: 'none' }
    const gapless = held.length === 0 || (first.seq ?? 0) <= heldMaxSeq + 1
    if (gapless) return { kind: 'append', tail: append }
    const unseen = page.reduce((n, m) => ((m.seq ?? 0) > ackedSeq ? n + 1 : n), 0)
    return { kind: 'resync', messages: page, unseen }
  }
  // No seq on the newest message means a legacy Gateway. An empty page is not
  // evidence of legacy-ness, so settle it directly: nothing held → nothing to
  // show; something held → the thread reports empty, rebuild to empty.
  if (page.length === 0) {
    return held.length === 0
      ? { kind: 'none' }
      : { kind: 'resync', messages: page, unseen: 0 }
  }
  return planPageByCount(held, counts.knownTotal, page, counts.total)
}

/**
 * Deprecated `total`-delta fallback for a pre-ADR-092 Gateway (see `planPage`).
 * Kept as the rule it replaced; the count-guess bug family it carries is
 * knowingly preserved, not fixed.
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
      ? { kind: 'none' }
      : {
          kind: 'resync',
          messages: page,
          unseen: Math.max(0, (total ?? page.length) - (knownTotal ?? 0)),
        }
  }
  const d = total - knownTotal
  if (d === 0) return { kind: 'none' }
  if (d < 0 || d > page.length) return { kind: 'resync', messages: page, unseen: Math.max(0, d) }
  const tail = page.slice(page.length - d)
  return tail.length === 0 ? { kind: 'none' } : { kind: 'append', tail }
}

/** Reference equality per item: a re-parse of the same page is not "the same". */
function sameMessages(a: UserChatMessage[], b: UserChatMessage[]): boolean {
  if (a.length !== b.length) return false
  return a.every((m, i) => m === b[i])
}

/* ---------------- store ---------------- */

/* A burst is normal, not exceptional: one send from my other laptop emits two
 * signals and both reach this device. Each would otherwise start a chat-list
 * GET and a page GET, so the rounds are coalesced — one in flight, at most one
 * more queued behind it. Declared above the store because `wake` closes over
 * both; the timer only ever calls them after module init. */
let wakeInFlight: Promise<void> | null = null
let wakeQueued = false

export const useUserChatStore = create<UserChatStore>((set, get) => ({
  chats: [],
  chatsLoaded: false,
  activePeerId: null,
  activePeerName: null,
  messages: [],
  messagesTotal: null,
  unseenCount: 0,
  loading: false,
  sending: false,
  error: null,

  refreshChats: async () => {
    const me = meId()
    if (!me) return
    try {
      set({ chats: await fetchUserChats(me), chatsLoaded: true })
    } catch {
      /* the inbox still renders the directory — a failed preview pull is not fatal */
    }
  },

  openChat: async (peerId, peerName) => {
    const me = meId()
    if (!me) return
    const chatId = userChatId(me, peerId)
    set({
      activePeerId: peerId,
      activePeerName:
        peerName ?? get().chats.find((c) => c.peer_user_id === peerId)?.peer_display_name ?? peerId,
      messages: [],
      // The window is about to be the newest page of a DIFFERENT thread: an
      // old count would read as a negative delta and force a resync.
      messagesTotal: null,
      unseenCount: 0,
      loading: true,
      error: null,
    })
    try {
      const page = await fetchUserMessages(me, chatId)
      // Drop a stale page if the user switched threads while it was in flight.
      if (get().activePeerId !== peerId) return
      set({ messages: page.messages, messagesTotal: page.total, unseenCount: 0 })
      // Opening the thread lands at the newest message, which IS reading it:
      // clear the badge server-side, then refetch the summaries so the inbox
      // row stops claiming otherwise.
      await markUserChatRead(me, chatId).catch(() => undefined)
      void get().refreshChats()
    } catch (e) {
      if (get().activePeerId === peerId) set({ error: e instanceof Error ? e.message : '加载失败' })
    } finally {
      if (get().activePeerId === peerId) set({ loading: false })
    }
  },

  closeChat: () =>
    set({
      activePeerId: null,
      activePeerName: null,
      messages: [],
      messagesTotal: null,
      unseenCount: 0,
      error: null,
      sending: false,
    }),

  send: async (text) => {
    const me = meId()
    const peer = get().activePeerId
    const body = text.trim()
    if (!me || !peer || !body) return false
    set({ sending: true, error: null })
    try {
      const sent = await sendUserMessage(me, userChatId(me, peer), body)
      // Only append if the user has not switched threads mid-round-trip.
      if (get().activePeerId === peer) {
        set((s) => ({
          messages: [...s.messages, sent],
          // Account for it in the count too, or the echo of our own signal
          // arrives looking like one new message and appends it twice.
          messagesTotal: s.messagesTotal === null ? null : s.messagesTotal + 1,
        }))
      }
      // The first message also CREATES the thread, so refetch rather than
      // patching a row that may not exist yet.
      void get().refreshChats()
      return true
    } catch (e) {
      set({ error: e instanceof Error ? e.message : '发送失败' })
      return false
    } finally {
      set({ sending: false })
    }
  },

  refreshMessages: async () => {
    const me = meId()
    const peer = get().activePeerId
    if (!me || !peer) return
    let page
    try {
      page = await fetchUserMessages(me, userChatId(me, peer))
    } catch {
      /* transient network failure: keep showing what we have */
      return
    }
    // A page that lands after a thread switch belongs to a conversation the
    // user walked away from; `messagesTotal` is per-thread, so it must not.
    if (get().activePeerId !== peer) return
    // Mobile has no per-peer ack memory (it acks via `markSeen`); the newest
    // seq on screen is the best available `ackedSeq` for the hole branch.
    const ackedSeq = get().messages.reduce((n, x) => Math.max(n, x.seq ?? 0), 0)
    const plan = planPage(
      get().messages,
      page.messages,
      { knownTotal: get().messagesTotal, total: page.total },
      ackedSeq,
    )
    if (plan.kind === 'none') return
    if (plan.kind === 'append') {
      set((s) => ({
        messages: [...s.messages, ...plan.tail],
        messagesTotal: page.total,
        // The first paint lands on the newest page — that is not "missed"
        // anything. After that, arrivals while scrolled up are the pill count;
        // the screen clears it on the same tick when already at the bottom.
        unseenCount: s.messages.length === 0 ? 0 : s.unseenCount + plan.tail.length,
        error: null,
      }))
      return
    }
    set((s) => ({
      messages: plan.messages,
      messagesTotal: page.total,
      // A rebuilt window has no meaningful "how many did I miss" beyond what
      // `planPage` counted; the first load of a thread missed nothing at all.
      unseenCount: s.messages.length === 0 ? 0 : s.unseenCount + plan.unseen,
      error: null,
    }))
  },

  markSeen: async () => {
    const me = meId()
    const peer = get().activePeerId
    if (!me || !peer || get().unseenCount === 0) return
    set({ unseenCount: 0 })
    try {
      await markUserChatRead(me, userChatId(me, peer))
      void get().refreshChats()
    } catch {
      /* the next signal re-reads the badge; a failed ack is not user-visible */
    }
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
      wakeQueued = true
      return wakeInFlight
    }
    const run = (async () => {
      do {
        wakeQueued = false
        await get().refreshChats()
        await get().refreshMessages()
      } while (wakeQueued)
      // Cleared inside the body, not in a `finally`: the gap between the loop's
      // last test and this line has no await, so no signal can be lost in it.
      wakeInFlight = null
    })()
    wakeInFlight = run
    return run
  },
}))

/** Convenience binding for the MQTT channel and the fallback timer. */
export function wakeUserChat(): Promise<void> {
  return useUserChatStore.getState().wake()
}

/* ---------------- fallback timer ---------------- */

/* One interval at a time, however many mount/unmount cycles the screen sees
 * (StrictMode double-mounts in dev). Same contract as `lib/realtime.ts`. */
let pollTimer: ReturnType<typeof setInterval> | null = null
let threadOnScreen = false
let pushChannelUp = false

/** The thread screen mounts with this true, unmounts with false. */
export function setUserChatThreadOnScreen(on: boolean): void {
  threadOnScreen = on
  syncPoll()
}

/** The background channel reports its own SUBACK / loss through this. */
export function setChatPushChannel(up: boolean): void {
  // ADR-092 decision 6: the edge invariant lives here, not in the caller.
  // `down → up` stops the timer (via syncPoll) *and* fires the one catch-up
  // re-read (the doorbells that fired while unsubscribed are non-retained);
  // `up → up` is a no-op so a reconnect cycle that re-reports the same state
  // never double-fires; `up → down` only re-arms. This is what fixes the
  // historical Mobile drift — `reportChat` now only reports the fact.
  if (pushChannelUp === up) return
  pushChannelUp = up
  syncPoll()
  if (up) void wakeUserChat()
}

/**
 * Push owns freshness while it is up; the timer only runs for the window where
 * a thread is open AND the bridge is not (design doc 25 §8.0 exclusivity). A
 * channel that drops later re-arms it, which is the point: the degraded path
 * must still converge, just slower.
 */
function syncPoll(): void {
  if (threadOnScreen && !pushChannelUp) startPoll()
  else stopPoll()
}

function startPoll(): void {
  if (pollTimer) return
  pollTimer = setInterval(() => void wakeUserChat(), USER_CHAT_POLL_MS)
}

function stopPoll(): void {
  if (pollTimer) {
    clearInterval(pollTimer)
    pollTimer = null
  }
}
