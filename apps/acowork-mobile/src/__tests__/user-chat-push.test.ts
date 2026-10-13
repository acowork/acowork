/**
 * The mobile half of the user-chat wake-up path (design doc 25 §8).
 *
 * Freshness now arrives as an EMPTY doorbell on `acowork/users/{me}/chat/changed`
 * (`lib/presence.ts`), and the reaction is always an HTTP re-read. That makes
 * two things load-bearing, and both are store rules rather than UI behaviour:
 *
 *   1. `planPage` — the re-read must not move the thread the reader is looking
 *      at, so a page that differs from the window by `d` new tail items is an
 *      append, and only an unreconstructable delta replaces the window;
 *   2. push/poll exclusivity — while the channel is up the 5 s fallback timer
 *      stands down entirely, and it re-arms the moment the channel drops.
 *
 * The Desktop carries the same rule in its own store
 * (`src/stores/userChatStore.test.ts` in acowork-desktop): the two clients read
 * one endpoint and one signal, so these mirrored cases are the drift guard —
 * change the delta arithmetic on one side and the other side's copy goes red.
 *
 * `lib/api` is the seam: the store talks to the Gateway through it. Only the
 * id derivation stays real, because a flipped `min__max` pair would file one
 * conversation under two ids.
 */
import { describe, expect, it, vi, beforeEach, afterEach } from 'vitest'

const api = vi.hoisted(() => ({
  fetchUserChats: vi.fn(async (_me: string): Promise<unknown[]> => []),
  fetchUserMessages: vi.fn(
    async (_me: string, _chatId: string, _limit?: number) =>
      ({ messages: [] as unknown[], total: null as number | null }),
  ),
  sendUserMessage: vi.fn(async (_me: string, _chatId: string, _body: string) => ({}) as unknown),
  markUserChatRead: vi.fn(async (_me: string, _chatId: string) => {}),
}))

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, ...api }
})

import { userChatId } from '../lib/api'
import { useAuthStore } from '../stores/authStore'
import {
  USER_CHAT_POLL_MS,
  planPage,
  setChatPushChannel,
  setUserChatThreadOnScreen,
  useUserChatStore,
  wakeUserChat,
} from '../stores/userChatStore'
import type { AccountMe, UserChatMessage, UserChatSummary } from '../lib/types'

const ME = 'bbbbbbbb-0000-4000-8000-000000000002'
const PEER = 'aaaaaaaa-0000-4000-8000-000000000001'
const CHAT = `${PEER}__${ME}`

const me: AccountMe = {
  user_id: ME,
  username: 'bob',
  display_name: 'Bob',
  role: 'user',
  language: 'zh-CN',
  timezone: 'UTC',
}

function m(ts: number, body: string, from = PEER): UserChatMessage {
  return { ts, from, kind: 'text', body }
}

const M1 = m(1, 'one')
const M2 = m(2, 'two')
const M3 = m(3, 'three')
const M4 = m(4, 'four')

function summary(unread: number): UserChatSummary {
  return {
    chat_id: CHAT,
    peer_user_id: PEER,
    peer_display_name: 'Alice',
    last_active_at: 10,
    last_message_preview: 'hi',
    unread_count: unread,
  }
}

/**
 * Serve `GET …/messages` from a queue of pages (the last one repeats). The
 * message objects are shared between pages on purpose: reference identity is
 * what the tail-append rule hands to React, and a fresh object per fetch would
 * make "nothing re-rendered" unmeasurable.
 */
function servePages(pages: Array<{ messages: UserChatMessage[]; total: number | null }>) {
  let served = 0
  let fail = false
  api.fetchUserMessages.mockImplementation(async () => {
    if (fail) throw new Error('gateway is restarting')
    const p = pages[Math.min(served, pages.length - 1)]!
    served += 1
    return { messages: p.messages, total: p.total }
  })
  return {
    setFail(v: boolean) {
      fail = v
    },
    reads: () => served,
  }
}

function clearState() {
  useUserChatStore.setState({
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
  })
}

