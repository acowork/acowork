/**
 * `rowCentredScrollTop` — the arithmetic that makes a locate land on the ROW
 * the reader clicked instead of on the block that holds it.
 */
import { describe, expect, it } from "vitest";
import { rowCentredScrollTop, rowSelector } from "./scrollAnchor";

const pane = { alignedTop: 5000, clientHeight: 800 };

describe("rowCentredScrollTop", () => {
  it("centres a row that sits deep inside a tall item", () => {
    // The block's top is at the viewport's top (that is `alignedTop`); the row
    // is 2400px down inside it, 120px tall, in an 800px pane.
    // 5000 + 2400 - (800-120)/2 = 7060
    expect(
      rowCentredScrollTop({
        ...pane,
        item: { top: 0, height: 3000 },
        row: { top: 2400, height: 120 },
      }),
    ).toBe(7060);
  });

  it("collapses to the plain item centring when the row IS the item", () => {
    // A standalone row: its box is the item's box, so the result is the item
    // centred — the behaviour `scrollToKey(key, "center")` had by estimate.
    // 5000 + 0 - (800-120)/2 = 4660
    expect(
      rowCentredScrollTop({
        ...pane,
        item: { top: 0, height: 120 },
        row: { top: 0, height: 120 },
      }),
    ).toBe(4660);
  });

  it("keeps a row taller than the pane at its top edge", () => {
    // lead would be negative: showing the row's opening line beats centring it.
    expect(
      rowCentredScrollTop({
        ...pane,
        item: { top: 0, height: 4000 },
        row: { top: 1000, height: 1200 },
      }),
    ).toBe(6000);
  });

  it("never returns a negative offset (the item's first rows)", () => {
    expect(
      rowCentredScrollTop({
        alignedTop: 100,
        clientHeight: 800,
        item: { top: 0, height: 3000 },
        row: { top: 0, height: 120 },
      }),
    ).toBe(0);
  });

  it("is scroll-independent: only the row's offset inside the item counts", () => {
    // Between the two calls the pane scrolled 10 000px, so both boxes moved up
    // by exactly that much and `alignedTop` grew by it: the anchor is the same
    // absolute offset either way.
    const atItemTop = rowCentredScrollTop({
      alignedTop: 20000,
      clientHeight: 800,
      item: { top: 0, height: 3000 },
      row: { top: 2400, height: 120 },
    });
    const itemAboveViewport = rowCentredScrollTop({
      alignedTop: 20000,
      clientHeight: 800,
      item: { top: -10000, height: 3000 },
      row: { top: -7600, height: 120 },
    });
    expect(atItemTop).toBe(22060);
    expect(itemAboveViewport).toBe(22060);
  });
});

describe("rowSelector", () => {
  it("names the row the transcript paints for a seq", () => {
    expect(rowSelector(42)).toBe('[data-seq="42"]');
  });
});
