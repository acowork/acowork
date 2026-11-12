/**
 * servicesApi tests — P1-G / P2-B.
 *
 * Coverage focus (matches the §3.2 / §3.3 acceptance criteria):
 *   - `probeGateway` flips to offline on non-2xx
 *   - `probeGateway` flips to offline on `AbortError` (timeout)
 *   - `fetchGatewayDiagnose` maps a valid snapshot into 7 rows and
 *     resolves `null` on 404 / malformed bodies / network errors (P2)
 *   - `probeAllServices` prefers the snapshot (exactly one fetch) and
 *     falls back to the direct probes on a pre-P2 Gateway (P2)
 *   - `probeAllServices` short-circuits the 6 other probes when
 *     gateway is offline — they all carry `last_error: "gateway offline: …"`
 *   - `probeService` dispatches through the snapshot first, then the
 *     direct per-type functions
 *   - `PROBE_TIMEOUT_MS` is exactly 1000
 */

import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

// Mock `@tauri-apps/api/core` so the `invoke('get_mqtt_status')` call
// inside `probeMqtt` can be controlled without booting Tauri.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
}));

// Import after the mock so the module picks up the mocked invoke.
import { invoke } from "@tauri-apps/api/core";
import {
  fetchGatewayDiagnose,
  PROBE_TIMEOUT_MS,
  probeAllServices,
  probeGateway,
  probeMqtt,
  probeNodes,
  probeService,
} from "./servicesApi";
import type { ServiceHealth, ServiceType } from "./types";

const mockedInvoke = vi.mocked(invoke);

beforeEach(() => {
  mockedInvoke.mockReset();
  vi.unstubAllGlobals();
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

/** Build a `Response` object with the given JSON body and status. */
function jsonResponse(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: () => Promise.resolve(body),
  } as Response;
}

/** URL-aware fetch stub: `routes` maps a URL substring to a responder;
 *  unmatched URLs answer 404 (the pre-P2 "snapshot endpoint absent"
 *  case the fallback path must tolerate). */
function stubFetchRoutes(
  routes: Array<[string, () => Response]>,
): ReturnType<typeof vi.fn> {
  const fn = vi.fn((url: string) => {
    for (const [match, respond] of routes) {
      if (url.includes(match)) return Promise.resolve(respond());
    }
    return Promise.resolve(jsonResponse({ error: "not found" }, 404));
  });
  vi.stubGlobal("fetch", fn);
  return fn;
}

const SNAPSHOT_URL = "/api/services/diagnose";
const STATUS_URL = "/api/status";
const NODES_URL = "/api/nodes";

/** `GET /api/status` body (direct-path probe fixture). */
function statusPayload(): Record<string, unknown> {
  return {
    version: "0.9.1",
    agents_installed: 3,
    agents_running: 2,
    uptime_secs: 60,
    mqtt_port: 19875,
  };
}

/** `GET /api/services/diagnose` body (P2 snapshot fixture) — mirrors
 *  `services_api.rs`: running embed (model loaded) + pm, a stopped doc,
 *  broker up, and 1/2 nodes online (the online one advertises the
 *  lsp-relay endpoint via its retained `lsps` topic — `has_lsp_relay`
 *  is derived from `NodeRegistry::lsp_endpoint`). */
function snapshotPayload(): Record<string, unknown> {
  return {
    gateway: {
      version: "0.9.2",
      instance_id: "inst-1",
      http_port: 19876,
      mqtt_port: 19875,
      agents_running: 2,
      agents_installed: 3,
    },
    mqtt: { broker_running: true, port: 19875, auth_enabled: true },
    embed: {
      running: true,
      ready: true,
      port: 19901,
      pid: 111,
      active_model_id: "bge-small-zh-v1.5",
    },
    pm: { running: true, ready: true, port: 19902, pid: 222 },
    doc: { running: false, ready: false, port: 0, pid: 0 },
    nodes: {
      total: 2,
      online: 1,
      items: [
        {
          node_id: "n1",
          online: true,
          node_version: "0.9.0",
          has_lsp_relay: true,
        },
        {
          node_id: "n2",
          online: false,
          node_version: "0.8.9",
          has_lsp_relay: false,
        },
      ],
    },
    diagnosed_at: "2026-09-14T00:00:00Z",
  };
}

describe("PROBE_TIMEOUT_MS", () => {
  it("is 1000ms — gates the worst-case diagnostic panel render time", () => {
    expect(PROBE_TIMEOUT_MS).toBe(1000);
  });
});

describe("probeGateway", () => {
  it("returns online + version on a healthy 2xx", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          jsonResponse({
            version: "0.9.1",
            agents_installed: 3,
            agents_running: 2,
            uptime_secs: 60,
            mqtt_port: 19875,
          }),
        ),
      ),
    );
    const row = await probeGateway("http://gw");
    expect(row.online).toBe(true);
    expect(row.service_type).toBe("gateway");
    expect(row.version).toBe("0.9.1");
    expect(row.last_error).toBeNull();
    expect(row.latency_ms).toBeGreaterThanOrEqual(0);
  });

  it("flips to offline + HTTP error on a non-2xx response", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.resolve(jsonResponse({ error: "boom" }, 503))),
    );
    const row = await probeGateway("http://gw");
    expect(row.online).toBe(false);
    expect(row.last_error).toMatch(/HTTP 503/);
    expect(row.service_type).toBe("gateway");
  });

  it("flips to offline + 'timeout' on AbortError", async () => {
    // jsdom's fetch is synchronous-promise — emulate an abort.
    vi.stubGlobal(
      "fetch",
      vi.fn(
        (_url: string, init?: RequestInit) =>
          new Promise((_resolve, reject) => {
            init?.signal?.addEventListener("abort", () => {
              const err = new DOMException("aborted", "AbortError");
              reject(err);
            });
          }),
      ),
    );
    const row = await probeGateway("http://gw");
    expect(row.online).toBe(false);
    expect(row.last_error).toMatch(/timeout/);
  });
});

