/**
 * NodeGroupHeader visual contract.
 *
 * Two deliberate choices are pinned here because neither is visible to the
 * type checker and both are easy to "tidy up" back into place:
 *
 *   1. The permissions entry is a plain gear. It was a `UserCog` (person +
 *      cog), which reads as "manage a user" — wrong for a node, and
 *      inconsistent with the gear used for every other settings entry.
 *   2. The agent count is gone from the header row. It was the only thing
 *      forcing `ml-auto` onto the name, and the number competed with the
 *      online dot for the same attention in a 240px sidebar.
 *
 * Source scanning, not rendered DOM: AgentList pulls in the Tauri plugin
 * dialog, the gateway API and the agent store, so a render assertion here
 * would be brittle and unrepresentative. Same trade-off as
 * capsule.test.tsx — the markup IS the spec.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { SIDEBAR_GROUP_BANNER_CN } from "../common/sidebarGroupBanner";

const __dirname = dirname(fileURLToPath(import.meta.url));
const src = readFileSync(resolve(__dirname, "AgentList.tsx"), "utf8");

describe("NodeGroupHeader", () => {
  it("uses a plain gear for node permissions", () => {
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    expect(header).toContain("<Settings className=");
    expect(header).not.toContain("UserCog");
  });

  it("no longer renders an agent count", () => {
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    // Assert on the badge *markup*, not on the prop identifier — a count
    // inlined as a literal (`{7}`) would otherwise sail past a check that
    // only looks for the string "agentCount".
    expect(header).not.toMatch(/ml-auto[\s\S]{0,120}(opacity-60|text-xs)/);
    // The prop must be gone from the signature too, not just its render.
    expect(header).not.toMatch(/agentCount[?]?:\s*number/);
    expect(header).not.toMatch(/\{agentCount\}/);
  });

  it("keeps the permissions button pinned to the right edge", () => {
    // Without the count there is no `ml-auto` pushing anything; the header
    // button being `flex-1` is what now reserves the space before the gear.
    // If that ever changes the node name collides with the gear.
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    expect(header).toMatch(/flex-1[^"]*"[\s\S]{0,80}onClick=\{onToggle\}|onToggle[\s\S]{0,200}flex-1/);
  });

  it("marks the row as a machine with a monitor glyph", () => {
    // The node band is the only place in the sidebar that labels a
    // *device* rather than an agent/user. The Monitor icon leads so
    // "device" reads before the host name; without it the band is just a
    // divider line and the rows under it look like agents.
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    expect(header).toContain("<Monitor className=");
    // Anchor on the rendered name span: `{nodeName}` alone also matches the
    // aria-label on the toggle button, which sits earlier and would flip
    // every ordering assertion below.
    const icon = header.indexOf("<Monitor");
    const name = header.indexOf('<span className="truncate">{nodeName}');
    expect(icon).toBeLessThan(name);
    // The status dot is a trailing badge — after the name, so the row leads
    // with the glyph + identifier and status does not compete for the lead
    // position with the monitor icon.
    expect(header.indexOf("bg-ok-solid")).toBeGreaterThan(name);
  });

  it("drops the monitor glyph and the name on the avatar rail", () => {
    // Regression: the band knows nothing about the sidebar's collapsed
    // width, and its `shrink-0` children (chevron, monitor, name, dot) could
    // not shrink below ~72px. At the 64px rail they overflowed the flex
    // toggle button and painted the monitor straight on top of the
    // permissions gear. The fix is to stop drawing them, not to add
    // `overflow-hidden`, which would also have eaten the gear's hit area.
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    expect(src).toContain("compact={isCollapsed}");
    expect(header).toMatch(/\{!compact && <Monitor /);
    expect(header).toMatch(/\{!compact && <span className="truncate">\{nodeName\}/);
    // The gear survives the rail: it is the band's only action there, so it
    // may lose margin but never the whole button.
    expect(header).toMatch(/compact \? "mr-1" : "mr-2"/);
  });

  it("sits on the same fill as the right-panel card title bars", () => {
    // The node band is raised off `nav-surface` with the one token that
    // also drives ListBox's card (right-panel Agent Status / Session
    // Status titles). Both themes auto-flip, so a `dark:` pair would
    // drift from the token by definition.
    //
    // The fill now arrives through the shared banner constant (the band
    // is a floating pill), so the assertion follows the constant rather
    // than a literal in this file — a literal here is what let the three
    // banners drift apart on geometry in the first place.
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    expect(header).toContain("SIDEBAR_GROUP_BANNER_CN");
    expect(SIDEBAR_GROUP_BANNER_CN).toContain("bg-panel-block");
    expect(SIDEBAR_GROUP_BANNER_CN).not.toMatch(/bg-panel-block[^\n]*dark:bg-/);
  });
});

describe("agent status dot (beside the name)", () => {
  // Source-scanned like the rest of this file. The dot used to be an
  // avatar-corner working-only marker; it is now the single 3-state status
  // dot beside the agent name (gray offline / solid emerald online /
  // breathing emerald online+working), matching the mobile row. The old
  // `{busyAgentIds.has(id) && (...)}` anchor is gone by design.
  const start = src.indexOf('title={busyAgentIds.has(id) && agent.alive');
  const end = src.indexOf('/>', start);
  const dot = src.slice(start, end);
  // Sanity: the anchor must exist, or every assertion below passes vacuously.
  expect(start).toBeGreaterThan(-1);

  it("is emerald when online, matching the node online dot", () => {
    // Two dots, one meaning ("something is live"). Matching the node
    // header's literal is deliberate: a different shade would look like a
    // different state at this size.
    expect(dot).toContain("bg-ok-solid");
    expect(dot).toContain("agent.alive");
  });

  it("is gray when offline", () => {
    expect(dot).toContain("bg-loading");
  });

  it("breathes (fully transparent <-> solid) while online and working", () => {
    // `animate-pulse` only dipped to 0.5 opacity -- too subtle at 6px.
    // `dot-breathing` is the dedicated keyframe in globals.css.
    expect(dot).toContain("dot-breathing");
    expect(dot).not.toContain("animate-pulse");
  });

  it("is the same size as the node header dot", () => {
    expect(dot).toContain("h-1.5 w-1.5");
  });

  it("carries a label for hover and screen readers", () => {
    // Colour alone is not an accessible signal; the node band already sets
    // this precedent with its `title={statusLabel}` dot.
    expect(dot).toContain('t("agentList.dotBusy")');
    expect(dot).toContain('t("agentList.dotOnline")');
    expect(dot).toContain('t("agentList.dotOffline")');
  });
});
