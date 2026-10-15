/**
 * Single source of truth for MessageBlock layout constants.
 *
 * Both the data-driven height estimator (`blockHeightEstimator.ts`) and the
 * rendered components (`MessageBubble`, `ExploreBlock`, `CompactionCard`,
 * `DocumentChip`, `VirtualMessageList`) MUST agree on these values, otherwise
 * scrollToIndex(end) lands off by N pixels and the user sees blank space
 * below the last message.
 *
 * Rendered components currently express dimensions as Tailwind classes
 * (e.g. `px-4 py-2.5`, `mt-[6px]`, `max-h-48`).  The numerical values
 * below mirror those classes byte-for-byte.  When you change a class,
 * change the matching constant here — the comment in this file is the
 * audit trail.
 *
 * Constants driven by user settings (font size, content width) live as
 * `getFontSizePx()` / `getContentMaxWidthPct()` getters so the estimator
 * picks up live settings without re-rendering on every keystroke.
 */

// ── Virtualizer ──────────────────────────────────────────────────────
/** `gap: 4` on `useVirtualizer({...})` in VirtualMessageList.tsx. */
export const BLOCK_GAP = 4;

// ── Container chrome (the scroll container itself) ──────────────────
/** `px-4 py-3` on the messages scroll container (NOT counted per-block;
 *  scrolls with content, doesn't inflate the last block). */
export const CONTAINER_PADDING_X = 16;
export const CONTAINER_PADDING_Y = 12;

// ── Bubble chrome (user / assistant / thought text bubbles) ──────────
/** `px-4 py-2.5` on bubble — 16px horizontal × 2, 10px vertical × 2. */
export const BUBBLE_PADDING_X = 32;
export const BUBBLE_PADDING_Y = 20;

/** `mt-[6px]` between user message name (liveUserName) and bubble. */
export const USER_NAME_TO_BUBBLE_GAP = 6;

// ── Typography ──────────────────────────────────────────────────────
/** Default font size in px.  var(--ui-font-size, 0.875rem). */
export const DEFAULT_FONT_SIZE_REM = 0.875;
export const DEFAULT_FONT_SIZE_PX = DEFAULT_FONT_SIZE_REM * 16;

/** Average char width as fraction of font-size for CJK + ASCII mix.
 *  CJK glyphs are ~fontSize wide; ASCII chars are ~0.55 × fontSize. */
export const AVG_CHAR_WIDTH_PCT = 0.6;

/** line-height: 1.6 × fontSize (var --ui-font-size defaults to 14px,
 *  so 14 × 1.6 = 22.4).  Computed from `prose prose-sm` defaults. */
export const LINE_HEIGHT_PX = 22;

/** Extra vertical spacing between paragraphs in `prose prose-sm`. */
export const PROSE_PARAGRAPH_GAP = 8;

// ── Content max width ──────────────────────────────────────────────
/** Default value of `var(--content-max-width)`.  Written by settingsStore
 *  via `applyContentWidth(DEFAULT_CONTENT_WIDTH)` → `${width}%`.  */
export const DEFAULT_CONTENT_WIDTH_PCT = 90;

// ── ExploreBlock ───────────────────────────────────────────────────
/** `calc(var(--ui-font-size, 0.875rem) * 0.9)` — ExploreBlock label font. */
export const EXPLORE_FONT_SIZE_REM = DEFAULT_FONT_SIZE_REM * 0.9;
export const EXPLORE_FONT_SIZE_PX = EXPLORE_FONT_SIZE_REM * 16;

/** `px-2.5 py-1.5` on ExploreBlock header button. */
export const EXPLORE_HEADER_PADDING_X = 10;
export const EXPLORE_HEADER_PADDING_Y = 6;

/** Icon size: `h-3.5 w-3.5` (Tailwind = 14px). */
export const EXPLORE_ICON_PX = 14;

/** Approx ExploreBlock header total height.  Tight bound: button padding
 *  6+6=12 + icon line-height (≈22) = 34, but the chevron is small and
 *  text vertically centers.  Empirical value from render: ~32. */
export const EXPLORE_HEADER_HEIGHT = 32;

/** `ml-12` on the explore_block wrapper in VirtualMessageList.tsx.  This
 *  only affects horizontal offset, NOT block height — included for
 *  completeness so callers can compute block height as
 *  (heightFromContent + EXPLORE_LEFT_INDENT_INVISIBLE? no, indent is invisible).
 */
