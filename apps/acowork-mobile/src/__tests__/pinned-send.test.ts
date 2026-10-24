/**
 * ADR-100 D12 — the mobile execution composer sends through the Chat entry
 * point when its session came from a Chat.
 *
 * Two layers are pinned, because the rule is split on purpose:
 * - the STORE decides whether a pin exists. It reads the binding off the
 *   server's session row (`SessionInfo.chat_session_id`) and requires a
 *   signed-in account, since the Chat channel is identity-bearing. It never
 *   invents a binding and never sends a pin it cannot name a sender for.
 * - the TRANSPORT decides what a pin means on the wire: the chat-message
 *   intent channel, carrying `for_execution_session` plus the client-minted
 *   `message_id` both surfaces reconcile on.
 *
 * `command` rides both paths, so a skill picked in the composer survives the
 * change of entry point.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest'
import {
  useChatStore,
  setChatTransport,
  emptyView,
  type ChatTransport,
} from '../stores/chatStore'
import { useAgentStore } from '../stores/agentStore'
import { useAuthStore } from '../stores/authStore'
import { sendMessage, setAuthBridge } from '../lib/api'
import type { MessagesWindow, SessionInfo } from '../lib/types'

const AGENT = 'a1'
const SESSION = 's1'
const CHAT = 'cs-1'
const ME = 'u-me'

function fakeTransport(over: Partial<ChatTransport> = {}): ChatTransport {
  return {
    openSession: vi.fn(async () => {}),
    fetchMessages: vi.fn(async (): Promise<MessagesWindow> => ({ messages: [], offset: 0, limit: 0, total: 0 })),
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

/** Seed the ONE owner of the session list with this session, bound or not. */
function seedSession(chatSessionId?: string) {
  useAgentStore.setState({
    agents: {
      [AGENT]: {
        info: { id: AGENT, name: 'A', node_id: 'n1' },
        sessions: [{ session_id: SESSION, title: 'S', can_write: true, chat_session_id: chatSessionId ?? null }],
      },
    },
    agentList: [{ id: AGENT, name: 'A', node_id: 'n1' }],
    users: [],
  })
  useChatStore.setState({ agentStates: { [AGENT]: { ...emptyView(), activeSessionId: SESSION } } })
}

describe('ADR-100 D12: the store reports the Chat binding', () => {
  beforeEach(() => {
    useAuthStore.setState({ me: { user_id: ME } as never })
  })

  afterEach(() => {
    useAuthStore.setState({ me: null })
  })

  it('passes the pin when the session is bound and a human is signed in', async () => {
    seedSession(CHAT)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'continue the task')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'continue the task', '', {
      me: ME,
      chatSessionId: CHAT,
    })
  })

  it('passes no pin for an unbound session — the session-scoped write is its only path', async () => {
    seedSession()
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'hello')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'hello', '', undefined)
  })

  it('passes no pin without an account, even for a bound session', async () => {
    // The chat channel 401s an identity-less caller; falling back is the
    // correct send, not a degraded one.
    useAuthStore.setState({ me: null })
    seedSession(CHAT)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'hello')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'hello', '', undefined)
  })

  it('keeps the picked skill on the pinned send', async () => {
    seedSession(CHAT)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'weekly report please', 'weekly-report')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'weekly report please', 'weekly-report', {
      me: ME,
      chatSessionId: CHAT,
    })
  })

  it('paints the optimistic bubble under the id the transport minted', async () => {
    // One id, three records: the bubble, the Chat row and the execution
    // session's own transcript row all reconcile on what `send` returns.
    seedSession(CHAT)
    const t = fakeTransport({ send: vi.fn(async () => 'mid-7') })
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'same id')

    expect(useChatStore.getState().agentStates[AGENT]?.messages.at(-1)?.id).toBe('mid-7')
  })
})

describe('ADR-100 D12: a pin changes the entry point, not the destination', () => {
  interface Call {
    url: string
    body: Record<string, unknown>
  }
  let calls: Call[]

  beforeEach(() => {
    calls = []
    setAuthBridge({
      baseUrl: () => 'http://gw.test',
      accessToken: () => 'tok',
      refresh: async () => null,
      sessionExpired: () => {},
    })
    vi.stubGlobal(
      'fetch',
      vi.fn(async (url: RequestInfo | URL, init?: RequestInit) => {
        calls.push({
          url: String(url),
          body: init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {},
        })
        return {
          ok: true,
          status: 200,
          json: async () => ({}),
          text: async () => '',
          headers: { get: () => null },
        }
      }),
    )
  })

  afterEach(() => {
    vi.unstubAllGlobals()
  })

  it('posts a pinned turn to the chat-message channel', async () => {
    const mid = await sendMessage(AGENT, SESSION, 'run it again', 'weekly-report', {
      me: ME,
      chatSessionId: CHAT,
    })

    const post = calls.find((c) => c.url.endsWith('/api/intents/chat-message'))
    expect(post).toBeTruthy()
    expect(post!.body).toMatchObject({
      chat_session_id: CHAT,
      from_user: ME,
      text: 'run it again',
      in_reply_to: '',
      for_execution_session: SESSION,
      // A bound chat can be a project group, which has several agents: the
      // send names the one owning this execution session, and the Gateway
      // checks that claim against the chat's participants.
      pinned_agent: AGENT,
      command: 'weekly-report',
    })
    // The id returned for the bubble IS the row id sent, or the two surfaces
    // would each show their own copy of one message.
    expect(post!.body.message_id).toBe(mid)
    // And the session-scoped write is not also made: one send, one turn.
    expect(calls.find((c) => c.url.includes(`/sessions/${SESSION}/messages`))).toBeUndefined()
  })

  it('keeps the session-scoped write without a pin', async () => {
    await sendMessage(AGENT, SESSION, 'plain', undefined)

    const post = calls.find((c) => c.url.endsWith(`/api/agents/${AGENT}/sessions/${SESSION}/messages`))
    expect(post).toBeTruthy()
    expect(post!.body).toMatchObject({ content: 'plain', command: '' })
    expect(calls.find((c) => c.url.endsWith('/api/intents/chat-message'))).toBeUndefined()
  })
})
