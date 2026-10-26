/**
 * ChatEventText — the ONE renderer for a chat transcript's "event text": the
 * small tertiary line that is neither a participant's utterance (a bubble) nor
 * a control. Three things produce one today: a turn's terminal outcome, the
 * in-flight wait line, a channel receipt, and the note a decision card leaves
 * behind when its turn ended with nobody answering it.
 *
 * ## The grammar
 *
 * The axis follows the FACT, not the surface or the component:
 *
 *   - `anchor="turn"` — the fact belongs to **no single row**: the turn's
 *     outcome ("interrupted" / "failed" / "timeout"), the derived wait, a
 *     channel receipt. There is nothing to attach it to, so it is **centered**
 *     and **icon-less**: the line speaks for the whole transcript.
 *   - `anchor="row"` — the fact belongs to **one row** and occupies that row's
 *     slot: a card that closed with nobody answering it. It stays in that row's
 *     **content column** (`ml-12`, the same gutter as the bubble body) and
 *     leads with the kind glyph, because the glyph is what says *which*
 *     row-level fact this is.
 *
 * The distinguishing cue is therefore deliberate and single: an icon means
 * "this belongs to one row", no icon means "this is the turn / the channel
 * talking". The two tiers exist because a row-anchored note must stay legible
 * as a note about the card right above it, while a turn-level line belongs to
 * the column, not to a row.
 *
 * ## Why it is one component
 *
 * The rules were already in the protocol (design 27 §5.5.5: an empty-body
 * terminal renders as a centered status line) and in
 * `docs/plan/zh/chat-card-receipt-to-reply-quote-plan.md` §3.2 (the unanswered
 * note stays in the card's own slot) — but each renderer wrote its own class
 * string, so the same-looking gray text landed on two different axes and read
 * as "placed at random". Choosing an `anchor` is now the only way to render
 * one: a new event text has to declare which fact it states.
 *
 * Not in this grammar: the paging bands ("load earlier" / "jump to latest") are
 * list chrome and always centered, and the execution surface's
 * iteration-limit / error notices are boxed banners with their own chrome.
 */
import { cn } from "../../lib/utils";

interface ChatEventTextProps {
  /** Which fact this line states — see the grammar above. */
  anchor: "turn" | "row";
  /** Already-localized text. */
  text: string;
  /** Full text behind a truncated `row` line (the row it comments on). The
   *  `turn` tier is never truncated, so it ignores this. */
  title?: string;
  /** Kind glyph — `row` only (see the grammar: the glyph marks a row-level
   *  fact). */
  icon?: React.ReactNode;
  /** An in-flight fact shimmers, the same `thinking-shimmer` the execution
   *  view's "running a tool" line wears, so "someone is busy" reads the same
   *  everywhere. A settled outcome is quiet. */
  live?: boolean;
  /** `failed` / `timeout` paint as an error. `aborted` must NOT: a stop is the
   *  human's action, not a fault of the run. */
  danger?: boolean;
  /** Trailing affordance on a `turn` line — the "view run" drill. */
  trailing?: React.ReactNode;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
}

export function ChatEventText({
  anchor,
  text,
  title,
  icon,
  live,
  danger,
  trailing,
  className,
}: ChatEventTextProps) {
  // `data-event-text` is the axis made greppable: the regression tests assert
  // it, and a reviewer can see at a glance which tier a line claims to be.
  if (anchor === "row") {
    return (
      <div
        data-event-text="row"
        className={cn(
          "ml-12 my-1 flex max-w-[var(--content-max-width)] items-center gap-1.5 text-10 text-text-tertiary",
          className,
        )}
        title={title}
      >
        {icon}
        <span className="min-w-0 truncate">{text}</span>
      </div>
    );
  }

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