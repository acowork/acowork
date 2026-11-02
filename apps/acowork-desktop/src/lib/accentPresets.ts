/**
 * Accent color presets — shared by SettingsPage, settingsStore, AppLayout.
 *
 * Each preset defines:
 *   - `id`    : short identifier used as the CSS class `accent-{id}` on `<html>`
 *               for tint lookup and as the persistence key in localStorage
 *   - `hex`   : the canonical accent color (used for buttons, links, etc.)
 *   - `hue`   : HSL hue (0-360) — drives the glass-tint hue in globals.css
 *   - `glassTintLight` / `glassTintDark`
 *             : CSS HSL components (without hsl() wrapper) used as the
 *               `--glass-tint-light` / `--glass-tint-dark` values when this
 *               accent is active. Saturation is 8%, lightness is 92% (light)
 *               / 14% (dark) — the color is intentionally almost neutral
 *               with only a faint hue shift, providing a "color anchor"
 *               that keeps the frosted-glass surface distinguishable from
 *               pure white/black desktop wallpaper without ever looking
 *               overtly colored.
 *   - `glassRgbLight` / `glassRgbDark`
 *             : pre-computed RGB equivalent of `glassTintLight/Dark`,
 *               passed to the macOS native `set_window_effect` command so
 *               that the NSVisualEffectView tint also picks up the hue
 *               shift (the CSS tint layer alone is not enough at opacity=0
 *               because the native vibrancy layer dominates).
 *
 * Saturation/lightness is fixed at 8%/92% (light) and 8%/14% (dark) so
 * the tints remain consistent across all hues. If you want to adjust the
 * perceived intensity, change the percentages here and re-run the
 * `hslRgb()` script in the commit message that introduced this file.
 */

export interface AccentPreset {
  id: string;
  hex: string;
  hue: number;
  /** HSL components (e.g. "217 8% 92%") — bound to `--glass-tint-light`. */
  glassTintLight: string;
  /** HSL components (e.g. "217 8% 14%") — bound to `--glass-tint-dark`. */
  glassTintDark: string;
  /** RGB equivalent for macOS native tint (light mode). */
  glassRgbLight: { r: number; g: number; b: number };
  /** RGB equivalent for macOS native tint (dark mode). */
  glassRgbDark: { r: number; g: number; b: number };
}

export const ACCENT_PRESETS: AccentPreset[] = [
  {
    id: "blue",
    hex: "#3b82f6",
    hue: 217,
    glassTintLight: "217 8% 92%",
    glassTintDark: "217 8% 14%",
    glassRgbLight: { r: 233, g: 234, b: 236 },
    glassRgbDark: { r: 33, g: 35, b: 39 },
  },
  {
    id: "indigo",
    hex: "#6366f1",
    hue: 243,
    glassTintLight: "243 8% 92%",
    glassTintDark: "243 8% 14%",
    glassRgbLight: { r: 233, g: 233, b: 236 },
    glassRgbDark: { r: 33, g: 33, b: 39 },
  },
  {
    id: "violet",
    hex: "#8b5cf6",
    hue: 258,
    glassTintLight: "258 8% 92%",
    glassTintDark: "258 8% 14%",
    glassRgbLight: { r: 234, g: 233, b: 236 },
    glassRgbDark: { r: 35, g: 33, b: 39 },
  },
  {
    id: "cyan",
    hex: "#06b6d4",
    hue: 189,
    glassTintLight: "189 8% 92%",
    glassTintDark: "189 8% 14%",
    glassRgbLight: { r: 233, g: 236, b: 236 },
    glassRgbDark: { r: 33, g: 38, b: 39 },
  },
  {
    id: "teal",
    hex: "#14b8a6",
    hue: 173,
    glassTintLight: "173 8% 92%",
    glassTintDark: "173 8% 14%",
    glassRgbLight: { r: 233, g: 236, b: 236 },
    glassRgbDark: { r: 33, g: 39, b: 38 },
  },
  {
    id: "green",
    hex: "#00C375",
    hue: 158,
    glassTintLight: "158 8% 92%",
    glassTintDark: "158 8% 14%",
    glassRgbLight: { r: 233, g: 236, b: 235 },
    glassRgbDark: { r: 33, g: 39, b: 36 },
  },
  {
    id: "rose",
    hex: "#f43f5e",
    hue: 350,
    glassTintLight: "350 8% 92%",
    glassTintDark: "350 8% 14%",
    glassRgbLight: { r: 236, g: 233, b: 234 },
    glassRgbDark: { r: 39, g: 33, b: 34 },
  },
  {
    id: "orange",
    hex: "#f97316",
    hue: 24,
    glassTintLight: "24 8% 92%",
    glassTintDark: "24 8% 14%",
    glassRgbLight: { r: 236, g: 234, b: 233 },
    glassRgbDark: { r: 39, g: 35, b: 33 },
  },
  {
    id: "amber",
    hex: "#f59e0b",
    hue: 38,
    glassTintLight: "38 8% 92%",
    glassTintDark: "38 8% 14%",
    glassRgbLight: { r: 236, g: 235, b: 233 },
    glassRgbDark: { r: 39, g: 36, b: 33 },
  },
  {
    id: "pink",
    hex: "#ec4899",
    hue: 330,
    glassTintLight: "330 8% 92%",
    glassTintDark: "330 8% 14%",
    glassRgbLight: { r: 236, g: 233, b: 235 },
    glassRgbDark: { r: 39, g: 33, b: 36 },
  },
];

