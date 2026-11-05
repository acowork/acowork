/**
 * The composer's reference picks on the wire (ADR-106 rev 2026-11-04) — the
 * mobile twin of Desktop's `sendGroupText` refs-shape case.
 *
 * `api.sendChatMessage` is where a `ChatAddress.refs` becomes the snake_case
 * payload the Gateway pins, so this pins the transform directly rather than
 * through the store (which mocks the transport). What must hold:
 *   1. a fresh pick sends only the coordinate the Gateway needs to pin the round
 *      (`{agent_instance_id, session_id}`) — no invented `round_id`;
 *   2. an echoed, already-pinned ref (a card answer re-delivering its original
 *      material) carries `round_id`/`terminal` straight through so the Gateway
 *      passes it rather than re-resolving the round of a written sentence;
 *   3. NO refs means NO `refs` key at all — absence is what keeps a turn on the
 *      ladder's lower levels, an empty array would be a different answer.
 */
import { describe, expect, it, beforeEach, afterEach, vi } from 'vitest'
import { sendChatMessage, setAuthBridge } from '../lib/api'

let bodies: Array<Record<string, unknown>> = []

beforeEach(() => {
  bodies = []
  setAuthBridge({
    baseUrl: () => 'http://gw.test',
    accessToken: () => 'tok',
    refresh: async () => null,
    sessionExpired: () => {},
  })
  vi.stubGlobal(
    'fetch',
    vi.fn(async (_input: RequestInfo | URL, init?: RequestInit) => {
      bodies.push(init?.body ? (JSON.parse(String(init.body)) as Record<string, unknown>) : {})
      return {
        ok: true,
        status: 200,
        json: async () => ({ message_id: 'm-1', status: 'delivered' }),
        text: async () => JSON.stringify({ message_id: 'm-1', status: 'delivered' }),
      }
    }),
  )
})

afterEach(() => {
  vi.unstubAllGlobals()
})

describe('sendChatMessage reference wire shape', () => {
  it('a fresh pick sends just the coordinate the Gateway pins, in snake_case', async () => {
    await sendChatMessage('u1', 'cs-1', '引用这个', undefined, undefined, {
      refs: [{ agentInstanceId: 'ag-1', sessionId: 's-7' }],
    })
    expect(bodies[0]).toMatchObject({
      chat_session_id: 'cs-1',
      from_user: 'u1',
      text: '引用这个',
    })
    // Strict: a fresh pick is exactly the pin coordinate — no invented round.
    expect(bodies[0]!.refs).toEqual([{ agent_instance_id: 'ag-1', session_id: 's-7' }])
    // No delivery target sneaks in alongside a material reference.
    expect(bodies[0]).not.toHaveProperty('target_session')
  })

  it('an echoed, already-pinned ref carries round_id/terminal straight through', async () => {
    await sendChatMessage('u1', 'cs-1', '继续那个', undefined, undefined, {
      targetSession: 's-9',
      messageId: 'm-1',
      refs: [{ agentInstanceId: 'ag-1', sessionId: 's-7', roundId: 'r-3', terminal: true }],
    })
    expect(bodies[0]!.refs).toEqual([
      { agent_instance_id: 'ag-1', session_id: 's-7', round_id: 'r-3', terminal: true },
    ])
    // A card answer keeps its delivery target and dedup id beside the material.
    expect(bodies[0]).toMatchObject({ target_session: 's-9', message_id: 'm-1' })
  })

  it('sends no refs key at all when nothing was picked', async () => {
    await sendChatMessage('u1', 'cs-1', '普通一句', undefined, undefined, { forceNew: true })
    expect(bodies[0]).not.toHaveProperty('refs')
    expect(bodies[0]).toMatchObject({ force_new: true })
  })
})
