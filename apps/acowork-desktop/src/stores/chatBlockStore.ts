/**
 * Execution-block store (ADR-106 D3/D5/D9).
 *
 * The transcript renders one block per EXECUTION SESSION (ADR-106's presentation
 * contract), so a block asks for every round its session has in the loaded
 * window, and every row of a window asks for something. One HTTP call per ref
 * would be a request storm on scroll, so reads are BATCHED per chat: whichever
 * component asks first queues the refs, and the queue flushes on a microtask-free
 * timer (a frame) with the refs this chat does not already hold.
 *
 * What is deliberately NOT here: any invalidation of a terminal block. D5 gives
 * the cache no invalidation path because a terminal projection cannot change, so
 * the only re-reads are a `pending` block's re-read — driven by the round-state
 * signal (ADR-106 revision 2026-11-03), with a slow interval as the loss
 * backstop — and an explicit retry on an `unreachable` one.
 */

import { useEffect, useMemo } from "react";
import { create } from "zustand";
import { fetchChatBlocks, type ChatBlockEntry } from "../lib/chatBlocks";

/** How long a `pending` block waits between backstop re-reads. The round-state
 *  signal is what flips a finished block promptly; this interval only mops up a
 *  dropped frame (QoS 1 covers broker loss, not a webview that was closed), so
 *  it is deliberately long: a run of minutes must cost almost nothing. */
const PENDING_POLL_MS = 15000;

/** How long after a round-state signal to re-read once more if the block is
 *  still `pending`. The signal is published at the turn boundary, while the
 *  round's terminal record is appended by the session's single writer (ADR-092)
 *  — measured ~8 ms apart on disk, and the batched read leaves the request ~16
 *  ms later, so the first answer can legitimately still say `pending`. One
 *  bounded retry, not a faster poll. */
const ROUND_SIGNAL_RETRY_MS = 500;

/** Batch window: refs requested during one render pass ride one request. */
const BATCH_MS = 16;

interface ChatBlockState {
  /** By ref (`instance|session|round`) — a ref names one round of one session,
   *  so it cannot collide across chats. */
  blocks: Record<string, ChatBlockEntry>;
  /** Refs queued for the next batch, by chat. */
  pending: Record<string, Set<string>>;
  /** Chats with a request in flight, so a flush while one is running does not
   *  re-issue the same refs. */
  inFlight: Record<string, boolean>;
  /** Queue refs this chat needs but does not hold yet. */
  ensure: (chatId: string, refs: string[]) => void;
  /** Re-read refs regardless of what is held (the pending poll, the retry). */
  reload: (chatId: string, refs: string[]) => void;
}

const timers = new Map<string, ReturnType<typeof setTimeout>>();

/** Mounted readers: chat → ref → the hook keys watching that ref.
 *
 *  A round-state signal names a round, not a Chat surface, and the store holds
 *  no chat → refs map of its own (the transcript owns that knowledge). Without
 *  this registry the signal would have to request blocks nobody is looking at,
 *  or be dropped; with it, a signal re-reads exactly the mounted block that
 *  named the round. Held outside the store because it is component lifetime,
 *  not cache content — putting it in state would re-render every block on every
 *  mount. */
const watchers = new Map<string, Map<string, Set<string>>>();

function watchRefs(chatId: string, refs: string[], key: string) {
  let byRef = watchers.get(chatId);
  if (!byRef) {
    byRef = new Map();
    watchers.set(chatId, byRef);
  }
  for (const ref of refs) {
    const holders = byRef.get(ref) ?? new Set<string>();
    holders.add(key);
    byRef.set(ref, holders);
  }
}

function unwatchRefs(chatId: string, refs: string[], key: string) {
  const byRef = watchers.get(chatId);
  if (!byRef) return;
  for (const ref of refs) {
    const holders = byRef.get(ref);
    if (!holders) continue;
    holders.delete(key);
    if (holders.size === 0) byRef.delete(ref);
  }
  if (byRef.size === 0) watchers.delete(chatId);
}

/**
 * Is this held block still able to change on its own?
 *
 * A round that has not reached a terminal state, or that is still offering a
 * card, moves without the Chat transcript gaining a row (ADR-106's zero-write),
 * so a successful read of it is provisional in the same way a `pending` read is
 * — it must be re-read whenever the block is asked for, not cached as an answer.
 */
