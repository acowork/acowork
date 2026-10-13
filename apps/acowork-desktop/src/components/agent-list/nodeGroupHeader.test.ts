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
    expect(header.indexOf("bg-emerald-500")).toBeGreaterThan(name);
  });

  it("sits on the same fill as the right-panel card title bars", () => {
    // The node band is raised off `nav-surface` with the one token that
    // also drives ListBox's card (right-panel Agent Status / Session
    // Status titles). Both themes auto-flip, so a `dark:` pair would
    // drift from the token by definition.
    const header = src.slice(src.indexOf("function NodeGroupHeader"));
    expect(header).toContain("bg-panel-block");
    expect(header).not.toMatch(/bg-panel-block[^\n]*dark:bg-/);
  });
});

describe("agent working dot", () => {
  // Anchor on the JSX comment, not the bare "IM-style" phrase — the same
  // wording appears in the `activeAgentIds` memo further up, and
  // indexOf would silently slice that one instead.
  const dot = src.slice(
    src.indexOf('IM-style "needs attention" indicator dot'),
    src.indexOf('IM-style "needs attention" indicator dot') + 2000,
  );

  it("uses the same emerald as the node online dot", () => {
    // Two dots, one meaning ("something is live"). Matching the node
    // header's literal is deliberate: a new `bg-emerald-600` variant here
    // would look like a different state at 8px.
    expect(dot).toContain("bg-emerald-500");
  });

  it("hides on the selected row", () => {
    // The selection fill already marks the row; an emerald dot over it
    // reads as a smudge. Assert the full guard so dropping any single
    // clause (e.g. the offline carve-out) also fails here.
    expect(dot).toMatch(
      /agent\.alive\s*&&\s*activeAgentIds\.has\(id\)\s*&&\s*selectedAgentId\s*!==\s*id/,
    );
  });
});
