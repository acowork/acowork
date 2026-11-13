/**
 * Unit tests for the account session store (ADR-076 §决策 3 / 6 / 12).
 *
 * These cover the parts the login gate and the fetch interceptor depend
 * on: account-state resolution (ADR-114: setup gate vs login), session
 * restore from storage, and — most important — the single-flight refresh
 * that stops parallel 401s from double-rotating the token family (the
 * Gateway treats a reused refresh token as a compromise and kills it).
 */
import { describe, it, expect, vi, beforeEach } from "vitest";

vi.mock("../lib/config", () => ({ getGatewayUrl: () => "http://gw.test" }));
vi.mock("../lib/logger", () => ({
  log: { debug: () => {}, info: () => {}, warn: () => {}, error: () => {} },
}));

import { useAuthStore } from "./authStore";
import { __resetForTests, adopt, current } from "../lib/session/tokens";
import type { UserAccount } from "../lib/types";

const TOKENS_KEY = "acowork.auth.tokens";

const acct: UserAccount = {
  user_id: "u-1",
  username: "alice",
  display_name: "Alice",
  role: "admin",
  language: "en",
  timezone: "UTC",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
};

function json(body: unknown, status = 200): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
  } as unknown as Response;
}

/** Route-aware fetch stub: `routes[urlSuffix]` → Response factory. */
function stubFetch(routes: Record<string, () => Response>) {
  const spy = vi.fn(async (input: RequestInfo | URL) => {
    const url = typeof input === "string" ? input : input.toString();
    for (const [suffix, make] of Object.entries(routes)) {
      if (url.includes(suffix)) return make();
    }
    throw new Error(`unexpected fetch: ${url}`);
  });
  globalThis.fetch = spy as unknown as typeof fetch;
  return spy;
}

beforeEach(() => {
  localStorage.clear();
  __resetForTests();
  useAuthStore.setState({
    accountState: "unknown",
    status: "unknown",
    account: null,
    accessToken: null,
    error: null,
    viewAsUserId: null,
    setupRequired: false,
    registrationOpen: false,
    _setupPollHandle: null,
  });
});

describe("authStore.init", () => {
  it("treats an explicit ready account_state as a normal login gate", async () => {
    stubFetch({ "/api/status": () => json({ account_state: "ready" }) });

    await useAuthStore.getState().init();

    expect(useAuthStore.getState().accountState).toBe("ready");
    expect(useAuthStore.getState().status).toBe("logged_out");
  });

  it("requires a login on a ready account system", async () => {
    stubFetch({ "/api/status": () => json({ account_state: "ready" }) });

    await useAuthStore.getState().init();

    expect(useAuthStore.getState().status).toBe("logged_out");
  });

  it("restores a stored session via /me", async () => {
    localStorage.setItem(
      TOKENS_KEY,
      JSON.stringify({ accessToken: "at", refreshToken: "rt" }),
    );
    stubFetch({
      "/api/status": () => json({ account_state: "ready" }),
      "/api/auth/me": () => json(acct),
    });

    await useAuthStore.getState().init();

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_in");
    expect(state.account?.user_id).toBe("u-1");
  });

  it("treats a Gateway without account_state as ready (no setup gate)", async () => {
    stubFetch({ "/api/status": () => json({ version: "0.0.0" }) });

    await useAuthStore.getState().init();

    expect(useAuthStore.getState().accountState).toBe("ready");
    expect(useAuthStore.getState().status).toBe("logged_out");
  });

  it("enters setup_required when account_state is needs_setup", async () => {
    stubFetch({ "/api/status": () => json({ account_state: "needs_setup" }) });

    await useAuthStore.getState().init();

    const state = useAuthStore.getState();
    expect(state.accountState).toBe("needs_setup");
    expect(state.status).toBe("setup_required");
    expect(state.setupRequired).toBe(true);
    expect(state._setupPollHandle).not.toBeNull();
    // Cleanup so the interval doesn't leak across tests.
    useAuthStore.getState().stopSetupPoll();
  });

  it("enters setup_required when the account store is uninitialized", async () => {
    stubFetch({ "/api/status": () => json({ account_state: "uninitialized" }) });

    await useAuthStore.getState().init();

    expect(useAuthStore.getState().accountState).toBe("uninitialized");
    expect(useAuthStore.getState().status).toBe("setup_required");
    useAuthStore.getState().stopSetupPoll();
  });

  it("does not gate a ready account system", async () => {
    stubFetch({ "/api/status": () => json({ account_state: "ready" }) });

    await useAuthStore.getState().init();

    expect(useAuthStore.getState().status).toBe("logged_out");
    expect(useAuthStore.getState().setupRequired).toBe(false);
  });
});

