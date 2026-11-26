// @vitest-environment jsdom
/**
 * VirtualRowList — the generic virtualization shell (render-perf L2).
 *
 * jsdom has no layout, but @tanstack/virtual-core measures everything through
 * `offsetWidth`/`offsetHeight` (verified against the installed virtual-core:
 * `getRect` and `measureElement` both read the offset properties, and
 * `observeElementRect` fires the initial rect synchronously on subscribe).
 * Geometry is therefore faked deterministically: the prototype getter below
 * reads a `data-oh` attribute — container 400px, rows 40px — and the
 * virtualizer behaves exactly as it does in the browser.
 *
 * The pins here are the shell's contract: viewport-only mounting, the
 * imperative handle (scrollToIndex/Key, first/last-visible queries,
 * isKeyInViewport) and its honest no-op reporting. The measured-height cache
 * is the same discipline `blockHeightEstimator.ts` already proved for exec
 * (it only shows on rows that are NOT mounted, which jsdom cannot isolate);
 * it is covered by the exec regression suite once VirtualMessageList runs on
 * the shell, plus the manual pass in the plan's L2 acceptance.
 */
import { act, render } from "@testing-library/react";
import { createRef } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { VirtualRowList, type VirtualRowListHandle } from "./VirtualRowList";

// Deterministic geometry: offsetHeight from data-oh; elements without the
// attribute measure 0 (the shell's own record() guard ignores 0 heights).
Object.defineProperty(HTMLElement.prototype, "offsetHeight", {
  configurable: true,
  get(this: HTMLElement) {
    const v = this.dataset.oh;
    return v === undefined ? 0 : Number(v);
  },
});

// Viewport boxes for the row-inside-item anchor (../common/scrollAnchor): top
// from data-rt, height from data-oh. jsdom reports a zero rect for everything,
// and the anchor reads ONE quantity — `row.top - item.top`, the row's offset
// inside its item — so the fake only has to be consistent with itself, not
// with the pane's scroll position.
Object.defineProperty(Element.prototype, "getBoundingClientRect", {
  configurable: true,
  value(this: HTMLElement) {
    const top = this.dataset?.rt === undefined ? 0 : Number(this.dataset.rt);
    const height = this.dataset?.oh === undefined ? 0 : Number(this.dataset.oh);
    return {
      top,
      bottom: top + height,
      height,
      left: 0,
      right: 0,
      width: 0,
      x: 0,
      y: top,
      toJSON: () => ({}),
    } as DOMRect;
  },
});

interface Item {
  id: string;
}

function items(n: number, prefix: string): Item[] {
  return Array.from({ length: n }, (_, i) => ({ id: `${prefix}-${i}` }));
}

