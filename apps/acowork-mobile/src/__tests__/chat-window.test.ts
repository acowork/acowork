/**
 * ADR-050 window-model tests for the mobile chat store (the Desktop
 * pagination port): cursor merge math, the non-contiguous replace rule,
 * and the scroll-up `loadEarlier` action. Pure store — no renderer, no
 * timers (the poll loop is covered in auth-poll.test.ts).
 */
import { describe, expect, it, vi } from 'vitest'
import {
  useChatStore,
  setChatTransport,
  applyWindow,
  mergeMessageWindow,
  emptyView,
  type ChatTransport,
} from '../stores/chatStore'
import { useAgentStore } from '../stores/agentStore'
import type { ChatMessage, MessagesWindow } from '../lib/types'

function msg(id: string, at: number, content = `m${id}`): ChatMessage {
  return { id, role: 'assistant', content, created_at: at, kind: 'text' }
}

function viewOf(over: Partial<ReturnType<typeof emptyView>> = {}) {
  return { ...emptyView(), activeSessionId: 's1', ...over }
}

function transportReturning(w: MessagesWindow): ChatTransport {
  return {
    openSession: vi.fn(async () => {}),
    fetchMessages: vi.fn(async () => w),
    fetchSessions: vi.fn(async () => ({ items: [], hasMore: false })),
    createSession: vi.fn(async () => ({ session_id: 'n', title: 'n' })),
    deleteSession: vi.fn(async () => {}),
    fetchSnapshot: vi.fn(async () => ({ status: null, messageCount: null })),
    send: vi.fn(async () => 'x'),
    approval: vi.fn(async () => {}),
    answer: vi.fn(async () => {}),
  }
}

describe('mergeMessageWindow', () => {
  it('server wins on id collision, cache-only rows survive, result is time-ordered', () => {
    const cache = [msg('a', 1, 'old-a'), msg('b', 2, 'old-b')]
    const server = [msg('b', 2, 'server-b'), msg('c', 3, 'new-c')]
    const merged = mergeMessageWindow(cache, server)
    expect(merged.map((m) => m.id)).toEqual(['a', 'b', 'c'])
    expect(merged.find((m) => m.id === 'b')!.content).toBe('server-b')
  })

  it('collapses the optimistic echo into the server copy via the shared id', () => {
    // The send path stamps the optimistic bubble with the id the server
    // persists the user entry under — dedupe by id is what keeps ONE
    // bubble after a reload merges the authoritative window.
    const cache = [msg('u1', 5, 'hello')] // optimistic (id = sent message_id)
    const server = [msg('u1', 5, 'hello'), msg('a1', 6, 'hi back')]
    const merged = mergeMessageWindow(cache, server)
    expect(merged.map((m) => m.id)).toEqual(['u1', 'a1'])
  })
})

describe('applyWindow', () => {
  it('adopts the server window verbatim when the cache is empty', () => {
    const w: MessagesWindow = { messages: [msg('1', 1)], offset: 400, limit: 100, total: 500 }
    const out = applyWindow(viewOf(), w)
    expect(out.messages.map((m) => m.id)).toEqual(['1'])
    expect(out.messageOffset).toBe(400)
    expect(out.messageLimit).toBe(100)
  })

  it('merges contiguous windows and extends the cursor to the union', () => {
    // cached raw range [100,200), earlier page [50,150) → union [50,200)
    const view = viewOf({
      messages: [msg('x', 10), msg('y', 11)],
      messageOffset: 100,
      messageLimit: 100,
      messageTotal: 300,
    })
    const w: MessagesWindow = { messages: [msg('w', 5), msg('x', 10)], offset: 50, limit: 100, total: 300 }
    const out = applyWindow(view, w)
    expect(out.messageOffset).toBe(50)
    expect(out.messageLimit).toBe(150)
    expect(out.messages.map((m) => m.id)).toEqual(['w', 'x', 'y'])
  })

  it('REPLACES (never unions) across a gap — a sparse cache must not fake a [0,total) cursor', () => {
    // cached [0,100), a new tail at [500,600): the middle was never loaded.
    const view = viewOf({
      messages: [msg('old', 1)],
      messageOffset: 0,
      messageLimit: 100,
      messageTotal: 600,
    })
    const w: MessagesWindow = { messages: [msg('tail', 9)], offset: 500, limit: 100, total: 600 }
    const out = applyWindow(view, w)
    expect(out.messages.map((m) => m.id)).toEqual(['tail'])
    expect(out.messageOffset).toBe(500)
    expect(out.messageLimit).toBe(100)
  })
})

describe('loadEarlier', () => {
  it('fetches the page BEFORE the cursor, merges it in and walks the cursor back', async () => {
    useAgentStore.setState({ agents: { a1: { info: { id: 'a1', name: 'A', node_id: 'n1' }, sessions: [{ session_id: 's1', title: 'S', can_write: true }] } }, agentList: [], users: [] })
    const page: MessagesWindow = { messages: [msg('e1', 1), msg('e2', 2)], offset: 50, limit: 100, total: 250 }
    const t = transportReturning(page)
    setChatTransport(t)
    useChatStore.setState({
      agentStates: {
        a1: viewOf({
          messages: [msg('e2', 2), msg('e3', 3)],
          loadedCount: 250,
          messageOffset: 150,
          messageLimit: 100,
          messageTotal: 250,
        }),
      },
    })

    await useChatStore.getState().loadEarlier('a1')

    expect(t.fetchMessages).toHaveBeenCalledWith('a1', 's1', { offset: 50, limit: 100 })
    const v = useChatStore.getState().agentStates.a1!
    expect(v.messages.map((m) => m.id)).toEqual(['e1', 'e2', 'e3']) // ordered, deduped
    expect(v.messageOffset).toBe(50)
    expect(v.isLoadingMore).toBe(false)
  })

  it('is a no-op at the oldest end, without a cursor, or while a page is in flight', async () => {
    const t = transportReturning({ messages: [], offset: 0, limit: 0, total: 0 })
    setChatTransport(t)

    useChatStore.setState({ agentStates: { a1: viewOf({ messageOffset: 0, messageLimit: 100, messageTotal: 100 }) } })
    await useChatStore.getState().loadEarlier('a1')
    useChatStore.setState({ agentStates: { a1: viewOf({ messageOffset: null }) } })
    await useChatStore.getState().loadEarlier('a1')
    useChatStore.setState({ agentStates: { a1: viewOf({ messageOffset: 300, isLoadingMore: true }) } })
    await useChatStore.getState().loadEarlier('a1')

    expect(t.fetchMessages).not.toHaveBeenCalled()
  })

  it('clamps the last partial page to offset 0', async () => {
    const t = transportReturning({ messages: [msg('first', 1)], offset: 0, limit: 30, total: 130 })
    setChatTransport(t)
    useChatStore.setState({
      agentStates: {
        a1: viewOf({
          messages: [msg('second', 2)],
          messageOffset: 30,
          messageLimit: 100,
          messageTotal: 130,
        }),
      },
    })

    await useChatStore.getState().loadEarlier('a1')

    expect(t.fetchMessages).toHaveBeenCalledWith('a1', 's1', { offset: 0, limit: 100 })
    const v = useChatStore.getState().agentStates.a1!
    expect(v.messageOffset).toBe(0) // no earlier history left to page
    expect(v.messages.map((m) => m.id)).toEqual(['first', 'second'])
  })
})
