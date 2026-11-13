/**
 * Unit tests for the auth-aware fetch interceptor (ADR-076 §决策 3).
 *
 * The interceptor is the single place every Gateway request gets its
 * bearer token and its 401→refresh→replay ladder, so a regression here
 * silently breaks the whole Desktop. Pin the four
 * behaviours: skip non-Gateway origins, skip `/api/auth/*`, attach the
 * token, and replay once after a successful refresh.
 */
import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";

vi.mock("./config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("./logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

import { installAuthFetchInterceptor } from "./authFetch";
import { useAuthStore } from "../stores/authStore";
import { adopt, clear, __resetForTests } from "./session/tokens";
import { installTokenTransport } from "./session/wiring";

function response(status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => ({}),
  } as unknown as Response;
}

let original: typeof fetch;
let uninstall: () => void;

beforeEach(() => {
  original = globalThis.fetch;
  // The 401→refresh→replay case drives a real rotation, which needs the
  // module's transport. It is wired explicitly now (not as an authStore
  // module side effect), so install it here.
  installTokenTransport();
  uninstall = installAuthFetchInterceptor();
  useAuthStore.setState({ accountState: "ready", status: "logged_in" });
  // The interceptor takes tokens from the session module, so that is where
  // a test puts one. `at` is not a JWT, which keeps the scheduled rotation
  // out of these cases — they are about the request path, not the timer.
  __resetForTests();
  adopt({ access_token: "at", refresh_token: "rt" });
});

afterEach(() => {
  uninstall();
  __resetForTests();
  globalThis.fetch = original;
});

function authHeaderOf(init?: RequestInit): string | undefined {
  return init?.headers instanceof Headers
    ? (init.headers.get("Authorization") ?? undefined)
    : undefined;
}

describe("installAuthFetchInterceptor", () => {
  it("attaches the bearer token to Gateway requests", async () => {
    const spy = vi.fn(async () => response(200));
    globalThis.fetch = spy as unknown as typeof fetch;
    uninstall = installAuthFetchInterceptor();

    await window.fetch("http://gw.test/api/agents");

    expect(authHeaderOf((spy.mock.calls[0][1] as RequestInit) ?? undefined)).toBe("Bearer at");
  });

  it("leaves non-Gateway origins untouched", async () => {
    const spy = vi.fn(async () => response(200));
    globalThis.fetch = spy as unknown as typeof fetch;
    uninstall = installAuthFetchInterceptor();

    await window.fetch("http://127.0.0.1:9999/lsp");

    expect(spy.mock.calls[0][1]).toBeUndefined();
  });

  it("does not touch the auth routes", async () => {
    const spy = vi.fn(async () => response(200));
    globalThis.fetch = spy as unknown as typeof fetch;
    uninstall = installAuthFetchInterceptor();

    await window.fetch("http://gw.test/api/auth/login");

    expect(spy.mock.calls[0][1]).toBeUndefined();
  });

  it("refreshes and replays the request once on a 401", async () => {
    let call = 0;
    const spy = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
      const url = typeof input === "string" ? input : input.toString();
      if (url.includes("/api/auth/refresh")) {
        return {
          ok: true,
          status: 200,
          json: async () => ({
            access_token: "at-new",
            refresh_token: "rt-new",
            token_type: "Bearer",
            expires_in: 900,
          }),
        } as unknown as Response;
      }
      call += 1;
      return call === 1 ? response(401) : response(200);
    });
    globalThis.fetch = spy as unknown as typeof fetch;
    uninstall = installAuthFetchInterceptor();

    const res = await window.fetch("http://gw.test/api/agents");

    expect(res.status).toBe(200);
    expect(call).toBe(2);
    // The replay carries the rotated token.
    const replayInit = spy.mock.calls.at(-1)?.[1] as RequestInit;
    expect(authHeaderOf(replayInit)).toBe("Bearer at-new");
  });

  it("does not attach a token when there is none", async () => {
    clear();
    const spy = vi.fn(async () => response(200));
    globalThis.fetch = spy as unknown as typeof fetch;
    uninstall = installAuthFetchInterceptor();

    await window.fetch("http://gw.test/api/agents");

    expect(spy.mock.calls[0][1]).toBeUndefined();
  });
});
