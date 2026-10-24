/**
 * rightPanelGate — the right-panel tab availability rules (pure, unit-tested).
 *
 * Two independent facts decide whether a tab in the right rail exists:
 *
 *   - `agentRunning`      — every agent-scoped tab reads that agent's Runtime,
 *                           so a stopped agent removes them (pre-existing rule).
 *   - `memoryServiceReady` — ADR-100 moved memory out of the Runtime into a
 *                           standalone service. When the supervisor holds no
 *                           live process, `/api/agents/{id}/memory/*` answers
 *                           503, so the memory tile must not be drawn at all —
 *                           the same reason the agent-scoped tiles disappear
 *                           when the agent stops.
 *
 * `RightNavBar` (which tiles to draw) and `AppLayout` (which tab to fall back
 * to) both read these helpers, so the rail and the panel can never disagree
 * about whether the memory tab exists.
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
