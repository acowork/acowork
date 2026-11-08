import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

/**
 * InteractionCardShell — the ONE card chrome every interaction card wears,
 * on the execution surface and inside a Chat execution block alike.
 *
 * Why this exists: the recipe (hairline shell + neutral header bar + padded
 * body) was written once in `AskQuestionCard`, then re-invented without any
 * shell in `BlockCardActions` — bare title row, full-width accent-washed
 * option buttons, an unstyled `<input>` — and hand-copied with drifted
 * paddings into `ChatNoticeEventRow`. A question must not change costume
 * between the surface it was asked on and the block that shows the run
 * (ADR-106 D8; the comment in `ChatNoticeEventRow` has claimed this rule
 * since day one — this component is what makes it structurally true).
 *
 * The chrome rule `chatCardChrome.test.tsx` pins: the highlight color paints
 * a BUTTON BACKGROUND and nothing else. The shell border and the header
 * title stay neutral (`border-border-outer`, `bg-chat-title` + `text-text`).
 */
interface InteractionCardShellProps {
  icon: LucideIcon;
  /** The card's subject line — a short, translated string. */
  title: string;
  /** Right-aligned header slot: countdown, busy copy. Never an action. */
  headerRight?: ReactNode;
  children: ReactNode;
  /** The surface's width wrapper (the execution card clamps to the content
   *  column; a card inside a block fills the block). */
  className?: string;
  /** Body surface override. Default is the agent-bubble fill with padding;
   *  a card whose body is a full-width ruled list passes its own surface
   *  instead. Replaces, not merges — the two families must not fight. */
  bodyClassName?: string;
}

export function InteractionCardShell({
  icon: Icon,
  title,
  headerRight,
  children,
  className,
  bodyClassName,
}: InteractionCardShellProps) {
  return (
    <div className={className}>
      <div className="overflow-hidden rounded-md border border-border-outer">
        <div className="flex items-center gap-[7px] border-b border-border-divider bg-chat-title px-3 py-[7px] text-xs font-medium text-text">
          <Icon className="h-3.5 w-3.5 shrink-0" />
          <span className="flex-1">{title}</span>
          {headerRight}
        </div>
        <div className={bodyClassName ?? "bg-chat-bubble px-3 py-2.5"}>{children}</div>
      </div>
    </div>
  );
}
