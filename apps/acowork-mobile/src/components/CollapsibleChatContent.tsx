/**
 * CollapsibleChatContent — the fold an over-long chat bubble wears.
 *
 * The mobile port of Desktop's `chat/CollapsibleChatContent`; the measurement
 * and the state rules are verbatim, only the skin differs (this app styles
 * through `app.css`, not utility classes).
 *
 * Why this exists: an agent's answer is usually LONG, and an unclamped bubble
 * under every block turns a phone transcript into a wall of text — the column
 * is 390pt wide, so the same answer takes roughly twice as many lines as on a
 * desktop pane.
 *
 * The fold, as the reader asked for it:
 * - collapsed: the first eight lines of the content, and a double-down chevron
 *   that says there is more;
 * - expanded: the whole content, and a double-up chevron that says there is
 *   less;
 * - the cut is a FADE, not a knife: the bottom band dissolves the text
 *   through a mask instead of slicing a line in half. Folded and idle, the
 *   reader sees ONLY that fade — the chevron's row is fully transparent, so
 *   no rectangle is drawn over the bubble;
 * - the chevron keeps its own row BELOW the text in BOTH states. Floating it
 *   over the content puts the up-chevron on top of the answer's last line
 *   once expanded, where a transparent control is illegible;
 * - the rounded highlight is a pointer affordance, not a permanent skin: it
 *   appears on hover / keyboard focus and disappears again on leave;
 * - the chevron toggles between the two.
 *
 * It is a CONTENT fold, not a bubble fold: the bubble's own skin (colour,
 * padding, radius) belongs to the surface that draws it — so this component
 * owns only the clip and the chevron, and the caller keeps the bubble around
 * it.
 *
 * Three rules this component must not break:
 * - a live refresh must not snap the content shut under the reader's finger.
 *   A session keeps talking, the answer grows, and the fold keeps whatever
 *   shape the reader gave it; only content that no longer needs a fold
 *   returns to the resting shape.
 * - the fold must not make the page jump. The clip animates its height
 *   instead of snapping, so expanding costs a transition rather than a jolt.
 * - the chevron must stay REACHABLE. It lives outside the clipped box: a
 *   toggle the overflow cuts in half is a toggle the reader cannot use — and
 *   on a phone, one the thumb cannot aim at.
 *
 * The measurement policy (how a line is counted, where the cut falls, when
 * the fold survives a refresh) lives in `lib/chatBubbleFold.ts`, apart from
 * the DOM, so it can be tested without one.
 */
import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { ChevronsDown, ChevronsUp } from 'lucide-react'
import {
  FOLD_FADE_BAND_PX,
  RUN_ANSWER_COLLAPSED_LINES,
  collapsedHeightPx,
  isOverflowing,
  resolveExpanded,
  resolveLineHeightPx,
} from '../lib/chatBubbleFold'

/** Fallback characters-per-line, used ONLY where the DOM cannot report a
 *  layout (no measurement environment). The browser path never uses it. */
const FALLBACK_CHARS_PER_LINE = 80;

/** How long the clip takes to open and close. Short on purpose: the reader
 *  asked for a fold, not an animation. */
const CLIP_TRANSITION_MS = 180;

interface CollapsibleChatContentProps {
  /** The content to fold. Measured in place, so it must be the element that
   *  actually paints the text (markdown, plain text, anything). */
  children: React.ReactNode;
  /** The content's plain text — used only where the DOM cannot lay out (no
   *  measurement environment), to decide whether there is more to read. */
  text: string;
  /** Extra class on the clip. */
  className?: string;
}

/**
 * The rendered line boxes of a subtree, as their `top` edges, in the origin
 * element's own coordinate space.
 *
 * A range's client rects are the browser's own answer to "how many lines does
 * this text take" — it accounts for the real font, the real width and the
 * real wrapping, and it counts a wrapped line once instead of guessing from a
 * character count. That is what makes the cut land after eight lines of text
 * rather than after eight lines of an estimate.
 *
 * Only the TOP of each line is kept, because that is where the fold's cut
 * belongs: the cut is the boundary BETWEEN the eighth line and the ninth
 * one. A text rect's bottom sits at the bottom of the glyph box, which is
 * below the line box's own bottom whenever the leading is generous — cutting
 * there would slice the ninth line in half.
 */
