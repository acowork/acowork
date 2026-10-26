/**
 * Tests for the bubble meta-anchor rule.
 *
 * The reported defect this pins: the small time text under a bubble appeared
 * "sometimes in the middle, sometimes on the left". Both halves of that came
 * from the same place — the human's meta block was anchored to the ROW's start
 * edge instead of the bubble's own edge, so a shrink-wrapped bubble decided the
 * timestamp's x-position:
 *
 *   - short message ("批准")  → the column shrink-wraps to the bubble, and the
 *                               timestamp sits at its left edge = mid-row;
 *   - long message           → the column is at the content clamp, so the
 *                               timestamp sits at the far left.
 *
 * And the 1:1 chat / execution surface did that while the group panel already
 * hugged the outer edge, so the same message showed its time on the right in a
 * group and on the left in a 1:1.
 *
 * The DOM-shaped half of the guard lives in `bubbleMetaAnchor.dom.test.tsx`
 * (jsdom has no layout, so that file asserts which column carries which edge
 * rather than painted geometry).
 */
import { describe, expect, it } from "vitest";
import { bubbleMetaAnchor } from "./bubbleMetaAnchor";

describe("bubbleMetaAnchor", () => {
  it("anchors the human's own meta block to the end of the row", () => {
    expect(bubbleMetaAnchor("own")).toBe("items-end");
  });

  it("anchors a peer's meta block to the start of the row", () => {
    expect(bubbleMetaAnchor("peer")).toBe("items-start");
  });

  it("never returns the row-start edge for the own side", () => {
    // The regression in one assertion: `items-start` on the own column is
    // exactly what stranded the timestamp under a right-aligned bubble's left
    // edge.
    expect(bubbleMetaAnchor("own")).not.toBe("items-start");
  });
});