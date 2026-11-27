import { useCallback, useRef, useState } from "react";

/**
 * Shared responsive-collapse engine for composer toolbars
 * (execution-session ChatPanel, chat-session AgentChatView).
 *
 * The bottom toolbar row (model / reasoning / workspace / skills buttons +
 * an icon-only right cluster) must keep all buttons non-overlapping when
 * the panel width narrows. Folding runs left → right: while the row is too
 * narrow for every label, drop the leftmost remaining label back to its icon,
 * measure again, and repeat — a folded button shows its icon only, no chevron.
 *
 * DOM contract (provided by `ToolbarDropdownTrigger` and the host toolbar):
 *  - the toolbar container is the element the returned ref is attached to;
 *  - each foldable button wrapper carries `data-toolbar-btn="<id>"`;
 *  - inside it, the label span carries `data-toolbar-text` and the chevron
 *    carries `data-toolbar-chevron`;
 *  - the foldable cluster carries `data-toolbar-cluster="left"` and the
 *    icon-only cluster `data-toolbar-cluster="right"` (measured, not counted,
 *    so adding a button needs no change here).
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
        }));

        const setText = (i: number, display: "" | "none") => {
          const { text, chev } = spans[i];
          if (text) text.style.display = display;
          if (chev) chev.style.display = display;
        };

        const leftCluster = node.querySelector<HTMLElement>(
          '[data-toolbar-cluster="left"]',
        );
        // No left cluster, no measurable row: bail and let the observers
        // schedule another pass once the toolbar is actually mounted.
        if (!leftCluster) return;

        // The right cluster's width is MEASURED, not counted: it is a
        // row of icon-only buttons whose own width is whatever the DOM
        // gives it, so reading `offsetWidth` covers "added a new
        // button", "button label changes", "icon-only stays icon-only
        // under all conditions" without anyone editing a number here.
        //
        // The row's own padding and gap are measured for the same reason.
        // They used to be the constants 24 / 8, matching `px-3 … gap-2`; the
        // composer strip then changed to `pl-[38px]` (hanging its buttons off
        // the `+` circle) and the fold went on believing it had 26px more than
        // it did — which does not fold late, it truncates: a row that thinks
        // it fits never collapses a label.
        const rowStyle = getComputedStyle(node);
        const PAD_X =
          (parseFloat(rowStyle.paddingLeft) || 0) +
          (parseFloat(rowStyle.paddingRight) || 0);
        const FLEX_GAP = parseFloat(rowStyle.columnGap) || 8;
        const rightCluster = node.querySelector<HTMLElement>(
          '[data-toolbar-cluster="right"]',
        );
        // ponytail: ceiling — if the right cluster is missing (early
        // mount race), fall back to 0 so the fold still has a valid
        // available width. The next ResizeObserver / MutationObserver
        // tick re-measures. Upgrade: hoist this into a ResizeObserver
        // on the right cluster itself so it never races.
        const RIGHT_CLUSTER_W = rightCluster?.offsetWidth ?? 0;
        // `offsetWidth` is integer-rounded on both sides, so a row measured
        // "exactly fitting" can still be a pixel or two over. The slack
        // makes the fold fire slightly BEFORE the squeeze would start, which
        // is where the labels go to zero and the chevron is left stranded.
        const SLACK = 4;
        const available =
          node.offsetWidth - PAD_X - FLEX_GAP - RIGHT_CLUSTER_W - SLACK;

        // Progressive collapse from LEFT to RIGHT (the product rule: model
        // hides first, then effort, then ws, then sk — the rightmost, usually
        // most context-relevant buttons survive longest).
        //
        // Each step is a REAL measurement, not a prediction: fold the
        // leftmost button, read the row again, fold the next one if it still
        // does not fit, and stop when it fits or every button is folded.
        //
        // The read has to happen in a layout that CANNOT squeeze. The trigger
        // chain is shrinkable by design (`min-w-0 truncate`, see
        // ToolbarDropdown.shrink.test.tsx), so an over-full row truncates the
        // labels — `Me…`, `ACowo…` — instead of overflowing, and every button
        // then reports its truncated width: the fold never fires and the row
        // sits there with half a label and a stranded chevron. `min-width:
        // max-content` on the left cluster forces its labels to their whole
        // width, so a deficit shows up as an overflowing row that the fold can
        // actually see. Restored before this frame paints — never visible.
        spans.forEach((_, i) => setText(i, ""));
        leftCluster.style.minWidth = "max-content";

        const fold = spans.map(() => false);
        for (let i = 0; i < els.length; i++) {
          if (leftCluster.offsetWidth <= available) break;
          fold[i] = true;
          setText(i, "none");
        }

        leftCluster.style.minWidth = "";

        // Apply visibility to DOM synchronously (user sees correct
        // layout this frame), THEN sync React state.
        spans.forEach((_, i) => setText(i, fold[i] ? "none" : ""));

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
