/**
 * ChatTurnStatusLine — the centered, chrome-free line that carries a chat
 * turn's STATE, shared by the 1:1 thread and the group panel.
 *
 * ADR-100 revision: state is not conversation. A turn's dispatch / round-open
 * milestones are derived at render time (see `chatTurnState`) and never
 * persisted; the only durable state rows left are terminal outcomes
 * (aborted / failed / timeout), which explain why a message went unanswered.
 *
 * Either way the line stays OUT of a bubble. A bubble belongs to something a
 * participant said; "interrupted" is the platform talking, and padding it into
 * an agent-shaped bubble made a stop read like the agent's own words.
 */

import { cn } from "../../lib/utils";
import { ViewRunButton } from "./ViewRunButton";

interface ChatTurnStatusLineProps {
  /** Already-localized status text. */
  text: string;
  /** A turn still in flight shimmers — the same `thinking-shimmer` treatment
   *  as the execution view's "running a tool" line, so "the agent is busy"
   *  reads the same everywhere. A settled outcome is quiet. */
  live?: boolean;
  /** `failed` / `timeout` paint as an error. `aborted` must NOT: a stop is a
   *  user action, not a fault of the run. */
  danger?: boolean;
  /** Drill into the execution session behind this outcome. */
  onJump?: () => void;
  /** Name of that session — the drill button's tooltip. */
  runName?: string;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
}

export function ChatTurnStatusLine({
  text,
  live,
  danger,
  onJump,
  runName,
  className,
}: ChatTurnStatusLineProps) {
  return (
    <div
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
      {onJump && (
        <ViewRunButton
          runName={runName}
          onJump={onJump}
          className="inline-flex shrink-0 items-center gap-1 text-10 text-text-tertiary transition-colors hover:text-[var(--color-accent)]"
        />
      )}
    </div>
  );
}
