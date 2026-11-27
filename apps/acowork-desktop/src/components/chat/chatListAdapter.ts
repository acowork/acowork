/**
 * chatListAdapter - The v2 ChatListAdapter: data-driven contract between
 * chatStore + chatAdapterStore and VirtualMessageList.
 *
 * ADR-050 C3 — replaces the v1 `useChatListAdapter.ts` (ADR-041) with a
 * structure that:
 *
 *   1. Owns a per-(agentId, sessionId) singleton store built on the
 *      `useSyncExternalStore` pattern.  Multiple components can share
 *      the same adapter instance without Context plumbing.
 *
 *   2. Exposes `blocks` as the single data source for VML.  `blocks` is
 *      a derived value: `foldMessages(history ++ liveBuffer atTail)`,
 *      with `isLive: true` on every block that contains at least one
 *      entry from chatAdapterStore's liveBuffer.
 *
 *   3. Exposes 7 UI primitives: `loadPrevPage / loadNextPage /
 *      scrollToTop / scrollToBottom / scrollToPosition / isAtTail /
 *      hasPendingFlush`.  No `jumpToLatest / jumpToOldest` aliases.
 *
 *   4. Exposes a `subscribe(cb)` API that emits `liveUpdate /
 *      pageLoaded / flushAvailable` events.  Replaces the v1 model's
 *      `jumpTarget` + `clearJumpTarget` ad-hoc signaling.
 *
 *   5. No scroll-position state lives in the adapter itself; the scroll
 *      controller (C4) is the single layer that reads DOM and drives
 *      scroll-side effects via adapter queries (isAtTail / hasOlder).
 *
 * Why a separate file
 * -------------------
 * v1 (`useChatListAdapter.ts`) is still consumed by ChatPanel /
 * VirtualMessageList in C2 — C3 can't replace it without a coordinated
 * consumer migration.  This file lands the v2 implementation behind a
 * new export; C5 will switch consumers to v2 and the v1 file will be
 * deleted.
 */
import { useMemo, useSyncExternalStore } from "react";
import { useChatStore, expandWindowToTurnBoundary } from "../../stores/chatStore";
import {
  type ChatAdapterEvent,
  subscribeChatAdapter,
  getLiveBuffer,
  releaseAdapterSession,
} from "./chatAdapterStore";
import { foldMessages, type MessageBlock } from "./messageFolder";
import { DeliverablesMemo } from "../../lib/deriveDeliverables";
import type { ChatMessage } from "../../lib/types";
import { isAtTail } from "../../lib/paginationUtils";

// ── Constants ──────────────────────────────────────────────────────────────

const PAGINATION_PAGE_SIZE = 50;
const EMPTY_BLOCKS: readonly MessageBlock[] = Object.freeze([]);

// ── Types ──────────────────────────────────────────────────────────────────

/**
 * Adapter event types — the v2 subscribe contract.  Mirrors the
 * `AdapterEvent` shape in ADR-050 §4.1.
 */
export type AdapterEvent =
  | { type: "liveUpdate"; reason: "streamDelta" | "recordComplete" | "userSent" | "flush" }
  | { type: "pageLoaded"; direction: "prev" | "next"; offset: number; limit: number; total: number }
  | { type: "flushAvailable"; pendingCount: number }
  | { type: "scrollToIndex"; index: number };

/**
 * UI command surface for the v2 adapter.  See ADR-050 §4.1.
 *
 * Note: named `ChatListAdapterV2` (NOT `ChatListAdapter`) because
 * v1 (`useChatListAdapter.ts`) exports an interface of the same name.
 * C5 will resolve the collision by removing v1 and renaming v2.
 */
export interface ChatListAdapterV2 {
  // ── Data output ──
  readonly blocks: readonly MessageBlock[];
  readonly totalBlocks: number;
  readonly messageOffset: number;
  readonly messageLimit: number;
  readonly messageTotal: number;
  /**
   * Monotonically increasing counter bumped whenever the cache is
   * REPLACED (jump operations: scrollToTop / scrollToBottom /
   * loadPageForMessage).  The controller's prepend-anchoring effect
   * watches this to distinguish genuine prepends (loadPrevPage) from
   * full-cache replacements — anchoring must be skipped on replace.
   */
  readonly cacheGeneration: number;

