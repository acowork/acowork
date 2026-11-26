/**
 * Row-level anchoring inside a multi-row virtual item.
 *
 * `VirtualRowList`'s placement unit is an ITEM, but the chat transcript folds a
 * whole execution session's rounds into ONE item anchored at its oldest row in
 * the loaded window (ADR-106 D3/D4). So `scrollToKey(key, "center")` centres the
 * BLOCK — and on a session that has been running for a while the block is many
 * screens tall, so the pane lands on whichever round happens to sit at the
 * block's middle, not on the row the reader clicked. Near the tail the two are
 * a few pixels apart and nobody notices; deep in a paged-back history the block
 * holds dozens of rounds and the jump lands on a different message entirely.
 *
 * The row is in the DOM either way — every round wears its own `data-seq`
 * (`ChatExecutionBlock`), and a standalone row IS its item — so the anchor is
 * corrected from the row's position INSIDE its item. The correction is a pure
 * function of the two boxes so it can be tested without a browser, and
 * `VirtualRowList` re-applies it on every commit of its bounded scroll pin, the
 * same way it re-applies a plain key scroll while estimates settle.
 */

/** The two fields of a `DOMRect` the correction reads — narrowed so a caller
 *  (and a test) can hand in plain numbers. */
export interface Box {
  top: number;
  height: number;
}

/**
 * The pane's `scrollTop` that lands `row` in the middle of the viewport.
 *
 * Precondition: the item holding the row was just aligned to the pane's top
 * (`scrollToIndex(idx, { align: "start" })`), so `alignedTop` is the pane's
 * offset with the item's top edge at the viewport's top edge.
 *
 * `row.top - item.top` is the row's distance below its item's top — the one
 * quantity that survives scrolling, which is exactly why rects are used and not
 * `offsetTop` (whose reference is the nearest positioned ancestor, not the item).
 */
export function rowCentredScrollTop(args: {
  /** The pane's `scrollTop` with the item's top edge at the viewport's top. */
  alignedTop: number;
  /** The pane's visible height. */
  clientHeight: number;
  /** The item's box, viewport-relative. */
  item: Box;
  /** The row's box, viewport-relative. */
  row: Box;
}): number {
  const inItem = args.row.top - args.item.top;
  // A row taller than the pane has nothing to centre: its top edge is the
  // most useful thing to show, and a negative lead would cut the answer's
  // opening line off above the viewport.
  const lead = Math.max(0, (args.clientHeight - args.row.height) / 2);
  return Math.max(0, args.alignedTop + inItem - lead);
}

/**
 * The selector a locate uses to name its row inside its item. One spelling in
 * one place: the surfaces pass it to the list handle, and the row markup
 * (`useTranscriptLayout.virtualAttrs`, `ChatExecutionBlock`) is what it points
 * at.
 */
export function rowSelector(seq: number): string {
  return `[data-seq="${seq}"]`;
}
