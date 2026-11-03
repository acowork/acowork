/**
 * The execution-block read layer (ADR-106 D4/D5/D9) — `stores/chatBlockStore`.
 *
 * Why this file exists: every row of a chat window asks for its block, and a
 * scroll that issues one HTTP call per row is a request storm on the Gateway and
 * on the hosting Runtime. So reads are batched per chat, and the cache has
 * deliberately NO invalidation path — a terminal projection cannot change (D5).
 * The two rules pinned here:
 *
 *   1. refs asked in one render pass ride ONE request, and a ref already held is
 *      not re-requested (only `reload` forces, which is what the `pending` poll
 *      and the unreachable-retry use);
 *   2. a failed BATCH is reported per ref as `unreachable` — never dropped,
 *      because an absent entry a surface would read as "no such round" while the
 *      truth is "we could not reach the host" (D9); and a ref that already has a
 *      terminal `ok` projection keeps it, since history does not go stale.
 *
 * `lib/api` is the seam, same as `agent-chat-session.test.ts`.
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'

const api = vi.hoisted(() => ({
  // The seam is `lib/api`, whose `fetchChatBlocks` already unwraps the Gateway
  // envelope: the store is handed the entries, in request order.
  fetchChatBlocks: vi.fn(async (_chatId: string, refs: string[]) =>
    refs.map((ref) => {
      const [agent_instance_id = '', agent_session_id = '', round_id = ''] = ref.split('|')
      return {
        ref,
        agent_instance_id,
        agent_session_id,
        round_id,
        status: 'ok' as const,
        projection: { round_id, terminal: true, status: 'completed' },
      }
    }),
  ),
}))

vi.mock('../lib/api', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../lib/api')>()
  return { ...actual, ...api }
})

import { useChatBlockStore } from '../stores/chatBlockStore'
import type { ChatBlockEntry } from '../lib/types'

const A = 'ag|s1|r1'
const B = 'ag|s1|r2'

function blocks(): Record<string, ChatBlockEntry> {
  return useChatBlockStore.getState().blocks
}

beforeEach(() => {
  useChatBlockStore.setState({ blocks: {}, pending: {}, inFlight: {} })
  api.fetchChatBlocks.mockClear()
})

describe('batching', () => {
  it('refs requested in one pass ride a single request', async () => {
    const { ensure } = useChatBlockStore.getState()
    ensure('cs-1', [A])
    ensure('cs-1', [B])
    await vi.waitFor(() => expect(api.fetchChatBlocks).toHaveBeenCalledTimes(1))
    expect(api.fetchChatBlocks).toHaveBeenCalledWith('cs-1', [A, B])
    expect(Object.keys(blocks()).sort()).toEqual([A, B].sort())
  })

  it('a ref already held is not asked for again', async () => {
    const { ensure, reload } = useChatBlockStore.getState()
    ensure('cs-1', [A])
    await vi.waitFor(() => expect(blocks()[A]).toBeDefined())
    ensure('cs-1', [A])
    ensure('cs-1', [A, B])
    await vi.waitFor(() => expect(blocks()[B]).toBeDefined())
    expect(api.fetchChatBlocks).toHaveBeenCalledTimes(2)
    expect(api.fetchChatBlocks.mock.calls[1]![1]).toEqual([B])
    // Only an explicit reload re-reads what is held.
    reload('cs-1', [A])
    await vi.waitFor(() => expect(api.fetchChatBlocks).toHaveBeenCalledTimes(3))
    expect(api.fetchChatBlocks.mock.calls[2]![1]).toEqual([A])
  })

  it('an empty ref list or an empty chat issues nothing', () => {
    const { ensure, reload } = useChatBlockStore.getState()
    ensure('cs-1', [])
    ensure('', [A])
    reload('cs-1', [])
    expect(api.fetchChatBlocks).not.toHaveBeenCalled()
  })
})

describe('a failed read is a hole in the transcript, not a missing round (D9)', () => {
  it('marks every requested ref unreachable with the batch error as its detail', async () => {
    api.fetchChatBlocks.mockRejectedValueOnce(new Error('connection refused'))
    useChatBlockStore.getState().ensure('cs-1', [A, B])
    await vi.waitFor(() => expect(blocks()[A]?.status).toBe('unreachable'))
    expect(blocks()[A]!.detail).toBe('connection refused')
    expect(blocks()[A]!.agent_session_id).toBe('s1')
    expect(blocks()[B]!.status).toBe('unreachable')
    expect(blocks()[B]!.round_id).toBe('r2')
  })

  it('keeps a terminal projection that is already held — history does not go stale (D5)', async () => {
    useChatBlockStore.getState().ensure('cs-1', [A])
    await vi.waitFor(() => expect(blocks()[A]?.status).toBe('ok'))
    // Force a re-read of a held-ok ref and let it fail: the block must stay ok.
    api.fetchChatBlocks.mockRejectedValueOnce(new Error('timeout'))
    useChatBlockStore.getState().reload('cs-1', [A, B])
    await vi.waitFor(() => expect(blocks()[B]?.status).toBe('unreachable'))
    expect(blocks()[A]!.status).toBe('ok')
    expect(blocks()[A]!.projection?.terminal).toBe(true)
  })
})
