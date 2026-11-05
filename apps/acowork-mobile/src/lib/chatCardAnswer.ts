/**
 * Answering an addressing card is a RE-DELIVERY (ADR-106 D6) — the mobile twin
 * of Desktop's `lib/chatCardAnswer`.
 *
 * A `session_choice` / `confirm_continue` card asks "which task did you mean?"
 * about a message the human already wrote. So the answer is not a new
 * utterance: it is the same words, now addressed, sent again. Carrying the
 * original row's id as `message_id` is what keeps that honest — the store
 * dedupes on it, so the transcript holds ONE message while the delivery goes
 * out, and a surface that re-typed the text would show the human the same
 * sentence twice for a single decision.
 *
 * Mobile is 1:1 only (design 27 §5.4.4: the pair has exactly one Chat session,
 * no group surface), so the first level of the `@` is degenerate and the
 * address takes the single-session form — `directCardAddress`, never the
 * group's per-agent `target_sessions`.
 *
 * What stays HERE is only the click: the answer's content comes from the
 * service (`fetchCardAnswer`, design 31 §6). Rebuilding it from the loaded
 * window — as this module used to — made a card whose message had scrolled
 * away render buttons that silently did nothing.
 */

import type { CardAnswerFacts } from './api'
import type { ChatAddress, ChatRefPick } from '../stores/chatSessionStore'
import type { ChatMessageRow } from './types'

/** What the human clicked. */
export interface CardChoice {
  sessionId?: string
  forceNew?: boolean
}

/** The facts a re-delivery needs. */
export interface CardAnswer {
  text: string
  attachmentIds: string[]
  /** The agent that asked — the instance the address belongs to. */
  agentId: string
  /** The row being re-delivered; sent as `message_id` so it stays one row. */
  messageId: string
  sessionId?: string
  forceNew?: boolean
  /** ADR-106 revision 2026-11-04: the reference material the ORIGINAL utterance
   *  carried. A card answer is the same sentence, so its quoted material must
   *  not be lost — the pre-pinned `{round, terminal}` coordinates ride back
   *  verbatim and the Gateway passes them through instead of re-pinning. */
  refs?: ChatRefPick[]
}

/**
 * Fold the human's click into the service's answer.
 *
 * `facts` is what the service resolved from the log (`fetchCardAnswer`) — the
 * original words, their attachments and the row's pinned material. This
 * function adds only the address the human just chose; `null` when the click
 * named neither a session nor new work, because a delivery with no address is
 * not a decision the human made.
 */
export function answerWithChoice(
  facts: CardAnswerFacts,
  choice: CardChoice,
): CardAnswer | null {
  // The card's author IS the agent whose runs are on offer (`from` on the
  // notice write); a card with no author cannot be addressed at all.
  const agentId = facts.agentId.trim()
  if (!agentId || !facts.messageId) return null
  const sessionId = choice.sessionId?.trim() || undefined
  const forceNew = choice.forceNew === true
  if (!sessionId && !forceNew) return null
  return {
    text: facts.text,
    attachmentIds: facts.attachmentIds,
    agentId,
    messageId: facts.messageId,
    sessionId,
    forceNew,
    ...(facts.refs.length > 0 ? { refs: facts.refs } : {}),
  };
}

/** Whether this human may answer the card (ADR-106 D10). The card asks the
 *  person who spoke — `to` is them — so a bystander gets the question without
 *  the answer. An empty `me` is local mode, where the surface cannot resolve an
 *  id because there is exactly one person: refusing there would hide the only
 *  answerable card in the app. */
export function mayAnswerCard(card: ChatMessageRow, me: string): boolean {
  return !me || !card.to || card.to === me
}

/** The 1:1 form of the answer's address (the first level is degenerate there,
 *  so one session id). */
export function directCardAddress(answer: CardAnswer): ChatAddress {
  const base: ChatAddress = answer.sessionId
    ? { targetSession: answer.sessionId, messageId: answer.messageId }
    : { forceNew: true, messageId: answer.messageId }
  return answer.refs?.length ? { ...base, refs: answer.refs } : base
}