  // ── State queries ──
  readonly isAtTail: () => boolean;
  readonly hasPendingFlush: () => boolean;
  readonly hasOlder: boolean;
  readonly hasNewer: boolean;
  readonly isLoading: boolean;

  // ── Pagination primitives ──
  loadPrevPage(): Promise<void>;
  loadNextPage(): Promise<void>;
  /**
   * Load the initial page for a session that has no cached data.
   * Always loads the NEWEST page (tail).  Delegates to the chatStore's
   * ensureLatestInCache.
   */
  loadInitialPage(): Promise<void>;
  /**
   * Load the page containing the given absolute message index.
   * Used by session-switch restoration to ensure the content at the
   * user's saved position is in the rendering window before the
   * controller calls scrollToPosition.
   */
  loadPageForMessage(messageIndex: number): Promise<void>;
  /**
   * Load the page that should contain the given blockId, using
   * hintMessageIndex (the absolute message index of the first visible
   * block at save time) to compute the correct page.  If the blockId
   * is not in the hinted page, tries one page forward (compaction may
   * have shifted messages).  Used by session-switch restoration to
   * reload the user's browsing position.  After this resolves, the
   * controller calls vml.scrollToBlockId(blockId) and falls back to
   * scrollToBottom if the block is not found.
   */
  loadPageForBlockId(blockId: string, hintMessageIndex: number): Promise<void>;

  // ── Scroll primitives ──
  scrollToTop(): Promise<void>;
  scrollToBottom(): Promise<void>;
  scrollToPosition(blockIndex: number): Promise<void>;

  // ── Event subscription ──
  subscribe(cb: (event: AdapterEvent) => void): () => void;
}

// ── Per-session singleton store ────────────────────────────────────────────

/**
 * The store shape held outside React.  The adapter subscribes to BOTH
 * `useChatStore` (history / pagination cursor) and `chatAdapterStore`
 * (liveBuffer).  When either updates, `version` is bumped and a
 * snapshot is rebuilt.
 */
interface AdapterStoreState {
  agentId: string;
  sessionId: string;
  /** Bumped on every internal mutation; useSyncExternalStore reads this. */
  version: number;
  /** Cached snapshot — the snapshot the React subscription observes. */
  snapshot: AdapterSnapshot;
}

interface AdapterSnapshot {
  blocks: readonly MessageBlock[];
  messageOffset: number;
  messageLimit: number;
  messageTotal: number;
  isLoading: boolean;
  hasOlder: boolean;
  hasNewer: boolean;
  atTail: boolean;
  pendingCount: number;
  hasPendingFlush: boolean;
}

/** Per-session store instance — kept in a module-level Map so two
 *  components with the same (agentId, sessionId) share the same store. */
const stores = new Map<string, AdapterStore>();

/** Global listener fan-out — when ANY per-session store bumps its
 *  version, every per-session store's local subscribers fire (each
 *  store filters by its own session key). */
type LocalListener = () => void;

class AdapterStore {
  readonly agentId: string;
  readonly sessionId: string;
  private state: AdapterStoreState;
  private listeners = new Set<LocalListener>();
  /** Event subscribers (for the `subscribe(cb)` API on ChatListAdapter). */
  private eventListeners = new Set<(event: AdapterEvent) => void>();

  /** Per-session deliverables derivation memo: keeps unchanged banners' object
   *  identity across the snapshot recomputes that every stream delta triggers.
   *  Lives and dies with this adapter (one per session). */
  private deliverablesMemo = new DeliverablesMemo();

  /** Unsubscribers for the two upstream store subscriptions. */
  private unsubHistory: (() => void) | null = null;
  private unsubAdapter: (() => void) | null = null;

  /** In-flight jump-to-bottom, shared with duplicate callers (see
   *  scrollToBottom). */
  private bottomJump: Promise<void> | null = null;

  constructor(agentId: string, sessionId: string) {
    this.agentId = agentId;
    this.sessionId = sessionId;
    this.state = {
      agentId,
      sessionId,
      version: 0,
      snapshot: this.buildSnapshot(),
    };
    this.bindUpstream();
  }

  /** Tear down — used when the LAST React subscriber disconnects. */
  teardown(): void {
    this.unsubHistory?.();
    this.unsubAdapter?.();
    this.unsubHistory = null;
    this.unsubAdapter = null;
    this.listeners.clear();
    this.eventListeners.clear();
  }

  // ── useSyncExternalStore hooks ──

