/**
 * The title-bar appearance menu and Settings ▸ Appearance must be two
 * doors to ONE state, not two states that happen to look alike.
 *
 * The failure this pins: the menu keeps its own `useState` for the
 * theme and the accent, so a user picks "dark" in the menu, walks into
 * Settings and finds the tab still saying "system" — and vice versa,
 * a change made in Settings never reaches the menu. Two writers, one
 * subject, and whichever wrote last wins; the app looks like it is
 * fighting itself.
 *
 * The contract: both entry points read and write `settingsStore`, so a
 * write through either is immediately visible in the other.
 */
import { describe, it, expect, beforeEach } from "vitest";
import { render, screen, fireEvent } from "@testing-library/react";
import { AppearanceMenu } from "./AppearanceMenu";
import { useSettingsStore } from "../../stores/settingsStore";

const trigger = () =>
  screen.getByRole("button", { name: /Appearance|外观|外觀|外観|외관/ });

const themeRow = (mode: "light" | "dark" | "system") => {
  const labels = {
    light: /Light|浅色|淺色|ライト|라이트/,
    dark: /Dark|深色|ダーク|다크/,
    system: /System|跟随系统|跟隨系統|システム|시스템/,
  };
  return screen.getByRole("radio", { name: labels[mode] });
};

const swatch = (id: string) =>
  document.querySelector(`[data-accent-id="${id}"]`) as HTMLButtonElement;

describe("AppearanceMenu ⇄ settingsStore sync", () => {
  beforeEach(() => {
    localStorage.clear();
    useSettingsStore.setState({
      theme: "system",
      osTheme: "light",
      accentColor: "#3b82f6",
    });
    document.documentElement.classList.remove("dark");
  });

  it("a mode picked in the menu is the mode the store holds (what Settings reads)", () => {
    render(<AppearanceMenu />);
    fireEvent.click(trigger());
    fireEvent.click(themeRow("dark"));

    // This is the exact field Settings ▸ Appearance binds its RadioGroup
    // to — one write, both doors see it.
    expect(useSettingsStore.getState().theme).toBe("dark");
  });

  it("a mode changed in the store shows up as the checked row in the menu", () => {
    // Simulates the OTHER writer: the Settings tab (or a store subscriber)
    // changes the theme; the menu must reflect it without a reload.
    useSettingsStore.setState({ theme: "light" });
    render(<AppearanceMenu />);
    fireEvent.click(trigger());

    expect(themeRow("light").getAttribute("aria-checked")).toBe("true");
    expect(themeRow("dark").getAttribute("aria-checked")).toBe("false");
  });

  it("an accent picked in the menu is the accent the store holds", () => {
    render(<AppearanceMenu />);
    fireEvent.click(trigger());
    fireEvent.click(swatch("violet"));

    expect(useSettingsStore.getState().accentColor).toBe("#8b5cf6");
  });

  it("an accent changed in the store shows up as the pressed swatch", () => {
    useSettingsStore.setState({ accentColor: "#14b8a6" });
    render(<AppearanceMenu />);
    fireEvent.click(trigger());

    expect(swatch("teal").getAttribute("aria-pressed")).toBe("true");
    expect(swatch("blue").getAttribute("aria-pressed")).toBe("false");
  });
});
