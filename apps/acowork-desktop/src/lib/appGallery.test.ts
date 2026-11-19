import { describe, expect, it } from "vitest";
import {
    catalogVersionIsNewer,
    filterGalleryCards,
    galleryAttributionLinks,
    mergeGalleryCards,
    type GalleryApp,
} from "./appGallery";
import type { AppDto } from "./apps-api";

function gallery(partial: Partial<GalleryApp> & Pick<GalleryApp, "id" | "name">): GalleryApp {
    return {
        summary: "summary",
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

function installed(partial: Partial<AppDto> & Pick<AppDto, "id" | "name">): AppDto {
    return {
        version: "1.0.0",
        description: "",
        icon: "",
        builtin: false,
        enabled: true,
        status: "ready",
        port: 1,
        runtime: "generic",
        mcp_path: "/mcp",
        mcp_gateway_path: "/api/apps/x/mcp",
        mcp_url: null,
        ui_path: "/ui/",
        ui_mode: "webview",
        nav_label: "",
        nav_icon: "",
        nav_order: 0,
        health_path: "/health",
        permissions: [],
        ...partial,
    };
}

describe("mergeGalleryCards", () => {
    it("marks catalog-only when package is null", () => {
        const cards = mergeGalleryCards([gallery({ id: "figcraft", name: "FigCraft" })], []);
        expect(cards).toHaveLength(1);
        expect(cards[0].badge).toBe("catalog_only");
    });

    it("marks available when package is present", () => {
        const cards = mergeGalleryCards(
            [
                gallery({
                    id: "figcraft",
                    name: "FigCraft",
                    package: {
                        url: "https://example.com/figcraft.app",
                        sha256: "a".repeat(64),
                    },
                }),
            ],
            [],
        );
        expect(cards[0].badge).toBe("available");
    });

    it("appends local-only builtins", () => {
        const cards = mergeGalleryCards(
            [gallery({ id: "figcraft", name: "FigCraft" })],
            [installed({ id: "doc", name: "Docs", builtin: true })],
        );
        expect(cards.map((c) => c.id)).toEqual(["figcraft", "doc"]);
        expect(cards[1].badge).toBe("builtin");
    });

    it("does not offer Update when the local install is newer than the catalog", () => {
        const cards = mergeGalleryCards(
            [gallery({ id: "figcraft", name: "FigCraft", latest_version: "0.18.1" })],
            [
                installed({
                    id: "figcraft",
                    name: "FigCraft (ACowork)",
                    installed_version: "0.18.2",
                }),
            ],
        );
        expect(cards[0].badge).toBe("installed");
    });

    it("offers Update only when the catalog version is newer", () => {
        const cards = mergeGalleryCards(
            [gallery({ id: "figcraft", name: "FigCraft", latest_version: "0.18.2" })],
            [installed({ id: "figcraft", name: "FigCraft", installed_version: "0.18.1" })],
        );
        expect(cards[0].badge).toBe("update");
    });
});

describe("catalogVersionIsNewer", () => {
    it("treats 0.18.2 as newer than 0.18.1", () => {
        expect(catalogVersionIsNewer("0.18.2", "0.18.1")).toBe(true);
        expect(catalogVersionIsNewer("0.18.1", "0.18.2")).toBe(false);
        expect(catalogVersionIsNewer("0.18.2", "0.18.2")).toBe(false);
    });
});

describe("filterGalleryCards", () => {
    it("filters by publisher and summary", () => {
        const cards = mergeGalleryCards(
            [
                gallery({ id: "a", name: "Alpha", summary: "design tool" }),
                gallery({ id: "b", name: "Beta", summary: "notes" }),
            ],
            [],
        );
        expect(filterGalleryCards(cards, "design").map((c) => c.id)).toEqual(["a"]);
    });
});

describe("galleryAttributionLinks", () => {
    it("drops a homepage that repeats the repository and keeps a distinct upstream", () => {
        expect(
            galleryAttributionLinks({
                repository: "https://github.com/acowork/figcraft-app",
                homepage: "https://github.com/acowork/figcraft-app",
                upstream: "https://github.com/storytold/figcraft",
            }),
        ).toEqual([
            { kind: "repository", href: "https://github.com/acowork/figcraft-app" },
            { kind: "upstream", href: "https://github.com/storytold/figcraft" },
        ]);
    });

    it("does not treat an upstream product site as the listing homepage", () => {
        expect(
            galleryAttributionLinks({
                repository: "https://github.com/acowork/figcraft-app",
                homepage: "https://github.com/acowork/figcraft-app",
                upstream: "https://getartcraft.com/apps/figcraft",
            }).map((l) => l.kind),
        ).toEqual(["repository", "upstream"]);
    });
});
