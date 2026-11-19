import { describe, expect, it } from "vitest";
import {
    filterGalleryCards,
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
