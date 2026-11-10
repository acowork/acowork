/**
 * Self-check for the chat-view overflow ladder (`panelClamp.ts`).
 *
 * Bug being fixed: with the file pane dragged to its maximum and the right
 * panel collapsed, the row's rigid panes summed to less than the viewport.
 * Clicking the right-nav to open the panel added `rightWidth` pixels that
 * nothing absorbed — ChatPanel is already pinned at its 288px minimum and the
 * other three panes are `shrink-0` — so the row overflowed and the leftmost
 * pane (the agent list) was pushed out of the window.
 *
 * Properties verified:
 *   1. No overflow → every width returned untouched (the clamp must not
 *      pre-emptively shrink a pane the user sized by hand).
 *   2. Overflow → the file pane pays first, down to its minimum.
 *   3. File pane already at its minimum → the right pane pays, and only as
 *      much as the deficit requires.
 *   4. Both at their minimum → nothing changes (honest clipping, the agent
 *      list is never the sacrifice).
 *   5. Idempotent — feeding a result back in is a no-op, which is what makes
 *      it safe to run from a React effect.
 *   6. ChatPanel maxed / file pane min is NOT an overflow case: `flex-1`
 *      already absorbed it, so the clamp stays out of the way.
 */
import { describe, expect, it } from "vitest";
import {
  CHROME_WIDTH,
  MIN_CHAT_WIDTH,
  MIN_FILE_WIDTH,
  MIN_RIGHT_WIDTH,
  clampFileWidth,
  filePaneCeiling,
  fitPanesToViewport,
  initialFileWidth,
} from "./panelClamp";

/** Rigid-panes total plus chrome must never exceed the viewport. */
const occupied = (sidebar: number, file: number, right: number) =>
  CHROME_WIDTH + MIN_CHAT_WIDTH + sidebar + file + right;

describe("fitPanesToViewport", () => {
  it("leaves a row that already fits completely alone", () => {
    const r = fitPanesToViewport({ windowWidth: 1920, sidebarWidth: 240, fileWidth: 450, rightWidth: 340 });
    expect(r).toEqual({ fileWidth: 450, rightWidth: 340 });
  });

  it("takes the reported overflow out of the file pane, not the agent list", () => {
    // The repro: file pane dragged to its 900px max, right panel then opened.
    const r = fitPanesToViewport({ windowWidth: 1920, sidebarWidth: 240, fileWidth: 900, rightWidth: 600 });
    expect(r.fileWidth).toBeLessThan(900);
    expect(r.rightWidth).toBe(600); // the panel the user just opened keeps its width
    expect(occupied(240, r.fileWidth, r.rightWidth)).toBeLessThanOrEqual(1920);
  });

  it("does not clamp a maxed file pane while the right panel is hidden", () => {
    // Same widths, right panel not rendered (caller passes 0).
    const r = fitPanesToViewport({ windowWidth: 1920, sidebarWidth: 240, fileWidth: 900, rightWidth: 0 });
    expect(r).toEqual({ fileWidth: 900, rightWidth: 0 });
  });

  it("falls through to the right pane once the file pane has bottomed out", () => {
    const r = fitPanesToViewport({ windowWidth: 1400, sidebarWidth: 400, fileWidth: MIN_FILE_WIDTH, rightWidth: 600 });
    expect(r.fileWidth).toBe(MIN_FILE_WIDTH);
    expect(r.rightWidth).toBeLessThan(600);
    expect(r.rightWidth).toBeGreaterThanOrEqual(MIN_RIGHT_WIDTH);
    expect(occupied(400, r.fileWidth, r.rightWidth)).toBeLessThanOrEqual(1400);
  });

  it("shrinks the right pane only as far as the deficit requires", () => {
    // Row needs 12px more than the window has: the right pane gives up exactly
    // 12px, not its whole 400px of slack.
    const windowWidth = occupied(240, MIN_FILE_WIDTH, 600) - 12;
    const r = fitPanesToViewport({ windowWidth, sidebarWidth: 240, fileWidth: MIN_FILE_WIDTH, rightWidth: 600 });
    expect(r.fileWidth).toBe(MIN_FILE_WIDTH);
    expect(r.rightWidth).toBe(600 - 12);
  });

  it("gives up instead of eating the agent list when both payers are spent", () => {
    // 1000px cannot hold 400 + 200 + 200 + 288 + 100. The row clips; the
    // agent list keeps its width (it has no vote in this ladder).
    const r = fitPanesToViewport({ windowWidth: 1000, sidebarWidth: 400, fileWidth: MIN_FILE_WIDTH, rightWidth: MIN_RIGHT_WIDTH });
    expect(r).toEqual({ fileWidth: MIN_FILE_WIDTH, rightWidth: MIN_RIGHT_WIDTH });
  });

  it("spends the file pane before the agent list even when the list is wider", () => {
    // Agent list at its 400px max is the widest pane here; the 300px file pane
    // pays first (down to its 200 minimum), and any remaining deficit comes
    // out of the right pane. The split tracks MIN_CHAT_WIDTH (288 → 338):
    // 1300 - chrome - MIN_CHAT_WIDTH - 400 - 300 - 300 = -138 deficit, 100
    // from the file pane, 38 from the right pane.
    const r = fitPanesToViewport({ windowWidth: 1300, sidebarWidth: 400, fileWidth: 300, rightWidth: 300 });
    expect(r.fileWidth).toBe(200);
    expect(r.rightWidth).toBe(262);
  });

  it("is idempotent", () => {
    const first = fitPanesToViewport({ windowWidth: 1500, sidebarWidth: 240, fileWidth: 900, rightWidth: 600 });
    const second = fitPanesToViewport({ windowWidth: 1500, sidebarWidth: 240, ...first });
    expect(second).toEqual(first);
  });

  it("survives a window narrower than the minimum row without going negative", () => {
    const r = fitPanesToViewport({ windowWidth: 600, sidebarWidth: 400, fileWidth: 900, rightWidth: 600 });
    expect(r.fileWidth).toBe(MIN_FILE_WIDTH);
    expect(r.rightWidth).toBe(MIN_RIGHT_WIDTH);
  });
});

