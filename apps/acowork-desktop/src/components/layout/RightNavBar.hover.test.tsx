/**
 * RightNavBar hover contract — the rail is the ONLY entry point to the hover
 * drawer while the panel is collapsed, so two things must hold:
 *
 *   1. every visible tile reports pointer-enter with its own tab (a tile that
 *      forgets to report is a tile that silently lost the feature);
 *   2. the tile the drawer is previewing reads active, and its tooltip goes
 *      away — the bubble used to sit exactly where the drawer now paints.
 *
 * The rail never decides whether a hover earns a drawer; it only reports
 * (layout/useRightPanelDrawer.ts owns that).
 */
import { describe, it, expect, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";

vi.mock("../../i18n/useTranslation", () => ({
  useTranslation: () => ({ t: (key: string) => key.replace("rightPanel.", "") }),
}));
vi.mock("../common/Tooltip", () => ({
  // Pass-through that still paints `content` in its own element, so the tests
  // can assert the bubble is suppressed for the peeked tile (the real one is
  // a delayed portal, which jsdom cannot hover).
  Tooltip: ({ children, content }: { children?: React.ReactNode; content?: React.ReactNode }) => (
    <>
      {children}
      <span data-testid="tooltip">{content}</span>
    </>
  ),
}));

import { RightNavBar } from "./RightNavBar";

const base = {
  activeTab: "status" as const,
  onTabChange: () => {},
  agentRunning: true,
  memoryServiceReady: true,
  collapsed: true,
};

function tiles() {
  return screen.getAllByRole("button");
}

describe("hover reporting", () => {
  it("reports each tile with the tab it stands for", () => {
    const onTabHover = vi.fn();
    render(<RightNavBar {...base} onTabHover={onTabHover} />);

    // workspace / status / memory / debug / tasks / setup — in rail order.
    fireEvent.mouseEnter(tiles()[0]);
    fireEvent.mouseEnter(tiles()[2]);
    expect(onTabHover.mock.calls.map(([tab]) => tab)).toEqual(["workspace", "memory"]);
  });

  it("reports the rail being left", () => {
    const onHoverEnd = vi.fn();
    const { container } = render(<RightNavBar {...base} onHoverEnd={onHoverEnd} />);

    fireEvent.mouseLeave(container.querySelector("nav")!);
    expect(onHoverEnd).toHaveBeenCalledTimes(1);
  });

  it("still draws the rail when the owner does not wire hover at all", () => {
    render(<RightNavBar {...base} />);
    expect(tiles()).toHaveLength(6);
  });
});

describe("the peeked tile", () => {
  it("reads active while the drawer previews it, though the layout is collapsed", () => {
    render(<RightNavBar {...base} drawerTab="memory" />);
    const memory = tiles()[2];
    expect(memory.querySelector("svg")?.getAttribute("fill")).toBe("currentColor");
  });

  it("does not light up a different tile than the drawer is showing", () => {
    render(<RightNavBar {...base} drawerTab="memory" />);
    expect(tiles()[0].querySelector("svg")?.getAttribute("fill")).toBe("none");
  });

  it("drops its tooltip, because the drawer is painted where the bubble goes", () => {
    const withDrawer = render(<RightNavBar {...base} drawerTab="memory" />);
    expect(withDrawer.queryByText("memory")).toBeNull();
    withDrawer.unmount();

    const without = render(<RightNavBar {...base} drawerTab={null} />);
    expect(without.queryByText("memory")).not.toBeNull();
  });

  it("ignores a drawer tab while the panel is pinned — the pinned tab owns the highlight", () => {
    render(<RightNavBar {...base} collapsed={false} activeTab="status" drawerTab="memory" />);
    expect(tiles()[2].querySelector("svg")?.getAttribute("fill")).toBe("none");
    expect(tiles()[1].querySelector("svg")?.getAttribute("fill")).toBe("currentColor");
  });
});
