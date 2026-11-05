/**
 * ChatNoticeEventRow — the shared renderer for `system_notice` chat rows.
 *
 * A notice is a fact about the channel, never an utterance, so it paints as a
 * quiet centered line and carries no avatar. Three things still write one
 * (ADR-106 D12, after the reference-row consolidation):
 * - `delivery_failed`: a send this Chat owed the human and could not make. The
 *   Gateway writes it bodyless — the wording belongs to the surface, not to a
 *   server-side English sentence with an error code in it (ADR-105).
 * - `budget_exhausted` / `truncated` / `unavailable`: a limit the round hit.
 * - `session_choice` / `confirm_continue`: an addressing question (D6 levels 3
 *   and 5). Empty-bodied: its options and its answered-ness arrive as the
 *   service's projection (`row.delivery.question`, design 31 §6), and the
 *   surfaces render that verbatim instead of reading `meta` and re-deriving
 *   whether the question is still open.
 *
 * Everything the runtime used to write as progress (dispatch / round-open
 * milestones, the router-fallback receipt, the card-resolution receipt) is gone
 * from the stream: the log records conversation, and a card is no longer a chat
 * row at all (D8) — its answer is an execution-side fact shown inside the block.
 */

import { ChatEventText } from "./ChatEventText";
import { useTranslation } from "../../i18n/useTranslation";
import { RETIRED_NOTICES } from "../../lib/retiredNotices";
import { cn } from "../../lib/utils";
import type { CardChoice } from "../../lib/chatCardAnswer";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

interface ChatNoticeEventRowProps {
  row: ChatMessageRow;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
  /** Display name of the party this row is about, when the surface knows it
   *  (the group panels resolve it from the roster). Only the `delivery_failed`
   *  receipt uses it: "the message never reached <name>" is actionable, while a
   *  generic "not delivered" leaves the human guessing which agent to re-@. */
  name?: string;
  /** Answering an addressing card. Absent means the row is not answerable here:
   *  a read-only viewer never gets the action (ADR-106 D10 — only the person who
   *  spoke may decide where their words go), and the option buttons are part of
   *  the answer, not decoration. The choice is turned into a re-delivery of the
   *  human's own utterance by `chatCardAnswer` (D6). */
  onChoose?: (choice: CardChoice) => void;
}

export function ChatNoticeEventRow({
  row,
  className,
  name,
  onChoose,
}: ChatNoticeEventRowProps) {
  const { t } = useTranslation();
  const noticeType = row.meta?.notice_type ?? "";

  // Retired milestones: history only.
  if (RETIRED_NOTICES.has(noticeType)) return null;
  // An addressing question (ADR-106 D6 levels 3 and 5). Its text is the option
  // list, which the surfaces paint as buttons — the line below is only the
  // question they ask in common, localized here rather than server-side.
  if (noticeType === "session_choice" || noticeType === "confirm_continue") {
    const question = t(
      noticeType === "session_choice"
        ? "chatView.block.chooseSession"
        : "chatView.block.confirmSession",
    );
    // The service's projection: the options the question offers and whether a
    // round already answered it. A card that reached the transcript without
    // options has none to project — that is an unanswerable line, which is what
    // the plain sentence below reports.
    const card = row.delivery?.question;
    const options = card?.options ?? [];
    const answered = card?.answered === true;
    // Nothing to offer, or nobody allowed to offer it: the question is still a
    // fact about the transcript, so it stays as the plain line.
    if (!onChoose || options.length === 0) {
      return <ChatEventText text={question} className={className} />;
    }
    // The card's own row is a question, so it reads as one: the line above, the
    // answers below, all centered like every other transcript chrome.
    return (
      <div
        className={cn("flex flex-col items-center gap-1.5 py-1", className)}
        data-testid="chat-card-options"
      >
        <span className="text-10 text-text-tertiary">{question}</span>
        <div className="flex flex-wrap items-center justify-center gap-1.5">
          {options.map((option) => {
            // An answer must NAME what it continues. The payload carries the
            // candidate's own title (the session's), and `confirm_continue` also
            // carries the router's reason for proposing it — a card that said
            // only "Continue this one" made the human pick a task blind. The
            // generic wording survives only as the fallback for an option that
            // arrived with no label at all, where there is nothing better to say.
            const label =
              option.label?.trim() ||
              option.session_id?.trim().slice(0, 8) ||
              t("chatView.block.chooseThis");
            const why = option.why?.trim();
            return (
              <button
                key={option.session_id}
                type="button"
                disabled={answered}
                onClick={() => onChoose({ sessionId: option.session_id })}
                title={why ? `${label} — ${why}` : label}
                className={cn(
                  "flex max-w-[26rem] flex-col items-center gap-0.5 rounded-md border border-border-outer px-2 py-1 text-xs",
                  answered
                    ? "text-text-disabled"
                    : "text-text-secondary hover:bg-nav-item-hover hover:text-text",
                )}
              >
                <span className="max-w-full truncate">{label}</span>
                {why && (
                  <span className="max-w-full truncate text-10 opacity-70">{why}</span>
                )}
              </button>
            );
          })}
          {/* D6's cheap escape: a question must be answerable in one click, and
              "new task" sits beside every suggestion rather than below a fold. */}
          {card?.allow_new_task && (
            <button
              type="button"
              disabled={answered}
              onClick={() => onChoose({ forceNew: true })}
              className={cn(
                "rounded-md border border-border-outer px-2 py-1 text-xs",
                answered
                  ? "text-text-disabled"
                  : "text-text-secondary hover:bg-nav-item-hover hover:text-text",
              )}
            >
              {t("chatView.block.newTask")}
            </button>
          )}
        </div>
      </div>
    );
  }
  // A delivery that never reached its agent (ADR-105). Also keyed by
  // notice_type: the Gateway writes a bodyless row (its prose would be
  // server-side English plus an internal error code), so the surfaces own the
  // wording. It MUST paint — this receipt exists precisely because the failure
  // used to be invisible.
  if (noticeType === "delivery_failed") {
    return (
      <ChatEventText
        text={name ? t("chatView.deliveryFailed", { name }) : t("chatView.deliveryFailedNoTarget")}
        className={className}
      />
    );
  }

  const text = row.body || row.meta?.status || "";
  if (!text) return null;
  return <ChatEventText text={text} className={className} />;
}
