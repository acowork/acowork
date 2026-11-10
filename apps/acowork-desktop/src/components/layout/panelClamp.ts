/**
 * panelClamp.ts — who pays when the chat-view row outgrows the viewport.
 *
 * The chat view is a flex row in which every pane is rigid (`shrink-0` plus a
 * fixed inline width) except ChatPanel (`flex-1`, `min-w-[288px]`). So
 * ChatPanel absorbs the first `available - 288` pixels of pressure on its own,
 * and once it is pinned at its minimum there is nothing left that can give:
 * opening the right panel, dragging a pane wider or shrinking the window past
 * that point makes the row overflow the window.
 *
 * The overflow is resolved here, in one place, by a fixed sacrifice order
 * rather than by "shrink whichever pane is widest":
 *
 *   1. file pane  — its content wraps and scrolls, so a lost pixel costs the
 *      least there; it is also the pane users drag to the max, i.e. the one
 *      most likely to *be* the surplus.
 *   2. right pane — only when the file pane has bottomed out, and only down to
 *      what is needed to fit. Never more: the user just clicked to open it, so
 *      shrinking it beyond the deficit contradicts the action that caused it.
 *   3. nothing    — both at their minimum means the window is simply too
 *      narrow for this combination. The row clips instead of every pane
 *      becoming unreadable; that is an honest failure, not a bug.
 *
 * AgentList is deliberately not in the ladder. It is the app's only agent
 * navigation, and width is a bad proxy for "who can pay": a 400px list is the
 * widest pane on a small window, yet giving up 140px of it destroys
 * navigability while 140px from the file pane only reflows text. Paying the
 * bill with the leftmost pane is exactly the bug this module exists to stop.
 *
 * `fitPanesToViewport` is pure and idempotent — call it with already-fitted
 * widths and it returns them untouched — so AppLayout can run it on every
 * layout-relevant state change without risking an update loop.
 */

/** Narrowest usable file pane. */
export const MIN_FILE_WIDTH = 200;
/** Narrowest usable right panel. */
export const MIN_RIGHT_WIDTH = 200;
/** ChatPanel's `min-w-[...]` — below this the collapsed toolbar breaks.
 *
 * 338 = toolbar's own `min-w-[264px]` (everything in the left cluster
 * folded to icon-only) + 24 of `px-3` inside the toolbar + 50 of
 * `mx-6` (24px each side) + 1px border on each side of the input
 * container. Below this the toolbar overflows the input container
 * and the right cluster gets clipped by the panel's `overflow-hidden`.
 * Kept in lock-step with `CHAT_PANEL_SHELL_CN` in ChatPanel.tsx —
 * the right cluster's own width is now MEASURED at runtime (see
 * `data-toolbar-cluster="right"`), so future right-cluster buttons
 * no longer require editing either of these numbers; only an outer
 * chrome change does. */
export const MIN_CHAT_WIDTH = 338;

/**
 * Horizontal cost of everything in the row that is neither a pane nor sized by
 * a pane: NavBar 48 + right rail 40 + two 4px resize handles + the right
 * panel's 4px `ml-1` gutter = 100.
 *
 * The gutter is counted even when the right panel is hidden, which makes the
 * fit 4px conservative — it can clamp 4px earlier than strictly necessary.
 * Not worth branching a second constant over.
 */
export const CHROME_WIDTH = 100;

/**
 * ── The file pane's upper bound ────────────────────────────────────────────
 *
 * There is deliberately **no fixed pixel ceiling**. The file pane holds
 * previews of things that were sized for a browser window — an `https://` page
 * or a workspace `.html` in the address-bar browser (ADR-112) — and a hard cap
 * makes those render narrower than the window can afford: content reflows into
 * a phone-like column while 1000px of desktop sits unused. A 900px cap was
 * exactly that bug.
 *
 * What must stay readable is only ChatPanel, so the bound is what remains once
 * every other fixed part of the row has been paid for: `MIN_CHAT_WIDTH` is the
 * entire budget of pressure the file pane may apply. Drag the pane wider than
 * that and the ceiling stops it — not a number picked by hand.
 *
 * Keep every write path on this one function (initial state, first-open
 * auto-size, window-resize scaling, drag, and the overflow ladder above): the
 * cap used to be duplicated in four places with four slightly different
 * formulas, which is how it drifted.
 */