function isLiveBlock(e: ChatBlockEntry): boolean {
  return e.status === "ok" && (e.projection?.terminal === false || !!e.projection?.card);
}

export const useChatBlockStore = create<ChatBlockState>((set, get) => {
  const flush = (chatId: string) => {
    timers.delete(chatId);
    const state = get();
    if (state.inFlight[chatId]) {
      // Still busy: keep the queue for the next tick rather than dropping the
      // refs, which is what would leave a rendered block empty forever.
      const again = setTimeout(() => flush(chatId), BATCH_MS);
      timers.set(chatId, again);
      return;
    }
    const queue = state.pending[chatId];
    if (!queue || queue.size === 0) return;
    const refs = [...queue];
    const nextPending = { ...get().pending };
    delete nextPending[chatId];
    set({ pending: nextPending, inFlight: { ...get().inFlight, [chatId]: true } });
    fetchChatBlocks(chatId, refs)
      .then((entries) => {
        const held = { ...get().blocks };
        for (const e of entries) held[e.ref] = e;
        set({ blocks: held });
      })
      .catch((err: unknown) => {
        // The batch failed, not the block: report each requested ref as
        // unreachable (which renders the retry affordance) rather than leaving
        // it absent, which a surface would read as "no block exists".
        const detail = err instanceof Error ? err.message : String(err);
        const held = { ...get().blocks };
        for (const ref of refs) {
          if (held[ref]?.status === "ok") continue;
          const parts = ref.split("|");
          held[ref] = {
            ref,
            agent_instance_id: parts[0] ?? "",
            agent_session_id: parts[1] ?? "",
            round_id: parts[2] ?? "",
            status: "unreachable",
            detail,
          };
        }
        set({ blocks: held });
      })
      .finally(() => {
        set({ inFlight: { ...get().inFlight, [chatId]: false } });
        if ((get().pending[chatId]?.size ?? 0) > 0) {
          timers.set(chatId, setTimeout(() => flush(chatId), BATCH_MS));
        }
      });
  };

  const queueRefs = (chatId: string, refs: string[], force: boolean) => {
    if (!chatId || refs.length === 0) return;
    const state = get();
    const held = state.blocks;
    // A `pending` read is provisional by contract (D9) — it says "still
    // running" and nothing about the card the round may have parked on
    // since. Holding one is NOT a reason to skip a re-read: a block
    // re-mounted from a held-but-stale `pending` entry would render the
    // pre-card state (no card, no buttons) until the interval backstop
    // fired — and a webview that throttles timers does not fire it at all.
    // This was the "second approval card never shows in the Chat" symptom:
    // the doorbell lands while the Chat surface is unmounted, the re-entry
    // read is skipped as already-held, and the block stays cardless.
    //
    // A round that is still running or still offering a card is the same
    // shape of provisional: its projection can change without the Chat
    // transcript gaining a row (ADR-106's zero-write), so a successful read
    // is not a settled answer either. This is what left a pause card's
    // Continue rendered after the run had been resumed — the withdrawal
    // happened while this surface was away, the bell reached no watcher, and
    // the held `waiting` copy was never re-read.
    const wanted = refs.filter(
      (r) =>
        force || !held[r] || held[r]!.status === "pending" || isLiveBlock(held[r]!),
    );
    if (wanted.length === 0) return;
    const queue = new Set(state.pending[chatId] ?? []);
    wanted.forEach((r) => queue.add(r));
    set({ pending: { ...state.pending, [chatId]: queue } });
    // Re-arm, never skip: the batch window is what it is, and a map entry that
    // outlives its timer would wedge this chat's reads forever — the guard
    // would refuse to schedule while nothing is left to flush the queue.
    const stale = timers.get(chatId);
    if (stale) clearTimeout(stale);
    timers.set(chatId, setTimeout(() => flush(chatId), BATCH_MS));
  };

  return {
    blocks: {},
    pending: {},
    inFlight: {},
    ensure: (chatId, refs) => queueRefs(chatId, refs, false),
    reload: (chatId, refs) => queueRefs(chatId, refs, true),
  };
});

/** Delimiter for the ref-set keys below. Refs are `instance|session|round`, so
 *  `|` is taken; a newline cannot appear in an id and keeps the key readable. */
