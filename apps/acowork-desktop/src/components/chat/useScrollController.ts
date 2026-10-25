/**
 * useScrollController - Event-driven pagination + scroll-position
 * controller for the chat list.
 *
 * ADR-050 C5 — replaces the v1 782-line state machine with a ~150-line
 * event-driven pipeline.  The controller is the ONLY layer that reads
 * DOM (scrollTop / scrollHeight) and writes scroll-side effects.  It
 * does NOT carry any per-render scroll state — every value is derived
 * from either DOM (read on demand) or the adapter (data source).
 *
 * Adapter compatibility
 * ---------------------
 * The controller accepts ONLY the v2 `ChatListAdapterV2` (from
 * `chatListAdapter.ts`).  The v1 `useChatListAdapter.ts` is no longer
 * referenced.
 *
 * Responsibilities
 * ----------------
 * 1. **Pagination trigger** (event-driven, rAF-throttled):
 *      - On scroll events (user-initiated), check proximity to edges.
 *      - If user is near the TOP of the scroll container, call
 *        `adapter.loadPrevPage()`.
 *      - If user is near the BOTTOM, call `adapter.loadNextPage()`.
 *      - On `liveUpdate` adapter events (content growth), re-check
 *        edges to handle passive proximity (content shrank/grew).
 *      - Zero CPU when idle — no polling timer.
 *
 * 2. **Scroll arrow visibility** (data-driven, no DOM read):
 *      - `showScrollToBottom` = `!isNearBottom || !adapter.isAtTail()`.
 *      - `showScrollToTop`    = `!isNearTop || adapter.hasOlder`.
 *
 * 3. **Jump buttons** (delegate to the adapter):
 *      - `jumpToBottom` = `adapter.scrollToBottom()`
 *      - `jumpToTop`    = `adapter.scrollToTop()`
 *
 * 4. **Event subscription** (C5 wired):
 *      The controller subscribes to `adapter.subscribe(...)` and on
 *      `liveUpdate` events checks whether the streaming block is
 *      in the viewport (via `vmlRef.isStreamingBlockInViewport()`).
 *      When in viewport, the browser naturally keeps the user at the
 *      bottom as scrollHeight grows.  When out of viewport, the
 *      user's reading position is preserved (no forced scroll).
 *
 * Why no state
 * ------------
 * Pre-ADR-050, the controller carried a 5-state machine
 * (pinned-bottom / idle / loading-older / loading-newer / jumping)
 * with `prevScrollHeightRef` deltas and `wasAtBottomRef` ground-truth
 * tracking.  This state was duplicated by `chatStore.isPinnedToBottom`
 * and competed with `useChatListAdapter.isAtTail()` for the
 * "is the user at the bottom?" question.  ADR-050 collapses all three
 * into a single adapter-derived signal (`isAtTail` /
 * `hasPendingFlush`); the controller is reduced to a DOM-reading
 * + adapter-calling shim.
 *
 * "Auto-follow when streaming" behavior:
 * Pre-ADR-050, the controller's sticky-bottom logic forced the scroll
 * position to the bottom whenever a new message arrived while the
 * user was pinned.  ADR-050 C4 DROPS this behavior (per user feedback:
 * "用户滚到哪里显示哪里").  When the user is at the bottom, the
 * BROWSER naturally keeps them there as `scrollHeight` grows.  When
 * the user scrolls away, the position is preserved — they see new
 * content accumulate BELOW the current viewport.  Clicking the
 * "jump to latest" button re-anchors at any time.
 */
import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react";
import { log } from "../../lib/logger";
import type { ChatListAdapterV2 } from "./chatListAdapter";
import type { VirtualMessageListHandle } from "./VirtualMessageList";

// ── Constants ──────────────────────────────────────────────────────────────

/**
 * Distance from the top/bottom edge of the scroll container (in px)
 * within which we trigger pagination.  Small enough that prepending /
 * appending is seamless (the user doesn't see the threshold line) and
 * large enough that a single scroll event doesn't oscillate between
 * triggering and idle.
 */
const EDGE_THRESHOLD_PX = 50;

/**
 * Minimum usable scroll range (as a fraction of the viewport) the loaded
 * window must provide.  A pane whose content is only marginally taller than
 * the viewport has a thumb that covers ~the whole track and dies after one
 * wheel notch — indistinguishable from "no scrollbar, cannot scroll".  Keep
 * the window at least one full viewport taller than the pane, then stop.
 */
const FILL_MIN_RANGE_VIEWPORTS = 1;

/**
 * Deadline for the init-scroll positioning retry loop (see the init-scroll
 * effect).  The virtualizer attaches its scroll element during the child's
 * layout phase, so one or two frames is normally enough; the cap exists only
 * so a permanently unusable container cannot spin rAF forever.
 */
const INIT_SCROLL_MAX_MS = 500;

// ── Public types ───────────────────────────────────────────────────────────

