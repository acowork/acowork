// @vitest-environment jsdom
/**
 * `useToolbarCollapse`'s MEASUREMENT — the part that has to be truthful for
 * the fold to happen at all.
 *
 * Two bugs came from measuring a squeezed row:
 *  - labels truncated to `Me…` / `ACowo…` while the row still had free space:
 *    the trigger chain is deliberately shrinkable (`min-w-0 truncate`, see
 *    ToolbarDropdown.shrink.test.tsx), so an over-full row reports the
 *    TRUNCATED width as the button's width — the fold never fires.
 *  - only the leftmost button folded, then the row was declared "fitting"
 *    while the rest stayed truncated, because a button nested one level
 *    deeper (the workspace trigger sits in its own wrapper) cannot be pinned
 *    from the outside.
 *
 * jsdom computes no layout, so every width is stubbed to the rule the browser
 * applies: a label in a row that is NOT pinned to max-content reports a
 * squeezed width, a pinned (or hidden) one reports the truth.
 */
import { renderHook } from "@testing-library/react";
import { act } from "react";
import { describe, expect, it } from "vitest";
import { useToolbarCollapse } from "./useToolbarCollapse";

const ICON = 28;
const CHEVRON = 12;
const GAP = 4;

type Box = {
  full: number;
  text: HTMLElement | null;
  chev: HTMLElement | null;
  el: HTMLElement;
};

function buildToolbar(rowWidth: number) {
  document.body.innerHTML = "";
  const node = document.createElement("div");
  // jsdom applies no stylesheet, so the hook's `getComputedStyle` read of the
  // row's own padding/gap needs the values the real row carries (`px-3 … gap-2`).
  node.style.paddingLeft = "12px";
  node.style.paddingRight = "12px";
  node.style.columnGap = "8px";
  const left = document.createElement("div");
  left.dataset.toolbarCluster = "left";
  node.appendChild(left);

  /** Width a box reports: honest only when the cluster is pinned. */
  const widthOf = (box: Box) => {
    const hidden = !box.text || box.text.style.display === "none";
    if (hidden) return ICON;
    return left.style.minWidth !== "" ? box.full : ICON + CHEVRON;
  };

  const make = (id: string, full: number, labelled: boolean): Box => {
    const el = document.createElement("div");
    el.dataset.toolbarBtn = id;
    let text: HTMLElement | null = null;
    let chev: HTMLElement | null = null;
    if (labelled) {
      text = document.createElement("span");
      text.dataset.toolbarText = "";
      chev = document.createElement("i");
      chev.dataset.toolbarChevron = "";
      el.append(text, chev);
    }
    const box = { full, text, chev, el };
    Object.defineProperty(el, "offsetWidth", { get: () => widthOf(box) });
    return box;
  };

  const boxes = [
    make("plus", ICON, false),
    make("model", 100, true),
    make("effort", 90, true),
  ];
  boxes.forEach((b) => left.appendChild(b.el));
  // The workspace trigger sits inside its own wrapper (`wsBtnRef`), exactly
  // like ChatPanel: whatever nests it, the cluster width must still add up.
  const wrap = document.createElement("div");
  const ws = make("ws", 80, true);
  wrap.appendChild(ws.el);
  left.appendChild(wrap);
  boxes.push(ws);

  Object.defineProperty(left, "offsetWidth", {
    get: () =>
      boxes.reduce((sum, b) => sum + widthOf(b), 0) + (boxes.length - 1) * GAP,
  });

  const right = document.createElement("div");
  right.dataset.toolbarCluster = "right";
  node.appendChild(right);
  Object.defineProperty(right, "offsetWidth", { get: () => 120 });
  Object.defineProperty(node, "offsetWidth", { get: () => rowWidth });

  return { node, left, boxes };
}

async function measure(node: HTMLElement) {
  const { result } = renderHook(() => useToolbarCollapse());
  act(() => result.current.setToolbarRef(node));
  await act(async () => {
    await new Promise<void>((r) => requestAnimationFrame(() => r()));
  });
}

const shown = (b: Box) => !!b.text && b.text.style.display !== "none";
const chevronShown = (b: Box) => !!b.chev && b.chev.style.display !== "none";

describe("useToolbarCollapse measurement", () => {
  it("shows every label when the row is wide enough", async () => {
    const { node, boxes } = buildToolbar(800);
    await measure(node);
    expect(boxes.filter((b) => b.text).every(shown)).toBe(true);
  });

  it("folds from the left, one button per measurement, chevron included", async () => {
    // 400 - 24 pad - 8 gap - 120 right - 4 slack = 244 available.
    // Row 310 → fold model (saves 72, the `+` saves nothing) → 238 fits.
    const { node, left, boxes } = buildToolbar(400);
    await measure(node);
    const [, model, effort, ws] = boxes;
    expect(shown(model)).toBe(false);
    expect(chevronShown(model)).toBe(false);
    expect(shown(effort)).toBe(true);
    expect(shown(ws)).toBe(true);
    // The pin used for measuring must not survive into the laid-out row.
    expect(left.style.minWidth).toBe("");
  });

  it("folds everything when even icon-only buttons do not fit", async () => {
    // 300 - 24 - 8 - 120 - 4 = 144 available; 310 → 238 → 176 → 124 fits.
    const { node, boxes } = buildToolbar(300);
    await measure(node);
    expect(boxes.every((b) => !shown(b) && !chevronShown(b))).toBe(true);
  });

  it("keeps folding until the labels really fit, not just the first one", async () => {
    // 240 - 24 - 8 - 120 - 4 = 84 available. Icon-only row is 28*4+12 = 124,
    // so every label folds and the row stops compressing there.
    const { node, boxes } = buildToolbar(240);
    await measure(node);
    expect(boxes.filter((b) => b.text).every((b) => !shown(b))).toBe(true);
  });
});