  getSnapshot = (): AdapterSnapshot => this.state.snapshot;
  subscribeLocal = (cb: LocalListener): (() => void) => {
    this.listeners.add(cb);
    return () => { this.listeners.delete(cb); };
  };

  /** Public event subscription — used by both the React hook facade
   *  AND by the controller (C4).  Returns the unsubscribe function. */
  subscribe = (cb: (event: AdapterEvent) => void): (() => void) => {
    this.eventListeners.add(cb);
    return () => { this.eventListeners.delete(cb); };
  };

  // ── ChatListAdapter public methods ──

  loadPrevPage = async (): Promise<void> => {
    const snap = this.state.snapshot;
    if (this.isLoading()) return;
    if (!snap.hasOlder) return;
    const nextOffset = Math.max(0, snap.messageOffset - PAGINATION_PAGE_SIZE);
    await useChatStore
      .getState()
      .loadSessionMessages(this.agentId, this.sessionId, nextOffset, PAGINATION_PAGE_SIZE);
  };

  loadNextPage = async (): Promise<void> => {
    const snap = this.state.snapshot;
    if (this.isLoading()) return;
    if (!snap.hasNewer) return;
    const nextOffset = snap.messageOffset + snap.messageLimit;
    await useChatStore
      .getState()
      .loadSessionMessages(this.agentId, this.sessionId, nextOffset, PAGINATION_PAGE_SIZE);
  };

  loadInitialPage = async (): Promise<void> => {
    const ss = useChatStore.getState();
    await ss.ensureLatestInCache(this.agentId, this.sessionId);
  };

  loadPageForMessage = async (messageIndex: number): Promise<void> => {
    const ss = useChatStore.getState();
    const offset = Math.max(0, Math.floor(messageIndex / PAGINATION_PAGE_SIZE) * PAGINATION_PAGE_SIZE);
    await ss.loadSessionMessages(this.agentId, this.sessionId, offset, PAGINATION_PAGE_SIZE, true);
    this.cacheGeneration++;
  };

  loadPageForBlockId = async (blockId: string, hintMessageIndex: number): Promise<void> => {
    const ss = useChatStore.getState();
    const pageOffset = Math.max(0, Math.floor(hintMessageIndex / PAGINATION_PAGE_SIZE) * PAGINATION_PAGE_SIZE);

    // 1. Load the hinted page (replaceCache for a clean window).
    await ss.loadSessionMessages(this.agentId, this.sessionId, pageOffset, PAGINATION_PAGE_SIZE, true);
    this.cacheGeneration++;

    // 2. Check if the blockId is in the loaded blocks.
    if (this.state.snapshot.blocks.some((b) => b.blockId === blockId)) return;

    // 3. Not found - try one page forward.  Compaction may have replaced
    //    older messages with a summary, shifting the target message to a
    //    later page.  More than one page of shift is extremely unlikely.
    const cur = ss.agentStates[this.agentId]?.sessionStates[this.sessionId];
    const total = cur?.messageTotal ?? 0;
    const nextOffset = pageOffset + PAGINATION_PAGE_SIZE;
    if (nextOffset < total) {
      await ss.loadSessionMessages(this.agentId, this.sessionId, nextOffset, PAGINATION_PAGE_SIZE, true);
      this.cacheGeneration++;
    }
    // If still not found, leave the last loaded page.  The controller's
    // init-scroll will try scrollToBlockId and fall back to scrollToBottom.
  };

  /**
   * Scroll to the OLDEST block.  If the head page (offset=0) is
   * already cached, just signal the controller.  Otherwise REPLACE
   * the cache with the head page in a single request — no intermediate
   * pages are loaded.
   */
  scrollToTop = async (): Promise<void> => {
    const ss = useChatStore.getState();
    const cur = ss.agentStates[this.agentId]?.sessionStates[this.sessionId];
    if (!cur) return;
    // Already at head?
    if (cur.messageOffset === 0 && cur.messages.length > 0) {
      this.bumpVersion();
      return;
    }
    // Single jump: replace cache with offset=0 page.
    await ss.loadSessionMessages(this.agentId, this.sessionId, 0, PAGINATION_PAGE_SIZE, true);
    this.cacheGeneration++;
    this.bumpVersion();
  };

