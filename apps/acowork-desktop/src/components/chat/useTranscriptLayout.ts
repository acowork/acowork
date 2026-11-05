/**
 * The virtualized-transcript adapters both Chat surfaces need, in one place.
 *
 * The layout is not the transcript (`chatTranscriptGroups`), and it is not the
 * list shell (`VirtualRowList`): it is the membrane between them — keys, height
 * estimates, per-item attributes and the seq→item index a locate jump needs.
 * Every Chat surface (the 1:1 view and the group panel) renders the same
 * transcript through the same shell, so this glue has exactly one correct
 * spelling; two copies meant a fix to the estimate or the key scheme had to be
 * remembered in the second file.
 *
 * Three rules the adapters must keep (see `VirtualRowList` for the shell side):
 * - **Keys are scope-qualified.** The shell's measured-height cache is
 *   module-level and `seq` is unique only *within* one conversation, so the key
 *   carries the scope id (a chat session, a group).
 * - **Rows are reached through `rowsRef`, not through the closure.** A locate
 *   jump is a stamp-keyed effect; re-creating it per `rows` tick would re-arm
 *   the jump on every poll.
 * - **A folded row is not an item.** `keyOfSeq` is what turns the seq of a row
 *   now folded inside a block into the block that holds it (ADR-097).
 */

import { useCallback, useMemo, useRef, type RefObject } from "react";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import {
  blockSeqs,
  buildTranscriptItems,
  type TranscriptItem,
} from "./chatTranscriptGroups";

export interface TranscriptLayoutView {
  /** `buildTranscriptItems`'s output: the ordered items plus the seq→key index. */
  layout: ReturnType<typeof buildTranscriptItems>;
  items: TranscriptItem[];
  /** The current rows, for callbacks that must not close over `rows`. */
  rowsRef: RefObject<ChatMessageRow[]>;
  /** Stable across `rows` ticks: the locate fallback reads it, not `layout`. */
  keyOfSeqRef: RefObject<Map<number, string>>;
  /** The row an item is anchored at (a block's anchor row). */
  itemOf: (item: TranscriptItem) => ChatMessageRow;
  virtualKeyOf: (item: TranscriptItem) => string;
  virtualEstimate: (item: TranscriptItem) => number;
  virtualAttrs: (item: TranscriptItem) => { "data-seq": number };
  /** Which seqs one item renders, so a flash lights the block that holds the row
   *  a search hit — including the rows folded inside it. */
  seqsByItem: Map<string, Set<number>>;
}

/**
 * @param rows the loaded window, oldest first.
 * @param scopeKey the conversation the rows belong to (chat session id, group
 *   id): part of every key, because heights are cached per key process-wide.
 */
export function useTranscriptLayout(
  rows: ChatMessageRow[],
  scopeKey: string,
): TranscriptLayoutView {
  const rowKeyOf = useCallback(
    (r: ChatMessageRow) => `${scopeKey}:${r.seq}`,
    [scopeKey],
  );
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  // Coarse pre-measure estimate from body length; `measureElement` corrects it on
  // first mount and the shell's cache keeps the real height after.
  const estimateRow = useCallback((r: ChatMessageRow) => {
    const body = r.body ?? "";
    const lines = Math.max(1, Math.ceil(body.length / 70));
    return Math.min(480, 32 + lines * 22);
  }, []);
  // The session-level fold (ADR-106's presentation contract): the reference rows
  // of one execution session become ONE block item, and the chat's own question
  // rows they were built from fold inside it. Memoized on the window, so a
  // composer keystroke keeps every item identity that the row views' memo needs.
  const layout = useMemo(
    () => buildTranscriptItems(rows, rowKeyOf),
    [rows, rowKeyOf],
  );
  const items = layout.items;
  const keyOfSeqRef = useRef(layout.keyOfSeq);
  keyOfSeqRef.current = layout.keyOfSeq;
  const itemOf = useCallback(
    (item: TranscriptItem): ChatMessageRow =>
      item.kind === "run" ? item.anchorRow : item.row,
    [],
  );
  const virtualKeyOf = useCallback(
    (item: TranscriptItem) => rowKeyOf(itemOf(item)),
    [rowKeyOf, itemOf],
  );
  // A block is a header plus one line per visible round; collapsed it is a pill.
  // `measureElement` corrects the estimate on mount and the shell caches it.
  const virtualEstimate = useCallback(
    (item: TranscriptItem) =>
      item.kind === "run"
        ? 64 + 34 * Math.min(item.rounds.length, 3)
        : estimateRow(item.row),
    [estimateRow],
  );
  const virtualAttrs = useCallback(
    (item: TranscriptItem) => ({ "data-seq": itemOf(item).seq }),
    [itemOf],
  );
  const seqsByItem = useMemo(() => {
    const m = new Map<string, Set<number>>();
    for (const item of items) {
      m.set(
        virtualKeyOf(item),
        new Set(item.kind === "run" ? blockSeqs(item) : [item.row.seq]),
      );
    }
    return m;
  }, [items, virtualKeyOf]);

  return {
    layout,
    items,
    rowsRef,
    keyOfSeqRef,
    itemOf,
    virtualKeyOf,
    virtualEstimate,
    virtualAttrs,
    seqsByItem,
  };
}