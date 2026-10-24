/**
 * Live turn state, derived from the durable chat window.
 *
 * ADR-100 revision: the chat log records CONVERSATION, not progress. The
 * dispatch / round-open milestones the original ADR made durable are runtime
 * facts — once a turn is over, "your message reached the agent" and "the round
 * opened" say nothing a reader wants, and persisted they filled every
 * transcript with status lines. So nothing in this module is written anywhere:
 * the surfaces infer "a turn is in flight" from the only durable evidence that
 * exists — a human message whose turn has not produced a terminal row yet.
 *
 * The other half of the same rule: a TERMINAL row (aborted / failed / timeout,
 * empty body) is history and stays. It explains why a message went unanswered,
 * which is exactly what a reader needs and what the intermediate rows could
 * never provide.
 */

import type { ChatMessageRow } from "../../stores/chatSessionStore";

/** Statuses a turn-end write-back can carry (§7.1). A row without one is a
 *  pre-ADR-100 placeholder and is treated as non-terminal. */
const TERMINAL_STATUSES = new Set(["completed", "failed", "aborted", "timeout", "waiting"]);

/** Did this row end its turn? `waiting` counts: the turn handed the ball to
 *  the human with a `pause` card, so the agent is no longer working. */
export function isTerminalReply(row: ChatMessageRow): boolean {
  return row.kind === "agent_message" && TERMINAL_STATUSES.has(row.meta?.status ?? "");
}

/** A terminal reply with no text: the outcome IS the message. These paint as a
 *  centered status line (with the drill into the run), never as an agent
 *  bubble — a bubble with "interrupted" in it reads as if the agent said it. */
export function isEmptyTerminalReply(row: ChatMessageRow): boolean {
  return (
    isTerminalReply(row) &&
    !row.body.trim() &&
    !(row.attachments && row.attachments.length > 0)
  );
}

/** The routing reaction row (ADR-100 D11): the agent's immediate in-chat
 *  answer to "where did my message go" — "started a new task: X" / "continues
 *  task X". It is a `system_notice` by kind because the RUNTIME wrote it, but
 *  it renders as an agent bubble (avatar + name), never as a centered line:
 *  the conversational rule it exists for is that a user message is always
 *  answered by an agent message, not left hanging on a status text. */
export function isTurnRoutedNotice(row: ChatMessageRow): boolean {
  return row.kind === "system_notice" && row.meta?.notice_type === "turn_routed";
}

/** `failed` / `timeout` are errors; `aborted` is a user action and must not
 *  paint red. */
export function isFailureStatus(status?: string): boolean {
  return status === "failed" || status === "timeout";
}

/** i18n key for the centered outcome line of an empty terminal reply. */
export function terminalNoticeKey(status?: string): string {
  switch (status) {
    case "aborted":
      return "chatView.notice.interrupted";
    case "timeout":
      return "chatView.notice.timeout";
    case "failed":
      return "chatView.notice.failed";
    case "waiting":
      return "chatView.notice.waiting";
    default:
      return "chatView.notice.noAnswer";
  }
}

/**
 * Did this human row actually ask an Agent something?
 *
 * The waiting line asserts "an agent is working", so it may only be keyed on a
 * row that opened an agent turn. Which rows those are is a durable fact, not a
 * guess: `/api/intents/chat-message` files its `user_message` with `to` = the
 * agent it delivered to (1:1, a card answer, a pinned execution turn), and the
 * §5.3.1 mention fan-out files ONE broadcast row (`to` empty) carrying the
 * mentioned agents in `meta.mentions`. Everything else — a `user_user` DM, an
 * un-@'d group remark — goes through the archive-only chat write entry, which
 * stores `to: ""` and wakes nobody. Keying the line on those rows made a DM
 * display "working on it" forever: no agent will ever answer a conversation
 * that contains no agent.
 */
export function opensAgentTurn(row: ChatMessageRow): boolean {
  if (row.to?.trim()) return true;
  return (row.meta?.mentions?.length ?? 0) > 0;
}

/**
 * Is the newest human message in this window still waiting for its answer?
 *
 * `sending` covers the HTTP round trip before the human row even lands (the
 * transcript cannot show a turn for a message it does not have yet), so it
 * short-circuits to `true` — on a surface where an agent could answer at all.
 * `agentReachable=false` is a chat with no Agent participant (a `user_user`
 * DM, a humans-only group): there is nobody working, so nothing is shown even
 * while the send is in flight.
 *
 * Once the trigger row exists, the turn is over — and this returns `false` —
 * as soon as any of these appears after it:
 * - a terminal `agent_message` answering it (`in_reply_to`),
 * - an OPEN `interaction_request` (the ball is in the human's court, not the
 *   agent's),
 * - a newer human message that opens its own turn (the new question supersedes
 *   the previous one).
 *
 * Human rows that opened no turn are skipped, not treated as a superseder: an
 * un-@'d remark typed next to a running task does not cancel that task's wait.
 *
 * The routing reaction row (ADR-100 D11) does NOT close the wait: it answers
 * "where did my message go" within seconds, but the run is still in flight,
 * and the live line pinned at the tail is the only thing standing between the
 * reaction and the final answer — a run may take hours, and the chat must not
 * fall silent in between (the interaction-feel rule behind D11 itself).
 *
 * A window parked mid-history by a locate contains no newest message at all;
 * callers that can page forward must not render the indicator in that state.
 */
export function pendingTurn(
  rows: ChatMessageRow[],
  sending: boolean,
  closedCards: ReadonlySet<string>,
  agentReachable = true,
): boolean {
  if (!agentReachable) return false;
  if (sending) return true;
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i]!;
    if (row.kind !== "user_message" || !opensAgentTurn(row)) continue;
    const trigger = row.meta?.message_id;
    // A human row without an id cannot be answered by anything — nothing to
    // wait on either (a pre-ADR-099 hand-written row).
    return !!trigger && !answeredAfter(rows, i, trigger, closedCards);
  }
  return false;
}

/** Does anything after `fromIndex` close the turn opened by `trigger`? */
function answeredAfter(
  rows: ChatMessageRow[],
  fromIndex: number,
  trigger: string,
  closedCards: ReadonlySet<string>,
): boolean {
  for (let i = fromIndex + 1; i < rows.length; i++) {
    const row = rows[i]!;
    if (row.kind === "agent_message" && row.meta?.in_reply_to === trigger && isTerminalReply(row)) {
      return true;
    }
    if (row.kind === "interaction_request" && row.meta?.in_reply_to === trigger) {
      const cardId = row.meta?.message_id;
      if (!cardId || !closedCards.has(cardId)) return true;
    }
  }
  return false;
}
