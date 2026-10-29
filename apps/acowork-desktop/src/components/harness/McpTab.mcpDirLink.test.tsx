//! Regression guard for the "find MCP servers here" hint in the add-server
//! dialog.
//!
//! The catalog only ships the built-in presets, so the add dialog used to
//! look like the only place a server could possibly come from. The hint
//! row (label + the two directory links) is the fix; this pins the two
//! halves of it: the row is reachable from the dialog, and clicking a link
//! hands that URL to plugin-shell `open` — the system browser — rather than
//! to the webview's new-window path (the one under suspicion for the
//! link-click webview crash, see the temporary probe in main.tsx).

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import i18n from "../../i18n";
import { McpTab } from "./HarnessPage";

const mockOpen = vi.fn(async () => {});

vi.mock("@tauri-apps/plugin-shell", () => ({ open: (p: string) => mockOpen(p) }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => ({})) }));

/** The dialog opens from the catalog card header; everything it renders
 *  before that is the preset/custom server list, which needs only a
 *  well-formed (empty) catalog response. */
function stubFetch() {
  vi.stubGlobal(
    "fetch",
    vi.fn((url: string) => {
      if (url.includes("/api/mcp-catalog")) {
        return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({ servers: [] }) });
      }
      return Promise.resolve({ ok: true, status: 200, json: () => Promise.resolve({}) });
    }),
  );
}

function openDialog() {
  fireEvent.click(screen.getByRole("button", { name: i18n.t("harnessMcp.addServer") }));
}

/** Both directory links share one testid — the row is a list, not a slot. */
function findLinks(): HTMLElement[] {
  return screen.getAllByTestId("mcp-find-servers-link");
}

async function renderDialog() {
  render(<McpTab />);
  await waitFor(() => expect(screen.queryByText(i18n.t("harnessMcp.loadingCatalog"))).toBeNull());
  openDialog();
}

describe("McpTab add-dialog directory hint", () => {
  beforeEach(() => {
    mockOpen.mockClear();
    stubFetch();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("shows both directory links with their labels once the dialog is open", async () => {
    await renderDialog();

    expect(findLinks().map((el) => el.textContent)).toEqual([
      "https://mcpdir.dev",
      "https://github.com/mcp",
    ]);
    expect(screen.getByText(i18n.t("harnessMcp.findServers"))).toBeTruthy();
  });

  it("opens the matching directory in the system browser on click", async () => {
    await renderDialog();

    fireEvent.click(findLinks()[0]);
    expect(mockOpen).toHaveBeenCalledWith("https://mcpdir.dev");

    fireEvent.click(findLinks()[1]);
    expect(mockOpen).toHaveBeenCalledWith("https://github.com/mcp");
  });
});
