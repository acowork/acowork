/**
 * ADR-073 §4: pure helper that builds the sidebar's per-node
 * groups. Kept side-effect-free so it is trivially unit-testable without
 * mocking stores.
 *
 * The node is the primary axis, not the agent: the sidebar is the only
 * place a node can be acted on (e.g. installing the first agent onto an
 * empty node), so a node with zero agents must still produce a group.
 * Deriving the groups from `agents` instead would make an agent-less node
 * structurally invisible — hence one group per `nodes` entry, always.
 *
 * Contract:
 *   - Groups are emitted in `nodes` order (the Gateway's natural
 *     ordering), one per node, including nodes with no agents.
 *   - Agents are attached by `agent.node_id`; empty/missing → bucket key
 *     `"__unknown__"`.
 *   - Agents whose `node_id` is not present in `nodes` (or whose
 *     `node_id` is missing) are appended as a trailing "unknown"
 *     bucket so they never silently disappear from the UI.
 *   - Within each group the agents keep their input order (which the
 *     caller controls via `filteredAgents`).
 */
import type { AgentInfo, NodeInfo } from "../../lib/types";

export interface AgentNodeGroup {
  /** Stable bucket key — `node.node_id` for known nodes, `"__unknown__"` for the fallback. */
  nodeId: string;
  /** The matching NodeInfo, or `null` for the unknown bucket. */
  node: NodeInfo | null;
  /** Agents in this bucket, in their original order. */
  agents: AgentInfo[];
}

export const UNKNOWN_NODE_ID = "__unknown__";

export function partitionAgentsByNode(
  agents: AgentInfo[],
  nodes: NodeInfo[],
): AgentNodeGroup[] {
  const byNode = new Map<string, AgentInfo[]>();
  for (const a of agents) {
    const nid = a.node_id ?? UNKNOWN_NODE_ID;
    let arr = byNode.get(nid);
    if (!arr) {
      arr = [];
      byNode.set(nid, arr);
    }
    arr.push(a);
  }
  const ordered: AgentNodeGroup[] = [];
  for (const n of nodes) {
    // A node with zero agents is still a group: it is the only place the
    // sidebar can offer "install the first agent here" (ADR-073 §4).
    ordered.push({ nodeId: n.node_id, node: n, agents: byNode.get(n.node_id) ?? [] });
  }
  const knownIds = new Set(nodes.map((n) => n.node_id));
  for (const [nid, agentsForNode] of byNode) {
    if (!knownIds.has(nid) && agentsForNode.length > 0) {
      ordered.push({ nodeId: nid, node: null, agents: agentsForNode });
    }
  }
  return ordered;
}

/**
 * Display name for a group (ADR-075 D8): the renameable `node_name`
 * wins, then the operator-friendly hostname, then the opaque node_id
 * (a UUID). Falls back to the raw nodeId (e.g. `"__unknown__"` for the
 * orphan bucket) so the header always renders.
 */
export function nodeDisplayName(group: AgentNodeGroup): string {
  return (
    group.node?.node_name ??
    group.node?.hostname ??
    group.node?.node_id ??
    group.nodeId
  );
}
