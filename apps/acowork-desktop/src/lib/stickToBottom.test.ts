/**
 * Follow-rule tests for the desktop `FollowController` (the scroll-hijack fix
 * that made the wake-up signal usable — design doc 25 §8).
 *
 * The store now re-reads the open thread on every `chat/changed` signal, so
 * "refresh" is no longer something the user caused. These cases pin that only
 * a reader who is at the bottom gets moved:
 *
 *   1. pinned -> follow; scrolled-up -> survives any number of refreshes;
 *   2. a thread switch (`forceKey`) and a send (`stickNow`) re-claim the follow;
 *   3. `onBecamePinned` fires on the unpinned -> pinned flip and on nothing
 *      else — it is the only way the panel learns a scroll happened, and the
 *      read receipt depends on it;
 *   4. the hook binding actually wires the container, the effect and the
 *      callback ref together (driven with a faked jsdom element).
 *
 * `FollowController` itself is React-free, so the bulk of it needs no renderer.
 */
import { describe, expect, it, vi } from "vitest";
import { act, renderHook } from "@testing-library/react";
import {
  FollowController,
  isNearBottom,
  STICK_PX,
  useStickToBottom,
  type ScrollEl,
} from "./stickToBottom";

/**
 * Stand-in for a scroll container: only the surface the controller touches.
 * `scrollTop` clamps like a real browser, so the max is
 * scrollHeight - clientHeight = 1000 - 300 = 700.
 */
class FakeEl implements ScrollEl {
  scrollHeight = 1000;
  clientHeight = 300;
  private top = 0;
  private fns: (() => void)[] = [];
  get scrollTop(): number {
    return this.top;
  }
  set scrollTop(v: number) {
    this.top = Math.max(0, Math.min(v, this.scrollHeight - this.clientHeight));
  }
  get bottom(): number {
    return this.scrollHeight - this.clientHeight;
  }
  addEventListener(_type: "scroll", fn: () => void): void {
    this.fns.push(fn);
  }
  removeEventListener(_type: "scroll", fn: () => void): void {
    this.fns = this.fns.filter((f) => f !== fn);
  }
  /** A user scroll: move (clamped) and fire the listeners, like a browser. */
  userScrollTo(top: number): void {
    this.scrollTop = top;
    this.fns.forEach((f) => f());
  }
  /** Content growth (new messages): the follow decision runs against it. */
  grow(by: number): void {
    this.scrollHeight += by;
  }
  listenerCount(): number {
    return this.fns.length;
  }
}

function bound(): { c: FollowController; el: FakeEl } {
  const c = new FollowController();
  const el = new FakeEl();
  c.bind(el);
  return { c, el };
}

