/**
 * ChatTurnStatusLine — the line that carries a chat turn's STATE, shared by the
 * 1:1 thread and the group panel.
 *
 * ADR-100 revision: state is not conversation. A turn's dispatch / round-open
 * milestones are derived at render time (see `chatTurnState`) and never
 * persisted; the only durable state rows left are terminal outcomes
 * (aborted / failed / timeout), which explain why a message went unanswered.
 *
 * Either way the line stays OUT of a bubble, and it wears the transcript's
 * `turn` tier (see `ChatEventText`): centered and icon-less, because it speaks
 * for the whole turn rather than for one row. A bubble belongs to something a
 * participant said; "interrupted" is the platform talking, and padding it into
 * an agent-shaped bubble made a stop read like the agent's own words.
 *
 * This is the turn-tier call site of the shared event-text grammar — the
 * `ChatEventText` docblock is the rule, this file is one caller of it.
 */

import { ChatEventText } from "./ChatEventText";
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
    <ChatEventText
      anchor="turn"
      text={text}
      live={live}
      danger={danger}
      className={className}
      trailing={
        onJump && <ViewRunButton runName={runName} onJump={onJump} />
      }
    />
  );
}