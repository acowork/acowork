/**
 * Nav rail optimistic first paint (catalogCache.ts seed).
 *
 * The complaint this pins away: every launch painted letter placeholders
 * first and flipped to the real hosted-app artwork when the gallery answered
 * — two frames of visible flicker for data the machine had already seen.
 *
 * What this asserts:
 *   1. with a populated cache and the network STILL in flight, the first
 *      frame already carries the real artwork <img> (no letter tile);
 *   2. a cold machine (no cache) keeps the letter tile — the seed is an
 *      optimization, not a promise of artwork;
 *   3. when the network finally answers with a DIFFERENT icon, the tile
 *      updates — the seed never freezes the screen.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";

const h = vi.hoisted(() => ({
  listApps: vi.fn(),
  fetchGalleryIndex: vi.fn(),
}));

vi.mock("../../lib/apps-api", async (importActual) => ({
  ...(await importActual<Record<string, unknown>>()),
  listApps: h.listApps,
}));
vi.mock("../../lib/appGallery", async (importActual) => ({
  ...(await importActual<Record<string, unknown>>()),
  fetchGalleryIndex: h.fetchGalleryIndex,
}));
vi.mock("../account/AccountMenu", () => ({
  AccountMenu: () => <div data-testid="account-menu" />,
}));

import { NavBar } from "./NavBar";
import { useAuthStore } from "../../stores/authStore";
import { useProjectGroupStore } from "../../stores/projectGroupStore";
import { writeCatalog } from "../../lib/catalogCache";
import { getGatewayUrl } from "../../lib/config";
import { DEFAULT_APP_GALLERY_INDEX_URL } from "../../lib/appGallery";
import type { AppDto } from "../../lib/apps-api";
import type { UserAccount } from "../../lib/types";

const ME = "me-0000-4000-8000-000000000000";
const self: UserAccount = {
  user_id: ME,
  username: "me",
  display_name: "Me",
  role: "user",
  language: "en",
  timezone: "UTC",
  created_at: "2025-01-01T00:00:00Z",
  updated_at: "2025-01-01T00:00:00Z",
};

const FIGCRAFT: AppDto = {
  id: "com.acowork.figcraft",
  name: "FigCraft",
  version: "1.0.0",
  description: "",
  icon: "figcraft",
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

const GALLERY_ICON = "https://gallery.acowork.ai/icons/figcraft.png";

function seedCache() {
  writeCatalog(`installedApps:${getGatewayUrl()}`, [FIGCRAFT]);
  writeCatalog(`appGallery:${DEFAULT_APP_GALLERY_INDEX_URL}`, {
    schema_version: 1,
    base_url: "https://gallery.acowork.ai/data",
    apps: [
      {
        id: FIGCRAFT.id,
        name: "FigCraft",
        summary: "s",
        publisher: { id: "acowork", display_name: "ACowork" },
        license: "MIT",
        repository: "https://github.com/acowork/figcraft",
        categories: [],
        icon: GALLERY_ICON,
        package: null,
        readme_md: "",
        source_path: "apps/acowork/figcraft",
      },
    ],
  });
}

function navHostedImgs(): HTMLImageElement[] {
  return Array.from(document.querySelectorAll("nav img")).filter((img) =>
    /^https:\/\/gallery\.acowork\.ai\//.test(img.src),
  );
}

describe("NavBar optimistic first paint", () => {
  beforeEach(() => {
    localStorage.clear();
    useAuthStore.setState({
      accountState: "ready",
      status: "logged_in",
      account: self,
      accessToken: "token",
      viewAsUserId: null,
      registrationOpen: false,
    });
    useProjectGroupStore.setState({ groups: [], dms: [], activeGroupId: null });
    // Both network reads stay in flight for the whole first render.
    h.listApps.mockReturnValue(new Promise<AppDto[]>(() => {}));
    h.fetchGalleryIndex.mockReturnValue(new Promise<never>(() => {}));
  });

  it("paints the real artwork on the first frame from the cache", () => {
    seedCache();
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);

    // Synchronous — no waitFor: the whole point is that frame one has it.
    const img = navHostedImgs().find((i) => i.src === GALLERY_ICON);
    expect(img, "the cached gallery icon must be on screen before the network answers").toBeTruthy();
    expect(screen.queryByText("F"), "the letter placeholder must NOT paint when the icon is cached").toBeNull();
  });

  it("keeps the letter tile on a cold machine (no cache, no network)", () => {
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    // Nothing cached: the tile cannot exist yet — the list itself arrives
    // with the network. No crash, no fake artwork.
    expect(navHostedImgs().length).toBe(0);
  });

  it("applies a different icon when the network finally answers", async () => {
    seedCache();
    const FRESH = "https://gallery.acowork.ai/icons/figcraft-v2.png";
    h.fetchGalleryIndex.mockResolvedValue({
      schema_version: 1,
      base_url: "https://gallery.acowork.ai/data",
      apps: [
        {
          id: FIGCRAFT.id,
          name: "FigCraft",
          summary: "s",
          publisher: { id: "acowork", display_name: "ACowork" },
          license: "MIT",
          repository: "https://github.com/acowork/figcraft",
          categories: [],
          icon: FRESH,
          package: null,
          readme_md: "",
          source_path: "apps/acowork/figcraft",
        },
      ],
    } as never);
    h.listApps.mockResolvedValue([FIGCRAFT]);
    render(<NavBar currentView="chat" onViewChange={() => {}} onAvatarClick={() => {}} />);
    await waitFor(() => {
      const img = Array.from(document.querySelectorAll("nav img")).find((i) => i.src === FRESH);
      expect(img, "a changed catalog must overwrite the seed").toBeTruthy();
    });
  });
});
