/**
 * Transcript layout: one run block per execution session (ADR-106 D3/D4,
 * presentation contract in design 27 §5.5) — the mobile twin of Desktop's
 * `components/chat/chatTranscriptGroups`.
 *
 * Storage is per round — a chat holds one bodyless `execution_round` row per
 * round of work, and that is what keeps the timeline honest (D3). Rendering is
 * per SESSION: the rounds of one execution session belong together the same way
 * one run's tool calls fold into a single `ExploreBlock` on the execution
 * surface. Drawing one block per row turned a long-lived session into a stack of
 * near-identical cards, which is neither of the two grammars — and on a 390pt
 * screen it is worse than ugly, it pushes the conversation off the device.
 *
 * So this module folds a session's reference rows into ONE item anchored at the
 * session's newest row in the loaded window, and takes with it the chat's own
 * `user_message` rows those rounds were built from: the block renders the
 * question as the round's own line, so the transcript must not hold the same
 * sentence twice (D6's re-delivery rule is the precedent).
 *
 * One exception, ADR-106's presentation-contract amendment: the round that
 * OPENED the session (`meta.opened_new`) keeps its trigger row as its own
 * bubble. A new session otherwise appears beside the words that created it with
 * no visible pairing at all — "a question and an answer, back and forth" is
 * what makes the block read as an exchange rather than an orphan. The block's
 * line for that round then leads with the router's `assignment_brief`, which is
 * what the execution session actually received — not a second copy of the raw
 * sentence.
 *
 * Two shapes stay standalone:
 * - a reference row missing a coordinate addresses nothing, so it has no block
 *   to join and renders its own explicit hole (D3);
 * - a human row that opened a round in ANOTHER session (an `@A @B` fan-out) is
 *   claimed by the first group only, so it stays visible once instead of twice.
 */

import type { ChatMessageRow } from './types'

/** One round as the block sees it. */
export interface RoundItem {
  /** `instance|session|round` — the block read's key (D5). */
  ref: string
  /** The durable reference row this round is folded from. */
  roundRow: ChatMessageRow
  /** The chat's own row that opened the round (`in_reply_to`), when this surface
   *  owns it. Rendered as the round's question line, not as a bubble. */
  triggerRow?: ChatMessageRow
  /** False when the same sentence is already standing on its own in the
   *  transcript (a fan-out trigger claimed by another session's block, or the
   *  opener round of a new session): then the block shows the answer only, so
   *  one message never appears twice. */
  showUserLine: boolean
  /** The trigger row stays a bubble of its own: this round opened the session
   *  (`meta.opened_new`), so the transcript shows the human's words that
   *  created it and the block must not claim them. */
  questionOnTranscript?: boolean
}

/** One execution session's worth of rounds, rendered as a single block. */
export interface RunBlock {
  kind: 'run'
  instanceId: string
  execSessionId: string
  /** Chronological. The newest is the block's anchor row. */
  rounds: RoundItem[]
  anchorRow: ChatMessageRow
}

/** Any other transcript row, rendered as itself. */
export interface PlainItem {
  kind: 'row'
  row: ChatMessageRow
}

export type TranscriptItem = RunBlock | PlainItem

export interface TranscriptLayout {
  items: TranscriptItem[]
  /** Every row's render key, folded ones included — a locate hit must still
   *  scroll to the block that now holds the row. */
  keyOfSeq: Map<number, string>
}

/** The block identity: one execution session, as the reference row names it. A
 *  row missing any of the three coordinates addresses nothing (D3), so it has no
 *  session and no round to name. */
function sessionOf(
  row: ChatMessageRow,
): { instanceId: string; execSessionId: string; roundId: string } | null {
  const instanceId = row.meta?.agent_instance_id?.trim() ?? ''
  const execSessionId = row.meta?.agent_session_id?.trim() ?? ''
  const roundId = row.meta?.round_id?.trim() ?? ''
  if (!instanceId || !execSessionId || !roundId) return null
  return { instanceId, execSessionId, roundId }
}