/** Lookup an AccentPreset by its id (e.g. "blue"). Returns undefined if missing. */
export function getAccentPresetById(id: string): AccentPreset | undefined {
  return ACCENT_PRESETS.find((p) => p.id === id);
}

/** Lookup an AccentPreset by its hex value (case-insensitive). Returns undefined if missing. */
export function getAccentPresetByHex(hex: string): AccentPreset | undefined {
  const normalized = hex.toLowerCase();
  return ACCENT_PRESETS.find((p) => p.hex.toLowerCase() === normalized);
}

/** Default preset (first one — "blue"). Used as fallback when localStorage is empty
 *  or contains an unknown hex value (e.g. a future preset that was removed). */
export const DEFAULT_ACCENT_PRESET: AccentPreset = ACCENT_PRESETS[0];

/** `id` of a synthesized preset. Not a CSS class target: globals.css only
 *  ships `.accent-{id}` blocks for the ten shipped presets, so a custom
 *  accent gets its glass tints written inline on `<html>` instead
 *  (see `settingsStore.applyAccentPreset`). */
export const CUSTOM_ACCENT_ID = "custom";

/** sRGB hex → HSL hue in degrees (0-360). Achromatic input (grey/white/black)
 *  has no hue by definition; 0 is returned, which renders a neutral tint —
 *  the same thing the accent-agnostic fallback in globals.css does. */
export function hexToHue(hex: string): number {
  const { r, g, b } = hexToRgb(hex);
  const rn = r / 255, gn = g / 255, bn = b / 255;
  const max = Math.max(rn, gn, bn);
  const min = Math.min(rn, gn, bn);
  const d = max - min;
  if (d === 0) return 0;
  let h: number;
  if (max === rn) h = ((gn - bn) / d) % 6;
  else if (max === gn) h = (bn - rn) / d + 2;
  else h = (rn - gn) / d + 4;
  return Math.round((h * 60 + 360) % 360);
}

function hexToRgb(hex: string): { r: number; g: number; b: number } {
  const n = parseInt(hex.replace(/^#/, ""), 16);
  return { r: (n >> 16) & 255, g: (n >> 8) & 255, b: n & 255 };
}

/** HSL(hue, 8%, lightness) → RGB, matching the fixed 8% saturation every
 *  preset uses. Mirrors the `hslRgb()` script noted in the header comment. */
function hslToRgb(hue: number, lightness: number): { r: number; g: number; b: number } {
  const s = 0.08;
  const c = (1 - Math.abs(2 * lightness - 1)) * s;
  const x = c * (1 - Math.abs(((hue / 60) % 2) - 1));
  const m = lightness - c / 2;
  const seg = [
    [c, x, 0], [x, c, 0], [0, c, x], [0, x, c], [x, 0, c], [c, 0, x],
  ][Math.floor((hue / 60) % 6)];
  return {
    r: Math.round((seg[0] + m) * 255),
    g: Math.round((seg[1] + m) * 255),
    b: Math.round((seg[2] + m) * 255),
  };
}

/**
 * Synthesize an `AccentPreset` for an arbitrary hex, so a user-picked color
 * drives the same three layers a shipped preset does: `--color-accent`, the
 * CSS glass tints, and the macOS native vibrancy RGB.
 *
 * The tints are derived, not stored — the presets hard-code exactly
 * `H 8% 92%` / `H 8% 14%` and the RGB equivalent of the same, so reproducing
 * that formula keeps a custom accent visually consistent with the shipped
 * ten rather than inventing a second tint model.
 *
 * Note the shipped `hue` field is hand-tuned for three presets (indigo 243
 * vs the true 239, green 158 vs 156, orange 24 vs 25). The derived hue here
 * is the true one; the difference is invisible at 8% saturation — the RGB
 * triples come out identical to the authored ones for all ten. Presets are
 * matched by hex first (see `resolveAccentPreset`), so their authored hue
 * still wins when one is picked.
 */
export function presetFromHex(hex: string): AccentPreset {
  const hue = hexToHue(hex);
  return {
    id: CUSTOM_ACCENT_ID,
    hex,
    hue,
    glassTintLight: `${hue} 8% 92%`,
    glassTintDark: `${hue} 8% 14%`,
    glassRgbLight: hslToRgb(hue, 0.92),
    glassRgbDark: hslToRgb(hue, 0.14),
  };
}

/** Resolve any accent hex to a preset: a shipped one if it matches, otherwise
 *  a synthesized custom preset. This is what lets `accentColor` hold a color
 *  outside the ten presets without being coerced back to blue. */
export function resolveAccentPreset(hex: string): AccentPreset {
  return getAccentPresetByHex(hex) ?? presetFromHex(hex);
}

/** True when `hex` is not one of the shipped presets. */
export function isCustomAccent(hex: string): boolean {
  return getAccentPresetByHex(hex) === undefined;
}