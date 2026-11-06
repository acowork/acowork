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
  CardAnswerFacts,
  ChatAddress,
  ChatMessageRow,
  ChatRefPick,
} from "../stores/chatSessionStore";

/** What the human clicked. */
export interface CardChoice {
  sessionId?: string;
  forceNew?: boolean;
}

/** What delivering a click reports back to the card.
 *
 *  `false` is the only value with a meaning the card acts on: it says "nothing
 *  was delivered", so the question re-opens instead of staying busy on a click
 *  that never moved. Everything else (`true`, `void`) means the click became a
 *  delivery, and the card then holds its busy state until the projection answers
 *  it away — a surface that cannot tell does not get to promise a retry. */
export type CardChooseResult = void | boolean;

/** One click on a card's option. The card awaits it; see
 *  [`CardChooseResult`] for what the answer changes. */
export type CardChooseHandler = (
  choice: CardChoice,
) => CardChooseResult | Promise<CardChooseResult>;

/** A surface's card-answer handler: the click plus the row it belongs to (the
 *  row names the message the answer is re-delivered under). */
export type CardAnswerHandler = (
  card: ChatMessageRow,
  choice: CardChoice,
) => CardChooseResult | Promise<CardChooseResult>;

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
  /** ADR-106 revision 2026-11-04: the original row's `@session` material,
   *  pinned coordinates and all, re-sent verbatim so a re-delivery of the SAME
   *  utterance does not lose (or re-decide) the reference the human attached.
   *  Only the answering agent's own refs — a card is that agent's question. */
  refs?: ChatRefPick[];
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
  const agentId = facts.agentId.trim();
  if (!agentId || !facts.messageId) return null;
  const sessionId = choice.sessionId?.trim() || undefined;
  const forceNew = choice.forceNew === true;
  if (!sessionId && !forceNew) return null;
  return {
    text: facts.text,
    attachmentIds: facts.attachmentIds,
    agentId,
    messageId: facts.messageId,
    sessionId,
    forceNew,
    // An empty material list is omitted, not sent as `[]`-noise: "the original
    // row carried no reference" is the absence, and every consumer already
    // reads `refs?.length`.
    ...(facts.refs.length > 0 ? { refs: facts.refs } : {}),
  };
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
        ...(answer.refs?.length ? { refs: answer.refs } : {}),
      }
    : {
        forceNewFor: [answer.agentId],
        messageId: answer.messageId,
        ...(answer.refs?.length ? { refs: answer.refs } : {}),
      };
}

/** The 1:1 form (the first level is degenerate there, so one session id). */
export function directCardAddress(answer: CardAnswer): ChatAddress {
  return answer.sessionId
    ? {
        targetSession: answer.sessionId,
        messageId: answer.messageId,
        ...(answer.refs?.length ? { refs: answer.refs } : {}),
      }
    : {
        forceNew: true,
        messageId: answer.messageId,
        ...(answer.refs?.length ? { refs: answer.refs } : {}),
      };
}
