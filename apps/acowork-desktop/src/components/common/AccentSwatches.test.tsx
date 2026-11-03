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
 *   4. The saved custom color survives selecting a preset, and survives a
 *      restart — losing it that way was a bug, and it is why `customColor` is
 *      a separate prop from `value` and a separate key from `accentColor`.
 */
import { describe, it, expect, beforeEach, vi } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { AccentSwatches } from "./AccentSwatches";
import { useSettingsStore } from "../../stores/settingsStore";
import { ACCENT_PRESETS } from "../../lib/accentPresets";

const CUSTOM = "#d4228a";
const BLUE = ACCENT_PRESETS[0].hex;

const root = () => document.documentElement;
const customBtn = () =>
  screen.getByRole("button", { name: /Custom|自定义|自訂|カスタム|사용자 지정/ });
/** The hidden native picker the "+" button drives. */
const picker = () =>
  document.querySelector('input[type="color"]') as HTMLInputElement;
const badge = () => document.querySelector("[data-accent-edit-badge]");

function setAccent(hex: string) {
  useSettingsStore.getState().setAccentColor(hex);
}

describe("AccentSwatches", () => {
  beforeEach(() => {
    useSettingsStore.setState({ customAccent: null });
    setAccent(BLUE);
  });

  it("renders the ten presets plus one custom slot", () => {
    render(<AccentSwatches value={BLUE} customColor={null} onChange={() => {}} />);
    expect(document.querySelectorAll("[data-accent-id]")).toHaveLength(ACCENT_PRESETS.length + 1);
  });

  it("calls onChange with the color picked from the native palette", () => {
    const seen: string[] = [];
    render(<AccentSwatches value={BLUE} customColor={null} onChange={(h) => seen.push(h)} />);
    fireEvent.click(customBtn());
    fireEvent.change(picker(), { target: { value: CUSTOM } });
    expect(seen).toEqual([CUSTOM]);
  });

  it("selects the saved custom color on click, before any color is picked", () => {
    // "Switch to my custom color" is a complete action; it must not require
    // re-picking a color. Closing the native picker without touching it fires
    // no `change`, so the commit has to happen on the click itself.
    const seen: string[] = [];
    render(<AccentSwatches value={BLUE} customColor={CUSTOM} onChange={(h) => seen.push(h)} />);
    fireEvent.click(customBtn());
    expect(seen).toEqual([CUSTOM]);
  });

  it("shows a plus when there is no custom color, an edit badge when there is", () => {
    // Without the badge a saved custom color is just another plain circle,
    // indistinguishable from the ten presets beside it — and the "+" the user
    // clicked is gone, so nothing says the slot is still clickable.
    const { unmount } = render(
      <AccentSwatches value={BLUE} customColor={null} onChange={() => {}} />,
    );
    expect(badge()).toBeNull();
    // The plus glyph is the affordance in this state (an <svg>, so it carries
    // no text — assert the element, not textContent).
    expect(customBtn().querySelector("svg")).not.toBeNull();

    unmount();
    render(<AccentSwatches value={BLUE} customColor={CUSTOM} onChange={() => {}} />);
    expect(badge()).not.toBeNull();
    expect(badge()?.querySelector("svg")).not.toBeNull();
  });

  it("paints the slot with the saved custom color, not the active accent", () => {
    // While a preset is active the slot must still show the user's own color.
    // Painting `value` would render the active preset a second time and make
    // the custom color look destroyed.
    render(<AccentSwatches value={BLUE} customColor={CUSTOM} onChange={() => {}} />);
    expect(customBtn().style.backgroundColor).toBe("rgb(212, 34, 138)");
  });

  it("rings the custom slot only when the custom color is the active accent", () => {
    // `aria-pressed` answers "is this the current color", which is a
    // different question from "does the user have one".
    const { unmount } = render(
      <AccentSwatches value={BLUE} customColor={CUSTOM} onChange={() => {}} />,
    );
    expect(customBtn().getAttribute("aria-pressed")).toBe("false");
    unmount();
    render(<AccentSwatches value={CUSTOM} customColor={CUSTOM} onChange={() => {}} />);
    expect(customBtn().getAttribute("aria-pressed")).toBe("true");
  });

  it("re-opens the picker at the saved custom color while a preset is active", () => {
    // The user is editing *that* color, so the palette starts there rather
    // than on the active preset or on black.
    render(<AccentSwatches value={BLUE} customColor={CUSTOM} onChange={() => {}} />);
    expect(picker().value).toBe(CUSTOM);
  });

  it("lowercases the picker value so an authored uppercase hex is not sanitized to black", () => {
    // `<input type="color">` only accepts a *valid simple color* — lowercase
    // hex — and rewrites anything else to #000000. Green's authored hex is
    // "#00C375", so without normalizing, picking green would open the palette
    // on black.
    const green = ACCENT_PRESETS.find((p) => p.id === "green")!;
    expect(green.hex).toBe("#00C375");
    render(<AccentSwatches value={green.hex} customColor={null} onChange={() => {}} />);
    expect(picker().value).toBe("#00c375");
  });
});

describe("custom accent persistence and DOM", () => {
  beforeEach(() => {
    // These cases assert on storage, so the keys must start empty — an
    // earlier test's write would otherwise satisfy or poison the assertion.
    localStorage.removeItem("acowork-accent-custom");
    localStorage.removeItem("acowork-accent-color");
    useSettingsStore.setState({ customAccent: null });
    setAccent(BLUE);
  });

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

  it("keeps the custom color when a shipped preset is selected afterwards", () => {
    // The regression this exists for: selecting a preset used to drop the
    // custom color and force a re-mix.
    setAccent(CUSTOM);
    setAccent(ACCENT_PRESETS[5].hex);
    expect(useSettingsStore.getState().accentColor).toBe(ACCENT_PRESETS[5].hex);
    expect(useSettingsStore.getState().customAccent).toBe(CUSTOM);
    expect(localStorage.getItem("acowork-accent-custom")).toBe(CUSTOM);
  });

  it("survives a restart: the stored custom color is read back on init", async () => {
    setAccent(CUSTOM);
    setAccent(BLUE);
    expect(localStorage.getItem("acowork-accent-custom")).toBe(CUSTOM);

    // A fresh module graph stands in for a page reload.
    vi.resetModules();
    const { useSettingsStore: reloaded } = await import("../../stores/settingsStore");
    expect(reloaded.getState().customAccent).toBe(CUSTOM);
    // The active accent is the preset, so the two really are independent.
    expect(reloaded.getState().accentColor).toBe(BLUE);
  });

  it("does not record a shipped preset as a custom color", () => {
    setAccent(ACCENT_PRESETS[2].hex);
    expect(useSettingsStore.getState().customAccent).toBeNull();
    expect(localStorage.getItem("acowork-accent-custom")).toBeNull();
  });

  it("rejects a malformed hex instead of writing it into the CSS variable", () => {
    // The store's contract is that `accentColor` is always `#rrggbb`; the
    // picker can only emit that, but a caller must not be able to break it.
    // Starting from a valid custom color so a no-op write would fail this.
    setAccent(CUSTOM);
    setAccent("red");
    expect(useSettingsStore.getState().accentColor).toBe(BLUE);
    expect(root().style.getPropertyValue("--color-accent")).toBe(BLUE);
    expect(localStorage.getItem("acowork-accent-color")).toBe(BLUE);
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
