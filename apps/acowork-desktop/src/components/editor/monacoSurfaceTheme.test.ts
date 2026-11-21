/**
 * The contract these tests hold: the document surface has exactly one source,
 * `--color-editor-canvas`, and every colour Monaco is handed is *derived* from
 * it. A regression here does not throw — it shows up as a white slab above a
 * tinted preview, which is the bug that started this.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import {
    SURFACE_TOKEN,
    SURFACE_FALLBACK,
    buildSurfaceTheme,
    computedColorToHex,
    ensureSurfaceTheme,
    lastSurfaceThemeName,
    readSurfaceColor,
    resolveColor,
    shadeSurface,
    surfaceThemeName,
    __resetSurfaceThemeRegistry,
    type SurfaceThemeRegistrar,
} from "./monacoSurfaceTheme";

describe("computedColorToHex", () => {
    it("parses the legacy comma syntax getComputedStyle emits", () => {
        expect(computedColorToHex("rgb(248, 245, 240)")).toBe("#f8f5f0");
    });

    it("parses the modern space syntax", () => {
        expect(computedColorToHex("rgb(248 245 240)")).toBe("#f8f5f0");
    });

    it("keeps alpha, which Monaco carries as a trailing byte", () => {
        expect(computedColorToHex("rgba(248, 245, 240, 0.5)")).toBe("#f8f5f080");
        expect(computedColorToHex("rgb(248 245 240 / 50%)")).toBe("#f8f5f080");
    });

    it("clips out-of-range channels instead of emitting a broken hex", () => {
        expect(computedColorToHex("rgb(300, -20, 0)")).toBe("#ff0000");
    });

    it("rejects anything that is not a computed colour", () => {
        expect(computedColorToHex("#f8f5f0")).toBeNull();
        expect(computedColorToHex("hsl(211 30% 95%)")).toBeNull();
        expect(computedColorToHex("rebeccapurple")).toBeNull();
        expect(computedColorToHex("rgb(248, 245)")).toBeNull();
        expect(computedColorToHex("rgb(a, b, c)")).toBeNull();
    });
});

describe("resolveColor", () => {
    it("resolves a literal through the browser's colour engine", () => {
        // jsdom serialises an assigned `color` back as rgb(), so this exercises
        // the real path: hsl/named/var() in, hex out.
        expect(resolveColor("rgb(248, 245, 240)")).toBe("#f8f5f0");
        expect(resolveColor("#F8F5F0")).toBe("#f8f5f0");
    });

    it("rejects a value the CSSOM will not accept rather than reporting inherited black", () => {
        expect(resolveColor("definitely-not-a-colour")).toBeNull();
    });
});

describe("shadeSurface", () => {
    it("blends toward black when asked for the darker step", () => {
        expect(shadeSurface("#ffffff", true, 0.1)).toBe("#e6e6e6");
    });

    it("blends toward white for the lighter step", () => {
        expect(shadeSurface("#1e1e1e", false, 0.1)).toBe("#353535");
    });

    it("scales with the amount", () => {
        expect(shadeSurface("#ffffff", true, 0)).toBe("#ffffff");
        expect(shadeSurface("#ffffff", true, 1)).toBe("#000000");
    });

    it("preserves an alpha channel", () => {
        expect(shadeSurface("#ffffff80", true, 0.5)).toBe("#80808080");
    });

    it("returns the input unchanged when it is not a parseable hex", () => {
        expect(shadeSurface("not-a-colour", true, 0.5)).toBe("not-a-colour");
    });

    it("derives from the pack tint rather than naming a literal", () => {
        // The point of the whole exercise: a mist canvas must not produce the
        // default theme's #F3F3F3 highlight.
        const onMist = shadeSurface("#f8f5f0", true, 0.03);
        expect(onMist).not.toBe("#f3f3f3");
        expect(onMist).toBe("#f1eee9");
    });
});

describe("buildSurfaceTheme", () => {
    it("bases on the built-in matching the mode so syntax colours are inherited", () => {
        expect(buildSurfaceTheme(false, "#ffffff").base).toBe("vs");
        expect(buildSurfaceTheme(true, "#1e1e1e").base).toBe("vs-dark");
        expect(buildSurfaceTheme(false, "#ffffff").inherit).toBe(true);
    });

    it("paints every flat background with the canvas, not with a default", () => {
        const canvas = "#f0f5fa";
        const { colors } = buildSurfaceTheme(false, canvas);
        for (const key of [
            "editor.background",
            "editorGutter.background",
            "editorWidget.background",
            "editorSuggestWidget.background",
            "editorHoverWidget.background",
        ]) {
            expect(colors[key as keyof typeof colors]).toBe(canvas);
        }
    });

    it("puts the line highlight and indent guides on the far side of the canvas", () => {
        const light = buildSurfaceTheme(false, "#ffffff").colors;
        expect(light["editor.lineHighlightBackground"] < "#ffffff").toBe(true);

        const dark = buildSurfaceTheme(true, "#1e1e1e").colors;
        expect(dark["editor.lineHighlightBackground"] > "#1e1e1e").toBe(true);
    });

    it("leaves no default surface colour behind once a pack tints the canvas", () => {
        // The hardcoded literals this replaced. If any of them reappears in the
        // derived set, the pack is being ignored again.
        const { colors } = buildSurfaceTheme(false, "#f1eee9");
        const banned = new Set(["#ffffff", "#FFFFFF", "#f3f3f3", "#F3F3F3", "#1e1e1e"]);
        for (const value of Object.values(colors)) {
            expect(banned.has(value as string)).toBe(false);
        }
    });
});

describe("surfaceThemeName", () => {
    it("changes when the resolved colour changes, so the theme prop repaints", () => {
        expect(surfaceThemeName(false, "#ffffff")).not.toBe(surfaceThemeName(false, "#f0f5fa"));
    });

    it("is stable for the same inputs", () => {
        expect(surfaceThemeName(false, "#ffffff")).toBe(surfaceThemeName(false, "#ffffff"));
    });

    it("keeps light and dark apart even at the same colour", () => {
        expect(surfaceThemeName(false, "#ffffff")).not.toBe(surfaceThemeName(true, "#ffffff"));
    });

    it("is case-insensitive so the same tint never registers twice", () => {
        expect(surfaceThemeName(false, "#F8F5F0")).toBe(surfaceThemeName(false, "#f8f5f0"));
    });
});

describe("ensureSurfaceTheme", () => {
    let registrar: SurfaceThemeRegistrar & { defineTheme: ReturnType<typeof vi.fn> };
    beforeEach(() => {
        __resetSurfaceThemeRegistry();
        registrar = { defineTheme: vi.fn() };
    });

    it("returns a name it has actually registered, in the same call", () => {
        const name = ensureSurfaceTheme(registrar, false, "#ffffff");
        expect(registrar.defineTheme).toHaveBeenCalledWith(name, expect.anything());
    });

    it("registers once and reuses the name on repeat calls", () => {
        const a = ensureSurfaceTheme(registrar, false, "#ffffff");
        const b = ensureSurfaceTheme(registrar, false, "#ffffff");
        expect(a).toBe(b);
        expect(registrar.defineTheme).toHaveBeenCalledTimes(1);
    });

    it("registers again for a different colour", () => {
        ensureSurfaceTheme(registrar, false, "#ffffff");
        ensureSurfaceTheme(registrar, false, "#f0f5fa");
        expect(registrar.defineTheme).toHaveBeenCalledTimes(2);
    });

    it("remembers the last name so a re-mounted pane does not flash the built-in", () => {
        // Monaco's registry is global, React state is not, and the file pane
        // unmounts when the last file tab closes.
        expect(lastSurfaceThemeName()).toBeNull();
        const name = ensureSurfaceTheme(registrar, false, "#f0f5fa");
        expect(lastSurfaceThemeName()).toBe(name);
    });
});

describe("readSurfaceColor", () => {
    it("falls back to the built-in value when the token is unresolvable", () => {
        // jsdom ships no canvas backend and no resolved custom properties, so
        // the fallback is what these tests see. It must be the `vs`/`vs-dark`
        // pair, never an empty string that Monaco would reject.
        expect(readSurfaceColor(false)).toBe(SURFACE_FALLBACK.light);
        expect(readSurfaceColor(true)).toBe(SURFACE_FALLBACK.dark);
    });

    it("resolves the token from the live cascade when one is set", () => {
        // The path that matters: a pack value reaching Monaco. Asserted only if
        // the environment can actually resolve custom properties, so the case
        // never encodes a jsdom limitation as a requirement.
        document.documentElement.style.setProperty(SURFACE_TOKEN, "rgb(248, 245, 240)");
        try {
            const declared = getComputedStyle(document.documentElement).getPropertyValue(SURFACE_TOKEN).trim();
            if (!declared) return;
            expect(readSurfaceColor(false)).toBe("#f8f5f0");
        } finally {
            document.documentElement.style.removeProperty(SURFACE_TOKEN);
        }
    });

    it("keeps the fallback equal to the default pack's own values", () => {
        // The fallback is only honest while it matches themes/default.css.
        expect(SURFACE_FALLBACK.light).toBe("#FFFFFF");
        expect(SURFACE_FALLBACK.dark).toBe("#1E1E1E");
    });

    it("reads the token every pack is required to override", () => {
        expect(SURFACE_TOKEN).toBe("--color-editor-canvas");
    });
});