export interface ScrollController {
  /** Show the floating "jump to latest" arrow. */
  showScrollToBottom: boolean;
  /** Show the floating "jump to oldest" arrow. */
  showScrollToTop: boolean;
  /** Subscribe to the scroll container's onScroll event. */
  handleScroll: () => void;
  /** User clicked "jump to latest". */
  jumpToBottom: () => void;
  /** User clicked "jump to oldest". */
  jumpToTop: () => void;
  /** Whether the user is viewing the latest content (at tail, no pending). */
  isAtLatest: () => boolean;
  /**
   * Whether the viewport currently shows a position that is WORTH RECORDING
   * for the current session — either the init-scroll landed where we asked,
   * or the user has since interacted with the viewport themselves.
   *
   * ChatPanel's scroll-snapshot writer MUST refuse to record a reading
   * position while this is false.  An unpositioned virtualizer sits at its
   * `initialOffset: 0` (the TOP); persisting that as "where the user was
   * reading" is what made a session latch to the top: every later nav-back
   * faithfully restored block 0, and `onScroll` then re-confirmed the bogus
   * position.  Restoring to the tail is the safe default, so refusing to
   * record costs nothing and breaks the lock.
   */
  isPositioned: () => boolean;
  /**
   * Declare that the user has directly interacted with the message viewport
   * (wheel over the container, pointer-down inside it).  Two effects:
   *
   *   1. Marks the viewport as worth recording.  Without this, a session
   *      whose init-scroll gave up (see `isPositioned`) could never persist
   *      a position even after the user scrolled somewhere and read it for
   *      minutes — the snapshot writer would keep refusing.
   *   2. Stops the init-scroll retry loop, so a still-pending positioning
   *      attempt cannot yank the viewport back to the tail underneath a user
   *      who has already taken manual control.
   *
   * Deliberately NOT driven from `handleScroll`: when a session switches,
   * the old content is removed, scrollHeight shrinks, and the browser fires
   * a synthetic scroll event to clamp `scrollTop`.  Treating that as "the
   * user saw this viewport" is exactly the bogus reading position this gate
   * exists to reject.  Only genuine input events qualify.
   */
  noteViewportInteraction: () => void;
}

export interface ScrollControllerConfig {
  /** Ref to the scroll container (owned by ChatPanel). */
  containerRef: React.RefObject<HTMLDivElement | null>;
  /** The v2 chat list adapter (single source of truth). */
  adapter: ChatListAdapterV2;
  /** Imperative handle to VirtualMessageList (scrollToBottom / scrollToTop / isStreamingBlockInViewport). */
  vmlRef: React.RefObject<VirtualMessageListHandle | null>;
  /** Session key — used to reset the tick on session change. */
  sessionKey: string | null;
  /**
   * Content-derived blockId of the first visible block when the user
   * left the session.  Data-driven replacement for pixel scroll offset.
   */
  initialFirstVisibleBlockId?: string | null;
  /**
   * Whether the user was at the bottom (near the latest content) when
   * they left the session.  When true, init-scroll scrolls to bottom;
   * otherwise it scrolls to initialFirstVisibleBlockId.
   */
  initialAtBottom?: boolean;
}

// ── Hook ───────────────────────────────────────────────────────────────────

