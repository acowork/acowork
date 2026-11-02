import { useEffect, useRef, type RefObject } from "react";
import { cn } from "../../lib/utils";

/**
 * Shared chrome for the title-bar popover menus (Gateway, Appearance).
 *
 * Why this exists: the title bar hosts several glyph buttons, and each
 * one that opens a popover has to answer the same questions — where does
 * the panel anchor, what does its surface look like, how does it dismiss.
 * Two menus that answer them separately drift: the second one gets a
 * different width, a different shadow, a different dismiss rule, and the
 * title bar ends up with two visual dialects.
 *
 * So the three answers live here, once:
 *   - `titleBarMenuPanel`   — the panel surface (anchor, radius, border,
 *                             shadow, padding)
 *   - `titleBarMenuTrigger` — the glyph button's chrome (32px square,
 *                             hover / open states)
 *   - `useDismissMenu`      — outside-click + Escape dismissal
 *
 * The Gateway menu was the first popover here and set the look; the
 * Appearance menu copies it rather than inventing a second style.
 */

/** Popover surface: right-aligned under its trigger, one step above the
 *  title bar, on the page background so the panel reads as a sheet of the
 *  app rather than a floating widget. */
export const titleBarMenuPanel =
  "absolute right-0 z-30 mt-1 w-80 rounded-lg border border-border-outer bg-page-bg p-2 shadow-lg";

/** Glyph-button chrome for a title-bar trigger. `open` switches the
 *  button from "idle" to "pressed" so the user can see which popover is
 *  up. */
export function titleBarMenuTrigger(open: boolean): string {
  return cn(
    "flex h-8 w-8 items-center justify-center rounded transition-colors",
    open
      ? "bg-zinc-300 text-text-primary dark:bg-zinc-700"
      : "text-text-secondary hover:bg-zinc-300 dark:hover:bg-zinc-700",
  );
}

/**
 * Dismiss the open menu on an outside click or on Escape.
 *
 * The callback is read from a ref so an inline arrow at the call site
 * does not re-subscribe the listeners on every render (same reasoning as
 * `hooks/useEscapeClose.ts`).
 */
export function useDismissMenu(
  ref: RefObject<HTMLElement | null>,
  open: boolean,
  onDismiss: () => void,
): void {
  const latest = useRef(onDismiss);
  // Written during render so the handler always sees this commit's
  // callback — an effect would leave one frame where a click hits a
  // stale closure.
  latest.current = onDismiss;

  useEffect(() => {
    if (!open) return;
    const onDocDown = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) latest.current();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") latest.current();
    };
    document.addEventListener("mousedown", onDocDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDocDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, ref]);
}
