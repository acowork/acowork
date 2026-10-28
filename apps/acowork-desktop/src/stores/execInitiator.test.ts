/**
 * ADR-076 §decision 4 — an execution session has exactly ONE initiator
 * (regression t-9764c5fa).
 *
 * The three rules this file pins, one per bug the ticket reported:
 *
 *   1. the execution surface names the session's INITIATOR, never the
 *      logged-in account (`resolveInitiatorIdentity`);
 *   2. a read-only execution session opened from history keeps its tab —
 *      `syncActiveTabs` prunes only tabs the Runtime no longer reports AND
 *      that are not in the foreground, otherwise the panel parked on
 *      "Loading session…" forever;
 *   3. `setMode` before the agent's Chat surface resolved is remembered,
 *      so a drill into an execution session is not undone by the later
 *      `openAgent` landing on the Chat surface.
 */
import { describe, it, expect, vi, beforeEach } from "vitest";
import { resolveInitiatorIdentity } from "../lib/sessionInitiator";
import { useChatStore } from "./chatStore";
import { useChatSessionStore } from "./chatSessionStore";
import { useAuthStore } from "./authStore";

const fetchMock = vi.fn(async (url: string) => {
  if (url.includes("/api/chat-sessions/default")) {
    return {
      ok: true,
      json: async () => ({
        session_id: "cs-1",
        kind: "user_agent",
        title: "",
        created_at: 1,
        last_active_at: 1,
        message_count: 0,
      }),
    };
  }
  if (url.includes("/messages")) return { ok: true, json: async () => ({ messages: [], total: 0 }) };
  return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
});
vi.stubGlobal("fetch", fetchMock);

describe("execution surface identity = initiator", () => {
  it("names another human from the account directory, not the current profile", () => {
    const id = resolveInitiatorIdentity(
      "u-admin",
      "u-nicholas",
      [{ user_id: "u-admin", username: "admin", display_name: "Admin" } as never],
      () => undefined,
    );
    expect(id?.displayName).toBe("Admin");
  });

  it("names an agent initiator from the agent's own metadata", () => {
    const id = resolveInitiatorIdentity("inst-1", "u-nicholas", [], () => ({
      name: "A.Ponytail",
      display_name: "A.Ponytail",
    }));
    expect(id?.displayName).toBe("A.Ponytail");
  });

  it("returns null when I am the initiator — the surface uses my own profile", () => {
    expect(resolveInitiatorIdentity("u-nicholas", "u-nicholas", [], () => undefined)).toBeNull();
  });
});

describe("the account directory is never cached as 'empty'", () => {
  it("re-asks once a token exists, and after a failed fetch", async () => {
    // A fresh module registry: the directory cache is module state, so a
    // single test file would otherwise inherit a neighbour's result.
    vi.resetModules();
    const auth = await import("./authStore");
    auth.useAuthStore.setState({ accessToken: null, account: null } as never);
    const mod = await import("../lib/sessionInitiator");

    // No token yet: empty answer, nothing cached. Caching this would pin
    // every other participant to a short id for the rest of the process.
    fetchMock.mockClear();
    expect(await mod.loadDirectory()).toEqual([]);
    expect(fetchMock).not.toHaveBeenCalled();

    // Token arrives: the next call really asks.
    auth.useAuthStore.setState({ accessToken: "tok", account: null } as never);
    const dir = await mod.loadDirectory();
    expect(fetchMock).toHaveBeenCalledTimes(1);
    expect(Array.isArray(dir)).toBe(true);

    // One blip must not be cached as "there are no other users" either.
    vi.resetModules();
    const auth2 = await import("./authStore");
    auth2.useAuthStore.setState({ accessToken: "tok", account: null } as never);
    const mod2 = await import("../lib/sessionInitiator");
    fetchMock.mockClear();
    fetchMock.mockImplementationOnce(async () => ({
      ok: false,
      status: 503,
      json: async () => ({}),
      text: async () => "",
    }));
    expect(await mod2.loadDirectory()).toEqual([]);
    await mod2.loadDirectory();
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });
});

describe("read-only execution session tab survives syncActiveTabs", () => {
  beforeEach(() => {
    useChatStore.setState({
      agentStates: {
        a1: {
          sessionStates: {},
          activeSessionId: "ro",
          openSessionIds: ["ro", "live"],
          lastLoadedSessionId: null,
          isSessionInitLoading: false,
          preferredModel: null,
          preferredProvider: null,
          llmAvailability: "unspecified",
        },
      },
    } as never);
  });

  it("keeps the foreground tab even though the Runtime never reports it active", () => {
    useChatStore.getState().syncActiveTabs("a1", ["live"]);
    const st = useChatStore.getState().agentStates["a1"]!;
    expect(st.openSessionIds).toEqual(["ro", "live"]);
    expect(st.activeSessionId).toBe("ro");
  });

  it("still prunes a background tab the Runtime dropped", () => {
    useChatStore.getState().syncActiveTabs("a1", ["ro"]);
    expect(useChatStore.getState().agentStates["a1"]!.openSessionIds).toEqual(["ro"]);
  });
});

describe("setMode before the Chat surface resolves", () => {
  beforeEach(() => {
    useChatSessionStore.setState({ byAgent: {}, windows: {}, sending: {}, draft: {} });
    useAuthStore.setState({ account: { user_id: "u1" } as never, accessToken: "tok" });
    fetchMock.mockClear();
  });

  it("remembers the exec surface across openAgent", async () => {
    useChatSessionStore.getState().setMode("agent-1", "exec");
    await useChatSessionStore.getState().openAgent("agent-1");
    const st = useChatSessionStore.getState().byAgent["agent-1"]!;
    expect(st.resolved).toBe(true);
    expect(st.mode).toBe("exec");
  });

  it("defaults to the Chat surface when nothing was requested", async () => {
    await useChatSessionStore.getState().openAgent("agent-2");
    expect(useChatSessionStore.getState().byAgent["agent-2"]!.mode).toBe("chat");
  });
});
