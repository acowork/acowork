/**
 * ADR-113 D6 guard: node topology is replayable protocol state.
 *
 * Any surface that decides whether an install is possible must read
 * `GET /api/nodes` when it becomes active and keep reading while that
 * decision is on screen. Relying on a mount snapshot is what produced the
 * cold-start bug where a Gateway-spawned node existed but the dialog still
 * said "No available node" until an unrelated action refreshed the store.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { renderHook } from "@testing-library/react";

const h = vi.hoisted(() => ({
  fetchNodes: vi.fn(async () => {}),
  warn: vi.fn(),
}));

vi.mock("../../stores/agentStore", () => {
  const store = () => undefined;
  (store as unknown as { getState: unknown }).getState = () => ({
    fetchNodes: h.fetchNodes,
  });
  return { useAgentStore: store };
});

vi.mock("../../lib/logger", () => ({ log: { warn: h.warn } }));

import {
  NODE_TOPOLOGY_REFRESH_MS,
  useNodeTopologyRefresh,
} from "./useNodeTopologyRefresh";

beforeEach(() => {
  vi.useFakeTimers();
  h.fetchNodes.mockClear();
  h.warn.mockClear();
  h.fetchNodes.mockImplementation(async () => {});
});

afterEach(() => {
  vi.useRealTimers();
});

describe("useNodeTopologyRefresh", () => {
  it("does not poll while inactive", () => {
    renderHook(() => useNodeTopologyRefresh(false));
    vi.advanceTimersByTime(NODE_TOPOLOGY_REFRESH_MS * 3);
    expect(h.fetchNodes).not.toHaveBeenCalled();
  });

  it("reads immediately on activation and keeps polling until unmount", () => {
    const { unmount } = renderHook(() => useNodeTopologyRefresh(true));

    // No waiting for the first tick: the caller needs the state now.
    expect(h.fetchNodes).toHaveBeenCalledTimes(1);

    vi.advanceTimersByTime(NODE_TOPOLOGY_REFRESH_MS);
    expect(h.fetchNodes).toHaveBeenCalledTimes(2);

    vi.advanceTimersByTime(NODE_TOPOLOGY_REFRESH_MS * 2);
    expect(h.fetchNodes).toHaveBeenCalledTimes(4);

    unmount();
    vi.advanceTimersByTime(NODE_TOPOLOGY_REFRESH_MS * 2);
    expect(h.fetchNodes).toHaveBeenCalledTimes(4);
  });

  it("survives a failed poll and retries on the next tick", async () => {
    h.fetchNodes.mockRejectedValueOnce(new Error("503"));

    renderHook(() => useNodeTopologyRefresh(true));
    // Let the rejected promise settle; the hook must not surface it.
    await Promise.resolve();
    await Promise.resolve();

    expect(h.warn).toHaveBeenCalled();
    expect(h.fetchNodes).toHaveBeenCalledTimes(1);

    vi.advanceTimersByTime(NODE_TOPOLOGY_REFRESH_MS);
    expect(h.fetchNodes).toHaveBeenCalledTimes(2);
  });
});
