/**
 * Regression tests for the jump-to-bottom anchor's disarm rule.
 *
 * Live repro (desktop log 2026-09-25 22:47:32/36): the anchor disarmed while
 * the content was still settling, because a layout SHRINK clamps scrollTop
 * and looked like a user scroll-up:
 *   pin  {scrollTopBefore: 2477, max: 4150, lastPinTop: 2477}   → content grew
 *   pin DISARM release {scrollTop: 3697, max: 3697, lastPinTop: 4150}
 *   jump settled+300   {scrollTop: 3234, distFromBottom: 627}   → stuck mid-way
 */
import { describe, expect, it } from "vitest";
import { BOTTOM_PIN_RELEASE_PX, decideBottomPin } from "./bottomAnchor";

describe("decideBottomPin", () => {
  it("keeps pinning while the content grows (anchor rides the bottom)", () => {
    // Previous pin left scrollTop at the OLD max; the content then grew.
    expect(decideBottomPin({ scrollTop: 2477, max: 4150, lastPinTop: 2477 })).toBe("pin");
  });

  it("keeps pinning when the content SHRANK and clamped scrollTop with it", () => {
    // A shrink is a layout event (measurement correction / window
    // replacement / "Loading more..." row disappearing) — not user intent.
    expect(decideBottomPin({ scrollTop: 3697, max: 3697, lastPinTop: 4150 })).toBe("pin");
    expect(decideBottomPin({ scrollTop: 3478, max: 3478, lastPinTop: 3682 })).toBe("pin");
  });

  it("disarms when the user scrolls up at an unchanged height", () => {
    const max = 4150;
    expect(decideBottomPin({ scrollTop: max - 500, max, lastPinTop: max })).toBe("disarm");
    expect(decideBottomPin({ scrollTop: 0, max, lastPinTop: max })).toBe("disarm");
  });

  it("ignores sub-threshold jitter (< BOTTOM_PIN_RELEASE_PX)", () => {
    const max = 4150;
    expect(decideBottomPin({ scrollTop: max - BOTTOM_PIN_RELEASE_PX, max, lastPinTop: max })).toBe("pin");
    expect(decideBottomPin({ scrollTop: max - BOTTOM_PIN_RELEASE_PX - 1, max, lastPinTop: max })).toBe("disarm");
  });

  it("disarms when the user scrolls up while the content also grew", () => {
    expect(decideBottomPin({ scrollTop: 3000, max: 4150, lastPinTop: 3700 })).toBe("disarm");
  });
});