describe("FollowController", () => {
  it("a first refresh re-pins by the view key and follows to the bottom", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    expect(el.scrollTop).toBe(el.bottom);
    // new content lands while pinned — follow
    el.grow(200);
    c.refresh("peer1");
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("a user scroll-up survives every refresh that follows", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    el.userScrollTo(0); // far from the bottom: gap 700 > STICK_PX
    el.grow(200);
    c.refresh("peer1");
    el.grow(200);
    c.refresh("peer1");
    expect(el.scrollTop).toBe(0);
  });

  it("returning near the bottom re-pins the follow", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    el.userScrollTo(0);
    c.refresh("peer1");
    expect(el.scrollTop).toBe(0);
    el.userScrollTo(el.bottom - STICK_PX); // exactly at the threshold: pinned again
    el.grow(200);
    c.refresh("peer1");
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("switching the view key re-pins even after a scroll-up", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    el.userScrollTo(0);
    c.refresh("peer2");
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("stickNow re-pins and keeps following (the send path)", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    el.userScrollTo(0);
    c.stickNow();
    expect(el.scrollTop).toBe(el.bottom);
    el.grow(200);
    c.refresh("peer1");
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("bind(null) detaches: refresh becomes a no-op and old scrolls stop mattering", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    c.bind(null);
    el.userScrollTo(0); // would unpin if the listener were still attached
    expect(() => c.refresh("peer1")).not.toThrow();
    expect(el.scrollTop).toBe(0); // nothing moved
  });

  it("rebinding adopts the new container position (no scroll-up is lost)", () => {
    const { c } = bound();
    c.refresh("peer1");
    const el2 = new FakeEl();
    el2.scrollTop = 0; // a remounted DOM node starts at the top of overflowing content
    c.bind(el2);
    expect(c.pinned).toBe(false);
    el2.grow(200);
    c.refresh("peer1"); // same key: no force — the adoption stands
    expect(el2.scrollTop).toBe(0);
  });

  it("without a container every method is a no-op", () => {
    const c = new FollowController();
    expect(() => {
      c.refresh("peer1");
      c.stickNow();
    }).not.toThrow();
    expect(c.pinned).toBe(true);
  });
});

describe("onBecamePinned", () => {
  /** Bind with the callback and return a call counter. */
  function withCallback(): { c: FollowController; el: FakeEl; hits: () => number } {
    const c = new FollowController();
    const el = new FakeEl();
    let n = 0;
    c.bind(el, () => {
      n += 1;
    });
    return { c, el, hits: () => n };
  }

  it("fires when the reader scrolls down into the bottom band", () => {
    const { c, el, hits } = withCallback();
    c.refresh("peer1");
    el.userScrollTo(0);
    expect(hits()).toBe(0); // going up is not an arrival
    el.userScrollTo(el.bottom - STICK_PX);
    expect(hits()).toBe(1);
  });

  it("does not fire again while the reader stays pinned", () => {
    // The auto-scroll refresh() performs has `was === true`, so the receipt
    // storm this guards against (one POST per arriving signal) never starts.
    const { c, el, hits } = withCallback();
    c.refresh("peer1");
    el.grow(200);
    c.refresh("peer1");
    el.userScrollTo(el.bottom); // a scroll while already pinned
    el.userScrollTo(el.bottom - 1);
    expect(hits()).toBe(0);
  });

  it("does not fire for a scroll-up, and fires again after each departure", () => {
    const { c, el, hits } = withCallback();
    c.refresh("peer1");
    el.userScrollTo(0);
    expect(hits()).toBe(0);
    el.userScrollTo(el.bottom);
    expect(hits()).toBe(1);
    el.userScrollTo(0);
    el.userScrollTo(el.bottom);
    expect(hits()).toBe(2);
  });

  it("is replaced, not stacked, when the same container is re-bound", () => {
    // The hook re-binds on every render with a fresh closure; binding the same
    // element must swap the callback and leave exactly one scroll listener.
    const c = new FollowController();
    const el = new FakeEl();
    const first = vi.fn();
    const second = vi.fn();
    c.bind(el, first);
    c.bind(el, second);
    expect(el.listenerCount()).toBe(1);
    c.refresh("peer1");
    el.userScrollTo(0);
    el.userScrollTo(el.bottom);
    expect(first).not.toHaveBeenCalled();
    expect(second).toHaveBeenCalledTimes(1);
  });
});

describe("isNearBottom", () => {
  it("uses the STICK_PX threshold inclusive", () => {
    expect(isNearBottom({ scrollHeight: 1000, scrollTop: 700, clientHeight: 300 })).toBe(true);
    expect(
      isNearBottom({ scrollHeight: 1000, scrollTop: 700 - STICK_PX, clientHeight: 300 }),
    ).toBe(true);
    expect(
      isNearBottom({ scrollHeight: 1000, scrollTop: 700 - STICK_PX - 1, clientHeight: 300 }),
    ).toBe(false);
  });

  it("treats a non-overflowing container as at-bottom", () => {
    expect(isNearBottom({ scrollHeight: 200, scrollTop: 0, clientHeight: 300 })).toBe(true);
  });
});

/* ---------------- the hook binding ---------------- */

/*
 * Driven with the same `FakeEl` the controller tests use: jsdom has no layout
 * engine, so a real `div` reports `scrollHeight === 0` and silently ignores
 * every `scrollTop` write — the wiring would "pass" without proving anything.
 * The hook only ever hands its ref to a DOM node in the app, and a node
 * satisfies `ScrollEl` structurally, so the cast is the whole extent of the
 * faking.
 */
function bindHook(
  initial: { count: number; peer: string },
) {
  const el = new FakeEl();
  const { result, rerender } = renderHook(
    ({ count, peer }: { count: number; peer: string }) => useStickToBottom([count], peer),
    { initialProps: initial },
  );
  // Callback refs run before effects in a real render; replay that order.
  act(() => result.current.ref(el as unknown as HTMLDivElement));
  // The mount effect already ran with nothing bound, so a caller that wants the
  // first-follow behaviour has to trigger one content render — which is also
  // what a real screen does, since the page arrives after the container mounts.
  return { el, result, rerender };
}

describe("useStickToBottom", () => {
  it("the effect follows new content while pinned and stops after a scroll-up", () => {
    const { el, result, rerender } = bindHook({ count: 1, peer: "peer1" });
    act(() => rerender({ count: 2, peer: "peer1" }));
    expect(el.scrollTop).toBe(el.bottom);

    el.userScrollTo(0); // a user scroll-up
    expect(result.current.isPinned()).toBe(false);

    el.grow(400);
    act(() => rerender({ count: 3, peer: "peer1" }));
    expect(el.scrollTop).toBe(0); // the signal did not move the reader

    el.userScrollTo(el.bottom - STICK_PX);
    expect(result.current.isPinned()).toBe(true);
    act(() => result.current.stickNow());
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("a thread switch re-pins through the same effect", () => {
    const { el, result, rerender } = bindHook({ count: 1, peer: "peer1" });
    el.userScrollTo(0);
    expect(result.current.isPinned()).toBe(false);

    act(() => rerender({ count: 1, peer: "peer2" }));
    expect(result.current.isPinned()).toBe(true);
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("exposes a reactive atBottom flag so a screen can show the back-to-bottom arrow", () => {
    // `atBottom` is the only follow signal a render can read: `isPinned()` is a
    // plain ref read that never re-renders, so the arrow would not appear on a
    // scroll. This flag flips on every transition without moving the viewport.
    const { el, result, rerender } = bindHook({ count: 1, peer: "peer1" });
    // A content render re-pins and follows to the bottom -> arrow hidden.
    act(() => rerender({ count: 2, peer: "peer1" }));
    expect(result.current.atBottom).toBe(true);

    // Scroll up off the tail -> flag flips false (the screen shows the arrow),
    // and crucially the reader is NOT pulled back down by the re-render.
    act(() => el.userScrollTo(0));
    expect(result.current.atBottom).toBe(false);

    // Scroll back into the bottom band -> re-pins -> arrow hides again.
    act(() => el.userScrollTo(el.bottom - STICK_PX));
    expect(result.current.atBottom).toBe(true);

    // The send path re-claims the follow via stickNow.
    act(() => el.userScrollTo(0));
    expect(result.current.atBottom).toBe(false);
    act(() => result.current.stickNow());
    expect(result.current.atBottom).toBe(true);
  });

  it("re-renders keep the latest onBecamePinned closure without stacking listeners", () => {
    // The callback lives in a ref precisely so a screen can pass a fresh
    // closure every render; a re-render must not re-add a scroll listener
    // (that would POST the read receipt once per listener).
    const onBecamePinned = vi.fn();
    const el = new FakeEl();
    const { result, rerender } = renderHook(
      ({ n }: { n: number }) => useStickToBottom([n], "peer1", onBecamePinned),
      { initialProps: { n: 1 } },
    );
    act(() => result.current.ref(el as unknown as HTMLDivElement));
    act(() => rerender({ n: 2 }));
    act(() => rerender({ n: 3 }));
    expect(el.listenerCount()).toBe(1);

    el.userScrollTo(0);
    el.userScrollTo(el.bottom);
    expect(onBecamePinned).toHaveBeenCalledTimes(1);
  });

  it("unmounting the container (ref null) leaves later refreshes inert", () => {
    const { el, result, rerender } = bindHook({ count: 1, peer: "peer1" });
    act(() => rerender({ count: 2, peer: "peer1" }));
    expect(el.scrollTop).toBe(el.bottom);

    act(() => result.current.ref(null)); // what React calls on unmount
    el.grow(400);
    act(() => rerender({ count: 2, peer: "peer1" }));
    expect(el.scrollTop).toBe(700); // not dragged to the new bottom
    expect(el.listenerCount()).toBe(0);
  });
});
