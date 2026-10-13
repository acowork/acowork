/**
 * Pure placement for the workspace dropdown.
 *
 * Why this exists:
 *   The dropdown used to be `absolute left-0 w-80` inside the trigger's
 *   `.relative` wrapper, so it was anchored to the trigger's LEFT edge and
 *   simply overflowed to the right. The workspace panel is user-resizable,
 *   and once it is dragged narrower than the menu's 320px the menu got
 *   clipped — the whole right panel lives inside `CAPSULE_PANE_CN`
 *   (`overflow-hidden`), so that was a hard crop, not a hidden overflow.
 *
 *   The fix has two halves, and only one of them is geometry:
 *     1. Render through a portal with `position: fixed` (component-side) —
 *        nothing inside an `overflow-hidden` pane can escape it otherwise.
 *     2. Anchor to the trigger's RIGHT edge so the menu grows LEFTWARD.
 *        This module is half 2.
 *
 * Deliberately NOT a reuse of `computeMenuPosition` (the right-click menu
 * helper): that one anchors on a pointer, flips vertically around it and
 * clamps to the viewport. A dropdown is anchored on an element and its
 * vertical side is an explicit prop, so sharing that code would mean
 * fighting its semantics. The clamp below is the one behaviour they share,
 * and it is 3 lines.
 *
 * Units: CSS pixels in viewport coordinates — the same space as
 * `getBoundingClientRect()`.
 */

/** Measured rect of the trigger the menu hangs off. */
export interface AnchorRect {
  left: number;
  right: number;
  top: number;
  bottom: number;
}

/** Which side of the trigger the menu opens on. */
export type DropDirection = "up" | "down";

export interface DropdownPlacementInput {
  /** Trigger's viewport rect. */
  anchor: AnchorRect;
  /** Rendered menu size. */
  menuWidth: number;
  menuHeight: number;
  /** Visible viewport size (`window.innerWidth/Height`, or `visualViewport`). */
  viewportWidth: number;
  viewportHeight: number;
  /** Which side the menu opens on. */
  direction: DropDirection;
  /** Gap between trigger and menu, px. Default 4. */
  gap?: number;
}

export interface DropdownPlacement {
  /** `position: fixed` left. */
  left: number;
  /** `position: fixed` top. */
  top: number;
  /**
   * True when the menu was anchored on the trigger's right edge (its right
   * edge is flush with the trigger's right edge). The caller does not act
   * on it today — it exists so tests can assert *which* anchoring rule won,
   * rather than only that the number happens to be small.
   */
  rightAligned: boolean;
}

/** Minimum gap (px) between the menu and the viewport edge. */
export const DROPDOWN_EDGE_GAP = 4;

/**
 * Place a dropdown so it never needs the horizontal room the trigger's left
 * edge could offer — i.e. it hangs off the trigger's RIGHT edge and grows
 * leftward, out of whatever narrow pane contains the trigger.
 *
 * Horizontal:
 *   1. Default to right-aligned: `left = anchor.right - menuWidth`.
 *   2. If that would push the menu off the viewport's LEFT edge (trigger
 *      near the left of a wide window), fall back to left-aligned —
 *      `left = anchor.left` — because growing right is the only way to
 *      stay on screen there.
 *   3. Clamp both branches inside `[gap, viewportWidth - menuWidth - gap]`.
 *      Degenerate case (menu wider than the viewport) pins to `gap`, which
 *      is the CSS `max-w-[calc(100vw-2rem)]` case, not a crash.
 *
 * Vertical:
 *   `down` sits below the trigger, `up` above it. The `up` branch is
 *   clamped to `gap` because the chat toolbar's trigger sits near the top of
 *   the window and a tall menu would otherwise render off-screen above it.
 *
 * Never throws; degenerate inputs (zero menu size, zero viewport) collapse
 * to the edge gap.
 */
export function computeDropdownPlacement({
  anchor,
  menuWidth,
  menuHeight,
  viewportWidth,
  viewportHeight,
  direction,
  gap = DROPDOWN_EDGE_GAP,
}: DropdownPlacementInput): DropdownPlacement {
  const g = Math.max(0, gap);

  // ── Horizontal: right-aligned by default ──
  let left = anchor.right - menuWidth;
  let rightAligned = true;
  // Overflowing the LEFT edge means there is no room to grow leftward, so
  // grow rightward instead.
  if (left < g) {
    left = anchor.left;
    rightAligned = false;
  }
  // Belt-and-braces: keep the menu on screen even if it is wider than the
  // window (`w-80` in a very narrow window).
  const maxLeft = Math.max(g, viewportWidth - menuWidth - g);
  left = Math.min(Math.max(left, g), maxLeft);

  // ── Vertical ──
  const rawTop =
    direction === "down" ? anchor.bottom + g : anchor.top - menuHeight - g;
  const maxTop = Math.max(g, viewportHeight - menuHeight - g);
  const top = Math.min(Math.max(rawTop, g), maxTop);

  return { left, top, rightAligned };
}