  /**
   * Scroll to the NEWEST block (including liveBuffer).  If the tail
   * page is already cached, just signal the controller.  Otherwise
   * REPLACE the cache with the tail page in a single request.
   */
  scrollToBottom = (): Promise<void> => {
    // Coalesce concurrent jumps.  The arrow click and the controller's
    // followMode-convergence effect both call jumpToBottom about one frame
    // apart, and loadSessionMessages aborts the previous in-flight request
    // (and bumps `loadSequence`) BEFORE any guard — so a second call used to
    // throw away the page the first one was fetching, leaving the pane pinned
    // to the bottom of a mid-history window.  Duplicates now await the same
    // jump instead of starting a competing load.
    if (!this.bottomJump) {
      this.bottomJump = this.runScrollToBottom().finally(() => {
        this.bottomJump = null;
      });
    }
    return this.bottomJump;
  };

  private runScrollToBottom = async (): Promise<void> => {
    const ss = useChatStore.getState();
    const cur = ss.agentStates[this.agentId]?.sessionStates[this.sessionId];
    if (!cur) return;
    // Already at tail?  NOTE: this intentionally uses `limit > 0`
    // (NOT `isAtTail()`) because `limit === 0` means "no page loaded" -
    // scrollToBottom must still trigger an HTTP load in that case.
    //
    // No bumpVersion on this branch: the cursor didn't move, no
    // chatStore write happened, and the liveBuffer is already driven
    // by subscribeChatAdapter.  A gratuitous version bump here used to
    // feed the "effect → setState → effect" loop driven by the
    // contradiction-resolver effect in useScrollController (followMode
    // + needsLatestContent holding true across many spurious
    // re-renders — e.g. on resume-from-sleep).
    if (cur.messageLimit > 0 && cur.messageOffset + cur.messageLimit >= cur.messageTotal) {
      return;
    }
    // Single jump: replace cache with the tail page.
    const tailOffset = Math.max(0, cur.messageTotal - PAGINATION_PAGE_SIZE);
    await ss.loadSessionMessages(this.agentId, this.sessionId, tailOffset, PAGINATION_PAGE_SIZE, true);
    // A bare tail page can fold to a LONE explore block (see the helper) —
    // same treatment as the session-open/switch-back path.
    await expandWindowToTurnBoundary(this.agentId, this.sessionId);
    this.cacheGeneration++;
    this.bumpVersion();
  };

  /**
   * Scroll to a specific block index (0 = first / oldest,
   * blocks.length-1 = last / newest).
   *
   * ADR-050 §4.1: if the target block is in the current window,
   * signal the controller to scrollToIndex.  If it's in an unloaded
   * page, walk pages until the target is in range, then signal.
   *
   * The actual vmlRef.scrollToIndex call is owned by the controller
   * (C4); the adapter records the pending target on the snapshot so
   * VML picks it up on its next render.
   */
  scrollToPosition = async (blockIndex: number): Promise<void> => {
    const snap = this.state.snapshot;
    const clamped = Math.max(0, Math.min(blockIndex, snap.blocks.length - 1));
    // Record the pending scroll target.
    this.pendingScrollIndex = clamped;
    this.bumpVersion();
    // Emit the scroll command — the controller (C4) subscribes and
    // executes vmlRef.scrollToIndex(index).
    this.emitEvent({ type: "scrollToIndex", index: clamped });
  };

  /** Pending scroll target index (read by VML on render). */
  pendingScrollIndex: number | null = null;

  /** Bumped on every cache-replacing jump (scrollToTop/Bottom/loadPageForMessage). */
  cacheGeneration = 0;

  isAtTail = (): boolean => this.state.snapshot.atTail;
  hasPendingFlush = (): boolean => this.state.snapshot.hasPendingFlush;
  isLoading = (): boolean => this.state.snapshot.isLoading;

  // ── Internal snapshot ──

