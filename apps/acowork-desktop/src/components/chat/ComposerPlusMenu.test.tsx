// @vitest-environment jsdom
/**
 * The composer `+` menu — what only this component can get wrong.
 *
 * The switch *semantics* (narrow-never-widen, deny bookkeeping) live in
 * `lib/toolInventory.ts` and are pinned there; the persistence contract lives
 * in `stores/toolPolicyStore.ts`. This file pins the navigation and the wiring:
 * four level-1 entries, drill-down into a server's tools, the attachment row
 * handing off to the picker, and a switch reaching the store.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("../../lib/logger", () => ({
  log: { warn: vi.fn(), debug: vi.fn(), info: vi.fn(), error: vi.fn() },
  setLevel: vi.fn(),
  getLevel: vi.fn(),
}));
// The skill level is another extracted component with its own store; keep this
// test to the menu shell.
vi.mock("../skills/SkillMenuList", () => ({
  SkillMenuList: ({ onDone }: { onDone?: () => void }) => (
    <div>
      <span>skill-list</span>
      <button type="button" onClick={() => onDone?.()}>
        pick-skill
      </button>
    </div>
  ),
}));

import { ComposerPlusMenu } from "./ComposerPlusMenu";
import { useToolPolicyStore } from "../../stores/toolPolicyStore";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";

const TOOLS = {
  tools: [
    { name: "read_file", enabled: true },
    { name: "shell", enabled: false },
  ],
  mcp_servers_defs: [
    { name: "playwright", transport: "stdio", command: "npx", args: [], active: true },
  ],
};

const MCP_TOOLS = { playwright: [{ name: "navigate", enabled: true }] };

function stubFetch(putStatus = 200) {
  const puts: unknown[] = [];
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string, init?: RequestInit) => {
      const ok = (payload: unknown) =>
        Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(payload) });
      if (url.endsWith("/agents/agent-1/tools")) return ok(TOOLS);
      if (url.endsWith("/agents/agent-1/mcp-tools")) return ok(MCP_TOOLS);
      if (url.includes("/sessions/s-1/tool-policy")) {
        if (init?.method === "PUT") {
          puts.push(JSON.parse(String(init.body)));
          return Promise.resolve({
            ok: putStatus === 200,
            status: putStatus,
            json: () => Promise.resolve(JSON.parse(String(init.body))),
          });
        }
        return ok({ tool_policy: null });
      }
      return ok({});
    }),
  );
  return puts;
}

function renderMenu(onPickAttachments = vi.fn()) {
  render(
    <ComposerPlusMenu readOnly={false} onPickAttachments={onPickAttachments} />,
  );
  fireEvent.click(screen.getByLabelText("chatPanel.plusMenu"));
  return onPickAttachments;
}

describe("ComposerPlusMenu", () => {
  beforeEach(() => {
    // Real stores, seeded: the menu reads the agent / active session from them
    // and `useAgentStore()` is called without a selector, which a call-through
    // mock cannot satisfy.
    useAgentStore.setState({ selectedAgentId: "agent-1" });
    useChatStore.setState({
      agentStates: { "agent-1": { activeSessionId: "s-1" } },
    } as never);
    useToolPolicyStore.setState({
      inventory: {},
      inventoryLoading: {},
      inventoryError: {},
      policies: {},
      inFlight: {},
    });
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("opens on four level-1 entries", () => {
    stubFetch();
    renderMenu();
    expect(screen.getByText("chatPanel.plusSkills")).toBeTruthy();
    expect(screen.getByText("chatPanel.plusMcp")).toBeTruthy();
    expect(screen.getByText("chatPanel.plusBuiltinTools")).toBeTruthy();
    expect(screen.getByText("chatPanel.plusAttachments")).toBeTruthy();
  });

  it("hands attachments off to the picker and closes", () => {
    stubFetch();
    const onPick = renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusAttachments"));
    expect(onPick).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("chatPanel.plusSkills")).toBeNull();
  });

  it("drills into skills and closes on a pick", () => {
    stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusSkills"));
    fireEvent.click(screen.getByText("pick-skill"));
    expect(screen.queryByText("skill-list")).toBeNull();
  });

  it("switches a builtin tool off through the store", async () => {
    const puts = stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusBuiltinTools"));

    // `read_file` is agent-enabled, `shell` is not.
    const switches = await screen.findAllByRole("switch");
    expect(switches).toHaveLength(2);
    expect((switches[0] as HTMLInputElement).checked).toBe(true);
    expect((switches[1] as HTMLInputElement).disabled).toBe(true);

    fireEvent.click(switches[0]);
    await waitFor(() => expect(puts).toHaveLength(1));
    expect(puts[0]).toEqual({
      tool_policy: {
        disabled_builtin_tools: ["read_file"],
        disabled_mcp_servers: [],
        // The wire shape is Rust's `BTreeMap`, not the UI's identity list.
        disabled_mcp_tools: {},
      },
    });
  });

  it("holds the switches until the session policy has been read", async () => {
    // A GET that never settles: the read is in flight for the whole test.
    vi.stubGlobal(
      "fetch",
      vi.fn((url: string) => {
        const ok = (payload: unknown) =>
          Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(payload) });
        if (url.endsWith("/agents/agent-1/tools")) return ok(TOOLS);
        if (url.endsWith("/agents/agent-1/mcp-tools")) return ok(MCP_TOOLS);
        if (url.includes("/tool-policy")) return new Promise(() => {});
        return ok({});
      }),
    );
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusBuiltinTools"));

    const switches = await screen.findAllByRole("switch");
    // Every switch is held and the reason is on screen: a toggle would be a
    // whole-policy PUT computed from a value that never arrived.
    expect(switches.every((s) => (s as HTMLInputElement).disabled)).toBe(true);
    expect(screen.getByText("chatPanel.plusLoading")).toBeTruthy();
  });

  it("drills from a server into its tools", async () => {
    stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusMcp"));

    // The server row: its switch plus a chevron for the tool level.
    const serverSwitch = await screen.findByRole("switch");
    expect((serverSwitch as HTMLInputElement).checked).toBe(true);
    fireEvent.click(screen.getByLabelText("tools"));

    await waitFor(() => expect(screen.getByText("navigate")).toBeTruthy());
    // The server switch is gone; the tool's is the only one left.
    expect(screen.getAllByRole("switch")).toHaveLength(1);
  });

  it("does not offer level 2 for an agent-disabled server", async () => {
    stubFetch();
    useToolPolicyStore.setState((s) => ({
      ...s,
      inventory: {
        "agent-1": {
          agentId: "agent-1",
          builtin: [],
          mcpServers: [
            {
              name: "pm",
              agentEnabled: false,
              tools: [
                {
                  server: "pm",
                  tool: "list_tasks",
                  prefixedName: "pm__list_tasks",
                  agentEnabled: false,
                },
              ],
            },
          ],
        },
      },
    }));
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusMcp"));
    // No chevron: the box it would open holds nothing reachable.
    expect(screen.queryByLabelText("tools")).toBeNull();
  });

  it("shows the reset affordance only when the group denies something", async () => {
    stubFetch();
    renderMenu();
    fireEvent.click(screen.getByText("chatPanel.plusBuiltinTools"));
    const reset = await screen.findByLabelText("chatPanel.followAgentDefault");
    expect((reset as HTMLButtonElement).disabled).toBe(true);

    await waitFor(() => expect(useToolPolicyStore.getState().inventory["agent-1"]).toBeTruthy());
    useToolPolicyStore.setState((s) => ({
      ...s,
      policies: {
        ...s.policies,
        "agent-1/s-1": {
          policy: {
            disabled_builtin_tools: ["read_file"],
            disabled_mcp_servers: [],
            disabled_mcp_tools: {},
          },
          loaded: true,
          loading: false,
          saving: false,
          error: null,
          revision: 0,
        },
      },
    }));
    await waitFor(() =>
      expect(
        (screen.getByLabelText("chatPanel.followAgentDefault") as HTMLButtonElement)
          .disabled,
      ).toBe(false),
    );
  });
});
