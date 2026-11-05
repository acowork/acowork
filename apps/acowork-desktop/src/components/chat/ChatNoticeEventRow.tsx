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
 *   whether the question is still open. This one is NOT a quiet line: it is a
 *   card with a header, one button per candidate and a footer escape, because
 *   the human's click is a delivery (D6) and a decision has to look like one.
 *
 * Everything the runtime used to write as progress (dispatch / round-open
 * milestones, the router-fallback receipt, the card-resolution receipt) is gone
 * from the stream: the log records conversation, and a card is no longer a chat
 * row at all (D8) — its answer is an execution-side fact shown inside the block.
 */

import { ChevronRight, CornerDownRight, CornerUpRight, ListTree, Plus } from "lucide-react";
import { ChatEventText, type EventTone } from "./ChatEventText";
import { useTranslation } from "../../i18n/useTranslation";
import { RETIRED_NOTICES } from "../../lib/retiredNotices";
import { cn } from "../../lib/utils";
import type { CardChoice } from "../../lib/chatCardAnswer";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

/** The candidate's state as a dot on its row (design 29 §5.2 vocabulary). A
 *  card must not claim a status it was not told: an option that arrived without
 *  one gets a neutral dot, never "idle". */
function statusTone(status?: string | null): string {
  switch (status) {
    case "working":
      return "bg-amber-500 animate-pulse";
    case "idle":
      return "bg-emerald-500";
    case "errored":
      return "bg-red-500";
    default:
      return "bg-zinc-400 dark:bg-zinc-600";
  }
}

