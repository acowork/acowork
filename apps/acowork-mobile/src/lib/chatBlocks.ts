/**
 * The Chat surface's execution-block read (ADR-106 D3/D5/D9) — the mobile twin
 * of Desktop's `lib/chatBlocks`.
 *
 * A chat transcript holds one bodyless `execution_round` row per round of work
 * (D3). What the round actually did — the human's row, the agent's answer, the
 * card it is parked on, how it ended — lives in the EXECUTION session and is
 * served by `GET /api/chat-blocks/{chatId}?refs=` (the transport lives in
 * `lib/api.ts`, because every authenticated read in this app rides the one
 * 401-rotation ladder).
 *
 * The two rules this module exists to keep:
 * - a ref is the row's own coordinate triple, never a lookup the client invents
 *   (a missing `agent_session_id` means there is no block to read, D3);
 * - `pending` is the only status that may be re-read: a terminal projection is
 *   immutable and already cached server-side (D5), so polling it would be
 *   re-fetching history as if it were live.
 */

/** The row fields a block ref is built from. `ChatMessageRow.meta`'s relevant
 *  subset — kept structural so this module does not own the transcript type. */
export interface BlockRefParts {
  agent_instance_id?: string
  agent_session_id?: string
  round_id?: string
}

/** `instance|session|round`, the Gateway's cache and response key.
 *  `null` when the row names no resolvable round: a reference missing any of its
 *  three coordinates addresses nothing, and inventing a fourth would put a block
 *  under a row that has no run behind it (D3). */
export function blockRefOf(meta?: BlockRefParts): string | null {
  const instance = meta?.agent_instance_id?.trim() ?? ''
  const session = meta?.agent_session_id?.trim() ?? ''
  const round = meta?.round_id?.trim() ?? ''
  if (!instance || !session || !round) return null
  return `${instance}|${session}|${round}`
}

/** `failed` / `timeout` are faults; `aborted` is a user action, not a fault. */
export function isFailureStatus(status: string): boolean {
  return status === 'failed' || status === 'timeout'
}

/** Outcome copy for a round, worded the way the transcript's own status lines
 *  word it — "how a round ended" must read the same in a block header and in a
 *  centered line, or the two drift (same rule as Desktop's shared notice keys). */
export function blockStatusText(status: string): string {
  switch (status) {
    case 'completed':
      return '已完成'
    case 'failed':
      return '本轮执行失败'
    case 'aborted':
    case 'interrupted':
      return '本轮执行已中断'
    case 'timeout':
      return '本轮执行超时'
    case 'waiting':
      return '等待你回复'
    case 'unknown':
      return '结果未知'
    default:
      return '执行中'
  }
}

/** Bound a label so one collapsed line stays one line on a 390pt screen. */
export function truncateLabel(text: string, max = 34): string {
  const collapsed = text.trim().replace(/\s+/g, ' ')
  if (!collapsed) return ''
  return collapsed.length > max ? `${collapsed.slice(0, max)}…` : collapsed
}
