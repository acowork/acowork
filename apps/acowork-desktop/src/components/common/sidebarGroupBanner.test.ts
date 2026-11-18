/**
 * The agent list's three group banners wear one rounded band.
 *
 * The report: the node band, the project-groups band and the Users band
 * were three hand-written copies of a full-bleed strip — square corners,
 * `border-y` hairlines — and they read as flat bars rather than as the
 * labels they are. They now share one rounded band.
 *
 * The contract this pins:
 *   1. The shared constant carries the band: rounded corners, inset from
 *      the list edges (so the band does not bleed to the pane), and the
 *      `panel-block` fill the bands already wore — with NO outline, which
 *      was tried and read as too loud on a 24px strip.
 *   2. Its corner radius is the one the agent / user rows below it wear
 *      (`rounded-md`). The banner is a sibling of those rows, not a chip
 *      of its own kind — `rounded-full` was tried and read as a pill
 *      bolted onto a list of rectangles.
 *   3. All three call sites go through the constant — a hand-written
 *      class list in one of them is exactly how the three drifted apart
 *      the first time.
 *   4. The old square strip is gone: no `border-y` band, and no
 *      full-bleed `w-full` band (a `w-full` band with horizontal margins
 *      overflows the scroll viewport, which clips it).
 *
 * Source scanning, not rendered DOM — same trade-off as
 * `nodeGroupHeader.test.ts` and `sidebarCountBadge.test.ts`: AgentList
 * pulls in the Tauri plugin dialog, the gateway API and the agent store,
 * so a render assertion there would be brittle. The markup IS the spec.
 */
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import { SIDEBAR_GROUP_BANNER_CN } from "./sidebarGroupBanner";

const __dirname = dirname(fileURLToPath(import.meta.url));
const read = (rel: string) => readFileSync(resolve(__dirname, rel), "utf8");

const AGENT_LIST = read("../agent-list/AgentList.tsx");
const USER_LIST = read("../user-list/UserList.tsx");

describe("sidebar group banners are capsules", () => {
  it("the shared constant carries the rounded-band shape", () => {
    // `rounded-md` (6px) — the SAME radius the agent / user /
    // project-group rows wear. The band is a sibling of those rows, so
    // it has to share their corner language; `rounded-full` read as a
    // pill bolted onto a list of rectangles.
    expect(SIDEBAR_GROUP_BANNER_CN).toContain("rounded-md");
    expect(SIDEBAR_GROUP_BANNER_CN).not.toContain("rounded-full");
    // A gap on all sides, so the band reads as a label for the group
    // under it instead of as another row in it — and the SAME gap
    // vertically and horizontally, so it sits evenly in its slot.
    expect(SIDEBAR_GROUP_BANNER_CN).toContain("m-px");
    expect(SIDEBAR_GROUP_BANNER_CN).not.toMatch(/\bmy-/);
    // NO outline: on a 24px band a hairline is the loudest thing in the
    // sidebar and the banner shouted over the agent rows it labels. The
    // fill step alone separates it — `panel-block` is one step above the
    // `nav-surface` it sits on.
    expect(SIDEBAR_GROUP_BANNER_CN).not.toMatch(/\bborder\b/);
    // The fill the bands already wore (raised over `nav-surface`).
    expect(SIDEBAR_GROUP_BANNER_CN).toContain("bg-panel-block");
    // One semantic token flips both themes — no `dark:` twin may come
    // back, or the two modes drift from the constant by definition.
    expect(SIDEBAR_GROUP_BANNER_CN).not.toContain("dark:");
  });

  it("insets 1px from the pane edge, so its corners are not clipped", () => {
    // The band is a rounded pill; flush against the pane border its
    // corners are cut off. `m-px` is the smallest inset that reads as
    // "inside" without turning the band into a different list style —
    // and it is the SAME inset on all four sides, so the band sits
    // evenly in its slot.
    expect(SIDEBAR_GROUP_BANNER_CN).toContain("m-px");
    // No horizontal PADDING of its own — the inner button owns the text
    // inset (`px-3`), exactly as the rows below it do.
    expect(SIDEBAR_GROUP_BANNER_CN).not.toMatch(/\bpx-/);
    // The rows themselves carry no horizontal margin either — asserted
    // on the source so the two cannot drift apart again.
    expect(AGENT_LIST).toMatch(/items-center rounded-md px-3 py-2/);
    expect(USER_LIST).toMatch(/items-center rounded-md px-3 py-2/);
  });

  it("shares its corner radius with the agent rows below it", () => {
    // The rows the band introduces are `rounded-md`. If the band's radius
    // ever drifts from theirs, the banner reads as a chip of a different
    // kind again — the exact mismatch that made `rounded-full` wrong.
    const ROW_RADIUS = "rounded-md";
    expect(AGENT_LIST).toContain(ROW_RADIUS);
    expect(USER_LIST).toContain(ROW_RADIUS);
  });

  it("the node band wears the shared band", () => {
    expect(AGENT_LIST).toContain("SIDEBAR_GROUP_BANNER_CN");
    // The old square strip must be gone from the node header.
    expect(AGENT_LIST).not.toContain("border-y border-nav-divider/40");
  });

  it("the project-groups band wears the shared band", () => {
    expect(AGENT_LIST).toContain("SIDEBAR_GROUP_BANNER_CN");
  });

  it("the Users band wears the shared band", () => {
    expect(USER_LIST).toContain("SIDEBAR_GROUP_BANNER_CN");
  });

  it("every band carries the shared 1px inset and never overflows", () => {
    // `w-full` on a band that ALSO carries a horizontal margin would
    // overflow the scroll viewport by the margin width, and the viewport
    // clips it. So the inset lives in the shared constant and no call
    // site adds `w-full` or its own margin on top.
    //
    // Comments are stripped first: these call sites explain the geometry,
    // and the prose must not be mistaken for a class.
    const stripComments = (s: string) =>
      s.replace(/\/\*[\s\S]*?\*\//g, "").replace(/\/\/[^\n]*/g, "");

    const bands = [
      ...AGENT_LIST.matchAll(/<div className=\{(?:cn\()?SIDEBAR_GROUP_BANNER_CN[^}]*\}/g),
      ...USER_LIST.matchAll(/<div className=\{(?:cn\()?SIDEBAR_GROUP_BANNER_CN[^}]*\}/g),
    ];
    expect(bands.length).toBeGreaterThan(0);
    for (const band of bands) {
      const classes = stripComments(band[0]);
      // No horizontal margin of its own — the constant owns the inset.
      expect(classes).not.toMatch(/"[^"]*\bmx-[^"]*"/);
      // No `w-full` either: it would overflow the viewport by the inset.
      expect(classes).not.toMatch(/"[^"]*\bw-full\b[^"]*"/);
    }
    // The node header composes the constant with `cn(...)`; it must not
    // re-add a margin or a fixed width on top of it.
    const nodeHeader = AGENT_LIST.slice(AGENT_LIST.indexOf("function NodeGroupHeader"));
    const nodeClasses = nodeHeader.slice(
      nodeHeader.indexOf("SIDEBAR_GROUP_BANNER_CN"),
      nodeHeader.indexOf(")}", nodeHeader.indexOf("SIDEBAR_GROUP_BANNER_CN")),
    );
    expect(stripComments(nodeClasses)).not.toMatch(/"[^"]*\bmx-[^"]*"/);
    expect(stripComments(nodeClasses)).not.toMatch(/"[^"]*\bw-full\b[^"]*"/);
  });
});