// ADR-114 (2026-11-14): the boot probe can outrun the user-service
// snapshot and land the user on LoginView while NO admin exists. The
// restricted gate then answers the sign-in with 403 `setup_required` —
// that must route to the first-boot wizard, never print the raw code.
describe("authStore.login routes the restricted gate to the setup wizard", () => {
  it("a 403 setup_required flips the store into the first-boot flow", async () => {
    stubFetch({
      "/api/auth/login": () =>
        json({ error: "setup_required", message: "not initialised" }, 403),
      "/api/status": () => json({ account_state: "needs_setup" }),
    });

    await useAuthStore.getState().login("alice", "pw");
    useAuthStore.getState().stopSetupPoll();

    const s = useAuthStore.getState();
    expect(s.status).toBe("setup_required");
    expect(s.accountState).toBe("needs_setup");
    expect(s.setupRequired).toBe(true);
    expect(s.error).toBeNull();
  });

  it("any other 403 still surfaces its message as a login error", async () => {
    stubFetch({
      "/api/auth/login": () => json({ error: "invalid_credentials" }, 403),
    });

    await expect(useAuthStore.getState().login("alice", "pw")).rejects.toThrow();
    const s = useAuthStore.getState();
    expect(s.status).toBe("unknown");
    expect(s.error).toBe("invalid_credentials");
  });
});

describe("authStore.firstLogin", () => {
  it("activates the account and stores the token pair", async () => {
    stubFetch({
      "/api/auth/first-login": () =>
        json({ access_token: "at", refresh_token: "rt", token_type: "Bearer", expires_in: 900 }),
      "/api/auth/me": () => json(acct),
    });

    await useAuthStore.getState().firstLogin("invite-123", "newpass12");

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_in");
    expect(state.accessToken).toBe("at");
    expect(state.account?.user_id).toBe("u-1");
  });

  it("surfaces the Gateway error on a spent invite token", async () => {
    stubFetch({ "/api/auth/first-login": () => json({ detail: "invalid invite token" }, 401) });

    await expect(useAuthStore.getState().firstLogin("spent", "newpass12")).rejects.toThrow();

    expect(useAuthStore.getState().status).not.toBe("logged_in");
    expect(useAuthStore.getState().error).toContain("invite");
  });
});

describe("authStore.completeSetup", () => {
  const pair = {
    access_token: "at-setup",
    refresh_token: "rt-setup",
    token_type: "Bearer",
    expires_in: 900,
  };

  it("waits for /api/status to be ready, then adopts the session with its account", async () => {
    stubFetch({
      "/api/auth/setup": () => json(pair),
      "/api/status": () => json({ account_state: "ready" }),
      "/api/auth/me": () => json(acct),
    });

    await useAuthStore.getState().completeSetup("admin", "s3cret123");

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_in");
    expect(state.accountState).toBe("ready");
    expect(state.setupRequired).toBe(false);
    expect(state.account?.user_id).toBe("u-1");
    expect(state.accessToken).toBe("at-setup");
  });

  /** Regression (ADR-114 review): the old code set `logged_in` before
   *  `/me` and merely logged a warning on failure, leaving a half-built
   *  session with `account === null`. The wizard cannot be retried (a
   *  second setup answers 409), so the only sound recovery is the login
   *  form with the credentials just created. */
  it("falls back to logged_out instead of a half-built session when /me fails", async () => {
    stubFetch({
      "/api/auth/setup": () => json(pair),
      "/api/status": () => json({ account_state: "ready" }),
      "/api/auth/me": () => json({ error: "upstream unavailable" }, 503),
    });

    await useAuthStore.getState().completeSetup("admin", "s3cret123");

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_out");
    expect(state.account).toBeNull();
    expect(state.accessToken).toBeNull();
    expect(state.setupRequired).toBe(false);
    expect(state.error).toContain("sign in");
  });
});

