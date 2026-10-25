/**
 * ReplyQuote — the one line above an answer bubble that names what it answers.
 *
 * Design 27 §5.5.4 records a card answer on the answer row itself: the human's
 * `user_message` carries `in_reply_to` = the card's message id. Until now that
 * link was invisible — the only visible cue was the answered receipt card,
 * which repeated the question in a full card shell (header band, hairline,
 * padding) directly above. That was redundant twice over: the question had
 * just been read in the open card, and the answer text sat right below it.
 *
 * So the receipt is gone and this line carries the link instead: an IM-style
 * quote, one line, bounded. It rides above the bubble as an independent unit —
 * the same grammar as the attachment chips and the task chip — because the own
 * bubble is filled with the highlight color (`--color-chat-user`) and white
 * text, and a tertiary-gray quote inside it would be unreadable.
 */

import { CornerDownRight } from "lucide-react";

/** Bound a quoted question so one line stays one line. Longer than a task-name
 *  fallback (see `truncateTaskName`) because this is the question itself, not a
 *  label for it. */
export function truncateQuote(text: string | undefined): string {
  const collapsed = (text ?? "").trim().replace(/\s+/g, " ");
  if (!collapsed) return "";
  return collapsed.length > 60 ? `${collapsed.slice(0, 60)}…` : collapsed;
}

interface ReplyQuoteProps {
  /** The quoted question (the card row's body). Empty or blank paints nothing. */
  quote: string | undefined;
  /** Bubble side the answer is on: 'end' = the human's own right-aligned bubble. */
  align?: "start" | "end";
}

export function ReplyQuote({ quote, align = "end" }: ReplyQuoteProps) {
  const text = truncateQuote(quote);
  if (!text) return null;
  return (
    <div
      className={`mb-1 flex max-w-full items-center gap-1.5 ${align === "end" ? "justify-end" : "justify-start"}`}
      title={quote}
    >
      {/* Neutral hairline strip + tertiary label: the highlight belongs to the
          bubble fill, never to a quote rule. */}
      <span className="flex min-w-0 items-center gap-1.5 border-l-2 border-border-divider pl-1.5 text-10 text-text-tertiary">
        <CornerDownRight className="h-3 w-3 shrink-0 opacity-60" aria-hidden />
        <span className="truncate">{text}</span>
      </span>
    </div>
  );
}
