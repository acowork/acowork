/**
 * accentPresets — the custom-color derivation.
 *
 * A user-picked accent has to drive the same three layers a shipped preset
 * does: `--color-accent`, the CSS glass tints, and the macOS native vibrancy
 * RGB. Only the last is a computation the shipped presets hide — they hard-code
 * it — so this pins that the formula reproduces their values exactly. If it
 * drifts, a custom color tints the glass differently from the ten swatches
 * beside it, which is the one thing a synthesized preset must not do.
 */
import { describe, it, expect } from "vitest";
import {
  ACCENT_PRESETS,
  CUSTOM_ACCENT_ID,
  hexToHue,
  isCustomAccent,
  presetFromHex,
  resolveAccentPreset,
} from "./accentPresets";

describe("presetFromHex", () => {
  it("reproduces every shipped preset's native vibrancy RGB from its hex", () => {
    // The RGB triples are what the macOS `set_window_effect` tint consumes,
    // and they are the only derived part of a preset — so this is the formula
    // a custom color has to get right.
    for (const p of ACCENT_PRESETS) {
      const s = presetFromHex(p.hex);
      expect(s.glassRgbLight, p.id).toEqual(p.glassRgbLight);
      expect(s.glassRgbDark, p.id).toEqual(p.glassRgbDark);
    }
  });

  it("follows the same tint formula the shipped presets use", () => {
    // Not an exact hue match: three presets hand-tune `hue` a few degrees
    // away from the true HSL hue of their hex (indigo 243 vs 239). The RGB
    // above proves the formula is right; this pins the shape.
    for (const p of ACCENT_PRESETS) {
      const s = presetFromHex(p.hex);
      expect(s.glassTintLight, p.id).toBe(`${s.hue} 8% 92%`);
      expect(s.glassTintDark, p.id).toBe(`${s.hue} 8% 14%`);
      // A few degrees of hue at 8% saturation is invisible — the rounded RGB
      // is identical either way.
      expect(Math.abs(s.hue - p.hue), p.id).toBeLessThanOrEqual(4);
    }
  });
});

describe("resolveAccentPreset", () => {
  it("returns the authored preset for a shipped hex", () => {
    const p = ACCENT_PRESETS[3];
    const r = resolveAccentPreset(p.hex);
    expect(r.id).toBe(p.id);
    // Authored hue wins over the computed one — three presets hand-tune it a
    // few degrees away from the exact HSL hue of their hex.
    expect(r.hue).toBe(p.hue);
  });

  it("synthesizes a custom preset for a hex outside the ten", () => {
    const r = resolveAccentPreset("#d4228a");
    expect(r.id).toBe(CUSTOM_ACCENT_ID);
    expect(r.hex).toBe("#d4228a");
    expect(r.hue).toBe(hexToHue("#d4228a"));
    expect(r.glassTintLight).toBe(`${r.hue} 8% 92%`);
    expect(r.glassTintDark).toBe(`${r.hue} 8% 14%`);
  });

  it("is case-insensitive about a shipped hex", () => {
    // The store persists whatever the source handed it; `#00C375` is green's
    // authored casing, and a picker emits lowercase.
    expect(resolveAccentPreset("#00c375").id).toBe("green");
  });

  it("flags only non-preset colors as custom", () => {
    expect(isCustomAccent(ACCENT_PRESETS[0].hex)).toBe(false);
    expect(isCustomAccent("#123456")).toBe(true);
  });

  it("treats an achromatic color as a neutral tint rather than throwing", () => {
    const r = resolveAccentPreset("#808080");
    expect(r.hue).toBe(0);
    expect(r.glassTintLight).toBe("0 8% 92%");
  });
});
