/**
 * An execution-view send on mobile always writes into its own session.
 *
 * Two layers are pinned, because the rule is split on purpose:
 * - the STORE hands the transport the session it is showing and nothing else.
 *   It used to also report the Chat binding (`SessionInfo.chat_session_id`) so
 *   the transport could post through the Chat entry point; that mirror is what
 *   forced a delivery pin, so the binding is no longer an input at all.
 * - the TRANSPORT decides what that means on the wire: one POST to
 *   `/api/agents/{id}/sessions/{sid}/messages`, carrying the client-minted
 *   `message_id` the optimistic bubble is keyed on.
 *
 * `command` rides the write, so a skill picked in the composer survives.
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
        sessions: [{ session_id: SESSION, title: 'S', can_write: true, chat_session_id: chatSessionId } as SessionInfo],
      },
    },
    agentList: [{ id: AGENT, name: 'A', node_id: 'n1' }],
    users: [],
  })
  useChatStore.setState({ agentStates: { [AGENT]: { ...emptyView(), activeSessionId: SESSION } } })
}

describe('the store sends to the session it shows', () => {
  beforeEach(() => {
    useAuthStore.setState({ me: { user_id: ME } as never })
  })

  afterEach(() => {
    useAuthStore.setState({ me: null })
    useChatStore.setState({ agentStates: {} })
    useAgentStore.setState({ agents: {}, agentList: [] })
  })

  it('passes no chat binding when the session is bound', async () => {
    seedSession(CHAT)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'run it again')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'run it again', '')
  })

  it('sends the same way for an unbound session', async () => {
    seedSession(undefined)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'plain')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'plain', '')
  })

  it('sends the same way without an account', async () => {
    useAuthStore.setState({ me: null })
    seedSession(CHAT)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'plain')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'plain', '')
  })

  it('keeps the picked skill on the send', async () => {
    seedSession(CHAT)
    const t = fakeTransport()
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'draft it', 'weekly-report')

    expect(t.send).toHaveBeenCalledWith(AGENT, SESSION, 'draft it', 'weekly-report')
  })

  it('paints the optimistic bubble under the id the transport minted', async () => {
    // One id, two records: the bubble and the execution session's own
    // transcript row reconcile on what `send` returns.
    seedSession(CHAT)
    const t = fakeTransport({ send: vi.fn(async () => 'mid-7') })
    setChatTransport(t)

    await useChatStore.getState().sendMessage(AGENT, SESSION, 'same id')

    expect(useChatStore.getState().agentStates[AGENT]?.messages.at(-1)?.id).toBe('mid-7')
  })
})

describe('the transport writes to the session, never to the chat channel', () => {
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

  it('posts the turn to its own session', async () => {
    const mid = await sendMessage(AGENT, SESSION, 'run it again', 'weekly-report')

    const post = calls.find((c) => c.url.endsWith(`/api/agents/${AGENT}/sessions/${SESSION}/messages`))
    expect(post).toBeTruthy()
    expect(post!.body).toMatchObject({
      content: 'run it again',
      command: 'weekly-report',
    })
    // The id returned for the bubble IS the row id sent, or the transcript and
    // the bubble would each show their own copy of one message.
    expect(post!.body.message_id).toBe(mid)
  })

  it('makes no chat-channel request at all', async () => {
    await sendMessage(AGENT, SESSION, 'plain', undefined)

    expect(calls.find((c) => c.url.endsWith('/api/intents/chat-message'))).toBeUndefined()
    expect(calls).toHaveLength(1)
  })
})
