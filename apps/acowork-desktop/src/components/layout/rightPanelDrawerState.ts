// Right-side hover drawer — the transient, overlay-mode preview of a panel
// the layout keeps collapsed.
//
// Why this exists: when the user is working in a workspace file, the screen
// width belongs to the editor. Reading the workspace tree / memory / status
// should not cost a click-to-expand and a click-to-collapse around every
// glance. So hovering a collapsed rail tile slides the panel out OVER the
// chat/editor (it takes no layout width), and moving the pointer away slides
// it back. Clicking the tile still means "pin it open" — the drawer is never
// sticky, it is a peek.
//
// This file is the pure state machine: no React, no timers of its own, no
// DOM. `useRightPanelDrawer.ts` owns the clock and feeds events in, so the
// intent rules (open delay, cross-gap grace, close animation) are unit
// testable with a fake timer.
//
// Three observable facts, deliberately not one flag:
//   - `key`   — what the overlay is showing. It survives the close so the
//               slide-out animation has content, and it is what the rail
//               tile highlights.
//   - `open`  — the pointer is inside (tile or panel): the overlay is slid
//               in. The highlight and the store mirror key off THIS, not
//               `key` — a panel nobody can see must not keep the fs-watch
//               subscribed to its directories.
//   - mounted — `key !== null`: there is an overlay in the tree at all.

import type { PanelTab } from "../../stores/layoutStore";

/**
 * What a peek can show.
 *
 * The agent chat rail has one tile per panel tab, so those keys are the tabs
 * themselves. The other right rails (group members / tasks, pm, docs) each have
 * a tile meaning "this view's side panel"; they are not panel tabs and get
 * their own keys. `visiblePanelTab` in `rightPanelGate.ts` only reads the
 * tab-shaped keys, which is what keeps the drawer from leaking a pm peek into
 * the agent panel's visibility rules.
 */
export type DrawerKey =
  | PanelTab
  | "group_members"
  | "group_tasks"
  | "view_pm"
  | "view_docs";

export interface DrawerState {
  /** What the overlay is showing (or sliding out). Null = no overlay. */
  key: DrawerKey | null;
  /** Pointer is inside the drawer: it is slid in. */
  open: boolean;
}

/**
 * How long the pointer must rest on a tile before the drawer slides out.
 * The rail is a 40px strip the cursor crosses on its way to the chat pane,
 * so an immediate open would fire on every pass through the strip.
 */
export const HOVER_OPEN_DELAY_MS = 120;

/**
 * Grace period for the gap between a rail tile and the panel it opens: the
 * pointer leaves the tile before it reaches the panel, and without this the
 * drawer would blink closed mid-crossing.
 */
export const HOVER_CROSS_GAP_MS = 200;

/**
 * The slide-out duration, and the unmount delay that goes with it. Kept in
 * step with the `right-drawer-out` animation in `src/styles/globals.css` —
 * the CSS owns the motion, this constant only says how long the overlay
 * stays mounted so the motion has somewhere to play.
 */
export const DRAWER_RETRACT_MS = 180;

export const CLOSED: DrawerState = { key: null, open: false };

/** True while the overlay is in the tree (slid in, or sliding out). */
export function drawerMounted(s: DrawerState): boolean {
  return s.key !== null;
}

/**
 * The pointer entered a rail tile.
 *
 * A different tile replaces the current one immediately — that is the
 * horizontal sweep across the rail, and a drawer that re-delays on every
 * step feels broken. The open delay applies only to a cold start
 * (`shouldDelayOpen`).
 */
export function drawerKeyEnter(key: DrawerKey): DrawerState {
  return { key, open: true };
}

/**
 * A peek is worth delaying only when nothing is on screen yet. Once the
 * overlay is mounted the user is already engaged with it — sweeping to the
 * next tile must swap panels on the same frame, and stepping back onto the
 * tile of a panel that is mid-slide-out must bring it straight back rather
 * than making them wait out the dwell a second time.
 */
export function shouldDelayOpen(s: DrawerState): boolean {
  return s.key === null;
}

/**
 * The pointer left the overlay panel. Unlike leaving the rail — where the
 * pointer is usually still on its way INTO the panel — leaving the panel is
 * the gesture the user asked to be decisive: the drawer slides out at once
 * and only the unmount waits for the animation.
 */
export function drawerLeave(s: DrawerState): DrawerState {
  return { ...s, open: false };
}

/** The pointer entered the overlay panel itself: it stays. */
export function drawerEnter(s: DrawerState): DrawerState {
  return { ...s, open: true };
}

/**
 * Slide the overlay out. Keeps `key` so the animation has content; the
 * caller unmounts with `drawerClosed()` after `DRAWER_RETRACT_MS`.
 */
export function drawerCloseNow(s: DrawerState): DrawerState {
  return { ...s, open: false };
}

/** The overlay is gone: no content, no highlight. */
export function drawerClosed(): DrawerState {
  return CLOSED;
}

/** Narrow a peek key to the agent-panel tabs (the ones `hoverPanelTab` mirrors). */
export function isPanelTabKey(key: DrawerKey | null): PanelTab | null {
  switch (key) {
    case "workspace":
    case "status":
    case "memory":
    case "debug":
    case "setup":
    case "tasks":
      return key;
    default:
      return null;
  }
}
