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

describe("hover drawer wiring", () => {
  it("reports pointer-enter on the gear and pointer-leave on the rail", () => {
    const onHoverStart = vi.fn();
    const onHoverEnd = vi.fn();
    const { container } = render(
      <ViewSettingsRail
        expanded={false}
        onToggle={vi.fn()}
        tooltipKey="pm.settingsPanel.title"
        onHoverStart={onHoverStart}
        onHoverEnd={onHoverEnd}
      />,
    );
    fireEvent.mouseEnter(screen.getAllByRole("button")[0]);
    expect(onHoverStart).toHaveBeenCalledTimes(1);

    fireEvent.mouseLeave(container.querySelector("nav")!);
    expect(onHoverEnd).toHaveBeenCalledTimes(1);
  });

  it("reads filled for a floating preview, the same as for a pinned panel", () => {
    // The user cannot tell a peek from a pin at a glance; the rail must not
    // tell them differently either.
    const { container } = render(
      <ViewSettingsRail
        expanded={false}
        onToggle={vi.fn()}
        tooltipKey="doc.settingsPanel.title"
        drawerOpen
      />,
    );
    expect(container.querySelector("svg")?.getAttribute("fill")).toBe("currentColor");
  });

  it("stays an outline when the drawer is not up", () => {
    const { container } = render(
      <ViewSettingsRail expanded={false} onToggle={vi.fn()} tooltipKey="doc.settingsPanel.title" />,
    );
    expect(container.querySelector("svg")?.getAttribute("fill")).toBe("none");
  });
});
