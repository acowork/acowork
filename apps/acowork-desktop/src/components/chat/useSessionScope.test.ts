// @vitest-environment jsdom
/**
 * Both boxes above the composer (to-dos and the message queue) share one
 * interaction: collapsed by default, expand on click, header carries the first
 * item. This pins the default and the per-session reset so the two never drift.
 */
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useSessionScope } from "./useSessionScope";

describe("useSessionScope collapse state", () => {
  it("starts to-dos and the message queue collapsed", () => {
    const { result } = renderHook(() => useSessionScope("s1"));
    expect(result.current.todosCollapsed).toBe(true);
    expect(result.current.queueCollapsed).toBe(true);
  });

  it("re-collapses both when the session changes", () => {
    const { result, rerender } = renderHook(({ id }) => useSessionScope(id), {
      initialProps: { id: "s1" },
    });
    act(() => {
      result.current.setTodosCollapsed(false);
      result.current.setQueueCollapsed(false);
    });
    expect(result.current.queueCollapsed).toBe(false);

    rerender({ id: "s2" });
    expect(result.current.todosCollapsed).toBe(true);
    expect(result.current.queueCollapsed).toBe(true);
  });
});