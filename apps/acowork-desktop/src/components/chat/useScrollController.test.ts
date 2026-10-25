/**
 * useScrollController — unit tests for the C5 event-driven controller.
 *
 * ADR-050 §7 C5 verification:
 *   - tsc --noEmit: clean (smoke-checked outside)
 *   - Manual tests in §8 acceptance matrix
 *   - Unit tests pin the data-derived flag contract (the only piece
 *     that doesn't need a DOM):
 *       showScrollToBottom = !isNearBottom || !adapter.isAtTail()
 *       showScrollToTop    = !isNearTop || adapter.hasOlder
 *       isAtLatest   = adapter.isAtTail() && !adapter.hasPendingFlush()
 *       jumpToBottom / jumpToTop  delegate to adapter
 *
 * The event-driven pagination trigger (checkEdges via rAF) reads DOM
 * and is covered indirectly: any provider that toggles `isLoading`
 * correctly would dedupe concurrent loads, so we don't need to drive
 * scroll events in unit tests.
 */
import { describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import { useScrollController } from "./useScrollController";
import type { ChatListAdapterV2 } from "./chatListAdapter";
import type { MessageBlock } from "./messageFolder";

const DUMMY_BLOCK: MessageBlock = {
  blockId: "block-1",
  type: "user",
  items: [],
  rawCount: 1,
  anchorToLatest: false,
  hasFollowUpReply: false,
  isLive: false,
};

function makeMockAdapter(overrides: Partial<ChatListAdapterV2> = {}): ChatListAdapterV2 {
  return {
    blocks: [DUMMY_BLOCK],
    totalBlocks: 1,
    messageOffset: 0,
    messageLimit: 0,
    messageTotal: 0,
    isAtTail: () => true,
    hasPendingFlush: () => false,
    hasOlder: false,
    hasNewer: false,
    isLoading: false,
    loadPrevPage: vi.fn(async () => {}),
    loadNextPage: vi.fn(async () => {}),
    scrollToTop: vi.fn(async () => {}),
    scrollToBottom: vi.fn(async () => {}),
    scrollToPosition: vi.fn(async () => {}),
    subscribe: vi.fn(() => () => {}),
    ...overrides,
  };
}

describe("useScrollController: data-derived flags", () => {
  it("showScrollToBottom=false when not at tail but followMode active (streaming case)", () => {
    // Default state: followMode=true (fresh session starts in follow
    // mode) and isNearBottom=true.  Even when isAtTail() is false (data
    // window hasn't caught up to the live stream), the button stays
    // hidden — the controller will auto-scroll on each liveUpdate.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("showScrollToBottom=true when not at tail and followMode disabled", () => {
    // Once followMode is off, the original isAtTail check applies.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // jumpToTop exits follow mode (act() flushes the state update so
    // result.current reflects the post-render value).
    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("showScrollToBottom=false when at tail and no pending flush", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("showScrollToBottom=false when at tail with pending flush but near bottom", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => true });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("showScrollToBottom=false and showScrollToTop=false when no blocks (empty session)", () => {
    const adapter = makeMockAdapter({ blocks: [], totalBlocks: 0 });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
    expect(result.current.showScrollToTop).toBe(false);
  });

  it("showScrollToTop: hasOlder=true always shows; hasOlder=false depends on isNearTop", () => {
    // hasOlder=true → always show (can load older pages)
    const adapter1 = makeMockAdapter({ hasOlder: true });
    const { result: r1 } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: adapter1,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(r1.current.showScrollToTop).toBe(true);

    // hasOlder=false + initial isNearTop=false (not at top) → show
    // (user is mid-list, can scroll up within current page)
    const adapter2 = makeMockAdapter({ hasOlder: false });
    const { result: r2 } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: adapter2,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // isNearTop defaults to false → !isNearTop=true → showScrollToTop=true
    expect(r2.current.showScrollToTop).toBe(true);
  });

  it("isAtLatest() returns true when at tail and no pending flush", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.isAtLatest()).toBe(true);
  });

  it("isAtLatest() returns false when not at tail", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.isAtLatest()).toBe(false);
  });

  it("isAtLatest() returns false when pending flush (streaming not yet caught up)", () => {
    const adapter = makeMockAdapter({ isAtTail: () => true, hasPendingFlush: () => true });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.isAtLatest()).toBe(false);
  });
});

