/**
 * ChatEventText — the ONE renderer for a chat transcript's "event text": the
 * small tertiary line that is neither a participant's utterance (a bubble) nor
 * a control. Three things produce one today: the in-flight wait this surface
 * states about its own send, a channel receipt, and a block's placeholder while
 * the round it names has nothing readable to show.
 *
 * ## The grammar
 *
 * A line with **no subject** speaks for the transcript, so it is centered and
 * icon-less. A line that names a row — a failed send, a delivery the channel
 * refused — is NOT ownerless: it belongs in the column of the message it is
 * about, pointing back at it (see `align`). Centering those made a fact about
 * one message read as a fact about the conversation, floating with nothing to
 * attach it to.
 *
 * ## Why it is one component
 *
 * The rules were already in the protocol (design 27 §5.5.5: a turn's outcome
 * renders as a centered status line) — but each renderer wrote its own class
 * string, so the same-looking gray text landed on two different axes and read
 * as "placed at random". One renderer, one axis.
 *
 * A line's colour is a TONE on the same axis, not an alarm: `quiet` is the
 * platform stating a fact, `warn` is something the human may need to act on but
 * that broke nothing (a send the Chat could not deliver — retry it), and
 * `danger` is an actual fault of the run. Painting every failure red made the
 * one retry-able receipt the loudest thing in the transcript, and left nothing
 * louder for a run that really did fail.
 *
 * Not in this grammar: the paging bands ("load earlier" / "jump to latest") are
 * list chrome and always centered, and the execution surface's
 * iteration-limit / error notices are boxed banners with their own chrome.
 */
import { cn } from "../../lib/utils";

/** `quiet` (default) states a fact, `warn` asks for an action that broke
 *  nothing, `danger` reports a fault of the run. */
export type EventTone = "quiet" | "warn" | "danger";

interface ChatEventTextProps {
  /** Already-localized text. */
  text: string;
  /** An in-flight fact shimmers, the same `thinking-shimmer` the execution
   *  view's "running a tool" line wears, so "someone is busy" reads the same
   *  everywhere. A settled outcome is quiet. */
  live?: boolean;
  /** How loud the line is — see the docblock. `aborted` must NOT be `danger`:
   *  a stop is the human's action, not a fault of the run. */
  tone?: EventTone;
  /** Trailing affordance — the retry on a block that could not be read. */
  trailing?: React.ReactNode;
  /** Leading glyph — the "this refers back to that message" arrow a row-anchored
   *  note wears. */
  leading?: React.ReactNode;
  /**
   * Which column the line sits in. `center` (default) is for a fact about the
   * transcript; `end` / `start` are for a fact about ONE message, placed in the
   * sender's own column so ownership is visible, not inferred.
   */
  align?: "center" | "start" | "end";
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
}

export function ChatEventText({
  text,
  live,
  tone = "quiet",
  trailing,
  leading,
  align = "center",
  className,
}: ChatEventTextProps) {
  // ONE tone, applied to the line's parts rather than to its box: the glyph the
  // reader's eye lands on first and the words beside it must not disagree about
  // how loud the fact is (a gray arrow next to a red sentence is how the failed
  // send used to read as two unrelated things).
  const toneClass =
    tone === "warn"
      ? // The QUIET warning token: a receipt the reader sees every turn is a
        // hint about what to do next, not an alarm. Hard-coded hues are how the
        // transcript ended up with a red that outshouted a failed run.
        "text-[var(--color-warning-quiet)]"
      : tone === "danger"
        ? "text-red-500"
        : "text-text-tertiary";

  // `data-event-text` is the axis made greppable: the regression tests assert
  // it, and a reviewer can see at a glance that a line is transcript chrome
  // rather than someone's speech.
  return (
    <div
      data-event-text="turn"
      data-align={align}
      data-tone={tone}
      className={cn(
        "flex items-center gap-1.5 py-1",
        align === "center" && "justify-center",
        align === "start" && "justify-start",
        align === "end" && "justify-end text-right",
        className,
      )}
      role={live ? "status" : undefined}
    >
      {leading && <span className={cn("flex shrink-0 items-center", toneClass)}>{leading}</span>}
      <span
        className={cn(
          "text-10",
          toneClass,
          live && "thinking-shimmer",
        )}
      >
        {text}
      </span>
      {trailing}
    </div>
  );
}