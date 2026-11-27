/**
 * cacheApi tests (ADR-123 §3).
 *
 * The one thing worth pinning: the cleanup request body carries the service
 * routing key and the category ids and **nothing else** — no size, no path, no
 * "keep N" knob (ADR-123 §1.3 red line 3). If someone later adds a field here,
 * this test is the one that says no.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

import { cleanupCache, fetchCacheReport, CacheApiError } from "./cacheApi";

const ok = (body: unknown) =>
  ({ ok: true, status: 200, json: async () => body }) as unknown as Response;

describe("cacheApi", () => {
  const fetchMock = vi.fn();

  beforeEach(() => {
    fetchMock.mockReset();
    vi.stubGlobal("fetch", fetchMock);
  });

  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("sends the service key and ids only", async () => {
    fetchMock.mockResolvedValue(ok({ freed_bytes: 2048 }));

    const result = await cleanupCache("node:abc", ["logs"]);

    expect(result.freed_bytes).toBe(2048);
    const [url, init] = fetchMock.mock.calls[0];
    expect(String(url)).toContain("/api/cache/cleanup");
    expect(init.method).toBe("POST");
    expect(JSON.parse(init.body)).toEqual({ service: "node:abc", ids: ["logs"] });
  });

  it("returns the report rows verbatim and surfaces the server's error text", async () => {
    fetchMock.mockResolvedValueOnce(
      ok({
        disk: {
          total_bytes: 100,
          available_bytes: 10,
          used_percent: 90,
          threshold_percent: 90,
          alarm: true,
        },
        services: [{ service: "embed", reachable: false, error: "unreachable", items: [] }],
      }),
    );
    const report = await fetchCacheReport();
    expect(report.disk.alarm).toBe(true);
    expect(report.services[0].reachable).toBe(false);

    fetchMock.mockResolvedValue({
      ok: false,
      status: 400,
      statusText: "Bad Request",
      json: async () => ({ error: "'nope' is not a cleanable gateway category" }),
    } as unknown as Response);
    await expect(cleanupCache("gateway", ["nope"])).rejects.toBeInstanceOf(CacheApiError);
    await expect(cleanupCache("gateway", ["nope"])).rejects.toThrow(/not a cleanable/);
  });
});