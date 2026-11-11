/**
 * chatBubbleFold — the measurement rules behind a folded chat bubble's
 * ten-line ceiling.
 *
 * Why this lives apart from the component: the fold's behaviour is a
 * measurement policy (how many lines fit, when there is anything more to
 * show, whether an expansion survives a live refresh), and that policy is
 * decidable without a DOM. Keeping it here means the rules can be tested
 * directly, and the component keeps only the wiring (measure → decide →
 * render).
 *
 * The facts the policy rests on:
 * - a "line" is the markdown shell's own line box. The bubble must not
 *   invent its own line height: the content is rendered by `ChatMarkdown`,
 *   whose shell (`.prose prose-sm`) carries the app's font scale, so a
 *   clamp computed against anything else would cut the text at a different
 *   place than the reader sees it.
 * - the browser resolves `line-height` to pixels; jsdom and hand-written
 *   styles do not, so the parser accepts the shapes both can produce.
 */

/** How many lines the folded bubble shows. */
export const RUN_ANSWER_COLLAPSED_LINES = 10;

/** The line-height ratio used when the computed value says nothing useful
 *  (`normal`, or an unparsable string). Matches the chat prose's own
 *  leading, so a fallback never cuts the text somewhere else. */
export const DEFAULT_LINE_HEIGHT_RATIO = 1.6;

/**
 * Turn a computed `line-height` into pixels.
 *
 * Accepts the shapes a caller can hand over:
 * - `"19.9px"` — what a browser's `getComputedStyle` returns.
 * - `"1.625"` — a unitless ratio (hand-written styles, jsdom).
 * - `"1.25rem"` / `"1.25em"` — a length against the root / local font size.
 * - `"normal"` — no information; the ratio fallback applies.
 */
export function resolveLineHeightPx(
  computedLineHeight: string | null | undefined,
  fontSizePx: number,
): number {
  const raw = (computedLineHeight ?? "").trim();
  const font = Number.isFinite(fontSizePx) && fontSizePx > 0 ? fontSizePx : 16;
  if (!raw || raw === "normal") return font * DEFAULT_LINE_HEIGHT_RATIO;
  const num = parseFloat(raw);
  if (!Number.isFinite(num) || num <= 0) return font * DEFAULT_LINE_HEIGHT_RATIO;
  if (raw.endsWith("px")) return num;
  // `em` resolves against the element's own font size, `rem` against the root
  // one — both are approximated with the local font size, which is the only
  // size the caller knows.
  if (raw.endsWith("em") || raw.endsWith("rem")) return num * font;
  // A bare number is a ratio of the font size.
  return num * font;
}

/**
 * The collapsed height: N line boxes.
 *
 * The bubble's job is to show the reader the first ten lines, so the ceiling
 * is a whole number of line boxes — a partial line at the cut would read as
 * a rendering defect rather than as a fold.
 */
export function collapsedHeightPx(
  lineHeightPx: number,
  lines: number = RUN_ANSWER_COLLAPSED_LINES,
): number {
  const lh = Number.isFinite(lineHeightPx) && lineHeightPx > 0 ? lineHeightPx : 0;
  const n = Number.isFinite(lines) && lines > 0 ? lines : RUN_ANSWER_COLLAPSED_LINES;
  return Math.max(1, Math.round(lh * n));
}

/**
 * Is there more to read? One pixel of slack, because a rounding difference
 * between the measured content and the computed ceiling is not "more text".
 */
export function isOverflowing(contentHeightPx: number, clampHeightPx: number): boolean {
  return contentHeightPx > clampHeightPx + 1;
}

/**
 * Whether the bubble is expanded, given what it just measured.
 *
 * The rule the reader asked for: an expansion survives a live refresh of the
 * content — a session that keeps talking must not snap the bubble back to
 * ten lines under the reader's finger. The one exception is content that
 * no longer needs a fold: then there is nothing to expand, so the bubble
 * returns to its resting shape instead of keeping a stale "expanded" flag.
 */
export function resolveExpanded(
  expanded: boolean,
  overflowing: boolean,
): boolean {
  return overflowing ? expanded : false;
}
