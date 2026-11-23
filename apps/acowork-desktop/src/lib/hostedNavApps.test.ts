import { describe, expect, it } from "vitest";
import type { AppDto } from "./apps-api";
import { hostedAppIconSrc, hostedAppLetter, hostedNavApps } from "./hostedNavApps";

function app(partial: Partial<AppDto> & Pick<AppDto, "id">): AppDto {
    return {
        name: partial.id,
        version: "1.0.0",
        description: "",
        icon: "",
        builtin: false,
        enabled: true,
        status: "ready",
        port: 18090,
        runtime: "generic",
        mcp_path: "/mcp",
        mcp_gateway_path: `/api/apps/${partial.id}/mcp`,
        mcp_url: null,
        ui_path: "/ui/",
        ui_mode: "webview",
        nav_label: partial.id,
        nav_icon: "",
        nav_order: 40,
        health_path: "/health",
        permissions: [],
        ...partial,
    };
}

describe("hostedNavApps", () => {
    it("keeps enabled non-builtin webview apps and sorts by nav_order", () => {
        const slots = hostedNavApps([
            app({ id: "zed", nav_order: 50 }),
            app({ id: "figcraft", nav_order: 40, nav_label: "FigCraft" }),
            app({ id: "pm", builtin: true, nav_order: 10 }),
            app({ id: "off", enabled: false, nav_order: 5 }),
            app({ id: "native", ui_mode: "native", nav_order: 1 }),
        ]);
        expect(slots.map((a) => a.id)).toEqual(["figcraft", "zed"]);
    });

    it("drops disabled status", () => {
        expect(hostedNavApps([app({ id: "x", status: "disabled" })])).toEqual([]);
    });

    it("orders by install time so a new install is appended last", () => {
        const slots = hostedNavApps([
            app({ id: "new", installed_at: "1700000002" }),
            app({ id: "old", installed_at: "1700000001" }),
        ]);
        expect(slots.map((a) => a.id)).toEqual(["old", "new"]);
    });

    it("keeps undated apps last and falls back to nav_order", () => {
        const slots = hostedNavApps([
            app({ id: "undated", nav_order: 41 }),
            app({ id: "dated", installed_at: "1700000001" }),
        ]);
        expect(slots.map((a) => a.id)).toEqual(["dated", "undated"]);
    });
});

describe("hosted app icon helpers", () => {
    it("uses http(s) icons as img src", () => {
        expect(
            hostedAppIconSrc(app({ id: "a", icon: "https://gallery.acowork.ai/x.png" })),
        ).toBe("https://gallery.acowork.ai/x.png");
        expect(hostedAppIconSrc(app({ id: "a", icon: "figcraft" }))).toBeNull();
    });

    it("falls back to the gallery catalog icon when the manifest stores a name", () => {
        expect(
            hostedAppIconSrc(app({ id: "figcraft", icon: "figcraft", nav_icon: "figcraft" }), {
                figcraft: "https://gallery.acowork.ai/apps/acowork/figcraft/assets/icon.png",
            }),
        ).toBe("https://gallery.acowork.ai/apps/acowork/figcraft/assets/icon.png");
    });

    it("prefers an absolute nav_icon over the gallery catalog", () => {
        expect(
            hostedAppIconSrc(
                app({ id: "figcraft", icon: "figcraft", nav_icon: "https://example.com/a.png" }),
                { figcraft: "https://gallery.acowork.ai/apps/acowork/figcraft/assets/icon.png" },
            ),
        ).toBe("https://example.com/a.png");
    });

    it("letter comes from nav_label", () => {
        expect(hostedAppLetter(app({ id: "figcraft", nav_label: "FigCraft (ACowork)" }))).toBe("F");
    });
});