describe("probeMqtt", () => {
  it("returns online when invoke reports known+connected", async () => {
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });
    const row = await probeMqtt();
    expect(row.online).toBe(true);
    expect(row.service_type).toBe("mqtt");
    expect(mockedInvoke).toHaveBeenCalledWith("get_mqtt_status");
  });

  it("flips to offline when invoke reports known=false", async () => {
    mockedInvoke.mockResolvedValueOnce({
      known: false,
      connected: false,
      reason: "client not initialized",
    });
    const row = await probeMqtt();
    expect(row.online).toBe(false);
    expect(row.last_error).toMatch(/not initialized/);
  });

  it("flips to offline when invoke reports connected=false", async () => {
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: false,
      reason: "broker unreachable",
    });
    const row = await probeMqtt();
    expect(row.online).toBe(false);
    expect(row.last_error).toMatch(/broker unreachable/);
  });
});

describe("probeNodes", () => {
  it("returns online with summary when at least one node reports online", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          jsonResponse([
            {
              node_id: "n1",
              online: true,
              node_version: "0.9.0",
              capabilities: [],
            },
            {
              node_id: "n2",
              online: false,
              node_version: "0.9.0",
              capabilities: [],
            },
          ]),
        ),
      ),
    );
    const row = await probeNodes("http://gw");
    expect(row.online).toBe(true);
    expect(row.service_type).toBe("node");
    expect(row.detail).toMatch(/1\/2 nodes online/);
    expect(row.version).toBe("0.9.0");
  });

  it("flips to offline when all nodes are offline", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() =>
        Promise.resolve(
          jsonResponse([
            {
              node_id: "n1",
              online: false,
              node_version: "0.9.0",
              capabilities: [],
            },
          ]),
        ),
      ),
    );
    const row = await probeNodes("http://gw");
    expect(row.online).toBe(false);
    expect(row.last_error).toMatch(/0\/1 nodes online/);
  });
});