interface ChatNoticeEventRowProps {
  row: ChatMessageRow;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
  /** Which column this row belongs in: `self` / `peer` when the row names a
   *  message the surface can place (a failed send names the human's row), absent
   *  when it names nothing this surface can resolve. A receipt about ONE message
   *  must sit in that message's column — centering it made it read as a fact
   *  about the whole conversation, attached to nothing. */
  anchor?: "self" | "peer";
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
  anchor,
  name,
  onChoose,
}: ChatNoticeEventRowProps) {
  const { t } = useTranslation();
  const noticeType = row.meta?.notice_type ?? "";

  // Retired milestones: history only.
  if (RETIRED_NOTICES.has(noticeType)) return null;
  // An addressing question (ADR-106 D6 levels 3 and 5) — a decision card, and
  // it has to look like one: a hairline shell with a header, full-width option
  // rows that read as buttons (label, the router's reason, a status dot, a
  // chevron), and one quiet escape. The language is `AskQuestionCard`'s, so the
  // same question does not change costume between the chat surface and the
  // execution surface.
  if (noticeType === "session_choice" || noticeType === "confirm_continue") {
    const confirm = noticeType === "confirm_continue";
    const question = t(
      confirm ? "chatView.block.confirmSession" : "chatView.block.chooseSession",
    );
    // The service's projection (design 31 §6): the options the question offers,
    // whether a round already answered it, and which session took the answer.
    const card = row.delivery?.question;
    const options = card?.options ?? [];
    const answered = card?.answered === true;
    // A card with nothing to offer is retired chrome, not a question: the rows
    // that reach a surface like this are pre-projection leftovers (no options,
    // nothing to click), and painting them as a centered sentence in the middle
    // of the transcript made a bare "Continue this task?" look like the agent
    // saying something it never said.
    if (options.length === 0) return null;

    // An answer must NAME what it continued. The payload carries the
    // candidate's own title; the generic wording is only the fallback for an
    // option that arrived with no label at all.
    const labelOf = (option: (typeof options)[number]) =>
      option.label?.trim() ||
      option.session_id?.trim().slice(0, 8) ||
      t("chatView.block.chooseThis");

    // Settled: paint nothing. A card is a question, and an answered question is
    // an intermediate state of that turn, not a record of the conversation —
    // ADR-106 D14 keeps history on disk and hides it at render time, and where
    // the words went is already told by the round's block. Leaving a receipt
    // here stated the same fact a second time, one identical line per test
    // message, floating with no subject of its own.
    if (answered) return null;

    return (
      <div
        className={cn("ml-12 my-1.5 w-fit min-w-[18rem] max-w-[32rem]", className)}
        data-testid="chat-card-options"
      >
        <div className="overflow-hidden rounded-md border border-border-outer">
          {/* Header: icon + the question + whether it still wants an answer. */}
          <div className="flex items-center gap-[7px] border-b border-border-divider bg-chat-title px-2.5 py-[6px] text-11 font-medium text-text-secondary">
            {confirm ? (
              <CornerDownRight className="h-3.5 w-3.5 shrink-0" />
            ) : (
              <ListTree className="h-3.5 w-3.5 shrink-0" />
            )}
            <span className="flex-1 truncate">{question}</span>
            {onChoose && (
              <span className="shrink-0 text-10 font-normal text-text-tertiary">
                {t("chatView.block.unanswered")}
              </span>
            )}
          </div>
          {/* One row per candidate. The whole row is the button: a bordered box
              holding two lines of text reads as a disabled field, not as
              something to press. */}
          <div className="divide-y divide-border-divider bg-surface">
            {options.map((option) => {
              const label = labelOf(option);
              const why = option.why?.trim();
              const row = (
                <>
                  <span
                    className={cn("h-1.5 w-1.5 shrink-0 rounded-full", statusTone(option.status))}
                    aria-hidden="true"
                  />
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-11 font-medium text-text">{label}</span>
                    {why && (
                      <span className="block truncate text-10 text-text-tertiary">{why}</span>
                    )}
                  </span>
                </>
              );
              // A viewer who may not decide (ADR-106 D10) sees the options but
              // not a button: the question is a fact about the channel, the
              // click is the speaker's.
              if (!onChoose) {
                return (
                  <div
                    key={option.session_id}
                    className="flex items-center gap-2 px-2.5 py-1.5"
                    title={why ? `${label} — ${why}` : label}
                  >
                    {row}
                  </div>
                );
              }
              return (
                <button
                  key={option.session_id}
                  type="button"
                  onClick={() => onChoose({ sessionId: option.session_id })}
                  title={why ? `${label} — ${why}` : label}
                  className="group flex w-full items-center gap-2 px-2.5 py-1.5 text-left transition-colors hover:bg-nav-item-hover focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-[var(--color-accent)]"
                >
                  {row}
                  <ChevronRight className="h-3.5 w-3.5 shrink-0 text-text-tertiary transition-transform group-hover:translate-x-0.5" />
                </button>
              );
            })}
          </div>
          {/* D6's cheap escape, demoted to a footer action: "new task" is the
              way OUT of the suggestion, never a second candidate. */}
          {card?.allow_new_task && onChoose && (
            <div className="border-t border-border-divider bg-surface">
              <button
                type="button"
                onClick={() => onChoose({ forceNew: true })}
                className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-11 text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-text-secondary"
              >
                <Plus className="h-3.5 w-3.5 shrink-0" />
                {t("chatView.block.newTask")}
              </button>
            </div>
          )}
          {!onChoose && (
            <div className="border-t border-border-divider bg-surface px-2.5 py-1.5 text-10 text-text-tertiary">
              {t("chatView.block.answeredByOwner")}
            </div>
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
  // A row-anchored note: the arrow says "this refers back to what was just
  // sent", the column says whose message it is. Both only when the surface could
  // place the row — an unplaceable receipt stays a centered channel fact rather
  // than claiming an owner it cannot name.
  const note = (text: string, tone?: EventTone) => (
    <ChatEventText
      text={text}
      tone={tone}
      align={anchor === "self" ? "end" : anchor === "peer" ? "start" : "center"}
      leading={
        anchor ? <CornerUpRight className="h-3 w-3 shrink-0 opacity-70" /> : undefined
      }
      className={className}
    />
  );

  if (noticeType === "delivery_failed") {
    // A send this Chat owed and could not make. `warn`, not `danger`: the
    // receipt is retry-able and nothing in the run broke. Quiet gray made the
    // one receipt that exists to be noticed the quietest thing on screen; red
    // made it the loudest, and left no headroom for a run that really failed.
    return note(
      name ? t("chatView.deliveryFailed", { name }) : t("chatView.deliveryFailedNoTarget"),
      "warn",
    );
  }

  const text = row.body || row.meta?.status || "";
  if (!text) return null;
  return note(text);
}
