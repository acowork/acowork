/**
 * The mobile chat-session surface (plan Wave 5.5, design 27 §6.1/§6.5).
 *
 * The chat session is a durable EVENT LOG — the UI renders the point-in-time
 * rows (`turn_dispatched` / `turn_started` / terminal `agent_message`) and owns
 * no live status mirror, so there is no read-time turn projection to pin here
 * any more. What remains is the store rule that matters:
 *
 *   the freshness loop — the `chat/changed` doorbell is content-free, so
 *   `wake()` re-reads over HTTP and never patches a row from a signal;
 *   `sendText` re-reads once after the POST because the durable row
 *   exists before the delivery resolves (§5.1).
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

import { useChatSessionStore, wakeAgentChat } from '../stores/chatSessionStore'
import { useAuthStore } from '../stores/authStore'

describe('chatSessionStore freshness loop', () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {} })
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
})
