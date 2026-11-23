// @vitest-environment jsdom
/**
 * The to-dos box above the composer is collapsed by default, expands on click,
 * and re-collapses when the session changes. (The message queue this test used
 * to cover was removed with the queue feature itself.)
 */
import { act, renderHook } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { useSessionScope } from "./useSessionScope";

describe("useSessionScope collapse state", () => {
  it("starts to-dos collapsed", () => {
    const { result } = renderHook(() => useSessionScope("s1"));
    expect(result.current.todosCollapsed).toBe(true);
  });

  it("re-collapses on session change", () => {
    const { result, rerender } = renderHook(({ id }) => useSessionScope(id), {
      initialProps: { id: "s1" },
    });
    act(() => {
      result.current.setTodosCollapsed(false);
    });
    expect(result.current.todosCollapsed).toBe(false);

    rerender({ id: "s2" });
    expect(result.current.todosCollapsed).toBe(true);
  });
});