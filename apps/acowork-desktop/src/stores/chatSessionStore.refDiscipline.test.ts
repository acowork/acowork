/**
 * Store reference discipline on the doorbell path (render-perf L3).
 *
 * The invariant the memoized surfaces (L1) and the virtualized window (L2)
 * depend on: a background re-read that found nothing new must not produce a
 * new store identity at all, and a re-read that found one new row must hand
 * back the OLD objects for every unchanged row. Both are asserted here by
 * object reference, not by render counts — render counts are the surfaces'
 * tests (AgentChatView.memo / GroupChatPanel.memo).
 *
 * `fetch` is the seam (same as chatSessionStore.test.ts).
 */
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  useChatSessionStore,
  wakeAgentChat,
  type ChatMessageRow,
} from "./chatSessionStore";
import { useAuthStore } from "./authStore";

const server = { rows: [] as ChatMessageRow[] };

const row = (seq: number, body = `m${seq}`): ChatMessageRow => ({
  seq,
  ts: 1000 + seq,
  from: "agent-1",
  kind: "agent_message",
  body,
  to: "u1",
  meta: { message_id: `mid-${seq}` },
});

async function defaultFetch(url: string): Promise<unknown> {
  if (url.includes("/messages")) {
    const params = new URLSearchParams(url.split("?")[1] ?? "");
    const offset = Number(params.get("offset") ?? "0");
    const limit = Number(params.get("limit") ?? "50");
    const total = server.rows.length;
    const messages = server.rows.slice(Math.max(0, total - offset - limit), total - offset);
    return { ok: true, json: async () => ({ messages, total }) };
  }
  return { ok: false, status: 503, json: async () => ({}), text: async () => "" };
}

const fetchMock = vi.fn(defaultFetch);
vi.stubGlobal("fetch", fetchMock);

/** Seed a tail window the way `refresh` would have built it. */
function seedWindow(chatSessionId: string) {
  const n = server.rows.length;
  useChatSessionStore.setState({
    windows: {
      [chatSessionId]: {
        older: [],
        tail: server.rows.slice(),
        total: n,
        firstPos: 0,
        lastPos: n - 1,
        loadingOlder: false,
        loadingNewer: false,
        pendingLocateSeq: null,
      },
    },
  });
}

describe("chatSessionStore reference discipline (render-perf L3)", () => {
  beforeEach(() => {
    server.rows = [];
    fetchMock.mockClear();
    fetchMock.mockImplementation(defaultFetch);
    useAuthStore.setState({ account: { user_id: "u1" } as never, accessToken: "tok" });
    useChatSessionStore.setState({ byAgent: {}, openGroupIds: {}, windows: {}, sending: {}, draft: {} });
  });

  it("an unchanged non-forced refresh sets NOTHING — window identity survives", async () => {
    server.rows = Array.from({ length: 5 }, (_, i) => row(i + 1));
    seedWindow("cs-1");
    const before = useChatSessionStore.getState().windows["cs-1"];
    await useChatSessionStore.getState().refresh("cs-1");
    expect(useChatSessionStore.getState().windows["cs-1"]).toBe(before);
  });

  it("a mid-history window with unchanged total also sets nothing", async () => {
    server.rows = Array.from({ length: 10 }, (_, i) => row(i + 1));
    // The reader jumped to an early page: the loaded span (rows 1..5) stops
    // short of the newest (total 10), so the window is mid-history.
    useChatSessionStore.setState({
      windows: {
        "cs-1": {
          older: [],
          tail: server.rows.slice(0, 5),
          total: 10,
          firstPos: 0,
          lastPos: 4,
          loadingOlder: false,
          loadingNewer: false,
          pendingLocateSeq: null,
        },
      },
    });
    const parked = useChatSessionStore.getState().windows["cs-1"];
    await useChatSessionStore.getState().refresh("cs-1");
    expect(useChatSessionStore.getState().windows["cs-1"]).toBe(parked);
    // A row appended above while the reader was parked: the window keeps its
    // place, only `total` folds — the row arrays keep their identities.
    server.rows.push(row(11, "above"));
    await useChatSessionStore.getState().refresh("cs-1");
    const after = useChatSessionStore.getState().windows["cs-1"]!;
    expect(after.total).toBe(11);
    expect(after.tail).toBe(parked!.tail);
    expect(after.older).toBe(parked!.older);
  });

  it("a new row lands, but unchanged rows keep their object identities", async () => {
    server.rows = Array.from({ length: 5 }, (_, i) => row(i + 1));
    seedWindow("cs-1");
    const tailBefore = useChatSessionStore.getState().windows["cs-1"]!.tail;
    server.rows.push(row(6, "fresh"));
    await useChatSessionStore.getState().refresh("cs-1");
    const after = useChatSessionStore.getState().windows["cs-1"]!;
    expect(after.total).toBe(6);
    expect(after.tail).not.toBe(tailBefore);
    // The page re-read carries rows 2..6 (PAGE=50 here returns all 6): rows
    // 1..5 are the same bytes — same objects. Only row 6 is a new identity.
    for (const old of tailBefore) {
      const reused = after.tail.find((r) => r.seq === old.seq);
      expect(reused).toBe(old);
    }
    const fresh = after.tail.find((r) => r.seq === 6);
    expect(fresh).toBeDefined();
    expect(fresh!.body).toBe("fresh");
  });

  it("force always re-reads into a new window even with identical content", async () => {
    server.rows = Array.from({ length: 3 }, (_, i) => row(i + 1));
    seedWindow("cs-1");
    const before = useChatSessionStore.getState().windows["cs-1"];
    await useChatSessionStore.getState().refresh("cs-1", true);
    expect(useChatSessionStore.getState().windows["cs-1"]).not.toBe(before);
    // …with the row objects still shared (the reset must not cost renders
    // for rows that did not change).
    const after = useChatSessionStore.getState().windows["cs-1"]!;
    for (const old of before!.tail) {
      expect(after.tail.find((r) => r.seq === old.seq)).toBe(old);
    }
  });

  it("wake() coalesces a burst of doorbells into one re-read round", async () => {
    vi.useFakeTimers();
    try {
      server.rows = [row(1)];
      useChatSessionStore.setState({
        byAgent: {
          "agent-1": { chatSessionId: "cs-1", resolved: true, mode: "chat" },
        },
      });
      seedWindow("cs-1");
      fetchMock.mockClear();
      // One turn rings the bell several times.
      for (let i = 0; i < 5; i += 1) wakeAgentChat();
      await vi.advanceTimersByTimeAsync(300);
      const messageReads = fetchMock.mock.calls.filter((c) => String(c[0]).includes("/messages"));
      expect(messageReads.length).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("wake() re-reads every open session in one round (no serial chain)", async () => {
    vi.useFakeTimers();
    try {
      server.rows = [row(1)];
      useChatSessionStore.setState({
        byAgent: { "agent-1": { chatSessionId: "cs-1", resolved: true, mode: "chat" } },
        openGroupIds: { "grp-1": true },
      });
      fetchMock.mockClear();
      wakeAgentChat();
      await vi.advanceTimersByTimeAsync(300);
      const urls = fetchMock.mock.calls
        .map((c) => String(c[0]))
        .filter((u) => u.includes("/messages"));
      expect(urls.length).toBe(2);
      expect(urls.some((u) => u.includes("cs-1"))).toBe(true);
      expect(urls.some((u) => u.includes("grp-1"))).toBe(true);
    } finally {
      vi.useRealTimers();
    }
  });
});
