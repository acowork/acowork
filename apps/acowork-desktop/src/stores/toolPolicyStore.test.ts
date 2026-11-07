/**
 * Per-session tool policy store: optimistic writes, rollback, and the "newest
 * edit wins" rule.
 *
 * The store is a cache of Runtime state, so each test stubs `fetch` and asserts
 * on what the UI would render: the cached policy, `saving`, and `error`.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { selectPolicyEntry, useToolPolicyStore } from "./toolPolicyStore";
import { emptyToolPolicy } from "../lib/toolInventory";

const AGENT = "agent-1";
const SESSION = "s-1";
const BASE = "http://gw";

interface Call {
  method: string;
  url: string;
  body?: unknown;
}

/** Route stubs by (method, url-suffix). */
function stubFetch(routes: {
  tools?: unknown;
  mcpTools?: unknown;
  getPolicy?: unknown;
  putPolicy?: (body: unknown) => { ok: boolean; status?: number; payload?: unknown };
}) {
  const calls: Call[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      const body =
        typeof init?.body === "string" ? JSON.parse(init.body) : undefined;
      calls.push({ method, url, body });
      const ok = (payload: unknown) =>
        Promise.resolve({
          ok: true,
          status: 200,
          text: () => Promise.resolve(JSON.stringify(payload)),
          json: () => Promise.resolve(payload),
        });

      if (url.endsWith(`/agents/${AGENT}/tools`)) return ok(routes.tools ?? {});
      if (url.endsWith(`/agents/${AGENT}/mcp-tools`))
        return ok(routes.mcpTools ?? {});
      if (url.endsWith(`/sessions/${SESSION}/tool-policy`)) {
        if (method === "GET")
          return ok({ tool_policy: routes.getPolicy ?? null });
        const res = routes.putPolicy?.(body ?? {}) ?? { ok: true };
        return Promise.resolve({
          ok: res.ok,
          status: res.status ?? 200,
          text: () => Promise.resolve("rejected"),
          json: () => Promise.resolve(res.payload ?? body),
        });
      }
      return Promise.resolve({
        ok: false,
        status: 404,
        json: () => Promise.resolve({}),
      });
    }),
  );
  return calls;
}

const TOOLS = {
  tools: [
    { name: "read_file", enabled: true },
    { name: "shell", enabled: false },
  ],
  mcp_servers_defs: [
    { name: "playwright", transport: "stdio", command: "npx", args: [], active: true },
  ],
};
const MCP_TOOLS = {
  playwright: [{ name: "navigate", enabled: true }],
};

function reset() {
  useToolPolicyStore.setState({
    inventory: {},
    inventoryLoading: {},
    inventoryError: {},
    policies: {},
    inFlight: {},
  });
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
}

/** Load inventory + policy, the way the menu does on open. */
async function arrange(getPolicy: unknown = null) {
  const calls = stubFetch({ tools: TOOLS, mcpTools: MCP_TOOLS, getPolicy });
  await useToolPolicyStore.getState().loadInventory(AGENT);
  await useToolPolicyStore.getState().loadPolicy(AGENT, SESSION);
  return calls;
}

function state() {
  return selectPolicyEntry(useToolPolicyStore.getState(), AGENT, SESSION);
}

describe("inventory + policy loading", () => {
  beforeEach(reset);

  it("caches the agent layer per agent, not per session", async () => {
    const calls = await arrange();
    await useToolPolicyStore.getState().loadInventory(AGENT);
    expect(calls.filter((c) => c.url.endsWith("/tools"))).toHaveLength(1);
    expect(useToolPolicyStore.getState().inventory[AGENT].mcpServers[0].name).toBe(
      "playwright",
    );
  });

  it("reads a null policy as 'follow the agent layer'", async () => {
    await arrange(null);
    expect(state().policy).toEqual(emptyToolPolicy());
    expect(state().error).toBeNull();
  });

  it("hydrates a stored policy", async () => {
    await arrange({ disabled_builtin_tools: ["read_file"] });
    expect(state().policy.disabled_builtin_tools).toEqual(["read_file"]);
  });

  it("keeps the inventory when the policy read fails", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        if (url.endsWith("/tools"))
          return Promise.resolve({
            ok: true,
            status: 200,
            json: () => Promise.resolve(TOOLS),
          });
        if (url.endsWith("/mcp-tools"))
          return Promise.resolve({
            ok: true,
            status: 200,
            json: () => Promise.resolve(MCP_TOOLS),
          });
        return Promise.resolve({
          ok: false,
          status: 500,
          text: () => Promise.resolve("boom"),
          json: () => Promise.resolve({}),
        });
      }),
    );
    await useToolPolicyStore.getState().loadInventory(AGENT);
    await useToolPolicyStore.getState().loadPolicy(AGENT, SESSION);
    expect(useToolPolicyStore.getState().inventory[AGENT]).toBeDefined();
    expect(state().error).toContain("500");
    // A failed read leaves the session inheriting, so the menu still works.
    expect(state().policy).toEqual(emptyToolPolicy());
  });
});

