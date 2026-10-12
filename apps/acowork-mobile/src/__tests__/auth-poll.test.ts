/**
 * v1.1 §7/§8 behavior tests: the 401 ladder, the boot state machine, the
 * conversation mapping, and the poll loop's reload rule. All against fake
 * transports — no HTTP, no timers left running.
 */
import { describe, it, expect, beforeEach, afterEach, vi } from 'vitest'
import {
  mapConversationEntries,
  mapSessionRow,
  extractStatus,
  type AuthBridge,
} from '../lib/api'
import type { ChatMessage, ConversationEntry, LiveStatus, SessionDetail, SessionRow } from '../lib/types'

/* ---------------- api: mapping ---------------- */

describe('mapConversationEntries', () => {
  const e = (over: Partial<ConversationEntry> & { id: string; role: ConversationEntry['role'] }): ConversationEntry => ({
    ts: '2026-01-01T00:00:00.000Z',
    content: 'x',
    ...over,
  })

  it('keeps user/assistant/thought/tool_call, drops system/compaction/internal', () => {
    const out = mapConversationEntries([
      e({ id: '1', role: 'user', content: 'hi' }),
      e({ id: '2', role: 'thought', content: 'hmm' }),
      e({ id: '3', role: 'assistant', content: 'yo' }),
      e({ id: '4', role: 'system', content: 'sys' }),
      e({ id: '5', role: 'assistant', content: 'c', kind: 'compaction' }),
      e({ id: '6', role: 'assistant', content: 'i', metadata: { internal: true } }),
    ])
    expect(out.map((m) => m.id)).toEqual(['1', '2', '3'])
  })

  it('parses ISO ts to epoch ms and never NaN', () => {
    const out = mapConversationEntries([e({ id: '1', role: 'user', ts: 'not-a-date' })])
    expect(out[0]!.created_at).toBe(0)
  })

  it('folds tool_call into an assistant row with the tool name', () => {
    const out = mapConversationEntries([
      e({ id: '1', role: 'tool_call', content: '{}', metadata: { tool_name: 'file_read' } }),
    ])
    expect(out).toHaveLength(1)
    expect((out[0]!.payload as { name: string }).name).toBe('file_read')
  })
})

describe('mapSessionRow', () => {
  const row: SessionRow = {
    session_id: 's1',
    title: null,
    created_at: '2026-01-01T00:00:00Z',
    last_active_at: '2026-01-02T03:04:05Z',
    message_count: 3,
    can_write: false,
  }
  it('carries can_write through untouched and defaults title', () => {
    const m = mapSessionRow(row)
    expect(m.can_write).toBe(false)
    expect(m.title).toBe('新会话')
    expect(m.updated_at).toBe(Date.parse('2026-01-02T03:04:05Z'))
  })
})

describe('extractStatus', () => {
  const d = (live: unknown): SessionDetail =>
    ({ session_id: 's', meta: { session_id: 's', created_at: '', last_active_at: '', message_count: 0 }, live_state: live } as SessionDetail)
  it('null when no live_state', () => expect(extractStatus(d(null))).toBeNull())
  it('null when status missing', () => expect(extractStatus(d({}))).toBeNull())
  it('passes the tagged object through', () => {
    const s = extractStatus(d({ status: { status: 'waiting_approval', detail: { request_id: 'r1' } } }))
    expect(s).toEqual({ status: 'waiting_approval', detail: { request_id: 'r1' } })
  })
})

/* ---------------- authStore: boot + 401 ladder ---------------- */

// The store installs its bridge at import; we replace fetch wholesale.
const fetchMock = vi.fn()
beforeEach(() => {
  fetchMock.mockReset()
  vi.stubGlobal('fetch', fetchMock)
  localStorage.clear()
})

function jsonRes(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } })
}

async function freshAuth() {
  vi.resetModules()
  return (await import('../stores/authStore')).useAuthStore
}

