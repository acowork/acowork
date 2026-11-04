/**
 * Pure function that folds raw `ChatMessage[]` into `MessageBlock[]`.
 *
 * Extracted from ChatPanel's `useMemo` (ADR-041 C1) so that the folding
 * logic is reusable and testable without a React component context.
 *
 * Key change from the old implementation: `blockId` is now **content-derived**
 * (`block-${items[0].id}`) instead of **index-derived** (`block-${i}`).
 * This means prepend/append operations do not change the blockId of any
 * existing block, which is the foundation for stable scroll anchoring.
 */

import type { ChatMessage } from "../../lib/types";

/** ADR-046 §2.5: 5 attachment metadata types. */
const ATTACHMENT_META_TYPES = new Set([
  "file_upload",
  "image_upload",
  "attached_file",
  "attached_selection",
  "attached_folder",
]);

/** ADR-107: message-part metadata types. A *part* is any row that belongs to
 *  the user turn above it instead of being a message of its own — attachments
 *  (things the human dragged in) and references (context the runtime
 *  supplied, e.g. the chat background a routed turn came from). One union of
 *  two sets, because the fold rule and the block type are the same for both:
 *  the difference is only which slot of the bubble renders them. */
const MESSAGE_REF_META_TYPES = new Set(["context_ref"]);

/** Max timestamp delta (ms) between a user message and its attachment
 *  system entries for them to be folded into a single block. */
const ATTACHMENT_FOLD_WINDOW_MS = 100;

/**
 * Strict intermediate representation between the data layer (raw
 * `ChatMessage[]` in chatStore) and the rendering layer
 * (`VirtualMessageList`).
 *
 * Every property is data-defined, not visually estimated:
 *
 * - `type`           drives component choice (MessageBubble vs ExploreBlock).
 * - `items`          raw messages the block contains; for non-group blocks
 *                    `items.length === 1`.
 * - `rawCount`       `items.length` cached for cheap arithmetic.
 * - `anchorToLatest` true iff this block contains the **last raw entry** in
 *                    the current messages array.
 * - `hasFollowUpReply` true iff an `assistant` (or other non-explore) message
 *                    follows this explore block in display order.
 *                    Only meaningful for `type === "explore_group"`.
 *
 * ADR-050 C3 extension:
 *  - `isLive`        : true iff at least one `items` entry originated in
 *                    the adapter's `liveBuffer` (stream delta / optimistic
 *                    user / record-complete) rather than the server-
 *                    authoritative HTTP window.  VML routes
 *                    `isLive === true` blocks through
 *                    `StreamingSourceBlock`.  `foldMessages` itself is
 *                    pure — it never sets `isLive`; the v2 adapter
 *                    applies the flag in a post-fold pass based on
 *                    `liveBuffer.containsId(item.id)`.
 */
export interface MessageBlock {
  /** Content-derived stable ID: `block-${items[0].id}`. Survives prepend/append. */
  blockId: string;
  type: ChatMessage["type"] | "explore_group" | "user_with_parts";
  items: ChatMessage[];
  rawCount: number;
  anchorToLatest: boolean;
  hasFollowUpReply: boolean;
  /**
   * ADR-050 C3: true iff at least one `items` entry originated in the
   * adapter's liveBuffer.  VML routes `isLive === true` blocks through
   * `StreamingSourceBlock` so the trailing preview + duration timer
   * remain functional during streaming.  Default false (history-only).
   */
  isLive: boolean;
}

/** Check if a system message is an attachment entry (ADR-046 §2.5). */
export function isAttachmentPart(msg: ChatMessage): boolean {
  if (msg.type !== "system") return false;
  const metaType = msg.metadata?.type as string | undefined;
  return metaType !== undefined && ATTACHMENT_META_TYPES.has(metaType);
}

/** Check if a system message is a reference part (ADR-107). */
export function isMessageRefPart(msg: ChatMessage): boolean {
  if (msg.type !== "system") return false;
  const metaType = msg.metadata?.type as string | undefined;
  return metaType !== undefined && MESSAGE_REF_META_TYPES.has(metaType);
}

/** Any row that folds into the user turn above it. */
export function isUserTurnPart(msg: ChatMessage): boolean {
  return isAttachmentPart(msg) || isMessageRefPart(msg);
}