export function useScrollController(config: ScrollControllerConfig): ScrollController {
  const { containerRef, adapter, vmlRef, sessionKey, initialFirstVisibleBlockId, initialAtBottom } = config;

  // Mirrors of the latest adapter / vmlRef / container refs so the
  // interval callback (registered with stable deps) reads current
  // values without re-subscribing on every render.  This is the ONLY
  // "state" the controller carries; it is not used to drive UI
  // re-renders.
  const adapterRef = useRef(adapter);
  adapterRef.current = adapter;
  const vmlRefRef = useRef(vmlRef);
  vmlRefRef.current = vmlRef;
  const containerRefRef = useRef(containerRef);
  containerRefRef.current = containerRef;

  // ── Scroll-position tracking ──
  // Two boolean states track whether the viewport is near each edge.
  // They flip when the user crosses the EDGE_THRESHOLD_PX boundary,
  // driving a minimal re-render to update arrow-button visibility.
  // Without these, button visibility is purely data-derived and misses
  // the case where the user scrolls within a page that's already at
  // the data-window tail/head.
  const [isNearBottom, setIsNearBottom] = useState(true);
  const [isNearTop, setIsNearTop] = useState(false);

  // ── Auto-follow mode ──
  // User intent: "I want to stay anchored at the latest content".
  // When true, the controller hides the "jump to latest" button and
  // forces scrollToBottom() on every liveUpdate, so streaming content
  // continuously keeps the user at the tail.  When the user actively
  // scrolls away from the bottom (> EDGE_THRESHOLD_PX), this flips to
  // false and the button reappears.
  //
  // Why useState (not useRef): drives showScrollToBottom, which gates
  // a DOM element. Re-render cost is negligible (only flips on user
  // intent, not on every streaming delta).
  const [followMode, setFollowMode] = useState(true);
  // Mirror ref so the subscribe callback (registered once, see
  // useEffect below) can read the latest followMode without re-subscribing
  // on every flip. Same pattern as adapterRef / vmlRefRef above.
  const followModeRef = useRef(followMode);
  followModeRef.current = followMode;
  // Helper: setFollowMode updates BOTH state and ref synchronously.
  // The state update drives the re-render (and thus showScrollToBottom),
  // while the ref update lets the subscribe callback — which was
  // registered ONCE on first render and reads followModeRef.current —
  // see the new value immediately, before the re-render commits.
  // Without the ref update, a liveUpdate fired in the same tick as
  // jumpToTop would still see the stale followMode and wrongly call
  // vml.scrollToBottom() (yanking the user back to the tail).
  const setFollowModeSync = useCallback((next: boolean) => {
    followModeRef.current = next;
    setFollowMode(next);
  }, []);

  // `showScrollToBottom`: show when the user is NOT viewing the latest
  // content.  Two triggers:
  //   1. User scrolled away from bottom (!isNearBottom)
  //   2. Data window doesn't cover the tail (!isAtTail)
  // Gate: NEVER show while followMode is active.  followMode is the
  // user's explicit intent to stay anchored at the tail; while it is
  // on, any divergence from the latest content is resolved by
  // auto-scrolling (see the contradiction-resolver effect below), so
  // the button would only ever appear as a stale flash.
  // Note: `adapter.hasPendingFlush()` is intentionally NOT used here.
  // A pending flush only means there are live stream entries (e.g. a
  // thinking block) that are already folded into `adapter.blocks` at the
  // tail.  Those entries are already rendered, so they do not imply the
  // user is behind the latest content.
  // Guard: hide both arrows when there are no blocks (empty session).
  // Without this, `isNearTop` defaults to `false` (set on session switch)
  // which makes `!isNearTop = true` -> `showScrollToTop = true` even though
  // there is no content to scroll.  `checkEdges()` (which updates
  // `isNearTop`) only runs after `initializedRef` is set, and
  // `initializedRef` is only set when `blocksLen > 0` - so for an empty
  // session the initial state never gets corrected.
  const hasBlocks = adapter.blocks.length > 0;
  // Raw need signal (no followMode gate): true when the data-derived
  // flags say the user is not viewing the latest content.  This is the
  // single truth source for "user needs to go to the bottom" — it drives
  // the contradiction resolver below and the arrow button.
  const needsLatestContent = hasBlocks
    && (!isNearBottom || !adapter.isAtTail());
  // Final UI flag: NEVER show the arrow while followMode is active.
  // followMode is the user's explicit intent to stay anchored at the
  // tail; while it is on, any divergence from the latest content is
  // resolved by auto-scrolling (the contradiction resolver), so the
  // button would only ever appear as a stale flash.
  const showScrollToBottom = needsLatestContent && !followMode;
  const showScrollToTop = hasBlocks
    && (!isNearTop || adapter.hasOlder);

  // ── Initial positioning gate ──
  // Prevents edge-detection from triggering loadPrevPage before the
  // scroll container has been positioned (scroll-to-bottom on fresh
  // open, or restored via initialOffset).  Without this gate, the
  // first scroll event reads scrollTop=0 (the virtualizer's
  // initialOffset) and mistakes it for "user scrolled to top" —
  // cascading loadPrevPage until all messages are loaded.
  const initializedRef = useRef(false);
  /**
   * Cursor signature (`offset:limit`) of the window for which the fill rule
   * in `checkEdges` last issued a load.  Declared here because the
   * session-switch reset below consumes it, and `const` does not hoist.
   */
  const fillWindowRef = useRef("");
  /**
   * True once the viewport holds a position worth recording for this
   * session: either the init-scroll landed, or the user directly interacted
   * with the viewport (see `noteViewportInteraction`).  Kept apart from
   * `initializedRef` (the pagination gate) because that gate must open even
   * when positioning gives up — otherwise pagination dies — while the
   * snapshot writer must keep refusing in that case.
   */
  const positionedRef = useRef(false);
  /**
   * The live init-scroll retry loop, if any.  `null` when nothing is pending.
   * Held in a ref (not an effect local) so it outlives a single render pass:
   * the init-scroll effect re-runs on every data change, and the loop must
   * survive that churn until it either lands or hits its deadline.
   */
  const initRetryRef = useRef<{ raf: number; cancelled: boolean } | null>(null);

  /**
   * Cancels a pending init-scroll retry loop.  Owned by the SESSION rather
   * than by one render pass — see `initRetryRef` above for why the loop has
   * to survive the init-scroll effect's dep-driven re-runs.
   */
  const stopInitRetry = useCallback(() => {
    const loop = initRetryRef.current;
    if (!loop) return;
    loop.cancelled = true;
    if (loop.raf) cancelAnimationFrame(loop.raf);
    initRetryRef.current = null;
  }, []);

  // Reset on session switch.  Also reset prepend-tracking refs so the
  // scroll-preservation effect doesn't misfire across sessions.
  // MUST be useLayoutEffect (not useEffect) so it fires BEFORE the
  // init-scroll useLayoutEffect below — both are useLayoutEffect and
  // React fires them in definition order.  If this were useEffect it
  // would fire AFTER the init-scroll, leaving initializedRef=true
  // when the init-scroll checks it (skipping the scroll restoration).
  //
  // ORDERING CONTRACT with ChatPanel's scroll-snapshot writer: that writer
  // reads `isPositioned()` from a useLayoutEffect CLEANUP, and React runs
  // every layout-effect cleanup of a fiber before any of its bodies.  So the
  // outgoing session is still seen as positioned here — this reset lands only
  // afterwards.  Do NOT move the `positionedRef` write into a cleanup
  // function: it would then run before the snapshot writer's cleanup and
  // silently drop the snapshot of every session switch.
  useLayoutEffect(() => {
    initializedRef.current = false;
    positionedRef.current = false;
    // The fill rule's cursor signature is per-session: a session that left
    // the window at some cursor must not hand that signature to the next
    // session, or the first fill load for an identical cursor is skipped.
    fillWindowRef.current = "";
    setIsNearBottom(true); // fresh session starts at bottom
    setIsNearTop(false);   // ...not at top
    setFollowModeSync(true); // new session → auto-follow by default
    // handleScroll's direction test ("programmatic scrolls only move down,
    // so scrollTop < prev means the user scrolled up") is only valid
    // WITHIN one session.  Carried across a switch, the baseline is the
    // outgoing session's absolute scrollTop: restoring the new session to
    // its (smaller) bottom reads as a user scroll-up and cancels
    // followMode — sticky, since neither liveUpdate following nor the
    // contradiction resolver runs without it, leaving the session parked
    // mid-page.  Reset the baseline so the first post-switch event is
    // judged against the new session's own position.
    prevScrollTopRef.current = 0;
  }, [sessionKey, setFollowModeSync]);

  // Tear the positioning loop down when the session changes or the component
  // unmounts.  Two constraints on this effect:
  //   - keyed on `sessionKey` ALONE: the init-scroll effect cannot own the
  //     teardown, because it re-runs on every data change (blocksLen) and
  //     would kill a loop that is still trying to land.
  //   - useLayoutEffect, declared BEFORE the init-scroll effect: React runs
  //     all layout cleanups of a fiber before any layout body, so the old
  //     session's loop is stopped before the new session's loop starts.  As a
  //     passive effect its cleanup would run in a LATER phase — after the new
  //     loop had already been created — and would cancel the wrong loop.
  useLayoutEffect(() => () => stopInitRetry(), [sessionKey, stopInitRetry]);

  // ── Init-scroll effect (useLayoutEffect — before paint) ──
  // Data-driven restoration: the saved snapshot carries a stable
  // content-derived blockId and an atBottom flag.  We never restore
  // raw pixel offsets because VML remounts with an empty per-instance
  // measurement cache; pixel offsets land in the middle of the list.
  // useLayoutEffect (not useEffect) so the scroll is applied BEFORE
  // the browser paints — no top→position flash.
  // Two separate one-shot flags, because two different things depend on them:
  //   - initializedRef = "the init scroll has been ATTEMPTED".  Gates edge
  //     detection (loadPrevPage) and must open immediately, exactly as before —
  //     waiting on positioning would stall pagination for the retry window.
  //   - positionedRef  = "the viewport holds a position worth recording".
  //     Gates the snapshot writer in ChatPanel; set by a scroll that LANDS or
  //     by direct user interaction with the viewport.
  const blocksLen = adapter.blocks.length;
  useLayoutEffect(() => {
    if (initializedRef.current) return;
    if (blocksLen === 0) return;

    initializedRef.current = true;

    // Bounded rAF retry.  `vml.scrollToBottom()` reports failure when the
    // virtualizer has not attached its scroll element yet (fresh mount after a
    // top-level nav) or when there is nothing to scroll.  An IDLE session
    // produces no further commit, so relying on this effect re-running on a
    // data change is not enough to recover — poll for a few frames instead.
    //
    // The handle itself may still be null on this pass (VML not mounted),
    // which is exactly why the loop re-reads `vmlRefRef.current.current`
    // EVERY frame instead of capturing it here.  An early return on a null
    // handle would be unrecoverable: `initializedRef` is already consumed,
    // so this effect never re-runs for this session, and `positionedRef`
    // would stay false forever — silently disabling the snapshot writer.
    //
    // The loop lives in `initRetryRef`, not in this closure: this effect
    // re-runs whenever `blocksLen` changes (streaming, page loads), and a
    // dep-driven re-run must not cancel a loop that is still trying.
    const loop = { raf: 0, cancelled: false };
    initRetryRef.current = loop;
    const deadline = performance.now() + INIT_SCROLL_MAX_MS;

    const attempt = () => {
      // Bail when the loop was stopped (session change / unmount) or replaced
      // by a newer session's loop.  NOTE: this deliberately does NOT check
      // `positionedRef` — a user interaction can set that flag BEFORE any
      // data arrived (a click or wheel over an empty viewport), and the
      // first attempt must still position the session.  Landing clears
      // `initRetryRef` itself, so the identity check above covers it.
      if (loop.cancelled || initRetryRef.current !== loop) return;
      const cur = vmlRefRef.current.current;
      let landed = false;
      if (cur) {
        if (initialAtBottom) {
          // User left at the tail — show the latest content.
          landed = cur.scrollToBottom();
        } else if (initialFirstVisibleBlockId) {
          // User was browsing — restore the first visible block by stable id.
          // If the block is not in the loaded page (e.g. compaction removed
          // it), fall back to bottom so the user isn't stranded mid-list.
          landed =
            cur.scrollToBlockId(initialFirstVisibleBlockId) ||
            cur.scrollToBottom();
        } else {
          // Fresh open / no useful snapshot — scroll to bottom.
          landed = cur.scrollToBottom();
        }
      }
      if (landed) {
        positionedRef.current = true;
        initRetryRef.current = null;
        return;
      }
      if (performance.now() >= deadline) {
        // Give up.  Pagination stays open (initializedRef was opened on the
        // first attempt); only the snapshot writer cares about positionedRef,
        // and it stays FALSE here, so a viewport the user never saw is never
        // recorded as their reading position.  This visit may show the top;
        // the next one still gets a correct target instead of latching to it.
        // The user can still rescue the position by interacting with the
        // viewport — that routes through noteViewportInteraction.
        log.warn("[useScrollController] init-scroll could not position", {
          sessionKey,
          initialAtBottom,
          initialFirstVisibleBlockId,
        });
        initRetryRef.current = null;
        return;
      }
      loop.raf = requestAnimationFrame(attempt);
    };

    attempt();
  }, [blocksLen, sessionKey, initialAtBottom, initialFirstVisibleBlockId, adapter]);

  // ── Scroll preservation on prepend (useLayoutEffect) ──
  // When loadPrevPage() prepends older messages, the virtualizer's
  // totalSize grows but the browser keeps scrollTop unchanged.  Without
  // correction the user's viewport shows the SAME pixels — the newly
  // loaded content is above the fold and invisible.  The user perceives
  // "scrolling doesn't work" because repeated scroll-up → loadPrevPage
  // cycles produce no visible change.
  //
  // Fix: in a useLayoutEffect (runs after DOM commit, before paint),
  // detect prepend (first blockId changed + block count grew) and
  // shift scrollTop by the scrollHeight delta.  This keeps the user's
  // viewport anchored to the same content while new content appears
  // above — the standard chat-app "infinite scroll up" behavior.
  //
  // Detection uses blockId (content-derived, stable) rather than
  // messageOffset to avoid false positives when a session reload
  // resets offset to 0.
  const firstBlockId = blocksLen > 0 ? adapter.blocks[0].blockId : null;
  const cacheGeneration = adapter.cacheGeneration;
  const prevBlocksLenRef = useRef(0);
  const prevFirstBlockIdRef = useRef<string | null>(null);
  const prevScrollHeightRef = useRef(0);
  const prevSessionKeyForPrependRef = useRef<string | null | undefined>(undefined);
  const prevCacheGenRef = useRef(cacheGeneration);

  // Capture scrollHeight BEFORE the DOM commit (during render) so the
  // useLayoutEffect can compute the delta after the commit.
  const containerNow = containerRef.current;
  if (containerNow) {
    prevScrollHeightRef.current = containerNow.scrollHeight;
  }

  useLayoutEffect(() => {
    const container = containerRefRef.current.current;

    // Session changed — reset tracking refs, skip adjustment.
    if (prevSessionKeyForPrependRef.current !== sessionKey) {
      prevSessionKeyForPrependRef.current = sessionKey;
      prevBlocksLenRef.current = blocksLen;
      prevFirstBlockIdRef.current = firstBlockId;
      prevCacheGenRef.current = cacheGeneration;
      if (container) prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    // Cache replaced (jump operation) — NOT a prepend.  Reset refs
    // and skip anchoring; the caller (scrollToTop/Bottom/Position)
    // owns the subsequent scroll placement.
    if (prevCacheGenRef.current !== cacheGeneration) {
      prevCacheGenRef.current = cacheGeneration;
      prevBlocksLenRef.current = blocksLen;
      prevFirstBlockIdRef.current = firstBlockId;
      if (container) prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    const prevLen = prevBlocksLenRef.current;
    const prevId = prevFirstBlockIdRef.current;
    prevBlocksLenRef.current = blocksLen;
    prevFirstBlockIdRef.current = firstBlockId;

    if (!container) return;
    // Guard: only adjust on genuine prepend within the same session.
    // - prevId !== null: not the first data arrival (initial load).
    // - blocksLen > prevLen: content grew (not a clear / replace).
    // - firstBlockId changed: the head of the list is different
    //   (prepend), not just an append or in-place update.
    if (prevId === null || blocksLen <= prevLen || firstBlockId === prevId) {
      prevScrollHeightRef.current = container.scrollHeight;
      return;
    }

    // Prepend detected — shift scrollTop by the height delta so the
    // user's viewport stays anchored to the same content.
    const delta = container.scrollHeight - prevScrollHeightRef.current;
    if (delta > 0) {
      container.scrollTop += delta;
    }
    prevScrollHeightRef.current = container.scrollHeight;
  }, [blocksLen, firstBlockId, sessionKey, cacheGeneration]);

  // ── Edge-detection (shared by scroll handler + adapter events) ──
  // Reads DOM scrollTop/scrollHeight and triggers pagination when the
  // user is within EDGE_THRESHOLD_PX of either edge.  Called from:
  //   1. handleScroll (user-initiated scroll, rAF-throttled)
  //   2. adapter liveUpdate event (content growth may passively push
  //      the viewport near an edge)
  // Gated by initializedRef and adapter.isLoading.
  // Also updates `isNearBottom` state for arrow-button visibility.
  const checkEdges = useCallback(() => {
    if (!initializedRef.current) return;
    const container = containerRefRef.current.current;
    const a = adapterRef.current;
    if (!container) return;

    // ── Update scroll-position state (drives arrow buttons) ──
    const scrollTop = container.scrollTop;
    const distFromBottom =
      container.scrollHeight - (scrollTop + container.clientHeight);
    const nearBottom = distFromBottom <= EDGE_THRESHOLD_PX;
    const nearTop = scrollTop <= EDGE_THRESHOLD_PX;
    setIsNearBottom((prev) => (prev === nearBottom ? prev : nearBottom));
    setIsNearTop((prev) => (prev === nearTop ? prev : nearTop));

    // ── Pagination trigger ──
    if (!a) return;
    if (a.isLoading) return;

    // ── Viewport fill (restores ADR-050 C5 `ensureRenderable`) ──
    // Runs BEFORE the edge branches: when the window is too short there is
    // no scroll range to speak of, so "near an edge" is meaningless.  Keep
    // pulling pages until the pane overflows or the data runs out.
    //
    // The loop is bounded by DATA, not by a page budget.  A page budget
    // cannot work here: pagination is measured in raw JSONL entries while
    // the UI folds a whole thought/tool_call/tool_result run into ONE
    // block, and ExploreBlock caps that block's content height — so
    // crossing a long run adds hundreds of entries and ZERO pixels.  The
    // ADR-050 C5 budget (10 pages = 500 entries) expired in the middle of
    // the 617-entry run found in real session logs, and because exhausting
    // it `return`ed out of checkEdges AHEAD of the edge branches, it
    // switched off pagination for the whole pane — in a state with no
    // scroll range there is no user scroll event that could ever re-arm
    // it.  The user was left with an under-filled pane and unreachable
    // history.
    //
    // This also subsumes turn-boundary alignment for free: while the
    // leading block is a truncated explore run, more of that run adds no
    // height, so the pane cannot fill and the loop keeps going until the
    // window starts on a real turn boundary (a new block appears, which is
    // the only thing that adds height at the head).
    const range = container.scrollHeight - container.clientHeight;
    if (range < container.clientHeight * FILL_MIN_RANGE_VIEWPORTS) {
      // Under-filled: the fill rule owns this state ENTIRELY.  Do not fall
      // through to the edge branches — with no scroll range there is no
      // "near an edge" to speak of (scrollTop is pinned at 0, so the
      // near-top branch would fire on every single check and re-introduce
      // exactly the unbounded auto-load this guard exists to bound).
      const direction = a.hasNewer ? "next" : a.hasOlder ? "prev" : null;
      // One load per distinct window state: a load that MOVED the cursor
      // re-arms the rule, a load that changed nothing (no-op, duplicate
      // response, failure) does not — so the loop stops on a stuck adapter
      // instead of spinning, and a long explore run is never cut off
      // mid-way for being "too many pages".
      const cursor = `${a.messageOffset}:${a.messageLimit}`;
      if (direction && cursor !== fillWindowRef.current) {
        fillWindowRef.current = cursor;
        void (direction === "next" ? a.loadNextPage() : a.loadPrevPage());
      }
      return;
    }
    fillWindowRef.current = "";

    // near-top → load older
    if (scrollTop <= EDGE_THRESHOLD_PX && a.hasOlder) {
      void a.loadPrevPage();
      return;
    }
    // near-bottom → load newer
    if (distFromBottom <= EDGE_THRESHOLD_PX && a.hasNewer) {
      void a.loadNextPage();
    }
  }, []);

  // rAF gate — coalesces multiple scroll events within a single frame
  // into one checkEdges call.  Prevents layout thrashing on high-freq
  // scroll events (trackpad / 120Hz displays can fire 120+ events/s).
  const rafPendingRef = useRef(false);
  const scheduleEdgeCheck = useCallback(() => {
    if (rafPendingRef.current) return;
    rafPendingRef.current = true;
    requestAnimationFrame(() => {
      rafPendingRef.current = false;
      checkEdges();
    });
  }, [checkEdges]);

  // ── Viewport fill trigger ──
  // checkEdges owns the fill rule (see its fill branch); this effect is what
  // re-runs it deterministically after an explicit jump (scrollToTop /
  // scrollToBottom / loadPageForBlockId) or a short initial page replaced
  // the loaded window.  Without it the fill would only be driven by
  // scroll / pageLoaded events, and a window too short to overflow the pane
  // produces neither — the user is left with no scroll range at all.
  useLayoutEffect(() => {
    scheduleEdgeCheck();
  }, [blocksLen, cacheGeneration, sessionKey, scheduleEdgeCheck]);

  // ── Event subscription ──
  // The controller subscribes to adapter events:
  //   - `liveUpdate`: content grew (stream delta / record complete).
  //     Re-check edges — content growth may passively push the viewport
  //     within EDGE_THRESHOLD_PX of an edge (e.g. total shrank after
  //     compaction, or user was already near bottom and new content
  //     arrived).  Also runs the streaming-block viewport diagnostic.
  //   - `pageLoaded`: a page was loaded (prepend/append).  Re-check
  //     edges in case the new content didn't push the viewport far
  //     enough from the threshold (rapid consecutive scrolls).
  //   - `scrollToIndex`: adapter.scrollToPosition() was called.  The
  //     controller executes the DOM scroll via vmlRef.scrollToIndex().
  useEffect(() => {
    const unsub = adapter.subscribe((event) => {
      switch (event.type) {
        case "liveUpdate": {
          const vml = vmlRefRef.current.current;
          if (vml) {
            void vml.isStreamingBlockInViewport();
            // Auto-follow: when followMode is active, ALWAYS force the
            // scroll to the tail on content growth.  Deliberately NO DOM
            // distance check here — in a virtualized list the scrollHeight
            // delta of an incoming stream block is not reliable within the
            // same tick (measurement hasn't run), so a distance guard
            // mis-detects "scrolled away" while the user is still visually
            // at the bottom.  That mis-detection was the bug where the
            // arrow reappeared during streaming.  Cancelling followMode is
            // owned EXCLUSIVELY by handleScroll (user scroll direction +
            // DOM distance) — see the comment there.
            if (followModeRef.current) {
              vml.scrollToBottom();
            }
          }
          // Content grew — passive edge proximity may have changed.
          scheduleEdgeCheck();
          break;
        }
        case "pageLoaded":
          // Page loaded — re-check in case user is still at edge.
          scheduleEdgeCheck();
          break;
        case "scrollToIndex": {
          // adapter.scrollToPosition() requests a DOM scroll to a
          // specific block index.  The controller owns DOM side-effects.
          const vml = vmlRefRef.current.current;
          if (vml) {
            vml.scrollToIndex(event.index);
          }
          break;
        }
        default:
          break;
      }
    });
    return unsub;
  }, [adapter, sessionKey, scheduleEdgeCheck]);

  // ── handleScroll ──
  // Called from the scroll container's onScroll event (bound in
  // ChatPanel).  This is the PRIMARY pagination trigger — event-driven,
  // rAF-throttled.  Zero CPU when the user is not scrolling.
  // Also drives followMode transitions in BOTH directions:
  //   - followMode=true  + user scroll-up past the threshold → cancel.
  //   - followMode=false + user scroll-down into the bottom zone → re-arm.
  //
  // Why direction + DOM distance (not the isNearBottom state):
  //   The old check read `isNearBottom` from state, which is one render
  //   behind inside a scroll handler.  When jumpToBottom() programmatically
  //   scrolls to the tail, that scroll fires this handler while the state
  //   still holds the PRE-scroll value (false, because the user had scrolled
  //   up before clicking the arrow) — so followMode was cancelled by our own
  //   programmatic scroll, and every subsequent liveUpdate stopped
  //   auto-scrolling.  That was the "arrow reappears after clicking it
  //   during streaming" bug.
  //   Guard #1 (direction): programmatic scrolls (jumpToBottom / the
  //   liveUpdate auto-follow) only move scrollTop DOWN (increasing), so
  //   they can never cancel followMode.  Only a user-initiated scroll-up
  //   (decreasing scrollTop) can.
  //   Guard #2 (distance): read the DOM (always current in a scroll
  //   handler) instead of state.
  //   initializedRef gate: before init-scroll the container isn't
  //   positioned yet; a session-switch clamp scroll must not cancel the
  //   fresh session's followMode.
  //
  // Why a SYMMETRIC re-arm path (scroll-down into the bottom zone):
  //   Without this, a single scroll-up → scroll-down cycle can leave
  //   followMode stuck false: scroll-up clears it, then any subsequent
  //   scroll-down event is a no-op, so new messages stop auto-scrolling
  //   even though the user is visually at the tail.  Re-arming here
  //   matches the original requirement: "when the chat box is at the
  //   bottom, followMode is true" — both the first session-open state
  //   AND any state where the user has manually returned to the tail.
  const prevScrollTopRef = useRef(0);
  const handleScroll = useCallback(() => {
    // NOTE: do NOT set initializedRef here.  When a session switches,
    // the old content is removed → scrollHeight shrinks → the browser
    // fires an async scroll event (clamping scrollTop).  This event
    // arrives AFTER the reset effect (initializedRef=false) but BEFORE
    // the new session's data loads.  Setting initializedRef=true here
    // would block the init-scroll effect from restoring the position.
    scheduleEdgeCheck();

    const container = containerRefRef.current.current;
    if (!container || !initializedRef.current) return;

    const scrollTop = container.scrollTop;
    const distFromBottom =
      container.scrollHeight - (scrollTop + container.clientHeight);
    const nearBottom = distFromBottom <= EDGE_THRESHOLD_PX;
    const scrollingUp = scrollTop < prevScrollTopRef.current;
    prevScrollTopRef.current = scrollTop;

    if (followModeRef.current) {
      // Active follow → user scroll-up past the threshold cancels it.
      // Programmatic scrolls (jumpToBottom / liveUpdate auto-follow)
      // only move scrollTop DOWN (scrollingUp=false), so they cannot
      // trip this branch — see "Guard #1" above.
      if (scrollingUp && distFromBottom > EDGE_THRESHOLD_PX) {
        setFollowModeSync(false);
      }
    } else {
      // followMode off → user scroll-down into the bottom zone re-arms
      // it.  Without this, followMode can get stuck false after a
      // scroll-up → scroll-down cycle and new messages stop
      // auto-scrolling even though the user is visually at the tail.
      // Re-arming mirrors the initial session-open default: as soon as
      // the user is "at the bottom" again, followMode becomes true.
      if (!scrollingUp && nearBottom) {
        setFollowModeSync(true);
      }
    }
  }, [scheduleEdgeCheck, setFollowModeSync]);

  // ── Jump primitives ──
  // Two-phase: (1) adapter loads data pages until the window covers
  // the target edge; (2) controller commands VML to scroll the DOM.
  // The adapter only owns data — the controller owns DOM side-effects.
  const jumpToBottom = useCallback(() => {
    // User explicitly asked to anchor at the latest content → re-enable
    // auto-follow so subsequent streaming deltas keep them pinned.
    setFollowModeSync(true);
    void adapter.scrollToBottom().then(() => {
      const vml = vmlRefRef.current.current;
      if (vml) vml.scrollToBottom();
    });
  }, [adapter, setFollowModeSync]);

  const jumpToTop = useCallback(() => {
    // User explicitly asked to jump to the oldest content → exit
    // auto-follow; the button should reappear when new messages arrive
    // (which would happen at the top, not the bottom they came from).
    setFollowModeSync(false);
    void adapter.scrollToTop().then(() => {
      const vml = vmlRefRef.current.current;
      if (vml) vml.scrollToTop();
    });
  }, [adapter, setFollowModeSync]);

  // ── Contradiction resolver (auto-follow convergence) ──
  // followMode says "keep me anchored at the latest content", but the
  // raw data-derived signal can still report the user is NOT at the
  // latest content (needsLatestContent=true) — e.g. content grew and
  // checkEdges hasn't caught up, or the tail page is still loading.
  // When the two disagree, the button must NOT appear; instead execute
  // the jump-to-bottom action directly (same code path as the arrow
  // button) so the viewport converges to the tail.  This is the single
  // source of truth: followMode=true means auto-scroll, period — never
  // a visible arrow.
  //
  // Why ref (not dep) for jumpToBottom:
  //   The effect deps are the two booleans only.  jumpToBottom's identity
  //   changes on every ChatPanel re-render because the `adapter` arg is a
  //   fresh facade object each render (see chatListAdapter.ts:628
  //   useAdapterFacade).  Including it in the deps would re-fire the
  //   effect on EVERY render while `followMode && needsLatestContent`
  //   holds true — and each fire calls adapter.scrollToBottom(), which
  //   triggers a ChatPanel re-render via the chatStore subscription,
  //   re-creating `adapter` and the `jumpToBottom` closure, and the loop
  //   repeats: "Maximum update depth exceeded" on resume-from-sleep
  //   (when many spurious re-renders happen at once).  Mirror via ref so
  //   the latest jumpToBottom is invoked without re-firing the effect.
  const jumpToBottomRef = useRef(jumpToBottom);
  jumpToBottomRef.current = jumpToBottom;

  useEffect(() => {
    if (followMode && needsLatestContent) {
      jumpToBottomRef.current();
    }
  }, [followMode, needsLatestContent]);

  // ── isAtLatest ──
  // Preserved for ChatPanel's scroll-snapshot logic (nav-back restore).
  // Pure data derivation — no DOM read, no stored state.  ADR-050:
  // replaces the removed `isPinnedToBottom` stored field with a
  // computed query on the adapter.
  const isAtLatest = useCallback((): boolean => {
    return adapter.isAtTail() && !adapter.hasPendingFlush();
  }, [adapter]);

  const isPositioned = useCallback((): boolean => positionedRef.current, []);

  // See the ScrollController interface for why this is driven by INPUT
  // events rather than by `handleScroll`.
  const noteViewportInteraction = useCallback(() => {
    // The user has taken manual control — a still-pending positioning
    // attempt must not yank the viewport back to the tail.  This applies
    // even when the interaction happens at the very top.
    stopInitRetry();
    // Marking the viewport as recordable is a separate, stricter question:
    // only a viewport that is demonstrably NOT at the unpositioned origin
    // counts as something the user read.  Without this, a stray click, or a
    // wheel that cannot move anything (positioning failed, so the list sits
    // at `initialOffset: 0`), would record the bogus top position as the
    // reading position and re-introduce the very latch this gate exists to
    // prevent.
    //
    // The flag is sticky, so a user who scrolls down and then back to the
    // very top is still recorded correctly — the first non-zero position
    // already opened the gate.  A session short enough to never scroll is
    // already "at bottom" and its init-scroll reports success, so nothing is
    // lost here.
    const container = containerRefRef.current.current;
    // No container to inspect (SSR / test harness): there is no evidence of
    // the bogus state, so keep the permissive behaviour.  In a real browser
    // an interaction always has a live container.
    if (!container || container.scrollTop > 0) positionedRef.current = true;
  }, [stopInitRetry]);

  return {
    showScrollToBottom,
    showScrollToTop,
    handleScroll,
    jumpToBottom,
    jumpToTop,
    isAtLatest,
    isPositioned,
    noteViewportInteraction,
  };
}