describe("useScrollController: jump primitives delegate to adapter", () => {
  it("jumpToBottom calls adapter.scrollToBottom", () => {
    const scrollToBottom = vi.fn(async () => {});
    const adapter = makeMockAdapter({ scrollToBottom });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    act(() => {
      result.current.jumpToBottom();
    });
    expect(scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("jumpToTop calls adapter.scrollToTop", () => {
    const scrollToTop = vi.fn(async () => {});
    const adapter = makeMockAdapter({ scrollToTop });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    act(() => {
      result.current.jumpToTop();
    });
    expect(scrollToTop).toHaveBeenCalledTimes(1);
  });
});

describe("useScrollController: subscription", () => {
  it("subscribes to adapter on mount, unsubscribes on unmount", () => {
    const unsub = vi.fn();
    const subscribe = vi.fn(() => unsub);
    const adapter = makeMockAdapter({ subscribe });
    const { unmount } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(subscribe).toHaveBeenCalledTimes(1);
    unmount();
    expect(unsub).toHaveBeenCalledTimes(1);
  });

  it("re-subscribes when sessionKey changes", () => {
    const unsub = vi.fn();
    const subscribe = vi.fn(() => unsub);
    const adapter = makeMockAdapter({ subscribe });
    const { rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: null },
          adapter,
          vmlRef: { current: null },
          sessionKey,
        }),
      { initialProps: { sessionKey: "agent:sess1" } },
    );
    expect(subscribe).toHaveBeenCalledTimes(1);
    expect(unsub).toHaveBeenCalledTimes(0);
    rerender({ sessionKey: "agent:sess2" });
    expect(unsub).toHaveBeenCalledTimes(1);
    expect(subscribe).toHaveBeenCalledTimes(2);
  });
});

describe("useScrollController: data-driven init scroll", () => {
  // Helper: build a vmlRef with the imperative methods the controller calls.
  // `scrollToBottom`/`scrollToBlockId` return true: the controller now reads
  // their return value as "the viewport actually moved", so a bare vi.fn()
  // would silently put every case here on the failed-positioning path (and
  // leave a retry rAF pending).
  function makeMockVml() {
    return {
      current: {
        scrollToBottom: vi.fn(() => true),
        scrollToTop: vi.fn(),
        scrollToIndex: vi.fn(),
        scrollToBlockId: vi.fn(() => true),
        getFirstVisibleBlockIndex: vi.fn(() => null),
        getFirstVisibleBlockId: vi.fn(() => null),
        getLastVisibleBlockIndex: vi.fn(() => null),
        isStreamingBlockInViewport: vi.fn(() => false),
      } as any,
    };
  }

  it("calls scrollToBottom when initialAtBottom=true", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
        initialFirstVisibleBlockId: "block-msg-99",
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(vml.current.scrollToBlockId).not.toHaveBeenCalled();
  });

  it("calls scrollToBlockId when initialAtBottom=false and blockId is set", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: "block-msg-30",
      }),
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledWith("block-msg-30");
    expect(vml.current.scrollToBottom).not.toHaveBeenCalled();
  });

  it("falls back to scrollToBottom when atBottom=false and no blockId", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: null,
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(vml.current.scrollToBlockId).not.toHaveBeenCalled();
  });

  it("falls back to scrollToBottom when no initial props (fresh open)", () => {
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("does not re-run init scroll when sessionKey changes within an already-initialized session", () => {
    // The reset effect resets initializedRef on sessionKey change, so the
    // next render with new blocks will run init scroll once for the new
    // session.  We verify that within the same session, a re-render with
    // the same blocksLen does not call scrollToBlockId a second time.
    const vml = makeMockVml();
    const adapter = makeMockAdapter();
    const { rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: null },
          adapter,
          vmlRef: vml,
          sessionKey,
          initialAtBottom: false,
          initialFirstVisibleBlockId: "block-msg-30",
        }),
      { initialProps: { sessionKey: "agent:sess" } },
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledTimes(1);
    rerender({ sessionKey: "agent:sess" });
    expect(vml.current.scrollToBlockId).toHaveBeenCalledTimes(1);
  });

  it("falls back to scrollToBottom when scrollToBlockId returns false (block not found)", () => {
    const vml = makeMockVml();
    // Override: scrollToBlockId returns false (block not in loaded page).
    (vml.current.scrollToBlockId as ReturnType<typeof vi.fn>).mockReturnValue(false);
    const adapter = makeMockAdapter();
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: "block-msg-999",
      }),
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledWith("block-msg-999");
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });
});

