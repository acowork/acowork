/**
 * Data-driven precise-height estimator for MessageBlocks.
 *
 * Every constant comes from `blockLayout.ts` — the single source of
 * truth shared with the rendered components (MessageBubble, ExploreBlock,
 * CompactionCard, DocumentChip, VirtualMessageList).  When you change a
 * Tailwind class in any of those components, update the matching constant
 * in `blockLayout.ts`; this file will pick it up automatically.
 *
 * Estimation strategy — render-faithful:
 *   The estimator mirrors the actual DOM layout of each block type.  For
 *   user messages that's max(leftColumn, rightColumn) where rightColumn
 *   is name + bubble + docs and leftColumn is avatar+mt-1.  For assistant
 *   / thought messages it sums bubble chrome + text-line height + a prose
 *   markdown bonus for paragraph/list/heading spacing.
 *
 *   Exact prediction of `prose` markdown height is impossible from data
 *   alone (it depends on actual font metrics + heading/list rendering),
 *   so we add a per-line markdown bonus tuned empirically to bias slightly
 *   high (overshoot).  Overshoot is harmless: it just means scrollTop
 *   sits a few px past the true bottom, which is visually correct.  The
 *   tanstack-virtual `measureElement` ref callback will replace our
 *   estimate with the real height the moment the item is in view, so any
 *   drift self-corrects on the next scroll.
 *
 * Why we still need a precise estimator despite measureElement:
 *   `measureElement` only writes the cache for items currently in the DOM.
 *   `virtualizer.scrollToIndex(end)` runs immediately when called and
 *   depends on `measurementsCache` for every item along the way.  Until
 *   each item has been mounted at least once, scrolltoIndex lands on the
 *   estimateSize-derived position.  With the estimator below, that
 *   position is within ±10% of the true bottom for typical content, so
 *   users see "scroll lands close to the bottom" rather than "scrolls to
 *   the middle of the conversation".
 *
 * The gap between items (`useVirtualizer({ gap: 4 })`) is added by the
 * virtualizer itself, NOT by this function.  estimateSize must return
 * the body height of a single item; adding gap here double-counts.
 *
 * ── Module-level measured-height cache ─────────────────────────────
 *
 * Once `measureElement` reports a real height for a given block, we stash
 * it in `measuredHeightsByBlockId` keyed by MessageBlock.blockId (NOT by
 * index).  blockId is index-derived today (`"block-${i}"`), so it changes
 * for every block after the streaming tail — but that just means cache
 * thrash for the last few items, which are exactly the ones being
 * streamed and not-yet-stable.  Earlier blocks keep their ids and their
 * cached heights across new-message appends, so the visible scroll
 * position stays correct instead of jumping when the user is reading
 * the middle of a long conversation.
 *
 * The cache is intentionally module-scoped (lives for the page
 * lifetime), not per-component, so that switching between sessions and
 * back reuses heights for blocks whose content is byte-identical (same
 * blockId hash).  For sessions with very different content, the lookup
 * misses and falls back to the data-driven estimator.
 *
 * The cache is NEVER cleared by this file — we have no signal that
 * "this content has been removed from memory".  In practice this is fine
 * because blockIds are content-derived and the footprint is bounded by
 * the union of all sessions opened in the current window (~MB at worst).
 */

import type { MessageBlock } from "./messageFolder";
import {
  BUBBLE_PADDING_Y,
  LINE_HEIGHT_PX,
  AVG_CHAR_WIDTH_PCT,
  PROSE_PARAGRAPH_GAP,
  PROSE_PARAGRAPH_MARGIN_TOP_PX,
  PROSE_HEADING_LINE_BONUS_PX,
  PROSE_LIST_ITEM_BONUS_PX,
  USER_NAME_LINE_HEIGHT_PX,
  USER_NAME_TOP_GAP_PX,
  USER_NAME_TO_BUBBLE_GAP_PX,
  USER_AVATAR_SIZE_PX,
  USER_AVATAR_TOP_OFFSET_PX,
  USER_BUBBLE_MAX_HEIGHT_PX,
  DOCUMENT_CHIP_HEIGHT,
  EXPLORE_HEADER_HEIGHT,
  EXPLORE_CONTENT_MAX_HEIGHT,
  SYSTEM_BUBBLE_HEIGHT,
  SAFE_FALLBACK_HEIGHT,
  CODE_BLOCK_MIN_HEIGHT_PX,
  MERMAID_BLOCK_MIN_HEIGHT_PX,
  getContentMaxWidthPct,
  getFontSizePx,
} from "./blockLayout";

