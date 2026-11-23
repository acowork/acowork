/**
 * ADR-113: the ONE definition of "a node an Agent can be installed onto".
 *
 * Both the Agent library dialog and the first-run onboarding step consume
 * this. There is deliberately no `gatewayMode` input and no "local node"
 * special case: availability is derived only from protocol state exposed
 * by `GET /api/nodes`.
 *
 * `control_ready` (NodeReady / bootstrap `node.{id}`) and `can_manage`
 * are optional so an older Gateway still works — when absent, the final
 * readiness gate stays with the Gateway's `ensure_node_ready()` and the
 * structured `dependency_not_ready` retry.
 */
import type { NodeInfo } from "../../lib/types";

export function usableNodesOf(nodes: NodeInfo[]): NodeInfo[] {
  return nodes.filter(
    (n) => n.online && n.control_ready !== false && n.can_manage !== false,
  );
}

export function onlineNodesOf(nodes: NodeInfo[]): NodeInfo[] {
  return nodes.filter((n) => n.online);
}

/**
 * How long an offline node keeps its place in the list.
 *
 * A node that dropped out is worth showing for a while — a Wi-Fi switch, an
 * app restart, a Gateway bounce are all normal and the user wants to see the
 * machine come back. A device that has been silent for longer than this is
 * not coming back on its own (it was uninstalled, or abandoned), and keeping
 * it in the list as a permanent "offline" row is noise that looks like a
 * broken product.
 *
 * This is a DISPLAY rule only. Nothing here deletes or forgets a node: the
 * Gateway keeps the record until an explicit `DELETE /api/nodes/{id}`, which
 * is the only thing that can revoke a device's credential. Hiding must never
 * be mistaken for removing — that confusion is what made the old automatic
 * offline cleanup destructive enough to be reverted.
 */
export const NODE_OFFLINE_DISPLAY_WINDOW_MS = 5 * 60 * 1000;

/**
 * Whether an offline node has been silent long enough to drop out of the
 * default list.
 *
 * Fail-open by design: a node that is online is never stale, and a Gateway
 * that does not report `last_seen_at` (older than the field) hides nothing —
 * guessing from `online_since` would evict nodes for reasons we cannot see.
 */
export function isStaleOfflineNode(
  node: NodeInfo,
  now: number = Date.now(),
): boolean {
  if (node.online) return false;
  const seen = Date.parse(node.last_seen_at ?? "");
  if (Number.isNaN(seen)) return false;
  return now - seen > NODE_OFFLINE_DISPLAY_WINDOW_MS;
}


export type NoNodeDetail =
  | "none"
  | "notReady"
  | "notManageable"
  | "blocked";

/**
 * Why the usable set is empty, expressed as an i18n suffix (never as a
 * deployment/launcher statement). `none` = no online nodes at all.
 */
export function noNodeDetailOf(nodes: NodeInfo[]): NoNodeDetail {
  const online = onlineNodesOf(nodes);
  if (online.length === 0) return "none";
  if (online.every((n) => n.control_ready === false)) return "notReady";
  if (online.every((n) => n.can_manage === false)) return "notManageable";
  return "blocked";
}

export function noNodeDetailKey(detail: NoNodeDetail): string | null {
  switch (detail) {
    case "none":
      return null;
    case "notReady":
      return "agentHub.noNodeDetailNotReady";
    case "notManageable":
      return "agentHub.noNodeDetailNotManageable";
    default:
      return "agentHub.noNodeDetailBlocked";
  }
}
