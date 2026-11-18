/**
 * sidebarGroupBanner.ts — the shape every agent-list group banner wears.
 *
 * The agent list stacks three kinds of group banner: the node band (a
 * machine), the project-groups band, and the Users band. They were three
 * hand-written copies of the same strip — a full-bleed row with `border-y`
 * hairlines and square corners — and the report was that they read as flat
 * bars rather than as the labels they are.
 *
 * They now wear ONE rounded band:
 *   - 1px INSET on ALL sides (`m-px`), so the band's rounded corners are
 *     not clipped by the pane border and the band sits evenly inside its
 *     slot. Everything else stays as wide as the agent / user /
 *     project-group rows below it — the rows set the language; the banner
 *     joins them.
 *   - `rounded-md` (6px) — the SAME radius those rows wear. A band is a
 *     sibling of those rows, not a chip of its own kind, so it has to
 *     share their corner language. `rounded-full` was tried first and
 *     read as a pill bolted onto a list of rectangles: too round next to
 *     the selected agent row right below it, and the mismatch was the
 *     loudest thing on screen.
 *   - NO outline. A hairline border was tried and read as too loud: on a
 *     24px band it is the highest-contrast element in the sidebar, which
 *     made the banner shout over the agent rows it is supposed to label.
 *     The band is separated by its FILL alone — `panel-block` is one step
 *     above the `nav-surface` it sits on, so the step in the surface
 *     ladder already draws the shape. Adding a border on top of a fill
 *     that already separates itself is what made it read as a box.
 *   - the 1px gap above and below (same `m-px`) keeps the band from
 *     fusing with the rows around it, without the loose 4px vertical
 *     rhythm the old `my-1` gave.
 *   - the `panel-block` fill is unchanged, so the banner still reads as a
 *     raised band over `nav-surface` — the contract the node header and
 *     the Users header both already carried.
 *
 * The `border-y` hairlines are gone on purpose: the fill step and the gap
 * around the band are what separate one group from the next now.
 *
 * One constant, three call sites (AgentList x2, UserList x1) — the same
 * reasoning as `sidebarCountBadge.ts`: the three bands used to be able to
 * drift apart on geometry, and nothing but a shared constant stops them.
 */
export const SIDEBAR_GROUP_BANNER_CN =
  "m-px flex h-6 items-center rounded-md bg-panel-block";