// ── Attachment system entry heights (ADR-046 §2.5) ──────────────────
// These were previously exported from blockLayout.ts but are now local
// because they are only referenced by this estimator.
const ATTACHMENT_CHIP_HEIGHT_PX = 32;
const ATTACHMENT_THUMB_HEIGHT_PX = 56;

// ── Text-bubble content height ──────────────────────────────────────────

// ── Measured-height cache (see file header) ────────────────────────
//
// Keyed by MessageBlock.blockId (content-derived).  Stays valid across
// re-renders, session switches, and visible ResizeObserver fires.
const measuredHeightsByBlockId = new Map<string, number>();

/**
 * Look up a previously-measured height for the given block.  Returns
 * undefined when there's no entry (cache miss → fall back to estimator).
 */
export function getMeasuredHeight(blockId: string): number | undefined {
  return measuredHeightsByBlockId.get(blockId);
}

/**
 * Record a real measured height for a block.  Called from the
 * virtualizer's measureElement callback (which runs every time an item
 * enters the viewport or its size changes).
 *
 * Tiny drift (<2px) is ignored to avoid noisy log writes during browser
 * zoom / DPR changes.  Larger deltas (including async content settling
 * for code/Mermaid blocks) overwrite immediately.
 */
export function recordMeasuredHeight(blockId: string, height: number): void {
  if (!Number.isFinite(height) || height <= 0) return;
  const prev = measuredHeightsByBlockId.get(blockId);
  if (prev !== undefined && Math.abs(prev - height) < 2) return;
  measuredHeightsByBlockId.set(blockId, height);
}

/**
 * Estimate the rendered height of a plain text bubble (user / assistant /
 * thought).  Accounts for explicit \n wrap and character wrap at
 * `containerWidth × contentMaxWidthPct`.
 *
 * Returns the bubble's own height INCLUDING `py-2.5` padding.
 */
function estimateTextBubbleHeight(
  content: string,
  containerWidth: number,
): number {
  if (!content) {
    // Empty bubble (e.g. streaming placeholder) — base padding only,
    // plus one line for the cursor/dot.
    return BUBBLE_PADDING_Y + LINE_HEIGHT_PX;
  }
  if (containerWidth <= 0) return SAFE_FALLBACK_HEIGHT;

  const usableWidth = containerWidth * getContentMaxWidthPct();
  const fontSize = getFontSizePx();
  const charWidth = fontSize * AVG_CHAR_WIDTH_PCT;
  const charsPerLine = Math.max(1, Math.floor(usableWidth / charWidth));

  // Split into logical lines by \n first, then wrap each logical line.
  const logicalLines = content.split("\n");
  let totalLines = 0;
  for (const line of logicalLines) {
    if (line.length === 0) {
      totalLines += 1;
    } else {
      totalLines += Math.ceil(line.length / charsPerLine);
    }
  }

  return BUBBLE_PADDING_Y + totalLines * LINE_HEIGHT_PX;
}

// ── Public API ──────────────────────────────────────────────────────────

/**
 * Compute the height (in px) of one virtual item at `index`.
 *
 * ADR-050 C5: the virtualizer count === messageBlocks.length.  No
 * trailing extra slots (replying / compacting indicators) exist; all
 * indices map directly to message blocks.
 *
 * @param index                Virtual item index (0 .. messageBlocks.length-1).
 * @param messageBlocks        The strict intermediate MessageBlock array.
 * @param containerWidth       Current rendered width of the messages scroll
 *                             container in px.  Used for text-bubble line
 *                             wrap calculations; pass 0 if not yet measured
 *                             (the function falls back to a safe constant).
 */
