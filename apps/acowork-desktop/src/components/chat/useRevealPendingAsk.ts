/**
 * `useRevealPendingAsk` — bring the reader to the run the router is asking about.
 *
 * The layout hangs an addressing question on the block it belongs to
 * (`RunBlock.pendingAsk`), and the block is anchored where its exchange STARTED
 * (see `chatTranscriptGroups`). Both are deliberate: a block that moves is a
 * block that breaks the question/answer pairing, and a question drawn at the
 * transcript's newest row asks about a run the reader cannot see.
 *
 * The cost of that pair of decisions is paid here, and it is the whole point of
 * this hook: the human typed at the composer, at the bottom, and the block that
 * now holds the question may be hundreds of rows up. Nothing moves on its own,
 * so the surface moves the VIEWPORT — once, on the frame the question first
 * appears, and only when its block is off screen.
 *
 * Deliberately not a notification: no toast, no badge, no animation loop. The
 * reader is taken to the question and can answer it in place.
 */
import { useEffect, useRef, type RefObject } from "react";
import type { VirtualRowListHandle } from "../common/VirtualRowList";
import type { TranscriptItem } from "./chatTranscriptGroups";

/** How to land on the block: centred, so the question at its tail is on screen
 *  with the rounds above it rather than alone at the edge. */
const ALIGN = "center" as const;

/** Every open addressing question in this layout, with the block key that draws
 *  it (`null` when the layout could not resolve one — then nothing to scroll to). */
function openAsks(
  items: TranscriptItem[],
  keyOfSeq: Map<number, string>,
): { seq: number; key: string | null }[] {
  const out: { seq: number; key: string | null }[] = [];
  for (const item of items) {
    if (item.kind !== "run" || !item.pendingAsk) continue;
    out.push({
      seq: item.pendingAsk.seq,
      key: keyOfSeq.get(item.anchorRow.seq) ?? null,
    });
  }
  return out;
}

export function useRevealPendingAsk(
  items: TranscriptItem[],
  keyOfSeq: Map<number, string>,
  listRef: RefObject<VirtualRowListHandle | null>,
): void {
  /** Questions this hook has already accounted for. A card that is already open
   *  when the transcript mounts (a reload with an unanswered question) is
   *  history, not news — moving the viewport for it would fight the surface's
   *  own open-at-the-bottom behaviour.
   *
   *  The baseline is per MOUNT, so it is per session: this is safe because both
   *  surfaces key the transcript view by session id (`AgentChatView` by
   *  `chatSessionId`, `GroupChatPanel` by `groupId`) and a switch therefore
   *  remounts it. Drop that key and this ref would carry one session's seqs into
   *  the next, and the first transcript read would scroll. */
  const seen = useRef<Set<number> | null>(null);

  useEffect(() => {
    const asks = openAsks(items, keyOfSeq);
    if (!seen.current) {
      // First pass over a LOADED transcript establishes the baseline. An empty
      // layout is not a transcript — the window arrives asynchronously, and
      // baselining the empty frame would make every card that lands with the
      // first page look like news, scrolling the reader on open.
      if (items.length === 0) return;
      seen.current = new Set(asks.map((a) => a.seq));
      return;
    }
    const fresh = asks.filter((a) => !seen.current!.has(a.seq));
    for (const a of asks) seen.current.add(a.seq);
    if (fresh.length === 0) return;
    // The newest question is the one the human is waiting on; older fresh ones
    // (a batched page insert) are left to the reader.
    const target = fresh[fresh.length - 1]!;
    const list = listRef.current;
    if (!list || !target.key) return;
    if (list.isKeyInViewport(target.key)) return;
    list.scrollToKey(target.key, ALIGN);
  }, [items, keyOfSeq, listRef]);
}
