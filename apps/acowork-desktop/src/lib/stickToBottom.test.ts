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
import { act, render, renderHook } from "@testing-library/react";
import { createElement } from "react";
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
    const clamped = Math.max(0, Math.min(v, this.scrollHeight - this.clientHeight));
    if (clamped === this.top) return;
    this.top = clamped;
    // Like a browser, ANY offset change dispatches a scroll event — including
    // a programmatic bottom-pin write. The browser delivers it asynchronously;
    // the stale-event case (the delivery lands AFTER a later commit grew the
    // content) is modelled explicitly with `fireScroll()`.
    this.fns.forEach((f) => f());
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
    const before = this.top;
    this.scrollTop = top; // fires when the offset actually moved
    if (this.top === before) this.fns.forEach((f) => f()); // same-spot scroll still dispatches
  }
  /** Deliver a pending scroll event against the CURRENT geometry: models the
   *  event of an earlier programmatic write arriving late, after content grew. */
  fireScroll(): void {
    this.fns.forEach((f) => f());
  }
  /** Content growth (new messages): the browser dispatches NO scroll for this. */
  grow(by: number): void {
    this.scrollHeight += by;
  }
  /** Content shrink: the browser clamps an out-of-range scrollTop, and the
   *  clamp lands AT the new bottom (dispatching through the setter). */
  shrink(by: number): void {
    this.scrollHeight -= by;
    if (this.top > this.bottom) this.scrollTop = this.bottom;
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

  it("re-binding the same node (React callback-ref churn) keeps the follow bit", () => {
    // The startup shape of every chat surface: the container mounts empty, then
    // the newest page lands. A screen that merges the hook ref with its own
    // capture in an inline arrow hands React a new identity per render, so the
    // node is detached (`null`) and re-attached. Re-deriving `pinned` from that
    // node's offset read `scrollTop === 0` under brand-new content as a
    // scroll-up and vetoed the follow for the whole session — the reader
    // opened a conversation on its OLDEST row.
    const c = new FollowController();
    const el = new FakeEl();
    c.bind(el);
    c.refresh("peer1"); // nothing to follow yet
    expect(el.scrollTop).toBe(el.bottom);
    el.grow(900); // the page arrives below the pinned viewport
    c.bind(null);
    c.bind(el);
    expect(c.pinned).toBe(true);
    c.refresh("peer1"); // same view key, no force
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("a container swap preserves the intent a scroll recorded, not the node's offset", () => {
    // A fresh DOM node always starts at the top of overflowing content, so
    // geometry says nothing here either way: what carries over is the reader's
    // recorded decision.
    const { c, el } = bound();
    c.refresh("peer1");
    el.userScrollTo(0); // a real scroll-up
    expect(c.pinned).toBe(false);
    const el2 = new FakeEl();
    c.bind(el2);
    el2.grow(200);
    c.refresh("peer1"); // same key: the recorded scroll-up stands
    expect(el2.scrollTop).toBe(0);
    c.stickNow(); // and the follow can still be re-claimed
    expect(el2.scrollTop).toBe(el2.bottom);
  });

  it("a container attached after the page landed is positioned on arrival", () => {
    // The other startup ordering: screens swap the scroll container across
    // early-return branches, so the content can exist before the node does and
    // the refresh that carried it found no container. Attaching must follow then
    // — otherwise the reader sits at the oldest row with no arrow to offer
    // (the follow bit says they never left the tail).
    const c = new FollowController();
    const el = new FakeEl();
    el.grow(900);
    c.refresh("peer1"); // no container yet: nothing to move
    expect(el.scrollTop).toBe(0);
    c.bind(el);
    expect(el.scrollTop).toBe(el.bottom);
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

/* ---------------- the unpin direction guard ---------------- */

/*
 * On a virtualized pane the scroll event produced by our own bottom-pin write
 * can be DELIVERED after a later commit has grown the content: the geometry
 * then reads "far from the bottom" although nobody moved. Unpinning on that
 * stale event abandoned the follow mid-convergence — the transcript landed at
 * the bottom and drifted up as measurements settled (the agent-switch drift).
 * Unpin therefore requires a genuine upward move; a deliberate programmatic
 * departure (a locate jump parking mid-history) releases via explicit unpin().
 */
describe("FollowController unpin direction guard", () => {
  it("a stale scroll event after content growth does NOT unpin the follow", () => {
    const { c, el } = bound();
    c.refresh("peer1"); // pinned-on-arrival write lands at the bottom
    expect(el.scrollTop).toBe(el.bottom);
    el.grow(500); // a later commit expands the pane (measurements settling)
    el.fireScroll(); // OUR pin write's event, delivered late: far from bottom,
                     // but the offset did not move up — no reader intent.
    expect(c.pinned).toBe(true);
    c.refresh("peer1"); // the follow still re-pins
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("a genuine upward move away from the bottom unpins", () => {
    const { c, el } = bound();
    c.refresh("peer1"); // pinned at the bottom (700)
    el.grow(500); // content grew; the reader sits above the new bottom, still pinned
    el.userScrollTo(400); // moved UP (700 -> 400) and away: reader intent
    expect(c.pinned).toBe(false);
  });

  it("a shrink clamp lands at the new bottom and keeps the follow", () => {
    const { c, el } = bound();
    c.refresh("peer1");
    el.shrink(400); // window replacement shrinks the pane; the browser clamps
    expect(el.scrollTop).toBe(el.bottom); // clamp landed at the bottom
    expect(c.pinned).toBe(true); // not mistaken for a scroll-up
  });

  it("explicit unpin() releases the follow for a programmatic departure", () => {
    const { c, el } = bound();
    c.refresh("peer1"); // pinned at the bottom (700)
    el.grow(500); // window replacement: the pane is taller, bottom now 1200
    el.scrollTop = 900; // a locate jump lands DOWNWARD, mid-history: the
                        // offset moved down, so the guard reads no intent —
                        // follow would survive the jump and yank the reader back.
    expect(c.pinned).toBe(true); // the guard alone cannot release a downward move
    c.unpin(); // the locate releases explicitly (useLocateMessage's onLocate)
    expect(c.pinned).toBe(false);
    el.grow(200);
    c.refresh("peer1"); // same key: the release stands, nothing moves
    expect(el.scrollTop).toBe(900);
  });
});

/* ---------------- the bottom-positioning delegate ---------------- */
/*
 * A virtualized transcript cannot be landed on its bottom with a single raw
 * `scrollTop = scrollHeight` write: the pane's total height is an estimate
 * that converges asynchronously, and on the session-switch commit the list
 * may not even have a scroll element yet. The surface therefore hands the
 * controller a MECHANISM (`setGoToBottom`, wired from `useStickToBottom`'s
 * `goToBottom` arg); the controller keeps the POLICY (the pinned bit). The
 * raw write stays as the fallback for plain-DOM consumers.
 */
describe("FollowController bottom-positioning delegate", () => {
  function withDelegate(): { c: FollowController; el: FakeEl; hits: () => number } {
    const c = new FollowController();
    const el = new FakeEl();
    let n = 0;
    c.setGoToBottom(() => {
      n += 1;
      return true;
    });
    return { c, el, hits: () => n };
  }

  it("every bottom write goes through the delegate, never the raw assignment", () => {
    const { c, el, hits } = withDelegate();
    c.bind(el); // pinned-on-arrival positioning
    c.refresh("peer1");
    c.refresh("peer1");
    c.stickNow();
    expect(hits()).toBe(4);
    expect(el.scrollTop).toBe(0); // the raw write never ran
  });

  it("a delegate that declines falls back to the raw write", () => {
    const c = new FollowController();
    const el = new FakeEl();
    c.setGoToBottom(() => false); // e.g. no list handle mounted yet
    c.bind(el);
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("clearing the delegate restores the raw write (plain-DOM consumer)", () => {
    const { c, el } = withDelegate();
    c.setGoToBottom(null);
    c.bind(el);
    c.refresh("peer1");
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("a scroll-up still vetoes the delegate: positioning is mechanism, following is policy", () => {
    const { c, el, hits } = withDelegate();
    c.bind(el);
    c.refresh("peer1"); // establish the view key; the delegate arms
    // The armed mechanism's landing write (the shell pins on its next commit).
    // A no-op delegate models the arm; the element only reaches the bottom
    // when that write lands, exactly like the virtualized pane.
    el.scrollTop = el.bottom;
    const before = hits();
    el.userScrollTo(0); // a real scroll-up: the offset genuinely moved up
    c.refresh("peer1"); // same key, scrolled up -> no positioning at all
    expect(hits()).toBe(before);
  });

  it("the hook routes the 5th argument into the controller and refresh through it", () => {
    const el = new FakeEl();
    const goToBottom = vi.fn(() => true);
    const { result, rerender } = renderHook(
      ({ n }: { n: number }) => useStickToBottom([n], "s", undefined, undefined, goToBottom),
      { initialProps: { n: 1 } },
    );
    act(() => result.current.ref(el as unknown as HTMLDivElement));
    act(() => rerender({ n: 2 }));
    expect(goToBottom).toHaveBeenCalled();
    expect(el.scrollTop).toBe(0); // the raw write never ran
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

    act(() => el.userScrollTo(0)); // a user scroll-up
    expect(result.current.isPinned()).toBe(false);

    el.grow(400);
    act(() => rerender({ count: 3, peer: "peer1" }));
    expect(el.scrollTop).toBe(0); // the signal did not move the reader

    act(() => el.userScrollTo(el.bottom - STICK_PX));
    expect(result.current.isPinned()).toBe(true);
    act(() => result.current.stickNow());
    expect(el.scrollTop).toBe(el.bottom);
  });

  it("a thread switch re-pins through the same effect", () => {
    const { el, result, rerender } = bindHook({ count: 1, peer: "peer1" });
    act(() => el.userScrollTo(0));
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

  it("unpin releases the follow and mirrors into atBottom", () => {
    // The wiring the virtualized surfaces depend on: useLocateMessage's
    // onLocate calls unpin(), and the screen feeds the reactive atBottom into
    // the list's followBottom prop. Both halves of that chain must move on an
    // explicit release — a downward programmatic scroll carries no event
    // signal the direction guard may read.
    const { el, result, rerender } = bindHook({ count: 1, peer: "peer1" });
    act(() => rerender({ count: 2, peer: "peer1" })); // establishes the view key
    act(() => result.current.unpin());
    expect(result.current.isPinned()).toBe(false);
    expect(result.current.atBottom).toBe(false);
    el.grow(400);
    act(() => rerender({ count: 3, peer: "peer1" }));
    expect(el.scrollTop).toBe(700); // the release stands: nothing re-pinned
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

    act(() => el.userScrollTo(0));
    act(() => el.userScrollTo(el.bottom));
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

/* ---------------- the startup path through a real React render ---------------- */

/*
 * `renderHook` never re-binds the container, so it cannot express the case that
 * actually broke startup: a screen merging the hook ref with its own capture in
 * a callback of a NEW identity per render, which makes React detach the ref with
 * `null` and re-attach the same node. This drives a real component instead; the
 * geometry is faked on the prototype because jsdom has no layout engine.
 */
describe("useStickToBottom through a re-created callback ref", () => {
  /** A chat-surface stand-in: inline merged callback ref, one content dep. */
  function FakePanel({ n }: { n: number }) {
    const { ref, atBottom } = useStickToBottom([n], "sess");
    return createElement(
      "div",
      {
        ref: (el: HTMLDivElement | null) => {
          ref(el);
        },
        "data-testid": "vp",
      },
      String(atBottom),
    );
  }

  /** Report every element as a 300px viewport over `content()` px of rows. */
  function fakeLayout(content: () => number): { top: () => number; restore: () => void } {
    let top = 0;
    Object.defineProperty(HTMLElement.prototype, "scrollHeight", {
      configurable: true,
      get: content,
    });
    Object.defineProperty(HTMLElement.prototype, "clientHeight", {
      configurable: true,
      get: () => 300,
    });
    Object.defineProperty(HTMLElement.prototype, "scrollTop", {
      configurable: true,
      get: () => top,
      set(this: HTMLElement, v: number) {
        const clamped = Math.max(0, Math.min(v, content() - 300));
        if (clamped === top) return;
        top = clamped;
        // Like a browser, ANY offset change dispatches a scroll event —
        // including the controller's programmatic bottom-pin write (async in
        // a browser, synchronous in this harness). Without it the follow
        // rule's direction baseline (`lastTop`) never sees the pin land and a
        // reader's scroll-up reads as "no movement".
        this.dispatchEvent(new Event("scroll"));
      },
    });
    const proto = HTMLElement.prototype as unknown as Record<string, unknown>;
    return {
      top: () => top,
      // Only our shadows are configurable, so deleting them returns the
      // element to jsdom's own (layout-free) definitions.
      restore: () => {
        delete proto.scrollHeight;
        delete proto.clientHeight;
        delete proto.scrollTop;
      },
    };
  }

  it("follows the newest page that lands after the container mounted", () => {
    let content = 100; // content shorter than the viewport: an empty conversation
    const layout = fakeLayout(() => content);
    try {
      const view = render(createElement(FakePanel, { n: 0 }));
      expect(layout.top()).toBe(0); // nothing to follow yet

      content = 2000; // the tail page arrives, opening a scroll range of 1700
      act(() => view.rerender(createElement(FakePanel, { n: 50 })));

      expect(layout.top()).toBe(1700); // the NEWEST row, not the oldest of the page
      expect(view.getByTestId("vp").textContent).toBe("true"); // so no back-to-bottom arrow
    } finally {
      layout.restore();
    }
  });

  it("still refuses to move a reader who scrolled up once the next page lands", () => {
    // The other half of the rule: the fix must not have become a scroll hijack.
    let content = 2000;
    const layout = fakeLayout(() => content);
    try {
      const view = render(createElement(FakePanel, { n: 50 }));
      act(() => view.rerender(createElement(FakePanel, { n: 50 })));
      const vp = view.getByTestId("vp") as HTMLDivElement;
      // The reader scrolls up off the tail (a scroll is an event, not a render,
      // and the follow flag it flips is React state — hence the act wrapper).
      act(() => {
        vp.scrollTop = 0;
        vp.dispatchEvent(new Event("scroll"));
      });

      content = 3000; // a doorbell wake brings newer rows
      act(() => view.rerender(createElement(FakePanel, { n: 80 })));

      expect(layout.top()).toBe(0); // their position stands
      expect(view.getByTestId("vp").textContent).toBe("false"); // arrow offered
    } finally {
      layout.restore();
    }
  });
});