describe("useScrollController: followMode auto-follow", () => {
  // Helper: build a vmlRef with scrollToBottom + isStreamingBlockInViewport
  // (the two methods the controller invokes during liveUpdate auto-follow).
  // Returns true from scrollToBottom so init-scroll counts as landed — see
  // the note on makeMockVml above.
  function makeAutoFollowVml() {
    return {
      current: {
        scrollToBottom: vi.fn(() => true),
        scrollToTop: vi.fn(),
        scrollToIndex: vi.fn(),
        scrollToBlockId: vi.fn(() => true),
        getFirstVisibleBlockIndex: vi.fn(() => null),
        getFirstVisibleBlockId: vi.fn(() => null),
        getLastVisibleBlockIndex: vi.fn(() => null),
        isStreamingBlockInViewport: vi.fn(() => false),
      } as any,
    };
  }

  // Helper: build a subscribe mock that captures the callback so we can
  // fire events at will after the useEffect has run.  Synchronously
  // firing inside subscribe() runs DURING render and before effects,
  // which would make init-scroll calls indistinguishable from
  // liveUpdate-triggered calls in the assertions.
  function makeDeferredSubscribe(): {
    subscribe: ReturnType<typeof vi.fn>;
    fire: (event: { type: string }) => void;
  } {
    let cb: ((event: any) => void) | null = null;
    const subscribe = vi.fn((fn: (event: any) => void) => {
      cb = fn;
      return () => {};
    });
    return {
      subscribe,
      fire: (event) => {
        if (!cb) throw new Error("subscribe not yet called");
        cb(event);
      },
    };
  }

  // ── showScrollToBottom behavior ────────────────────────────────────────

  it("fresh session: button hidden even though isAtTail=false (streaming)", () => {
    // Default state — followMode=true (session-reset effect), isNearBottom=true.
    // isAtTail=false simulates pending streaming content not yet in cache.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("jumpToBottom re-enables followMode → button hidden again", () => {
    const scrollToBottom = vi.fn(async () => {});
    const adapter = makeMockAdapter({ isAtTail: () => false, scrollToBottom });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Mount effect (contradiction resolver: isAtTail=false + followMode=true)
    // already fired jumpToBottom once — clear it so we only count the
    // explicit jumpToTop/jumpToBottom sequence below.
    scrollToBottom.mockClear();

    // Exit follow mode via jumpToTop
    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Click the button → re-enter follow mode.  The explicit jumpToBottom
    // (plus the contradiction resolver re-running on the followMode flip)
    // triggers at least one adapter scroll.
    act(() => {
      result.current.jumpToBottom();
    });
    expect(scrollToBottom).toHaveBeenCalled();
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("jumpToTop exits followMode", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Default: button hidden (followMode=true, near bottom)
    expect(result.current.showScrollToBottom).toBe(false);

    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("session switch resets followMode to true", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const { result, rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: null },
          adapter,
          vmlRef: { current: null },
          sessionKey,
        }),
      { initialProps: { sessionKey: "agent:sess1" } },
    );

    // Exit follow mode
    act(() => {
      result.current.jumpToTop();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Switch session → followMode resets to true (rerender flushes the
    // sessionKey change effect).
    rerender({ sessionKey: "agent:sess2" });
    expect(result.current.showScrollToBottom).toBe(false);
  });

  // ── liveUpdate auto-scroll behavior ────────────────────────────────────

  it("liveUpdate forces scrollToBottom when followMode active and near bottom", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe });
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Init-scroll calls scrollToBottom once — clear it so we only count
    // liveUpdate-driven calls.
    vml.current.scrollToBottom.mockClear();

    // Fire liveUpdate AFTER the useEffect has registered (wait one tick).
    await Promise.resolve();
    fire({ type: "liveUpdate" });

    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("liveUpdate does NOT auto-scroll when followMode disabled", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe, isAtTail: () => false });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Mount effect (contradiction resolver) fires jumpToBottom once because
    // isAtTail=false + followMode=true.  Flush its .then chain first, then
    // exit follow mode.
    await Promise.resolve();
    act(() => {
      result.current.jumpToTop();
    });
    vml.current.scrollToBottom.mockClear();

    fire({ type: "liveUpdate" });

    expect(vml.current.scrollToBottom).not.toHaveBeenCalled();
  });

  it("liveUpdate does NOT auto-scroll after the user scrolls away (followMode cancelled)", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    // Container reports a large distance from bottom (> EDGE_THRESHOLD_PX).
    const container = {
      scrollTop: 500,
      scrollHeight: 2000,
      clientHeight: 400, // distFromBottom = 2000-500-400 = 1100 > 50
    } as unknown as HTMLDivElement;
    const adapter = makeMockAdapter({ subscribe });
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // User scrolls away: baseline scroll-down, then a scroll-up past the
    // threshold.  The scroll-up cancels followMode.
    act(() => {
      result.current.handleScroll(); // 500 > prev 0 → down, no cancel
    });
    container.scrollTop = 100;
    act(() => {
      result.current.handleScroll(); // 100 < 500 → up, dist 1500 > 50 → cancel
    });

    vml.current.scrollToBottom.mockClear();

    fire({ type: "liveUpdate" });

    expect(vml.current.scrollToBottom).not.toHaveBeenCalled();
  });

  it("programmatic scroll-down does not cancel followMode", () => {
    // jumpToBottom's programmatic scroll fires a scroll event while the
    // container is already at (or moving to) the bottom.  Moving DOWN must
    // never cancel followMode — this was the bug where clicking the arrow
    // during streaming made the arrow reappear.
    const adapter = makeMockAdapter();
    const container = {
      scrollTop: 1600,
      scrollHeight: 2000,
      clientHeight: 400, // distFromBottom = 0 ≤ threshold
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    act(() => {
      result.current.handleScroll();
    });

    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("user scroll-up away from bottom cancels followMode (arrow reappears)", () => {
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = {
      scrollTop: 500,
      scrollHeight: 2000,
      clientHeight: 400,
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    // followMode defaults to true → arrow hidden even though !isAtTail.
    expect(result.current.showScrollToBottom).toBe(false);

    // Baseline scroll-down, then scroll-up past the threshold.
    act(() => {
      result.current.handleScroll(); // 500 > prev 0 → down
    });
    container.scrollTop = 100;
    act(() => {
      result.current.handleScroll(); // 100 < 500 → up, dist 1500 > 50 → cancel
    });

    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("session switch does not inherit the old session's scrollTop baseline", () => {
    // Regression: `prevScrollTopRef` is the baseline for handleScroll's
    // "programmatic scrolls only move down" direction test.  Carried across
    // a session switch, the baseline was the OUTGOING session's absolute
    // scrollTop — restoring the new (shorter) session to its bottom then
    // read as a user scroll-up, cancelling followMode stickily (neither
    // liveUpdate following nor the contradiction resolver runs without it).
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe });
    const container = {
      scrollTop: 5000,
      scrollHeight: 6000,
      clientHeight: 400, // dist 1600 — mid-page in the long session
    } as unknown as HTMLDivElement;
    const { result, rerender } = renderHook(
      ({ sessionKey }) =>
        useScrollController({
          containerRef: { current: container },
          adapter,
          vmlRef: vml,
          sessionKey,
        }),
      { initialProps: { sessionKey: "agent:sess1" } },
    );

    // Session 1: establish the large baseline.
    act(() => {
      result.current.handleScroll(); // 5000 > 0 → down, no cancel
    });

    // Switch to a shorter session whose bottom sits at scrollTop 800.
    // Virtualizer measurement has not fully landed, so distFromBottom is
    // still above the cancel threshold when the restore's scroll event
    // arrives.
    container.scrollTop = 800;
    container.scrollHeight = 1000;
    container.clientHeight = 50; // dist 150 > 50
    rerender({ sessionKey: "agent:sess2" });
    act(() => {
      result.current.handleScroll();
    });

    // followMode must have survived: a liveUpdate still auto-scrolls.
    vml.current.scrollToBottom.mockClear();
    fire({ type: "liveUpdate" });
    expect(vml.current.scrollToBottom).toHaveBeenCalled();
  });

  it("bug repro: click arrow → programmatic scroll → liveUpdate keeps auto-scrolling", async () => {
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    // isAtTail=false keeps needsLatestContent constant so the assertions
    // don't depend on rAF-driven isNearBottom state updates (which the
    // test environment never runs).
    const adapter = makeMockAdapter({ subscribe, isAtTail: () => false });
    const container = {
      scrollTop: 500,
      scrollHeight: 2000,
      clientHeight: 400,
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Mount effect (contradiction resolver: isAtTail=false + followMode=true)
    // fires jumpToBottom once — flush its .then chain and ignore it.
    await Promise.resolve();
    vml.current.scrollToBottom.mockClear();

    // User had scrolled up → followMode cancelled → arrow visible.
    act(() => {
      result.current.handleScroll(); // down baseline
    });
    container.scrollTop = 100;
    act(() => {
      result.current.handleScroll(); // up → cancel
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Click the arrow → jumpToBottom → followMode re-enabled.
    act(() => {
      result.current.jumpToBottom();
    });
    await Promise.resolve(); // flush adapter.scrollToBottom().then → vml.scrollToBottom

    // The programmatic scroll to the bottom fires a scroll event (scrollTop
    // moves DOWN).  This must NOT cancel followMode.
    container.scrollTop = 1600;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);

    // New stream content arrives → auto-scroll must still fire.
    vml.current.scrollToBottom.mockClear();
    fire({ type: "liveUpdate" });
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });

  it("contradiction resolver: followMode + needsLatestContent triggers jumpToBottom", async () => {
    // isAtTail=false keeps needsLatestContent=true; followMode defaults to
    // true → the resolver must resolve the contradiction by jumping.
    const scrollToBottom = vi.fn(async () => {});
    const vml = makeAutoFollowVml();
    const adapter = makeMockAdapter({ isAtTail: () => false, scrollToBottom });
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );
    await act(async () => {});

    expect(scrollToBottom).toHaveBeenCalled();
  });

  // ── followMode re-arm (symmetric path) ──────────────────────────────

  // Container preset: simulates a session that's been scrolled up.  The
  // baseline "previous" scrollTop is what the controller's
  // prevScrollTopRef last saw — set it explicitly so the direction
  // detection is unambiguous.
  function makeScrolledContainer(scrollTop: number) {
    return {
      scrollTop,
      scrollHeight: 2000,
      clientHeight: 400, // distFromBottom = 2000 - scrollTop - 400
    } as unknown as HTMLDivElement;
  }

  it("user scroll-up then scroll-down into bottom zone re-arms followMode", () => {
    // Bug: a single scroll-up → scroll-down cycle used to leave followMode
    // stuck false, so new messages stopped auto-scrolling even though the
    // user was visually at the tail.  The symmetric re-arm path in
    // handleScroll fixes this.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = makeScrolledContainer(1600); // initially at bottom
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Mount: container.scrollTop=1600, followMode=true.
    // First handleScroll establishes the prevScrollTopRef baseline.
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);

    // User scrolls up past the threshold (60 > 50): followMode cancels.
    container.scrollTop = 60;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // User scrolls back down into the bottom zone (1590 ≤ 50 from
    // bottom): followMode re-arms automatically.
    container.scrollTop = 1590;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);
  });

  it("scroll-up exactly at threshold (50px) does NOT cancel followMode", () => {
    // Edge case: distFromBottom === EDGE_THRESHOLD_PX.  The check uses
    // `> threshold` (strict), so the boundary stays in follow mode.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = makeScrolledContainer(1550); // distFromBottom = 50
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    act(() => {
      result.current.handleScroll();
    });
    // First scroll event establishes the baseline; followMode stays true.
    expect(result.current.showScrollToBottom).toBe(false);

    // Scroll up 1px: distFromBottom becomes 51 > 50 → cancel.
    container.scrollTop = 1549;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("scroll-down outside bottom zone does NOT re-arm followMode", () => {
    // Re-arm only fires when the user has scrolled DOWN *into* the
    // bottom zone.  Scrolling down while still far from the bottom
    // (e.g. user is reading mid-page and nudges the wheel) must not
    // hijack followMode back to true.
    const adapter = makeMockAdapter({ isAtTail: () => false });
    const container = makeScrolledContainer(1600); // start at bottom
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );
    // Baseline establishes the prevScrollTopRef.
    act(() => {
      result.current.handleScroll();
    });

    // Scroll up past the threshold so followMode is genuinely off.
    container.scrollTop = 60;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);

    // Scroll down to mid-page (distFromBottom = 2000-500-400 = 1100):
    // direction is DOWN, but the user is nowhere near the tail.
    container.scrollTop = 500;
    act(() => {
      result.current.handleScroll();
    });
    // followMode stays false because the re-arm path is gated on nearBottom.
    expect(result.current.showScrollToBottom).toBe(true);
  });

  it("liveUpdate keeps auto-scrolling after followMode is re-armed by returning to bottom", async () => {
    // End-to-end smoke: scroll-up → arrow visible → scroll-down into
    // bottom → re-arm → liveUpdate forces scrollToBottom again.
    const vml = makeAutoFollowVml();
    const { subscribe, fire } = makeDeferredSubscribe();
    const adapter = makeMockAdapter({ subscribe, isAtTail: () => false });
    const container = makeScrolledContainer(1600);
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: vml,
        sessionKey: "agent:sess",
      }),
    );

    // Baseline.
    act(() => {
      result.current.handleScroll();
    });
    // Scroll up past threshold → arrow appears, followMode off.
    container.scrollTop = 60;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(true);
    vml.current.scrollToBottom.mockClear();

    // Scroll back down into bottom zone → arrow hides, followMode on.
    container.scrollTop = 1590;
    act(() => {
      result.current.handleScroll();
    });
    expect(result.current.showScrollToBottom).toBe(false);

    // New stream content arrives → auto-scroll fires again.
    fire({ type: "liveUpdate" });
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
  });
});