const REF_SET_SEP = "\n";

/** Refs of one block, read on demand (ADR-106 D3/D5/D9).
 *
 * A block is a whole EXECUTION SESSION (design 27 §5.5), so it owns several
 * rounds and needs them all to draw its lines: it asks once for its whole ref
 * list, and the reads are batched per chat — N rounds in one render pass are one
 * request, not N. The list is joined into a string key because the caller
 * rebuilds the array on every render: the effect dependencies and the store read
 * must ride the CONTENT, not the identity.
 *
 * Re-read rule: only a `pending` ref is re-read, and only while its block is
 * mounted. The round-state signal is what makes that re-read prompt; the
 * interval is the backstop for a frame the webview never saw. A block that never
 * re-reads looks frozen; a settled one that keeps re-reading is a client that
 * does not believe history (D5).
 */
export function useChatBlocks(
  chatId: string,
  refs: readonly string[],
): Record<string, ChatBlockEntry | undefined> {
  const ensure = useChatBlockStore((s) => s.ensure);
  const reload = useChatBlockStore((s) => s.reload);
  // The whole record, not a derived object: a selector that built a new object
  // per call would re-render the block on every store tick (the transcript's
  // memo contract). Record identity changes only when a read lands.
  const blocks = useChatBlockStore((s) => s.blocks);
  const key = refs.join(REF_SET_SEP);

  const entries = useMemo(() => {
    const out: Record<string, ChatBlockEntry | undefined> = {};
    if (!key) return out;
    for (const ref of key.split(REF_SET_SEP)) out[ref] = blocks[ref];
    return out;
  }, [blocks, key]);

  useEffect(() => {
    if (!chatId || !key) return;
    ensure(chatId, key.split(REF_SET_SEP));
  }, [chatId, key, ensure]);

  // Registration is on mount, not on `pending`: a round that parks on a card
  // (`terminal = false`) is not pending-by-cache-status yet, and a block that
  // has not asked is exactly the one a signal must reach.
  useEffect(() => {
    if (!chatId || !key) return;
    const refsList = key.split(REF_SET_SEP);
    watchRefs(chatId, refsList, key);
    return () => unwatchRefs(chatId, refsList, key);
  }, [chatId, key]);

  const anyPending = refs.some((r) => entries[r]?.status === "pending");
  useEffect(() => {
    if (!chatId || !key || !anyPending) return;
    // Recurring, never one-shot. A block that stays `pending` sees no change in
    // any dependency — ADR-106's zero-write means the transcript gains no row
    // while a run finishes — so a `setTimeout` armed by this effect fired once
    // and was never re-armed: the block sat at "running" until an unmount /
    // remount (the "view run → back" round trip) armed it again.
    const timer = setInterval(() => {
      // Read the pending set at fire time: a block whose round settled while the
      // timer ran must not be re-requested (D5 — a finished round is history).
      const held = useChatBlockStore.getState().blocks;
      const pending = key
        .split(REF_SET_SEP)
        .filter((r) => held[r]?.status === "pending");
      if (pending.length) reload(chatId, pending);
    }, PENDING_POLL_MS);
    return () => clearInterval(timer);
  }, [chatId, key, anyPending, reload]);

  return entries;
}

/** Tell the store a round changed state (ADR-106 revision 2026-11-03).
 *
 * Called from the MQTT event dispatcher with the ref the signal names. Re-reads
 * that ref if a mounted block holds it — and only then, so a background Chat
 * never pays for a run it is not showing. Returns whether anyone was listening,
 * which is what makes the gap this event closes measurable in a test.
 */
export function noteRoundState(chatId: string, ref: string): boolean {
  if (!chatId || !ref) return false;
  if (!(watchers.get(chatId)?.has(ref) ?? false)) return false;
  useChatBlockStore.getState().reload(chatId, [ref]);
  setTimeout(() => {
    // One bounded retry: the signal can outrun the writer's terminal record by a
    // few ms, and a read that landed in that window still says `pending`.
    const held = useChatBlockStore.getState().blocks[ref];
    const stillWatched = watchers.get(chatId)?.has(ref) ?? false;
    if (stillWatched && held?.status === "pending") {
      useChatBlockStore.getState().reload(chatId, [ref]);
    }
  }, ROUND_SIGNAL_RETRY_MS);
  return true;
}