export interface FileCeilingInput {
  /** Live viewport width in CSS pixels. */
  windowWidth: number;
  sidebarWidth: number;
  /**
   * Width the right panel actually occupies. Pass 0 when it is not rendered —
   * the caller owns that condition, same as for `fitPanesToViewport`.
   */
  rightWidth: number;
}

/** Widest the file pane may get: everything but ChatPanel's floor is spoken for. */
export function filePaneCeiling({ windowWidth, sidebarWidth, rightWidth }: FileCeilingInput): number {
  return Math.max(
    windowWidth - CHROME_WIDTH - sidebarWidth - rightWidth - MIN_CHAT_WIDTH,
    MIN_FILE_WIDTH,
  );
}

/**
 * Clamp a candidate file-pane width into `[MIN_FILE_WIDTH, ceiling]`.
 *
 * A non-finite candidate (a corrupt or absent `localStorage` value) collapses
 * to the floor — callers that have a default should test for it first and use
 * the default, because "200px" is a valid width but rarely the intended one.
 */
export function clampFileWidth(candidate: number, ceiling: number): number {
  if (!Number.isFinite(candidate)) return MIN_FILE_WIDTH;
  return Math.min(Math.max(Math.round(candidate), MIN_FILE_WIDTH), Math.max(ceiling, MIN_FILE_WIDTH));
}

export interface InitialFileWidthInput {
  /** Width the user last dragged to, or null when nothing was recorded. */
  storedWidth: number | null;
  /** The row's budget — `filePaneCeiling` for the current window. */
  ceiling: number;
  /** What to use when there is no recorded width. */
  fallbackWidth: number;
}

/**
 * How wide to make the file pane when the surface is (re)built: on mount, and
 * again on the first file opened after the last tab closed.
 *
 * Order of preference, and why it is not "whatever was stored":
 *   - **A width the user dragged to wins.** It is the only recorded intent,
 *     and with the fixed cap gone that intent spans hundreds of pixels, so
 *     silently substituting the default is a visible loss.
 *   - **Otherwise the caller's fallback** — 450 on mount, and half of the row
 *     the pane shares with ChatPanel on a first open.
 *
 * Either way the row's ceiling applies last, so restoring a width from a wider
 * window can never reach the overflow ladder's doorstep.
 */
export function initialFileWidth({ storedWidth, ceiling, fallbackWidth }: InitialFileWidthInput): number {
  const wanted = storedWidth != null && Number.isFinite(storedWidth) ? storedWidth : fallbackWidth;
  return clampFileWidth(wanted, ceiling);
}

export interface PaneFitInput {
  /** Live viewport width in CSS pixels. */
  windowWidth: number;
  sidebarWidth: number;
  fileWidth: number;
  /**
   * Width the right panel actually occupies. Pass 0 when it is not rendered
   * (collapsed, or no agent selected) — the caller owns that condition because
   * it also owns the JSX that renders the panel.
   */
  rightWidth: number;
}

export interface PaneFitResult {
  fileWidth: number;
  rightWidth: number;
}

/**
 * Shrink the file pane, then the right pane, until the row fits the viewport.
 * Returns the inputs unchanged when there is no overflow.
 */
export function fitPanesToViewport({
  windowWidth,
  sidebarWidth,
  fileWidth,
  rightWidth,
}: PaneFitInput): PaneFitResult {
  const surplus =
    windowWidth - CHROME_WIDTH - MIN_CHAT_WIDTH - sidebarWidth - fileWidth - rightWidth;
  if (surplus >= 0) return { fileWidth, rightWidth };

  const deficit = -surplus;
  const fromFile = Math.min(deficit, Math.max(fileWidth - MIN_FILE_WIDTH, 0));
  const fromRight = Math.min(deficit - fromFile, Math.max(rightWidth - MIN_RIGHT_WIDTH, 0));
  return { fileWidth: fileWidth - fromFile, rightWidth: rightWidth - fromRight };
}