describe("useScrollController: viewport fill", () => {
  // Regression: after scrollToTop REPLACES the cache with only the head
  // page, if that page folds to fewer pixels than the viewport the pane has
  // no overflow: no scroll event ever fires, the edge checks stop, and the
  // user is stuck (no scrollbar, wheel dead, the rest of the history
  // unreachable).  The fill effect re-runs the edge check on every
  // content / cache change so the next page is pulled deterministically.
  const flushRaf = () =>
    act(async () => {
      await new Promise<void>((r) => requestAnimationFrame(() => r()));
    });

  it("loads the next page when the loaded window is shorter than the pane", async () => {
    const container = {
      scrollTop: 0,
      scrollHeight: 300, // < clientHeight → no overflow
      clientHeight: 600,
    } as unknown as HTMLDivElement;
    const loadNextPage = vi.fn(async () => {});
    const adapter = makeMockAdapter({ hasNewer: true, hasOlder: false, loadNextPage });

    renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    await flushRaf();
    expect(loadNextPage).toHaveBeenCalled();
  });

  it("keeps filling when the range is only a sliver (past the edge threshold, far from a viewport)", async () => {
    // Live repro: after scrollToTop the window held 6 folded blocks = 753px
    // in a 600px pane.  distFromBottom (153) already exceeded
    // EDGE_THRESHOLD_PX (50), so the edge branch stopped filling — the user
    // was left with a 153px range: a thumb covering ~80% of the 6px track
    // (reads as "no scrollbar"), one wheel notch and stuck.  The fill rule
    // must keep going until the range is worth scrolling.
    const container = {
      scrollTop: 0,
      scrollHeight: 753,
      clientHeight: 600,
    } as unknown as HTMLDivElement;
    const loadNextPage = vi.fn(async () => {});
    const adapter = makeMockAdapter({ hasNewer: true, hasOlder: false, loadNextPage });

    renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    await flushRaf();
    expect(loadNextPage).toHaveBeenCalled();
  });

  it("does not over-fetch when the loaded window already overflows the pane", async () => {
    const container = {
      scrollTop: 0,
      scrollHeight: 2000, // > clientHeight → scrollable, user at top
      clientHeight: 600,
    } as unknown as HTMLDivElement;
    const loadNextPage = vi.fn(async () => {});
    const loadPrevPage = vi.fn(async () => {});
    const adapter = makeMockAdapter({
      hasNewer: true,
      hasOlder: false,
      loadNextPage,
      loadPrevPage,
    });

    renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    await flushRaf();
    expect(loadNextPage).not.toHaveBeenCalled();
    expect(loadPrevPage).not.toHaveBeenCalled();
  });

  it("keeps pulling past the old 10-page budget while a long explore run is still truncated", async () => {
    // Regression: pagination is measured in RAW JSONL entries, but the UI
    // folds a whole thought/tool_call/tool_result run into ONE explore
    // block whose rendered height is CAPPED (ExploreBlock scrolls its
    // content internally).  Pulling the rest of that run therefore adds
    // hundreds of entries and ZERO pixels, so a page-count budget stops
    // exactly while the rule is still doing useful work.  The old budget
    // (10 pages = 500 entries) expired mid-run on the 617-entry explore
    // found in real session logs, and because tripping it `return`ed out of
    // checkEdges AHEAD of the edge branches, the remaining history became
    // unreachable in a pane that still had no scroll range — i.e. no user
    // scroll event could ever re-arm pagination.  The session looked
    // "loaded but not full screen" with `hasOlder` still true.
    const PAGE = 50;
    const TOTAL = 617;
    let offset = TOTAL - PAGE; // a bare tail page, starting mid-run
    let fire: (event: { type: string }) => void = () => {};
    const loadPrevPage = vi.fn(async () => {
      offset = Math.max(0, offset - PAGE);
      fire({ type: "pageLoaded", direction: "prev", offset, limit: PAGE, total: TOTAL });
    });
    const adapter = {
      ...makeMockAdapter({ loadPrevPage }),
      subscribe: (cb: (e: any) => void) => { fire = cb; return () => {}; },
      get messageOffset() { return offset; },
      get messageLimit() { return TOTAL - offset; },
      get hasOlder() { return offset > 0; },
      hasNewer: false,
      // The pane never overflows: the single explore block is height-capped
      // no matter how many entries fold into it.
    } as unknown as ChatListAdapterV2;
    const container = {
      scrollTop: 0,
      scrollHeight: 180,
      clientHeight: 600,
    } as unknown as HTMLDivElement;

    renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    // 12 walks 567 → 0.  Allow headroom so the loop must stop on its own.
    for (let i = 0; i < 20; i++) await flushRaf();

    expect(offset).toBe(0);
    expect(loadPrevPage.mock.calls.length).toBeGreaterThan(10);
    // Terminates on exhausted data, not on a budget: no 13th request.
    await flushRaf();
    expect(loadPrevPage.mock.calls.length).toBe(12);
  });

  it("issues at most one fill load per window state when the adapter makes no progress", async () => {
    // The runaway guard is a PROGRESS guard, not a page count.  A load that
    // did not move the cursor (no-op, duplicate response, failure) must not
    // be retried once per frame forever, and the near-top edge branch must
    // not sneak a second request in — an under-filled pane pins scrollTop
    // at 0, so "near an edge" is not a user-driven signal there.
    const loadPrevPage = vi.fn(async () => {});
    const adapter = makeMockAdapter({ hasOlder: true, hasNewer: false, loadPrevPage });
    const container = {
      scrollTop: 0,
      scrollHeight: 180,
      clientHeight: 600,
    } as unknown as HTMLDivElement;

    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: { current: null },
        sessionKey: "agent:sess",
      }),
    );

    for (let i = 0; i < 5; i++) await flushRaf();
    act(() => { result.current.handleScroll(); });
    expect(loadPrevPage).toHaveBeenCalledTimes(1);
  });
});

