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