beforeEach(() => {
  vi.useRealTimers()
  setUserChatThreadOnScreen(false)
  setChatPushChannel(false)
  api.fetchUserChats.mockReset()
  api.fetchUserMessages.mockReset()
  api.sendUserMessage.mockReset()
  api.markUserChatRead.mockReset()
  api.fetchUserChats.mockImplementation(async () => [summary(0)])
  api.markUserChatRead.mockImplementation(async () => {})
  servePages([{ messages: [], total: 0 }])
  useAuthStore.setState({ me })
  clearState()
})

afterEach(() => {
  setUserChatThreadOnScreen(false)
  setChatPushChannel(false)
  vi.useRealTimers()
})

describe('planPage — deprecated total-delta fallback (no `seq`)', () => {
  // These messages carry no `seq`, so `planPage` takes the legacy branch a
  // pre-ADR-092 Gateway is served by — the rule Desktop mirrors exactly.
  it('does nothing at all when the count did not move', () => {
    expect(planPage([M1, M2], [M1, M2], { knownTotal: 2, total: 2 })).toEqual({ kind: 'none' })
  })

  it("appends only the newest delta, and the appended items ARE the page's", () => {
    const plan = planPage([M1, M2], [M1, M2, M3, M4], { knownTotal: 2, total: 4 })
    expect(plan).toEqual({ kind: 'append', tail: [M3, M4] })
    if (plan.kind !== 'append') throw new Error('unreachable')
    expect(plan.tail[0]).toBe(M3)
    expect(plan.tail[1]).toBe(M4)
  })

  it('resyncs when the gap is bigger than one page, and when the thread shrank', () => {
    expect(planPage([M1, M2], [M3, M4], { knownTotal: 2, total: 60 })).toEqual({
      kind: 'resync',
      messages: [M3, M4],
      unseen: 58,
    })
    expect(planPage([M1, M2, M3, M4], [M2], { knownTotal: 5, total: 3 })).toEqual({
      kind: 'resync',
      messages: [M2],
      unseen: 0,
    })
  })

  it('replaces on an unknown count, but only when the content differs', () => {
    // `undefined` never reaches here: `fetchUserMessages` normalizes a missing
    // `total` to null. Letting NaN through would fall past every comparison and
    // append the whole page on top of the window — a duplicated transcript.
    expect(planPage([M1, M2], [M1, M2], { knownTotal: 2, total: null })).toEqual({ kind: 'none' })
    expect(planPage([M1], [M1, M2], { knownTotal: 2, total: null })).toEqual({
      kind: 'resync',
      messages: [M1, M2],
      unseen: 0,
    })
    expect(planPage([M1, M2], [M1, M2], { knownTotal: null, total: 2 })).toEqual({ kind: 'none' })
  })

  it('counts a same-length page of different objects as a change', () => {
    expect(planPage([m(1, 'one')], [m(1, 'one')], { knownTotal: 1, total: null }).kind).toBe(
      'resync',
    )
  })
})

describe('planPage — seq suffix-diff (ADR-092 decision 2)', () => {
  // Seq-bearing messages: content judgement, not a count guess. Mirrors Desktop.
  function s(seq: number, body: string, from = PEER) {
    return { seq, ts: seq, from, kind: 'text', body }
  }
  const S1 = s(1, 'one')
  const S2 = s(2, 'two')
  const S3 = s(3, 'three')

  it('appends the suffix past heldMaxSeq and keeps the held references', () => {
    const plan = planPage([S1, S2], [S1, S2, S3], { knownTotal: null, total: 3 })
    expect(plan).toEqual({ kind: 'append', tail: [S3] })
    if (plan.kind === 'append') expect(plan.tail[0]).toBe(S3)
  })

  it('is none when nothing is past heldMaxSeq — even if total jumped', () => {
    expect(planPage([S1, S2], [S1, S2], { knownTotal: 2, total: 60 })).toEqual({ kind: 'none' })
  })

  it('fills on the first load: a whole page is appended with no hole', () => {
    expect(planPage([], [S1, S2, S3], { knownTotal: null, total: 3 })).toEqual({
      kind: 'append',
      tail: [S1, S2, S3],
    })
  })

  it('resyncs on a hole and counts unseen from ackedSeq', () => {
    const page = [s(5, 'five'), s(6, 'six')]
    expect(planPage([S1, S2], page, { knownTotal: 6, total: 6 }, 2)).toEqual({
      kind: 'resync',
      messages: page,
      unseen: 2,
    })
  })
})

