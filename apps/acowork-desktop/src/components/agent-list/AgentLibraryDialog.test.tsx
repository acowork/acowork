/**
 * Agent library dialog (PRD docs/prd/zh/prd-agent-hub-install-dialog.md).
 *
 * Pinned behaviours:
 *   - REQ-002 / D5: opens on the Library tab; the Local tab is a second tab.
 *   - REQ-004: one card per catalog entry.
 *   - D2 / F8 / Plan A: the primary button is `ensure` only while the target
 *     node holds no instance of the package; once it does, the click lands
 *     another instance (`ensure: false`). The instance it names is what gets
 *     selected — never the package id (ADR-073).
 *   - REQ-007: a high-risk entry is NOT installed on the first click — the
 *     confirmation has to be accepted first.
 *   - REQ-006 / D2: the card menu is the non-idempotent path (`ensure: false`),
 *     which is how a legal second instance gets created.
 *   - REQ-009: remote mode with nothing online cannot install, and says why.
 *   - REQ-012: a failed catalog read shows a retry, and the Local tab still
 *     works (the Hub being down must not lock the user out of installing).
 *   - REQ-011: a dropped file that is not `.agent` is refused client-side.
 *   - REQ-020: the window-level drop channel is handed back when the dialog
 *     goes away, otherwise the chat would stop receiving attachments.
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
    selectAgent: vi.fn(),
    installAgent: vi.fn(async () => "inst-local" as string | null),
  },
  settings: { gatewayMode: "local" as string },
  dropHandler: null as ((event: { payload: string[] }) => void) | null,
}));

// Key echoes: assertions then pin the hook, not one language's wording.
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
  useSettingsStore: (selector: (state: typeof h.settings) => unknown) => selector(h.settings),
}));

vi.mock("../common/ToastProvider", () => ({
  useToast: () => ({ addToast: h.addToast }),
  showToast: vi.fn(),
}));

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { AgentLibraryDialog } from "./AgentLibraryDialog";
import { resetHubIndexCache } from "../../lib/agentHub";
import { isFileDropClaimed } from "../../lib/fileDropClaim";

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

const RISKY_AGENT = {
  ...SAFE_AGENT,
  agent_id: "com.acowork.risky",
  display_name: "Risky Agent",
  permissions: ["Read", "Shell"],
  high_risk_permissions: ["Shell"],
};

const ONLINE_NODE = {
  node_id: "n1",
  online: true,
  gateway_managed: false,
  capabilities: [],
  node_name: "Workstation",
  can_manage: true,
};

function mockCatalog(agents: unknown[]) {
  h.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    if (command === "fetch_hub_index") {
      return { schema_version: 2, base_url: "https://hub.acowork.ai", agents };
    }
    if (command === "install_from_hub") {
      return { agent_id: args?.agentId, instance_id: "inst-1", already_present: false };
    }
    if (command === "inspect_local_package") {
      return { file_name: "picked.agent", size_bytes: 4096, agent_id: "com.acowork.local" };
    }
    if (command === "install_agent") return { ok: true };
    throw new Error(`unexpected command: ${command}`);
  });
}

function renderOpen(props: { initialNodeId?: string; onClose?: () => void } = {}) {
  const closed: string[] = [];
  render(
    <AgentLibraryDialog
      open
      initialNodeId={props.initialNodeId}
      onClose={props.onClose ?? (() => closed.push("closed"))}
    />,
  );
  return closed;
}

beforeEach(() => {
  cleanup();
  vi.clearAllMocks();
  resetHubIndexCache();
  h.agent.nodes = [];
  h.agent.agents = {};
  h.agent.installAgent = vi.fn(async () => "inst-local" as string | null);
  h.settings.gatewayMode = "local";
  h.dropHandler = null;
  h.listen.mockImplementation(async (_event: string, handler: (e: { payload: string[] }) => void) => {
    h.dropHandler = handler;
    return () => {};
  });
  h.open.mockResolvedValue(null);
  mockCatalog([SAFE_AGENT, RISKY_AGENT]);
});

describe("tabs", () => {
  it("opens on the Library tab and renders one card per entry", async () => {
    renderOpen();
    expect((await screen.findByRole("tab", { name: "agentHub.tabLibrary" })).getAttribute("aria-selected")).toBe(
      "true",
    );
    await screen.findByText("Safe Agent");
    await screen.findByText("Risky Agent");
  });

  it("switches to the Local tab, which offers the drop zone", async () => {
    renderOpen();
    fireEvent.click(await screen.findByRole("tab", { name: "agentHub.tabLocal" }));
    expect(screen.getByText("agentHub.localDropHint")).toBeDefined();
  });
});

// The dialog follows the platform footer-button-bar convention: the Library
// tab needs an explicit dismiss button bottom-right (the Local tab already
// has Cancel). The header `×` shares the `common.close` name, so the footer
// button is pinned by its own class.
describe("footer close button", () => {
  function footerClose(): HTMLElement {
    return screen.getAllByRole("button", { name: "common.close" }).at(-1)!;
  }

  it("sits in the Library tab footer and closes the dialog", async () => {
    const closed = renderOpen();
    await screen.findByText("Safe Agent");

    const button = footerClose();
    expect(button.closest('[class*="border-t"]')).not.toBeNull();
    fireEvent.click(button);
    expect(closed).toContain("closed");
  });

  it("is disabled while an install is in flight", async () => {
    let release: () => void = () => {};
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") {
        return { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [SAFE_AGENT] };
      }
      return new Promise((resolve) => {
        release = () => resolve({ agent_id: "com.acowork.safe", instance_id: "inst-1", already_present: false });
      });
    });
    renderOpen();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));
    await waitFor(() => expect(footerClose().hasAttribute("disabled")).toBe(true));
    release();
  });
});

describe("hub install", () => {
  it("sends the primary button as an idempotent ensure and selects the acked instance", async () => {
    renderOpen();
    const cards = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(cards[0]);

    await waitFor(() => expect(h.invoke).toHaveBeenCalledWith("install_from_hub", expect.anything()));
    const [, args] = h.invoke.mock.calls.find(([command]) => command === "install_from_hub")!;
    expect(args).toMatchObject({ ensure: true, agentId: "com.acowork.safe" });
    await waitFor(() => expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-1"));
    expect(h.addToast).toHaveBeenCalledWith(expect.objectContaining({ type: "success" }));
  });

  it("reports an already-present package as info rather than a new install", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") {
        return { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [SAFE_AGENT] };
      }
      return { agent_id: "com.acowork.safe", instance_id: "inst-existing", already_present: true };
    });
    renderOpen();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));

    await waitFor(() => expect(h.addToast).toHaveBeenCalledWith(expect.objectContaining({ type: "info" })));
    expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-existing");
  });

  it("does not submit a high-risk package until the confirmation is accepted", async () => {
    h.agent.nodes = [ONLINE_NODE];
    renderOpen();
    await screen.findByText("Safe Agent");

    // The risky card is the second one.
    const installButtons = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(installButtons[1]);

    // Confirmation asked for, nothing sent.
    expect(await screen.findByRole("alertdialog")).toBeDefined();
    expect(h.invoke).not.toHaveBeenCalledWith("install_from_hub", expect.anything());

    fireEvent.click(screen.getByRole("button", { name: "agentHub.highRiskConfirm" }));
    await waitFor(() =>
      expect(h.invoke).toHaveBeenCalledWith(
        "install_from_hub",
        expect.objectContaining({ agentId: "com.acowork.risky" }),
      ),
    );
  });

  it("closes the dialog on success", async () => {
    const closed = renderOpen();
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(buttons[0]);
    await waitFor(() => expect(closed).toContain("closed"));
  });

  // REQ-006 / D2: the primary button is idempotent, so the extra-instance
  // entry is the only way to create a second instance. Without coverage here
  // a regression to `ensure: true` would silently remove that capability.
  it("sends the card menu entry as a non-idempotent install", async () => {
    renderOpen();
    const triggers = await screen.findAllByRole("button", { name: "agentHub.moreActions" });
    fireEvent.click(triggers[0]);
    // The menu portals with `visibility: hidden` until a size measurement that
    // never lands under jsdom, and role queries skip hidden subtrees — so the
    // item is taken by its own class rather than by accessible name.
    const item = await waitFor(() => {
      const el = document.querySelector<HTMLButtonElement>(".context-menu-item");
      if (!el) throw new Error("card menu did not render");
      return el;
    });
    fireEvent.click(item);

    await waitFor(() =>
      expect(h.invoke).toHaveBeenCalledWith(
        "install_from_hub",
        expect.objectContaining({ ensure: false }),
      ),
    );
  });

  // REQ-014 serializes installs; it must not paint every card as busy. The
  // in-flight label belongs to the button that was pressed, and the siblings
  // stay readable as "Install" while disabled.
  it("marks only the pressed card as installing while the request is in flight", async () => {
    let release: (value: unknown) => void = () => {};
    const pending = new Promise((resolve) => {
      release = resolve;
    });
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") {
        return { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [SAFE_AGENT, RISKY_AGENT] };
      }
      return await pending;
    });
    renderOpen();
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(buttons[0]);

    const inFlight = await screen.findByRole("button", { name: "agentHub.installing" });
    expect(inFlight.hasAttribute("disabled")).toBe(true);
    const waiting = screen.getAllByRole("button", { name: "agentHub.install" });
    expect(waiting).toHaveLength(1);
    expect(waiting[0].hasAttribute("disabled")).toBe(true);

    release({ agent_id: "com.acowork.safe", instance_id: "inst-1", already_present: false });
    await waitFor(() => expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-1"));
  });

  // REQ-007 across sessions: the confirmation has no focus trap, so the dialog
  // can be closed underneath it. Reopening must not resurrect a pending
  // confirmation that would then install against the NEW target node.
  it("does not carry a pending confirmation into the next opening", async () => {
    h.agent.nodes = [ONLINE_NODE];
    const { rerender } = render(<AgentLibraryDialog open initialNodeId="n1" onClose={() => {}} />);
    await screen.findByText("Safe Agent");
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(buttons[1]);
    expect(await screen.findByRole("alertdialog")).toBeDefined();

    rerender(<AgentLibraryDialog open={false} initialNodeId="n1" onClose={() => {}} />);
    rerender(<AgentLibraryDialog open initialNodeId="n1" onClose={() => {}} />);

    await screen.findByText("Safe Agent");
    expect(screen.queryByRole("alertdialog")).toBeNull();
  });
});

// Plan A routing: an already-installed package must not dead-end the primary
// button. The moment the target node holds one instance, the click lands
// another (ADR-073 multi-instance is legal) — and the button says so.
describe("multi-instance routing", () => {
  function installedEntry(agentId: string, instanceId: string, nodeId: string) {
    return { [instanceId]: { meta: { agent_id: agentId, node_id: nodeId } } };
  }

  it("sends ensure:false and relabels the button when the node already holds an instance", async () => {
    h.agent.nodes = [ONLINE_NODE];
    h.agent.agents = installedEntry("com.acowork.safe", "inst-old", "n1");
    renderOpen({ initialNodeId: "n1" });

    const again = await screen.findByRole("button", { name: "agentHub.installAnother" });
    fireEvent.click(again);

    await waitFor(() => expect(h.invoke).toHaveBeenCalledWith("install_from_hub", expect.anything()));
    const [, args] = h.invoke.mock.calls.find(([command]) => command === "install_from_hub")!;
    expect(args).toMatchObject({ agentId: "com.acowork.safe", ensure: false });
    // The other package has no instance on n1 — its button stays the plain
    // idempotent install.
    expect(screen.getAllByRole("button", { name: "agentHub.install" }).length).toBe(1);
  });

  it("counts instances against the \"local\" anchor when no node is selected", async () => {
    // Local mode with nothing enrolled: targetNodeId stays null and the
    // Gateway inventory anchors own-machine instances on "local" (ADR-075 D6).
    h.agent.agents = installedEntry("com.acowork.safe", "inst-old", "local");
    renderOpen();

    fireEvent.click(await screen.findByRole("button", { name: "agentHub.installAnother" }));
    await waitFor(() =>
      expect(h.invoke).toHaveBeenCalledWith("install_from_hub", expect.objectContaining({ ensure: false })),
    );
  });
});

describe("install target (REQ-008 / REQ-009)", () => {
  it("blocks every install and says why when a remote Gateway has no online node", async () => {
    h.settings.gatewayMode = "remote";
    renderOpen();
    expect(await screen.findByText("agentHub.noOnlineNode")).toBeDefined();
    for (const button of await screen.findAllByRole("button", { name: "agentHub.install" })) {
      expect(button.hasAttribute("disabled")).toBe(true);
    }
    fireEvent.click(screen.getAllByRole("button", { name: "agentHub.install" })[0]);
    expect(h.invoke).not.toHaveBeenCalledWith("install_from_hub", expect.anything());
  });

  it("blocks installs to a node the caller cannot manage", async () => {
    h.settings.gatewayMode = "remote";
    h.agent.nodes = [{ ...ONLINE_NODE, can_manage: false }];
    renderOpen();
    expect(await screen.findByText("agentHub.nodeNotManageable")).toBeDefined();
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    for (const button of buttons) expect(button.hasAttribute("disabled")).toBe(true);
  });

  it("pre-selects the node the entry point came from when several are online", async () => {
    h.settings.gatewayMode = "remote";
    h.agent.nodes = [ONLINE_NODE, { ...ONLINE_NODE, node_id: "n2", node_name: "Server" }];
    renderOpen({ initialNodeId: "n2" });
    const select = await screen.findByRole("combobox", { name: "agentHub.targetNode" });
    expect((select as HTMLSelectElement).value).toBe("n2");
  });
});

describe("catalog failure (REQ-012)", () => {
  it("shows a retry instead of an empty library", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("Hub answered text/html");
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();
    expect(await screen.findByText(/Hub answered text\/html/)).toBeDefined();
    expect(screen.getByRole("button", { name: "agentHub.retry" })).toBeDefined();
  });

  it("keeps the Local tab usable while the Hub is unreachable", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("offline");
      if (command === "inspect_local_package") {
        return { file_name: "picked.agent", size_bytes: 4096, agent_id: "com.acowork.local" };
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();
    fireEvent.click((await screen.findByRole("tab", { name: "agentHub.tabLocal" })));
    fireEvent.click(screen.getByText("agentHub.localDropHint"));

    await waitFor(() => expect(h.open).toHaveBeenCalled());
    h.open.mockResolvedValue("C:\\pkgs\\picked.agent");
    fireEvent.click(screen.getByText("agentHub.localDropHint"));
    await waitFor(() => expect(screen.getByText("picked.agent")).toBeDefined());
    expect(screen.getByRole("button", { name: "agentHub.install" }).hasAttribute("disabled")).toBe(false);
  });
});

describe("local file tab (REQ-011 / REQ-020)", () => {
  async function openLocalTab() {
    renderOpen();
    fireEvent.click(await screen.findByRole("tab", { name: "agentHub.tabLocal" }));
    // The dialog claims the window-level drop channel while mounted.
    await waitFor(() => expect(h.dropHandler).not.toBeNull());
  }

  it("refuses a dropped file that is not an .agent package", async () => {
    await openLocalTab();
    h.dropHandler!({ payload: ["C:\\notes\\todo.txt"] });
    expect(await screen.findByText("agentHub.notAgentFile")).toBeDefined();
    expect(h.invoke).not.toHaveBeenCalledWith("inspect_local_package", expect.anything());
  });

  it("accepts a dropped .agent and installs it through the local path", async () => {
    await openLocalTab();
    h.dropHandler!({ payload: ["C:\\pkgs\\picked.agent"] });

    expect(await screen.findByText("picked.agent")).toBeDefined();
    fireEvent.click(screen.getByRole("button", { name: "agentHub.install" }));
    await waitFor(() => expect(h.agent.installAgent).toHaveBeenCalled());
  });

  // F8 / ADR-073: the local path returns the acked INSTANCE key. Selecting the
  // package id instead would highlight an older instance of the same package.
  it("selects the instance the install acked, not the package id", async () => {
    await openLocalTab();
    h.dropHandler!({ payload: ["C:\\pkgs\\picked.agent"] });

    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));
    await waitFor(() => expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-local"));
    expect(h.agent.selectAgent).not.toHaveBeenCalledWith("com.acowork.local");
  });

  // REQ-020: the claim is the only thing keeping a dropped package out of the
  // chat attachment list — leaking it would disable attachments for the whole
  // session.
  it("releases the window-level drop claim when it closes", async () => {
    await openLocalTab();
    expect(isFileDropClaimed()).toBe(true);

    cleanup();
    await waitFor(() => expect(isFileDropClaimed()).toBe(false));
  });
});
