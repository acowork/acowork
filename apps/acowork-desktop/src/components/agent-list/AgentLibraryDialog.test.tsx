/**
 * Agent library dialog (PRD docs/prd/zh/prd-agent-hub-install-dialog.md).
 *
 * Pinned behaviours:
 *   - REQ-021 / D9: opens on the LOCAL tab (the machine's file library) and
 *     renders it from a single local command; the Hub catalog is warmed in the
 *     background and never awaited (REQ-022).
 *   - REQ-023..030: one row per `.agent` file, fields from the manifest, a
 *     broken file only breaks its own row, two versions stay two rows.
 *   - REQ-025 / A5: the trailing "+ add agent file" row keeps the v0.1
 *     pick-and-install path and does NOT copy the file into the library.
 *   - REQ-026 / REQ-027: the row primary button is `ensure`; the row menu is
 *     the non-idempotent `/install`.
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
import { pickDeterministicBuiltinIconId } from "../../lib/avatar";
import { BUILTIN_ICONS } from "../../lib/builtinIcons";

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

/** A row as `list_local_agent_packages` returns it (manifest-derived). */
const LOCAL_AVATAR = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUg==";

const LOCAL_ROW = {
  file_name: "com.acowork.demo-1.0.0.agent",
  path: "C:\lib\com.acowork.demo-1.0.0.agent",
  agent_id: "com.acowork.demo",
  name: "Demo",
  version: "1.0.0",
  description: "A demo agent",
  size_bytes: 4096,
  role: "Demo Manager",
  labels: ["demo", "testing"],
  capabilities: ["demo_run"],
  skills: ["demo-skill", "demo-extra"],
  avatar: { kind: "packaged", data_url: LOCAL_AVATAR },
  permissions: ["network", "filesystem:read"],
  error: null,
};

const LOCAL_ROW_V2 = { ...LOCAL_ROW, file_name: "com.acowork.demo-1.1.0.agent", path: "C:\lib\com.acowork.demo-1.1.0.agent", version: "1.1.0" };

const LOCAL_ROW_BROKEN = {
  file_name: "broken.agent",
  path: "C:\lib\broken.agent",
  agent_id: null,
  name: null,
  version: null,
  description: null,
  size_bytes: 9,
  role: null,
  labels: [],
  capabilities: [],
  skills: [],
  avatar: null,
  permissions: [],
  error: "Not a valid .agent package (ZIP): invalid Zip archive",
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
    if (command === "list_local_agent_packages") return { rows: [], warning: null };
    if (command === "ensure_local_agent") {
      return { agent_id: "com.acowork.demo", instance_id: "inst-ensured", already_present: false };
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

/**
 * Open the dialog and switch to the Hub catalog tab. Only the Hub tests need
 * this: the dialog opens on Local (REQ-021), so every assertion about cards has
 * to get there first.
 */
async function renderLibraryTab(props: { initialNodeId?: string; onClose?: () => void } = {}) {
  const closed = renderOpen(props);
  fireEvent.click(await screen.findByRole("tab", { name: "agentHub.tabLibrary" }));
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
  // REQ-021: the default tab must not depend on a network round-trip.
  it("opens on the Local tab and warms the catalog in the background", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "list_local_agent_packages") return { rows: [LOCAL_ROW], warning: null };
      if (command === "fetch_hub_index") {
        return { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [SAFE_AGENT] };
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();

    expect((await screen.findByRole("tab", { name: "agentHub.tabLocal" })).getAttribute("aria-selected")).toBe(
      "true",
    );
    // The list is rendered from the local command alone…
    expect(await screen.findByText("Demo")).toBeDefined();
    // …and the Hub warm-up was fired anyway (REQ-022), without blocking it.
    await waitFor(() => expect(h.invoke).toHaveBeenCalledWith("fetch_hub_index"));
  });

  it("switches to the Library tab and renders one card per entry", async () => {
    await renderLibraryTab();
    fireEvent.click(await screen.findByRole("tab", { name: "agentHub.tabLibrary" }));
    await screen.findByText("Safe Agent");
    await screen.findByText("Risky Agent");
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
    const closed = await renderLibraryTab();
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
    await renderLibraryTab();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));
    await waitFor(() => expect(footerClose().hasAttribute("disabled")).toBe(true));
    release();
  });
});

