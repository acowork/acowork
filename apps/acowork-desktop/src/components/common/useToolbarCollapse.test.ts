import { describe, expect, it } from "vitest";
import { foldLeftToRight } from "./useToolbarCollapse";

/**
 * The greedy fold math of the shared composer-toolbar collapse engine
 * (`useToolbarCollapse`): given each button's full width and icon-only
 * width, decide which labels fold so the row fits `available`.
 * Folding is progressive from the LEFT (model hides first, then the
 * buttons to its right) — the rightmost, most context-relevant labels
 * survive longest.
 */
describe("foldLeftToRight", () => {
  it("folds nothing when the row fits", () => {
    const full = [120, 90, 80];
    const icon = [28, 28, 28];
    // 120+90+80 + gaps 8 = 298 <= 300
    expect(foldLeftToRight(full, icon, 300, 8)).toEqual([false, false, false]);
  });

  it("folds exactly the leftmost button when that alone fits", () => {
    const full = [120, 90, 80];
    const icon = [28, 28, 28];
    // total 298; available 210 → fold model (saves 92) → 206 <= 210
    expect(foldLeftToRight(full, icon, 210, 8)).toEqual([true, false, false]);
  });

  it("folds progressively left-to-right until the row fits", () => {
    const full = [120, 90, 80];
    const icon = [28, 28, 28];
    // total 298; available 120 → fold model (206) → fold effort (144) →
    // fold ws (112 <= 120) → all three folded
    expect(foldLeftToRight(full, icon, 120, 8)).toEqual([true, true, true]);
  });

  it("never folds more buttons than exist", () => {
    const full = [120, 90];
    const icon = [28, 28];
    // even fully icon-only (84) exceeds available 10 → both folded, stop
    expect(foldLeftToRight(full, icon, 10, 4)).toEqual([true, true]);
  });

  it("handles a single button and an empty row", () => {
    expect(foldLeftToRight([100], [28], 50, 0)).toEqual([true]);
    expect(foldLeftToRight([100], [28], 100, 0)).toEqual([false]);
    expect(foldLeftToRight([], [], 50, -4)).toEqual([]);
  });
});
