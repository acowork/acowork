import { type ReactElement, useRef, useState, useCallback, useEffect } from "react";
import { createPortal, flushSync } from "react-dom";
import { cn } from "../../lib/utils";

/**
 * Unified tooltip component.
 *
 * - Default (no tipClass): rendered via React Portal to document.body
 *   so it is never clipped by parent overflow-hidden containers.
 * - With tipClass: rendered inline (CSS-positioned) to preserve
 *   @container query support for toolbar collapse behavior.
 *
 * Usage:
 *   <Tooltip content="Send message">
 *     <button>...</button>
 *   </Tooltip>
 */

type TooltipPosition = "top" | "bottom" | "left" | "right";
type TooltipVariant = "inverted" | "plain";

interface TooltipProps {
  /** Tooltip text content */
  content: string;
  /** The trigger element — must be a single ReactElement */
  children: ReactElement;
  /** Tooltip position relative to trigger. Default: 'top' */
  position?: TooltipPosition;
  /** Visual variant. Default: 'inverted' */
  variant?: TooltipVariant;
  /** Max width for long content. Default: '200px' */
  maxWidth?: string;
  /**
   * CSS class for container-query tooltip collapse (e.g. "tb-model-tip").
   * When provided, the tooltip is rendered inline (not via portal) so that
   * @container queries on the parent toolbar can hide it.
   */
  tipClass?: string;
  /** Delay before showing tooltip (ms). Default: 400 */
  delayMs?: number;
}

const variantClasses: Record<TooltipVariant, string> = {
  // Background intentionally inverts with theme (dark bg in light mode, light
  // bg in dark mode), so text must flip too — do NOT "simplify" away the
  // dark:text override (regression: bf9432b8 made dark tooltips white-on-white).
  inverted:
    "rounded-md shadow-lg bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-800",
  plain:
    "rounded-md shadow-lg bg-zinc-800 text-white dark:bg-zinc-200 dark:text-zinc-800",
};

const GAP = 6; // px gap between trigger and tooltip

// ── CSS-based positioning classes for inline (non-portal) mode ─────────
const inlinePositionClasses: Record<TooltipPosition, string> = {
  top: "bottom-full left-1/2 -translate-x-1/2 mb-1.5",
  bottom: "top-full left-1/2 -translate-x-1/2 mt-1.5",
  left: "right-full top-1/2 -translate-y-1/2 mr-1.5",
  right: "left-full top-1/2 -translate-y-1/2 ml-1.5",
};

// ── Transform strings for portal mode ──────────────────────────────────
const portalTransformMap: Record<TooltipPosition, string> = {
  top: "translate(-50%, -100%)",
  bottom: "translate(-50%, 0)",
  left: "translate(-100%, -50%)",
  right: "translate(0, -50%)",
};

// ── Inline tooltip (CSS-positioned, supports @container queries) ───────

function InlineTooltip({
  content,
  children,
  position,
  variant,
  maxWidth,
  tipClass,
  delayMs,
}: Required<Omit<TooltipProps, "tipClass">> & { tipClass: string }) {
  const [visible, setVisible] = useState(false);
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const handleEnter = useCallback(() => {
    // Defense 1: clear any pending timer from a previous enter that hasn't
    // fired yet. Without this, rapid hover-out-and-in races leave a stale
    // timer scheduled to fire and call setVisible(true) after we already
    // cleared state via handleLeave.
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    timerRef.current = setTimeout(() => {
      // Defense 2: clear ref after fire so handleLeave doesn't no-op
      // clearTimeout on a stale (already-fired) id.
      timerRef.current = null;
      setVisible(true);
    }, delayMs);
  }, [delayMs]);

  const handleLeave = useCallback(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    // Force synchronous commit. React 18 auto-batches state updates from
    // event handlers — if a parent re-render (e.g. a zustand store update)
    // queues a state change between our setVisible(false) and React's next
    // commit, the deferred hide can be overwritten by a quick re-enter's
    // setVisible(true) (or merged with another store update), leaving the
    // tooltip "stuck visible" until the user does another full hover cycle.
    // flushSync forces this update to commit before any other queued update.
    flushSync(() => {
      setVisible(false);
    });
  }, []);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, []);

  return (
    // `min-w-0 max-w-full`: this wrapper is a flex item of the trigger's
    // flex root, and a flex item's `min-width:auto` resolves to its content
    // width — so without this the trigger keeps its full intrinsic width and
    // overflows onto whatever sits next to it. The tooltip bubble itself is
    // `position:absolute`, so constraining this box never clips it.
    <div
      className="relative inline-flex min-w-0 max-w-full"
      onMouseEnter={handleEnter}
      onMouseLeave={handleLeave}
    >
      {children}
      {/*
        width: max-content lets the tooltip size to its natural content
        width (capped at maxWidth by the inner div), independent of the
        trigger's width. Without this, a position:absolute child with
        width:auto fills the containing block, so an icon-only trigger
        (~28px) would squeeze the tooltip to a 28px column and wrap
        the text one character per line.
      */}
      <div
        className={cn(
          inlinePositionClasses[position],
          "pointer-events-none absolute z-50 w-max",
          tipClass,
          visible ? "block" : "hidden",
        )}
      >
        <div
          className={cn(
            // Use whitespace-pre-wrap + break-words instead of nowrap so long
            // content (e.g. file paths) wraps within maxWidth instead of
            // overflowing the background. Short tooltips stay on one line
            // because they have natural break points (or none at all).
            "whitespace-pre-wrap break-words px-2.5 py-1.5 text-11 leading-tight",
            variantClasses[variant],
          )}
          style={{ maxWidth }}
        >
          {content}
        </div>
      </div>
    </div>
  );
}

