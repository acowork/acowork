/**
 * The mobile chat-session surface (plan Wave 5.5, design 27 §6.1/§6.5).
 *
 * The chat session is a durable EVENT LOG — the UI renders the rows as they
 * landed (ADR-106 D1) and owns no live status mirror, so there is no read-time
 * turn projection to pin here any more. What remains is the store rule that
 * matters:
 *
 *   the freshness loop — the `chat/changed` doorbell is content-free, so
 *   `wake()` re-reads over HTTP and never patches a row from a signal;
 *   `sendText` re-reads once after the POST because the durable row
 *   exists before the delivery resolves (§5.1).
 *
 * Plus the claim the waiting line and the composer lock run on (ADR-106 D9):
 * the row id the send's OWN response carried, held until a row answers it or
 * the deadline passes — see the last block.
 *
 * Plus the one write-side rule the addressing ladder depends on: the human's
 * address (a named session, or the refusal to continue one) reaches the wire
 * exactly as given — no client may re-decide it (D6/D7).
 *
 * `lib/api` is the seam, same as `user-chat-push.test.ts`.
 */
import { describe, expect, it, vi, afterEach, beforeEach } from 'vitest'

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
  sendChatMessage: vi.fn(
    async (_me: string, _sid: string, _text: string): Promise<import('../lib/api').ChatSendResponse> => ({
      message_id: 'm-1',
      status: 'delivered',
    }),
  ),
}))

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, ...api }
})

import { useChatSessionStore, wakeAgentChat } from '../stores/chatSessionStore'
import { TURN_INFLIGHT_DEADLINE_MS } from '../lib/chatTurnInflight'
import { useAuthStore } from '../stores/authStore'

describe('chatSessionStore freshness loop', () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, inflight: {} })
    api.fetchDefaultChatSession.mockClear()
    api.fetchChatMessages.mockClear()
    api.sendChatMessage.mockClear()
    api.sendChatMessage.mockResolvedValue({ message_id: 'm-1', status: 'delivered' })
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
    const ok = await useChatSessionStore.getState().sendText('cs-1', 'hello', 'round:ag|s1|r1')
    expect(ok).toBe(true)
    expect(api.sendChatMessage).toHaveBeenCalledWith('u1', 'cs-1', 'hello', 'round:ag|s1|r1', undefined, undefined)
    expect(api.fetchChatMessages).toHaveBeenCalledTimes(1)
  })

  it('carries the human\'s address to the wire unchanged (D6/D7)', async () => {
    await useChatSessionStore.getState().openAgent('agent-1')
    const { sendText } = useChatSessionStore.getState()
    await sendText('cs-1', '继续那个', undefined, ['a-1'], { targetSession: 's-9', messageId: 'm-1' })
    expect(api.sendChatMessage).toHaveBeenCalledWith('u1', 'cs-1', '继续那个', undefined, ['a-1'], {
      targetSession: 's-9',
      messageId: 'm-1',
    })
    await sendText('cs-1', '换个话题', undefined, undefined, { forceNew: true, messageId: 'm-2' })
    expect(api.sendChatMessage).toHaveBeenLastCalledWith('u1', 'cs-1', '换个话题', undefined, undefined, {
      forceNew: true,
      messageId: 'm-2',
    })
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

/**
 * The claim the waiting line and the composer lock run on (ADR-106 D9) — the
 * mobile twin of the desktop store's cases, 1:1 only (there is no @-fan-out
 * entry here, design 27 §5.4.4).
 *
 * `sending` covers the POST and nothing else — measured ~150 ms, while the
 * turn's first durable row can be a minute behind it. The rules these hold are
 * the ones that keep the claim honest: it is taken only from the id this
 * device's OWN response carried, and only for a send that actually reached an
 * agent (a failed 1:1 delivery writes no receipt into the Chat, so a claim would
 * lock the composer against a row that can never arrive), and a release clears
 * only the claim it names.
 */
describe('the in-flight turn claim (ADR-106 D9)', () => {
  beforeEach(() => {
    vi.useFakeTimers()
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, inflight: {} })
    api.fetchChatMessages.mockClear()
    api.sendChatMessage.mockClear()
    api.sendChatMessage.mockResolvedValue({ message_id: 'm-1', status: 'delivered' })
    useAuthStore.setState({ me: { user_id: 'u1' } as never })
  })

  afterEach(() => {
    // Every claim arms a 300 s deadline; never let one outlive its case.
    vi.clearAllTimers()
    vi.useRealTimers()
  })

  const claimOf = (sid: string) => useChatSessionStore.getState().inflight[sid]

  it('a delivered send claims the row id its OWN response carried', async () => {
    // Not a row read off the transcript — the id the Gateway gave this device's
    // message, which is what makes the claim first-person.
    await useChatSessionStore.getState().sendText('cs-1', 'hello')
    expect(claimOf('cs-1')?.messageId).toBe('m-1')
  })

  it('a delivery that failed claims nothing: nothing will ever answer it', async () => {
    api.sendChatMessage.mockResolvedValueOnce({ message_id: 'm-1', status: 'failed' })
    const ok = await useChatSessionStore.getState().sendText('cs-1', 'hello')
    expect(ok).toBe(true)
    expect(claimOf('cs-1')).toBeUndefined()
  })

  it('the claim survives the POST: `sending` clears, the turn stays claimed', async () => {
    await useChatSessionStore.getState().sendText('cs-1', 'hello')
    expect(useChatSessionStore.getState().sending['cs-1']).toBe(false)
    expect(claimOf('cs-1')).toBeDefined()
  })

  it('settleTurn releases the claim and cancels its deadline', async () => {
    await useChatSessionStore.getState().sendText('cs-1', 'hello')
    useChatSessionStore.getState().settleTurn('cs-1', 'm-1')
    expect(claimOf('cs-1')).toBeUndefined()
    expect(vi.getTimerCount()).toBe(0)
  })

  it('the deadline releases a turn whose outcome never landed', async () => {
    // The case the surface cannot see: a Runtime that died between delivery and
    // round open writes no outcome row. Without a deadline the composer would
    // stay locked for good.
    await useChatSessionStore.getState().sendText('cs-1', 'hello')
    vi.advanceTimersByTime(TURN_INFLIGHT_DEADLINE_MS)
    expect(claimOf('cs-1')).toBeUndefined()
  })

  it('a stale release cannot drop a fresher claim', async () => {
    await useChatSessionStore.getState().sendText('cs-1', 'first')
    api.sendChatMessage.mockResolvedValueOnce({ message_id: 'm-2', status: 'delivered' })
    await useChatSessionStore.getState().sendText('cs-1', 'second')
    expect(claimOf('cs-1')?.messageId).toBe('m-2')
    useChatSessionStore.getState().settleTurn('cs-1', 'm-1')
    useChatSessionStore.getState().expireTurn('cs-1', 'm-1')
    expect(claimOf('cs-1')?.messageId).toBe('m-2')
    // Only the m-2 send's own deadline lets go.
    vi.advanceTimersByTime(TURN_INFLIGHT_DEADLINE_MS)
    expect(claimOf('cs-1')).toBeUndefined()
  })

  it('one session’s claim says nothing about another’s', async () => {
    await useChatSessionStore.getState().sendText('cs-1', 'hello')
    useChatSessionStore.getState().settleTurn('cs-2', 'm-1')
    expect(claimOf('cs-1')?.messageId).toBe('m-1')
  })
})