function collectRenderedLineTops(
  container: HTMLElement,
  origin: HTMLElement,
): number[] {
  const originTop = origin.getBoundingClientRect().top;
  const byTop = new Map<number, number>();
  const walker = document.createTreeWalker(container, NodeFilter.SHOW_TEXT);
  let node = walker.nextNode();
  while (node) {
    const text = node.textContent ?? '';
    if (text.trim().length > 0) {
      const range = document.createRange();
      range.selectNodeContents(node);
      // A Range that cannot report its boxes has no layout to report: the
      // environment lays nothing out (jsdom), and the caller falls back to
      // the length estimate. A missing method is not an error here.
      const rects = typeof range.getClientRects === 'function'
        ? Array.from(range.getClientRects())
        : [];
      for (const rect of rects) {
        // A zero-height rect is a collapsed/invisible box — not a line.
        if (rect.height <= 0) continue;
        const key = Math.round(rect.top);
        const bottom = rect.bottom;
        const prev = byTop.get(key);
        if (prev === undefined || bottom > prev) byTop.set(key, bottom);
      }
      range.detach?.();
    }
    node = walker.nextNode();
  }
  return [...byTop.keys()]
    .map((top) => top - originTop)
    .sort((a, b) => a - b);
}

/**
 * A line count for environments that cannot lay out text. It is a fallback,
 * not the browser's answer: the browser always has one.
 */
function estimateLines(text: string): number {
  return text
    .split('\n')
    .reduce(
      (sum, line) =>
        sum + Math.max(1, Math.ceil(line.length / FALLBACK_CHARS_PER_LINE)),
      0,
    );
}

