/**
 * CollapsibleChatContent — the fold an over-long chat bubble wears.
 *
 * Why this exists: an agent's answer is usually LONG, and an unclamped bubble
 * under every message turns a transcript into a wall of text. Paging through
 * the conversation then means scrolling past a wall, and the thing the reader
 * actually came for — the shape of the conversation — is invisible.
 *
 * The fold, as the reader asked for it:
 * - collapsed: the first ten lines of the content, and a double-down chevron
 *   that says there is more;
 * - expanded: the whole content, and a double-up chevron that says there is
 *   less;
 * - the chevron toggles between the two.
 *
 * It is a CONTENT fold, not a bubble fold: the bubble's own skin (colour,
 * padding, radius) belongs to the surface that draws it — the 1:1 chat, the
 * group chat and the execution block each have their own — so this component
 * owns only the clip and the chevron, and the caller keeps the bubble around
 * it. Two surfaces use it today (the group chat's agent bubbles and the run
 * answer under an execution block); a third will not have to re-implement the
 * measurement.
 *
 * Three rules this component must not break:
 * - a live refresh must not snap the content shut under the reader's finger.
 *   A session keeps talking, the answer grows, and the fold keeps whatever
 *   shape the reader gave it; only content that no longer needs a fold
 *   returns to the resting shape.
 * - the fold must not make the page jump. The clip animates its height
 *   instead of snapping, so expanding costs a transition rather than a jolt.
 * - the chevron must stay REACHABLE. It lives outside the clipped box: a
 *   toggle the overflow cuts in half is a toggle the reader cannot use.
 *
 * The measurement policy (how a line is counted, where the cut falls, when
 * the fold survives a refresh) lives in `chatBubbleFold.ts`, apart from the
 * DOM, so it can be tested without one.
 */

import { memo, useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { ChevronsDown, ChevronsUp } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import {
  RUN_ANSWER_COLLAPSED_LINES,
  collapsedHeightPx,
  isOverflowing,
  resolveExpanded,
  resolveLineHeightPx,
} from "./chatBubbleFold";

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
 * real wrapping, and it counts a wrapped line once instead of guessing from
 * a character count. That is what makes the cut land after ten lines of text
 * rather than after ten lines of an estimate.
 *
 * Only the TOP of each line is kept, because that is where the fold's cut
 * belongs: the cut is the boundary BETWEEN the tenth line and the eleventh
 * one. A text rect's bottom sits at the bottom of the glyph box, which is
 * below the line box's own bottom whenever the leading is generous — cutting
 * there would slice the eleventh line in half.
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
    const text = node.textContent ?? "";
    if (text.trim().length > 0) {
      const range = document.createRange();
      range.selectNodeContents(node);
      // A Range that cannot report its boxes has no layout to report: the
      // environment lays nothing out (jsdom), and the caller falls back to
      // the length estimate. A missing method is not an error here.
      const rects = typeof range.getClientRects === "function"
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
    .split("\n")
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
  const { t } = useTranslation();
  /** The content, unclipped — the clip is its parent, so the content is
   *  always laid out at full size and can always be measured. */
  const contentRef = useRef<HTMLDivElement | null>(null);
  /** The clip — the box whose ceiling is the fold. Its `scrollHeight` is the
   *  content's FULL height, even while the ceiling hides most of it. */
  const clipRef = useRef<HTMLDivElement | null>(null);
  const [overflowing, setOverflowing] = useState(false);
  const [expanded, setExpanded] = useState(false);
  /** The clip's height in each state, in pixels. `null` means "no ceiling"
   *  (nothing to fold). */
  const [clampPx, setClampPx] = useState<number | null>(null);
  const [fullPx, setFullPx] = useState<number | null>(null);

  /**
   * Measure the content: how many lines it really takes, and where the tenth
   * line ends. Runs after every content change and after every width change
   * (the transcript column is fluid).
   */
  const measure = useCallback(() => {
    const el = contentRef.current;
    if (!el) return;
    // The markdown shell owns the type: `.prose prose-sm` carries the app's
    // font scale and the chat's leading. Measuring anything else would cut
    // the text where the reader does not see a line break.
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
    // The cut is the TOP of the eleventh line — the boundary between the
    // tenth line and the eleventh one. Cutting at the tenth line's own bottom
    // would slice the eleventh line in half whenever the leading is generous.
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

    if (!more) {
      setOverflowing(false);
      setClampPx(null);
      setFullPx(null);
      // Nothing to expand any more — the resting shape is the whole content.
      setExpanded((v) => (v ? false : v));
      return;
    }
    setOverflowing(true);
    setClampPx(cut);
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
    if (!el || typeof ResizeObserver === "undefined") return;
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

  return (
    <>
      {/* The clip. `overflow: hidden` with an animated ceiling is what makes
          the fold a fold; the inner content is never clipped itself, so it
          can always be measured at full height. */}
      <div
        ref={clipRef}
        data-chat-fold-clip=""
        className={`overflow-hidden transition-[max-height] duration-200 ease-out${className ? ` ${className}` : ""}`}
        style={{
          maxHeight: clipHeight === null ? undefined : `${clipHeight}px`,
          transitionDuration: `${CLIP_TRANSITION_MS}ms`,
        }}
      >
        <div ref={contentRef}>{children}</div>
      </div>
      {/* The chevron lives OUTSIDE the clip: a toggle that the clip cuts in
          half is a toggle the reader cannot reach. */}
      {overflowing && (
        <button
          type="button"
          data-chat-fold-toggle=""
          onClick={toggle}
          aria-expanded={shownExpanded}
          aria-label={
            shownExpanded
              ? t("chatView.block.collapseAnswer")
              : t("chatView.block.expandAnswer")
          }
          title={
            shownExpanded
              ? t("chatView.block.collapseAnswer")
              : t("chatView.block.expandAnswer")
          }
          className="mt-1 flex h-7 w-full items-center justify-center rounded-md text-text-tertiary transition-colors hover:bg-black/5 hover:text-text-secondary dark:hover:bg-white/10"
        >
          {shownExpanded ? (
            <ChevronsUp className="h-4 w-4" />
          ) : (
            <ChevronsDown className="h-4 w-4" />
          )}
        </button>
      )}
    </>
  );
});