  private buildSnapshot(): AdapterSnapshot {
    const ss = useChatStore.getState();
    const agent = ss.agentStates[this.agentId];
    const cur = agent?.sessionStates[this.sessionId];
    const historyMessages = cur?.messages ?? [];
    const offset = cur?.messageOffset ?? 0;
    const limit = cur?.messageLimit ?? 0;
    const total = cur?.messageTotal ?? 0;
    const isLoading = cur?.isLoadingMore ?? false;

    // atTail: window covers [total-limit, total).  ADR-050 post-C5 fix:
    // `limit === 0` (fresh session, no messages loaded yet) is also
    // considered at-tail so the liveBuffer streams fold into blocks
    // immediately - the user must see their optimistic message and any
    // streaming preview even before the initial HTTP load completes.
    //
    // Shared with chatStore's record_complete handler via
    // `isAtTail()` to prevent definition drift.
    const atTail = isAtTail(offset, limit, total);

    // liveBuffer projection - only streaming previews (thinkingStream /
    // assistantStream).  ADR-050 post-C5 fix: pendingUserMessage and
    // pendingRecordComplete are removed; completed records are written
    // directly into messages[] by chatStore.
    const lb = getLiveBuffer(this.agentId, this.sessionId);
    const liveEntries: ChatMessage[] = [];
    if (atTail) {
      if (lb.thinkingStream) liveEntries.push(lb.thinkingStream);
      if (lb.assistantStream) liveEntries.push(lb.assistantStream);
    }
    // Dedup by id — history wins.
    const historyIds = new Set(historyMessages.map((m) => m.id));
    const dedupedLive = liveEntries.filter((m) => !historyIds.has(m.id));
    const pendingCount = dedupedLive.length;
    const hasPendingFlush = pendingCount > 0;

    // blocks
    let blocks: readonly MessageBlock[];
    if (dedupedLive.length === 0) {
      blocks = foldMessages(this.deliverablesMemo.resolve(historyMessages));
    } else {
      const merged = [...historyMessages, ...dedupedLive].sort(
        (a, b) => a.timestamp - b.timestamp,
      );
      const folded = foldMessages(this.deliverablesMemo.resolve(merged));
      // Mark isLive: any block whose items include at least one liveBuffer id.
      const liveIds = new Set(dedupedLive.map((m) => m.id));
      blocks = folded.map((b) => {
        const isLive = b.items.some((it) => liveIds.has(it.id));
        return isLive ? { ...b, isLive: true } : b;
      });
    }

    return {
      blocks,
      messageOffset: offset,
      messageLimit: limit,
      messageTotal: total,
      isLoading,
      hasOlder: offset > 0,
      hasNewer: offset + limit < total,
      atTail,
      pendingCount,
      hasPendingFlush,
    };
  }

  private bumpVersion(): void {
    this.state = {
      ...this.state,
      version: this.state.version + 1,
      snapshot: this.buildSnapshot(),
    };
    this.notifyListeners();
  }

  private notifyListeners(): void {
    for (const cb of this.listeners) cb();
  }

  private emitEvent(event: AdapterEvent): void {
    for (const cb of this.eventListeners) {
      try {
        cb(event);
      } catch (err) {
        // eslint-disable-next-line no-console
        console.error("[chatListAdapter] event subscriber threw:", err);
      }
    }
  }

  private bindUpstream(): void {
    // Subscribe to chatStore (history).  Re-derive on any change.
    this.unsubHistory = useChatStore.subscribe((s, prev) => {
      const a = s.agentStates[this.agentId]?.sessionStates[this.sessionId];
      const p = prev.agentStates[this.agentId]?.sessionStates[this.sessionId];
      if (a === p) return;
      // Capture total for the pageLoaded event meta.
      const totalChanged = a?.messageTotal !== p?.messageTotal;
      const prevOffset = p?.messageOffset ?? 0;
      const prevLimit = p?.messageLimit ?? 0;
      const prevTotal = p?.messageTotal ?? 0;
      this.bumpVersion();
      // Emit a pageLoaded event if the cursor changed.
      if (a && (a.messageOffset !== prevOffset
        || a.messageLimit !== prevLimit
        || a.messageTotal !== prevTotal)) {
        const direction: "prev" | "next" = totalChanged && a.messageTotal > prevTotal
          ? "next"
          : a.messageOffset < prevOffset
            ? "prev"
            : "next";
        this.emitEvent({
          type: "pageLoaded",
          direction,
          offset: a.messageOffset,
          limit: a.messageLimit,
          total: a.messageTotal,
        });
      }
    });

    // Subscribe to chatAdapterStore (liveBuffer / flags).  We piggyback
    // on the existing `subscribeChatAdapter` (event-style) which is
    // already wired up in C2; the v2 adapter translates the
    // `liveUpdate / recordComplete / flushAvailable / pageLoaded`
    // events into ChatListAdapter events.
    this.unsubAdapter = subscribeChatAdapter((event: ChatAdapterEvent) => {
      if (event.sessionKey !== sessionKey(this.agentId, this.sessionId)) return;
      switch (event.kind) {
        case "liveUpdate":
          this.bumpVersion();
          this.emitEvent({ type: "liveUpdate", reason: "streamDelta" });
          break;
        case "recordComplete":
          this.bumpVersion();
          this.emitEvent({ type: "liveUpdate", reason: "recordComplete" });
          break;
        case "flushAvailable":
          this.bumpVersion();
          this.emitEvent({ type: "flushAvailable", pendingCount: this.state.snapshot.pendingCount });
          break;
        case "pageLoaded":
          // Already handled by the chatStore subscription above.
          break;
      }
    });
  }
}

