/**
 * Publish wizard — REQ-029 / D12 library write-back (plan §3 T2).
 *
 * Pinned behaviours:
 *   - REQ-029: after the package is **signed and built**, the wizard asks the
 *     backend to copy that exact artifact (`build_publish`'s `output_path`) into
 *     the local library, and reports success with the file name.
 *   - D12 / B5: when the artifact lives on another machine the backend answers
 *     `remote_skipped`; the wizard names the owning node and offers **no**
 *     retry — nothing failed, and remote transfer is a non-goal (PRD §14.6).
 *   - A failed copy is retryable and never blocks the wizard: Close stays
 *     enabled, because the package itself is already finished on the node.
 *   - Nothing is copied before the sign step (the unsigned build artifact is not
 *     what the user distributes).
 *   - The avatar the picker shows is the avatar the artifact gets: the manifest
 *     is written before **every** build, not only after the user moves the
 *     picker (`get_agent_detail` seeds from the instance's effective avatar,
 *     which outranks the package manifest's default).
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  invoke: vi.fn(),
  open: vi.fn(),
  nodes: [
    {
      node_id: "node-1",
      node_name: "workstation",
      online: true,
      capabilities: [],
    },
  ] as unknown[],
}));

// Echo the key plus its variables so assertions pin the contract (which values
// the component passes) instead of one language's wording.
vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({
    t: (key: string, vars?: Record<string, unknown>) =>
      vars
        ? `${key} ${Object.entries(vars)
            .map(([k, v]) => `${k}=${String(v)}`)
            .join(" ")}`
        : key,
  }),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: h.invoke }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: h.open }));
vi.mock("../../stores/agentStore", () => ({
  useAgentStore: (selector: (state: { nodes: unknown[] }) => unknown) =>
    selector({ nodes: h.nodes }),
}));

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { PublishWizard } from "./PublishWizard";

const AGENT_ID = "com.example.demo";
const NODE_PACKAGE =
  "C:\\Users\\u\\.acowork\\acowork-node\\packages\\com.example.demo-1.0.0.agent";
const LIBRARY_PATH =
  "C:\\Users\\u\\AppData\\Local\\com.acowork.desktop\\agent-library\\com.example.demo-1.0.0.agent";

/** Fake Gateway: prepare → build → signed build, plus the write-back command. */
function mockPublishFlow(
  save: (args: Record<string, unknown>) => Promise<unknown>,
) {
  h.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case "get_agent_detail":
        return {
          instance_id: "inst-1",
          agent_id: AGENT_ID,
          node_id: "node-1",
          // The *effective* avatar: `agent_config.json` wins over the package
          // manifest's default, and it is what the picker displays.
          builtin_avatar: "icon-16",
          name: "Demo",
          version: "1.0.0",
          description: "A demo agent",
          author: "acowork",
          install_path: "C:\\agents\\demo",
          alive: true,
          lifecycle: "running",
          pid: null,
          started_at: null,
        };
      case "prepare_publish":
        return { checks: [], warnings: [], errors: [], cleaned: args?.clean === true };
      case "build_publish":
        return { output_path: NODE_PACKAGE, signed: args?.sign === true, file_size: 4096 };
      case "save_publish_to_library":
        return save(args ?? {});
      case "update_agent_manifest_avatar":
        return undefined;
      default:
        throw new Error(`unexpected command: ${command}`);
    }
  });
}

function saveCalls() {
  return h.invoke.mock.calls.filter(([command]) => command === "save_publish_to_library");
}

/** Walk the wizard to the distribute step through its own buttons. */
async function runToDistribute() {
  render(<PublishWizard open agentId={AGENT_ID} agentName="Demo" onClose={() => {}} />);
  // Steps advance only on success, so each label proves the previous call ran.
  fireEvent.click(await screen.findByRole("button", { name: /Run Check/ }));
  fireEvent.click(await screen.findByRole("button", { name: /Run Clean/ }));
  fireEvent.click(await screen.findByRole("button", { name: /Build Package/ }));
  fireEvent.click(await screen.findByRole("button", { name: /Sign Package/ }));
}

beforeEach(() => {
  cleanup();
  h.invoke.mockReset();
  h.open.mockReset();
  h.nodes = [
    {
      node_id: "node-1",
      node_name: "workstation",
      online: true,
      capabilities: [],
    },
  ];
});

