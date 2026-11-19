// @vitest-environment jsdom
/**
 * The hover-drawer clock.
 *
 * The state machine is pure; what can only be tested here is WHEN a
 * transition is allowed to happen. Every one of these timings exists to stop
 * the drawer from doing something the user reads as broken: firing on a
 * cursor that was only passing through, blinking shut while it is being
 * crossed, or refusing to go away after the pointer did.
 */
import { renderHook, act } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  DRAWER_RETRACT_MS,
  HOVER_CROSS_GAP_MS,
  HOVER_OPEN_DELAY_MS,
} from "./rightPanelDrawerState";
import { useRightPanelDrawer } from "./useRightPanelDrawer";
import { useLayoutStore } from "../../stores/layoutStore";

beforeEach(() => {
  vi.useFakeTimers();
  useLayoutStore.setState({ hoverPanelTab: null });
});

afterEach(() => {
  vi.useRealTimers();
});

/** Advance the clock in one step, inside `act` so React flushes the state. */
function tick(ms: number) {
  act(() => {
    vi.advanceTimersByTime(ms);
  });
}

describe("cold hover", () => {
  it("does not open the instant the pointer touches the tile", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));

    expect(result.current.open).toBe(false);
    expect(result.current.key).toBeNull();
  });

  it("opens once the dwell is long enough to be intent", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);

    expect(result.current).toMatchObject({ key: "workspace", open: true });
  });

  it("cancels the pending open if the pointer moves off first", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    act(() => result.current.railLeave());
    tick(HOVER_OPEN_DELAY_MS + HOVER_CROSS_GAP_MS + DRAWER_RETRACT_MS);

    expect(result.current.key).toBeNull();
    expect(result.current.open).toBe(false);
  });
});

describe("sweeping across tiles", () => {
  it("swaps the panel on the same frame, without re-delaying", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);

    act(() => result.current.enter("memory"));
    expect(result.current).toMatchObject({ key: "memory", open: true });
  });
});

describe("leaving the rail", () => {
  it("stays open across the gap, so crossing into the panel does not blink", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);

    act(() => result.current.railLeave());
    tick(HOVER_CROSS_GAP_MS - 1);
    expect(result.current.open).toBe(true);
    expect(result.current.key).toBe("workspace");
  });

  it("slides out when the pointer never arrives, then unmounts after the animation", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);

    act(() => result.current.railLeave());
    tick(HOVER_CROSS_GAP_MS);
    expect(result.current.open).toBe(false);
    // Mounted: the slide-out has to have something to move.
    expect(result.current.key).toBe("workspace");

    tick(DRAWER_RETRACT_MS);
    expect(result.current.key).toBeNull();
  });

  it("re-opening within the grace window cancels the close outright", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);
    act(() => result.current.railLeave());
    tick(HOVER_CROSS_GAP_MS - 1);
    act(() => result.current.panelEnter());
    tick(HOVER_CROSS_GAP_MS + DRAWER_RETRACT_MS);

    expect(result.current).toMatchObject({ key: "workspace", open: true });
  });

  it("steps straight back onto a tile whose panel is mid-slide-out", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);
    act(() => result.current.panelLeave());
    tick(40);
    expect(result.current.open).toBe(false);

    // The overlay is still on screen, animating away. Requiring the full
    // dwell again here makes the drawer feel like it is fighting back.
    act(() => result.current.enter("workspace"));
    expect(result.current.open).toBe(true);
  });
});

describe("leaving the panel", () => {
  it("slides out immediately — this is the gesture the drawer exists for", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);

    act(() => result.current.panelLeave());
    expect(result.current.open).toBe(false);

    tick(DRAWER_RETRACT_MS);
    expect(result.current.key).toBeNull();
  });

  it("does nothing once it is already closed", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.panelLeave());
    expect(result.current.key).toBeNull();
  });
});

describe("a pinned panel", () => {
  it("never gets a drawer on top of it", () => {
    const { result } = renderHook(() => useRightPanelDrawer(false, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS + HOVER_CROSS_GAP_MS);

    expect(result.current.key).toBeNull();
    expect(result.current.open).toBe(false);
  });

  it("drops a peek when the subject changes out from under it", () => {
    const { result, rerender } = renderHook(({ on, scope }) => useRightPanelDrawer(on, scope), {
      initialProps: { on: true, scope: "chat:agent" },
    });
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);
    expect(result.current.open).toBe(true);

    rerender({ on: true, scope: "chat:other" });
    expect(result.current.key).toBeNull();
    expect(useLayoutStore.getState().hoverPanelTab).toBeNull();
  });
});

describe("layoutStore mirror", () => {
  it("publishes the previewed tab while the peek is up", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);

    expect(useLayoutStore.getState().hoverPanelTab).toBe("workspace");
  });

  it("clears it as soon as the peek starts sliding away, not after the animation", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("workspace"));
    tick(HOVER_OPEN_DELAY_MS);
    act(() => result.current.panelLeave());

    // The overlay is still mounted for the animation, but nobody is reading
    // it — the fs-watch must not keep its directories subscribed for that.
    expect(useLayoutStore.getState().hoverPanelTab).toBeNull();
  });

  it("does not mirror a rail that is not an agent panel", () => {
    const { result } = renderHook(() => useRightPanelDrawer(true, "chat:agent"));
    act(() => result.current.enter("view_pm"));
    tick(HOVER_OPEN_DELAY_MS);

    expect(result.current.open).toBe(true);
    expect(useLayoutStore.getState().hoverPanelTab).toBeNull();
  });
});
