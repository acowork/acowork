/**
 * chatCardAnswer — answering an addressing card is a RE-DELIVERY (ADR-106 D6).
 *
 * A `session_choice` / `confirm_continue` card asks "which task did you mean?"
 * about a message the human already wrote. So the answer is not a new
 * utterance: it is the same words, now addressed, sent again. Carrying the
 * original row's id as `message_id` is what keeps that honest — the store
 * dedupes on it, so the transcript holds ONE message while the delivery goes
 * out, and a surface that re-typed the text would show the human the same
 * sentence twice for a single decision.
 *
 * The address names only the agent that asked. A card is that agent's question
 * about its own runs, so answering it must not re-route a second agent's turn
 * — which is why the group surface sends `target_sessions` / `force_new_for`
 * keyed by that instance, and the 1:1 surface sends the single-session form.
 */

import type {
  ChatAddress,
  ChatMessageRow,
} from "../stores/chatSessionStore";

/** What the human clicked. */
export interface CardChoice {
  sessionId?: string;
  forceNew?: boolean;
}

/** The facts a re-delivery needs. The address shape is the surface's own: the
 *  group channel names a session per agent, the 1:1 channel names one. */
export interface CardAnswer {
  text: string;
  attachmentIds: string[];
  /** The agent that asked — the instance the address belongs to. */
  agentId: string;
  /** The row being re-delivered; sent as `message_id` so it stays one row. */
  messageId: string;
  sessionId?: string;
  forceNew?: boolean;
}

/**
 * Turn a card row + the human's click into a re-delivery.
 *
 * `original` is the row the card asked about (`meta.in_reply_to`), looked up by
 * the surface in its own window. `null` whenever the answer cannot be a faithful
 * re-delivery — a card pointing at a row that is no longer readable, or a click
 * that named neither a session nor new work. Refusing there is the point: a
 * guessed-at text would put words in the human's mouth.
 */
export function buildCardAnswer(
  card: ChatMessageRow,
  original: ChatMessageRow | undefined,
  choice: CardChoice,
): CardAnswer | null {
  const messageId = card.meta?.in_reply_to?.trim();
  if (!messageId || !original) return null;
  // The card's author IS the agent whose runs are on offer (`from` on the
  // notice write); a card with no author cannot be answered with an address.
  const agentId = card.from?.trim();
  if (!agentId) return null;
  const sessionId = choice.sessionId?.trim() || undefined;
  const forceNew = choice.forceNew === true;
  if (!sessionId && !forceNew) return null;
  const text = original.body ?? "";
  // Resolved attachment meta when the row carries it, otherwise the raw ids —
  // the re-delivery must not quietly drop the files the original message had.
  const attachmentIds = original.attachments
    ? original.attachments.map((a) => a.id)
    : original.attachment_ids ?? [];
  return { text, attachmentIds, agentId, messageId, sessionId, forceNew };
}

/** Whether this human may answer the card (ADR-106 D10). The card asks the
 *  person who spoke — `to` is them — so a bystander gets the question without
 *  the answer. An empty `me` is local mode, where the surface cannot resolve an
 *  id because there is exactly one person: refusing there would hide the only
 *  answerable card in the app. */
export function mayAnswerCard(card: ChatMessageRow, me: string): boolean {
  return !me || !card.to || card.to === me;
}

/** The group form of the answer's address (D7's per-agent `target_sessions`). */
export function groupCardAddress(answer: CardAnswer): ChatAddress {
  return answer.sessionId
    ? {
        targetSessions: { [answer.agentId]: answer.sessionId },
        messageId: answer.messageId,
      }
    : { forceNewFor: [answer.agentId], messageId: answer.messageId };
}

/** The 1:1 form (the first level is degenerate there, so one session id). */
export function directCardAddress(answer: CardAnswer): ChatAddress {
  return answer.sessionId
    ? { targetSession: answer.sessionId, messageId: answer.messageId }
    : { forceNew: true, messageId: answer.messageId };
}
