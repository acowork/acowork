/**
 * The mobile chat-session surface (plan Wave 5.5, design 27 §6.1/§6.5).
 *
 * Two things are pinned here, both store rules rather than pixels:
 *
 *   1. `projectInFlight` — the read-time turn projection. Copied verbatim
 *      from Desktop's `chatSessionStore.test.ts`: the two clients read one
 *      endpoint and one vocabulary, so these mirrored cases are the drift
 *      guard — change the projection on one side and the other goes red.
 *      (Mobile drops only the transient-bubble half: v1 has no streaming.)
 *   2. the freshness loop — the `chat/changed` doorbell is content-free, so
 *      `wake()` re-reads over HTTP and never patches a row from a signal;
 *      `sendText` re-reads once after the POST because the durable row
 *      exists before the delivery resolves (§5.1).
 *
 * `lib/api` is the seam, same as `user-chat-push.test.ts`.
 */
import { describe, expect, it, vi, beforeEach } from 'vitest'

const api = vi.hoisted(() => ({
  fetchDefaultChatSession: vi.fn(async (_agentId: string) => ({
    session_id: 'cs-1',
    kind: 'user_agent',
    title: 'Agent',
    created_at: 1,
    last_active_at: 1,
    message_count: 0,
  })),
  fetchChatMessages: vi.fn(
    async (_sid: string, offset: number, _limit?: number): Promise<import('../lib/types').ChatMessagesPage> => ({
      messages: [],
      total: 0,
      offset,
      limit: 50,
    }),
 ),
  sendChatMessage: vi.fn(async (_me: string, _sid: string, _text: string, _reply?: string) => {}),
}))

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, ...api }
})

import { projectInFlight, useChatSessionStore, wakeAgentChat } from '../stores/chatSessionStore'
import { useAuthStore } from '../stores/authStore'
import type { ChatMessageRow } from '../lib/types'

const row = (kind: string, meta: Partial<ChatMessageRow['meta']> = {}): ChatMessageRow => ({
  seq: 0,
  ts: 0,
  from: 'u1',
  kind,
  body: 'x',
  to: '',
  meta: { message_id: '', ...meta },
})

const user = (id: string) => row('user_message', { message_id: id })
const pointer = (trigger: string, sid: string) =>
  row('system_notice', { notice_type: 'turn_started', in_reply_to: trigger, agent_session_id: sid })
const terminal = (trigger: string) =>
  row('agent_message', { message_id: `resp-${trigger}`, in_reply_to: trigger })

describe('projectInFlight (mirrored from Desktop — drift guard)', () => {
  it('sees a trailing user_message with its pointer as in flight', () => {
    const rows = [user('t1'), pointer('t1', 'exec-1')]
    expect(projectInFlight(rows)).toEqual({ triggerId: 't1', execSid: 'exec-1' })
  })

  it('clears once the terminal row lands', () => {
    const rows = [user('t1'), pointer('t1', 'exec-1'), terminal('t1')]
    expect(projectInFlight(rows)).toBeNull()
  })

  it('ignores a trailing user_message without a pointer (group message that @-nobody)', () => {
    expect(projectInFlight([user('t1')])).toBeNull()
  })

  it('treats an interaction card as turn-end (the card is pending, not the turn)', () => {
    const rows = [
      user('t1'),
      pointer('t1', 'exec-1'),
      row('interaction_request', { message_id: 'c1', in_reply_to: 't1', interaction: { kind: 'ask_question', request_id: 'r1' } }),
    ]
    expect(projectInFlight(rows)).toBeNull()
  })

  it('follows a card answer back into flight (position rule, not id chain)', () => {
    const rows = [
      user('t1'),
      pointer('t1', 'exec-1'),
      row('interaction_request', { message_id: 'c1', in_reply_to: 't1' }),
      row('user_message', { message_id: 't2', in_reply_to: 'c1' }),
    ]
    expect(projectInFlight(rows)).toEqual({ triggerId: 't2', execSid: 'exec-1' })
  })

  it('is null for an empty window', () => {
    expect(projectInFlight([])).toBeNull()
  })
})

describe('chatSessionStore freshness loop', () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, inFlight: {}, sending: {} })
    api.fetchDefaultChatSession.mockClear()
    api.fetchChatMessages.mockClear()
    api.sendChatMessage.mockClear()
    useAuthStore.setState({ me: { user_id: 'u1' } as never })
  })

  it('openAgent resolves the default session and loads its window', async () => {
    await useChatSessionStore.getState().openAgent('agent-1')
    const st = useChatSessionStore.getState().byAgent['agent-1']
    expect(st).toEqual({ sessionId: 'cs-1', resolved: true })
    expect(api.fetchChatMessages).toHaveBeenCalledWith('cs-1', 0, 50)
  })

  it('openAgent failure resolves with a null session — the screen shows its own error, never a spinner', async () => {
    api.fetchDefaultChatSession.mockRejectedValueOnce(new Error('hidden 404'))
    await useChatSessionStore.getState().openAgent('agent-dead')
    expect(useChatSessionStore.getState().byAgent['agent-dead']).toEqual({ sessionId: null, resolved: true })
  })

  it('sendText posts through the intent channel and re-reads once (§5.1: the row exists before delivery resolves)', async () => {
    await useChatSessionStore.getState().openAgent('agent-1')
    api.fetchChatMessages.mockClear()
    const ok = await useChatSessionStore.getState().sendText('cs-1', 'hello', 'card-9')
    expect(ok).toBe(true)
    expect(api.sendChatMessage).toHaveBeenCalledWith('u1', 'cs-1', 'hello', 'card-9')
    expect(api.fetchChatMessages).toHaveBeenCalledTimes(1)
  })

  it('wake() re-reads every resolved session over HTTP — a content-free doorbell patches no rows', async () => {
    await useChatSessionStore.getState().openAgent('agent-1')
    api.fetchChatMessages.mockClear()
    wakeAgentChat()
    await vi.waitFor(() => expect(api.fetchChatMessages).toHaveBeenCalledWith('cs-1', 0, 50))
    // And the window is only ever replaced wholesale: no row was injected.
    expect(useChatSessionStore.getState().windows['cs-1']!.tail).toEqual([])
  })

  it('refresh projects the in-flight turn from the durable rows', async () => {
    api.fetchChatMessages.mockResolvedValueOnce({
      messages: [user('t1'), pointer('t1', 'exec-9')],
      total: 2,
      offset: 0,
      limit: 50,
    })
    await useChatSessionStore.getState().openAgent('agent-1')
    expect(useChatSessionStore.getState().inFlight['cs-1']!).toEqual({ triggerId: 't1', execSid: 'exec-9' })
  })
})
