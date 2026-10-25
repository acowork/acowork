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
import { beforeEach, describe, expect, it } from "vitest";
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

interface Item {
  id: string;
}

function items(n: number, prefix: string): Item[] {
  return Array.from({ length: n }, (_, i) => ({ id: `${prefix}-${i}` }));
}

function setup(
  list: Item[],
  opts: { rowHeight?: number; estimateSize?: (item: Item, index: number, width: number) => number } = {},
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

  render(
    <VirtualRowList
      ref={handle}
      items={list}
      keyOf={(it) => it.id}
      scrollContainerRef={{ current: container }}
      estimateSize={opts.estimateSize}
      rowAttrs={() => ({ "data-oh": String(rowH), "data-testid": "row" })}
      renderItem={() => <span>row</span>}
    />,
    { container },
  );

  const scroll = (top: number) => {
    act(() => {
      container.scrollTop = top;
      container.dispatchEvent(new Event("scroll"));
    });
  };

  const mountedIndexes = () =>
    Array.from(container.querySelectorAll<HTMLElement>("[data-index]")).map((el) => Number(el.dataset.index));

  return { handle, container, scroll, mountedIndexes };
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

  it("scrollToBottom is honest: false on zero items, true once positioned", () => {
    const s = setup([], {});
    expect(s.handle.current!.scrollToBottom()).toBe(false);

    const s2 = setup(items(30, nextPrefix()), {});
    expect(s2.handle.current!.scrollToBottom()).toBe(true);
    s2.scroll(s2.container.scrollTop);
    expect(s2.mountedIndexes()).toContain(29);
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
