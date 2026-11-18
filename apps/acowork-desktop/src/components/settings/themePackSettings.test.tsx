/**
 * Settings ▸ Appearance — the theme-pack picker.
 *
 * What this pins:
 *   1. The tab offers the same pack list the title-bar quick menu does
 *      (Default + every styles/themes/*.css file) — one state, two doors.
 *   2. Picking a pack here scopes <html> and persists, exactly like the
 *      title-bar menu does.
 *   3. The doors sync BOTH ways: a pack changed through themePacks (the
 *      title-bar menu's write path) re-renders this tab's checked row
 *      while it stays mounted.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, cleanup, fireEvent, render } from "@testing-library/react";

import { AppearanceTab } from "./SettingsPage";
import {
  DEFAULT_THEME_PACK,
  THEME_PACKS,
  TINTED_PACKS,
  applyThemePack,
  setThemePack,
} from "../../styles/themePacks";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
// SettingsPage.tsx imports these transitively (ProfileTab et al.); the
// appearance tab itself never calls them, but the module graph still loads.
vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: vi.fn() }));

const packInput = (id: string) =>
  document.querySelector(`input[name="themePack"][value="${id}"]`) as HTMLInputElement;

const STORAGE_KEY = "acowork-theme-pack";

describe("Settings ▸ Appearance theme-pack picker", () => {
  beforeEach(() => {
    localStorage.clear();
    applyThemePack(DEFAULT_THEME_PACK);
  });
  afterEach(() => {
    applyThemePack(DEFAULT_THEME_PACK);
    localStorage.removeItem(STORAGE_KEY);
    cleanup();
  });

  it("lists Default plus every discovered pack, Default checked initially", () => {
    render(<AppearanceTab />);
    expect(packInput(DEFAULT_THEME_PACK).checked).toBe(true);
    for (const pack of TINTED_PACKS) {
      expect(packInput(pack.id), `missing row for ${pack.id}`).toBeTruthy();
      expect(packInput(pack.id).checked).toBe(false);
    }
  });

  it("picking a pack scopes <html> and persists, like the quick menu does", () => {
    render(<AppearanceTab />);
    fireEvent.click(packInput("paper"));
    const root = document.documentElement;
    expect(root.classList.contains("theme-paper")).toBe(true);
    expect(root.getAttribute("data-theme-pack")).toBe("paper");
    expect(localStorage.getItem(STORAGE_KEY)).toBe("paper");
    expect(packInput("paper").checked).toBe(true);
    expect(packInput(DEFAULT_THEME_PACK).checked).toBe(false);

    // Default restores the built-in palette and clears the persisted value.
    fireEvent.click(packInput(DEFAULT_THEME_PACK));
    expect(root.classList.contains("theme-paper")).toBe(false);
    expect(localStorage.getItem(STORAGE_KEY)).toBeNull();
  });

  it("re-renders when the OTHER door changes the pack", () => {
    render(<AppearanceTab />);
    // The write path the title-bar AppearanceMenu uses.
    act(() => {
      setThemePack("mist");
    });
    expect(packInput("mist").checked).toBe(true);
    expect(packInput(DEFAULT_THEME_PACK).checked).toBe(false);
    expect(document.documentElement.classList.contains("theme-mist")).toBe(true);
  });

  it("gives every theme-mode row a glyph, like the title-bar menu does", () => {
    render(<AppearanceTab />);
    for (const mode of ["light", "dark", "system"]) {
      const row = document.querySelector(
        `input[name="theme"][value="${mode}"]`,
      )!.closest("label")!;
      expect(row.querySelector("svg"), `missing glyph for ${mode}`).toBeTruthy();
    }
  });

  it("keeps the light/dark mode rows independent of the pack choice", () => {
    render(<AppearanceTab />);
    fireEvent.click(packInput("sage"));
    // Mode radio is a separate group; it must still exist and stay on its
    // current value — switching a palette is not switching a mode.
    const mode = document.querySelector('input[name="theme"]:checked') as HTMLInputElement;
    expect(mode).toBeTruthy();
    expect(mode.value).not.toBe("");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
  });
});