describe('signal-driven reload', () => {
  it('appends the arrival and leaves the already-held objects alone', async () => {
    servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2, M3], total: 3 }])
    await useUserChatStore.getState().openChat(PEER)
    const before = useUserChatStore.getState().messages
    expect(before.map((x) => x.body)).toEqual(['one', 'two'])

    await wakeUserChat()

    const s = useUserChatStore.getState()
    expect(s.messages.map((x) => x.body)).toEqual(['one', 'two', 'three'])
    // The reader's position rests on these two references not changing.
    expect(s.messages[0]).toBe(before[0])
    expect(s.messages[1]).toBe(before[1])
    expect(s.messagesTotal).toBe(3)
    expect(s.unseenCount).toBe(1) // the pill; cleared by the screen iff pinned
  });

  it('touches no state when the doorbell turned out to be about another thread', async () => {
    // The signal is content-free, so this is the common case, not an edge one.
    servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2], total: 2 }])
    await useUserChatStore.getState().openChat(PEER)
    const before = useUserChatStore.getState().messages

    await wakeUserChat()

    const s = useUserChatStore.getState()
    expect(s.messages).toBe(before) // not a copy: no re-render, no scroll
    expect(s.unseenCount).toBe(0)
  })

  it('resyncs a gap larger than a page and counts the rebuild honestly', async () => {
    const page = [M1, M2, M3, M4]
    servePages([{ messages: [M1, M2], total: 2 }, { messages: page, total: 60 }])
    await useUserChatStore.getState().openChat(PEER)

    await wakeUserChat()

    const s = useUserChatStore.getState()
    expect(s.messages).toBe(page)
    expect(s.messagesTotal).toBe(60)
    // No "how many did I miss" survives a rebuilt window; the count is the
    // delta the server reported, not the page length.
    expect(s.unseenCount).toBe(58)
  })

  it('treats a page with no count as a replacement, never as a delta', async () => {
    servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2, M3], total: null }])
    await useUserChatStore.getState().openChat(PEER)

    await wakeUserChat()

    const s = useUserChatStore.getState()
    expect(s.messages.map((x) => x.body)).toEqual(['one', 'two', 'three'])
    expect(s.messagesTotal).toBeNull()
  })

  it('keeps showing the window when a re-read fails', async () => {
    const srv = servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2, M3], total: 3 }])
    await useUserChatStore.getState().openChat(PEER)
    const before = useUserChatStore.getState().messages

    srv.setFail(true)
    await wakeUserChat()
    // A transient failure keeps showing what we have: the next signal re-reads,
    // and blanking a thread that rendered fine is worse than one beat late.
    expect(useUserChatStore.getState().messages).toBe(before)
  })

  it('counts the sent message, so its own doorbell does not append it twice', async () => {
    const sent = m(3, 'hello', ME)
    servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2, sent], total: 3 }])
    api.sendUserMessage.mockImplementation(async () => sent)

    await useUserChatStore.getState().openChat(PEER)
    expect(await useUserChatStore.getState().send('hello')).toBe(true)

    const afterSend = useUserChatStore.getState()
    expect(afterSend.messages.map((x) => x.body)).toEqual(['one', 'two', 'hello'])
    expect(afterSend.messagesTotal).toBe(3) // 2 held + the echo we just took

    await wakeUserChat()

    const s = useUserChatStore.getState()
    expect(s.messages.map((x) => x.body)).toEqual(['one', 'two', 'hello'])
    expect(s.messages.filter((x) => x.body === 'hello')).toHaveLength(1)
    expect(s.unseenCount).toBe(0)
  })

  it('never lets another thread\'s count travel with the window', async () => {
    servePages([{ messages: [M1, M2], total: 2 }])
    await useUserChatStore.getState().openChat(PEER)
    expect(useUserChatStore.getState().messagesTotal).toBe(2)

    await useUserChatStore.getState().openChat('cccccccc-0000-4000-8000-000000000003')
    // A fresh thread starts from "the server has never told us", so its first
    // page cannot read as a negative delta and force a resync.
    expect(useUserChatStore.getState().unseenCount).toBe(0)

    useUserChatStore.getState().closeChat()
    expect(useUserChatStore.getState().messagesTotal).toBeNull()
  })

  it('drops a page that lands after the reader switched thread', async () => {
    // `messagesTotal` is per-thread: a late answer for the old peer must not
    // set the new one's count.
    const other = 'cccccccc-0000-4000-8000-000000000003'
    type Page = { messages: UserChatMessage[]; total: number | null }
    // A holder object rather than a `let`: TS narrows a closure-assigned
    // variable to `never` after the declaration site, so `release()` below
    // would not even typecheck.
    const deferred: { resolve?: (p: Page) => void } = {}
    api.fetchUserMessages.mockImplementation(
      () =>
        new Promise<Page>((resolve) => {
          deferred.resolve = resolve
        }),
    )
    useUserChatStore.setState({ activePeerId: PEER })
    const pending = useUserChatStore.getState().refreshMessages()
    await vi.waitFor(() => expect(api.fetchUserMessages).toHaveBeenCalled())

    // The reader walks away while the request is in flight.
    useUserChatStore.setState({ activePeerId: other, messages: [], messagesTotal: null })
    deferred.resolve?.({ messages: [M1, M2], total: 2 })
    await pending

    expect(useUserChatStore.getState().messages).toEqual([])
    expect(useUserChatStore.getState().messagesTotal).toBeNull()
  })
})