function sessionKey(agentId: string, sessionId: string): string {
  return `${agentId}:${sessionId}`;
}

/**
 * Get (or create) the per-(agentId, sessionId) adapter store singleton.
 * Releases the singleton when the React component that called this hook
 * unmounts — so a long-lived app doesn't accumulate dead stores.
 */
function getOrCreateStore(agentId: string, sessionId: string): AdapterStore {
  const key = sessionKey(agentId, sessionId);
  let store = stores.get(key);
  if (store) return store;
  store = new AdapterStore(agentId, sessionId);
  stores.set(key, store);
  return store;
}

// ── Noop store (for null agent/session) ─────────────────────────────────────
// A frozen noop snapshot + stable subscribe function so that
// useSyncExternalStore can be called UNCONDITIONALLY in the hook body,
// satisfying the Rules of Hooks even when agentId/sessionId is null.

const NOOP_SNAPSHOT: AdapterSnapshot = Object.freeze({
  blocks: EMPTY_BLOCKS,
  messageOffset: 0,
  messageLimit: 0,
  messageTotal: 0,
  isLoading: false,
  hasOlder: false,
  hasNewer: false,
  atTail: false,
  pendingCount: 0,
  hasPendingFlush: false,
});

const noopSubscribe = (_cb: LocalListener): (() => void) => () => {};
const noopGetSnapshot = (): AdapterSnapshot => NOOP_SNAPSHOT;

// ── React hook ─────────────────────────────────────────────────────────────

/**
 * useChatListAdapter v2 — the React entry point.  Subscribes to the
 * per-session adapter store via useSyncExternalStore and exposes a
 * stable ChatListAdapter interface.
 *
 * ADR-050 §4.3: useSyncExternalStore ensures React 18 concurrent mode
 * tearing-safety.  Each (agentId, sessionId) gets its own singleton so
 * components can share adapter state without prop drilling.
 *
 * IMPORTANT: useSyncExternalStore is called UNCONDITIONALLY (Rules of
 * Hooks).  When agentId/sessionId is null we subscribe to a module-level
 * noop store that never emits, then return the frozen NOOP_ADAPTER.
 */
export function useChatListAdapter(
  agentId: string | null,
  sessionId: string | null,
): ChatListAdapterV2 {
  const key = agentId && sessionId ? sessionKey(agentId, sessionId) : null;
  const store = key ? getOrCreateStore(agentId!, sessionId!) : null;

  // useSyncExternalStore MUST be called unconditionally to keep hook
  // order stable across renders where agentId/sessionId toggle null.
  const snapshot = useSyncExternalStore(
    store ? store.subscribeLocal : noopSubscribe,
    store ? store.getSnapshot : noopGetSnapshot,
    store ? store.getSnapshot : noopGetSnapshot,
  );

  // Build the public ChatListAdapter surface.  Memoized so consumers
  // can re-render only when `snapshot` actually changes.  Called
  // UNCONDITIONALLY (even when `store` is null) — see useAdapterFacade
  // for the null-handling — so the hook count stays stable across
  // renders where the session key toggles null.  An earlier version
  // did `if (!store) return NOOP_ADAPTER;` here, which caused React's
  // "change in the order of Hooks" error (position 78 flipped from
  // useState to useMemo the first time a session was bound).
  return useAdapterFacade(store, snapshot);
}

const NOOP_ADAPTER: ChatListAdapterV2 = {
  blocks: EMPTY_BLOCKS,
  totalBlocks: 0,
  messageOffset: 0,
  messageLimit: 0,
  messageTotal: 0,
  cacheGeneration: 0,
  isAtTail: () => false,
  hasPendingFlush: () => false,
  hasOlder: false,
  hasNewer: false,
  isLoading: false,
  loadPrevPage: async () => {},
  loadNextPage: async () => {},
  loadInitialPage: async () => {},
  loadPageForMessage: async () => {},
  loadPageForBlockId: async () => {},
  scrollToTop: async () => {},
  scrollToBottom: async () => {},
  scrollToPosition: async () => {},
  subscribe: () => () => {},
};