describe("authStore mirrors the token module", () => {
  // The store no longer owns the pair; these pin the two directions of the
  // wiring — a new token appears as `accessToken`, and the module's
  // dead-session verdict lands as `logged_out`.
  it("reflects an adopted token without being told to", () => {
    adopt({ access_token: "at-x", refresh_token: "rt-x" });
    expect(useAuthStore.getState().accessToken).toBe("at-x");
    expect(current()).toBe("at-x");
  });

  it("clears every piece of session state when the module ends the session", () => {
    useAuthStore.setState({ status: "logged_in", account: acct, viewAsUserId: "u-2" });
    adopt({ access_token: "at-x", refresh_token: "rt-x" });

    useAuthStore.getState().clearLocalSession();

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_out");
    expect(state.account).toBeNull();
    expect(state.accessToken).toBeNull();
    expect(state.viewAsUserId).toBeNull();
    expect(current()).toBeNull();
    expect(localStorage.getItem(TOKENS_KEY)).toBeNull();
  });
});

describe("authStore.onGatewayUrlChanged", () => {
  it("is a no-op when newUrl equals oldUrl", async () => {
    const spy = stubFetch({ "/api/status": () => json({ account_state: "ready" }) });

    await useAuthStore
      .getState()
      .onGatewayUrlChanged("http://gw.test", "http://gw.test");

    expect(spy).not.toHaveBeenCalled();
  });

  it("keeps the session when the new URL is an alias for the same Gateway", async () => {
    adopt({ access_token: "at", refresh_token: "rt" });
    useAuthStore.setState({ status: "logged_in", account: acct });
    const meSpy = vi.fn(() => json(acct));
    stubFetch({
      "/api/auth/me": meSpy,
      "/api/status": () => json({ account_state: "ready" }),
    });

    await useAuthStore
      .getState()
      .onGatewayUrlChanged("http://localhost:19876", "http://127.0.0.1:19876");

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_in");
    expect(state.accessToken).toBe("at");
    expect(meSpy).toHaveBeenCalledTimes(1);
  });

  it("drops the session when the new Gateway rejects the token with 401", async () => {
    adopt({ access_token: "at", refresh_token: "rt" });
    useAuthStore.setState({ status: "logged_in", account: acct });
    stubFetch({
      "/api/auth/me": () => json({ detail: "invalid token" }, 401),
      "/api/status": () => json({ account_state: "ready" }),
    });

    await useAuthStore
      .getState()
      .onGatewayUrlChanged("http://new-gw:19876", "http://old-gw:19876");

    const state = useAuthStore.getState();
    expect(state.status).toBe("logged_out");
    expect(state.accessToken).toBeNull();
        expect(localStorage.getItem(TOKENS_KEY)).toBeNull();
  });

  it("keeps the session when the probe fails with a network error", async () => {
    adopt({ access_token: "at", refresh_token: "rt" });
    useAuthStore.setState({ status: "logged_in", account: acct });
    globalThis.fetch = vi.fn(async () => {
      throw new TypeError("Failed to fetch");
    }) as unknown as typeof fetch;

    await useAuthStore
      .getState()
      .onGatewayUrlChanged("http://unreachable:19876", "http://old-gw:19876");

    const state = useAuthStore.getState();
    expect(state.accessToken).toBe("at");
    expect(state.status).toBe("logged_in");
  });

  it("skips the probe when there is no session and just re-inits", async () => {
    useAuthStore.setState({
      status: "logged_out",
      accessToken: null, 
    });
    stubFetch({ "/api/status": () => json({ account_state: "ready" }) });

    await useAuthStore
      .getState()
      .onGatewayUrlChanged("http://new-gw:19876", "http://old-gw:19876");

    expect(useAuthStore.getState().status).toBe("logged_out");
  });
});
