/**
 * The persistent half of the gallery / installed-apps catalog (appGallery.ts,
 * apps-api.ts): a successful network read is written to localStorage so the
 * next entry into a surface can paint real artwork on the first frame.
 *
 * What this pins:
 *   1. a successful fetch leaves a readable cache (write-through);
 *   2. the cache is keyed by URL — a staging override must not seed the
 *      production gallery and vice versa;
 *   3. a failed fetch does NOT overwrite the last good cache (an outage
 *      must not erase the icons that were working);
 *   4. `galleryIconMap` is the ONE derivation shared by the cached seed and
 *      the fresh fetch, so the two paths cannot disagree.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
    fetchGalleryIndex,
    galleryIconMap,
    readCachedGalleryIndex,
    DEFAULT_APP_GALLERY_INDEX_URL,
    type GalleryApp,
} from "./appGallery";
import { listApps, readCachedInstalledApps } from "./apps-api";
import { getGatewayUrl } from "./config";
import { readCatalog } from "./catalogCache";

function galleryApp(partial: Partial<GalleryApp> & Pick<GalleryApp, "id">): GalleryApp {
    return {
        name: partial.id,
        summary: "s",
        publisher: { id: "acowork", display_name: "ACowork" },
        license: "MIT",
        repository: "https://github.com/acowork/x",
        categories: [],
        package: null,
        readme_md: "# Hi",
        source_path: "apps/acowork/x",
        ...partial,
    };
}

function jsonResponse(body: unknown): Response {
    return {
        ok: true,
        status: 200,
        headers: new Headers({ "content-type": "application/json" }),
        json: async () => body,
    } as unknown as Response;
}

describe("gallery catalog persistence", () => {
    beforeEach(() => {
        localStorage.clear();
        vi.unstubAllGlobals();
    });

    it("writes through: a successful fetch is readable from the cache", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(async () => jsonResponse({ schema_version: 1, apps: [galleryApp({ id: "figcraft", icon: "https://g/fig.png" })] })),
        );
        const index = await fetchGalleryIndex();
        expect(readCachedGalleryIndex()).toEqual(index);
    });

    it("keys the cache by URL: a staging fetch cannot seed production", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(async () => jsonResponse({ schema_version: 1, apps: [galleryApp({ id: "a", icon: "https://staging/a.png" })] })),
        );
        await fetchGalleryIndex("https://staging.acowork.ai/data/index.json");
        expect(readCachedGalleryIndex("https://staging.acowork.ai/data/index.json")).toBeTruthy();
        expect(readCachedGalleryIndex(DEFAULT_APP_GALLERY_INDEX_URL)).toBeNull();
    });

    it("keeps the last good cache when the network fails", async () => {
        vi.stubGlobal(
            "fetch",
            vi.fn(async () => jsonResponse({ schema_version: 1, apps: [galleryApp({ id: "a", icon: "https://g/a.png" })] })),
        );
        await fetchGalleryIndex();
        vi.stubGlobal("fetch", vi.fn(async () => {
            throw new Error("offline");
        }));
        await expect(fetchGalleryIndex()).rejects.toThrow("offline");
        expect(readCachedGalleryIndex()?.apps.map((a) => a.id)).toEqual(["a"]);
    });

    it("galleryIconMap skips entries without usable artwork", () => {
        const index = {
            schema_version: 1,
            apps: [
                galleryApp({ id: "a", icon: "https://g/a.png" }),
                galleryApp({ id: "b", icon: null }),
            ],
            base_url: "https://g",
        };
        expect(galleryIconMap(index)).toEqual({ a: "https://g/a.png" });
        expect(galleryIconMap(null)).toEqual({});
    });
});

describe("installed-apps persistence", () => {
    beforeEach(() => {
        localStorage.clear();
        vi.unstubAllGlobals();
    });

    it("writes through: a successful listApps is readable from the cache", async () => {
        const apps = [
            {
                id: "figcraft",
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
            },
        ];
        vi.stubGlobal("fetch", vi.fn(async () => jsonResponse({ apps })));
        expect(await listApps()).toEqual(apps);
        expect(readCachedInstalledApps()).toEqual(apps);
        // Same key the reader uses — the seed and the write cannot drift.
        expect(readCatalog(`installedApps:${getGatewayUrl()}`)).toEqual(apps);
    });
});
