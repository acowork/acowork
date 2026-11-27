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

import type { ChatMessage, ExploreStubMeta } from "../../lib/types";
import { isExploreRunRow, isUserTurnPart } from "./messageParts";

/** The part registry lives in `messageParts` because the JSONL loader needs the
 *  same definition (it decides which rows keep their `metadata` at all). Keeping
 *  a private copy here is what let a `context_ref` row reach the folder as a
 *  metadata-less `system` message and render as a system bubble. */
export { isAttachmentPart, isMessageRefPart, isUserTurnPart } from "./messageParts";

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
  /**
   * t-01412679: set when this `explore_group` was described by explore-STUB
   * metadata (`groups=stub` page projection) — the collapsed header renders
   * from it and the rows it covers are fetched on first expand.
   *
   * A stub row IS one whole run, because the Runtime snaps page boundaries to
   * record boundaries (§5.2.1), so a stub block's `items` is empty by
   * construction. The other shape a run takes — its raw rows — is a plain
   * block with no `stub`; a block never mixes the two.
   *
   * Fetched rows never land here — they live in the store's `exploreDetails`
   * cache (keyed by `stubId`) and reach the component as its `details` prop.
   * The folder stays a pure function of `messages[]`.
   */
  stub?: ExploreStubMeta;
  /** The stub ROW's id (the run's first row in this window) — the detail cache
   *  key and the fetch argument. Equal to `blockId.slice("block-".length)` by
   *  construction; carried explicitly so no consumer re-derives it from the
   *  key's format. */
  stubId?: string;
  /** True when the block holds no rows at all: everything it has is stub
   *  metadata. Every consumer that asks "does this block have rows to render?"
   *  reads this, not `items.length`. */
  lazy?: boolean;
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

  // ── the explore accumulator ──────────────────────────────────────────
  //
  // ONE block per record. A run reaches the client in one of two shapes and
  // never a mixture, because the Runtime pages by RECORD (t-01412679 §5.2.1):
  // the window edges are snapped to run boundaries, so a run is either here
  // as its raw rows (the trailing run of a page at the log end) or as ONE stub
  // row that describes it entirely. Nothing has to be fused, and a block can
  // never be half of a run — that was the reported "one run, two chips".
  let explore: { items: ChatMessage[]; stub?: ExploreStubMeta; stubId?: string } | null = null;
  let exploreStartMsgIdx = -1; // index into messages[] where the run starts
  let exploreEndMsgIdx = -1; // …and where the last row it consumed sits

  const flushExplore = () => {
    if (!explore) return;
    const { items, stub, stubId } = explore;
    const first = messages[exploreStartMsgIdx];
    blocks.push({
      blockId: `block-${first.id}`,
      type: "explore_group",
      items,
      // A raw block is exactly as wide as its rows; a stub block's width is the
      // span it advertises — its rows never reach the client.
      rawCount: stub ? stub.rawCount : items.length,
      anchorToLatest: exploreEndMsgIdx === lastIdx,
      // Backfilled in the second pass below.
      hasFollowUpReply: false,
      // ADR-050 C3: set by the v2 adapter in a post-fold pass, never
      // by foldMessages itself.  Default to false here so the
      // legacy / pre-v2 callers keep working.
      isLive: false,
      ...(stub ? { stub, stubId, lazy: true } : {}),
    });
    explore = null;
    exploreStartMsgIdx = -1;
    exploreEndMsgIdx = -1;
  };

  const startExplore = (i: number) => {
    if (!explore) {
      exploreStartMsgIdx = i;
      explore = { items: [] };
    }
    exploreEndMsgIdx = i;
    return explore;
  };

  // ── Ownership pre-pass ────────────────────────────────────
  // Which part row belongs to which user row.  Computed BEFORE the main
  // loop, because a part row that PRECEDES its user row would otherwise be
  // emitted as a standalone `system` block before the folder ever got to
  // the user row that owns it.
  //
  // Why that order happens at all: the Runtime writes the user row and its
  // attachment rows with the SAME millisecond timestamp (`write_attached_items`
  // runs in the same batch as the user row), so their relative order is
  // decided by whatever assembled the array — not by the log.
  // `mergeMessageWindow` fills `server` first and then `cache`, and its
  // stable sort keeps that insertion order for equal timestamps: a server
  // page holding the attachment rows lands them BEFORE a cached user row of
  // the same millisecond.  A forward-only scan then found no parts, the
  // attachments stayed standalone `system` blocks, and `MessageBubble`
  // rendered them as CENTERED notices — the pictures appeared in the middle
  // of the screen, detached from the turn they belong to, while the
  // reference part (which happened to sort after the user row) folded
  // correctly and stayed right-aligned.  Switching sessions reloaded the
  // pages in a different order, which is why the same turn appeared
  // right-aligned one minute and centered the next.
  //
  // Each user row claims the contiguous run of part rows immediately before
  // and after it, within the fold window.  Contiguity is what keeps a
  // block's `items` a contiguous span of the raw array, which
  // `blockIndexOfRawMessage` relies on to map a raw index to its block.
  // User rows are visited in array order and a part is claimed at most
  // once, so the assignment is unambiguous.
  const ownerOfPart = new Map<number, number>(); // part index -> user index
  const partsByUser = new Map<number, number[]>(); // user index -> part indices

  for (let u = 0; u < messages.length; u++) {
    if (messages[u].type !== "user") continue;
    const userMsg = messages[u];
    const leading: number[] = [];
    for (let k = u - 1; k >= 0; k--) {
      const prev = messages[k];
      if (!isUserTurnPart(prev)) break;
      if (ownerOfPart.has(k)) break;
      if (userMsg.timestamp - prev.timestamp > ATTACHMENT_FOLD_WINDOW_MS) break;
      leading.unshift(k);
    }
    const trailing: number[] = [];
    for (let j = u + 1; j < messages.length; j++) {
      const next = messages[j];
      if (!isUserTurnPart(next)) break;
      if (ownerOfPart.has(j)) break;
      if (next.timestamp - userMsg.timestamp > ATTACHMENT_FOLD_WINDOW_MS) break;
      trailing.push(j);
    }
    const owned = [...leading, ...trailing];
    if (owned.length === 0) continue;
    for (const k of owned) ownerOfPart.set(k, u);
    partsByUser.set(u, owned);
  }

  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i];

    // A part row is never a block of its own: it belongs to the user row
    // the pre-pass assigned it to, and is emitted with that row.
    if (ownerOfPart.has(i)) {
      // Still ends an open explore group: the part row physically sits
      // between two explore runs, so the group before it must close here.
      flushExplore();
      continue;
    }

    // t-01412679: an explore-STUB row is the server's projection OF one whole
    // run, so it is one block and it closes any open run first. There is no
    // fusion here: the Runtime snaps page boundaries to record boundaries, so
    // two stubs are two runs, never two halves of one.
    if (msg.type === "explore_stub" && msg.exploreStub) {
      flushExplore();
      exploreStartMsgIdx = i;
      exploreEndMsgIdx = i;
      explore = { items: [], stub: msg.exploreStub, stubId: msg.id };
      flushExplore();
      continue;
    }

    if (isExploreRunRow(msg.type)) {
      startExplore(i).items.push(msg);
    } else {
      flushExplore();

      // ── user_with_parts folding ──────────────────────────────
      // The user row plus the part rows the pre-pass assigned to it.  The
      // words lead; the parts follow in the order they were logged.
      if (msg.type === "user") {
        const owned = partsByUser.get(i);
        if (owned && owned.length > 0) {
          const parts = owned.map((k) => messages[k]);
          const items = [msg, ...parts];
          const blockId = `block-${msg.id}`;
          blocks.push({
            blockId,
            type: "user_with_parts",
            items,
            rawCount: items.length,
            // The block's span ends at its LAST row — a part row can sit
            // after the user row, so the user row's own index is not the
            // end of the block.
            anchorToLatest: Math.max(i, ...owned) === lastIdx,
            hasFollowUpReply: false,
            // ADR-050 C3: see note in the explore_group branch above.
            isLive: false,
          });
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
 * ADR-081 §4.2: index of the folded block containing the raw coordinate
 * `rawIdx`, or -1 when the target is not inside any block (e.g. a live-buffer
 * entry that predates the loaded window).
 *
 * `rawIdx` (and `rawBase`, the window's first coordinate) are measured in RAW
 * JSONL entries — the same unit as `messageOffset` / `messageLimit` and as a
 * stub's `start` / `end`. Blocks are consecutive spans in that dimension: a
 * plain block covers `rawCount` entries from the cursor, a lazy stub block
 * covers its advertised `[start, end)` and moves the cursor to `end`. Array
 * indices cannot be used here (t-01412679): a stub is one array row over N
 * entries, so array math lands every later block at the wrong coordinate.
 */
export function blockIndexOfRawMessage(
  blocks: readonly MessageBlock[],
  rawIdx: number,
  rawBase: number,
): number {
  // Two address schemes, both in RAW file coordinates:
  //  - a stub block is addressed by the span its stub advertises — its rows are
  //    not on the client at all, so only the coordinates know where it sits;
  //  - a raw block covers `rawCount` consecutive entries from the cursor, which
  //    is exactly its `items.length`.
  // Array indices cannot be used for either: one stub record spans N entries,
  // so array math lands every later block at the wrong coordinate (review I4).
  let cursor = rawBase;
  for (let i = 0; i < blocks.length; i++) {
    const b = blocks[i];
    if (b.stub) {
      if (rawIdx >= b.stub.start && rawIdx < b.stub.end) return i;
      // Resynchronise on the block's own coordinate: the array skipped a hole
      // the cursor walk cannot see.
      cursor = Math.max(cursor, b.stub.end);
      continue;
    }
    if (rawIdx >= cursor && rawIdx < cursor + b.rawCount) return i;
    cursor += b.rawCount;
  }
  return -1;
}