/**
 * Build the public ChatListAdapter facade bound to a per-session store.
 * The returned object identity is stable across renders as long as
 * `snapshot` is unchanged — every consumer re-render that does NOT
 * change data (e.g. parent re-renders, focus events, sleep-resume
 * resume) will get back the SAME facade reference, so downstream
 * `useEffect(..., [adapter])` and `useCallback(..., [adapter])` hooks
 * do not re-fire spuriously.  When the adapter snapshot genuinely
 * changes (chatStore update / liveBuffer mutation), `useSyncExternalStore`
 * has already replaced `snapshot` with a fresh reference, so the memo
 * rebuilds and consumers re-render with the new data.
 *
 * Without this memo, the old `return { ... }` allocation produced a
 * fresh facade on every render — a known footgun that fed an
 * "effect → setState → effect" loop in the contradiction-resolver
 * effect of useScrollController (see the comment on that effect).
 *
 * MUST be called unconditionally by the consumer (useChatListAdapter):
 * `useMemo` runs every render, and skipping it on the null branch
 * would flip the hook count and trip Rules of Hooks the moment a
 * session key becomes bound.  When `store` is null the factory
 * returns the frozen NOOP_ADAPTER instead.
 */
function useAdapterFacade(
  store: AdapterStore | null,
  snapshot: AdapterSnapshot,
): ChatListAdapterV2 {
  return useMemo<ChatListAdapterV2>(() => {
    // store may be null when no session is bound; in that case return
    // the frozen noop so consumers can still call no-op methods safely.
    // The check lives INSIDE the factory so the useMemo call itself is
    // unconditional from the consumer's perspective (Rules of Hooks).
    if (!store) return NOOP_ADAPTER;
    return {
      blocks: snapshot.blocks,
      totalBlocks: snapshot.blocks.length,
      messageOffset: snapshot.messageOffset,
      messageLimit: snapshot.messageLimit,
      messageTotal: snapshot.messageTotal,
      cacheGeneration: store.cacheGeneration,
      isAtTail: store.isAtTail,
      hasPendingFlush: store.hasPendingFlush,
      hasOlder: snapshot.hasOlder,
      hasNewer: snapshot.hasNewer,
      isLoading: snapshot.isLoading,
      loadPrevPage: store.loadPrevPage,
      loadNextPage: store.loadNextPage,
      loadInitialPage: store.loadInitialPage,
      loadPageForMessage: store.loadPageForMessage,
      loadPageForBlockId: store.loadPageForBlockId,
      scrollToTop: store.scrollToTop,
      scrollToBottom: store.scrollToBottom,
      scrollToPosition: store.scrollToPosition,
      subscribe: store.subscribe,
    };
  }, [store, snapshot]);
}

// Re-export the in-store event API for the controller (C4) to hook into
// without going through the React hook.
export function __adapterStoreFor(
  agentId: string,
  sessionId: string,
): AdapterStore | null {
  const key = sessionKey(agentId, sessionId);
  return stores.get(key) ?? null;
}

/** Drop a per-session adapter store and tear it down.  Used on session
 *  close / eviction so the singleton map doesn't leak. */
export function __releaseAdapterStore(agentId: string, sessionId: string): void {
  const key = sessionKey(agentId, sessionId);
  const store = stores.get(key);
  if (!store) return;
  store.teardown();
  stores.delete(key);
  releaseAdapterSession(agentId, sessionId);
}

/** Test-only helper: create a fresh per-session store WITHOUT going
 *  through React.  Used by `chatListAdapter.test.ts` to drive the
 *  snapshot builder directly.  Returns the new store. */
export function __createAdapterStoreForTest(
  agentId: string,
  sessionId: string,
): AdapterStore {
  const key = sessionKey(agentId, sessionId);
  // Tear down any previous store at this key.
  const existing = stores.get(key);
  if (existing) {
    existing.teardown();
    stores.delete(key);
  }
  const store = new AdapterStore(agentId, sessionId);
  stores.set(key, store);
  return store;
}