export const CollapsibleChatContent = memo(function CollapsibleChatContent({
  children,
  text,
  className,
}: CollapsibleChatContentProps) {
  /** The content, unclipped — the clip is its parent, so the content is
   *  always laid out at full size and can always be measured. */
  const contentRef = useRef<HTMLDivElement | null>(null);
  /** The clip — the box whose ceiling is the fold. Its `scrollHeight` is the
   *  content's FULL height, even while the ceiling hides most of it. */
  const clipRef = useRef<HTMLDivElement | null>(null);
  const [overflowing, setOverflowing] = useState(false);
  const [expanded, setExpanded] = useState(false);
  /** The clip's ceiling in each state, in pixels. `null` means "no ceiling"
   *  (nothing to fold). The expanded ceiling is the content's full height —
   *  `max-height` cannot stretch a box, so reserving space for the chevron
   *  inside the clip would only put the arrow back on top of the text. */
  const [clampPx, setClampPx] = useState<number | null>(null);
  const [fullPx, setFullPx] = useState<number | null>(null);

  /**
   * Measure the content: how many lines it really takes, and where the eighth
   * line ends. Runs after every content change and after every width change
   * (the transcript column is fluid).
   */
  const measure = useCallback(() => {
    const el = contentRef.current;
    if (!el) return;
    // The element that paints the text owns the type. Measuring anything
    // else would cut the text where the reader does not see a line break.
    const typed = el.firstElementChild as HTMLElement | null;
    const cs = getComputedStyle(typed ?? el);
    const fontSize = parseFloat(cs.fontSize) || 16;
    const lineHeight = resolveLineHeightPx(cs.lineHeight, fontSize);

    // The clip is the origin of the measurement: the cut is a height INSIDE
    // the clip, so it has to be measured from the clip's own top edge.
    const clipEl = clipRef.current;
    const origin = clipEl ?? el;
    const lineTops = collectRenderedLineTops(el, origin);
    const hasLayout = lineTops.length > 0;
    // Without a layout (no measurement environment) the content is estimated
    // from its own length; the browser path always measures.
    const lineCount = hasLayout ? lineTops.length : estimateLines(text);
    // The FULL height of the content: the clip's own scroll height, which is
    // the content's height whatever ceiling is painted over it.
    const full = hasLayout
      ? Math.ceil(
          Math.max(
            el.getBoundingClientRect().height,
            el.scrollHeight,
            clipEl?.scrollHeight ?? 0,
          ),
        )
      : collapsedHeightPx(lineHeight, lineCount);
    // The cut is the TOP of the ninth line — the boundary between the eighth
    // line and the ninth one. The ceiling sits a fade band BELOW it: the
    // ninth line is still painted there, but dissolves through the mask
    // instead of being sliced by a hard `overflow: hidden` edge.
    const cut = hasLayout
      ? lineCount > RUN_ANSWER_COLLAPSED_LINES
        ? Math.ceil(lineTops[RUN_ANSWER_COLLAPSED_LINES]!)
        : full
      : collapsedHeightPx(
          lineHeight,
          Math.min(lineCount, RUN_ANSWER_COLLAPSED_LINES),
        );
    // "More to read" is a question about TEXT, not about the box: content
    // whose last line ends at the cut has nothing more, even when a trailing
    // margin leaves the container a few pixels taller than the text.
    const more = hasLayout
      ? lineCount > RUN_ANSWER_COLLAPSED_LINES
      : isOverflowing(full, cut);
    // The ceiling never passes the content: a max-height taller than the
    // content would put the mask's fade band over the last line that is
    // actually there, fading readable text for no reason.
    const clamp = Math.min(cut + FOLD_FADE_BAND_PX, Math.max(1, full - 1));

    if (!more) {
      setOverflowing(false);
      setClampPx(null);
      setFullPx(null);
      // Nothing to expand any more — the resting shape is the whole content.
      setExpanded((v) => (v ? false : v));
      return;
    }
    setOverflowing(true);
    setClampPx(clamp);
    // The chevron owns a row of its own (outside the clip) in both states, so
    // the expand animation is a pure ceiling change: no row is added, no
    // page jump, and no arrow sitting on the answer's last line.
    setFullPx(full);
  }, [text]);

  useLayoutEffect(() => {
    measure();
  }, [measure]);

  // Width changes reflow the content, so the fold has to be re-decided. The
  // transcript column is fluid; a resize that makes the content shorter must
  // not leave a stale ceiling behind.
  useEffect(() => {
    const el = contentRef.current;
    if (!el || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => measure());
    ro.observe(el);
    return () => ro.disconnect();
  }, [measure]);

  // The reader's intent survives a live refresh: the content grows, the fold
  // keeps the shape the reader gave it. Only content that no longer needs a
  // fold puts it back to rest (handled in `measure`).
  const shownExpanded = resolveExpanded(expanded, overflowing);

  const toggle = useCallback(() => {
    setExpanded((v) => !resolveExpanded(v, overflowing));
  }, [overflowing]);

  const clipHeight = shownExpanded ? fullPx : clampPx;
  const folded = overflowing && !shownExpanded;
  // The mask fades the CONTENT's own pixels, not a coloured overlay. The
  // bubble under this fold is semi-transparent, so no "fade-to-background"
  // colour could match it without leaving a visible patch. The knee 20px
  // above the bottom makes the last stretch almost clear, so the fade ends
  // softly instead of leaving half-visible glyphs at the clip edge.
  const fadeMask = folded
    ? `linear-gradient(to bottom, #000 calc(100% - ${FOLD_FADE_BAND_PX}px), rgba(0, 0, 0, 0.25) calc(100% - 20px), transparent 100%)`
    : undefined;

  return (
    <>
      {/* The clip. `overflow: hidden` with an animated ceiling is what makes
          the fold a fold; the inner content is never clipped itself, so it
          can always be measured at full height. The mask is folded-state
          only — expanded, it would fade the answer's last line. */}
      <div
        ref={clipRef}
        data-chat-fold-clip=""
        data-chat-fold-fade={folded ? '' : undefined}
        className={`chat-fold-clip${className ? ` ${className}` : ''}`}
        style={{
          maxHeight: clipHeight === null ? undefined : `${clipHeight}px`,
          transitionDuration: `${CLIP_TRANSITION_MS}ms`,
          maskImage: fadeMask,
          WebkitMaskImage: fadeMask,
        }}
      >
        <div ref={contentRef}>{children}</div>
      </div>
      {/* The chevron keeps a row of its own BELOW the text, outside the clip:
          a toggle the clip cuts in half is a toggle the reader cannot reach,
          and one floating over the expanded answer sits on its last line.
          Its idle surface is fully transparent — no rectangle over the
          bubble. Only hover / focus paints the rounded highlight. */}
      {overflowing && (
        <button
          type="button"
          data-chat-fold-toggle=""
          className="chat-fold-toggle"
          onClick={toggle}
          aria-expanded={shownExpanded}
          aria-label={shownExpanded ? '收起全文' : '展开全文'}
          title={shownExpanded ? '收起全文' : '展开全文'}
        >
          {shownExpanded ? (
            <ChevronsUp aria-hidden />
          ) : (
            <ChevronsDown aria-hidden />
          )}
        </button>
      )}
    </>
  );
});