function setup(
  list: Item[],
  opts: {
    rowHeight?: number;
    estimateSize?: (item: Item, index: number, width: number) => number;
    followBottom?: boolean;
    /** Per-row wrapper attributes (height, and any anchor the test paints). */
    rowAttrs?: (item: Item, index: number) => Record<string, string>;
    /** Row content — a block-shaped item paints rows INSIDE itself. */
    renderItem?: (item: Item, index: number) => React.ReactNode;
  } = {},
) {
  const container = document.createElement("div");
  container.dataset.oh = "400";
  Object.defineProperty(container, "clientWidth", { configurable: true, get: () => 800 });
  // jsdom has no layout: scrollTop/scrollHeight/clientHeight are inert. Fake
  // them — scrollTop is a plain writable slot (the shell's WKWebView
  // scrollToFn assigns it synchronously), scrollHeight tracks the shell's
  // total-size div, clientHeight is the 400px viewport.
  let st = 0;
  Object.defineProperty(container, "scrollTop", {
    configurable: true,
    get: () => st,
    set: (v: number) => {
      st = v;
    },
  });
  Object.defineProperty(container, "clientHeight", { configurable: true, get: () => 400 });
  Object.defineProperty(container, "scrollHeight", {
    configurable: true,
    get: () => parseFloat((container.firstElementChild as HTMLElement)?.style.height || "0") || 0,
  });
  document.body.appendChild(container);

  const handle = createRef<VirtualRowListHandle>();
  const rowH = opts.rowHeight ?? 40;

  // The follow policy is mutable state of the harness: `commit` re-renders
  // with the CURRENT policy unless a test explicitly changes it (dropping the
  // prop on every commit would silently release the follow).
  let follow = opts.followBottom;
  const renderList = (l: Item[]) => (
    <VirtualRowList
      ref={handle}
      items={l}
      keyOf={(it) => it.id}
      scrollContainerRef={{ current: container }}
      followBottom={follow}
      estimateSize={opts.estimateSize}
      rowAttrs={
        opts.rowAttrs ?? (() => ({ "data-oh": String(rowH), "data-testid": "row" }))
      }
      renderItem={opts.renderItem ?? (() => <span>row</span>)}
    />
  );

  const view = render(renderList(list), { container });

  /** Commit new content (and optionally a new follow policy). */
  const commit = (l: Item[], nextFollow?: boolean) => {
    follow = nextFollow === undefined ? follow : nextFollow;
    act(() => {
      view.rerender(renderList(l));
    });
  };

  /** Let the virtualizer's throttled notifications (real setTimeout in
   *  virtual-core) land: measurement-driven commits converge the total size,
   *  exactly like the browser. Follow assertions must run after settling.
   *  Two rounds so a notify scheduled by the first round's commit is consumed
   *  inside act too (otherwise it leaks past teardown as an unhandled error). */
  const settle = () =>
    act(async () => {
      await new Promise((r) => setTimeout(r, 10));
      await new Promise((r) => setTimeout(r, 10));
    });

  const scroll = (top: number) => {
    act(() => {
      container.scrollTop = top;
      container.dispatchEvent(new Event("scroll"));
    });
  };

  const mountedIndexes = () =>
    Array.from(container.querySelectorAll<HTMLElement>("[data-index]")).map((el) => Number(el.dataset.index));

  return { handle, container, scroll, mountedIndexes, commit, settle };
}

let uid = 0;
const nextPrefix = () => `s${(uid += 1)}`; // session-qualify keys: the height cache is module-level

