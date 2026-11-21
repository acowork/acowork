/**
 * Guards the one-token rule for the file pane's document surface.
 *
 * The bug these lines prevent is invisible in the default palette: the tab strip
 * was pinned to `bg-white` to match Monaco's hardcoded `vs` theme, which looked
 * right until a theme pack tinted the preview underneath it. Source-level
 * assertions are the only cheap way to catch the literal coming back, because
 * jsdom paints nothing and Monaco's colour parser is not exercised in tests.
 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const src = (path: string) => readFileSync(resolve(__dirname, "..", "..", path), "utf8");

/** The tab-strip div: the flex container that opens ScrollableTabBar. */
function tabStripClass(panel: string): string {
    const at = panel.indexOf("<ScrollableTabBar");
    expect(at, "FileEditorPanel must render its tabs through ScrollableTabBar").toBeGreaterThan(-1);
    const opening = panel.lastIndexOf("<div", at);
    const cls = /className="([^"]+)"/.exec(panel.slice(opening, at));
    expect(cls, "tab strip container must declare a className").not.toBeNull();
    return cls![1];
}

describe("file pane document surface", () => {
    const panel = src("components/editor/FileEditorPanel.tsx");

    it("paints the tab strip from the surface token, not a literal", () => {
        const strip = tabStripClass(panel);
        expect(strip).toContain("bg-editor-canvas");
    });

    it("keeps the pre-fix hardcoding out of the tab strip", () => {
        // `bg-white` matched Monaco's `vs` but no pack could override it;
        // `dark:bg-right-panel` was the earlier, two-tone version of the same guess.
        const strip = tabStripClass(panel);
        expect(strip).not.toMatch(/bg-white/);
        expect(strip).not.toMatch(/dark:bg-right-panel/);
    });

    it("fills its own top padding so the outer panel cannot show through", () => {
        expect(tabStripClass(panel)).toContain("pt-[5px]");
    });

    it("hands every Monaco surface the resolved theme, never a built-in literal", () => {
        const literals = panel.match(/theme="(vs|vs-dark)"/g) ?? [];
        expect(literals, "Monaco theme must come from resolvedMonacoTheme").toEqual([]);
        const uses = panel.match(/theme=\{resolvedMonacoTheme\}/g) ?? [];
        // Editor + DiffEditor + the read-only/preview variants all share it; if
        // one is dropped it silently reverts to the pack-blind built-in.
        expect(uses.length).toBeGreaterThanOrEqual(4);
    });

    it("registers the surface theme with Monaco before relying on its name", () => {
        expect(panel).toContain("ensureSurfaceTheme");
        // Defining is not enough: the wrapper's setTheme effect can fire before
        // this component's, so the panel applies the theme itself as well.
        expect(panel).toMatch(/editor\.setTheme\(/);
    });

    it("re-registers when the pack or the mode changes", () => {
        expect(panel).toContain("subscribeThemePacks");
        // The effect's dependency list is the contract: a pack switch or a
        // light/dark flip must re-run registration, nothing else should.
        expect(panel).toMatch(/\[isDarkSurface, themePackId\]/);
    });

    it("reads the same token the preview already paints with", () => {
        const surface = src("components/editor/monacoSurfaceTheme.ts");
        expect(surface).toContain('"--color-editor-canvas"');
        const preview = src("components/editor/MarkdownPreviewView.tsx");
        expect(preview).toContain("bg-editor-canvas");
    });
});
