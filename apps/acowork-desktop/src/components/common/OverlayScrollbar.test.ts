import { describe, expect, it } from "vitest";
import { getOverlayThumbMetrics } from "./OverlayScrollbar";

describe("getOverlayThumbMetrics", () => {
  it("sizes and positions the thumb proportionally", () => {
    expect(getOverlayThumbMetrics(100, 500, 200)).toEqual({
      height: 20,
      top: 40,
      maxScroll: 400,
      maxTop: 80,
    });
  });

  it("uses the viewport height when content does not overflow", () => {
    expect(getOverlayThumbMetrics(100, 100, 0)).toEqual({
      height: 100,
      top: 0,
      maxScroll: 0,
      maxTop: 0,
    });
  });
});
