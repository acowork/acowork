/**
 * Switch semantics of the session tool layer (desktop side).
 *
 * The point of these tests is the ONE invariant the UI must never break: a
 * session can narrow the agent layer but never widen it. Everything else here
 * is bookkeeping around that.
 */
import { describe, expect, it } from "vitest";
import {
  buildToolInventory,
  deriveBuiltinState,
  deriveMcpServerState,
  deriveMcpToolState,
  emptyToolPolicy,
  isToolPolicyEmpty,
  fromWirePolicy,
  toWirePolicy,
  policyOrNull,
  prefixedMcpToolName,
  withBuiltinToolToggled,
  withGroupReset,
  withMcpServerToggled,
  withMcpToolToggled,
  type AgentToolInventory,
  type SessionToolPolicy,
} from "./toolInventory";
import type { McpServerView } from "./types";

function serverView(name: string, active: boolean): McpServerView {
  return {
    name,
    transport: "stdio",
    command: "npx",
    args: [],
    active,
  } as McpServerView;
}

function inventory(): AgentToolInventory {
  return buildToolInventory(
    "agent-1",
    {
      tools: [
        { name: "read_file", enabled: true },
        { name: "shell", enabled: false },
      ],
      mcp_servers_defs: [serverView("playwright", true), serverView("pm", false)],
    },
    {
      playwright: [
        { name: "navigate", enabled: true },
        { name: "screenshot", enabled: false },
      ],
      pm: [{ name: "list_tasks", enabled: true }],
    },
  );
}

describe("buildToolInventory", () => {
  it("folds /tools + /mcp-tools into one inventory", () => {
    const inv = inventory();
    expect(inv.agentId).toBe("agent-1");
    expect(inv.builtin.map((t) => [t.name, t.agentEnabled])).toEqual([
      ["read_file", true],
      ["shell", false],
    ]);
    expect(inv.mcpServers.map((s) => [s.name, s.agentEnabled])).toEqual([
      ["playwright", true],
      ["pm", false],
    ]);
  });

  it("names MCP tools the way the tool layer does", () => {
    const tools = inventory().mcpServers[0].tools;
    expect(tools.map((t) => t.prefixedName)).toEqual([
      "playwright__navigate",
      "playwright__screenshot",
    ]);
    expect(prefixedMcpToolName("a", "b")).toBe("a__b");
  });

  it("treats a tool under a switched-off server as unavailable", () => {
    // `pm` is off at the agent level, so its (enabled) tool cannot be reached
    // at either layer — the menu must not offer to switch it on.
    const pm = inventory().mcpServers[1];
    expect(pm.tools[0].agentEnabled).toBe(false);
  });

  it("survives a missing /mcp-tools payload", () => {
    const inv = buildToolInventory(
      "agent-1",
      { tools: [], mcp_servers_defs: [serverView("playwright", true)] },
      null,
    );
    expect(inv.mcpServers[0].tools).toEqual([]);
  });
});

describe("policy normalisation", () => {
  it("makes a partial wire value total", () => {
    expect(fromWirePolicy(null)).toEqual(emptyToolPolicy());
    expect(fromWirePolicy({ disabled_builtin_tools: ["shell"] })).toEqual({
      disabled_builtin_tools: ["shell"],
      disabled_mcp_servers: [],
      disabled_mcp_tools: [],
    });
  });

  /**
   * The wire shape is Rust's `BTreeMap<String, BTreeSet<String>>`, not the
   * `McpToolRef[]` the UI iterates. Both fixtures below are the literals
   * `acowork-core`'s `mcp_tool_denies_serialise_as_a_map` asserts, so a change
   * on either side of the boundary breaks exactly one test.
   */
  it("converts the per-server tool map both ways", () => {
    const wire = {
      disabled_builtin_tools: ["web_fetch"],
      disabled_mcp_servers: ["docling"],
      disabled_mcp_tools: { pm: ["pm_claim_task"], docling: ["convert"] },
    };
    const policy = fromWirePolicy(wire);
    expect(policy.disabled_mcp_tools).toEqual([
      { server: "pm", tool: "pm_claim_task" },
      { server: "docling", tool: "convert" },
    ]);
    expect(toWirePolicy(policy)).toEqual(wire);
  });

  it("round-trips an empty policy without inventing groups", () => {
    expect(toWirePolicy(emptyToolPolicy())).toEqual({
      disabled_builtin_tools: [],
      disabled_mcp_servers: [],
      disabled_mcp_tools: {},
    });
    expect(fromWirePolicy(toWirePolicy(emptyToolPolicy()))).toEqual(
      emptyToolPolicy(),
    );
  });

  it("reads an all-empty policy as 'inherit the agent layer'", () => {
    expect(isToolPolicyEmpty(emptyToolPolicy())).toBe(true);
    expect(policyOrNull(emptyToolPolicy())).toBeNull();
    const narrowed = withGroupReset(
      { ...emptyToolPolicy(), disabled_builtin_tools: ["shell"] },
      "builtin",
    );
    expect(policyOrNull(narrowed)).toBeNull();
  });
});

