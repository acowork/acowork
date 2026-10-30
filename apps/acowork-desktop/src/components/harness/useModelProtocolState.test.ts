//! ADR-103 D11/D12 — the save payload is the contract that decides what the
//! Gateway re-probes.
//!
//! Two failure modes are silent and expensive, so both are pinned here:
//!
//!   * an untouched model must NOT appear in `model_protocols` (otherwise every
//!     save rewrites it and D12 re-probes it — a real HTTP request per save);
//!   * withdrawing a selection must send `null` (not omit the key), because omit
//!     means "keep the stored value" and the §4.4 rollback path would be
//!     unreachable from the UI.

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { renderHook, act } from "@testing-library/react";
import { useModelProtocolState } from "./useModelProtocolState";
import { RESPONSES_PROTOCOL } from "../../lib/modelProtocol";
import type { ModelInfo } from "../../lib/types";

function model(id: string, extra: Partial<ModelInfo> = {}): ModelInfo {
  return { id, name: id, ...extra } as ModelInfo;
}

describe("useModelProtocolState", () => {
  it("seeds the dropdown from the stored selection", () => {
    const { result } = renderHook(() => useModelProtocolState());
    act(() => {
      result.current.hydrate([
        model("a", { protocol_selected: RESPONSES_PROTOCOL }),
        model("b"),
      ]);
    });
    expect(result.current.selections["a"]).toBe("responses");
    expect(result.current.selections["b"]).toBe("follow");
  });

  it("sends nothing for untouched models", () => {
    const { result } = renderHook(() => useModelProtocolState());
    const models = [model("a", { protocol_selected: RESPONSES_PROTOCOL }), model("b")];
    act(() => result.current.hydrate(models));

    const intent = result.current.buildIntent(models);
    expect(intent.model_protocols).toEqual({});
    expect(intent.model_base_urls).toEqual({});
  });

  it("sends the wire value only for models the user changed", () => {
    const { result } = renderHook(() => useModelProtocolState());
    const models = [model("a"), model("b")];
    act(() => result.current.hydrate(models));
    act(() => result.current.setSelection("a", "responses"));

    const intent = result.current.buildIntent(models);
    expect(intent.model_protocols).toEqual({ a: RESPONSES_PROTOCOL });
    expect(intent.model_protocols).not.toHaveProperty("b");
  });

  it("clears a withdrawn selection with an explicit null", () => {
    const { result } = renderHook(() => useModelProtocolState());
    const models = [model("a", { protocol_selected: RESPONSES_PROTOCOL })];
    act(() => result.current.hydrate(models));
    act(() => result.current.setSelection("a", "follow"));

    const intent = result.current.buildIntent(models);
    // `undefined` would be dropped by JSON.stringify and read as "preserve",
    // leaving the user stuck on a protocol they just deselected.
    expect("a" in intent.model_protocols).toBe(true);
    expect(intent.model_protocols["a"]).toBeNull();
  });

  it("carries the base_url override verbatim and drops blank input", () => {
    const { result } = renderHook(() => useModelProtocolState());
    const models = [model("a"), model("b")];
    act(() => result.current.hydrate(models));
    act(() => result.current.setSelection("a", "responses"));
    act(() => result.current.setBaseUrl("a", "  https://proxy.test/v1  "));
    act(() => result.current.setBaseUrl("b", "   "));

    const intent = result.current.buildIntent(models);
    expect(intent.model_base_urls).toEqual({ a: "https://proxy.test/v1" });
  });

  it("offers the base_url input only for Responses", () => {
    const { result } = renderHook(() => useModelProtocolState());
    act(() => result.current.hydrate([model("a")]));
    expect(result.current.needsBaseUrl("a")).toBe(false);
    act(() => result.current.setSelection("a", "responses"));
    expect(result.current.needsBaseUrl("a")).toBe(true);
  });

  it("counts the requests the next save will send", () => {
    const { result } = renderHook(() => useModelProtocolState());
    const models = [model("a"), model("b")];
    act(() => result.current.hydrate(models));
    expect(result.current.estimate(models)).toEqual({ models: 0, requests: 0 });

    act(() => result.current.setSelection("a", "responses"));
    // Deep tier = L1 + L3 + the function_call_output replay.
    expect(result.current.estimate(models)).toEqual({ models: 1, requests: 3 });
  });

  it("shows the stored verdict as-is while the selection is unchanged", () => {
    const { result } = renderHook(() => useModelProtocolState());
    const m = model("a", {
      protocol_selected: RESPONSES_PROTOCOL,
      protocol_active: RESPONSES_PROTOCOL,
      protocol_probe: { state: "verified" },
    });
    act(() => result.current.hydrate([m]));
    expect(result.current.badgeFor(m)).toEqual({ kind: "verified" });
  });

  it("drops the yellow badge when a pending choice replaces a failed verdict", () => {
    // The user switched a failed model back to "follow": the stale failure no
    // longer describes what happens on save, so the badge must go.
    const { result } = renderHook(() => useModelProtocolState());
    const m = model("a", {
      protocol_selected: RESPONSES_PROTOCOL,
      protocol_active: "openai",
      protocol_probe: { state: "failed", reason: "endpoint_unavailable" },
    });
    act(() => result.current.hydrate([m]));
    expect(result.current.badgeFor(m).kind).toBe("blocked");

    act(() => result.current.setSelection("a", "follow"));
    expect(result.current.badgeFor(m).kind).toBe("none");
  });
});

