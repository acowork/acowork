/**
 * Picking an avatar must reach every surface without a round-trip.
 *
 * ## The bug this pins
 *
 * The Setup panel's avatar handlers used to `await fetchAgents()` right after
 * `PUT /api/agents/{id}/avatar-config`. Two problems:
 *
 *   1. That PUT is answered by the *Runtime*, which owns
 *      `agent_config.json` — the response is already the effective identity.
 *      `GET /api/agents` reaches the same facts by a different, slower path
 *      (Node re-reads the file → republishes its retained inventory →
 *      Gateway upserts → `acowork/desktop/inventory` → `inventoryVersion` →
 *      `AgentList` refetches). Until that lands the list still reports the
 *      OLD avatar, so the immediate refetch wrote stale data over fresh and
 *      the sidebar / transcript / panel header kept the previous picture.
 *   2. It made the panel's own header flash: the store seeded
 *      `AgentAvatar` with the stale custom avatar, then the async
 *      `avatar-config` response swapped in the picked icon.
 *
 * The contract, then, is: **confirm optimistically from the PUT response, and
 * let the inventory doorbell's refetch be a convergence check.** The
 * assertions below are exactly that — the row is correct the moment the
 * request resolves, and no list fetch is issued to get there.
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useAgentStore, type AgentStorage } from "../../stores/agentStore";
import type { AgentInfo, AvatarConfigResponse } from "../../lib/types";
import { AgentSetupTab } from "./AgentSetupTab";

const listAgents = vi.fn<[], Promise<unknown[]>>();

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (cmd: string) => {
    if (cmd === "list_agents") return listAgents();
    return Promise.resolve({});
  },
}));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn(async () => null) }));
vi.mock("../../lib/gateway-api", () => ({ fetchProviders: vi.fn(async () => []) }));

const updateAvatarConfig = vi.fn<[], Promise<AvatarConfigResponse>>();

vi.mock("../../lib/avatar", async (importOriginal) => ({
  // Keep the pure helpers real (the profile store calls them while this
  // module graph initialises); stub only the network surface.
  ...(await importOriginal<typeof import("../../lib/avatar")>()),
  fetchAvatarAssets: vi.fn(async () => ({ assets: [] })),
  fetchAvatarConfig: vi.fn(async () => null),
  updateAvatarConfig: () => updateAvatarConfig(),
  deleteAvatarFile: vi.fn(async () => ({})),
}));

const INSTANCE = "inst-1";
const OLD_CUSTOM_AVATAR = "assets/avatar-02.png";
const PICKED_ICON = "icon-09";

function seedAgent(over: Partial<AgentInfo> = {}) {
  const meta = {
    instance_id: INSTANCE,
    agent_id: "com.acowork.senior-engineer",
    name: "SSE.Kevin",
    version: "1.0.0",
    alive: true,
    avatar: OLD_CUSTOM_AVATAR,
    ...over,
  } as AgentInfo;
  useAgentStore.setState({
    agents: { [INSTANCE]: { meta, profile: {}, sessions: [] } } as unknown as Record<
      string,
      AgentStorage
    >,
    selectedAgentId: INSTANCE,
  });
}

/** Minimal gateway surface: the panel reads `/config` and two tool endpoints;
 *  anything else may 404 (those cards handle it and start collapsed). */
function stubFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const json = (body: unknown) =>
        Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(body) });
      if (url.endsWith("/mcp-tools")) return json({ servers: {} });
      if (url.endsWith("/tools")) {
        return json({ tools: [], mcp_servers: [], mcp_servers_defs: [], recommended: [] });
      }
      if (url.endsWith("/config")) return json({});
      return Promise.resolve({ ok: false, status: 404, json: () => Promise.resolve({}) });
    }),
  );
}

/** Open the picker, switch to the builtin tab and click one icon. */
async function pickBuiltinIcon() {
  const { container } = render(<AgentSetupTab />);

  // The avatar preview button is the one that wraps the 64px avatar (it is
  // the only `rounded-lg ring-1` button that is not a picker tile).
  const preview = await waitFor(() => {
    const el = container.querySelector<HTMLButtonElement>("button.rounded-lg.ring-1");
    if (!el) throw new Error("avatar preview button not mounted yet");
    return el;
  });
  fireEvent.click(preview);
  fireEvent.click(await screen.findByText("Builtin"));
  fireEvent.click(await screen.findByAltText(PICKED_ICON));
}

beforeEach(() => {
  useAgentStore.setState({ agents: {}, selectedAgentId: null, loading: false, error: null });
  listAgents.mockReset();
  listAgents.mockResolvedValue([]);
  updateAvatarConfig.mockReset();
  updateAvatarConfig.mockResolvedValue({
    agent_id: INSTANCE,
    avatar: null,
    builtin_avatar: PICKED_ICON,
    name: "SSE.Kevin",
    source: "config",
  });
  seedAgent();
  stubFetch();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("AgentSetupTab — a picked avatar lands on every surface immediately", () => {
  it("writes the confirmed identity into the agent list without refetching it", async () => {
    await pickBuiltinIcon();

    // Sidebar + transcript read `agents[instance].meta`; it must already be
    // the picked icon, and the custom slot must be cleared (the two are
    // mutually exclusive upstream — a stale one would resurrect later).
    await waitFor(() => {
      const meta = useAgentStore.getState().agents[INSTANCE].meta;
      expect(meta.builtin_avatar).toBe(PICKED_ICON);
      expect(meta.avatar).toBeUndefined();
    });

    // ...and it got there from the PUT response, not from the list view.
    expect(listAgents).not.toHaveBeenCalled();
    expect(updateAvatarConfig).toHaveBeenCalledTimes(1);
  });

  it("leaves the row untouched when the confirmed identity already matches", async () => {
    // The inventory doorbell's refetch converges on exactly this value; it
    // must not repaint (AGENTS.md: identity-preserving store updates).
    await pickBuiltinIcon();
    await waitFor(() =>
      expect(useAgentStore.getState().agents[INSTANCE].meta.builtin_avatar).toBe(PICKED_ICON),
    );
    const agentsBefore = useAgentStore.getState().agents;
    const rowBefore = agentsBefore[INSTANCE];

    useAgentStore.getState().applyAgentIdentity(INSTANCE, {
      name: "SSE.Kevin",
      avatar: undefined,
      builtin_avatar: PICKED_ICON,
    });

    expect(useAgentStore.getState().agents).toBe(agentsBefore);
    expect(useAgentStore.getState().agents[INSTANCE]).toBe(rowBefore);
  });
});