describe("VirtualRowList shell", () => {
  beforeEach(() => {
    document.body.innerHTML = "";
  });

  it("mounts only viewport rows + overscan for a 500-item list", () => {
    const s = setup(items(500, nextPrefix()));
    const mounted = s.mountedIndexes();
    // 400px / 40px = 10 visible rows; overscan 5 pads each side. The plan's
    // acceptance line: DOM rows ≤ viewport rows + overscan (both sides).
    expect(mounted.length).toBeGreaterThan(0);
    expect(mounted.length).toBeLessThanOrEqual(10 + 5 + 5 + 2);
    expect(mounted[0]).toBe(0);
  });

  it("handle: scrollToIndex moves the mounted window and queries report it", () => {
    const prefix = nextPrefix();
    const s = setup(items(500, prefix));

    s.handle.current!.scrollToIndex(100);
    // The WKWebView-safe scrollToFn assigns scrollTop synchronously, but the
    // virtualizer only learns the new offset from the scroll EVENT — dispatch
    // it exactly as a browser would.
    s.scroll(s.container.scrollTop);

    expect(s.mountedIndexes()).toContain(100);
    const first = s.handle.current!.getFirstVisibleIndex();
    const last = s.handle.current!.getLastVisibleIndex();
    expect(first).not.toBeNull();
    expect(last).not.toBeNull();
    expect(first! <= 100 && 100 <= last!).toBe(true);
    expect(s.handle.current!.getFirstVisibleKey()).toBe(`${prefix}-${first}`);
    expect(s.handle.current!.isKeyInViewport(`${prefix}-100`)).toBe(true);
    expect(s.handle.current!.isKeyInViewport(`${prefix}-499`)).toBe(false);
  });

  it("scrollToKey finds a row by key and returns false for an absent key", () => {
    const prefix = nextPrefix();
    const s = setup(items(500, prefix));
    expect(s.handle.current!.scrollToKey(`${prefix}-250`)).toBe(true);
    s.scroll(s.container.scrollTop);
    expect(s.mountedIndexes()).toContain(250);
    expect(s.handle.current!.scrollToKey(`${prefix}-99999`)).toBe(false);
  });

  it("scrollToKey centres a ROW inside a tall item, not the item (history jump)", () => {
    // The transcript folds a whole execution session's rounds into ONE item
    // (ADR-106 D3/D4), anchored at its oldest round. Item 5 stands for such a
    // block: 1200px tall, holding round `b7` 900px down inside it. Centring the
    // ITEM put the reader on whatever round sat at 600px — a different message
    // from the one they clicked in the history dialog.
    const prefix = nextPrefix();
    const s = setup(items(40, prefix), {
      rowAttrs: (it) =>
        it.id === `${prefix}-5`
          ? { "data-oh": "1200", "data-testid": "row" }
          : { "data-oh": "40", "data-testid": "row" },
      renderItem: (it) =>
        it.id === `${prefix}-5` ? (
          <div data-seq="b7" data-rt="900" data-oh="60" />
        ) : (
          <span>row</span>
        ),
    });

    // Coarse placement: the item's top edge at the pane's top. Rows are 40px
    // plus the shell's 4px gap (5 × 44).
    expect(s.handle.current!.scrollToKey(`${prefix}-5`, "start")).toBe(true);
    const itemAligned = s.container.scrollTop;
    expect(itemAligned).toBe(220);

    // With the row named, the pane lands on it: 900px further down, less the
    // half-viewport lead that centres a 60px row in the 400px pane.
    expect(s.handle.current!.scrollToKey(`${prefix}-5`, "center", '[data-seq="b7"]')).toBe(true);
    expect(s.container.scrollTop).toBe(itemAligned + 900 - (400 - 60) / 2);
  });

  it("scrollToKey anchors a row that is its own item (standalone message)", () => {
    // A plain message row IS its item and carries `data-seq` on the wrapper:
    // the anchor must reduce to the old item centring (its offset inside itself
    // is zero), not answer false because `querySelector` cannot match the
    // element it starts from.
    const prefix = nextPrefix();
    const s = setup(items(40, prefix), {
      rowAttrs: (it) =>
        it.id === `${prefix}-5`
          ? { "data-oh": "120", "data-seq": "u9", "data-rt": "900" }
          : { "data-oh": "40" },
    });

    expect(s.handle.current!.scrollToKey(`${prefix}-5`, "start")).toBe(true);
    const itemAligned = s.container.scrollTop;
    expect(itemAligned).toBe(220);
    expect(s.handle.current!.scrollToKey(`${prefix}-5`, "center", '[data-seq="u9"]')).toBe(true);
    // inItem = 0 (the item's own rect is the row's), lead = (400-120)/2 = 140.
    expect(s.container.scrollTop).toBe(itemAligned - 140);
  });

  it("scrollToBottom is honest: false on zero items, true once positioned", () => {
    const s = setup([], {});
    expect(s.handle.current!.scrollToBottom()).toBe(false);

    const s2 = setup(items(30, nextPrefix()), {});
    expect(s2.handle.current!.scrollToBottom()).toBe(true);
    s2.scroll(s2.container.scrollTop);
    expect(s2.mountedIndexes()).toContain(29);
  });

  it("scrollToBottom arms through the late-ref window and lands when the container appears", () => {
    // The session-switch shape: the surface's follow refresh runs in its
    // layout effect while the virtualizer has not discovered its scroll
    // element yet (React attaches the parent's ref after this child's
    // effects). A dropped request left the transcript at the top or stuck on
    // the estimate; an armed one is honoured on the attach commit and rides
    // measurement convergence — no caller-side retry loop.
    const container = document.createElement("div");
    container.dataset.oh = "400";
    Object.defineProperty(container, "clientWidth", { configurable: true, get: () => 800 });
    let st = 0;
    Object.defineProperty(container, "scrollTop", {
      configurable: true,
      get: () => st,
      set: (v: number) => {
        st = v;
      },
    });
    Object.defineProperty(container, "clientHeight", { configurable: true, get: () => 400 });
    Object.defineProperty(container, "scrollHeight", {
      configurable: true,
      get: () =>
        parseFloat((container.firstElementChild as HTMLElement)?.style.height || "0") || 0,
    });
    document.body.appendChild(container);

    const lateRef = { current: null as HTMLDivElement | null };
    const handle = createRef<VirtualRowListHandle>();
    const list = items(30, nextPrefix());
    const tree = () => (
      <VirtualRowList
        ref={handle}
        items={list}
        keyOf={(it) => it.id}
        scrollContainerRef={lateRef}
        rowAttrs={() => ({ "data-oh": "40", "data-testid": "row" })}
        renderItem={() => <span>row</span>}
      />
    );
    const { rerender } = render(tree(), { container });

    // Not attached yet: the request is accepted (armed), nothing positioned.
    expect(handle.current!.scrollToBottom()).toBe(true);
    expect(container.scrollTop).toBe(0);

    // The container appears: the next commit must honour the armed anchor.
    // (A fresh element per render — rerendering the SAME element reference
    // lets React bail out of the subtree and the effects never run.)
    lateRef.current = container;
    act(() => rerender(tree()));
    const max = container.scrollHeight - container.clientHeight;
    expect(max).toBeGreaterThan(0);
    expect(container.scrollTop).toBe(max);
  });

  it("adapter estimateSize is consulted for rows without a measurement", () => {
    const seen: number[] = [];
    const s = setup(items(50, nextPrefix()), {
      estimateSize: (_it, i) => {
        seen.push(i);
        return 100;
      },
    });
    // The virtualizer sizes the whole list through estimateSize on first
    // pass — the prop is wired, and total size is non-zero.
    expect(seen.length).toBeGreaterThan(0);
    const total = parseFloat((s.container.firstElementChild as HTMLElement).style.height);
    expect(total).toBeGreaterThan(0);
    void s;
  });
});

