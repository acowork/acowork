//! ListRow — the unified single list row.
//!
//! Standardizes the row chrome that previously drifted between panels:
//! full-width flex layout, uniform padding, the hover / selected fills,
//! keyboard semantics when clickable, and the disabled (50% opacity) state.
//!
//! Fills come from the `listRowHover` / `listRowSelected` family in
//! `lib/ui-styles.ts` — the same tokens every popup menu, the file tree,
//! the todo/queued rows, and the inline ask-question options use, so the
//! cursor passing over any row in the app reads the same way.
//!
//! The caller owns the middle column (`children`) — single-line label
//! rows, two-line title+description rows, and rich master-detail rows
//! (memory) all compose on top of the same base. `leading` and
//! `trailing` slots keep icons and switches aligned across every list.
import type { ReactNode } from "react";
import { cn } from "../../../lib/utils";
import {
  listRowHover,
  listRowHoverInset,
  listRowSelected,
  listRowSelectedInset,
} from "../../../lib/ui-styles";

export interface ListRowProps {
  /** Leading icon / dot / avatar. Auto-shrinks, left of the content. */
  leading?: ReactNode;
  /** Middle content column (min-w-0 flex-1). Free-form by design. */
  children: ReactNode;
  /** Right-side action slot (Switch, icon buttons…). */
  trailing?: ReactNode;
  /** Selection highlight (e.g. memory master-detail rows). */
  selected?: boolean;
  /** Renders a disabled row at 50% opacity (e.g. tool without API key). */
  disabled?: boolean;
  /** Horizontal padding: "default" = px-3 flush row; "nested" = deep
   *  left indent (pl-10) for rows that are children of an expanded
   *  parent (MCP server → tool rows). */
  padding?: "default" | "nested";
  /** Which surface the row sits on — governs hover/selected fills:
   *  "card" (raised list surface) or "inset" (expanded sub-surface). */
  surface?: "card" | "inset";
  /** When provided the row renders as a <button> with click semantics. */
  onClick?: () => void;
  /** Second click gesture on a clickable row (e.g. file trees where a single
   *  click selects and a double click opens — the workspace Explorer
   *  convention). Only wired when `onClick` is set, since the row must render
   *  as a <button> for the native `dblclick` event to reach it. */
  onDoubleClick?: () => void;
  /** Accessible label for clickable rows (aria-label). */
  ariaLabel?: string;
  /** Extra classes appended to the row. */
  className?: string;
  /** `data-testid` for the row root — lets a test scope to one row
   *  (e.g. assert on a single MCP preset's action slot) without having to
   *  walk the DOM looking for a button that may not be there. */
  testId?: string;
}

export function ListRow({
  leading,
  children,
  trailing,
  selected,
  disabled,
  padding = "default",
  surface = "card",
  onClick,
  onDoubleClick,
  ariaLabel,
  className,
  testId,
}: ListRowProps) {
  const base = cn(
    "flex w-full items-center gap-2 py-1.5 text-left transition-colors",
    padding === "nested" ? "pl-10 pr-3" : "px-3",
    onClick && !disabled && "cursor-pointer",
    disabled && "opacity-50",
    !disabled && surface === "inset"
      ? selected
        ? listRowSelectedInset
        : listRowHoverInset
      : !disabled && (selected
        ? listRowSelected
        : listRowHover),
    className,
  );

  const content = (
    <>
      {leading != null && <span className="shrink-0">{leading}</span>}
      <div className="min-w-0 flex-1">{children}</div>
      {trailing != null && <span className="shrink-0">{trailing}</span>}
    </>
  );

  if (onClick) {
    return (
      <button
        type="button"
        onClick={onClick}
        onDoubleClick={onDoubleClick}
        disabled={disabled}
        aria-label={ariaLabel}
        className={base}
      >
        {content}
      </button>
    );
  }
  return <div className={base} data-testid={testId}>{content}</div>;
}
