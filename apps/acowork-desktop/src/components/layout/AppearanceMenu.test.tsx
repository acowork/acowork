/**
 * AppearanceMenu — the title-bar appearance quick-settings.
 *
 * What this pins:
 *   1. The trigger opens the menu, and the menu dismisses on Escape /
 *      outside click (the title-bar popover contract).
 *   2. Theme mode and accent color are NOT a second state machine: the
 *      menu reads and writes `settingsStore`, so picking a mode here is
 *      the same write Settings ▸ Appearance makes — and the store
 *      persists it, so the choice survives a restart.
 *   3. The theme actually reaches the DOM (`.dark` on `<html>`) — the
 *      store's own apply step, not a menu-local class swap.
 *   4. The accent actually reaches the DOM (`--color-accent` +
 *      the `accent-{id}` class on `<html>`).
 */
import { describe, it, expect, beforeEach } from "vitest";
import { render, screen, fireEvent, act } from "@testing-library/react";
import { AppearanceMenu } from "./AppearanceMenu";
import { useSettingsStore } from "../../stores/settingsStore";
import {
  DEFAULT_THEME_PACK,
  TINTED_PACKS,
  applyThemePack,
} from "../../styles/themePacks";

/** The trigger button — its accessible name is the localized
 *  "Appearance" label (any of the 5 locales). */
const trigger = () =>
  screen.getByRole("button", { name: /Appearance|外观|外觀|外観|외관/ });

/** A theme-mode row, by its localized label. */
const themeRow = (mode: "light" | "dark" | "system") => {
  const labels = {
    light: /Light|浅色|淺色|ライト|라이트/,
    dark: /Dark|深色|ダーク|다크/,
    system: /System|跟随系统|跟隨系統|システム|시스템/,
  };
  return screen.getByRole("radio", { name: labels[mode] });
};

/** An accent swatch, by its `data-accent-id`. */
const swatch = (id: string) =>
  document.querySelector(`[data-accent-id="${id}"]`) as HTMLButtonElement;

const openMenu = () => {
  render(<AppearanceMenu />);
  fireEvent.click(trigger());
};

describe("AppearanceMenu", () => {
  beforeEach(() => {
    localStorage.clear();
    useSettingsStore.setState({
      theme: "system",
      osTheme: "light",
      accentColor: "#3b82f6",
    });
    document.documentElement.classList.remove("dark");
  });

  it("the trigger opens the menu", () => {
    render(<AppearanceMenu />);
    expect(screen.queryByRole("radio")).toBeNull();
    fireEvent.click(trigger());
    // 3 theme-mode rows + the theme-pack rows (Default + every pack file).
    expect(screen.getAllByRole("radio").length).toBe(3 + 1 + TINTED_PACKS.length);
  });

  it("closes on Escape", () => {
    openMenu();
    expect(screen.getAllByRole("radio").length).toBe(3 + 1 + TINTED_PACKS.length);
    act(() => {
      fireEvent.keyDown(document, { key: "Escape" });
    });
    expect(screen.queryByRole("radio")).toBeNull();
  });

  it("closes on an outside click", () => {
    openMenu();
    fireEvent.mouseDown(document.body);
    expect(screen.queryByRole("radio")).toBeNull();
  });

  it("picking a theme mode writes the store, the DOM and the persisted value", () => {
    openMenu();
    fireEvent.click(themeRow("dark"));

    // Store — the same field Settings ▸ Appearance drives, so the two
    // entry points cannot drift apart.
    expect(useSettingsStore.getState().theme).toBe("dark");
    // DOM — the `.dark` class is what actually flips the app's theme.
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    // Persisted — the choice must survive a restart.
    expect(localStorage.getItem("acowork-theme")).toBe("dark");
  });

  it("picking light clears the dark class and persists", () => {
    useSettingsStore.setState({ theme: "dark" });
    act(() => {
      document.documentElement.classList.add("dark");
    });
    openMenu();
    fireEvent.click(themeRow("light"));

    expect(useSettingsStore.getState().theme).toBe("light");
    expect(document.documentElement.classList.contains("dark")).toBe(false);
    expect(localStorage.getItem("acowork-theme")).toBe("light");
  });

  it("picking system resolves through the OS theme and persists the CHOICE", () => {
    // The OS is dark, so "system" must render dark...
    useSettingsStore.setState({ theme: "light", osTheme: "dark" });
    openMenu();
    fireEvent.click(themeRow("system"));

    expect(useSettingsStore.getState().theme).toBe("system");
    expect(document.documentElement.classList.contains("dark")).toBe(true);
    // ...but the persisted value is the CHOICE ("system"), not the
    // resolved theme — otherwise the app would forget to follow the OS.
    expect(localStorage.getItem("acowork-theme")).toBe("system");
  });

  it("the active theme mode is the checked row", () => {
    useSettingsStore.setState({ theme: "dark" });
    openMenu();
    expect(themeRow("dark").getAttribute("aria-checked")).toBe("true");
    expect(themeRow("light").getAttribute("aria-checked")).toBe("false");
  });

  it("picking an accent color writes the store, the DOM and the persisted value", () => {
    openMenu();
    fireEvent.click(swatch("green"));

    expect(useSettingsStore.getState().accentColor).toBe("#00C375");
    // Shipped presets set NO inline value — `:root.accent-green` supplies the
    // token so a theme pack can still override it (inline style would outrank
    // every stylesheet).
    expect(
      document.documentElement.style.getPropertyValue("--color-accent"),
    ).toBe("");
    // The `accent-{id}` class selects the per-accent block.
    expect(document.documentElement.classList.contains("accent-green")).toBe(true);
    expect(localStorage.getItem("acowork-accent-color")).toBe("#00C375");
  });

  it("the selected accent swatch is the pressed one", () => {
    useSettingsStore.setState({ accentColor: "#f43f5e" });
    openMenu();
    expect(swatch("rose").getAttribute("aria-pressed")).toBe("true");
    expect(swatch("green").getAttribute("aria-pressed")).toBe("false");
  });
});

