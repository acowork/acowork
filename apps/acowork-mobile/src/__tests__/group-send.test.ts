/**
 * The group send wire — pinning what actually leaves the device (design 27
 * §4.5.1, ADR-105 §4). Two channels, one rule each:
 *   - `POST /api/intents/mentions` carries the resolved `targets`, and the
 *     address fields flattened at the TOP level (`target_sessions`,
 *     `force_new_for`, `refs`) — only for agents actually mentioned, and
 *     `force_new_for` never for a target whose session was named (the two
 *     are contradictory instructions);
 *   - a reply to a row stays on the level-1 `chat-message` channel with
 *     `in_reply_to` — the Gateway resolves the responder from the row being
 *     answered, and the client never guesses it.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { fanoutMentions, setAuthBridge } from '../lib/api'

let calls: Array<{ url: string; body: Record<string, unknown> }> = []

beforeEach(() => {
  calls = []
  vi.resetModules()
  setAuthBridge({
    baseUrl: () => 'http://gw.test',
    accessToken: () => 'tok',
    refresh: async () => null,
    sessionExpired: () => {},
  })
  vi.stubGlobal(
    'fetch',
    vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      calls.push({
        url: String(input),
        body: init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {},
      })
      return {
        ok: true,
        status: 200,
        json: async () => ({ message_id: 'm-1', status: 'delivered', delivered: 1, skipped: [], results: [{ status: 'delivered' }] }),
        text: async () => '{}',
      }
    }),
  )
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('fanoutMentions wire shape', () => {
  it('posts the resolved targets and flattens the address refs to snake_case', async () => {
    await fanoutMentions('u1', 'g-1', '看 @小助手', ['ag-1'], undefined, {
      refs: [{ agentInstanceId: 'ag-1', sessionId: 's-9' }],
    })
    expect(calls[0]!.url).toContain('/api/intents/mentions')
    expect(calls[0]!.body).toMatchObject({
      chat_session_id: 'g-1',
      from_user: 'u1',
      text: '看 @小助手',
      targets: ['ag-1'],
      refs: [{ agent_instance_id: 'ag-1', session_id: 's-9' }],
    })
  })

  it('never sends force_new_for for a target whose session was named', async () => {
    await fanoutMentions('u1', 'g-1', 'x @小助手 @Kevin', ['ag-1', 'ag-2'], undefined, {
      targetSessions: { 'ag-1': 's-1' },
      forceNewFor: ['ag-1', 'ag-2'],
    })
    expect(calls[0]!.body.force_new_for).toEqual(['ag-2'])
    expect(calls[0]!.body.target_sessions).toEqual({ 'ag-1': 's-1' })
  })

  it('no address means no address keys at all (absence, not empty objects)', async () => {
    await fanoutMentions('u1', 'g-1', '随便说一句', ['ag-1'])
    const b = calls[0]!.body
    expect('refs' in b).toBe(false)
    expect('force_new_for' in b).toBe(false)
    expect('target_sessions' in b).toBe(false)
  })
})

describe('sendGroupText channel choice', () => {
  async function seed() {
    const { useAuthStore } = await import('../stores/authStore')
    const { useChatSessionStore } = await import('../stores/chatSessionStore')
    useAuthStore.setState({ me: { user_id: 'u1', username: 'me', display_name: 'Me', role: 'user', language: 'zh', timezone: 'Asia/Shanghai' } })
    useChatSessionStore.setState({
      windows: { 'g-1': { older: [], tail: [], tailLoaded: 0, total: 0, loadingOlder: false } },
      openGroupIds: { 'g-1': true },
      sending: {},
    })
    return useChatSessionStore
  }

  it('a plain send goes to the mentions endpoint with the mentioned agents as targets', async () => {
    const store = await seed()
    const ok = await store.getState().sendGroupText('g-1', '看 @小助手', ['ag-1'])
    expect(ok).toBe(true)
    expect(calls[0]!.url).toContain('/api/intents/mentions')
    expect(calls[0]!.body.targets).toEqual(['ag-1'])
  })

  it('an un-addressed send still uses the mentions endpoint — with EMPTY targets, so the server routes it (ADR-105)', async () => {
    const store = await seed()
    await store.getState().sendGroupText('g-1', '随便说一句', [])
    expect(calls[0]!.url).toContain('/api/intents/mentions')
    expect(calls[0]!.body.targets).toEqual([])
  })

  it('a reply to a row stays on the chat-message channel with in_reply_to', async () => {
    const store = await seed()
    const ok = await store.getState().sendGroupText('g-1', '收到', [], 'r-1')
    expect(ok).toBe(true)
    expect(calls[0]!.url).toContain('/api/intents/chat-message')
    expect(calls[0]!.body.in_reply_to).toBe('r-1')
  })
})
