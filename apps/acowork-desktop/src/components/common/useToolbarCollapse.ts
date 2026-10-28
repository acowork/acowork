import { useCallback, useRef, useState } from "react";

/**
 * Shared responsive-collapse engine for composer toolbars
 * (execution-session ChatPanel, chat-session AgentChatView).
 *
 * The bottom toolbar row (model / reasoning / workspace / skills buttons +
 * an icon-only right cluster) must keep all buttons non-overlapping when
 * the panel width narrows. We measure each button's full width vs. its
 * icon-only width and greedily fold labels — starting from the leftmost
 * button and moving rightward — until the row fits.
 *
 * DOM contract (provided by `ToolbarDropdownTrigger` and the host toolbar):
 *  - the toolbar container is the element the returned ref is attached to;
 *  - each foldable button wrapper carries `data-toolbar-btn="<id>"`;
 *  - inside it, the label span carries `data-toolbar-text` and the chevron
 *    carries `data-toolbar-chevron`;
 *  - the icon-only right cluster carries `data-toolbar-cluster="right"`.
 *
 * Returns `{ setToolbarRef, textHidden }`:
 *  - `setToolbarRef` — attach to the toolbar div as a ref callback;
 *  - `textHidden` — per-button fold state (`textHidden.model`, …), meant to
 *    be passed into each button as its `textHidden` prop. The engine also
 *    toggles the spans' `display` imperatively on the DOM so the correct
 *    layout lands in the same frame the measurement runs; the React state
 *    keeps the components in sync across re-renders.
 *
 * IMPLEMENTATION: a `ref` callback (NOT a useEffect on a ref object).
 *
 * Why ref callback instead of useEffect?
 *
 * The model / reasoning buttons are conditionally rendered (agent alive,
 * models configured, reasoning-capable model selected...). On cold start
 * the toolbar div itself may render before its conditionally-rendered
 * children, or vice versa, depending on the order of state hydration.
 * useEffect with `[]` runs once on mount and then never again — so if it
 * runs BEFORE the model/effort buttons exist, ResizeObserver is attached
 * to a half-formed DOM and the observer's initial synchronous callback
 * already fired with the half-formed state and cached "no buttons → no
 * fold" forever.
 *
 * A ref callback fires every time React attaches the ref to a DOM node —
 * and crucially, when the toolbar div re-mounts due to React reconciler
 * activity (StrictMode double-invoke, or any parent state that causes the
 * panel's children to re-render and remount the toolbar subtree), the OLD
 * ref is called with `null` (cleanup) and the NEW ref is called with the
 * new node (setup). This naturally tears down and re-installs the
 * observers across all "toolbar identity" changes.
 *
 * Inside the ref callback we ALSO install a MutationObserver on the
 * toolbar subtree to catch button-set changes that don't cause the
 * toolbar itself to remount (agent becomes running → model menu appears
 * inside the toolbar).
 */