describe('authStore.boot', () => {
  it('no saved url → connect', async () => {
    const useAuthStore = await freshAuth()
    await useAuthStore.getState().boot()
    expect(useAuthStore.getState().phase).toBe('connect')
  })

  it('saved url, no tokens → probe → login', async () => {
    localStorage.setItem('acowork.gatewayUrl', 'http://g:19876')
    fetchMock.mockResolvedValue(jsonRes({ version: '1', agents_installed: 0, agents_running: 0, uptime_secs: 0, mqtt_port: 1, auth_mode: 'multi_user', registration_open: false, requires_setup: false }))
    const useAuthStore = await freshAuth()
    await useAuthStore.getState().boot()
    expect(useAuthStore.getState().phase).toBe('login')
  })

  it('requires_setup → restricted', async () => {
    localStorage.setItem('acowork.gatewayUrl', 'http://g:19876')
    fetchMock.mockResolvedValue(jsonRes({ version: '1', agents_installed: 0, agents_running: 0, uptime_secs: 0, mqtt_port: 1, auth_mode: 'multi_user', registration_open: false, requires_setup: true }))
    const useAuthStore = await freshAuth()
    await useAuthStore.getState().boot()
    expect(useAuthStore.getState().phase).toBe('restricted')
  })

  it('unreachable gateway → disconnected', async () => {
    localStorage.setItem('acowork.gatewayUrl', 'http://g:19876')
    fetchMock.mockRejectedValue(new Error('down'))
    const useAuthStore = await freshAuth()
    await useAuthStore.getState().boot()
    expect(useAuthStore.getState().phase).toBe('disconnected')
  })

  // A network switch used to leave the probe fetch hanging at the OS TCP layer
  // for ~20-30s before the reconnect UI appeared. The probe must now carry an
  // explicit timeout signal so `disconnected` fires within seconds.
  it('wires a timeout signal into the boot probe fetch', async () => {
    localStorage.setItem('acowork.gatewayUrl', 'http://g:19876')
    fetchMock.mockResolvedValue(jsonRes({ version: '1', agents_installed: 0, agents_running: 0, uptime_secs: 0, mqtt_port: 1, auth_mode: 'multi_user', registration_open: false, requires_setup: false }))
    const useAuthStore = await freshAuth()
    await useAuthStore.getState().boot()
    const init = fetchMock.mock.calls[0]?.[1] as RequestInit | undefined
    expect(init?.signal).toBeInstanceOf(AbortSignal)
  })

  it('a probe that times out (aborted) → disconnected', async () => {
    localStorage.setItem('acowork.gatewayUrl', 'http://g:19876')
    fetchMock.mockRejectedValue(new DOMException('The operation was aborted due to timeout', 'TimeoutError'))
    const useAuthStore = await freshAuth()
    await useAuthStore.getState().boot()
    expect(useAuthStore.getState().phase).toBe('disconnected')
  })
})

