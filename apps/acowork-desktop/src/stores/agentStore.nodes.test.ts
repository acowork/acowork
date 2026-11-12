/**
 * Node topology snapshot lifecycle in `agentStore` (ADR-073 §4 sidebar
 * groups, ADR-059 bootstrap refetch).
 *
 * The snapshot lives in the store — not in `AgentList` — so the Gateway
 * connection lifecycle drives it from one place:
 *   - `fetchNodes()`: reconcile from the Gateway; failure keeps the
 *     previous snapshot (a transient blip mustn't empty the sidebar).
 *   - `markNodesOffline()`: Gateway drop edge — the snapshot is stale
 *     the moment the Gateway dies, and no new `bootstrap-state` will
 *     arrive to bump `bootstrapVersion` (the broker lived in the
 *     Gateway), so this is the only thing that turns the sidebar node
 *     group headers gray.
 */

import { describe, it, expect, vi, beforeEach } from "vitest";

const mockFetchNodes = vi.fn<[], Promise<unknown[]>>();

vi.mock("../lib/gateway-api", () => ({
  fetchNodes: () => mockFetchNodes(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: () => Promise.reject(new Error("unexpected invoke in nodes test")),
}));

vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
  setLevel: () => {},
  getLevel: () => "off" as const,
}));

vi.mock("../lib/profileStore", () => ({
  loadAllProfiles: () => ({}),
  loadProfile: () => null,
  saveProfile: () => {},
  DEFAULT_PROFILE: {
    language: "zh-CN",
    timezone: "Asia/Shanghai",
    model_override: null,
    reasoning_effort: null,
    auto_approve_tools: [],
    yolo_mode: false,
  },
}));

import { useAgentStore } from "./agentStore";

const NODE_A = {
  node_id: "n-1",
  online: true,
  capabilities: [],
  hostname: "alpha",
};
const NODE_B = {
  node_id: "n-2",
  online: true,
  capabilities: [],
  hostname: "beta",
};

beforeEach(() => {
  useAgentStore.setState({ nodes: [] });
  mockFetchNodes.mockReset();
});

describe("agentStore.fetchNodes", () => {
  it("reconciles the topology from the Gateway on success", async () => {
    mockFetchNodes.mockResolvedValue([NODE_A, NODE_B] as never);
    await useAgentStore.getState().fetchNodes();
    expect(useAgentStore.getState().nodes).toHaveLength(2);
    expect(useAgentStore.getState().nodes[0].node_id).toBe("n-1");
  });

  it("keeps the previous snapshot when the Gateway is unreachable", async () => {
    useAgentStore.setState({ nodes: [NODE_A] as never });
    mockFetchNodes.mockRejectedValue(new Error("Gateway unreachable"));
    await useAgentStore.getState().fetchNodes();
    // A transient blip must not empty the remote-mode sidebar groups.
    expect(useAgentStore.getState().nodes).toHaveLength(1);
    expect(useAgentStore.getState().nodes[0].online).toBe(true);
  });
});

describe("agentStore.markNodesOffline", () => {
  it("flips every known node offline and keeps the structure", () => {
    useAgentStore.setState({ nodes: [NODE_A, NODE_B] as never });
    useAgentStore.getState().markNodesOffline();
    const nodes = useAgentStore.getState().nodes;
    expect(nodes).toHaveLength(2);
    for (const n of nodes) {
      expect(n.online).toBe(false);
    }
    // Headers keep their identity/name so the sidebar doesn't collapse.
    expect(nodes[0].node_id).toBe("n-1");
    expect(nodes[1].hostname).toBe("beta");
  });

  it("is a safe no-op with an empty snapshot", () => {
    useAgentStore.getState().markNodesOffline();
    expect(useAgentStore.getState().nodes).toEqual([]);
  });
});