// ── Portal tooltip (escapes overflow-hidden parents) ───────────────────

function PortalTooltip({
  content,
  children,
  position,
  variant,
  maxWidth,
  delayMs,
}: Required<Omit<TooltipProps, "tipClass">> & { tipClass?: undefined }) {
  const triggerRef = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [coords, setCoords] = useState<{ top: number; left: number }>({ top: 0, left: 0 });
  const timerRef = useRef<ReturnType<typeof setTimeout>>(null);

  const calcPosition = useCallback(() => {
    if (!triggerRef.current) return;
    const rect = triggerRef.current.getBoundingClientRect();

    let top = 0;
    let left = 0;

    switch (position) {
      case "top":
        top = rect.top - GAP;
        left = rect.left + rect.width / 2;
        break;
      case "bottom":
        top = rect.bottom + GAP;
        left = rect.left + rect.width / 2;
        break;
      case "left":
        top = rect.top + rect.height / 2;
        left = rect.left - GAP;
        break;
      case "right":
        top = rect.top + rect.height / 2;
        left = rect.right + GAP;
        break;
    }

    setCoords({ top, left });
  }, [position]);

  const handleEnter = useCallback(() => {
    // Defense 1: clear any pending timer from a previous enter that hasn't
    // fired yet. Without this, rapid hover-out-and-in races leave a stale
    // timer scheduled to fire and call setVisible(true) after we already
    // cleared state via handleLeave.
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    timerRef.current = setTimeout(() => {
      // Defense 2: clear ref after fire so handleLeave doesn't no-op
      // clearTimeout on a stale (already-fired) id.
      timerRef.current = null;
      calcPosition();
      setVisible(true);
    }, delayMs);
  }, [calcPosition, delayMs]);

  const handleLeave = useCallback(() => {
    if (timerRef.current !== null) {
      clearTimeout(timerRef.current);
      timerRef.current = null;
    }
    // Force synchronous commit. React 18 auto-batches state updates from
    // event handlers — if a parent re-render (e.g. a zustand store update)
    // queues a state change between our setVisible(false) and React's next
    // commit, the deferred hide can be overwritten by a quick re-enter's
    // setVisible(true) (or merged with another store update), leaving the
    // tooltip "stuck visible" until the user does another full hover cycle.
    // flushSync forces this update to commit before any other queued update.
    flushSync(() => {
      setVisible(false);
    });
  }, []);

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) {
        clearTimeout(timerRef.current);
        timerRef.current = null;
      }
    };
  }, []);

  return (
    // Same reasoning as InlineTooltip: as a flex item this box's
    // `min-width:auto` would keep the trigger at its content width and let
    // it overlap its neighbours. The bubble is portalled to body, so
    // constraining this box never clips it.
    <div
      ref={triggerRef}
      className="relative inline-flex min-w-0 max-w-full"
      onMouseEnter={handleEnter}
      onMouseLeave={handleLeave}
    >
      {children}
      {visible &&
        createPortal(
          <div
            // w-max: width follows the inner content's natural width (fit-content),
            // independent of the trigger's width. Without this, a position:fixed
            // child without an explicit width can collapse to ~0 in some
            // renderers for CJK / pre-wrap content, squeezing text into a
            // single-character-per-line column. See InlineTooltip above for
            // the analogous rationale with position:absolute.
            className="pointer-events-none fixed z-[9999] w-max"
            style={{
              top: coords.top,
              left: coords.left,
              transform: portalTransformMap[position],
            }}
          >
            <div
              className={cn(
                // See InlineTooltip above for why whitespace-pre-wrap +
                // break-words is used here instead of whitespace-nowrap.
                "whitespace-pre-wrap break-words px-2.5 py-1.5 text-11 leading-tight",
                variantClasses[variant],
              )}
              style={{ maxWidth }}
            >
              {content}
            </div>
          </div>,
          document.body,
        )}
    </div>
  );
}

// ── Public component ───────────────────────────────────────────────────

export function Tooltip({
  content,
  children,
  position = "top",
  variant = "inverted",
  maxWidth = "200px",
  tipClass,
  delayMs = 400,
}: TooltipProps) {
  // When content is empty, render children without tooltip wrapper
  if (!content) {
    return children;
  }

  const props = { content, children, position, variant, maxWidth, delayMs };

  // tipClass → inline mode (preserves @container query support)
  if (tipClass) {
    return <InlineTooltip {...props} tipClass={tipClass} />;
  }

  // Default → portal mode (escapes overflow-hidden clipping)
  return <PortalTooltip {...props} />;
}
