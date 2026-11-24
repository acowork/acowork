/**
 * Apps workbench optimistic first paint + sync strip (ExtensionsView).
 *
 * Two complaints, two pins:
 *   1. every entry into the panel used to be blank-then-pop — the cards are
 *      now seeded from the persistent catalog, so they exist on frame one;
 *   2. a seeded panel then revalidates SILENTLY, which reads as a frozen
 *      list — while that network pass is in flight a thin spinner strip
 *      (role=status, the chat history's load-more shape) sits above the
 *      cards, and it disappears when the data lands.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";

const h = vi.hoisted(() => ({
  listApps: vi.fn(),
  fetchGalleryIndex: vi.fn(),
}));

vi.mock("../lib/apps-api", async (importActual) => ({
  ...(await importActual<Record<string, unknown>>()),
  listApps: h.listApps,
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