export function estimateBlockHeight(
  index: number,
  messageBlocks: readonly MessageBlock[],
  containerWidth: number,
): number {
  // ADR-050 C5: trailing extra slots (replying / compacting indicators)
  // removed.  The virtualizer count === messageBlocks.length; all indices
  // map directly to message blocks.

  // Consult the module-level measured-height cache.
  if (index >= 0 && index < messageBlocks.length) {
    const cached = measuredHeightsByBlockId.get(messageBlocks[index].blockId);
    if (cached !== undefined) return cached;
  }

  const block = messageBlocks[index];
  if (!block) return SAFE_FALLBACK_HEIGHT;

  switch (block.type) {
    case "user": {
      const msg = block.items[0];
      const content = msg?.content ?? "";
      const hasName = !!msg?.senderDisplayName;
      // Right column (text side): liveUserName (mt-[2px] + line) →
      // mt-[6px] → bubble (capped at max-h-48). No inline attachment
      // chips — ADR-046 renders each attachment as a separate system
      // entry in the message list.
      const bubbleHeight = Math.min(
        USER_BUBBLE_MAX_HEIGHT_PX,
        estimateTextBubbleHeight(content, containerWidth),
      );
      const rightColumn =
        (hasName ? USER_NAME_TOP_GAP_PX + USER_NAME_LINE_HEIGHT_PX : 0)
        + USER_NAME_TO_BUBBLE_GAP_PX
        + bubbleHeight;
      // Left column (avatar side): mt-1 + 40px avatar.
      const leftColumn = USER_AVATAR_TOP_OFFSET_PX + USER_AVATAR_SIZE_PX;
      // The flex container is `items-start`, so total height = max of the
      // two columns.  This mirrors the actual rendered DOM.
      return Math.max(rightColumn, leftColumn);
    }
    case "assistant":
    case "thought": {
      const msg = block.items[0];
      const content = msg?.content ?? "";
      // Under the convergent model, messages[] only contains frozen records.
      // An empty content with isStreaming=true is a legacy placeholder that
      // should not exist; default to a single-line height as a safety net.
      if (!content && msg?.isStreaming) {
        return BUBBLE_PADDING_Y + LINE_HEIGHT_PX;
      }
      const textHeight = estimateTextBubbleHeight(content, containerWidth);
      // Markdown prose bonus — head/lists/paragraphs inflate real height
      // beyond what plain-text line counts predict.  We estimate by
      // scanning the content for structural markers and adding per-line
      // bonuses.  Biases slightly high (overshoot) for safety.
      const logicalLines = content.split("\n");
      const nonEmpty = logicalLines.filter((l) => l.length > 0);
      let markdownBonus = 0;
      // Code/Mermaid block height accumulator.  Each fenced ``` block's
      // rendered height depends on its CONTENT (syntax highlight token
      // count for code, graph node count for Mermaid), which we cannot
      // measure from data alone.  Strategy:
      //   1. Count lines inside the fence pair (open ``` → close ```).
      //   2. Estimate = max(MIN_FLOOR, contentLines * LINE_HEIGHT_PX + chrome)
      //      - Mermaid uses a larger per-line factor (40px) because the
      //        rendered SVG has node padding + arrow spacing the syntax
      //        source doesn't capture.
      //   3. MIN_FLOOR guards tiny snippets (1-2 line code) from being
      //      underestimated below the chrome of the code container.
      //
      // Critical bug being fixed: previously each ``` line added the floor
      // (opening AND closing), doubling the contribution per code block
      // and inflating totalSize by hundreds of pixels.
      let codeHeight = 0;
      const CHROME_PER_CODE_BLOCK = 20; // padding/border around the code element
      for (let i = 0; i < nonEmpty.length; i++) {
        const line = nonEmpty[i];
        // Markdown headings (`# `, `## `, `### `, `#### `) get an extra
        // ~8px line bonus for the larger font + line-height.
        if (/^#{1,6}\s/.test(line)) {
          markdownBonus += PROSE_HEADING_LINE_BONUS_PX;
        }
        // List markers (`- `, `* `, `1. `) get a per-item bonus.
        if (/^\s*([-*]|\d+\.)\s/.test(line)) {
          markdownBonus += PROSE_LIST_ITEM_BONUS_PX;
        }
        // Code fence — track open/close and count lines inside.
        if (/^```/.test(line)) {
          markdownBonus += LINE_HEIGHT_PX;
          // Look at next line to detect Mermaid (info language tag).
          const next = nonEmpty[i + 1] ?? "";
          const isMermaid = /^mermaid\b/i.test(next);
          // Count content lines until the closing fence.
          let contentLines = 0;
          let j = i + 1;
          while (j < nonEmpty.length && !/^```/.test(nonEmpty[j])) {
            contentLines++;
            j++;
          }
          // Per-line factor: Mermaid rendered SVG is taller per syntax
          // line than plain code (node boxes + arrows add vertical
          // spacing that the source text doesn't show).
          const perLine = isMermaid ? 40 : LINE_HEIGHT_PX;
          const minFloor = isMermaid
            ? MERMAID_BLOCK_MIN_HEIGHT_PX
            : CODE_BLOCK_MIN_HEIGHT_PX;
          const estimate = Math.max(
            minFloor,
            contentLines * perLine + CHROME_PER_CODE_BLOCK,
          );
          codeHeight += estimate;
          // Skip past the closing fence (if present) so we don't
          // re-enter the loop on it.
          if (j < nonEmpty.length) {
            i = j; // for-loop's i++ will move us past the closing fence
          }
        }
      }
      // Paragraph gap: every consecutive non-empty line after the first
      // gets a prose paragraph margin-top (16px) in addition to line-height.
      // Conservatively add one prose-margin per non-empty line for an
      // extra layer above plain text.
      const paragraphOverhead = Math.max(0, nonEmpty.length - 1) * PROSE_PARAGRAPH_GAP;
      // Additional prose-block margin for multi-paragraph responses.
      const proseBlockOverhead = nonEmpty.length > 1
        ? PROSE_PARAGRAPH_MARGIN_TOP_PX
        : 0;
      return textHeight + markdownBonus + paragraphOverhead + proseBlockOverhead + codeHeight;
    }
    case "explore_group": {
      // hasFollowUpReply is set by ChatPanel when it builds messageBlocks.
      // Collapsed (has follow-up reply): just the header.
      // Expanded: header + content area up to the 144px cap, scaled by item count.
      if (block.hasFollowUpReply) return EXPLORE_HEADER_HEIGHT;
      // Render-faithful height of an expanded explore_group:
      //   header (32) + mt-1 separator (4) + inner container with
      //   `style={{ maxHeight: 144px }}` (the maxHeight is a HARD cap —
      //   content scrolls internally, so itemCount does NOT affect the
      //   rendered block height).
      // The previous estimator applied Math.min(EXPLORE_CONTENT_MAX_HEIGHT,
      // itemCount * ROW + PAD) AND omitted the mt-1 separator, yielding
      // a UNDERSHOOTING 272px vs. the true 276px.  Worse, for content
      // whose itemCount * ROW + PAD was already >= 144 the cap kicked in
      // and the result was a flat 272px regardless of how many tools
      // were inside — accurate only by accident.  scrollToIndex(end) in
      // VirtualMessageList relies on this estimate placing the last
      // block inside (or past) the viewport so the virtualizer mounts
      // it and ResizeObserver writes the true height into
      // measurementsCache.  An undershoot leaves the last block outside
      // the viewport, the cache stays cold, and scrollHeight never grows
      // to the real bottom — the user has to click many times to
      // converge.  Use the render-faithful 180px value (32 + 4 + 144):
      // equals the true height in the common case, overshoots when
      // itemCount * ROW + PAD is below 144 (overshoot is harmless;
      // measureElement corrects it the moment the block mounts).
      return EXPLORE_HEADER_HEIGHT + 4 /* mt-1 */ + EXPLORE_CONTENT_MAX_HEIGHT;
    }
    case "system": {
      // ADR-046 §2.5: 5 attachment-type system entries have distinct
      // heights. Check the message metadata to dispatch.
      const msg = block.items[0];
      const metaType = msg?.metadata?.type;
      if (metaType === "file_upload") return DOCUMENT_CHIP_HEIGHT;
      if (metaType === "image_upload") return ATTACHMENT_THUMB_HEIGHT_PX;
      if (metaType === "attached_file"
        || metaType === "attached_selection"
        || metaType === "attached_folder") {
        return ATTACHMENT_CHIP_HEIGHT_PX;
      }
      // Generic system notification.
      return SYSTEM_BUBBLE_HEIGHT;
    }
    case "compaction":
    // tool_call / tool_result outside an explore_group (orphaned) are
    // rendered as their own MessageBubble with fixed-height toggles.
    case "tool_call":
    case "tool_result":
      // Single-line collapsed toggle.  Expanded form is rare and a small
      // undershoot is harmless (measureElement corrects it the moment the
      // user scrolls past).
      return 28;
    default:
      return SAFE_FALLBACK_HEIGHT;
  }
}
