// Right-side hover drawer — React binding for the pure state machine in
// `rightPanelDrawerState.ts`.
//
// The hook owns the clock. It mirrors an open agent-panel peek into
// `layoutStore` (`hoverPanelTab`) so consumers outside this component tree —
// the workspace fs-watch, the memory panel's fetch gate — can treat "the user
// can see this panel" as one condition instead of learning about the drawer.

import { useCallback, useEffect, useRef, useState } from "react";
import {
  CLOSED,
  DRAWER_RETRACT_MS,
  HOVER_CROSS_GAP_MS,
  HOVER_OPEN_DELAY_MS,
  drawerCloseNow,
  drawerClosed,
  drawerEnter,
  drawerKeyEnter,
  drawerLeave,
  isPanelTabKey,
  shouldDelayOpen,
  type DrawerKey,
  type DrawerState,
} from "./rightPanelDrawerState";
import { useLayoutStore } from "../../stores/layoutStore";

export interface RightPanelDrawerState extends DrawerState {
  /** The pointer rests on a collapsed rail tile: schedule the peek. */
  enter(key: DrawerKey): void;
  /** The pointer left the rail strip without reaching the panel. */
  railLeave(): void;
  /** The pointer crossed into the panel overlay. */
  panelEnter(): void;
  /** The pointer left the panel overlay. */
  panelLeave(): void;
}

/**
 * @param enabled whether a peek is possible at all — false when the panel is
 * pinned open (the real one is already on screen) or the current view has no
 * side panel to preview.
 * @param scope what the peek belongs to — view plus subject. Changing it
 * closes the drawer: `enabled` alone cannot catch this, because two different
 * views can both be peekable, and a workspace panel left floating over the
 * pm view is a panel that no longer belongs to anything the user is looking
 * at. Reachable by a fast click on the left rail, and by any programmatic
 * view or session change.
 */
export function useRightPanelDrawer(enabled: boolean, scope: string): RightPanelDrawerState {
  const [state, setState] = useState<DrawerState>(CLOSED);
  const stateRef = useRef(state);
  stateRef.current = state;

  const openTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const graceTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const closeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clear = useCallback((t: React.MutableRefObject<ReturnType<typeof setTimeout> | null>) => {
    if (t.current) {
      clearTimeout(t.current);
      t.current = null;
    }
  }, []);

  const clearAll = useCallback(() => {
    clear(openTimer);
    clear(graceTimer);
    clear(closeTimer);
  }, [clear]);

  // A pinned panel is on screen already; a drawer on top of it would be a
  // second live copy of the same surface. Drop the peek rather than leave one
  // hanging when the layout changes under the pointer.
  useEffect(() => {
    if (!enabled) {
      clearAll();
      setState(CLOSED);
    }
  }, [enabled, clearAll]);

  // Same rule for a change of subject: the peek that was opened for one view
  // or session does not carry over to the next.
  useEffect(() => {
    clearAll();
    setState(CLOSED);
  }, [scope, clearAll]);

  useEffect(() => clearAll, [clearAll]);

  /** Unmount once the slide-out has played. */
  const scheduleUnmount = useCallback(() => {
    clear(closeTimer);
    closeTimer.current = setTimeout(() => {
      closeTimer.current = null;
      setState(drawerClosed());
    }, DRAWER_RETRACT_MS);
  }, [clear]);

  /**
   * The pointer rests on a tile. A peek is worth delaying only when nothing
   * is on screen yet (`shouldDelayOpen`); sweeping across tiles swaps panels
   * on the same frame.
   */
  const enter = useCallback(
    (key: DrawerKey) => {
      if (!enabled) return;
      clear(graceTimer);
      clear(closeTimer);
      clear(openTimer);
      if (!shouldDelayOpen(stateRef.current)) {
        setState(drawerKeyEnter(key));
        return;
      }
      openTimer.current = setTimeout(() => {
        openTimer.current = null;
        setState(drawerKeyEnter(key));
      }, HOVER_OPEN_DELAY_MS);
    },
    [enabled, clear],
  );

  /**
   * Pointer left the rail. The peek is NOT dropped here: the most likely next
   * position is the panel itself, a few pixels away, and sliding out and back
   * in for that gap would make the drawer unusable. So the state stays `open`
   * through the grace window and only then slides out.
   */
  const railLeave = useCallback(() => {
    clear(openTimer);
    if (!stateRef.current.open) return;
    clear(graceTimer);
    graceTimer.current = setTimeout(() => {
      graceTimer.current = null;
      setState((s) => drawerCloseNow(s));
      scheduleUnmount();
    }, HOVER_CROSS_GAP_MS);
  }, [clear, scheduleUnmount]);

  /** Pointer crossed into the panel: cancel any pending close. */
  const panelEnter = useCallback(() => {
    clear(graceTimer);
    clear(closeTimer);
    setState((s) => (s.key === null ? s : drawerEnter(s)));
  }, [clear]);

  /**
   * Pointer left the panel. Unlike leaving the rail, this is the decisive
   * gesture the feature is named for — the drawer slides out at once and only
   * the unmount waits for the animation.
   */
  const panelLeave = useCallback(() => {
    if (!stateRef.current.open) return;
    clear(graceTimer);
    setState((s) => drawerLeave(s));
    scheduleUnmount();
  }, [clear, scheduleUnmount]);

  // Mirror the peek into the store. `open`, not `key`: the overlay keeps its
  // content mounted while it slides out, and a panel that is sliding away is
  // no longer something the user is reading.
  const setHoverPanelTab = useLayoutStore((s) => s.setHoverPanelTab);
  useEffect(() => {
    setHoverPanelTab(state.open ? isPanelTabKey(state.key) : null);
  }, [state.open, state.key, setHoverPanelTab]);

  return { ...state, enter, railLeave, panelEnter, panelLeave };
}
