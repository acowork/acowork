/**
 * SectionPane — the two-column shell settings / harness now navigate with.
 *
 * What this pins (the two things that silently drift the moment someone
 * copies this shell instead of importing it):
 *   1. Both columns wear CAPSULE_PANE_CN, and the left root carries
 *      `w-full` — as an AppLayout flex item, a root without it sizes to
 *      its content and leaves a strip of bare vibrancy beside the detail
 *      capsule (the same defect DocsView had).
 *   2. The surface split matches pm / docs: list = `bg-nav-surface`,
 *      detail = `bg-page-bg`. `bg-right-panel` is the chat view's darker
 *      inspector; putting it on the detail column makes it read as a
 *      demoted panel next to its own list.
 *   3. The left header names the section (icon + title) above a hairline
 *      divider, and the list scrolls on its own below it — the header
 *      must not scroll away with the items.
 *
 * Rendered DOM, not source scanning: SectionPane is pure (jsdom-safe), and
 * the surfaces only matter as *applied* class names.
 */
import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import { CAPSULE_PANE_CN } from "./capsule";
import { SectionPane } from "./SectionPane";

const items = [
  { id: "a", label: "Alpha", icon: <span>A</span> },
  { id: "b", label: "Beta", icon: <span>B</span> },
];

function renderPane(selected = "a", onSelect = vi.fn()) {
  return { ...render(
    <SectionPane
      title="Settings"
      icon={<span>icon</span>}
      items={items}
      selected={selected}
      onSelect={onSelect}
      storageKey="test-section-pane-width"
    >
      <p>detail body</p>
    </SectionPane>,
  ), onSelect };
}

describe("SectionPane", () => {
  it("wears the shared capsule shell on both columns", () => {
    const { container } = renderPane();
    const root = container.firstElementChild as HTMLElement;
    expect(root.className).toContain("w-full");

    const aside = container.querySelector("aside")!;
    const detail = container.querySelector("[role=tabpanel]")!;
    for (const cn of CAPSULE_PANE_CN.split(" ")) {
      expect(aside.className, `list missing "${cn}"`).toContain(cn);
      expect(detail.className, `detail missing "${cn}"`).toContain(cn);
    }
  });

  it("mounts the cards on the same surface the agent panel uses", () => {
    // The detail column is a CARD FLOW (ExpandableRow / ListBox), and
    // those cards are painted with the `panel-block` / `panel-inset` /
    // `panel-inset-2` ladder that was tuned against the right-side
    // agent settings panel. So the container must be `bg-right-panel` —
    // the surface that ladder was designed to sit on.
    //
    // It used to be `bg-page-bg`, copied from pm / docs. Those panes are
    // content surfaces, not card flows, and the mismatch inverted the
    // elevation: dark mode has page-bg L=8% vs panel-block L=16.7%, so
    // the cards floated ~8.7 lightness points above their own
    // container. Pin the container token so it cannot drift back.
    const { container } = renderPane();
    const detail = container.querySelector("[role=tabpanel]")!.className;
    expect(detail).toContain("bg-right-panel");
    expect(detail).not.toContain("bg-page-bg");

    // The list column is unaffected — still nav-surface, matching
    // ProjectSidebar / DocTreeSidebar.
    expect(container.querySelector("aside")!.className).toContain("bg-nav-surface");
  });

  it("keeps the header above a divider and scrolls only the list", () => {
    const { container } = renderPane();
    const aside = container.querySelector("aside")!;
    const header = aside.firstElementChild as HTMLElement;
    expect(header.textContent).toContain("Settings");
    expect(header.className).toContain("border-b");
    expect(header.className).toContain("shrink-0");

    // The list is now wrapped in OverlayScrollbar: the outer wrapper is the
    // flex-fill region (`flex-1`), and the actual `overflow-y-auto` scroll
    // surface is its inner viewport.
    const list = aside.children[1] as HTMLElement;
    expect(list.className).toContain("overlay-scrollbar");
    expect(list.className).toContain("flex-1");
    const viewport = list.querySelector(".overlay-scrollbar-viewport") as HTMLElement;
    expect(viewport.className).toContain("overflow-y-auto");
  });

  it("selects a section and marks the current one", () => {
    const onSelect = vi.fn();
    renderPane("b", onSelect);
    const [alpha, beta] = screen.getAllByRole("button");
    expect(beta.getAttribute("aria-current")).toBe("page");
    expect(alpha.getAttribute("aria-current")).toBeNull();

    alpha.click();
    expect(onSelect).toHaveBeenCalledWith("a");
  });

  it("matches the pm row chrome so list capsules read as one system", () => {
    // The row specs live in projectStore-land (pm's ProjectSidebar) and
    // were hand-copied; doc is the deliberate exception (a tree, so it
    // keeps denser py-1 rows). Pin the three shared traits so this
    // section list cannot drift back to its own accent/10 wash.
    const { container } = renderPane("b");
    const rows = screen.getAllByRole("button");
    const selected = rows[1];
    const unselected = rows[0];

    // 1. Height — py-2.5, same as pm's project row.
    for (const row of [selected, unselected]) {
      expect(row.className).toContain("py-2.5");
    }

    // 2. Selection — solid accent wash + white text. The unselected row
    //    takes the same accent family at listRowHover strength (per the
    //    app-wide "list rows share one tint" rule), so we no longer
    //    pin "no accent on unselected" — instead we pin that the
    //    selected wash is a SOLID /90 (not a tint), which keeps the
    //    selected row the loudest one on screen. The fill is the
    //    darkened `--color-accent-strong` variant (white-text surfaces)
    //    so dark mode contrast survives — see globals.css.
    expect(selected.className).toContain("bg-accent-strong/90");
    expect(selected.className).toContain("text-white");
    expect(unselected.className).not.toContain("bg-accent-strong/90");

    // 3. Hairline divider between rows, drawn on every row but the last.
    expect(unselected.className).toContain("row-divider-b");
    expect(selected.className, "last row must not draw a divider").not.toContain("row-divider-b");
    expect(container).toBeTruthy();
  });

  it("keeps the row icon readable on the solid selection", () => {
    // A `text-tertiary` glyph left grey on the solid accent fill is
    // barely legible; the selected row must lift its icon to white too.
    const { container } = renderPane("b");
    const iconSpans = container.querySelectorAll("aside [aria-hidden]");
    // header icon + one icon per row (2 rows)
    expect(iconSpans.length).toBeGreaterThanOrEqual(3);
    const selectedIcon = iconSpans[2];
    expect(selectedIcon.className).toContain("text-white");
  });
});
