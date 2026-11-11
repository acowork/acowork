/**
 * capsule.ts — the one shape every top-level app pane wears.
 *
 * The chat view has worn floating "capsule" panels (rounded-xl + hairline
 * border + own surface color) since the vibrancy pass; the pm / docs /
 * extensions views instead painted their own solid `bg-page-bg` plane, so
 * those views booted straight into a flat wall with no panel outlines and
 * no vibrancy showing through. This module is the shared shell both
 * languages now draw from.
 *
 * Shape only — the surface color stays with each pane, because the four
 * existing panels deliberately differ: chat body is white, right panel is
 * one step darker (it must read as a distinct raised surface), the left
 * sidebars are `nav-surface`. Append one of `bg-chat-body` / `bg-right-panel`
 * / `bg-page-bg` / `bg-nav-surface` after this constant:
 *
 *   <div className={cn(CAPSULE_PANE_CN, "bg-chat-body")}>
 *
 * `min-h-0` is load-bearing: a flex child with `flex-col` and overflow
 * content must be able to shrink below its content height, or inner
 * scroll roots get pushed out of the panel instead of scrolling.
 *
 * Why a constant and not a Tailwind `@utility`: every consumer needs a
 * *different* surface token, so the reusable part is the shape, not a
 * full class list. A `@utility` would have to hardcode one background.
 */
export const CAPSULE_PANE_CN =
  "flex min-h-0 flex-col overflow-hidden rounded-xl border border-border-outer";

/**
 * The surface ladder every pane in the app walks — four steps, no fifth.
 *
 *   1. `bg-page-bg`        the view plane itself.
 *   2. SURFACE_PANEL_CN    a panel off that plane: a board column, the capsule
 *                          around a table (its toolbar / pager show this fill).
 *   3. SURFACE_HEAD_CN     the title band of a piece of content: a board card's
 *                          title strip, a table's header row, an embedded list
 *                          box. Reads as "this block has a heading".
 *   4. SURFACE_CONTENT_CN  the content body below that band: a card's detail
 *                          row, the table body, an expanded list body.
 *
 * The board has worn exactly these four since the vibrancy pass —
 * page → column (P) → card title strip (H) → card body (C). The pm list now
 * walks the same four: page → list capsule (P) → table header (H) → rows (C).
 *
 * Two rules follow from the ladder, and they are what makes separate views
 * read as one system:
 *
 *   - content never sits directly on the page (`page-bg` → body with no panel
 *     and no header step between reads as a flat wall with no boundary);
 *   - content never wears the panel's own fill (a wall of rows on the panel
 *     grey goes muddy, and the title band disappears into it).
 *
 * A pane that reaches for `bg-zinc-*` to invent a fifth step is exactly how
 * these surfaces drifted apart before. Name the step, don't re-pick a colour.
 */
export const SURFACE_PANEL_CN = "bg-right-panel";
export const SURFACE_HEAD_CN = "bg-panel-block";
export const SURFACE_CONTENT_CN = "bg-panel-inset";