describe('401 ladder', () => {
  it('refreshes once and replays; parallel 401s share the rotation', async () => {
    vi.resetModules()
    const api = await import('../lib/api')
    let access = 'A0'
    // The bridge mock carries the SAME single-flight invariant as authStore:
    // concurrent callers share one in-flight rotation.
    let inflight: Promise<string> | null = null
    const rotate = async (): Promise<string> => {
      await new Promise((r) => setTimeout(r, 5))
      access = 'A1'
      return 'A1'
    }
    const bridge: AuthBridge = {
      baseUrl: () => 'http://g',
      accessToken: () => access,
      refresh: vi.fn(async () => {
        inflight ??= rotate().finally(() => (inflight = null))
        return inflight
      }),
      sessionExpired: vi.fn(),
    }
    api.setAuthBridge(bridge)

    const seen: string[] = []
    fetchMock.mockImplementation((_url: string, init?: RequestInit) => {
      const h = new Headers(init?.headers)
      const tok = h.get('Authorization')
      seen.push(tok ?? 'none')
      // 401 the FIRST request only — every later request must succeed with
      // the token the ladder replays (the CURRENT one, not a captured one).
      if (tok === 'Bearer A0' && seen.length === 1) return Promise.resolve(jsonRes({ error: 'expired' }, 401))
      return Promise.resolve(jsonRes([{ instance_id: 'i', agent_id: 'a', name: 'n', alive: true, lifecycle: 'http_ready' }]))
    })

    const [r1, r2] = await Promise.all([api.fetchAgents(), api.fetchAgents()])
    expect(r1).toHaveLength(1)
    expect(r2).toHaveLength(1)
    expect(bridge.refresh).toHaveBeenCalledTimes(1) // single-flight
    expect(seen).toEqual(['Bearer A0', 'Bearer A0', 'Bearer A1'])
  })

  it('failed rotation ends the session and never replays', async () => {
    vi.resetModules()
    const api = await import('../lib/api')
    const sessionExpired = vi.fn()
    api.setAuthBridge({
      baseUrl: () => 'http://g',
      accessToken: () => 'A0',
      refresh: vi.fn(async () => null),
      sessionExpired,
    })
    fetchMock.mockResolvedValue(jsonRes({ error: 'expired' }, 401))
    await expect(api.fetchAgents()).rejects.toThrow(/401/)
    expect(sessionExpired).toHaveBeenCalledTimes(1)
    expect(fetchMock).toHaveBeenCalledTimes(1) // no replay after failed refresh
  })
})

/* ---------------- chatStore: poll loop reload rule ---------------- */

import { useChatStore, setChatTransport, isActiveStatus, POLL_INTERVAL_MS, type ChatTransport } from '../stores/chatStore'
import { useAgentStore } from '../stores/agentStore'
import type { SessionInfo } from '../lib/types'

function fakeTransport(over: Partial<ChatTransport> = {}): ChatTransport {
  return {
    openSession: vi.fn(async () => {}),
    fetchMessages: vi.fn(async () => []),
    fetchSessions: vi.fn(async () => ({ items: [], hasMore: false })),
    createSession: vi.fn(async () => ({ session_id: 'new', title: 'new' }) as SessionInfo),
    deleteSession: vi.fn(async () => {}),
    fetchSnapshot: vi.fn(async () => ({ status: null, messageCount: null })),
    send: vi.fn(async () => 'm1'),
    approval: vi.fn(async () => {}),
    answer: vi.fn(async () => {}),
    ...over,
  }
}

beforeEach(() => {
  useAgentStore.setState({ agents: { a1: { info: { id: 'a1', name: 'A', node_id: 'n1' }, sessions: [{ session_id: 's1', title: 'S', can_write: true }] } }, agentList: [{ id: 'a1', name: 'A', node_id: 'n1' }], users: [] })
  useChatStore.setState({ agentStates: {}, selectedAgentId: null})
})

afterEach(() => {
  useChatStore.getState().stopWatching()
})