describe("hub install", () => {
  it("sends the primary button as an idempotent ensure and selects the acked instance", async () => {
    await renderLibraryTab();
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
    await renderLibraryTab();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));

    await waitFor(() => expect(h.addToast).toHaveBeenCalledWith(expect.objectContaining({ type: "info" })));
    expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-existing");
  });

  it("does not submit a high-risk package until the confirmation is accepted", async () => {
    h.agent.nodes = [ONLINE_NODE];
    await renderLibraryTab();
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
    const closed = await renderLibraryTab();
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(buttons[0]);
    await waitFor(() => expect(closed).toContain("closed"));
  });

  // REQ-006 / D2: the primary button is idempotent, so the extra-instance
  // entry is the only way to create a second instance. Without coverage here
  // a regression to `ensure: true` would silently remove that capability.
  it("sends the card menu entry as a non-idempotent install", async () => {
    await renderLibraryTab();
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
    await renderLibraryTab();
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
    // The dialog opens on Local (REQ-021); the cards live behind the other tab.
    fireEvent.click(await screen.findByRole("tab", { name: "agentHub.tabLibrary" }));
    await screen.findByText("Safe Agent");
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    fireEvent.click(buttons[1]);
    expect(await screen.findByRole("alertdialog")).toBeDefined();

    rerender(<AgentLibraryDialog open={false} initialNodeId="n1" onClose={() => {}} />);
    rerender(<AgentLibraryDialog open initialNodeId="n1" onClose={() => {}} />);

    fireEvent.click(await screen.findByRole("tab", { name: "agentHub.tabLibrary" }));
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
    await renderLibraryTab({ initialNodeId: "n1" });

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
    await renderLibraryTab();

    fireEvent.click(await screen.findByRole("button", { name: "agentHub.installAnother" }));
    await waitFor(() =>
      expect(h.invoke).toHaveBeenCalledWith("install_from_hub", expect.objectContaining({ ensure: false })),
    );
  });
});

describe("install target (REQ-008 / REQ-009)", () => {
  it("blocks every install and says why when a remote Gateway has no online node", async () => {
    h.settings.gatewayMode = "remote";
    await renderLibraryTab();
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
    await renderLibraryTab();
    expect(await screen.findByText("agentHub.nodeNotManageable")).toBeDefined();
    const buttons = await screen.findAllByRole("button", { name: "agentHub.install" });
    for (const button of buttons) expect(button.hasAttribute("disabled")).toBe(true);
  });

  it("pre-selects the node the entry point came from when several are online", async () => {
    h.settings.gatewayMode = "remote";
    h.agent.nodes = [ONLINE_NODE, { ...ONLINE_NODE, node_id: "n2", node_name: "Server" }];
    await renderLibraryTab({ initialNodeId: "n2" });
    const select = await screen.findByRole("combobox", { name: "agentHub.targetNode" });
    expect((select as HTMLSelectElement).value).toBe("n2");
  });
});

describe("catalog failure (REQ-012)", () => {
  it("shows a retry instead of an empty library", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("Hub answered text/html");
      if (command === "list_local_agent_packages") return { rows: [], warning: null };
      throw new Error(`unexpected command: ${command}`);
    });
    await renderLibraryTab();
    expect(await screen.findByText(/Hub answered text\/html/)).toBeDefined();
    expect(screen.getByRole("button", { name: "agentHub.retry" })).toBeDefined();
  });

  // REQ-022 / A2: a dead Hub must not touch the Local tab — it is a directory
  // read, and the warm-up failure is not surfaced as a dialog toast.
  it("keeps the Local tab usable while the Hub is unreachable", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("offline");
      if (command === "list_local_agent_packages") return { rows: [LOCAL_ROW], warning: null };
      if (command === "inspect_local_package") {
        return { file_name: "picked.agent", size_bytes: 4096, agent_id: "com.acowork.local" };
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();
    await screen.findByText("Demo");
    expect(h.addToast).not.toHaveBeenCalled();

    fireEvent.click(screen.getByText("agentHub.localAddFile"));
    await waitFor(() => expect(h.open).toHaveBeenCalled());
    h.open.mockResolvedValue("C:\pkgs\picked.agent");
    fireEvent.click(screen.getByText("agentHub.localAddFile"));
    await waitFor(() => expect(screen.getByText("picked.agent")).toBeDefined());
    // The pending card is the last install button in the body; the row above it
    // belongs to the library file.
    const installs = screen.getAllByRole("button", { name: "agentHub.install" });
    expect(installs.at(-1)!.hasAttribute("disabled")).toBe(false);
  });
});