describe("AppearanceMenu theme packs", () => {
  beforeEach(() => {
    localStorage.clear();
    applyThemePack(DEFAULT_THEME_PACK);
    useSettingsStore.setState({ theme: "system", osTheme: "light" });
    document.documentElement.classList.remove("dark");
  });

  /** A theme-pack row: "Paper" by name, or the localized Default row. */
  const packRow = (name: string | RegExp) => screen.getByRole("radio", { name });
  const defaultRow = () => packRow(/Default|默认|預設|デフォルト|기본값/);

  it("picking a pack scopes <html>, persists, and Default restores", () => {
    openMenu();
    fireEvent.click(packRow("Paper"));
    const root = document.documentElement;
    expect(root.classList.contains("theme-paper")).toBe(true);
    expect(root.getAttribute("data-theme-pack")).toBe("paper");
    expect(localStorage.getItem("acowork-theme-pack")).toBe("paper");
    expect(packRow("Paper").getAttribute("aria-checked")).toBe("true");
    expect(defaultRow().getAttribute("aria-checked")).toBe("false");

    fireEvent.click(defaultRow());
    expect(root.classList.contains("theme-paper")).toBe(false);
    // Default is a pack like the others: it activates its own scope.
    expect(root.classList.contains("theme-default")).toBe(true);
    expect(root.getAttribute("data-theme-pack")).toBe("default");
    expect(localStorage.getItem("acowork-theme-pack")).toBeNull();
    expect(defaultRow().getAttribute("aria-checked")).toBe("true");
  });

  it("switching a pack leaves the light/dark mode untouched", () => {
    // Go through the action (not bare setState) so applyTheme touches the DOM.
    useSettingsStore.getState().setTheme("dark");
    openMenu();
    fireEvent.click(packRow("Paper"));
    const root = document.documentElement;
    expect(root.classList.contains("dark")).toBe(true);
    expect(root.classList.contains("theme-paper")).toBe(true);
  });

  it("lists every pack discovered from styles/themes/", () => {
    openMenu();
    // The reference pack ships with the repo; a new theme file that forgets
    // to register itself fails right here.
    expect(() => packRow("Paper")).not.toThrow();
  });
});