/**
 * Positioning contract (init scroll).
 *
 * The one-shot `initializedRef` gate must be consumed only when a scroll
 * actually LANDS, and `isPositioned()` must tell the truth to ChatPanel's
 * snapshot writer.  Both matter because a nav-back remount can race the
 * virtualizer attaching its scroll element: `scrollToBottom()` then moves
 * nothing, and if that counted as success ChatPanel would persist the
 * untouched scroll-top-0 viewport as the session's reading position —
 * latching the session to the top for every later visit.
 */
describe("useScrollController: isPositioned() reflects real positioning", () => {
  function makeVmlReturning(ok: boolean) {
    return {
      current: {
        scrollToBottom: vi.fn(() => ok),
        scrollToTop: vi.fn(),
        scrollToIndex: vi.fn(),
        scrollToBlockId: vi.fn(() => ok),
        getFirstVisibleBlockIndex: vi.fn(() => null),
        getFirstVisibleBlockId: vi.fn(() => null),
        getLastVisibleBlockIndex: vi.fn(() => null),
        isStreamingBlockInViewport: vi.fn(() => false),
      } as any,
    };
  }

  const flushRaf = () =>
    act(async () => {
      await new Promise<void>((r) => requestAnimationFrame(() => r()));
    });

  it("true once the requested scroll lands", () => {
    const vml = makeVmlReturning(true);
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: makeMockAdapter(),
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(result.current.isPositioned()).toBe(true);
  });

  it("false while every scroll attempt no-ops (remount race)", () => {
    const vml = makeVmlReturning(false);
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: makeMockAdapter(),
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );
    // ChatPanel refuses to write a snapshot while this is false, so the
    // session keeps its previous (real) restore target instead of latching
    // to the top.
    expect(result.current.isPositioned()).toBe(false);
  });

  it("retries across frames instead of giving up after one attempt", async () => {
    const vml = makeVmlReturning(false);
    renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: makeMockAdapter(),
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );
    await flushRaf();
    await flushRaf();
    // An idle session produces no further commit, so recovery can only come
    // from the retry loop — one attempt would silently leave it at the top.
    expect(vml.current.scrollToBottom.mock.calls.length).toBeGreaterThanOrEqual(2);
  });

  it("counts the blockId fallback at bottom as positioned", () => {
    // Compaction can delete the block the user was reading; the controller
    // must not strand them mid-list.
    const base = makeVmlReturning(true).current;
    const vml = { current: { ...base, scrollToBlockId: vi.fn(() => false) } as any };
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: makeMockAdapter(),
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: false,
        initialFirstVisibleBlockId: "block-removed",
      }),
    );
    expect(vml.current.scrollToBlockId).toHaveBeenCalledWith("block-removed");
    expect(vml.current.scrollToBottom).toHaveBeenCalledTimes(1);
    expect(result.current.isPositioned()).toBe(true);
  });

  it("does not stall pagination when positioning fails", async () => {
    // The two flags are deliberately separate: initializedRef ("attempted")
    // gates edge detection, positionedRef ("landed") gates the snapshot
    // writer.  Conflating them would hold loadPrevPage dead for the whole
    // retry window on every session whose list mounts late.
    const loadPrevPage = vi.fn(async () => {});
    const adapter = makeMockAdapter({ hasOlder: true, loadPrevPage });
    const container = {
      scrollTop: 0,
      scrollHeight: 2000,
      clientHeight: 400,
    } as unknown as HTMLDivElement;
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter,
        vmlRef: makeVmlReturning(false),
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );

    act(() => {
      result.current.handleScroll();
    });
    await flushRaf();

    expect(result.current.isPositioned()).toBe(false);
    expect(loadPrevPage).toHaveBeenCalled();
  });

  it("recovers when the imperative handle appears after the first pass", async () => {
    // Regression: the init-scroll effect used to bail out on a null handle
    // AFTER consuming `initializedRef`.  Since that gate is one-shot per
    // session, the effect never re-ran, so a late-mounting VML left the
    // session unpositioned forever and permanently disabled the snapshot
    // writer.  The retry loop must cover the null-handle case too.
    const vmlRef = { current: null } as { current: any | null };
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: makeMockAdapter(),
        vmlRef,
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );
    expect(result.current.isPositioned()).toBe(false);

    // VML mounts on a later frame.
    const vml = makeVmlReturning(true);
    vmlRef.current = vml.current;
    await flushRaf();

    expect(vml.current.scrollToBottom).toHaveBeenCalled();
    expect(result.current.isPositioned()).toBe(true);
  });

  it("keeps retrying across a data change during the retry window", async () => {
    // The retry loop is owned by the SESSION, not by one render pass.  A
    // blocksLen change re-runs the init-scroll effect, whose body then
    // returns early on `initializedRef`; if that re-run had also torn down
    // the pending loop, positioning would be abandoned exactly when new
    // content keeps arriving (streaming, page loads).
    const vml = makeVmlReturning(false);
    const { rerender } = renderHook(
      ({ count }: { count: number }) =>
        useScrollController({
          containerRef: { current: null },
          adapter: makeMockAdapter({
            blocks: Array.from({ length: count }, (_, i) => ({
              ...DUMMY_BLOCK,
              blockId: `block-${i}`,
            })),
          }),
          vmlRef: vml,
          sessionKey: "agent:sess",
          initialAtBottom: true,
        }),
      { initialProps: { count: 1 } },
    );

    await flushRaf();
    const afterFrame1 = vml.current.scrollToBottom.mock.calls.length;
    expect(afterFrame1).toBeGreaterThanOrEqual(2);

    rerender({ count: 2 });
    await flushRaf();

    // Still polling after the re-render — the loop survived it.
    expect(vml.current.scrollToBottom.mock.calls.length).toBeGreaterThan(afterFrame1);
  });

  it("gives up at the deadline instead of polling forever", async () => {
    let now = 0;
    const nowSpy = vi.spyOn(performance, "now").mockImplementation(() => now);
    try {
      const vml = makeVmlReturning(false);
      const { result } = renderHook(() =>
        useScrollController({
          containerRef: { current: null },
          adapter: makeMockAdapter(),
          vmlRef: vml,
          sessionKey: "agent:sess",
          initialAtBottom: true,
        }),
      );
      const first = vml.current.scrollToBottom.mock.calls.length;

      // Jump past INIT_SCROLL_MAX_MS and give the loop one more frame.
      now = 10_000;
      await flushRaf();

      expect(vml.current.scrollToBottom.mock.calls.length).toBe(first + 1);
      expect(result.current.isPositioned()).toBe(false);

      // No further frames are scheduled: the loop is done.
      const settled = vml.current.scrollToBottom.mock.calls.length;
      await flushRaf();
      await flushRaf();
      expect(vml.current.scrollToBottom.mock.calls.length).toBe(settled);
    } finally {
      nowSpy.mockRestore();
    }
  });

  it("noteViewportInteraction() makes the view recordable and stops the loop", async () => {
    // A session whose init-scroll gave up must not stay permanently
    // un-snapshottable: once the user scrolls somewhere themselves, that IS
    // a reading position worth persisting.  Their input also has to end the
    // retry loop, or the next attempt yanks the viewport back to the tail.
    const vml = makeVmlReturning(false);
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: null },
        adapter: makeMockAdapter(),
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );
    expect(result.current.isPositioned()).toBe(false);

    act(() => {
      result.current.noteViewportInteraction();
    });
    expect(result.current.isPositioned()).toBe(true);

    const calls = vml.current.scrollToBottom.mock.calls.length;
    await flushRaf();
    await flushRaf();
    expect(vml.current.scrollToBottom.mock.calls.length).toBe(calls);
  });

  it("does not record an interaction at the unpositioned origin as a reading position", () => {
    // The hole noteViewportInteraction must not re-open: positioning failed,
    // so the list sits at scrollTop 0.  A stray click there is not "where the
    // user was reading" — recording it would latch the session to the top on
    // every later visit, which is the exact symptom being fixed.
    const container = { scrollTop: 0 } as unknown as HTMLDivElement;
    const vml = makeVmlReturning(false);
    const { result } = renderHook(() =>
      useScrollController({
        containerRef: { current: container },
        adapter: makeMockAdapter(),
        vmlRef: vml,
        sessionKey: "agent:sess",
        initialAtBottom: true,
      }),
    );
    expect(result.current.isPositioned()).toBe(false);

    act(() => {
      result.current.noteViewportInteraction();
    });

    // Still not recordable — ChatPanel will therefore keep this session's
    // previous snapshot instead of persisting the bogus top position.
    expect(result.current.isPositioned()).toBe(false);

    // Once the viewport actually moves, the same interaction does open it up.
    container.scrollTop = 640;
    act(() => {
      result.current.noteViewportInteraction();
    });
    expect(result.current.isPositioned()).toBe(true);
  });

  it("still positions the session when the user interacts before data lands", () => {
    // Interaction marks the view recordable; it must NOT be read as "the
    // viewport is already where it should be".  A click or wheel over an
    // empty, still-loading viewport happens all the time — the init scroll
    // still owes that session its restore.
    const vml = makeVmlReturning(true);
    const { rerender, result } = renderHook(
      ({ count }: { count: number }) =>
        useScrollController({
          containerRef: { current: null },
          adapter: makeMockAdapter({
            blocks: Array.from({ length: count }, (_, i) => ({
              ...DUMMY_BLOCK,
              blockId: `block-${i}`,
            })),
          }),
          vmlRef: vml,
          sessionKey: "agent:sess",
          initialAtBottom: true,
        }),
      { initialProps: { count: 0 } },
    );

    act(() => {
      result.current.noteViewportInteraction();
    });
    rerender({ count: 3 });

    expect(vml.current.scrollToBottom).toHaveBeenCalled();
    expect(result.current.isPositioned()).toBe(true);
  });

  it("resets positioning when the session changes", () => {
    // positionedRef is per-session.  Without the reset, the flag set by
    // session A would let session B's never-positioned (scroll-top-0)
    // viewport be snapshotted as B's reading position — the original latch.
    const vml = makeVmlReturning(true);
    const { rerender, result } = renderHook(
      ({ sessionKey }: { sessionKey: string }) =>
        useScrollController({
          containerRef: { current: null },
          adapter: makeMockAdapter(),
          vmlRef: vml,
          sessionKey,
          initialAtBottom: true,
        }),
      { initialProps: { sessionKey: "agent:sess" } },
    );
    expect(result.current.isPositioned()).toBe(true);

    // B cannot be positioned (every scroll no-ops), so the flag must be back
    // to false — proving A's true did not leak across the session switch.
    vml.current.scrollToBottom.mockImplementation(() => false);
    rerender({ sessionKey: "agent:sess2" });

    expect(result.current.isPositioned()).toBe(false);
  });
});
