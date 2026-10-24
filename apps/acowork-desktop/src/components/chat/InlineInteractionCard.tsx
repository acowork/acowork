/**
 * InlineInteractionCard — the shared renderer for a durable
 * `interaction_request` card (design 27 §5.5.4).
 *
 * A card is a decision the agent asked a human to make (ask_question /
 * approval) — an actionable thing that lives at a point in the transcript,
 * NOT a live status mirror. Extracted from the group surface so the 1:1
 * thread, the project group and the PM-dispatched task turns all answer a
 * card through one grammar and one set of affordances (options as accent
 * buttons, a free-text answer, an answered/closed receipt once resolved).
 *
 * Answering rides the chat answer channel (`onAnswer` → a `user_message`
 * with `in_reply_to` = the card id); the Gateway resolves it against the
 * execution session (approval / answer), continuing the ORIGINAL turn.
 */

import { useTranslation } from "../../i18n/useTranslation";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

interface InlineInteractionCardProps {
  row: ChatMessageRow;
  /** Cards that can no longer be answered (§5.5.4). */
  closed: Set<string>;
  /** The subset the human actually replied to — the receipt line says
   *  "Answered", anything else that closed (turn ended, timeout, stop)
   *  says "Closed". */
  answered: Set<string>;
  /** Resolved display name of the asking agent (the group resolves the
   *  participant; a 1:1 passes its single agent's name). */
  askerName?: string;
  /** Admin viewing a session they never joined: the question is shown,
   *  never answerable — posting into someone else's conversation stays
   *  forbidden. */
  readOnly?: boolean;
  /** Answer the card. `immediate=true` → the chosen option text is sent now;
   *  `immediate=false` → a free-text answer: the caller focuses its composer
   *  instead of sending an empty message. */
  onAnswer: (row: ChatMessageRow, text: string, immediate: boolean) => void;
}

export function InlineInteractionCard({
  row,
  closed,
  answered,
  askerName,
  readOnly,
  onAnswer,
}: InlineInteractionCardProps) {
  const { t } = useTranslation();
  const it = row.meta?.interaction;
  const question = it?.question || row.body;
  const mid = row.meta?.message_id ?? "";

  if (closed.has(mid)) {
    return (
      <div className="mx-6 my-1 rounded-md border border-border-divider bg-chat-bubble px-3 py-2 text-xs text-text-tertiary">
        {answered.has(mid) ? t("chatView.cardAnswered") : t("chatView.cardClosed")}
        : {question}
      </div>
    );
  }
  if (readOnly) {
    // Read-only view: show the question, no answer affordance.
    return (
      <div className="mx-6 my-2 rounded-lg border border-border-divider bg-chat-bubble p-3">
        {askerName && <div className="mb-1 text-10 text-text-tertiary">{askerName}</div>}
        <div className="text-xs font-medium text-text-primary">{question}</div>
      </div>
    );
  }
  return (
    <div className="mx-6 my-2 rounded-lg border border-[var(--color-accent)]/30 bg-chat-bubble p-3">
      {askerName && <div className="mb-1 text-10 text-text-tertiary">{askerName}</div>}
      <div className="text-xs font-medium text-text-primary">{question}</div>
      {it?.options && it.options.length > 0 ? (
        <div className="mt-2 flex flex-wrap gap-2">
          {it.options.map((opt) => (
            <button
              key={opt}
              onClick={() => onAnswer(row, opt, true)}
              className="rounded btn-accent px-3 py-1.5 text-xs"
            >
              {opt}
            </button>
          ))}
          <button
            onClick={() => onAnswer(row, "", false)}
            className="rounded btn-solid px-3 py-1.5 text-xs"
          >
            {t("chatView.freeAnswer")}
          </button>
        </div>
      ) : (
        <div className="mt-2">
          <button
            onClick={() => onAnswer(row, "", false)}
            className="rounded btn-accent px-3 py-1.5 text-xs"
          >
            {t("chatView.answer")}
          </button>
        </div>
      )}
    </div>
  );
}