describe("effective switch state", () => {
  it("is the agent layer minus the session denies", () => {
    const inv = inventory();
    const policy: SessionToolPolicy = {
      ...emptyToolPolicy(),
      disabled_builtin_tools: ["read_file"],
    };
    expect(deriveBuiltinState(inv.builtin[0], policy)).toEqual({
      agentEnabled: true,
      denied: true,
      effective: false,
    });
    expect(deriveBuiltinState(inv.builtin[1], policy).effective).toBe(false);
  });

  it("lets a server deny cover every tool under it", () => {
    const inv = inventory();
    const policy: SessionToolPolicy = {
      ...emptyToolPolicy(),
      disabled_mcp_servers: ["playwright"],
    };
    expect(deriveMcpServerState(inv.mcpServers[0], policy).effective).toBe(false);
    expect(deriveMcpToolState(inv.mcpServers[0].tools[0], policy)).toEqual({
      agentEnabled: true,
      denied: true,
      effective: false,
    });
  });

  it("reports the agent-layer state when the session denies nothing", () => {
    const inv = inventory();
    const policy = emptyToolPolicy();
    expect(deriveMcpToolState(inv.mcpServers[0].tools[0], policy).effective).toBe(
      true,
    );
    // Off at the agent layer: not effective, and not because of the session.
    const screenshot = deriveMcpToolState(inv.mcpServers[0].tools[1], policy);
    expect(screenshot).toEqual({
      agentEnabled: false,
      denied: false,
      effective: false,
    });
  });
});

describe("policy edits", () => {
  it("narrows and un-narrows a builtin tool", () => {
    const inv = inventory();
    const off = withBuiltinToolToggled(emptyToolPolicy(), inv.builtin[0], false);
    expect(off.disabled_builtin_tools).toEqual(["read_file"]);
    const back = withBuiltinToolToggled(off, inv.builtin[0], true);
    expect(back.disabled_builtin_tools).toEqual([]);
  });

  it("refuses to widen the agent layer", () => {
    const inv = inventory();
    const policy = emptyToolPolicy();
    // `shell` is off in agent-tools.json — a session cannot switch it on.
    expect(withBuiltinToolToggled(policy, inv.builtin[1], true)).toBe(policy);
    const pm = inv.mcpServers[1];
    expect(withMcpServerToggled(policy, pm, true)).toBe(policy);
    expect(withMcpToolToggled(policy, pm.tools[0], true)).toBe(policy);
  });

  it("keeps per-tool denies when the server is switched back on", () => {
    const inv = inventory();
    const server = inv.mcpServers[0];
    const withDeny = withMcpToolToggled(emptyToolPolicy(), server.tools[0], false);
    const off = withMcpServerToggled(withDeny, server, false);
    expect(off.disabled_mcp_servers).toEqual(["playwright"]);
    expect(off.disabled_mcp_tools).toEqual([
      { server: "playwright", tool: "navigate" },
    ]);

    const back = withMcpServerToggled(off, server, true);
    expect(back.disabled_mcp_servers).toEqual([]);
    expect(deriveMcpToolState(server.tools[0], back).effective).toBe(false);
    expect(deriveMcpToolState(server.tools[1], back).effective).toBe(false);
  });

  it("lifts a server deny when one of its tools is switched on", () => {
    const inv = inventory();
    const server = inv.mcpServers[0];
    const off = withMcpServerToggled(emptyToolPolicy(), server, false);
    const one = withMcpToolToggled(off, server.tools[0], true);
    expect(one.disabled_mcp_servers).toEqual([]);
    expect(deriveMcpToolState(server.tools[0], one).effective).toBe(true);
  });

  it("resets one group without touching the others", () => {
    const policy: SessionToolPolicy = {
      disabled_builtin_tools: ["shell"],
      disabled_mcp_servers: ["playwright"],
      disabled_mcp_tools: [{ server: "playwright", tool: "navigate" }],
    };
    expect(withGroupReset(policy, "mcpServers")).toEqual({
      disabled_builtin_tools: ["shell"],
      disabled_mcp_servers: [],
      disabled_mcp_tools: policy.disabled_mcp_tools,
    });
  });

  it("does not mutate the policy it is given", () => {
    const inv = inventory();
    const policy = emptyToolPolicy();
    withMcpToolToggled(policy, inv.mcpServers[0].tools[0], false);
    expect(policy).toEqual(emptyToolPolicy());
  });
});