describe("fetchGatewayDiagnose (P2 snapshot path)", () => {
  it("maps a healthy snapshot into 7 rows with the gateway-api source", async () => {
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(snapshotPayload())]]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await fetchGatewayDiagnose("http://gw");
    expect(result).not.toBeNull();
    expect(result?.source).toBe("gateway-api");
    const map = result!.services;
    expect(map.gateway.online).toBe(true);
    expect(map.gateway.version).toBe("0.9.2");
    expect(map.gateway.detail).toMatch(/2\/3 agents/);
    expect(map.mqtt.online).toBe(true);
    expect(map.mqtt.detail).toMatch(/auth on/);
    expect(map.node.online).toBe(true);
    expect(map.node.detail).toMatch(/1\/2 nodes online/);
    // Lowest version across the whole fleet (same rule as `probeNodes`) —
    // n2 is offline on 0.8.9.
    expect(map.node.version).toBe("0.8.9");
    expect(map.embed.online).toBe(true);
    expect(map.embed.detail).toMatch(/bge-small-zh-v1\.5/);
    expect(map.pm.online).toBe(true);
    expect(map.doc.online).toBe(false);
    expect(map.doc.last_error).toMatch(/doc subsystem not running/);
    expect(map["lsp-relay"].online).toBe(true);
    expect(map["lsp-relay"].detail).toMatch(
      /1\/1 online nodes advertise relay/,
    );
  });

  it("resolves null on 404 (pre-P2 Gateway) without throwing", async () => {
    stubFetchRoutes([]); // every URL answers 404
    const result = await fetchGatewayDiagnose("http://gw");
    expect(result).toBeNull();
  });

  it("resolves null when a 200 body does not match the contract", async () => {
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(statusPayload())]]);
    const result = await fetchGatewayDiagnose("http://gw");
    expect(result).toBeNull();
  });

  it("resolves null when the fetch rejects (gateway down)", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new Error("ECONNREFUSED"))),
    );
    const result = await fetchGatewayDiagnose("http://gw");
    expect(result).toBeNull();
  });

  it("marks embed offline when the process is running but not ready", async () => {
    const payload = snapshotPayload();
    (payload.embed as Record<string, unknown>).ready = false;
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(payload)]]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await fetchGatewayDiagnose("http://gw");
    expect(result!.services.embed.online).toBe(false);
    expect(result!.services.embed.last_error).toMatch(/not ready/);
  });

  it("marks mqtt offline when the broker is down even if the client connected", async () => {
    const payload = snapshotPayload();
    (payload.mqtt as Record<string, unknown>).broker_running = false;
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(payload)]]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await fetchGatewayDiagnose("http://gw");
    expect(result!.services.mqtt.online).toBe(false);
    expect(result!.services.mqtt.last_error).toMatch(/broker not running/);
  });

  it("marks mqtt offline when the local client is down even if the broker runs", async () => {
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(snapshotPayload())]]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: false,
      reason: "broker unreachable",
    });

    const result = await fetchGatewayDiagnose("http://gw");
    expect(result!.services.mqtt.online).toBe(false);
    expect(result!.services.mqtt.last_error).toMatch(/broker unreachable/);
    expect(result!.services.mqtt.detail).toMatch(/client disconnected/);
  });

  it("marks mqtt offline when the local client read fails", async () => {
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(snapshotPayload())]]);
    mockedInvoke.mockRejectedValueOnce(new Error("ipc broken"));

    const result = await fetchGatewayDiagnose("http://gw");
    // An unreadable Desktop-side status is the silent-failure mode this
    // row exists to catch (broker reports healthy while the chat input
    // is dead) — it must fail red instead of falling back to the broker
    // half alone, matching `probeMqtt`'s direct-path verdict.
    expect(result!.services.mqtt.online).toBe(false);
    expect(result!.services.mqtt.detail).toMatch(/client unreadable/);
    expect(result!.services.mqtt.last_error).toMatch(/unreadable/);
  });

  it("marks lsp-relay offline when no online node advertises it", async () => {
    const payload = snapshotPayload();
    payload.nodes = {
      total: 1,
      online: 1,
      items: [
        {
          node_id: "n1",
          online: true,
          node_version: "0.9.0",
          has_lsp_relay: false,
        },
      ],
    };
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(payload)]]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await fetchGatewayDiagnose("http://gw");
    expect(result!.services["lsp-relay"].online).toBe(false);
    expect(result!.services["lsp-relay"].last_error).toMatch(
      /advertises lsp-relay/,
    );
  });
});