/**
 * The file pane's upper bound (`filePaneCeiling`).
 *
 * Bug being fixed: the pane was capped at a hand-picked 900px on top of the
 * row's own math, so on a 2560px display a web preview inside the pane wrapped
 * into a phone-like column although 600px of the row were unclaimed. The cap
 * was duplicated (initial state, first-open auto-size, resize scaling, drag)
 * with four slightly different formulas.
 *
 * Properties verified:
 *   1. No fixed ceiling — a wide window gives a wide pane, well past 900.
 *   2. The ceiling is exactly the width `fitPanesToViewport` leaves untouched,
 *      i.e. the two share one contract instead of drifting apart.
 *   3. The right panel is only paid for when it is actually rendered.
 *   4. A window too narrow for the row still yields the floor, never a
 *      negative or above-floor bound.
 *   5. `clampFileWidth` is the only place a candidate becomes a width:
 *      rounding, floor, ceiling, and a corrupt stored value.
 */
describe("filePaneCeiling", () => {
  it("has no fixed pixel cap — a 2560px window yields a pane far past 900px", () => {
    const ceiling = filePaneCeiling({ windowWidth: 2560, sidebarWidth: 240, rightWidth: 340 });
    expect(ceiling).toBe(2560 - CHROME_WIDTH - 240 - 340 - MIN_CHAT_WIDTH);
    expect(ceiling).toBeGreaterThan(900 * 1.5);
  });

  it("is the width the overflow ladder leaves alone", () => {
    const input = { windowWidth: 1920, sidebarWidth: 240, rightWidth: 340 };
    const ceiling = filePaneCeiling(input);
    expect(fitPanesToViewport({ ...input, fileWidth: ceiling })).toEqual({
      fileWidth: ceiling,
      rightWidth: 340,
    });
    // One pixel wider is by definition an overflow, and the ladder takes it
    // out of the file pane again.
    expect(fitPanesToViewport({ ...input, fileWidth: ceiling + 1 }).fileWidth).toBe(ceiling);
  });

  it("does not pay for a right panel that is not rendered", () => {
    const hidden = filePaneCeiling({ windowWidth: 1920, sidebarWidth: 240, rightWidth: 0 });
    const shown = filePaneCeiling({ windowWidth: 1920, sidebarWidth: 240, rightWidth: MIN_RIGHT_WIDTH });
    expect(hidden - shown).toBe(MIN_RIGHT_WIDTH);
  });

  it("never returns a bound below the floor, however narrow the window", () => {
    expect(filePaneCeiling({ windowWidth: 600, sidebarWidth: 400, rightWidth: 340 })).toBe(MIN_FILE_WIDTH);
    expect(filePaneCeiling({ windowWidth: 0, sidebarWidth: 0, rightWidth: 0 })).toBe(MIN_FILE_WIDTH);
  });
});

describe("clampFileWidth", () => {
  it("rounds and respects the ceiling", () => {
    expect(clampFileWidth(1200.6, 1542)).toBe(1201);
    expect(clampFileWidth(4000, 1542)).toBe(1542);
  });

  it("never goes below the floor", () => {
    expect(clampFileWidth(10, 1542)).toBe(MIN_FILE_WIDTH);
    // A ceiling below the floor cannot invert the range.
    expect(clampFileWidth(500, 50)).toBe(MIN_FILE_WIDTH);
  });

  it("collapses a corrupt stored value to the floor", () => {
    expect(clampFileWidth(Number.NaN, 1542)).toBe(MIN_FILE_WIDTH);
    expect(clampFileWidth(Number.POSITIVE_INFINITY, 1542)).toBe(MIN_FILE_WIDTH);
  });
});

/**
 * `initialFileWidth` — what the pane should be when the surface is rebuilt.
 *
 * Bug being fixed: the first open after the last tab closed replaced the width
 * with half the budget unconditionally, so a width the user had dragged to was
 * silently discarded. Invisible while a 900px cap made every drag land at 900;
 * with the cap gone it costs hundreds of pixels.
 */
describe("initialFileWidth", () => {
  const ceiling = 1542;

  it("gives back the width the user chose", () => {
    expect(initialFileWidth({ storedWidth: 1200, ceiling, fallbackWidth: 450 })).toBe(1200);
  });

  it("falls back only when nothing was recorded", () => {
    expect(initialFileWidth({ storedWidth: null, ceiling, fallbackWidth: 450 })).toBe(450);
    expect(initialFileWidth({ storedWidth: Number.NaN, ceiling, fallbackWidth: 940 })).toBe(940);
  });

  it("bounds a width restored from a wider window by the current row", () => {
    // Dragged to 2200 on a 2560 display, reopened on a 1920 one.
    expect(initialFileWidth({ storedWidth: 2200, ceiling: 902, fallbackWidth: 450 })).toBe(902);
  });

  it("still never goes below the floor", () => {
    expect(initialFileWidth({ storedWidth: 40, ceiling, fallbackWidth: 450 })).toBe(MIN_FILE_WIDTH);
  });
});