describe("PublishWizard library write-back", () => {
  it("copies the signed artifact into the library and shows the file name", async () => {
    mockPublishFlow(async () => ({
      status: "saved",
      path: LIBRARY_PATH,
      file_name: "com.example.demo-1.0.0.agent",
    }));

    await runToDistribute();

    await waitFor(() => expect(saveCalls()).toHaveLength(1));
    // The signed build's output path — not the earlier unsigned build's.
    expect(saveCalls()[0][1]).toEqual({ packagePath: NODE_PACKAGE });
    expect(
      await screen.findByText(/publishWizard\.librarySaved file=com\.example\.demo-1\.0\.0\.agent/),
    ).toBeTruthy();
    expect(screen.getByText(LIBRARY_PATH)).toBeTruthy();
  });

  // Regression (real run): the picker showed `icon-16` (the instance's config)
  // while the artifact baked `icon-15` (the package manifest default), because
  // the manifest was only written when the user moved the picker in this
  // session. The build must write the selection it displays.
  it("writes the displayed avatar to the manifest before building, untouched", async () => {
    mockPublishFlow(async () => ({
      status: "saved",
      path: LIBRARY_PATH,
      file_name: "com.example.demo-1.0.0.agent",
    }));

    await runToDistribute();

    const writes = h.invoke.mock.calls.filter(
      ([command]) => command === "update_agent_manifest_avatar",
    );
    // One write for build + sign together: the selection did not change.
    expect(writes).toHaveLength(1);
    expect(writes[0][1]).toEqual({
      agentId: AGENT_ID,
      avatar: null,
      builtinAvatar: "icon-16",
    });
    // …and it happened before the package was built, not after.
    const order = h.invoke.mock.calls.map(([command]) => command);
    expect(order.indexOf("update_agent_manifest_avatar")).toBeLessThan(
      order.indexOf("build_publish"),
    );
  });

  // The guard on the other side: a detail call that never answered leaves the
  // picker on its `none` placeholder, and writing that would clear an avatar
  // the user never touched.
  it("writes nothing when the avatar selection was never seeded", async () => {
    mockPublishFlow(async () => ({
      status: "saved",
      path: LIBRARY_PATH,
      file_name: "com.example.demo-1.0.0.agent",
    }));
    const base = h.invoke.getMockImplementation();
    h.invoke.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command === "get_agent_detail") throw new Error("detail unavailable");
      return base?.(command, args);
    });

    await runToDistribute();

    expect(
      h.invoke.mock.calls.filter(([command]) => command === "update_agent_manifest_avatar"),
    ).toHaveLength(0);
  });

  it("does not copy anything before the sign step", async () => {
    mockPublishFlow(async () => ({
      status: "saved",
      path: LIBRARY_PATH,
      file_name: "com.example.demo-1.0.0.agent",
    }));

    render(<PublishWizard open agentId={AGENT_ID} agentName="Demo" onClose={() => {}} />);
    fireEvent.click(await screen.findByRole("button", { name: /Run Check/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Run Clean/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Build Package/ }));
    await screen.findByRole("button", { name: /Sign Package/ });

    expect(saveCalls()).toHaveLength(0);
  });

  it("names the owning node when the artifact is on another machine (B5)", async () => {
    mockPublishFlow(async () => ({
      status: "remote_skipped",
      path: null,
      file_name: "com.example.demo-1.0.0.agent",
    }));

    await runToDistribute();

    expect(
      await screen.findByText(
        /publishWizard\.libraryRemoteSkipped node=workstation file=com\.example\.demo-1\.0\.0\.agent/,
      ),
    ).toBeTruthy();
    // Nothing failed, so there is nothing to retry.
    expect(screen.queryByText(/^common\.retry/)).toBeNull();
    // A skipped copy must not disable the footer: the wizard still finishes.
    expect(
      screen
        .getByRole("button", { name: /publishWizard\.buttonClose/ })
        .hasAttribute("disabled"),
    ).toBe(false);
  });

  it("keeps the wizard closable when the copy fails, and can retry (B4)", async () => {
    let attempt = 0;
    mockPublishFlow(async () => {
      attempt += 1;
      if (attempt === 1) throw "The published package is not readable on this machine: denied";
      return {
        status: "saved",
        path: LIBRARY_PATH,
        file_name: "com.example.demo-1.0.0.agent",
      };
    });

    await runToDistribute();

    expect(
      await screen.findByText(/publishWizard\.librarySaveFailed error=/),
    ).toBeTruthy();
    const close = screen.getByRole("button", { name: /publishWizard\.buttonClose/ });
    expect(close.hasAttribute("disabled")).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: /common\.retry/ }));

    expect(
      await screen.findByText(/publishWizard\.librarySaved file=com\.example\.demo-1\.0\.0\.agent/),
    ).toBeTruthy();
    expect(saveCalls()).toHaveLength(2);
    expect(saveCalls()[1][1]).toEqual({ packagePath: NODE_PACKAGE });
  });
});
