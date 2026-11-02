/**
 * Execution-block store (ADR-106 D3/D5/D9) — the mobile twin of Desktop's
 * `chatBlockStore`.
 *
 * The transcript renders a block per `execution_round` row, and every row of a
 * window asks for its block. One HTTP call per row would be a request storm on
 * scroll, so reads are BATCHED per chat: whichever component asks first queues
 * the refs, and the queue flushes on a frame timer with the refs this chat does
 * not already hold.
 *
 * What is deliberately NOT here: any invalidation of a terminal block. D5 gives
 * the cache no invalidation path because a terminal projection cannot change, so
 * the only re-reads are a `pending` block's poll and an explicit retry on an
 * `unreachable` one.
 *
 * Mobile difference worth naming: the read goes through `lib/api.ts` (the one
 * authenticated ladder this app owns), so a 401 mid-scroll rotates and replays
 * instead of dropping the block.
 */

import { useEffect } from 'react'
import { create } from 'zustand'
import { fetchChatBlocks } from '../lib/api'
import type { ChatBlockEntry } from '../lib/types'

/** How long a `pending` block waits between re-reads. Long enough that a run of
 *  minutes costs almost nothing, short enough that the block does not look
 *  frozen when it finishes. */
const PENDING_POLL_MS = 4000

/** Batch window: refs requested during one render pass ride one request. */
const BATCH_MS = 16

interface ChatBlockState {
  /** By ref (`instance|session|round`) — a ref names one round of one session,
   *  so it cannot collide across chats. */
  blocks: Record<string, ChatBlockEntry>
  /** Refs queued for the next batch, by chat. */
  pending: Record<string, Set<string>>
  /** Chats with a request in flight, so a flush while one is running does not
   *  re-issue the same refs. */
  inFlight: Record<string, boolean>
  /** Queue refs this chat needs but does not hold yet. */
  ensure: (chatId: string, refs: string[]) => void
  /** Re-read refs regardless of what is held (the pending poll, the retry). */
  reload: (chatId: string, refs: string[]) => void
}

const timers = new Map<string, ReturnType<typeof setTimeout>>()

export const useChatBlockStore = create<ChatBlockState>((set, get) => {
  const flush = (chatId: string) => {
    timers.delete(chatId)
    const state = get()
    if (state.inFlight[chatId]) {
      // Still busy: keep the queue for the next tick rather than dropping the
      // refs, which is what would leave a rendered block empty forever.
      const again = setTimeout(() => flush(chatId), BATCH_MS)
      timers.set(chatId, again)
      return
    }
    const queue = state.pending[chatId]
    if (!queue || queue.size === 0) return
    const refs = [...queue]
    const nextPending = { ...get().pending }
    delete nextPending[chatId]
    set({ pending: nextPending, inFlight: { ...get().inFlight, [chatId]: true } })
    fetchChatBlocks(chatId, refs)
      .then((entries) => {
        const held = { ...get().blocks }
        for (const e of entries) held[e.ref] = e
        set({ blocks: held })
      })
      .catch((err: unknown) => {
        // The batch failed, not the block: report each requested ref as
        // unreachable (which renders the retry affordance) rather than leaving
        // it absent, which a surface would read as "no block exists".
        const detail = err instanceof Error ? err.message : String(err)
        const held = { ...get().blocks }
        for (const ref of refs) {
          if (held[ref]?.status === 'ok') continue
          const parts = ref.split('|')
          held[ref] = {
            ref,
            agent_instance_id: parts[0] ?? '',
            agent_session_id: parts[1] ?? '',
            round_id: parts[2] ?? '',
            status: 'unreachable',
            detail,
          }
        }
        set({ blocks: held })
      })
      .finally(() => {
        set({ inFlight: { ...get().inFlight, [chatId]: false } })
        if ((get().pending[chatId]?.size ?? 0) > 0) {
          timers.set(chatId, setTimeout(() => flush(chatId), BATCH_MS))
        }
      })
  }

  const queueRefs = (chatId: string, refs: string[], force: boolean) => {
    if (!chatId || refs.length === 0) return
    const state = get()
    const held = state.blocks
    const wanted = refs.filter((r) => force || !held[r])
    if (wanted.length === 0) return
    const queue = new Set(state.pending[chatId] ?? [])
    wanted.forEach((r) => queue.add(r))
    set({ pending: { ...state.pending, [chatId]: queue } })
    if (!timers.has(chatId)) {
      timers.set(chatId, setTimeout(() => flush(chatId), BATCH_MS))
    }
  }

  return {
    blocks: {},
    pending: {},
    inFlight: {},
    ensure: (chatId, refs) => queueRefs(chatId, refs, false),
    reload: (chatId, refs) => queueRefs(chatId, refs, true),
  }
})

/**
 * One block, read on demand (ADR-106 D3/D9).
 *
 * A row asks for exactly its own ref, so a re-render never rebuilds a list the
 * store has to compare by identity. Multiple rows asking in the same render pass
 * is not multiple requests: the queue batches per chat.
 *
 * A block that comes back `pending` is re-read on a timer while its row stays
 * mounted, and the poll stops the moment it reaches a terminal projection: an
 * in-progress block that never re-reads looks frozen, and a finished one that
 * keeps re-reading is a client that does not believe history (D5).
 */
export function useChatBlock(chatId: string, ref?: string | null): ChatBlockEntry | undefined {
  const ensure = useChatBlockStore((s) => s.ensure)
  const reload = useChatBlockStore((s) => s.reload)
  const entry = useChatBlockStore((s) => (ref ? s.blocks[ref] : undefined))
  useEffect(() => {
    if (!chatId || !ref) return
    ensure(chatId, [ref])
  }, [chatId, ref, ensure])
  const pending = entry?.status === 'pending'
  useEffect(() => {
    if (!chatId || !ref || !pending) return
    const timer = setTimeout(() => reload(chatId, [ref]), PENDING_POLL_MS)
    return () => clearTimeout(timer)
  }, [chatId, ref, pending, reload])
  return entry
}