export function buildTranscriptItems(
  rows: ChatMessageRow[],
  keyOf: (row: ChatMessageRow) => string,
): TranscriptLayout {
  // The chat's own rows, addressable by the id a reference row answers.
  const byMessageId = new Map<string, ChatMessageRow>()
  for (const r of rows) {
    if (r.kind === 'user_message' && r.meta?.message_id) {
      byMessageId.set(r.meta.message_id, r)
    }
  }

  interface Group {
    instanceId: string
    execSessionId: string
    rounds: RoundItem[]
  }
  const groups = new Map<string, Group>()
  // A trigger row is folded into exactly one block; the second claimant shows
  // the answer only, because the question is already on the transcript.
  const claimedTriggers = new Set<string>()

  for (const r of rows) {
    if (r.kind !== 'execution_round') continue
    const coord = sessionOf(r)
    if (!coord) continue
    const key = `${coord.instanceId}|${coord.execSessionId}`
    const group =
      groups.get(key) ?? { instanceId: coord.instanceId, execSessionId: coord.execSessionId, rounds: [] as RoundItem[] }
    if (!groups.has(key)) groups.set(key, group)

    const triggerId = r.meta?.in_reply_to?.trim() ?? ''
    const trigger = triggerId ? byMessageId.get(triggerId) : undefined
    // The round that opened a NEW session is the paired half of an exchange:
    // its question stays on the transcript as its own bubble, and the block
    // line leads with the brief instead of repeating the sentence.
    const opener = r.meta?.opened_new === true && !!trigger
    let showUserLine = true
    if (trigger && !opener) {
      if (claimedTriggers.has(trigger.meta?.message_id ?? '')) {
        showUserLine = false
      } else {
        claimedTriggers.add(trigger.meta?.message_id ?? '')
      }
    } else if (opener) {
      showUserLine = false
    }
    group.rounds.push({
      ref: `${key}|${coord.roundId}`,
      roundRow: r,
      ...(trigger ? { triggerRow: trigger } : {}),
      showUserLine,
      ...(opener ? { questionOnTranscript: true } : {}),
    })
  }

  // A group's anchor is its newest row: the block sits where the session's most
  // recent work is, so the freshest answer stays next to the freshest question
  // instead of stranded at the session's first appearance.
  const anchorOf = new Map<string, ChatMessageRow>()
  for (const [key, group] of groups) {
    anchorOf.set(key, group.rounds[group.rounds.length - 1]!.roundRow)
  }

  const items: TranscriptItem[] = []
  const keyOfSeq = new Map<number, string>()
  const emitted = new Set<string>()

  for (const r of rows) {
    const coord = r.kind === 'execution_round' ? sessionOf(r) : null
    const key = coord ? `${coord.instanceId}|${coord.execSessionId}` : null
    if (key) {
      const group = groups.get(key)!
      const anchor = anchorOf.get(key)!
      // Folded rows still resolve to the block's key, so a locate lands on the
      // block that holds them.
      if (r !== anchor) {
        keyOfSeq.set(r.seq, keyOf(anchor))
        continue
      }
      const triggerSeqs = group.rounds
        // Only the block that OWNS the question row folds it: a fan-out trigger
        // claimed by an earlier session's block is not rendered here, and mapping
        // its seq here would point a locate at a block that does not show it.
        .filter((round) => round.showUserLine)
        .map((round) => round.triggerRow)
        .filter(Boolean)
        .map((row) => row!.seq)
      // The block owns its question rows too: they are the same sentence the
      // round's line shows.
      const skip = new Set(triggerSeqs)
      for (const round of group.rounds) skip.add(round.roundRow.seq)
      for (const seq of skip) keyOfSeq.set(seq, keyOf(anchor))
      if (!emitted.has(key)) {
        emitted.add(key)
        items.push({
          kind: 'run',
          instanceId: group.instanceId,
          execSessionId: group.execSessionId,
          rounds: group.rounds,
          anchorRow: anchor,
        })
      }
      continue
    }
    // A question row this block folded: it renders inside the block.
    const answered =
      r.kind === 'user_message' && r.meta?.message_id && claimedTriggers.has(r.meta.message_id)
    if (answered) {
      keyOfSeq.set(r.seq, keyOf(r))
      continue
    }
    items.push({ kind: 'row', row: r })
    keyOfSeq.set(r.seq, keyOf(r))
  }

  return { items, keyOfSeq }
}

/** The rows a folded block renders as its own question lines — the transcript
 *  must not paint them twice, but the locate path still needs their seq to find
 *  the block. An opener round's trigger is NOT one of them: it renders as its
 *  own bubble, and its seq belongs to that row. */
export function blockSeqs(block: RunBlock): number[] {
  const seqs: number[] = [block.anchorRow.seq]
  for (const round of block.rounds) {
    seqs.push(round.roundRow.seq)
    if (round.triggerRow && !round.questionOnTranscript) seqs.push(round.triggerRow.seq)
  }
  return [...new Set(seqs)]
}