describe("local agent library (REQ-021..032)", () => {
  function mockLibrary(rows: unknown[], warning: string | null = null) {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("offline");
      if (command === "list_local_agent_packages") return { rows, warning };
      if (command === "inspect_local_package") {
        return { file_name: "picked.agent", size_bytes: 4096, agent_id: "com.acowork.local" };
      }
      if (command === "ensure_local_agent") {
        return { agent_id: "com.acowork.demo", instance_id: "inst-ensured", already_present: false };
      }
      if (command === "install_agent") return { ok: true };
      throw new Error(`unexpected command: ${command}`);
    });
  }

  // F13: N files, ONE invoke.
  it("enumerates the library with a single command call", async () => {
    mockLibrary([LOCAL_ROW, LOCAL_ROW_V2]);
    renderOpen();
    await screen.findAllByText("Demo");

    const calls = h.invoke.mock.calls.filter(([command]) => command === "list_local_agent_packages");
    expect(calls).toHaveLength(1);
    expect(h.invoke).not.toHaveBeenCalledWith("inspect_local_package", expect.anything());
  });

  // REQ-024: the row shows the manifest's own identity, not the filename.
  it("renders name, version, agent id, size and permission count per row", async () => {
    mockLibrary([LOCAL_ROW]);
    renderOpen();

    expect(await screen.findByText("Demo")).toBeDefined();
    expect(screen.getByText("v1.0.0")).toBeDefined();
    expect(screen.getByText("com.acowork.demo")).toBeDefined();
    expect(screen.getByText("com.acowork.demo-1.0.0.agent")).toBeDefined();
    expect(screen.getByText("4.0 KB")).toBeDefined();
    expect(screen.getByText("agentHub.localPermissionCount")).toBeDefined();
  });

  // REQ-030 / F15 / A9: two versions of one package are two rows.
  it("keeps two versions of the same agent as separate rows", async () => {
    mockLibrary([LOCAL_ROW, LOCAL_ROW_V2]);
    renderOpen();
    await screen.findByText("v1.0.0");
    expect(screen.getByText("v1.1.0")).toBeDefined();
    expect(screen.getAllByRole("button", { name: "agentHub.install" })).toHaveLength(2);
  });

  // REQ-026 / D11: the primary button is the idempotent ensure, and the
  // instance it selects is the one the Gateway acked (ADR-073).
  it("installs a row through ensure and selects the acked instance", async () => {
    mockLibrary([LOCAL_ROW]);
    const closed = renderOpen();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));

    await waitFor(() =>
      expect(h.invoke).toHaveBeenCalledWith("ensure_local_agent", {
        packagePath: LOCAL_ROW.path,
        nodeId: null,
      }),
    );
    // Never the non-idempotent path.
    expect(h.invoke).not.toHaveBeenCalledWith("install_agent", expect.anything());
    await waitFor(() => expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-ensured"));
    expect(h.addToast).toHaveBeenCalledWith(expect.objectContaining({ type: "success" }));
    expect(closed).toContain("closed");
  });

  // The two tabs pack identically: one agent per cell, two cells per row once
  // the dialog is wide. Pinned as an equality of the container class, because
  // the point is that the two lists cannot drift apart — re-tuning one grid
  // must re-tune the other.
  it("packs the rows in the same grid as the Hub tab", async () => {
    // Both commands answer from the start: the Hub index is warmed on open
    // (REQ-022), so a mock installed later would arrive after its fetch.
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") {
        return { schema_version: 2, base_url: "https://hub.acowork.ai", agents: [SAFE_AGENT] };
      }
      if (command === "list_local_agent_packages") {
        return { rows: [LOCAL_ROW, LOCAL_ROW_V2], warning: null };
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();

    const localCards = await screen.findAllByRole("article");
    expect(localCards).toHaveLength(2);
    const localGrid = localCards[0].parentElement as HTMLElement;
    expect(localGrid.className).toContain("grid-cols-1");
    expect(localGrid.className).toContain("lg:grid-cols-2");

    // Same dialog, other tab: the catalog's cards must sit in the same grid.
    fireEvent.click(screen.getByRole("tab", { name: "agentHub.tabLibrary" }));
    const hubCards = await screen.findAllByRole("article");
    expect((hubCards[0].parentElement as HTMLElement).className).toBe(localGrid.className);
  });

  // REQ-025 amended: the add entry is the first cell of the grid. Pinned by
  // position, because the whole point is that it does not drift to the end of a
  // list that keeps growing.
  // Same surface as an agent cell: it sits in the same grid, so a tile with no
  // fill reads as a gap in the list rather than an entry.
  it("gives the add entry the same card surface as an agent", async () => {
    mockLibrary([LOCAL_ROW]);
    renderOpen();

    const addEntry = await screen.findByRole("button", { name: /agentHub.localAddFile/ });
    const agentCard = (await screen.findAllByRole("article"))[0];
    for (const cls of ["bg-panel-block", "rounded-md", "border"]) {
      expect(addEntry.className).toContain(cls);
      expect(agentCard.className).toContain(cls);
    }
  });

  it("puts the add entry in the first cell, ahead of every agent", async () => {
    mockLibrary([LOCAL_ROW, LOCAL_ROW_V2]);
    renderOpen();

    const firstCard = (await screen.findAllByRole("article"))[0];
    const grid = firstCard.parentElement as HTMLElement;
    const addEntry = screen.getByRole("button", { name: /agentHub.localAddFile/ });
    expect(addEntry.parentElement).toBe(grid);
    expect(grid.firstElementChild).toBe(addEntry);
  });

  // REQ-025 on an empty library: the picked file still has a card, inside the
  // same grid — the pending row must not depend on there being library rows.
  it("shows the pending picked file even when the library is empty", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("offline");
      if (command === "list_local_agent_packages") return { rows: [], warning: null };
      if (command === "inspect_local_package") {
        return { file_name: "picked.agent", size_bytes: 4096, agent_id: "com.acowork.local" };
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();

    await screen.findByText("agentHub.localEmpty");
    h.open.mockResolvedValue("C:\pkgs\picked.agent");
    fireEvent.click(await screen.findByRole("button", { name: /agentHub.localAddFile/ }));

    expect(await screen.findByText("picked.agent")).toBeDefined();
    const pendingCard = screen.getByText("picked.agent").closest("div.grid");
    expect(pendingCard?.className).toContain("lg:grid-cols-2");
  });

  // The Local card is the Hub card's skeleton — avatar, name + role, chip row —
  // with the same chip sources read out of the archive, so the two tabs read as
  // one list in two sources rather than two designs.
  it("renders the role, the packed avatar and the chip row like a Hub card", async () => {
    mockLibrary([LOCAL_ROW]);
    renderOpen();

    expect(await screen.findByText("Demo Manager")).toBeDefined();
    // labels → capability keys → skill names, capped at four.
    expect(screen.getByText("demo")).toBeDefined();
    expect(screen.getByText("testing")).toBeDefined();
    expect(screen.getByText("demo_run")).toBeDefined();
    expect(screen.getByText("demo-skill")).toBeDefined();
    // Capped: the fifth source never renders.
    expect(screen.queryByText("demo-extra")).toBeNull();

    const avatar = screen.getByRole("img", { name: "com.acowork.demo" }) as HTMLImageElement;
    expect(avatar.getAttribute("src")).toBe(LOCAL_AVATAR);
  });

  // Regression: a published package that keeps its builtin icon declares
  // `builtin_avatar = "icon-15"` and ships no `assets/`. The row must draw that
  // icon — it used to arrive as "no avatar" and render a letter block.
  it("draws a builtin avatar instead of a letter block", async () => {
    mockLibrary([{ ...LOCAL_ROW, avatar: { kind: "builtin", icon_id: "icon-15" } }]);
    renderOpen();

    await screen.findByText("Demo");
    const avatar = screen.getByRole("img", { name: "icon-15" }) as HTMLImageElement;
    expect(avatar.getAttribute("src")).toBe(BUILTIN_ICONS["icon-15"]);
    // The letter block is not used for an identified row.
    expect(screen.queryByLabelText("Demo")).toBeNull();
  });

  // No avatar at all → ADR-017's last step, the deterministic icon keyed by
  // `agent_id` — the same picture the Hub card shows for that agent.
  it("falls back to the deterministic icon when the package has no avatar", async () => {
    mockLibrary([{ ...LOCAL_ROW, avatar: null }]);
    renderOpen();

    await screen.findByText("Demo");
    const avatar = screen.getByRole("img") as HTMLImageElement;
    expect(avatar.getAttribute("src")).toBe(
      BUILTIN_ICONS[pickDeterministicBuiltinIconId("com.acowork.demo") ?? "icon-01"],
    );
  });

  // An unreadable file has no `agent_id`, so there is no identity to key an icon
  // by — the letter block is the honest placeholder.
  it("uses the letter block only when the file could not be parsed", async () => {
    mockLibrary([LOCAL_ROW_BROKEN]);
    renderOpen();

    expect(await screen.findByLabelText("broken.agent")).toBeDefined();
    expect(screen.getByLabelText("broken.agent").textContent).toBe("B");
  });

  // A6: the row button is disabled while its own install is in flight, so a
  // double click cannot queue a second ensure. Two independent guards apply —
  // the disabled attribute and the serialized install lock — which is why the
  // assertion is on the command count, not on the DOM.
  it("ignores a second click on the same row while the install is in flight", async () => {
    let release: () => void = () => {};
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("offline");
      if (command === "list_local_agent_packages") return { rows: [LOCAL_ROW], warning: null };
      if (command === "ensure_local_agent") {
        return new Promise((resolve) => {
          release = () =>
            resolve({ agent_id: "com.acowork.demo", instance_id: "inst-ensured", already_present: false });
        });
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();

    const install = await screen.findByRole("button", { name: "agentHub.install" });
    fireEvent.click(install);
    await waitFor(() => expect(install.hasAttribute("disabled")).toBe(true));
    fireEvent.click(install);

    release();
    await waitFor(() => expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-ensured"));
    const ensures = h.invoke.mock.calls.filter(([command]) => command === "ensure_local_agent");
    expect(ensures).toHaveLength(1);
  });

  it("reports an already-present package as info instead of a new install", async () => {
    h.invoke.mockImplementation(async (command: string) => {
      if (command === "fetch_hub_index") throw new Error("offline");
      if (command === "list_local_agent_packages") return { rows: [LOCAL_ROW], warning: null };
      if (command === "ensure_local_agent") {
        return { agent_id: "com.acowork.demo", instance_id: "inst-existing", already_present: true };
      }
      throw new Error(`unexpected command: ${command}`);
    });
    renderOpen();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.install" }));

    await waitFor(() => expect(h.addToast).toHaveBeenCalledWith(expect.objectContaining({ type: "info" })));
    expect(h.agent.selectAgent).toHaveBeenCalledWith("inst-existing");
  });

  // REQ-027: the row menu is the non-idempotent path — that is where a second
  // instance is allowed to come from.
  it("installs another instance from the row menu through /install", async () => {
    mockLibrary([LOCAL_ROW]);
    renderOpen();
    fireEvent.click(await screen.findByRole("button", { name: "agentHub.moreActions" }));

    const item = await waitFor(() => {
      const el = document.querySelector<HTMLButtonElement>(".context-menu-item");
      if (!el) throw new Error("row menu did not render");
      return el;
    });
    fireEvent.click(item);

    await waitFor(() => expect(h.agent.installAgent).toHaveBeenCalledWith(LOCAL_ROW.path, undefined));
    expect(h.invoke).not.toHaveBeenCalledWith("ensure_local_agent", expect.anything());
  });

  // A8 / PRD §14.4: one unreadable file marks its own row; the others stay
  // usable and the reason is shown verbatim.
  it("marks a broken file and keeps the other rows installable", async () => {
    mockLibrary([LOCAL_ROW_BROKEN, LOCAL_ROW]);
    renderOpen();

    expect(await screen.findByText("agentHub.localBrokenFile")).toBeDefined();
    const installs = screen.getAllByRole("button", { name: "agentHub.install" });
    // The broken row keeps its button (disabled, titled with the reason); the
    // readable row is still installable.
    expect(installs.filter((b) => !b.hasAttribute("disabled"))).toHaveLength(1);
    // The broken row has no identity to show, but it is listed: the user put
    // that file there and has to be able to see it.
    expect(screen.getByText("broken.agent")).toBeDefined();
  });

  // REQ-028 / A4: an empty library explains itself and still offers the entry.
  it("shows the empty state with the raw reason and the add entry", async () => {
    mockLibrary([], "C:\lib does not exist: 系统找不到指定的文件。 (os error 2)");
    renderOpen();

    expect(await screen.findByText("agentHub.localEmpty")).toBeDefined();
    expect(screen.getByText(/os error 2/)).toBeDefined();
    const add = screen.getByText("agentHub.localAddFile").closest("button")!;
    expect(add.hasAttribute("disabled")).toBe(false);
  });

  // REQ-032: one enumeration per open, no watcher, no polling.
  it("re-enumerates when the dialog is reopened", async () => {
    mockLibrary([LOCAL_ROW]);
    const { rerender } = render(<AgentLibraryDialog open onClose={() => {}} />);
    await screen.findByText("Demo");

    rerender(<AgentLibraryDialog open={false} onClose={() => {}} />);
    rerender(<AgentLibraryDialog open onClose={() => {}} />);

    await waitFor(() =>
      expect(
        h.invoke.mock.calls.filter(([command]) => command === "list_local_agent_packages"),
      ).toHaveLength(2),
    );
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