describe('wake() coalescing', () => {
  it('folds a burst of doorbells into one follow-up round', async () => {
    servePages([{ messages: [M1], total: 1 }, { messages: [M1, M2], total: 2 }])
    await useUserChatStore.getState().openChat(PEER)
    api.fetchUserChats.mockClear()
    servePages([{ messages: [M1], total: 1 }, { messages: [M1, M2], total: 2 }])

    // One send from another device emits two signals (message + read cursor),
    // so a burst is the normal case; three rounds of reads would be three.
    await Promise.all([wakeUserChat(), wakeUserChat(), wakeUserChat()])

    expect(api.fetchUserChats).toHaveBeenCalledTimes(2)
    expect(useUserChatStore.getState().messages.map((x) => x.body)).toEqual(['one', 'two'])
  })

  it('leaves the merge state clean, so the next signal is not swallowed', async () => {
    servePages([
      { messages: [M1], total: 1 },
      { messages: [M1, M2], total: 2 },
      { messages: [M1, M2, M3], total: 3 },
    ])
    await useUserChatStore.getState().openChat(PEER)

    await Promise.all([wakeUserChat(), wakeUserChat()])
    await wakeUserChat()

    expect(useUserChatStore.getState().messages.map((x) => x.body)).toEqual([
      'one',
      'two',
      'three',
    ])
  })

  it('reads the inbox even with no thread open', async () => {
    // A first message in a brand-new conversation is only discoverable through
    // the chat list, which is why `wake` never skips that half.
    api.fetchUserChats.mockClear()
    const srv = servePages([{ messages: [M1], total: 1 }])

    await wakeUserChat()

    expect(api.fetchUserChats).toHaveBeenCalledTimes(1)
    expect(srv.reads()).toBe(0)
  })
})

