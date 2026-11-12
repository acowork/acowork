/**
 * ADR-113 D6: node topology is replayable protocol state, NOT a mount
 * snapshot or a launch event.
 *
 * `AgentList` owns the baseline fetch on mount plus the
 * `bootstrapVersion`-driven refetch. That is not enough for a cold start:
 * the Gateway can spawn its node *after* the Desktop has already read an
 * empty `GET /api/nodes`, and a node appearing does not necessarily bump
 * the bootstrap snapshot version. The UI then stays on "No available node"
 * until some unrelated action happens to refresh the store — previously
 * the "Start local node" button was the only such action, which made it
 * look as though a second node was required before installing.
 *
 * Any surface that *decides* whether an install is possible must therefore
 * read the protocol state at the moment it needs it and keep reading while
 * that decision is on screen. This hook is the single implementation of
 * that contract, shared by the Agent library dialog and first-run
 * onboarding. It never inspects Gateway mode and never waits for a
 * launcher-specific event.
 */
import { useEffect } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { log } from "../../lib/logger";

export const NODE_TOPOLOGY_REFRESH_MS = 2000;

export function useNodeTopologyRefresh(
  active: boolean,
  intervalMs: number = NODE_TOPOLOGY_REFRESH_MS,
): void {
  useEffect(() => {
    if (!active) return;
    const refresh = () => {
      void useAgentStore.getState().fetchNodes().catch((e) => {
        // A failed poll is not an empty topology: keep the last known
        // state and retry on the next tick rather than blanking the UI.
        log.warn("[nodeTopology] GET /api/nodes refresh failed:", e);
      });
    };
    // Read immediately on activation — never render a decision from the
    // store's possibly-stale mount snapshot.
    refresh();
    const timer = window.setInterval(refresh, intervalMs);
    return () => window.clearInterval(timer);
  }, [active, intervalMs]);
}