describe("toggles", () => {
  beforeEach(reset);

  it("writes only the group the user touched", async () => {
    const calls = await arrange();
    await useToolPolicyStore.getState().toggleBuiltin(AGENT, SESSION, "read_file", false);

    const put = calls.find((c) => c.method === "PUT");
    expect(put?.body).toEqual({
      tool_policy: {
        disabled_builtin_tools: ["read_file"],
        disabled_mcp_servers: [],
        // The wire shape is Rust's `BTreeMap`, not the UI's identity list.
        disabled_mcp_tools: {},
      },
    });
    expect(state().policy.disabled_builtin_tools).toEqual(["read_file"]);
    expect(state().saving).toBe(false);
  });

  it("sends null once everything is switched back on", async () => {
    const calls = await arrange({ disabled_builtin_tools: ["read_file"] });
    await useToolPolicyStore.getState().toggleBuiltin(AGENT, SESSION, "read_file", true);
    const put = calls.find((c) => c.method === "PUT");
    expect(put?.body).toEqual({ tool_policy: null });
    expect(state().policy).toEqual(emptyToolPolicy());
  });

  it("persists nothing when the switch cannot move", async () => {
    const calls = await arrange();
    // `shell` is off in the agent layer: switching it on is impossible.
    await useToolPolicyStore.getState().toggleBuiltin(AGENT, SESSION, "shell", true);
    expect(calls.some((c) => c.method === "PUT")).toBe(false);
  });

  it("persists nothing when a group reset changes nothing", async () => {
    const calls = await arrange();
    await useToolPolicyStore.getState().resetGroup(AGENT, SESSION, "mcpServers");
    expect(calls.some((c) => c.method === "PUT")).toBe(false);
  });

  it("refuses to write before the session policy has been read", async () => {
    // The PUT is a whole-policy replacement: committing on top of the empty
    // default would erase denies set by another surface or another device. The
    // menu disables the switches in this window; the store must not depend on
    // the UI for that.
    const calls = stubFetch({ tools: TOOLS, mcpTools: MCP_TOOLS });
    await useToolPolicyStore.getState().loadInventory(AGENT);
    await useToolPolicyStore.getState().toggleBuiltin(AGENT, SESSION, "shell", false);
    expect(calls.some((c) => c.method === "PUT")).toBe(false);
    expect(state().policy.disabled_builtin_tools).toEqual([]);
    expect(state().loaded).toBe(false);
  });

  it("writes once the read has landed", async () => {
    const calls = await arrange();
    await useToolPolicyStore.getState().toggleBuiltin(AGENT, SESSION, "shell", false);
    expect(calls.filter((c) => c.method === "PUT")).toHaveLength(1);
    expect(state().loaded).toBe(true);
  });

  it("rolls back to the accepted policy when the PUT fails", async () => {
    stubFetch({
      tools: TOOLS,
      mcpTools: MCP_TOOLS,
      getPolicy: { disabled_builtin_tools: ["read_file"] },
      putPolicy: () => ({ ok: false, status: 400 }),
    });
    await useToolPolicyStore.getState().loadInventory(AGENT);
    await useToolPolicyStore.getState().loadPolicy(AGENT, SESSION);

    await useToolPolicyStore.getState().toggleBuiltin(AGENT, SESSION, "shell", false);

    expect(state().policy.disabled_builtin_tools).toEqual(["read_file"]);
    expect(state().saving).toBe(false);
    expect(state().error).toContain("400");
  });

  it("keeps the newest edit when an older PUT echo lands late", async () => {
    const pending: ((v: unknown) => void)[] = [];
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string, init?: RequestInit) => {
        if (url.endsWith("/tools"))
          return Promise.resolve({
            ok: true,
            status: 200,
            json: () => Promise.resolve(TOOLS),
          });
        if (url.endsWith("/mcp-tools"))
          return Promise.resolve({
            ok: true,
            status: 200,
            json: () => Promise.resolve(MCP_TOOLS),
          });
        if (init?.method === "PUT") {
          const body = JSON.parse(String(init.body));
          return new Promise((resolve) => {
            pending.push(() =>
              resolve({
                ok: true,
                status: 200,
                json: () => Promise.resolve(body),
              }),
            );
          });
        }
        return Promise.resolve({
          ok: true,
          status: 200,
          json: () => Promise.resolve({ tool_policy: null }),
        });
      }),
    );
    await useToolPolicyStore.getState().loadInventory(AGENT);
    await useToolPolicyStore.getState().loadPolicy(AGENT, SESSION);

    const first = useToolPolicyStore
      .getState()
      .toggleBuiltin(AGENT, SESSION, "read_file", false);
    const second = useToolPolicyStore
      .getState()
      .toggleBuiltin(AGENT, SESSION, "shell", false);

    // Resolve the FIRST request last: its echo must not clobber edit two.
    pending[1]!({});
    await second;
    pending[0]!({});
    await first;

    expect(state().policy.disabled_builtin_tools).toEqual(["read_file", "shell"]);
  });
});

// Keep `BASE` referenced: the stubs match on suffix, and this documents that
// every URL below is built from the gateway base URL.
void BASE;
