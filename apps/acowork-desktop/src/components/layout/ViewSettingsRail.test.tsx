/**
 * ViewSettingsRail smoke — the shared 40px gear rail used by the pm / docs
 * views (and, via GroupRightNavBar, group chat).
 *
 * Pins the two behaviors the panels rely on:
 *   1. exactly one tile, clicks forwarded to `onToggle` (open/close cycle);
 *   2. the gear swaps outline → filled when `expanded` flips — the same
 *      Filled/Outline SettingsIcon contract `RightNavBar` uses, distinguish-
 *      able in jsdom via the svg `fill` attribute (no layout involved).
 */
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));
// Tooltip renders through a portal with a hover delay — pass-through is the
// established pattern (see AgentChatView.test.tsx).
vi.mock("../common/Tooltip", () => ({
  Tooltip: ({ children }: { children?: React.ReactNode }) => <>{children}</>,
}));

import { ViewSettingsRail } from "./ViewSettingsRail";

describe("ViewSettingsRail", () => {
  it("renders a single gear tile and forwards clicks to onToggle", () => {
    const onToggle = vi.fn();
    render(
      <ViewSettingsRail
        expanded={false}
        onToggle={onToggle}
        tooltipKey="pm.settingsPanel.title"
      />,
    );
    const buttons = screen.getAllByRole("button");
    expect(buttons).toHaveLength(1);
    fireEvent.click(buttons[0]);
    expect(onToggle).toHaveBeenCalledTimes(1);
  });

  it("reads outline when collapsed and filled when expanded", () => {
    const { container, rerender } = render(
      <ViewSettingsRail expanded={false} onToggle={vi.fn()} tooltipKey="doc.settingsPanel.title" />,
    );
    expect(container.querySelector("svg")?.getAttribute("fill")).toBe("none");

    rerender(
      <ViewSettingsRail expanded onToggle={vi.fn()} tooltipKey="doc.settingsPanel.title" />,
    );
    expect(container.querySelector("svg")?.getAttribute("fill")).toBe("currentColor");
  });
});
