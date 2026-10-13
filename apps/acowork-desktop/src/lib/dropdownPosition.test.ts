/**
 * Regression coverage for the workspace dropdown's placement.
 *
 * The reported bug: drag the right workspace panel narrower than the
 * dropdown (320px), click the workspace button, and the menu was CUT OFF.
 * Two independent causes, and this file only pins the geometric one — the
 * other is `overflow-hidden` on `CAPSULE_PANE_CN`, which no pure function
 * can see:
 *
 *   1. The menu was `absolute left-0`, anchored to the trigger's LEFT
 *      edge, so it always grew rightward, off the right edge of the
 *      window. Fixed here.
 *   2. The panel clips its overflow, so the menu also had to leave the
 *      panel's box entirely (portal + `position: fixed`). Fixed in the
 *      component.
 *
 * Numbers below use the real values: menu `w-80` = 320px, and the panel
 * sits at the RIGHT edge of the window — which is what makes cause 1 bite,
 * since left-anchoring pushed a 320px menu off a 1600px window as soon as
 * the panel was under ~320px wide.
 */
import { describe, expect, it } from "vitest";
import { computeDropdownPlacement } from "./dropdownPosition";

const MENU_W = 320;
const MENU_H = 400;
const VIEWPORT_W = 1600;
const VIEWPORT_H = 900;
const GAP = 4;

/**
 * Trigger for a workspace panel of `panelWidth` docked to the window's
 * right edge. The panel's own left edge is at `VIEWPORT_W - panelWidth`,
 * and the trigger sits a few px inside it (px-1.5).
 */
function triggerInPanel(panelWidth: number) {
  const left = VIEWPORT_W - panelWidth + 6;
  return { left, right: left + 100, top: 100, bottom: 128 };
}

function place(anchor: ReturnType<typeof triggerInPanel>, menuWidth = MENU_W) {
  return computeDropdownPlacement({
    anchor,
    menuWidth,
    menuHeight: MENU_H,
    viewportWidth: VIEWPORT_W,
    viewportHeight: VIEWPORT_H,
    direction: "down",
  });
}

describe("computeDropdownPlacement", () => {
  it("hangs off the trigger's right edge and grows leftward", () => {
    // The core ask. A 120px panel cannot hold a 320px menu, so the menu
    // must take the extra 200px from the chat column to its LEFT.
    const p = place(triggerInPanel(120));
    expect(p.rightAligned).toBe(true);
    // Menu's right edge == trigger's right edge.
    expect(p.left + MENU_W).toBe(triggerInPanel(120).right);
    expect(p.left).toBeLessThan(triggerInPanel(120).left); // grew leftward
  });

  it("stays fully on screen when the panel is narrower than the menu", () => {
    for (const panelWidth of [120, 200, 260, 319, 320, 400, 900]) {
      const p = place(triggerInPanel(panelWidth));
      expect(p.left).toBeGreaterThanOrEqual(0);
      expect(p.left + MENU_W).toBeLessThanOrEqual(VIEWPORT_W);
    }
  });

  it("still reaches outside a 120px panel", () => {
    const p = place(triggerInPanel(120));
    const panelLeft = VIEWPORT_W - 120;
    // Menu starts left of the panel and keeps going — so it spans the
    // 120px panel AND ~200px of the chat column beside it.
    expect(p.left).toBeLessThan(panelLeft);
    expect(p.left + MENU_W).toBeGreaterThan(panelLeft);
  });

  it("falls back to left-aligned only when growing left would leave the window", () => {
    // Chat toolbar's workspace button near the window's LEFT edge in a
    // 400px window: right-aligning would put the menu at -216..104,
    // off-screen. Growing right is the only way to keep it visible.
    const p = computeDropdownPlacement({
      anchor: { left: 4, right: 104, top: 100, bottom: 128 },
      menuWidth: MENU_W,
      menuHeight: MENU_H,
      viewportWidth: 400,
      viewportHeight: VIEWPORT_H,
      direction: "down",
    });
    expect(p.rightAligned).toBe(false);
    expect(p.left).toBe(4);
  });

  it("clamps inside the window on the right edge", () => {
    // Trigger flush against the window's right edge.
    const p = place({ left: VIEWPORT_W - 100, right: VIEWPORT_W, top: 100, bottom: 128 });
    // Right-aligning would put the menu's right edge flush with the window
    // edge; the gap clamp pulls it back by exactly `GAP`.
    expect(p.left).toBe(VIEWPORT_W - MENU_W - GAP);
    expect(p.left + MENU_W).toBe(VIEWPORT_W - GAP);
  });

  it("pins to the gap when the menu is wider than the window", () => {
    // `max-w-[calc(100vw-2rem)]` covers the render; this is the numeric
    // fallback so placement never returns a negative left.
    const p = computeDropdownPlacement({
      anchor: { left: 10, right: 110, top: 100, bottom: 128 },
      menuWidth: 500,
      menuHeight: MENU_H,
      viewportWidth: 400,
      viewportHeight: VIEWPORT_H,
      direction: "down",
    });
    expect(p.left).toBe(GAP);
  });

  it("opens below the trigger when direction is down", () => {
    const a = triggerInPanel(300);
    const p = place(a);
    expect(p.top).toBe(a.bottom + GAP);
  });

  it("opens above the trigger when direction is up", () => {
    // The chat toolbar's workspace button, mid-window: there is room above.
    const a = { left: 400, right: 500, top: 520, bottom: 548 };
    const p = computeDropdownPlacement({
      anchor: a,
      menuWidth: MENU_W,
      menuHeight: MENU_H,
      viewportWidth: VIEWPORT_W,
      viewportHeight: VIEWPORT_H,
      direction: "up",
    });
    expect(p.top).toBe(a.top - MENU_H - GAP);
  });

  it("clamps the up placement at the top edge instead of rendering off-screen", () => {
    // Toolbar trigger near the window top + a tall list: `up` would put the
    // menu at y=-284. Clamp to the gap instead.
    const p = computeDropdownPlacement({
      anchor: { left: 400, right: 500, top: 120, bottom: 148 },
      menuWidth: MENU_W,
      menuHeight: MENU_H,
      viewportWidth: VIEWPORT_W,
      viewportHeight: VIEWPORT_H,
      direction: "up",
    });
    expect(p.top).toBe(GAP);
  });

  it("never returns a negative or non-finite coordinate on degenerate input", () => {
    const p = computeDropdownPlacement({
      anchor: { left: 0, right: 0, top: 0, bottom: 0 },
      menuWidth: 0,
      menuHeight: 0,
      viewportWidth: 0,
      viewportHeight: 0,
      direction: "down",
    });
    expect(Number.isFinite(p.left)).toBe(true);
    expect(Number.isFinite(p.top)).toBe(true);
    expect(p.left).toBeGreaterThanOrEqual(0);
    expect(p.top).toBeGreaterThanOrEqual(0);
  });
});