/* ---------------- persistent follow (followBottom) ---------------- */

/*
 * The agent-switch drift: a cold transcript keeps growing AFTER the one-shot
 * bottom-pin's deadline (async measurements settling, code highlighting,
 * doorbell window replacement), and a policy that only re-positions on a deps
 * change never moves again — the reader is left mid-pane. While the surface's
 * policy says the reader follows the tail, the shell rides EVERY commit to the
 * bottom. A genuine user scroll-up is silenced locally (the policy's own
 * release arrives one render later; the gap must not be spent fighting the
 * reader), and an explicit jump re-syncs the follow from the policy.
 */
describe("VirtualRowList followBottom", () => {
  const maxTop = (container: HTMLElement) => container.scrollHeight - container.clientHeight;

  it("rides every commit to the bottom while the policy follows — past any deadline", async () => {
    // Only performance.now is faked: the virtualizer's own scheduling stays real.
    vi.useFakeTimers({ toFake: ["performance"] });
    try {
      const prefix = nextPrefix();
      const s = setup(items(30, prefix), { followBottom: true });
      await s.settle();
      expect(maxTop(s.container)).toBeGreaterThan(0);
      expect(s.container.scrollTop).toBe(maxTop(s.container)); // the mount commit rides

      // Late growth WELL past the armed one-shot's 2s window: measurements
      // that settle after the deadline (the cold-cache agent switch) must keep
      // the tail-anchored reader at the bottom.
      vi.advanceTimersByTime(3000);
      s.commit(items(60, prefix));
      await s.settle();
      expect(s.container.scrollTop).toBe(maxTop(s.container));

      s.commit(items(90, prefix));
      await s.settle();
      expect(s.container.scrollTop).toBe(maxTop(s.container));
    } finally {
      vi.useRealTimers();
    }
  });

  it("a user scroll-up silences the follow locally even while the prop stays true", async () => {
    const prefix = nextPrefix();
    const s = setup(items(30, prefix), { followBottom: true });
    await s.settle();
    expect(s.container.scrollTop).toBe(maxTop(s.container));

    // The reader leaves. The controller's release lands one render later;
    // the shell must not re-pin underneath them in the meantime.
    s.scroll(0);
    await s.settle();
    expect(s.container.scrollTop).toBe(0); // the silence commit did not re-pin

    s.commit(items(60, prefix));
    await s.settle();
    expect(s.container.scrollTop).toBe(0); // late growth does not fight them
    s.commit(items(90, prefix));
    await s.settle();
    expect(s.container.scrollTop).toBe(0);
  });

  it("an explicit scrollToBottom re-syncs the follow from the policy", async () => {
    const prefix = nextPrefix();
    const s = setup(items(30, prefix), { followBottom: true });
    await s.settle();
    s.scroll(0); // locally silenced
    s.commit(items(60, prefix));
    await s.settle();
    expect(s.container.scrollTop).toBe(0);

    // The back-to-bottom affordance (or the delegate's jump): an explicit
    // jump re-arms AND re-syncs the persistent follow from the prop.
    act(() => {
      expect(s.handle.current!.scrollToBottom()).toBe(true);
    });
    await s.settle();
    expect(s.container.scrollTop).toBe(maxTop(s.container));

    vi.useFakeTimers({ toFake: ["performance"] });
    try {
      vi.advanceTimersByTime(3000); // past the one-shot's deadline
      s.commit(items(90, prefix));
      await s.settle();
      expect(s.container.scrollTop).toBe(maxTop(s.container)); // follow, not arm
    } finally {
      vi.useRealTimers();
    }
  });

  it("a hidden pane (viewport 0) renders no rows, not the whole transcript", async () => {
    const prefix = nextPrefix();
    const s = setup(items(500, prefix));
    await s.settle();
    expect(s.container.querySelectorAll("[data-testid=row]").length).toBeGreaterThan(0);
    // Collapse the viewport the way display:none does when the editor tab
    // takes the middle pane: the windowed branch must yield, and the old
    // all-rows fallback must NOT mount 500 bubbles into an invisible pane.
    Object.defineProperty(s.container, "clientHeight", { configurable: true, get: () => 0 });
    Object.defineProperty(s.container, "offsetHeight", { configurable: true, get: () => 0 });
    s.commit(items(500, prefix));
    await s.settle();
    expect(s.container.querySelectorAll("[data-testid=row]").length).toBe(0);
  });

  it("a clamped scroll with an up-to-date offset does not force scroll events", async () => {
    const prefix = nextPrefix();
    const s = setup(items(1, prefix)); // total size < viewport: every offset clamps
    await s.settle();
    const written: number[] = [];
    let st = 0;
    Object.defineProperty(s.container, "scrollTop", {
      configurable: true,
      get: () => st,
      set: (v: number) => {
        // Real browsers clamp; the jsdom slot does not, and an unclamped
        // negative would hide the clamped-toggle path this test pins.
        const max = Math.max(0, s.container.scrollHeight - s.container.clientHeight);
        st = Math.max(0, Math.min(v, max));
        written.push(st);
      },
    });
    act(() => {
      s.handle.current?.scrollToBottom();
      s.handle.current?.scrollToBottom();
    });
    // Every write lands on the clamped target (0). The old unconditional
    // +1/target toggle wrote a transient 1 to force a scroll event; each
    // forced event committed a re-render that scrolled again — the
    // hidden-pane commit ping-pong. No transient, no loop.
    expect(written.length).toBeGreaterThan(0);
    expect(written.every((v) => v === 0)).toBe(true);
  });

  it("without the prop the shell does NOT ride late growth (one-shot only)", async () => {
    const prefix = nextPrefix();
    const s = setup(items(30, prefix)); // no follow policy wired
    await s.settle();
    expect(maxTop(s.container)).toBeGreaterThan(0);
    expect(s.container.scrollTop).toBe(0); // nothing armed, nothing pinned
    s.commit(items(60, prefix));
    await s.settle();
    expect(s.container.scrollTop).toBe(0); // the pre-fix drift behaviour, contained
  });
});