export const EXPLORE_LEFT_INDENT_PX = 48;

/** `maxHeight: 144px` on ExploreBlock expanded content scroll area. */
export const EXPLORE_CONTENT_MAX_HEIGHT = 144;

/** `py-2` on expanded content container — 8px × 2. */
export const EXPLORE_CONTENT_PADDING_Y = 16;

/** `gap-0.5` between paired items in expanded content (2px). */
export const EXPLORE_ITEMS_GAP = 2;

/** Per-paired-item row height: line-height × 0.9 font-size (12.6px) ×
 *  ~1.6 line-height ≈ 20px.  Empirical ~22. */
export const EXPLORE_ITEM_ROW_HEIGHT = 22;

// ── Misc block heights (single-row, fixed) ─────────────────────────
/** DocumentChip — single line, centered. */
export const DOCUMENT_CHIP_HEIGHT = 28;

/** System bubble — `px-3 py-1 text-xs`, single line. */
export const SYSTEM_BUBBLE_HEIGHT = 26;

/** Compacting indicator virtual item (`ml-12 py-1.5 + dot + label`). */
export const COMPACTING_INDICATOR_HEIGHT = 26;

/**
 * Replying indicator virtual item — renders a `StreamingSourceBlock`
 * variant="assistant" (header row with sparkles icon + "Replying" label
 * + duration timer + chevron, then a content <pre> with max-height =
 * 5 lines × 1.5rem) when the assistant has streamed past
 * `ASSISTANT_REPLYING_LINE_THRESHOLD`. Replaces the old static
 * "Replying..." pulse-dot — the user now sees the live streamed text
 * preview instead of a status label, with the same layout-stable
 * placeholder contract (slot collapses onto the real bubble content
 * without a jump when record_complete clears `isAssistantReplying`).
 *
 * Sits in VirtualMessageList as an extra virtual item at `index ===
 * messageBlocks.length`, physically pinned to the last message bubble —
 * the same conversation slot the reply will occupy once it lands.
 *
 * Height math (matches StreamingSourceBlock chrome byte-for-byte):
 *   header: ~26px (icon 12px + line-height × 0.9 font ≈ 19px, vertically
 *     centered in a button row)
 *   container py-2 = 16px vertical padding
 *   content <pre> maxHeight = 7.5rem × 16 = 120px (capped at 5 lines)
 *   truncation notice = ~16px (text-xs + mb-1)
 *   Total: 26 + 16 + 120 + 16 ≈ 178px when content fills 5 lines.
 *
 * ResizeObserver corrects the actual rendered height to truth once the
 * block mounts; the constant below is just an initial estimate to avoid
 * a big first-frame layout shift.  A slightly tight value is fine —
 * the virtualizer only uses it as a starting point.
 */
export const REPLYING_INDICATOR_HEIGHT = 178;

// ── Agent header (rendered in VirtualMessageList above agent block) ──
/** Avatar rendered at size=40 + `mb-2 mt-1` between header and bubble. */
export const AGENT_HEADER_AVATAR_PX = 40;
export const AGENT_HEADER_VERTICAL_GAP = 8; // mb-2 (mt-1 already implicit)
export const AGENT_HEADER_HEIGHT =
  AGENT_HEADER_AVATAR_PX + AGENT_HEADER_VERTICAL_GAP;

// ── UserMessage chrome (MessageBubble.tsx user branch) ─────────────
/** UserAvatar size=40 on the right side of user message. */
export const USER_AVATAR_SIZE_PX = 40;
/** `mt-1` on UserAvatar (`shrink-0 mt-1`). */
export const USER_AVATAR_TOP_OFFSET_PX = 4;
/** `mt-[2px]` on the liveUserName span. */
export const USER_NAME_TOP_GAP_PX = 2;
/** text-xs (12px) line-height ~1.5 = 18px. */
export const USER_NAME_LINE_HEIGHT_PX = 18;
/** `mt-[6px]` between liveUserName and the bubble. */
export const USER_NAME_TO_BUBBLE_GAP_PX = 6;
/** `max-h-48` on the user bubble — 12rem × 16 = 192px. Beyond this the
 *  bubble scrolls internally; the visible height never exceeds this. */