describe('poll loop', () => {
  it('active → idle reloads the complete history once', async () => {
    let status: LiveStatus | null = { status: 'thinking' }
    const t = fakeTransport({
      fetchSnapshot: vi.fn(async () => ({ status, messageCount: null })),
      fetchMessages: vi.fn(async (): Promise<ChatMessage[]> => [{ id: 'm1', role: 'assistant', content: 'final', created_at: 1, kind: 'text' }]),
    })
    setChatTransport(t)
    await useChatStore.getState().openSession('a1', 's1')
    useChatStore.setState({ agentStates: { a1: { activeSessionId: 's1', messages: [], loading: false, loaded: true, status: { status: 'thinking' }, loadedCount: null, pendingSend: null, channel: null, question: null, approvalDetail: null } } })

    status = { status: 'idle' }
    useChatStore.getState().startWatching('a1', 's1')
    await new Promise((r) => setTimeout(r, 50))

    expect(t.fetchMessages).toHaveBeenCalled()
    expect(useChatStore.getState().agentStates.a1?.messages.map((m) => m.content)).toEqual(['final'])
    expect(isActiveStatus(useChatStore.getState().agentStates.a1?.status ?? null)).toBe(false)
  })

  it('stays quiet while active (no reload mid-turn)', async () => {
    const t = fakeTransport({ fetchSnapshot: vi.fn(async () => ({ status: { status: 'tool_executing' } as never, messageCount: null })) })
    setChatTransport(t)
    useChatStore.setState({ agentStates: { a1: { activeSessionId: 's1', messages: [], loading: false, loaded: true, status: null, loadedCount: null, pendingSend: null, channel: null, question: null, approvalDetail: null } } })
    useChatStore.getState().startWatching('a1', 's1')
    await new Promise((r) => setTimeout(r, 50))
    expect(t.fetchMessages).not.toHaveBeenCalled()
    expect(isActiveStatus(useChatStore.getState().agentStates.a1?.status ?? null)).toBe(true)
  })

  it('fast reply: idle → idle with a changed message_count still reloads', async () => {
    // The race the transition-only rule missed: the turn starts and finishes
    // between two ticks, so no active → idle edge is ever observed. The
    // authoritative count moving 1 → 3 is the reload trigger.
    let count = 1
    const t = fakeTransport({
      fetchSnapshot: vi.fn(async () => ({ status: { status: 'idle' } as never, messageCount: count })),
      fetchMessages: vi.fn(async (): Promise<ChatMessage[]> => [
        { id: 'u1', role: 'user', content: 'ping', created_at: 1, kind: 'text' },
        { id: 'a1', role: 'assistant', content: 'pong', created_at: 2, kind: 'text' },
      ]),
    })
    setChatTransport(t)
    useChatStore.setState({
      agentStates: { a1: { activeSessionId: 's1', messages: [], loading: false, loaded: true, status: null, loadedCount: 1, pendingSend: null, channel: null, question: null, approvalDetail: null } },
    })
    useChatStore.getState().startWatching('a1', 's1')
    await new Promise((r) => setTimeout(r, 50))
    expect(t.fetchMessages).not.toHaveBeenCalled() // first tick: count matches baseline

    count = 3
    await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS + 50))
    expect(t.fetchMessages).toHaveBeenCalled()
    expect(useChatStore.getState().agentStates.a1?.messages.map((m) => m.content)).toEqual(['ping', 'pong'])
    expect(useChatStore.getState().agentStates.a1?.loadedCount).toBe(3)
  })

  it('sendMessage: success appends optimistic bubble, failure keeps the draft', async () => {
    const okT = fakeTransport()
    setChatTransport(okT)
    const ok = await useChatStore.getState().sendMessage('a1', 's1', 'hello')
    expect(ok).toBe(true)
    expect(okT.send).toHaveBeenCalledWith('a1', 's1', 'hello')
    expect(useChatStore.getState().agentStates.a1?.pendingSend).toBeNull()
    expect(useChatStore.getState().agentStates.a1?.messages.at(-1)?.content).toBe('hello')

    const badT = fakeTransport({ send: vi.fn(async () => { throw new Error('net') }) })
    setChatTransport(badT)
    const bad = await useChatStore.getState().sendMessage('a1', 's1', 'retry me')
    expect(bad).toBe(false)
    expect(useChatStore.getState().agentStates.a1?.pendingSend).toEqual({ content: 'retry me', state: 'failed' })

    // retry without content reuses the failed draft
    setChatTransport(okT)
    const retried = await useChatStore.getState().sendMessage('a1', 's1')
    expect(retried).toBe(true)
    expect(okT.send).toHaveBeenLastCalledWith('a1', 's1', 'retry me')
  })

  it('openSession clears the previous session status', async () => {
    setChatTransport(fakeTransport())
    useChatStore.setState({ agentStates: { a1: { activeSessionId: 's0', messages: [], loading: false, loaded: true, status: { status: 'thinking' }, loadedCount: null, pendingSend: { content: 'x', state: 'sending' }, channel: null, question: null, approvalDetail: null } } })
    await useChatStore.getState().openSession('a1', 's1')
    const v = useChatStore.getState().agentStates.a1!
    expect(v.status).toBeNull()
    expect(v.pendingSend).toBeNull()
  })
})
