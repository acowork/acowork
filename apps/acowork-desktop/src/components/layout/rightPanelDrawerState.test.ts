/**
 * The hover-drawer state machine.
 *
 * These are the rules the interaction lives or dies by, and they are all
 * pure: what a tile hover does to an already-open drawer, which facts the
 * rail highlight and the fs-watch mirror are allowed to disagree about.
 * The clock that drives them is tested separately in
 * `useRightPanelDrawer.test.ts`.
 */

import { describe, it, expect } from "vitest";

import {
  CLOSED,
  DRAWER_RETRACT_MS,
  HOVER_CROSS_GAP_MS,
  HOVER_OPEN_DELAY_MS,
  drawerCloseNow,
  drawerClosed,
  drawerEnter,
  drawerKeyEnter,
  drawerLeave,
  drawerMounted,
  isPanelTabKey,
  shouldDelayOpen,
} from "./rightPanelDrawerState";

describe("open / mounted are separate facts", () => {
  it("keeps the content mounted while it slides out", () => {
    const open = drawerKeyEnter("workspace");
    const sliding = drawerLeave(open);

    expect(sliding).toEqual({ key: "workspace", open: false });
    // Still mounted — the exit animation needs something to animate.
    expect(drawerMounted(sliding)).toBe(true);
  });

  it("unmounts only when the retract timer fires", () => {
    expect(drawerMounted(drawerClosed())).toBe(false);
    expect(drawerClosed()).toBe(CLOSED);
  });

  it("slides back in from a mid-animation re-enter", () => {
    const sliding = drawerLeave(drawerKeyEnter("memory"));
    expect(drawerEnter(sliding)).toEqual({ key: "memory", open: true });
  });
});

describe("drawerKeyEnter", () => {
  it("opens on the hovered key", () => {
    expect(drawerKeyEnter("status")).toEqual({ key: "status", open: true });
  });

  it("replaces the panel on a sweep, with no intermediate closed frame", () => {
    // The rail is a vertical strip: sweeping down it must swap panels
    // directly. A state that went `open: false` between the two would make
    // the drawer blink once per tile.
    expect(drawerKeyEnter("memory")).toEqual({ key: "memory", open: true });
  });
});

describe("shouldDelayOpen", () => {
  it("delays a cold start — the cursor crosses the rail on its way elsewhere", () => {
    expect(shouldDelayOpen(CLOSED)).toBe(true);
  });

  it("does not delay a sweep across tiles of an open drawer", () => {
    expect(shouldDelayOpen(drawerKeyEnter("workspace"))).toBe(false);
  });

  it("does not delay a re-enter during the slide-out", () => {
    expect(shouldDelayOpen(drawerCloseNow(drawerKeyEnter("workspace")))).toBe(false);
  });
});

describe("isPanelTabKey", () => {
  it("passes the agent-panel tabs through", () => {
    for (const tab of ["workspace", "status", "memory", "debug", "setup"] as const) {
      expect(isPanelTabKey(tab)).toBe(tab);
    }
  });

  it("drops the rails that are not agent panels, so a pm peek cannot leak into the panel visibility rules", () => {
    expect(isPanelTabKey("group_members")).toBeNull();
    expect(isPanelTabKey("view_pm")).toBeNull();
    expect(isPanelTabKey("view_docs")).toBeNull();
    expect(isPanelTabKey(null)).toBeNull();
  });
});

describe("timing constants", () => {
  it("retract is long enough to read as motion, short enough not to feel sticky", () => {
    expect(DRAWER_RETRACT_MS).toBe(180);
  });

  it("the cross-gap grace outlives the open delay it can interrupt", () => {
    expect(HOVER_CROSS_GAP_MS).toBeGreaterThan(HOVER_OPEN_DELAY_MS);
  });
});
