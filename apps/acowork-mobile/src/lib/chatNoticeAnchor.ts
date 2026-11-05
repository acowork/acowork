/**
 * Which column a channel receipt belongs in (ADR-106 D14's render-time rule) —
 * the mobile twin of Desktop's `src/lib/chatNoticeAnchor.ts`.
 *
 * A `system_notice` that names a row — a failed send points at the human's
 * `user_message`, a refused delivery points at the request that was refused —
 * is about ONE message. Painting it as a centered pill made it read as a fact
 * about the conversation, attached to nothing: on a phone nobody could tell
 * whose message did not go out.
 *
 * The join is a by-coordinate read of one known row, which design 31 §6 leaves
 * to the presentation layer (`chatTranscriptGroups` does the same for round
 * rows). Nothing here derives a state: whether something was delivered is
 * `row.delivery`, and this only decides a column.
 */
import type { ChatMessageRow } from './types'

export type NoticeAnchor = 'self' | 'peer'

/** `message_id → row` over the loaded window. Built once per window: the
 *  transcript looks a receipt's subject up by coordinate, and doing it per row
 *  would scan the window for every notice on screen. */
export function indexByMessageId(rows: ChatMessageRow[]): Map<string, ChatMessageRow> {
  const index = new Map<string, ChatMessageRow>()
  for (const row of rows) {
    const id = row.meta?.message_id?.trim()
    if (id) index.set(id, row)
  }
  return index
}

/**
 * The column this notice should sit in, or `undefined` when it names nothing the
 * window can resolve (a receipt whose subject has been paged out, a legacy row
 * with no coordinate). `undefined` keeps the pill centered: a fact with no
 * visible subject must not claim one.
 */
export function noticeAnchorOf(
  row: ChatMessageRow,
  byMessageId: Map<string, ChatMessageRow>,
  me: string,
): NoticeAnchor | undefined {
  const subjectId = row.meta?.in_reply_to?.trim()
  if (!subjectId) return undefined
  const subject = byMessageId.get(subjectId)
  if (!subject) return undefined
  return subject.from === me ? 'self' : 'peer'
}