describe('push channel vs the fallback poll', () => {
  it('runs the timer only while the thread is on screen and the channel is down', async () => {
    api.fetchUserChats.mockClear()
    servePages([{ messages: [M1], total: 1 }])
    await useUserChatStore.getState().openChat(PEER)
    api.fetchUserChats.mockClear()

    // No thread on screen, no push: nothing polls.
    setUserChatThreadOnScreen(false)
    setChatPushChannel(false)
    vi.useFakeTimers()
    await vi.advanceTimersByTimeAsync(USER_CHAT_POLL_MS * 3)
    expect(api.fetchUserChats).not.toHaveBeenCalled()

    // The thread opens: the degraded path takes over.
    setUserChatThreadOnScreen(true)
    await vi.advanceTimersByTimeAsync(USER_CHAT_POLL_MS * 2)
    expect(api.fetchUserChats).toHaveBeenCalledTimes(2)

    // The MQTT channel subscribes. The down→up edge fires exactly one catch-up
    // re-read (ADR-092 decision 6 folded it into `setChatPushChannel`); once it
    // settles, no timer tick follows while push owns freshness.
    api.fetchUserChats.mockClear()
    setChatPushChannel(true)
    await vi.advanceTimersByTimeAsync(0) // let the catch-up wake settle
    expect(api.fetchUserChats).toHaveBeenCalledTimes(1) // the one edge read
    api.fetchUserChats.mockClear()
    await vi.advanceTimersByTimeAsync(USER_CHAT_POLL_MS * 3)
    expect(api.fetchUserChats).not.toHaveBeenCalled() // no tick behind push

    // An explicit signal still gets through (the store's own `wake` is not gated).
    await wakeUserChat()
    expect(api.fetchUserChats).toHaveBeenCalledTimes(1)

    // The channel drops: the timer re-arms, because the degraded path still
    // has to converge — just slower.
    api.fetchUserChats.mockClear()
    setChatPushChannel(false)
    await vi.advanceTimersByTimeAsync(USER_CHAT_POLL_MS)
    expect(api.fetchUserChats).toHaveBeenCalledTimes(1)
  })

  it('down→up reads once; up→up is a no-op — the edge invariant', async () => {
    // The transfer-table contract (ADR-092 decision 6). `lib/presence.ts`
    // dedupes on its side, and the store now dedupes too: the first up-edge
    // fires the single catch-up read, the identical second report must not.
    api.fetchUserChats.mockClear()
    setUserChatThreadOnScreen(true)
    vi.useFakeTimers()
    setChatPushChannel(true)
    setChatPushChannel(true)
    await vi.advanceTimersByTimeAsync(0) // let the single catch-up wake settle
    expect(api.fetchUserChats).toHaveBeenCalledTimes(1)
    await vi.advanceTimersByTimeAsync(USER_CHAT_POLL_MS * 2)
    expect(api.fetchUserChats).toHaveBeenCalledTimes(1) // and no timer behind push
  })
})

describe('markSeen', () => {
  it('acks only when there is something unseen, and refreshes the badge', async () => {
    servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2, M3], total: 3 }])
    api.fetchUserChats.mockClear()
    await useUserChatStore.getState().openChat(PEER)
    api.markUserChatRead.mockClear()

    // Opening the thread already acked it; nothing is unseen.
    await useUserChatStore.getState().markSeen()
    expect(api.markUserChatRead).not.toHaveBeenCalled()

    await wakeUserChat()
    expect(useUserChatStore.getState().unseenCount).toBe(1)

    api.fetchUserChats.mockClear()
    await useUserChatStore.getState().markSeen()

    expect(api.markUserChatRead).toHaveBeenCalledWith(ME, CHAT)
    expect(useUserChatStore.getState().unseenCount).toBe(0)
    await vi.waitFor(() => expect(api.fetchUserChats).toHaveBeenCalled())
  })

  it('keeps the badge cleared locally when the Gateway refuses the ack', async () => {
    servePages([{ messages: [M1, M2], total: 2 }, { messages: [M1, M2, M3], total: 3 }])
    await useUserChatStore.getState().openChat(PEER)
    await wakeUserChat()
    api.markUserChatRead.mockRejectedValue(new Error('boom'))

    await useUserChatStore.getState().markSeen()

    // A refused receipt is not the reader's problem: the next signal re-reads
    // the server's count, and a banner here would blank a thread that loaded
    // fine.
    expect(useUserChatStore.getState().unseenCount).toBe(0)
    expect(useUserChatStore.getState().error).toBeNull()
  })
})

describe('the thread id contract', () => {
  it('is order-independent, so a thread cannot split in two', () => {
    expect(userChatId(ME, PEER)).toBe(CHAT)
    expect(userChatId(PEER, ME)).toBe(CHAT)
  })

  it('addresses the Gateway as the signed-in account, never the peer', async () => {
    servePages([{ messages: [M1], total: 1 }])
    await useUserChatStore.getState().openChat(PEER)
    const call = api.fetchUserMessages.mock.calls[0]!
    expect(call[0]).toBe(ME)
    expect(call[1]).toBe(CHAT)
  })
})