export const USER_BUBBLE_MAX_HEIGHT_PX = 192;
/** `mt-[6px]` between bubble and the documents chip row (when present). */
export const USER_DOCS_TOP_GAP_PX = 6;

// ── AssistantMessage / Thought chrome ──────────────────────────────
/** `prose prose-sm` paragraph margin-top on consecutive paragraphs. */
export const PROSE_PARAGRAPH_MARGIN_TOP_PX = 16;
/** `prose-h1:text-lg` — h1 line-height factor relative to base. */
export const PROSE_HEADING_LINE_BONUS_PX = 8;
/** `prose-li` marker/list item vertical spacing. */
export const PROSE_LIST_ITEM_BONUS_PX = 4;

// ── Async-rendered code / Mermaid blocks ────────────────────────────
//
// Code-fence content height is data-unpredictable (depends on syntax
// highlight + rendered SVG for Mermaid).  The estimator can't measure
// actual rendered pixels from source text alone, so we use a generous
// floor per code block.  This intentionally OVERESTIMATES for small
// code blocks — overshoot just leaves a small blank gap, which is
// harmless.  ResizeObserver + `measureElement` corrects the cache to
// the true height once the block renders (cache only shrinks from
// here, so no oscillation).
//
// Floor values tuned empirically:
//   - Plain code block: 240px (covers header + ~8 lines of code,
//     bias-high to compensate for the previous undershoot that
//     caused scrollToIndex(end) to miss the bottom by hundreds of px)
//   - Mermaid diagram:  800px (typical graph diagram including title;
//     vertical flowcharts frequently reach 800–1500px, so an 800px floor
//     keeps scrollToIndex(end) from undershooting the real bottom)
export const CODE_BLOCK_MIN_HEIGHT_PX = 240;
export const MERMAID_BLOCK_MIN_HEIGHT_PX = 800;

// ── AskQuestionCard (tool approval card) ───────────────────────────
/** Card uses `my-1.5 max-w-... px-3 py-2` plus a question + options. */
export const ASK_QUESTION_CARD_MIN_HEIGHT = 80;

/** Conservative fallback when a block has unknown type or empty content. */
export const SAFE_FALLBACK_HEIGHT = 60;

// ── Live settings getters ──────────────────────────────────────────
// settingsStore writes these to CSS custom properties on :root; we read
// them back here so the estimator always sees the user's current values.

/**
 * Read the live content-max-width percentage from the CSS custom
 * property `var(--content-max-width)`.  Returns the percentage as a
 * fraction (0..1).
 *
 * Falls back to `DEFAULT_CONTENT_WIDTH_PCT / 100` if the property isn't
 * set yet (early render before settingsStore has applied its defaults).
 */
export function getContentMaxWidthPct(): number {
  if (typeof window === "undefined") return DEFAULT_CONTENT_WIDTH_PCT / 100;
  const raw = getComputedStyle(document.documentElement)
    .getPropertyValue("--content-max-width")
    .trim();
  if (!raw) return DEFAULT_CONTENT_WIDTH_PCT / 100;
  // raw is like "90%" — strip the % and divide.
  const n = parseFloat(raw);
  if (!Number.isFinite(n)) return DEFAULT_CONTENT_WIDTH_PCT / 100;
  return n / 100;
}

/**
 * Read the live UI font size in px from `var(--ui-font-size)`.  Falls
 * back to `DEFAULT_FONT_SIZE_PX` if not yet set.
 */
export function getFontSizePx(): number {
  if (typeof window === "undefined") return DEFAULT_FONT_SIZE_PX;
  const raw = getComputedStyle(document.documentElement)
    .getPropertyValue("--ui-font-size")
    .trim();
  if (!raw) return DEFAULT_FONT_SIZE_PX;
  // raw is like "0.875rem" or "14px" — normalise.
  if (raw.endsWith("rem")) {
    const rem = parseFloat(raw);
    if (Number.isFinite(rem)) return rem * 16;
  }
  if (raw.endsWith("px")) {
    const px = parseFloat(raw);
    if (Number.isFinite(px)) return px;
  }
  const n = parseFloat(raw);
  if (Number.isFinite(n)) return n; // assume px
  return DEFAULT_FONT_SIZE_PX;
}
