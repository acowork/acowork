/**
 * AccentSwatches — the ten presets plus a custom color.
 *
 * Shared by the title-bar menu and Settings ▸ Appearance, so this pins the
 * control itself rather than either host. What is worth pinning is the custom
 * path, which the preset path never exercised:
 *   1. The trailing "+" opens the native picker and a picked color reaches
 *      the store, persisted.
 *   2. A custom color is not coerced back to a preset — it drives
 *      `--color-accent` AND the glass tints, which have no `.accent-{id}`
 *      CSS rule to select, so they must be written inline.
 *   3. Switching back to a preset must clear those inline tints, or the glass
 *      keeps the custom hue while the swatch says otherwise.
 */
import { describe, it, expect, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { AccentSwatches } from "./AccentSwatches";
import { useSettingsStore } from "../../stores/settingsStore";
import { ACCENT_PRESETS } from "../../lib/accentPresets";

const CUSTOM = "#d4228a";

const root = () => document.documentElement;
const customBtn = () =>
  screen.getByRole("button", { name: /Custom|自定义|自訂|カスタム|사용자 지정/ });
/** The hidden native picker the "+" button drives. */
const picker = () =>
  document.querySelector('input[type="color"]') as HTMLInputElement;

function setAccent(hex: string) {
  useSettingsStore.getState().setAccentColor(hex);
}

describe("AccentSwatches", () => {
  beforeEach(() => {
    setAccent(ACCENT_PRESETS[0].hex);
    localStorage.removeItem("acowork-accent-color");
  });

  it("renders the ten presets plus one custom slot", () => {
    render(<AccentSwatches value={ACCENT_PRESETS[0].hex} onChange={() => {}} />);
    expect(document.querySelectorAll("[data-accent-id]")).toHaveLength(ACCENT_PRESETS.length + 1);
  });

  it("calls onChange with the color picked from the native palette", () => {
    const seen: string[] = [];
    render(<AccentSwatches value={ACCENT_PRESETS[0].hex} onChange={(h) => seen.push(h)} />);
    fireEvent.click(customBtn());
    fireEvent.change(picker(), { target: { value: CUSTOM } });
    expect(seen).toEqual([CUSTOM]);
  });

  it("marks the custom slot pressed only when the color is outside the presets", () => {
    render(<AccentSwatches value={ACCENT_PRESETS[0].hex} onChange={() => {}} />);
    expect(customBtn().getAttribute("aria-pressed")).toBe("false");
    // Same host, a custom value: the slot becomes the current-color swatch.
    render(<AccentSwatches value={CUSTOM} onChange={() => {}} />);
    const slots = screen.getAllByRole("button", { name: /Custom|自定义|自訂|カスタム|사용자 지정/ });
    expect(slots[slots.length - 1].getAttribute("aria-pressed")).toBe("true");
  });

  it("opens the native picker at the color that is actually active", () => {
    render(<AccentSwatches value={CUSTOM} onChange={() => {}} />);
    expect(picker().value).toBe(CUSTOM);
  });

  it("lowercases the picker value so an authored uppercase hex is not sanitized to black", () => {
    // `<input type="color">` only accepts a *valid simple color* — lowercase
    // hex — and rewrites anything else to #000000. Green's authored hex is
    // "#00C375", so without normalizing, picking green would open the palette
    // on black.
    const green = ACCENT_PRESETS.find((p) => p.id === "green")!;
    expect(green.hex).toBe("#00C375");
    render(<AccentSwatches value={green.hex} onChange={() => {}} />);
    expect(picker().value).toBe("#00c375");
  });
});

describe("custom accent reaches the DOM", () => {
  beforeEach(() => setAccent(ACCENT_PRESETS[0].hex));

  it("sets --color-accent, the custom class, and inline glass tints", () => {
    setAccent(CUSTOM);
    expect(root().style.getPropertyValue("--color-accent")).toBe(CUSTOM);
    expect(root().classList).toContain("accent-custom");
    // No `.accent-custom` rule exists in globals.css, so these must be inline.
    expect(root().style.getPropertyValue("--glass-tint-light")).toMatch(/^(\d+) 8% 92%$/);
    expect(root().style.getPropertyValue("--glass-tint-dark")).toMatch(/^(\d+) 8% 14%$/);
  });

  it("persists a custom color and does not coerce it to a preset", () => {
    setAccent(CUSTOM);
    expect(useSettingsStore.getState().accentColor).toBe(CUSTOM);
    expect(localStorage.getItem("acowork-accent-color")).toBe(CUSTOM);
  });

  it("rejects a malformed hex instead of writing it into the CSS variable", () => {
    // The store's contract is that `accentColor` is always `#rrggbb`; the
    // picker can only emit that, but a caller must not be able to break it.
    // Starting from a valid custom color so a no-op write would fail this.
    setAccent(CUSTOM);
    setAccent("red");
    expect(useSettingsStore.getState().accentColor).toBe(ACCENT_PRESETS[0].hex);
    expect(root().style.getPropertyValue("--color-accent")).toBe(ACCENT_PRESETS[0].hex);
    expect(localStorage.getItem("acowork-accent-color")).toBe(ACCENT_PRESETS[0].hex);
  });

  it("clears the inline tints when switching back to a shipped preset", () => {
    // Otherwise an inline value left from the custom pick outranks the
    // `.accent-{id}` rule and the glass keeps the abandoned hue.
    setAccent(CUSTOM);
    setAccent(ACCENT_PRESETS[2].hex);
    expect(root().style.getPropertyValue("--glass-tint-light")).toBe("");
    expect(root().style.getPropertyValue("--glass-tint-dark")).toBe("");
    expect(root().classList).toContain("accent-violet");
    expect(root().classList).not.toContain("accent-custom");
  });
});