describe("probeAllServices (snapshot-first, P2)", () => {
  it("prefers the snapshot when available — exactly one fetch", async () => {
    const fetchMock = stubFetchRoutes([
      [SNAPSHOT_URL, () => jsonResponse(snapshotPayload())],
    ]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await probeAllServices("http://gw");
    expect(result.source).toBe("gateway-api");
    expect(result.services.gateway.online).toBe(true);
    expect(result.services.doc.online).toBe(false);
    // The snapshot must not fan out to the per-service endpoints.
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(String(fetchMock.mock.calls[0][0])).toContain(SNAPSHOT_URL);
  });

  it("falls back to direct probes on 404 — source 'direct'", async () => {
    stubFetchRoutes([
      [STATUS_URL, () => jsonResponse(statusPayload())],
      [NODES_URL, () => jsonResponse([])],
    ]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await probeAllServices("http://gw");
    expect(result.source).toBe("direct");
    expect(result.services.gateway.online).toBe(true);
    expect(result.services.embed.online).toBe(true);
    expect(result.services.node.online).toBe(false);
    expect(result.services.node.last_error).toMatch(/no nodes registered/);
  });

  it("falls back when a 200 body does not match the contract", async () => {
    stubFetchRoutes([
      [SNAPSHOT_URL, () => jsonResponse(statusPayload())],
      [STATUS_URL, () => jsonResponse(statusPayload())],
      [NODES_URL, () => jsonResponse([])],
    ]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await probeAllServices("http://gw");
    expect(result.source).toBe("direct");
    expect(result.services.gateway.online).toBe(true);
  });

  it("returns 7 rows — one per ServiceType — even when the gateway is down", async () => {
    // The snapshot fetch rejects and the fallback probes reject too —
    // the envelope must still carry all 7 rows. mqtt invoke still runs
    // (it's a Tauri command, not an HTTP fetch) — return a
    // known/connected snapshot.
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new Error("ECONNREFUSED"))),
    );
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const result = await probeAllServices("http://gw");
    const map = result.services;
    expect(result.source).toBe("direct");
    const expectedTypes: ServiceType[] = [
      "gateway",
      "mqtt",
      "node",
      "embed",
      "pm",
      "doc",
      "lsp-relay",
    ];
    for (const t of expectedTypes) {
      expect(map[t]).toBeDefined();
      expect(map[t].service_type).toBe(t);
    }
    expect(Object.keys(map)).toHaveLength(expectedTypes.length);
  });

  it("propagates the gateway-down reason to every other row", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(() => Promise.reject(new Error("ECONNREFUSED"))),
    );
    // mqtt invoke still runs — but we don't care about its outcome;
    // the override rule should stamp every other row.
    mockedInvoke.mockResolvedValueOnce({
      known: false,
      connected: false,
      reason: "irrelevant",
    });

    const map = (await probeAllServices("http://gw")).services;
    for (const t of Object.keys(map) as ServiceType[]) {
      if (t === "gateway") continue; // gateway carries its own error
      const row = map[t];
      expect(row.online).toBe(false);
      expect(row.last_error).toMatch(/gateway offline/);
    }
  });

  it("keeps individual probe results when the gateway is online", async () => {
    // Snapshot endpoint 404s (pre-P2 Gateway) → the direct path walks
    // the per-service endpoints: /api/status for gateway + embed +
    // optional subsystems, /api/nodes for the node row (empty list →
    // "no nodes registered").
    stubFetchRoutes([
      [STATUS_URL, () => jsonResponse(statusPayload())],
      [NODES_URL, () => jsonResponse([])],
    ]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });

    const map = (await probeAllServices("http://gw")).services;
    expect(map.gateway.online).toBe(true);
    expect(map.mqtt.online).toBe(true);
    // nodes: 0 online → offline, but for a different reason than "gateway offline"
    expect(map.node.online).toBe(false);
    expect(map.node.last_error).toMatch(/no nodes registered/);
    expect(map.embed.online).toBe(true);
    expect(map.pm.online).toBe(true);
    expect(map.doc.online).toBe(true);
    expect(map["lsp-relay"].online).toBe(true);
  });
});

describe("probeService (snapshot-first, then per-type dispatch)", () => {
  it("prefers the snapshot for a single-row retry (P2)", async () => {
    stubFetchRoutes([[SNAPSHOT_URL, () => jsonResponse(snapshotPayload())]]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });
    const row = await probeService("embed", "http://gw");
    expect(row.service_type).toBe("embed");
    expect(row.online).toBe(true);
    expect(row.detail).toMatch(/bge-small-zh-v1\.5/);
  });

  it("routes 'gateway' to probeGateway when the snapshot endpoint is absent", async () => {
    stubFetchRoutes([[STATUS_URL, () => jsonResponse(statusPayload())]]);
    const row = await probeService("gateway", "http://gw");
    expect(row.service_type).toBe("gateway");
    expect(row.online).toBe(true);
  });

  it("routes 'pm' / 'doc' / 'lsp-relay' through the optional subsystem probe", async () => {
    stubFetchRoutes([[STATUS_URL, () => jsonResponse(statusPayload())]]);
    const pm = await probeService("pm", "http://gw");
    const doc = await probeService("doc", "http://gw");
    const lsp = await probeService("lsp-relay", "http://gw");
    expect(pm.service_type).toBe("pm");
    expect(pm.group).toBe("optional");
    expect(doc.service_type).toBe("doc");
    expect(doc.group).toBe("optional");
    expect(lsp.service_type).toBe("lsp-relay");
    expect(lsp.group).toBe("optional");
    expect(pm.online).toBe(true);
  });

  it("every ServiceHealth carries a numeric probed_at timestamp", async () => {
    stubFetchRoutes([
      [STATUS_URL, () => jsonResponse(statusPayload())],
      [NODES_URL, () => jsonResponse([])],
    ]);
    mockedInvoke.mockResolvedValueOnce({
      known: true,
      connected: true,
      reason: null,
    });
    const map = (await probeAllServices("http://gw")).services;
    const now = Date.now();
    for (const t of Object.keys(map) as ServiceType[]) {
      const row: ServiceHealth = map[t];
      expect(typeof row.probed_at).toBe("number");
      // probed_at is the time the probe was *kicked off* — must be
      // within a sane window of "now".
      expect(row.probed_at).toBeGreaterThan(now - 5000);
      expect(row.probed_at).toBeLessThanOrEqual(now + 100);
    }
  });
});
