/**
 * ChatEventText — the ONE renderer for a chat transcript's "event text": the
 * small tertiary line that is neither a participant's utterance (a bubble) nor
 * a control. Three things produce one today: the in-flight wait this surface
 * states about its own send, a channel receipt, and a block's placeholder while
 * the round it names has nothing readable to show.
 *
 * ## The grammar
 *
 * The line belongs to **no single utterance**: it speaks for the transcript, so
 * it is **centered** and **icon-less**. Anything a participant said wears a
 * bubble; anything about one round of work lives inside that round's block
 * (ADR-106 D3/D8). The row-anchored note this component used to paint — "a card
 * closed with nobody answering it" — retired with the chat-side card row, and
 * with it the second tier: there is no longer a row-shaped fact to attach a
 * glyph to.
 *
 * ## Why it is one component
 *
 * The rules were already in the protocol (design 27 §5.5.5: a turn's outcome
 * renders as a centered status line) — but each renderer wrote its own class
 * string, so the same-looking gray text landed on two different axes and read
 * as "placed at random". One renderer, one axis.
 *
 * Not in this grammar: the paging bands ("load earlier" / "jump to latest") are
 * list chrome and always centered, and the execution surface's
 * iteration-limit / error notices are boxed banners with their own chrome.
 */
import { cn } from "../../lib/utils";

interface ChatEventTextProps {
  /** Already-localized text. */
  text: string;
  /** An in-flight fact shimmers, the same `thinking-shimmer` the execution
   *  view's "running a tool" line wears, so "someone is busy" reads the same
   *  everywhere. A settled outcome is quiet. */
  live?: boolean;
  /** `failed` / `timeout` paint as an error. `aborted` must NOT: a stop is the
   *  human's action, not a fault of the run. */
  danger?: boolean;
  /** Trailing affordance — the retry on a block that could not be read. */
  trailing?: React.ReactNode;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
}

export function ChatEventText({
  text,
  live,
  danger,
  trailing,
  className,
}: ChatEventTextProps) {
  // `data-event-text` is the axis made greppable: the regression tests assert
  // it, and a reviewer can see at a glance that a line is transcript chrome
  // rather than someone's speech.
  return (
    <div
      data-event-text="turn"
      className={cn("flex items-center justify-center gap-1.5 py-1", className)}
      role={live ? "status" : undefined}
    >
      <span
        className={cn(
          "text-10 text-text-tertiary",
          live && "thinking-shimmer",
          danger && "text-red-500",
        )}
      >
        {text}
      </span>
      {trailing}
    </div>
  );
}