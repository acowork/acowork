/**
 * rightPanelGate — the right-panel tab availability rules (pure, unit-tested).
 *
 * Two independent facts decide whether a tab in the right rail exists:
 *
 *   - `agentRunning`      — every agent-scoped tab reads that agent's Runtime,
 *                           so a stopped agent removes them (pre-existing rule).
 *   - `memoryServiceReady` — ADR-101 moved memory out of the Runtime into a
 *                           standalone service. When the supervisor holds no
 *                           live process, `/api/agents/{id}/memory/*` answers
 *                           503, so the memory tile must not be drawn at all —
 *                           the same reason the agent-scoped tiles disappear
 *                           when the agent stops.
 *
 * `RightNavBar` (which tiles to draw) and `AppLayout` (which tab to fall back
 * to) both read these helpers, so the rail and the panel can never disagree
 * about whether the memory tab exists.
 *
 * The module also answers the other question the rail and the panel share —
 * `visiblePanelTab` / `workspacePanelVisible`: which tab is on screen *right
 * now*, once the hover drawer is taken into account. A collapsed layout is no
 * longer the same thing as an invisible panel, and `lib/workspaceFsWatch.ts`
 * needs the difference (see `layoutStore.hoverPanelTab`).
 */
import type { PanelTab } from "../../stores/layoutStore";

export interface RightPanelFlags {
  agentRunning: boolean;
  /** `GET /api/status` → `memory_service.ready`. Unknown counts as false. */
  memoryServiceReady: boolean;
}

/** Whether the memory tile may be drawn. Narrower on purpose than the
 * `agentRunning` rule the other agent-scoped tabs use: the agent-stop bounce
 * in `AppLayout` stays edge-triggered, and this rule only ever adds a reason
 * to leave the memory tab. */
export function memoryTabAvailable(flags: RightPanelFlags): boolean {
  return flags.agentRunning && flags.memoryServiceReady;
}

/**
 * The tab to switch to when `activeTab` has just become unavailable, or
 * `null` to leave it alone. `status` is the landing tab for every fall-back
 * (it is the only tile that is always available).
 */
export function memoryTabFallback(
  activeTab: PanelTab,
  flags: RightPanelFlags,
): PanelTab | null {
  if (activeTab !== "memory") return null;
  return memoryTabAvailable(flags) ? null : "status";
}

/** Everything `visiblePanelTab` needs. Structural, so both the Zustand state
 *  and a plain fixture satisfy it. */
export interface PanelVisibility {
  activePanelTab: PanelTab;
  rightPanelCollapsed: boolean;
  /** Tab previewed by the hover drawer, or `null`. Optional because the
   *  feature post-dates most of this module's callers and fixtures. */
  hoverPanelTab?: PanelTab | null;
}

/**
 * Which right-panel tab is actually on screen, or `null` when none is.
 *
 * `rightPanelCollapsed` used to be the whole answer. The hover drawer previews
 * a tab while the layout stays collapsed, so visibility is now a three-way
 * question, and the drawer wins whenever it is up: while it is, the pinned tab
 * is not rendered at all (`AppLayout` mounts `RightPanel` only when
 * `!rightPanelCollapsed`), so exactly one tab can ever be on screen.
 */
export function visiblePanelTab(layout: PanelVisibility): PanelTab | null {
  if (layout.hoverPanelTab) return layout.hoverPanelTab;
  return layout.rightPanelCollapsed ? null : layout.activePanelTab;
}

/**
 * Whether the workspace file tree is on screen.
 *
 * This is the question `lib/workspaceFsWatch.ts` answers when it decides which
 * directories the Runtime should keep watched: a tree the user can look at must
 * stay live, whether it is pinned into the layout or merely previewed in the
 * drawer. Kept here, next the tab rules, so the rail, the panel and the watch
 * set cannot drift apart on what "visible" means.
 */
export function workspacePanelVisible(layout: PanelVisibility): boolean {
  return visiblePanelTab(layout) === "workspace";
}

