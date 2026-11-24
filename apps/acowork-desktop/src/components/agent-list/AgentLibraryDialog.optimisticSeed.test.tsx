/**
 * Agent 库 tab: persistent-seed first paint + revalidation strip.
 *
 * Before this change the first dialog open after a restart always paid a
 * skeleton frame (the session cache is empty, the Hub call is in flight).
 * The catalog is now seeded from `acowork.cache.hubIndex` on mount, the
 * first open revalidates once against the Hub, and while that call is in
 * flight a thin spinner strip (role=status) sits above the cards so the
 * refresh is visible instead of looking frozen.
 *
 * Pinned:
 *   1. with a disk seed and the Hub STILL unanswered, the cards are on
 *      screen synchronously — no skeleton;
 *   2. the same frame shows the sync strip;
 *   3. when the Hub answers with identical data the strip goes away and
 *      the cards stay (no error, no empty flash);
 *   4. a cold machine (no seed) keeps the skeleton behavior of REQ-012.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
  open: vi.fn(),
  addToast: vi.fn(),
  agent: {
    nodes: [] as unknown[],
    agents: {} as Record<string, unknown>,
    fetchAgents: vi.fn(async () => {}),
    fetchNodes: vi.fn(async () => {}),
    selectAgent: vi.fn(),
    installAgent: vi.fn(async () => "inst-local" as string | null),
  },
}));

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: h.invoke }));
vi.mock("@tauri-apps/api/event", () => ({ listen: h.listen }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: h.open }));
vi.mock("../../stores/agentStore", () => {
  const store = (selector: (state: typeof h.agent) => unknown) => selector(h.agent) as unknown;
  (store as { getState?: unknown }).getState = () => h.agent;
  return { useAgentStore: store };
});
vi.mock("../../stores/settingsStore", () => ({
  useSettingsStore: (selector: (state: { gatewayMode: string }) => unknown) =>
    selector({ gatewayMode: "remote" }),
}));
vi.mock("../common/ToastProvider", () => ({
  useToast: () => ({ addToast: h.addToast }),
  showToast: vi.fn(),
}));
vi.mock("../../lib/pairing", () => ({
  resolvePairingBaseUrl: vi.fn(async () => {
    throw new Error("no-lan-in-test");
  }),
}));

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AgentLibraryDialog } from "./AgentLibraryDialog";
import { resetHubIndexCache } from "../../lib/agentHub";
import { writeCatalog } from "../../lib/catalogCache";

const SAFE_AGENT = {
  agent_id: "com.acowork.safe",
  version: "1.0.0",
  name: "safe-agent",
  display_name: "Safe Agent",
  role: "Assistant",
  description: "Nothing risky",
  labels: ["ops"],
  capabilities: [],
  skills: [],
  avatar: null,
  permissions: ["Read"],
  high_risk_permissions: [],
  package: { file: "downloads/com.acowork.safe.agent", size_bytes: 2048, sha256: "aa" },
  signature: { status: "official-channel" },
  deprecated: null,
};

const ONLINE_NODE = {
  node_id: "n1",
  online: true,
  capabilities: [],
  node_name: "Workstation",
  can_manage: true,
};

const INDEX = { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [SAFE_AGENT] };

/** The Local tab's one command; the Hub command is controlled per test. */
function localOnly(hub: Promise<unknown> | (() => Promise<unknown>)) {
  h.invoke.mockImplementation((command: string) => {
    if (command === "list_local_agent_packages") {
      return Promise.resolve({ rows: [], warning: null });
    }
    if (command === "fetch_hub_index") {
      return typeof hub === "function" ? hub() : hub;
    }
    return Promise.reject(new Error(`unexpected command: ${command}`));
  });
}

function openLibraryTab() {
  render(<AgentLibraryDialog open onClose={() => {}} />);
  fireEvent.click(screen.getByRole("tab", { name: "agentHub.tabLibrary" }));
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  localStorage.clear();
  resetHubIndexCache();
  h.agent.nodes = [ONLINE_NODE];
  h.agent.agents = {};
  h.listen.mockImplementation(async (_e: string, handler: (p: { payload: string[] }) => void) => () => {});
  h.open.mockResolvedValue(null);
});

describe("Agent 库 optimistic first paint", () => {
  it("paints the catalog from the disk seed while the Hub is still in flight", () => {
    writeCatalog("hubIndex", INDEX);
    localOnly(new Promise(() => {})); // never answers
    openLibraryTab();

    // Synchronous — no findBy/waitFor: frame one must already carry the card.
    expect(screen.getByText("Safe Agent")).toBeTruthy();
    // The skeleton must NOT be what the user sees on a seeded machine.
    expect(document.querySelector('[aria-busy="true"]')).toBeNull();
    // …and the in-flight refresh is visible as the strip.
    expect(screen.getByRole("status").textContent).toContain("common.loading");
  });

  it("defers the seeded revalidation to the tab visit, not the dialog open", async () => {
    // The strip is only ever visible while the user is ON the tab, so the
    // revalidation must fire when the tab becomes visible — firing it on
    // open (as it first did) meant the answer landed before the switch and
    // the strip was invisible by construction.
    writeCatalog("hubIndex", INDEX);
    localOnly(new Promise(() => {}));
    render(<AgentLibraryDialog open onClose={() => {}} />);
    expect(h.invoke).not.toHaveBeenCalledWith("fetch_hub_index");
    fireEvent.click(screen.getByRole("tab", { name: "agentHub.tabLibrary" }));
    await waitFor(() =>
      expect(h.invoke).toHaveBeenCalledWith("fetch_hub_index"),
    );
    expect(screen.getByRole("status").textContent).toContain("common.loading");
  });

  it("drops the strip when the Hub answers with the same catalog", async () => {
    writeCatalog("hubIndex", INDEX);
    let resolveHub: (v: unknown) => void = () => {};
    localOnly(
      new Promise((resolve) => {
        resolveHub = resolve;
      }),
    );
    openLibraryTab();
    expect(screen.getByRole("status")).toBeTruthy();

    resolveHub(INDEX);
    await waitFor(() => expect(screen.queryByRole("status")).toBeNull());
    // Identical payload: the card is still the one the seed painted.
    expect(screen.getByText("Safe Agent")).toBeTruthy();
  });

  it("cold machine keeps the skeleton (no seed, Hub in flight)", () => {
    localOnly(new Promise(() => {}));
    openLibraryTab();
    expect(screen.queryByText("Safe Agent")).toBeNull();
    // The cold state is the pulsing SkeletonGrid — not the seeded cards, and
    // not the revalidation strip (nothing is on screen to revalidate).
    expect(document.querySelector('[class*="animate-pulse"]')).toBeTruthy();
    expect(screen.queryByRole("status")).toBeNull();
  });
});
