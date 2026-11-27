/**
 * The global font-size knob must scale the WHOLE app, not just chat.
 *
 * Why this file exists:
 *   Settings → Appearance → font size writes `--ui-font-size` onto
 *   :root. Until recently it only reached chat, because chat bound the
 *   variable to ~50 elements via inline `style={{fontSize: ...}}` while
 *   every other view (pm / doc / extension / harness / settings / setup)
 *   used fixed `rem` Tailwind sizes. Changing the setting resized chat
 *   and nothing else, so font sizes visibly diverged per view.
 *
 * What this pins (the two things that silently break the unification):
 *   1. `body` font-size tracks `--ui-font-size` — the inheritance root
 *      every un-classed row (the pm project list among them) reads.
 *   2. The `--text-*` tokens are `em` ratios of that same variable, not
 *      fixed `rem`. A `rem` here would make the setting a no-op outside
 *      chat again — and, worse, would also scale every layout offset
 *      that is expressed in rem.
 *
 * Why source scanning:
 *   These are CSS custom properties consumed by Tailwind's generated
 *   utilities. jsdom resolves no cascade and computes no font sizes, so
 *   the declaration text IS the spec — same approach as
 *   capsule.test.tsx.
 */
import { readdirSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";

const __dirname = dirname(fileURLToPath(import.meta.url));
const css = readFileSync(
  resolve(__dirname, "..", "styles", "globals.css"),
  "utf8",
);

describe("global font size scales the whole app", () => {
  it("declares --ui-font-size on :root as the pre-JS default", () => {
    // settingsStore overwrites it at boot; without a CSS default the
    // first paint would resolve var(--ui-font-size) to nothing.
    const root = css.match(/:root \{[^}]*--ui-font-size:\s*([^;]+);/);
    expect(root, ":root has no --ui-font-size").not.toBeNull();
    // Must equal DEFAULT_FONT_SIZE in lib/defaults.ts (0.875) — a
    // mismatch makes the app jump on first paint after a reload.
    expect(root![1].trim()).toBe("0.875rem");
  });

  it("binds body font-size to --ui-font-size, not a fixed size", () => {
    const body = css.match(/^body \{([\s\S]*?)\n\}/m);
    expect(body, "no body rule").not.toBeNull();
    expect(body![1]).toContain("font-size: var(--ui-font-size");
    // The old value pinned it to a static ratio, which is exactly the
    // state this file exists to prevent coming back.
    expect(body![1]).not.toContain("font-size: var(--text-base)");
  });

  it("expresses every --text-* token as an em ratio", () => {
    // `9/10/11` join `xs…xl` here: the dense card-title / list-meta sizes
    // used to be arbitrary px (`text-[11px]` & co, ~430 call sites),
    // which is precisely the class of value this knob cannot reach —
    // the symptom being "card titles don't follow the global font size".
    // They must stay named tokens or that regression comes straight back.
    const tokens = ["9", "10", "11", "xs", "sm", "base", "lg", "xl"];
    for (const t of tokens) {
      const m = css.match(new RegExp(`--text-${t}:\\s*([^;]+);`));
      expect(m, `--text-${t} is not declared`).not.toBeNull();
      // `em` keeps the ratio intact under any base size.
      // `rem` would freeze the scale AND drag every rem-based offset
      // with it, so the failure mode is layout drift, not just no-op.
      expect(m![1].trim(), `--text-${t} must be em, not rem`).toMatch(/em$/);
    }
  });

  // A nested `text-*` token compounds: the status bar root is `text-11`
  // (11px), so an inner `text-10` resolved to 0.714em × 11px ≈ 7.9px —
  // the LSP pill rendered at half the size of the language / cursor text
  // flanking it. A child that wants "the size of this bar" must inherit,
  // not declare a smaller token. Pinned on the one component that lives
  // inside another sized row.
  it("does not nest a second text step inside the status bar", () => {
    const lsp = readFileSync(
      resolve(__dirname, "..", "components", "editor", "LspIndicator.tsx"),
      "utf8",
    );
    // Status pill + circle row (the strip `FileStatusCluster` renders).
    // The install-hint popover below is a separate, self-sized surface.
    const strip = lsp.slice(
      lsp.indexOf("const isUnavailable"),
      lsp.indexOf("/* Install hint popover */"),
    );
    expect(
      strip.match(/className="[^"]*\btext-(9|10|11|xs|sm|base|lg|xl)\b[^"]*"/g),
      "LspIndicator must inherit the status-bar font size",
    ).toBeNull();
  });

  // The micro steps must be declared in @theme, not only in :root.
  // Tailwind v4 generates a `text-*` utility from a @theme token; a
  // :root-only custom property resolves in CSS but emits no class, so
  // every `text-11` in the tree would silently fall back to the
  // inherited size — the bug, wearing a different hat.
  it("declares the micro steps in @theme so Tailwind emits the utility", () => {
    const theme = css.slice(css.indexOf("@theme {"), css.indexOf(":root {"));
    for (const t of ["9", "10", "11"]) {
      expect(theme, `--text-${t} is not in @theme`).toContain(`--text-${t}:`);
    }
  });

  it("scales the card-column caps with the font size", () => {
    // `max-w-lg` / `max-w-2xl` (harness, settings, the right-panel
    // tabs) resolve to `var(--container-lg)` / `var(--container-2xl)`.
    // Tailwind ships those as frozen `32rem` / `42rem`, so a larger font
    // left the column at 512px and every card wrapped into a tall stack.
    for (const t of ["lg", "2xl"]) {
      const m = css.match(new RegExp(`--container-${t}:\\s*([^;]+);`));
      expect(m, `--container-${t} is not declared`).not.toBeNull();
      // Must reference the knob. A bare rem value here is the bug.
      expect(m![1], `--container-${t} must track --ui-font-size`).toContain(
        "--ui-font-size",
      );
      // And must NOT be `em`: this is a length, not a font step, so it
      // has to resolve against the global knob rather than whatever
      // font-size it happens to inherit.
      expect(m![1].trim(), `--container-${t} must not be em`).not.toMatch(/em$/);
    }
  });

  it("leaves the NavBar alone (icon-only, fixed px sizing)", () => {
    // The nav buttons carry no text and size their artwork in fixed units,
    // so the rail does not move when the font size changes: `NAV_ARTWORK_CLASS`
    // is the one 28px tile every icon in the column wears, inside a 40px
    // `NavButton`. Pin that sizing so a future "let's make it em too" edit
    // does not silently start resizing the rail. (The rail used to spell
    // `h-6 w-6` inline per icon; the artwork refactor hoisted it into the
    // constant, which is what this reads now.)
    const nav = readFileSync(
      resolve(__dirname, "..", "components", "layout", "NavBar.tsx"),
      "utf8",
    );
    const artwork = nav.match(/NAV_ARTWORK_CLASS = "([^"]+)"/);
    expect(artwork, "NavBar no longer declares NAV_ARTWORK_CLASS").not.toBeNull();
    expect(artwork![1]).toBe("h-7 w-7");
    expect(nav).not.toMatch(/className="[^"]*text-(9|10|11|xs|sm|base|lg|xl)[^"]*"/);
  });

  // The regression that produced all three of the original reports: a
  // `text-[Npx]` class is invisible to this setting. Scanning the tree
  // is the only way to catch it — nothing fails at build time, the text
  // just quietly stops following the knob.
  //
  // `text-[8px]` is the one sanctioned exception, and it is allow-listed
  // by value: both of its uses are fixed 16px badge boxes where a
  // growing glyph would overflow the box. A NEW arbitrary px size fails
  // this test even though 8px does not, so the exception cannot spread.
  it("has no arbitrary px font sizes outside the 8px badge allow-list", () => {
    const offenders: string[] = [];
    const walk = (dir: string) => {
      for (const e of readdirSync(dir, { withFileTypes: true })) {
        const p = resolve(dir, e.name);
        if (e.isDirectory()) {
          walk(p);
        } else if (/\.tsx?$/.test(e.name) && !/\.test\.tsx?$/.test(e.name)) {
          const src = readFileSync(p, "utf8");
          for (const m of src.matchAll(/text-\[(\d+(?:\.\d+)?)px\]/g)) {
            if (m[1] !== "8") {
              offenders.push(`${p.slice(process.cwd().length + 1)}: text-[${m[1]}px]`);
            }
          }
        }
      }
    };
    walk(resolve(__dirname, "..", "components"));
    expect(
      offenders,
      "arbitrary px font sizes ignore --ui-font-size; use a --text-* token",
    ).toEqual([]);
  });
});
