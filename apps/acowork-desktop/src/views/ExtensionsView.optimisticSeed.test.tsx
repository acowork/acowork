/**
 * Apps workbench first paint, sync strip and install feedback (ExtensionsView).
 *
 * Three complaints, three pins:
 *   1. every entry into the panel used to be blank-then-pop — the cards are
 *      now seeded from the persistent catalog, so they exist on frame one;
 *   2. a seeded panel then revalidates SILENTLY, which reads as a frozen
 *      list — while that network pass is in flight a thin spinner strip
 *      (role=status, the chat history's load-more shape) sits above the
 *      cards, and it disappears when the data lands.
 *   3. installs are Gateway-owned jobs: `POST /api/apps/install-url` answers
 *      `202 {job_id}` and the panel polls `/api/apps/install-jobs`. The job is
 *      what carries real byte progress, and refetching it on mount is what
 *      makes a running install survive switching panels.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";

const h = vi.hoisted(() => ({
  listApps: vi.fn(),
  fetchGalleryIndex: vi.fn(),
  startAppInstallFromUrl: vi.fn(),
  listInstallJobs: vi.fn(),
}));

vi.mock("../lib/apps-api", async (importActual) => ({
  ...(await importActual<Record<string, unknown>>()),
  listApps: h.listApps,
  startAppInstallFromUrl: h.startAppInstallFromUrl,
  listInstallJobs: h.listInstallJobs,
}));
vi.mock("../lib/appGallery", async (importActual) => ({
  ...(await importActual<Record<string, unknown>>()),
  fetchGalleryIndex: h.fetchGalleryIndex,
}));
// The key-echo convention: t() returns the key, so assertions pin keys.
vi.mock("../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

import { ExtensionsView } from "./ExtensionsView";
import { writeCatalog } from "../lib/catalogCache";
import { getGatewayUrl } from "../lib/config";
import { DEFAULT_APP_GALLERY_INDEX_URL } from "../lib/appGallery";
import type { AppDto } from "../lib/apps-api";

const FIGCRAFT: AppDto = {
  id: "com.acowork.figcraft",
  name: "FigCraft",
  version: "1.0.0",
  description: "",
  icon: "",
  builtin: false,
  enabled: true,
  status: "ready",
  port: 1,
  runtime: "generic",
  mcp_path: "/mcp",
  mcp_gateway_path: "/x",
  mcp_url: null,
  ui_path: "/ui/",
  ui_mode: "webview",
  nav_label: "FigCraft",
  nav_icon: "",
  nav_order: 0,
  health_path: "/health",
  permissions: [],
  installed_at: "1700000000",
};

function galleryIndex(icon: string) {
  return {
    schema_version: 1,
    base_url: "https://gallery.acowork.ai/data",
    apps: [
      {
        id: FIGCRAFT.id,
        name: "FigCraft",
        summary: "A diagram editor",
        publisher: { id: "acowork", display_name: "ACowork" },
        license: "MIT",
        repository: "https://github.com/acowork/figcraft",
        categories: [],
        icon,
        package: null,
        readme_md: "",
        source_path: "apps/acowork/figcraft",
      },
    ],
  };
}

function seedCache() {
  writeCatalog(`installedApps:${getGatewayUrl()}`, [FIGCRAFT]);
  writeCatalog(`appGallery:${DEFAULT_APP_GALLERY_INDEX_URL}`, galleryIndex("https://gallery/a.png"));
}

describe("ExtensionsView optimistic first paint", () => {
  beforeEach(() => {
    localStorage.clear();
    h.listApps.mockReturnValue(new Promise<AppDto[]>(() => {}));
    h.fetchGalleryIndex.mockReturnValue(new Promise<never>(() => {}));
    h.listInstallJobs.mockResolvedValue([]);
  });

  it("paints the cards on the first frame from the cache", () => {
    seedCache();
    render(<ExtensionsView />);
    // Synchronous: the card name must already be in the listbox.
    expect(screen.getByRole("option", { name: /FigCraft/ })).toBeTruthy();
  });

  it("shows a sync strip while the revalidation is in flight", () => {
    seedCache();
    render(<ExtensionsView />);
    const strip = screen.getByRole("status");
    expect(strip.textContent).toContain("common.loading");
  });

  it("drops the strip when the network answers", async () => {
    seedCache();
    h.listApps.mockResolvedValue([FIGCRAFT]);
    h.fetchGalleryIndex.mockResolvedValue(galleryIndex("https://gallery/a.png") as never);
    render(<ExtensionsView />);
    await waitFor(() => expect(screen.queryByRole("status")).toBeNull());
  });

  it("cold machine: no cards, no crash, strip says it is working", () => {
    render(<ExtensionsView />);
    expect(screen.queryByRole("option")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("common.loading");
  });
});

describe("ExtensionsView install jobs", () => {
  /** A catalog card that is installable: package present, nothing installed. */
  function installableIndex() {
    const index = galleryIndex("");
    index.apps[0].package = {
      url: "https://github.com/acowork/figcraft/releases/download/v1/figcraft.app.zip",
      sha256: "0".repeat(64),
      size_bytes: 31_685_517,
    };
    return index;
  }

  /** A job as the Gateway reports one mid-download (4 MB of 31.6 MB ≈ 13 %). */
  const RUNNING = {
    job_id: "job-1",
    hint_id: FIGCRAFT.id,
    phase: "download" as const,
    received_bytes: 4_000_000,
    total_bytes: 31_685_517,
    started_at: 1_700_000_000,
  };

  beforeEach(() => {
    localStorage.clear();
    h.listApps.mockResolvedValue([]);
    h.fetchGalleryIndex.mockResolvedValue(installableIndex() as never);
    h.listInstallJobs.mockResolvedValue([]);
    h.startAppInstallFromUrl.mockResolvedValue({
      job_id: "job-1",
      hint_id: FIGCRAFT.id,
      phase: "download",
      received_bytes: 0,
      total_bytes: 31_685_517,
      started_at: 1_700_000_000,
    });
  });

  it("reattaches to a running install on entry — the job is the Gateway's, not this view's", async () => {
    // Coming back to the panel with a download already running: the old
    // view-local state had nothing to show and offered Install again, which
    // started a SECOND download of the same package.
    h.listInstallJobs.mockResolvedValue([RUNNING]);
    render(<ExtensionsView />);

    const progress = await screen.findByText("extensions.downloading");
    expect(progress).toBeTruthy();
    expect(screen.getByText("13%")).toBeTruthy();
    // The card is still offering its install action, but the button is busy
    // and reads as progress rather than "Install".
    fireEvent.click(screen.getByRole("option", { name: /FigCraft/ }));
    const button = await screen.findByRole("button", { name: /extensions\.downloading/ });
    expect(button.getAttribute("aria-busy")).toBe("true");
    expect((button as HTMLButtonElement).disabled).toBe(true);
  });

  it("a download with no known size starts the bar at 0 %, not full", async () => {
    // Before the first bytes land the Gateway has no Content-Length yet. A
    // full-width bar here reads as "finished" and then visibly drops back to
    // 0 once the size arrives.
    const job = { ...RUNNING, received_bytes: 0, total_bytes: null };
    h.listInstallJobs.mockResolvedValue([job]);
    h.startAppInstallFromUrl.mockResolvedValue(job);
    render(<ExtensionsView />);

    await screen.findByText("extensions.downloading");
    const [, , fill] = screen.getByRole("status").querySelectorAll("div");
    expect((fill as HTMLElement).style.width).toBe("0%");
  });

  it("starting an install shows the job's progress instead of a dead button", async () => {
    render(<ExtensionsView />);
    await waitFor(() => expect(screen.queryByRole("status")).toBeNull());

    fireEvent.click(screen.getByRole("option", { name: /FigCraft/ }));
    fireEvent.click(screen.getByRole("button", { name: "extensions.install" }));

    // The POST only ACCEPTED the work; the job it answered with is what the
    // panel renders, and the Gateway's own id is sent back as the hint.
    await waitFor(() => expect(h.startAppInstallFromUrl).toHaveBeenCalledTimes(1));
    expect(h.startAppInstallFromUrl.mock.calls[0][2]).toBe(FIGCRAFT.id);
    const button = await screen.findByRole("button", { name: /extensions\.downloading/ });
    expect((button as HTMLButtonElement).disabled).toBe(true);
    // Same job on the card and in the detail pane: 0 % just started.
    expect(screen.getAllByText("0%").length).toBeGreaterThan(0);
  });

  it("a failed job surfaces the gateway's code and detail", async () => {
    h.listInstallJobs.mockResolvedValue([
      {
        ...RUNNING,
        phase: "failed",
        error_code: "package_malformed",
        error: "download body: connection reset",
        finished_at: 1_700_000_030,
      },
    ]);
    render(<ExtensionsView />);

    const line = await screen.findByText(/应用包格式无效/);
    expect(line.textContent).toContain("connection reset");
  });
});

describe("ExtensionsView app status", () => {
  beforeEach(() => {
    localStorage.clear();
    h.listInstallJobs.mockResolvedValue([]);
  });

  it("a starting app flips to running without a remount, and the dot says what it means", async () => {
    // The install job's single reloadAll() lands before the spawned process
    // answers: the dot used to sit amber until the panel was re-entered.
    h.listApps
      .mockResolvedValueOnce([{ ...FIGCRAFT, status: "starting" }])
      .mockResolvedValue([FIGCRAFT]);
    h.fetchGalleryIndex.mockResolvedValue(galleryIndex("") as never);
    render(<ExtensionsView />);

    // Colour alone says nothing: the dot carries the status as a label.
    expect(await screen.findByLabelText("extensions.statusStarting")).toBeTruthy();
    await waitFor(() => expect(screen.getByLabelText("extensions.statusReady")).toBeTruthy(), {
      timeout: 4000,
    });
  });
});