export function useToolbarCollapse(): {
  setToolbarRef: (node: HTMLDivElement | null) => void;
  textHidden: Record<string, boolean>;
} {
  const toolbarRef = useRef<HTMLDivElement | null>(null);
  const [textHidden, setTextHidden] = useState<Record<string, boolean>>({});

  const setToolbarRef = useCallback((node: HTMLDivElement | null) => {
    // Tear down any previous install before handing off — handles
    // re-mounts cleanly (the null call on unmount and the node call on
    // the new mount both flow through here).
    const prev = toolbarRef.current as unknown as
      { __toolbarTeardown__?: () => void } | null;
    prev?.__toolbarTeardown__?.();
    toolbarRef.current = node;
    if (!node) return;

    // ── Imperative, fully self-contained measurement engine ──────
    //
    // This closure captures nothing from React component state. It
    // reads/writes the DOM directly. State sync to React happens
    // through setTextHidden only at the end of a measurement, so even
    // if React is in the middle of a render, the DOM stays visually
    // consistent (we apply visibility BEFORE setting state).
    let measuring = false;
    let rafId = 0;

    const measure = () => {
      if (measuring) return;
      measuring = true;
      try {
        const els = Array.from(
          node.querySelectorAll<HTMLElement>("[data-toolbar-btn]"),
        );
        if (els.length === 0) return;

        const spans = els.map((el) => ({
          id: el.dataset.toolbarBtn as string,
          text: el.querySelector<HTMLElement>("[data-toolbar-text]"),
          chev: el.querySelector<HTMLElement>("[data-toolbar-chevron]"),
          // icon-only width (text hidden) — measured first so it's stable.
          iconWidth: el.offsetWidth,
        }));

        // Reveal all text/chevron to read true full widths.
        spans.forEach(({ text, chev }) => {
          if (text) text.style.display = "";
          if (chev) chev.style.display = "";
        });
        const fullWidths = els.map((el) => el.offsetWidth);

        const GAP = 4;
        const totalGaps = (els.length - 1) * GAP;

        // Layout constants: container padding (px-3 = 12px each side),
        // gap-2 between the left cluster and right cluster.
        // The right cluster's width is MEASURED, not counted: it is a
        // row of icon-only buttons whose own width is whatever the DOM
        // gives it, so reading `offsetWidth` covers "added a new
        // button", "button label changes", "icon-only stays icon-only
        // under all conditions" without anyone editing a number here.
        const PAD_X = 24;
        const FLEX_GAP = 8;
        const rightCluster = node.querySelector<HTMLElement>(
          '[data-toolbar-cluster="right"]',
        );
        // ponytail: ceiling — if the right cluster is missing (early
        // mount race), fall back to 0 so the fold still has a valid
        // available width. The next ResizeObserver / MutationObserver
        // tick re-measures. Upgrade: hoist this into a ResizeObserver
        // on the right cluster itself so it never races.
        const RIGHT_CLUSTER_W = rightCluster?.offsetWidth ?? 0;
        const available = node.offsetWidth - PAD_X - FLEX_GAP - RIGHT_CLUSTER_W;

        // Progressive collapse from LEFT to RIGHT (matches the
        // documented product behavior: model hides first, then effort,
        // then ws, then sk — preserving the rightmost buttons, which
        // are usually the most context-relevant).
        //
        //   1. Start with all buttons fully visible.
        //   2. If totalFull > available, fold the leftmost button.
        //   3. Recheck; if still overflowing, fold the next leftmost.
        //   4. Stop when it fits OR every button is folded.
        //
        // Folding a button changes total width by exactly
        // (fullWidth - iconWidth) — the button stays in the row, its
        // trailing gap-1 to the next button is unchanged.
        const fold = foldLeftToRight(fullWidths, spans.map((s) => s.iconWidth), available, totalGaps);

        // Apply visibility to DOM synchronously (user sees correct
        // layout this frame), THEN sync React state.
        spans.forEach(({ text, chev }, i) => {
          const display = fold[i] ? "none" : "";
          if (text) text.style.display = display;
          if (chev) chev.style.display = display;
        });

        setTextHidden((prev) => {
          let changed = false;
          const next = { ...prev };
          spans.forEach(({ id }, i) => {
            if (next[id] !== fold[i]) {
              next[id] = fold[i];
              changed = true;
            }
          });
          return changed ? next : prev;
        });
      } finally {
        measuring = false;
      }
    };

    const scheduleMeasure = () => {
      if (rafId !== 0) return;
      rafId = requestAnimationFrame(() => {
        rafId = 0;
        measure();
      });
    };

    // ResizeObserver: container width changes.
    const ro = new ResizeObserver(() => scheduleMeasure());
    ro.observe(node);

    // MutationObserver: button-set changes. `attributes: true` covers
    // WorkspaceSelector's text change (it updates the label when the
    // user picks a different workspace) which DOES affect width.
    const mo = new MutationObserver((mutations) => {
      const relevant = mutations.some(
        (m) =>
          m.type === "childList" ||
          m.type === "characterData" ||
          (m.type === "attributes" && m.attributeName === "data-toolbar-text"),
      );
      if (relevant) scheduleMeasure();
    });
    mo.observe(node, {
      childList: true,
      subtree: true,
      characterData: true,
      attributes: true,
      attributeFilter: ["data-toolbar-text"],
    });

    // Schedule the initial measurement for the next frame so React has
    // time to commit children (especially the conditionally-rendered
    // model/effort buttons).
    scheduleMeasure();

    // Stash teardown on the node so the next ref callback can clean up.
    // Without this, a re-mount would leak observers.
    (node as unknown as { __toolbarTeardown__?: () => void }).__toolbarTeardown__ = () => {
      ro.disconnect();
      mo.disconnect();
      if (rafId !== 0) {
        cancelAnimationFrame(rafId);
        rafId = 0;
      }
    };
  }, []);

  return { setToolbarRef, textHidden };
}

/**
 * The greedy fold math, extracted pure so it can be unit-tested without a
 * DOM. Returns, per button (left → right), whether its label folds.
 */
export function foldLeftToRight(
  fullWidths: number[],
  iconWidths: number[],
  available: number,
  totalGaps: number,
): boolean[] {
  const fold = new Array<boolean>(fullWidths.length).fill(false);
  let current = fullWidths.reduce((a, b) => a + b, 0) + totalGaps;
  for (let i = 0; i < fullWidths.length; i++) {
    if (current <= available) break;
    fold[i] = true;
    current -= fullWidths[i] - iconWidths[i];
  }
  return fold;
}
