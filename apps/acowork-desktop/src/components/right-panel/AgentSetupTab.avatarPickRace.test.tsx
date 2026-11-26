/**
 * The first avatar pick after opening the panel must not be undone by the
 * panel's own mount read.
 *
 * ## The bug this pins
 *
 * `AgentSetupTab` fires `GET /agents/{id}/avatar-config` on mount (and again
 * on every agent switch) and stores the answer in `avatarConfig`, which the
 * header paints and the picker's "selected" ring reads. A pick issues a `PUT`
 * and stores *its* answer the same way. Nothing ordered the two, so a pick
 * made while the read was still in flight was applied and then silently
 * reverted when the older read landed — the reported "click a custom avatar,
 * nothing happens; pick a different one and it works; come back to the first
 * one and now that works too". The read is only in flight for the first pick
 * after a panel open, which is exactly why every later pick behaved.
 *
 * A confirmed write always wins over an older read: the read's answer is
 * stale by construction once a PUT has been answered.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { useAgentStore, type AgentStorage } from "../../stores/agentStore";
import type { AgentInfo, AvatarConfigResponse } from "../../lib/types";
import { AgentSetupTab } from "./AgentSetupTab";
import { clearAuthedImageCache } from "../common/AuthedImage";

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
const fetchAvatarConfig = vi.fn<[], Promise<AvatarConfigResponse>>();

vi.mock("../../lib/avatar", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../../lib/avatar")>()),
  fetchAvatarAssets: vi.fn(async () => ({
    assets: [
      { relative_path: "assets/avatar-01.png" },
      { relative_path: "assets/avatar-02.png" },
    ],
  })),
  fetchAvatarConfig: () => fetchAvatarConfig(),
  updateAvatarConfig: () => updateAvatarConfig(),
  deleteAvatarFile: vi.fn(async () => ({})),
}));

const INSTANCE = "inst-1";
const PICKED = "assets/avatar-01.png";

function seedAgent() {
  const meta = {
    instance_id: INSTANCE,
    agent_id: "com.acowork.senior-engineer",
    name: "SSE.Kevin",
    version: "1.0.0",
    alive: true,
    builtin_avatar: "icon-03",
  } as AgentInfo;
  useAgentStore.setState({
    agents: { [INSTANCE]: { meta, profile: {}, sessions: [] } } as unknown as Record<
      string,
      AgentStorage
    >,
    selectedAgentId: INSTANCE,
  });
}

/** Minimal gateway surface; avatar bytes resolve so the tiles are clickable. */
function stubFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      const json = (body: unknown) =>
        Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve(body) });
      if (url.includes("/avatar-file")) {
        return Promise.resolve({
          ok: true,
          status: 200,
          blob: async () => new Blob(["bytes"]),
        });
      }
      if (url.endsWith("/mcp-tools")) return json({ servers: {} });
      if (url.endsWith("/tools")) {
        return json({ tools: [], mcp_servers: [], mcp_servers_defs: [], recommended: [] });
      }
      if (url.endsWith("/config")) return json({});
      return Promise.resolve({ ok: false, status: 404, json: () => Promise.resolve({}) });
    }),
  );
}

beforeEach(() => {
  URL.createObjectURL = vi.fn(() => `blob:${Math.random()}`);
  URL.revokeObjectURL = vi.fn();
  clearAuthedImageCache();
  useAgentStore.setState({ agents: {}, selectedAgentId: null, loading: false, error: null });
  listAgents.mockReset();
  listAgents.mockResolvedValue([]);
  updateAvatarConfig.mockReset();
  fetchAvatarConfig.mockReset();
  seedAgent();
  stubFetch();
});

afterEach(() => {
  clearAuthedImageCache();
  vi.unstubAllGlobals();
  vi.clearAllMocks();
});

describe("AgentSetupTab — a pick survives the panel's mount read", () => {
  it("keeps the picked custom avatar when the older avatar-config read lands afterwards", async () => {
    // The mount read stays in flight until the test releases it.
    let releaseRead: (cfg: AvatarConfigResponse) => void = () => {};
    fetchAvatarConfig.mockReturnValue(
      new Promise<AvatarConfigResponse>((resolve) => {
        releaseRead = resolve;
      }),
    );
    updateAvatarConfig.mockResolvedValue({
      agent_id: INSTANCE,
      avatar: PICKED,
      name: "SSE.Kevin",
      source: "config",
    });

    const { container } = render(<AgentSetupTab />);
    const preview = await waitFor(() => {
      const el = container.querySelector<HTMLButtonElement>("button.rounded-full.ring-1");
      if (!el) throw new Error("avatar preview button not mounted yet");
      return el;
    });
    fireEvent.click(preview);
    fireEvent.click(await screen.findByAltText(PICKED));

    // The pick landed: the header paints the custom avatar...
    await waitFor(() => expect(container.querySelector(`img[alt="${INSTANCE}"]`)).not.toBeNull());

    // ...and the pre-pick answer arriving now must not undo it.
    releaseRead({
      agent_id: INSTANCE,
      builtin_avatar: "icon-03",
      name: "SSE.Kevin",
      source: "manifest",
    });
    await new Promise((r) => setTimeout(r, 0));

    expect(container.querySelector(`img[alt="${INSTANCE}"]`)).not.toBeNull();
    // Reopening the picker agrees with the header: the picked tile is the
    // one marked selected.
    fireEvent.click(preview);
    const tile = await screen.findByAltText(PICKED);
    expect(tile.closest("div")?.className ?? "").toContain("border-text");
  });
});