/**
 * Fold raw `ChatMessage[]` into `MessageBlock[]`.
 *
 * Rules:
 *  - Consecutive `tool_call` / `tool_result` / `thought` messages are grouped
 *    into a single `explore_group` block.
 *  - A `user` message followed by message-part system entries — attachments
 *    (ADR-046) and references (ADR-107) alike, within
 *    `ATTACHMENT_FOLD_WINDOW_MS` — is folded into a single `user_with_parts`
 *    block. The parts stay in the block's `items` in log order; the renderer
 *    decides which slot each one goes to.
 *  - All other message types become individual blocks.
 *  - `blockId` is derived from the first item's `id` (content-derived, stable).
 *  - `anchorToLatest` is set on the block containing the last raw entry.
 *  - `hasFollowUpReply` is computed in a second pass for explore_group blocks.
 */
export function foldMessages(messages: ChatMessage[]): MessageBlock[] {
  const blocks: MessageBlock[] = [];
  const lastIdx = messages.length - 1;
  let exploreStartMsgIdx = -1; // index into messages[] where the explore group starts
  let exploreBuffer: ChatMessage[] = [];

  const flushExplore = () => {
    if (exploreBuffer.length === 0) return;
    const items = exploreBuffer;
    // Content-derived blockId: use the first item's id.
    const exploreBlockId = `block-${items[0].id}`;
    blocks.push({
      blockId: exploreBlockId,
      type: "explore_group",
      items,
      rawCount: items.length,
      anchorToLatest:
        exploreStartMsgIdx + items.length - 1 === lastIdx,
      // Backfilled in the second pass below.
      hasFollowUpReply: false,
      // ADR-050 C3: set by the v2 adapter in a post-fold pass, never
      // by foldMessages itself.  Default to false here so the
      // legacy / pre-v2 callers keep working.
      isLive: false,
    });
    exploreBuffer = [];
    exploreStartMsgIdx = -1;
  };

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i];

    if (
      msg.type === "tool_call" ||
      msg.type === "tool_result" ||
      msg.type === "thought"
    ) {
      if (exploreStartMsgIdx < 0) exploreStartMsgIdx = i;
      exploreBuffer.push(msg);
    } else {
      flushExplore();

      // ── user_with_parts folding ──────────────────────────────
      // If this is a user message, look ahead for the part entries that
      // belong to it (attachments and references alike, same batch,
      // within the fold window).
      if (msg.type === "user") {
        const parts: ChatMessage[] = [];
        let j = i + 1;
        while (
          j < messages.length &&
          isUserTurnPart(messages[j]) &&
          messages[j].timestamp - msg.timestamp <= ATTACHMENT_FOLD_WINDOW_MS
        ) {
          parts.push(messages[j]);
          j++;
        }
        if (parts.length > 0) {
          const items = [msg, ...parts];
          const blockId = `block-${msg.id}`;
          blocks.push({
            blockId,
            type: "user_with_parts",
            items,
            rawCount: items.length,
            anchorToLatest: j - 1 === lastIdx,
            hasFollowUpReply: false,
            // ADR-050 C3: see note in the explore_group branch above.
            isLive: false,
          });
          i = j - 1; // skip the consumed part entries
          continue;
        }
      }

      const blockId = `block-${msg.id}`;
      blocks.push({
        blockId,
        type: msg.type,
        items: [msg],
        rawCount: 1,
        anchorToLatest: i === lastIdx,
        hasFollowUpReply: false,
        // ADR-050 C3: see note in the explore_group branch above.
        isLive: false,
      });
    }
  }
  flushExplore();

  // Second pass: an explore_group has a follow-up reply iff the next block
  // in display order is NOT itself an explore_group.
  for (let i = 0; i < blocks.length - 1; i++) {
    const cur = blocks[i];
    if (
      cur.type === "explore_group" &&
      blocks[i + 1].type !== "explore_group"
    ) {
      blocks[i] = { ...cur, hasFollowUpReply: true };
    }
  }

  return blocks;
}

/**
 * ADR-081 §4.2: index of the folded block containing the raw
 * `messages[]` entry `rawIdx`, or -1 when the target is not inside any
 * block (e.g. a live-buffer entry that predates the loaded window).
 * Blocks are consecutive spans of `items`, so a linear accumulate is
 * exact — no per-block index bookkeeping needed.
 */
export function blockIndexOfRawMessage(blocks: readonly MessageBlock[], rawIdx: number): number {
  let acc = 0;
  for (let i = 0; i < blocks.length; i++) {
    if (rawIdx < acc + blocks[i].items.length) return i;
    acc += blocks[i].items.length;
  }
  return -1;
}
