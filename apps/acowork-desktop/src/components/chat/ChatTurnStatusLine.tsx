/**
 * ChatTurnStatusLine — the bare state line of a chat transcript, shared by the
 * 1:1 thread and the group panel.
 *
 * Since ADR-106 the transcript says almost nothing about state: how a round
 * ended is a fact about the RUN and it surfaces inside the execution block
 * (D3/D9). What is left here is the platform's own voice about this Chat — the
 * one line the surface is entitled to add about itself ("my send has not come
 * back yet") and the placeholder a block shows while it has no content.
 *
 * Either way the line stays OUT of a bubble, and it wears the transcript's
 * `turn` tier (see `ChatEventText`): centered and icon-less, because it speaks
 * for the whole turn rather than for one row. A bubble belongs to something a
 * participant said; "working on it" is the platform talking, and padding it into
 * an agent-shaped bubble made a stop read like the agent's own words.
 *
 * This is the turn-tier call site of the shared event-text grammar — the
 * `ChatEventText` docblock is the rule, this file is one caller of it.
 */

import { ChatEventText, type EventTone } from "./ChatEventText";

interface ChatTurnStatusLineProps {
  /** Already-localized status text. */
  text: string;
  /** A turn still in flight shimmers — the same `thinking-shimmer` treatment
   *  as the execution view's "running a tool" line, so "the agent is busy"
   *  reads the same everywhere. A settled outcome is quiet. */
  live?: boolean;
  /** `failed` / `timeout` are `danger`. `aborted` must NOT be: a stop is a user
   *  action, not a fault of the run. */
  tone?: EventTone;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
}

export function ChatTurnStatusLine({
  text,
  live,
  tone,
  className,
}: ChatTurnStatusLineProps) {
  return (
    <ChatEventText text={text} live={live} tone={tone} className={className} />
  );
}