describe("useModelProtocolState.verify", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("forces a re-probe and folds the result into the badge", async () => {
    // `ProbeResponse` flattens `ProbeOutcome`: state, tier list and
    // `stripped_fields` are all at the top level (D10.1).
    fetchMock.mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        state: "degraded",
        tier_passed: ["L1", "L2"],
        stripped_fields: ["reasoning"],
      }),
    });
    const { result } = renderHook(() => useModelProtocolState());
    const m = model("a", { protocol_selected: RESPONSES_PROTOCOL });
    act(() => result.current.hydrate([m]));

    let summary;
    await act(async () => {
      summary = await result.current.verify("acme", "a");
    });

    const [url, init] = fetchMock.mock.calls[0];
    expect(url).toContain("/api/providers/acme/probe?force=true");
    expect(JSON.parse(init.body)).toEqual({ model: "a", protocol: RESPONSES_PROTOCOL });
    expect(summary).toMatchObject({
      ok: true,
      state: "degraded",
      strippedFields: ["reasoning"],
    });
    // The field names must reach the badge, or the tooltip cannot say what was
    // dropped (S7: "degraded" alone is not actionable).
    expect(result.current.badgeFor(m)).toEqual({
      kind: "degraded",
      stripped: ["reasoning"],
    });
  });

  it("reports a failed probe as blocked with its reason", async () => {
    fetchMock.mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => ({
        state: "failed",
        error: { class: "EndpointUnavailable", message: "404 on /responses" },
      }),
    });
    const { result } = renderHook(() => useModelProtocolState());
    const m = model("a", { protocol_selected: RESPONSES_PROTOCOL });
    act(() => result.current.hydrate([m]));

    await act(async () => {
      await result.current.verify("acme", "a");
    });

    expect(result.current.badgeFor(m)).toEqual({
      kind: "blocked",
      reason: "failed",
      detail: "404 on /responses",
    });
  });

  it("surfaces a transport error instead of throwing", async () => {
    fetchMock.mockRejectedValue(new Error("gateway unreachable"));
    const { result } = renderHook(() => useModelProtocolState());
    act(() => result.current.hydrate([model("a")]));

    let summary;
    await act(async () => {
      summary = await result.current.verify("acme", "a");
    });
    expect(summary).toMatchObject({ ok: false, state: "error", detail: "gateway unreachable" });
  });
});